//! [`grpc`] 的服务级测试：对编译快照直接调用各 RPC，语义与 REST
//! （`rest_tests.rs` / `tests/rest_*.rs`）完全对齐——熔断器按实例 id 排序、
//! reset 幂等、stats 只把 Open 计入打开数。表驱动优先。
//!
//! 另覆盖 `mgmt.grpc` 开关（关闭 → UNIMPLEMENTED 占位）与未知方法路径的
//! UNIMPLEMENTED 兜底。真实监听 + tonic 客户端的端到端见 rgateway-server
//! 的 `tests/mgmt_grpc.rs`。

use std::sync::Arc;
use std::time::Duration;

use http_body_util::Full;
use rgateway_config::{
    compile, load_from_str, validate, CompileInput, FilterRegistry, RouteCompileInfo,
    RouteHandlerFactory, SnapshotStore,
};
use rgateway_core::{
    empty_body, BoxedService, BreakerConfig, GatewayError, ProxyBody, StateRegistry,
};
use tonic::Code;

use crate::pb::mgmt_service_server::MgmtService as MgmtServiceRpc;
use crate::pb::{
    BreakersRequest, HealthRequest, ResetBreakersRequest, RouteSummary, RoutesRequest,
    SnapshotRequest, StatsRequest,
};

use super::*;

/// 测试替身 handler 工厂：gRPC 服务级测试从不执行路由链。
struct StubFactory;

impl RouteHandlerFactory for StubFactory {
    fn build(&self, _route: &RouteCompileInfo) -> BoxedService {
        BoxedService::new(tower::service_fn(
            |_req: http::Request<ProxyBody>| async move {
                Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
            },
        ))
    }
}

/// 按 YAML 编译出管理面状态（真实编译快照驱动）。
fn state_with(yaml: &str) -> Arc<MgmtState> {
    let file = load_from_str(yaml).expect("yaml must parse");
    let errors = validate(&file);
    assert!(errors.is_empty(), "fixture must validate: {errors:?}");
    let states = Arc::new(StateRegistry::new());
    let compiled = compile(CompileInput {
        file: &file,
        registry: &FilterRegistry::new(),
        states: &states,
        handler_factory: &StubFactory,
        config_dir: std::path::Path::new("."),
    })
    .expect("config must compile");
    Arc::new(MgmtState::new(
        Arc::new(SnapshotStore::new(compiled)),
        states,
    ))
}

/// 两路由 + 三实例静态发现的 fixture（与 rest 测试同款，语义对拍用）。
const TWO_ROUTES: &str = r#"
server:
  listen: "127.0.0.1:8080"
discovery:
  providers:
    - type: static
      services:
        user-svc:
          - { address: "127.0.0.1:50051" }
          - { address: "127.0.0.1:50052" }
          - { address: "127.0.0.1:50053" }
routes:
  - id: user-http
    order: 1
    uri: "http://127.0.0.1:9000"
    predicates: ["Path=/api/**"]
  - id: user-lb
    order: 2
    uri: "lb://user-svc"
    protocol: rest-json
    predicates: ["Path=/lb/**"]
"#;

/// 路由摘要的元组形态：(order, id, uri, protocol)。
type Summary<'a> = (i32, &'a str, &'a str, &'a str);

/// 快照/路由端点共用的路由摘要期望值（按匹配顺序）。
const EXPECTED_SUMMARIES: &[Summary<'static>] = &[
    (1, "user-http", "http://127.0.0.1:9000/", "auto"),
    (2, "user-lb", "lb://user-svc", "rest-json"),
];

/// 一次失败即触发熔断的配置（open_duration 控制探测转移时机）。
fn trip_config(open_duration: Duration) -> BreakerConfig {
    BreakerConfig {
        sliding_window: 1,
        failure_rate: 0.5,
        min_calls: 1,
        open_duration,
        half_open_max_calls: 1,
    }
}

/// 造一个熔断器并用一次失败把它推到 Open（路由固定 `user-lb`）。
fn trip_open(state: &Arc<MgmtState>, instance_id: &str, open_duration: Duration) {
    let breaker = state
        .states()
        .route_state("user-lb")
        .breaker_for(instance_id, trip_config(open_duration));
    breaker.check().expect("permit").record(false);
}

/// 断言摘要列表与 [`EXPECTED_SUMMARIES`] 完全一致（顺序也一致）。
fn assert_summaries(routes: &[RouteSummary]) {
    let got: Vec<Summary<'_>> = routes
        .iter()
        .map(|r| (r.order, r.id.as_str(), r.uri.as_str(), r.protocol.as_str()))
        .collect();
    assert_eq!(got, EXPECTED_SUMMARIES);
}

// Health：恒返回 status="ok"。
#[tokio::test]
async fn health_returns_ok_status() {
    let state = state_with("{}");
    let resp = MgmtService::new(state)
        .health(tonic::Request::new(HealthRequest {}))
        .await
        .expect("health rpc");
    assert_eq!(resp.into_inner().status, "ok");
}

// Snapshot：版本、失败重载数、监听、路由计数与摘要，语义与 REST 一致。
#[tokio::test]
async fn snapshot_matches_rest_semantics() {
    let state = state_with(TWO_ROUTES);
    let resp = MgmtService::new(state)
        .snapshot(tonic::Request::new(SnapshotRequest {}))
        .await
        .expect("snapshot rpc");
    let snap = resp.into_inner();
    assert_eq!(snap.version, 1);
    assert_eq!(snap.failed_reloads, 0);
    assert_eq!(snap.listen, "127.0.0.1:8080");
    assert_eq!(snap.route_count, 2);
    assert_summaries(&snap.routes);
}

// Routes：与 snapshot 同形态、同顺序的路由摘要。
#[tokio::test]
async fn routes_lists_same_summaries_as_snapshot() {
    let state = state_with(TWO_ROUTES);
    let resp = MgmtService::new(state)
        .routes(tonic::Request::new(RoutesRequest {}))
        .await
        .expect("routes rpc");
    assert_summaries(&resp.into_inner().routes);
}

// 表驱动：路由摘要按匹配顺序（order 升序、平局按 id）输出，与声明顺序无关。
#[tokio::test]
async fn routes_follow_match_order_table() {
    let yaml = r#"
server:
  listen: "127.0.0.1:8080"
routes:
  - id: beta
    order: 2
    uri: "http://127.0.0.1:9001"
    predicates: ["Path=/b/**"]
  - id: gamma
    order: 2
    uri: "http://127.0.0.1:9002"
    predicates: ["Path=/g/**"]
  - id: alpha
    order: 1
    uri: "http://127.0.0.1:9000"
    predicates: ["Path=/a/**"]
"#;
    let state = state_with(yaml);
    let resp = MgmtService::new(state)
        .routes(tonic::Request::new(RoutesRequest {}))
        .await
        .expect("routes rpc");
    let routes = resp.into_inner().routes;
    let got: Vec<&str> = routes.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(got, ["alpha", "beta", "gamma"], "order 升序、平局按 id");
}

// Breakers：逐实例 closed/open/half_open 三态，按实例 id 稳定排序。
#[tokio::test]
async fn breakers_lists_states_per_instance_sorted() {
    let state = state_with(TWO_ROUTES);
    let route_state = state.states().route_state("user-lb");
    // 50051：只创建，保持 closed。
    let _keep = route_state.breaker_for("127.0.0.1:50051", trip_config(Duration::from_secs(30)));
    // 50052：推到 open（长探测期，停在 open）。
    trip_open(&state, "127.0.0.1:50052", Duration::from_secs(30));
    // 50053：零探测期 → 立即转入 half_open（首个探测在途）。
    trip_open(&state, "127.0.0.1:50053", Duration::ZERO);
    let half_open = route_state.breaker_for("127.0.0.1:50053", trip_config(Duration::ZERO));
    let probe = half_open.check().expect("probe permit");

    let resp = MgmtService::new(Arc::clone(&state))
        .breakers(tonic::Request::new(BreakersRequest {}))
        .await
        .expect("breakers rpc");
    let routes = resp.into_inner().routes;
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].route_id, "user-http");
    assert!(routes[0].breakers.is_empty());
    assert_eq!(routes[1].route_id, "user-lb");
    let got: Vec<(&str, &str)> = routes[1]
        .breakers
        .iter()
        .map(|b| (b.instance_id.as_str(), b.state.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("127.0.0.1:50051", "closed"),
            ("127.0.0.1:50052", "open"),
            ("127.0.0.1:50053", "half_open")
        ]
    );
    drop(probe);
}

// Breakers：无熔断器状态的路由输出空数组（边界）。
#[tokio::test]
async fn breakers_reports_empty_arrays_without_state() {
    let state = state_with(TWO_ROUTES);
    let resp = MgmtService::new(state)
        .breakers(tonic::Request::new(BreakersRequest {}))
        .await
        .expect("breakers rpc");
    let routes = resp.into_inner().routes;
    assert_eq!(routes.len(), 2);
    for route in &routes {
        assert!(
            route.breakers.is_empty(),
            "{} 必须无熔断状态",
            route.route_id
        );
    }
}

/// reset 用例列：(标签, 预置熔断实例, route_id, instance_id, 期望 reset 计数)。
type ResetCase<'a> = (&'a str, &'a [&'a str], &'a str, Option<&'a str>, u64);

// ResetBreakers：表驱动——未知路由/实例幂等返回 0；全部/单个重置返回触及
// 的熔断器个数，重复重放得到同一计数（重置无害、计数稳定，与 REST 一致）；
// 空 route_id 视为非法请求（INVALID_ARGUMENT）。
#[tokio::test]
async fn reset_breakers_semantics_table() {
    let cases: &[ResetCase<'_>] = &[
        ("unknown-route", &[], "nope", None, 0),
        (
            "unknown-instance",
            &["127.0.0.1:50051"],
            "user-lb",
            Some("nope"),
            0,
        ),
        (
            "reset-all",
            &["127.0.0.1:50051", "127.0.0.1:50052"],
            "user-lb",
            None,
            2,
        ),
        (
            "reset-single",
            &["127.0.0.1:50051", "127.0.0.1:50052"],
            "user-lb",
            Some("127.0.0.1:50052"),
            1,
        ),
    ];
    for &(label, trips, route_id, instance_id, want) in cases {
        let state = state_with(TWO_ROUTES);
        for instance in trips {
            trip_open(&state, instance, Duration::from_secs(30));
        }
        let svc = MgmtService::new(Arc::clone(&state));
        let make = || {
            tonic::Request::new(ResetBreakersRequest {
                route_id: route_id.to_string(),
                instance_id: instance_id.map(str::to_string),
            })
        };
        let first = svc
            .reset_breakers(make())
            .await
            .unwrap_or_else(|status| panic!("{label}: reset must succeed: {status}"))
            .into_inner()
            .reset;
        assert_eq!(first, want, "{label}: 首次 reset 计数");
        let second = svc
            .reset_breakers(make())
            .await
            .unwrap_or_else(|status| panic!("{label}: reset must succeed: {status}"))
            .into_inner()
            .reset;
        assert_eq!(second, want, "{label}: 重复重放必须得到同一计数");
    }
}

// ResetBreakers：空 route_id 一律拒绝（对齐 REST 的 400 invalid request）。
#[tokio::test]
async fn reset_breakers_rejects_empty_route_id() {
    let state = state_with(TWO_ROUTES);
    let err = MgmtService::new(state)
        .reset_breakers(tonic::Request::new(ResetBreakersRequest {
            route_id: String::new(),
            instance_id: None,
        }))
        .await
        .expect_err("空 route_id 必须拒绝");
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ResetBreakers：字段长度上限表驱动——超长 route_id/instance_id 一律拒绝
// （与 REST 请求体上限对称的防放大边界）；instance_id 空串走幂等 0
// （「未知实例」语义，与 REST 一致）。
// 列含义：(标签, route_id, instance_id, 期望成功?, 期望 reset 计数)
#[tokio::test]
async fn reset_breakers_field_length_table() {
    let route_at_limit = "r".repeat(256);
    let route_over = "r".repeat(257);
    let instance_over = "i".repeat(257);
    let cases: &[(&str, &str, Option<&str>, bool, u64)] = &[
        ("route-at-limit", &route_at_limit, None, true, 0),
        ("route-over-limit", &route_over, None, false, 0),
        (
            "instance-over-limit",
            "user-lb",
            Some(&instance_over),
            false,
            0,
        ),
        ("instance-empty", "user-lb", Some(""), true, 0),
    ];
    for &(label, route_id, instance_id, want_ok, want_reset) in cases {
        let state = state_with(TWO_ROUTES);
        let resp = MgmtService::new(state)
            .reset_breakers(tonic::Request::new(ResetBreakersRequest {
                route_id: route_id.to_string(),
                instance_id: instance_id.map(str::to_string),
            }))
            .await;
        if want_ok {
            let got = resp
                .unwrap_or_else(|status| panic!("{label}: 必须成功: {status}"))
                .into_inner()
                .reset;
            assert_eq!(got, want_reset, "{label}");
        } else {
            let err = resp.expect_err(label);
            assert_eq!(err.code(), Code::InvalidArgument, "{label}");
        }
    }
}

// Stats：只把 Open 计入打开数（half_open 是恢复探测），其余字段与快照一致。
#[tokio::test]
async fn stats_counts_open_breakers_only() {
    let state = state_with(TWO_ROUTES);
    let route_state = state.states().route_state("user-lb");
    let _closed = route_state.breaker_for("127.0.0.1:50051", trip_config(Duration::from_secs(30)));
    trip_open(&state, "127.0.0.1:50052", Duration::from_secs(30));
    // 50053：零探测期推 open 后立刻取探测 → half_open，不计入打开数。
    trip_open(&state, "127.0.0.1:50053", Duration::ZERO);
    let half_open = route_state.breaker_for("127.0.0.1:50053", trip_config(Duration::ZERO));
    let probe = half_open.check().expect("probe permit");

    let resp = MgmtService::new(Arc::clone(&state))
        .stats(tonic::Request::new(StatsRequest {}))
        .await
        .expect("stats rpc");
    let stats = resp.into_inner();
    assert_eq!(stats.version, 1);
    assert_eq!(stats.failed_reloads, 0);
    assert_eq!(stats.route_count, 2);
    assert_eq!(stats.breaker_count, 3);
    assert_eq!(stats.breaker_open_count, 1);
    drop(probe);
}

// mgmt.grpc=false：gRPC 请求走 UNIMPLEMENTED 占位（grpc-status 12）。
#[tokio::test]
async fn handle_unimplemented_when_grpc_disabled() {
    let state = state_with("mgmt:\n  grpc: false\n");
    let req = http::Request::builder()
        .uri("/rgateway.mgmt.v1.MgmtService/Health")
        .body(Full::new(Bytes::new()))
        .expect("request builds");
    let resp = handle(state, req).await;
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(grpc_status_of(&resp), Some("12"));
}

// 未知方法路径：由 tonic 生成代码兜底，同样 UNIMPLEMENTED。
#[tokio::test]
async fn handle_unknown_method_returns_unimplemented() {
    let state = state_with(TWO_ROUTES);
    let req = http::Request::builder()
        .uri("/rgateway.mgmt.v1.MgmtService/NoSuchMethod")
        .body(Full::new(Bytes::new()))
        .expect("request builds");
    let resp = handle(state, req).await;
    assert_eq!(grpc_status_of(&resp), Some("12"));
}

/// 取响应头里的 grpc-status 值（trailers-only 形态在头部）。
fn grpc_status_of<B>(resp: &http::Response<B>) -> Option<&str> {
    resp.headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
}

// 占位响应必须是 gRPC trailers-only UNIMPLEMENTED 形态：HTTP 200 +
// grpc-status: 12，消息可读、body 为空。
#[test]
fn unimplemented_is_trailers_only_unimplemented() {
    let resp = unimplemented();
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(grpc_status_of(&resp), Some("12"));
    let message = resp
        .headers()
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .expect("grpc-message header");
    assert!(!message.is_empty());
}
