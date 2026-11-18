//! 熔断器端点矩阵：GET /mgmt/v1/breakers 的状态快照（closed/open/half_open）
//! 与 POST /mgmt/v1/breakers/reset 的重置语义（指定实例 / 全部 / 未知路由幂等）
//! 及 body 边界校验（400）。
//!
//! 与 rest_api.rs 一样用真实编译快照驱动，仅通过公开 API 断言。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use rgateway_config::{
    compile, load_from_str, validate, CompileInput, FilterRegistry, RouteCompileInfo,
    RouteHandlerFactory, SnapshotStore,
};
use rgateway_core::{
    empty_body, BoxedService, BreakerConfig, GatewayError, ProxyBody, StateRegistry,
};
use rgateway_mgmt::{handle, MgmtState};
use serde_json::json;

/// 测试替身 handler 工厂：管理面测试从不执行路由链。
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

/// 用 fixture YAML 构造完整的管理面状态（快照 + 共享治理状态注册表）。
fn mgmt_state(yaml: &str) -> Arc<MgmtState> {
    let file = load_from_str(yaml).expect("fixture yaml must parse");
    let errors = validate(&file);
    assert!(errors.is_empty(), "fixture must validate: {errors:?}");
    let states = Arc::new(StateRegistry::new());
    let compiled = compile(CompileInput {
        file: &file,
        registry: &FilterRegistry::new(),
        states: &states,
        handler_factory: &StubFactory,
        config_dir: Path::new("."),
    })
    .expect("fixture must compile");
    Arc::new(MgmtState::new(
        Arc::new(SnapshotStore::new(compiled)),
        states,
    ))
}

/// 向 `handle` 发一个请求并收集 JSON 响应体。
async fn call(state: &Arc<MgmtState>, method: &str, path: &str, body: &str) -> (u16, String) {
    let req = http::Request::builder()
        .method(method)
        .uri(path)
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("request must build");
    let resp = handle(Arc::clone(state), req).await;
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("response body must collect")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// 一次 POST reset 并断言 200，返回 `data` 部分。
async fn reset_ok(state: &Arc<MgmtState>, body: &str) -> serde_json::Value {
    let (status, raw) = call(state, "POST", "/mgmt/v1/breakers/reset", body).await;
    assert_eq!(status, 200, "reset 必须成功: {raw}");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("body must be json");
    assert_eq!(v["success"], json!(true), "{raw}");
    v["data"].clone()
}

/// 两路由 + 三实例静态发现的 fixture（direct + lb）。
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
    predicates: ["Path=/lb/**"]
"#;

/// 一次失败即触发熔断的配置（open_duration 可控探测转移时机）。
fn trip_config(open_duration: Duration) -> BreakerConfig {
    BreakerConfig {
        sliding_window: 1,
        failure_rate: 0.5,
        min_calls: 1,
        open_duration,
        half_open_max_calls: 1,
    }
}

/// 造一个熔断器并用一次失败把它推到 Open。
fn trip_open(state: &Arc<MgmtState>, instance_id: &str, open_duration: Duration) {
    let breaker = state
        .states()
        .route_state("user-lb")
        .breaker_for(instance_id, trip_config(open_duration));
    breaker.check().expect("permit").record(false);
}

/// 验证：路由没有熔断器状态时输出空数组（空状态边界）。
#[tokio::test]
async fn breakers_reports_empty_arrays_for_routes_without_breakers() {
    let state = mgmt_state(TWO_ROUTES);
    let (status, raw) = call(&state, "GET", "/mgmt/v1/breakers", "").await;

    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&raw).expect("body must be json");
    assert_eq!(
        v,
        json!({
            "success": true,
            "data": {"routes": [
                {"route_id": "user-http", "breakers": []},
                {"route_id": "user-lb", "breakers": []}
            ]},
            "error": null
        })
    );
}

/// 验证：逐实例输出 closed/open/half_open 三态，且按实例 id 稳定排序。
#[tokio::test]
async fn breakers_lists_states_per_instance() {
    let state = mgmt_state(TWO_ROUTES);
    let route_state = state.states().route_state("user-lb");
    // 50051：只创建，保持 closed。
    let _keep = route_state.breaker_for("127.0.0.1:50051", trip_config(Duration::from_secs(30)));
    // 50052：推到 open（长探测期，停在 open）。
    trip_open(&state, "127.0.0.1:50052", Duration::from_secs(30));
    // 50053：零探测期 → 立即转入 half_open（首个探测在途）。
    trip_open(&state, "127.0.0.1:50053", Duration::ZERO);
    let half_open = route_state.breaker_for("127.0.0.1:50053", trip_config(Duration::ZERO));
    let probe = half_open.check().expect("probe permit");

    let (status, raw) = call(&state, "GET", "/mgmt/v1/breakers", "").await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&raw).expect("body must be json");
    assert_eq!(
        v,
        json!({
            "success": true,
            "data": {"routes": [
                {"route_id": "user-http", "breakers": []},
                {"route_id": "user-lb", "breakers": [
                    {"instance_id": "127.0.0.1:50051", "state": "closed"},
                    {"instance_id": "127.0.0.1:50052", "state": "open"},
                    {"instance_id": "127.0.0.1:50053", "state": "half_open"}
                ]}
            ]},
            "error": null
        })
    );
    drop(probe);
}

/// 验证：空配置下 breakers 输出空 routes 列表（边界）。
#[tokio::test]
async fn breakers_reports_no_routes_for_empty_config() {
    let state = mgmt_state("{}");
    let (status, raw) = call(&state, "GET", "/mgmt/v1/breakers", "").await;

    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&raw).expect("body must be json");
    assert_eq!(
        v,
        json!({"success": true, "data": {"routes": []}, "error": null})
    );
}

/// 验证：不带 instance_id 的 reset 重置该路由全部熔断器并返回个数。
#[tokio::test]
async fn reset_all_breakers_of_route() {
    let state = mgmt_state(TWO_ROUTES);
    trip_open(&state, "127.0.0.1:50051", Duration::from_secs(30));
    trip_open(&state, "127.0.0.1:50052", Duration::from_secs(30));

    let data = reset_ok(&state, "{\"route_id\":\"user-lb\"}").await;
    assert_eq!(data, json!({"reset": 2}));

    let (_, raw) = call(&state, "GET", "/mgmt/v1/breakers", "").await;
    let v: serde_json::Value = serde_json::from_str(&raw).expect("body must be json");
    assert_eq!(
        v["data"]["routes"][1]["breakers"],
        json!([
            {"instance_id": "127.0.0.1:50051", "state": "closed"},
            {"instance_id": "127.0.0.1:50052", "state": "closed"}
        ]),
        "重置后必须回到 closed"
    );
}

/// 验证：指定 instance_id 只重置那一个，其余保持原状。
#[tokio::test]
async fn reset_single_instance_only() {
    let state = mgmt_state(TWO_ROUTES);
    trip_open(&state, "127.0.0.1:50051", Duration::from_secs(30));
    trip_open(&state, "127.0.0.1:50052", Duration::from_secs(30));

    let data = reset_ok(
        &state,
        "{\"route_id\":\"user-lb\",\"instance_id\":\"127.0.0.1:50052\"}",
    )
    .await;
    assert_eq!(data, json!({"reset": 1}));

    let (_, raw) = call(&state, "GET", "/mgmt/v1/breakers", "").await;
    let v: serde_json::Value = serde_json::from_str(&raw).expect("body must be json");
    assert_eq!(
        v["data"]["routes"][1]["breakers"],
        json!([
            {"instance_id": "127.0.0.1:50051", "state": "open"},
            {"instance_id": "127.0.0.1:50052", "state": "closed"}
        ]),
        "只重置指定实例"
    );
}

/// 验证：instance_id 显式为 null 时等价于重置全部（与省略同义）。
#[tokio::test]
async fn reset_explicit_null_instance_resets_all() {
    let state = mgmt_state(TWO_ROUTES);
    trip_open(&state, "127.0.0.1:50051", Duration::from_secs(30));
    trip_open(&state, "127.0.0.1:50052", Duration::from_secs(30));

    let data = reset_ok(&state, "{\"route_id\":\"user-lb\",\"instance_id\":null}").await;
    assert_eq!(data, json!({"reset": 2}));
}

/// 验证：未知实例 id 幂等返回 reset 0。
#[tokio::test]
async fn reset_unknown_instance_is_idempotent_zero() {
    let state = mgmt_state(TWO_ROUTES);
    trip_open(&state, "127.0.0.1:50051", Duration::from_secs(30));

    let data = reset_ok(
        &state,
        "{\"route_id\":\"user-lb\",\"instance_id\":\"127.0.0.1:59999\"}",
    )
    .await;
    assert_eq!(data, json!({"reset": 0}));
}

/// 验证：未知路由幂等返回 reset 0，且不在注册表留下新状态（无副作用）。
#[tokio::test]
async fn reset_unknown_route_is_idempotent_zero() {
    let state = mgmt_state(TWO_ROUTES);
    let ids = |state: &Arc<MgmtState>| {
        let mut ids: Vec<String> = state
            .states()
            .route_ids()
            .iter()
            .map(|id| id.to_string())
            .collect();
        ids.sort();
        ids
    };
    let before = ids(&state);

    let data = reset_ok(&state, "{\"route_id\":\"no-such-route\"}").await;
    assert_eq!(data, json!({"reset": 0}));
    assert_eq!(ids(&state), before, "未知路由的 reset 不得创建状态条目");
}

/// 验证：路由存在但从未有熔断器状态时返回 reset 0。
#[tokio::test]
async fn reset_route_without_state_is_zero() {
    let state = mgmt_state(TWO_ROUTES);

    let data = reset_ok(&state, "{\"route_id\":\"user-http\"}").await;
    assert_eq!(data, json!({"reset": 0}));
}

/// 表格驱动：非法 body / 缺 route_id 一律 400，信封文案固定为 invalid request。
#[tokio::test]
async fn reset_rejects_invalid_bodies() {
    let state = mgmt_state(TWO_ROUTES);
    let cases: &[(&str, &str)] = &[
        ("empty body", ""),
        ("non-json", "not-json"),
        ("empty object", "{}"),
        ("route_id wrong type", "{\"route_id\":123}"),
        ("route_id empty string", "{\"route_id\":\"\"}"),
        (
            "instance_id wrong type",
            "{\"route_id\":\"user-lb\",\"instance_id\":123}",
        ),
        ("unknown field", "{\"route_id\":\"user-lb\",\"bogus\":1}"),
        ("json array", "[]"),
        ("json null", "null"),
        ("json string", "\"user-lb\""),
    ];
    for &(label, body) in cases {
        let (status, raw) = call(&state, "POST", "/mgmt/v1/breakers/reset", body).await;
        assert_eq!(status, 400, "{label}");
        assert_eq!(
            raw, "{\"success\":false,\"data\":null,\"error\":\"invalid request\"}",
            "{label}"
        );
    }
}
