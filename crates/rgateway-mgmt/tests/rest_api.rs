//! 管理面 REST 端点矩阵：health / snapshot / routes / stats 的正常与边界、
//! 方法矩阵（405 + Allow）、未知路径（404）、信封形态与内部细节不泄露。
//!
//! 测试一律用 `load_from_str + validate + compile` 构造真实快照（handler
//! 工厂用测试替身），断言走公开 API，不碰 crate 内部。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use rgateway_config::{
    compile, load_from_str, validate, CompileInput, CompiledConfig, FilterRegistry,
    RouteCompileInfo, RouteHandlerFactory, SnapshotStore,
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

/// 走真实编译管线把 YAML 编译成快照（states 由调用方持有以便断言）。
fn compile_fixture(yaml: &str, states: &Arc<StateRegistry>) -> CompiledConfig {
    let file = load_from_str(yaml).expect("fixture yaml must parse");
    let errors = validate(&file);
    assert!(errors.is_empty(), "fixture must validate: {errors:?}");
    compile(CompileInput {
        file: &file,
        registry: &FilterRegistry::new(),
        states,
        handler_factory: &StubFactory,
        config_dir: Path::new("."),
    })
    .expect("fixture must compile")
}

/// 用 fixture YAML 构造完整的管理面状态（快照 + 共享治理状态注册表）。
fn mgmt_state(yaml: &str) -> Arc<MgmtState> {
    let states = Arc::new(StateRegistry::new());
    let compiled = compile_fixture(yaml, &states);
    Arc::new(MgmtState::new(
        Arc::new(SnapshotStore::new(compiled)),
        states,
    ))
}

/// 一次调用的响应快照：状态码、Allow 头、content-type、原始 body。
struct Res {
    status: u16,
    allow: Option<String>,
    content_type: String,
    body: String,
}

impl Res {
    /// body 必须是合法 JSON（信封测试的最低要求）。
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body must be json: {e}; body={}", self.body))
    }
}

/// 向 `handle` 发一个请求并收集响应。
async fn call(state: &Arc<MgmtState>, method: &str, path: &str, body: &str) -> Res {
    let req = http::Request::builder()
        .method(method)
        .uri(path)
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("request must build");
    let resp = handle(Arc::clone(state), req).await;
    let status = resp.status().as_u16();
    let allow = resp
        .headers()
        .get(http::header::ALLOW)
        .map(|v| v.to_str().unwrap_or_default().to_string());
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("response body must collect")
        .to_bytes();
    Res {
        status,
        allow,
        content_type,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// 两路由 + 三实例静态发现的通用 fixture（direct + lb，协议含 auto/rest-json）。
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

/// 内部字段埋哨兵串：断言任何响应都不回显（TLS 证书路径、tracing 过滤器）。
const SENTINEL_YAML: &str = r#"
server:
  listen: "127.0.0.1:8080"
  tls:
    cert: "SECRET_TLS_SENTINEL.pem"
observability:
  tracing_filter: "SECRET_DETAIL_SENTINEL"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    predicates: ["Path=/api/**"]
"#;

/// 快照/路由端点共用的路由摘要期望值（按 (order, id) 匹配顺序）。
/// 直连 URI 的字符串形态带尾斜杠（http::Uri 归一化为 origin-form）。
fn expected_routes() -> serde_json::Value {
    json!([
        {"id": "user-http", "order": 1, "uri": "http://127.0.0.1:9000/", "protocol": "auto"},
        {"id": "user-lb", "order": 2, "uri": "lb://user-svc", "protocol": "rest-json"}
    ])
}

/// 验证：GET /health 返回 ok 信封，且 content-type 为 JSON。
#[tokio::test]
async fn health_reports_ok_envelope() {
    let state = mgmt_state(TWO_ROUTES);
    let res = call(&state, "GET", "/health", "").await;

    assert_eq!(res.status, 200);
    assert_eq!(
        res.content_type, "application/json; charset=utf-8",
        "管理面响应必须声明 JSON content-type"
    );
    assert_eq!(
        res.json(),
        json!({"success": true, "data": {"status": "ok"}, "error": null})
    );
}

/// 验证：snapshot 端点输出版本、失败重载数、监听地址、路由计数与路由摘要。
#[tokio::test]
async fn snapshot_reports_version_listen_and_routes() {
    let state = mgmt_state(TWO_ROUTES);
    let res = call(&state, "GET", "/mgmt/v1/snapshot", "").await;

    assert_eq!(res.status, 200);
    assert_eq!(
        res.json(),
        json!({
            "success": true,
            "data": {
                "version": 1,
                "failed_reloads": 0,
                "listen": "127.0.0.1:8080",
                "route_count": 2,
                "routes": expected_routes()
            },
            "error": null
        })
    );
}

/// 验证：换快照/失败计数的变化实时反映在 snapshot 输出里。
#[tokio::test]
async fn snapshot_reflects_version_bump_and_reload_failures() {
    let state = mgmt_state(TWO_ROUTES);
    state.store().record_reload_failure();
    state.store().record_reload_failure();
    let states = Arc::clone(state.states());
    state.store().swap(compile_fixture(TWO_ROUTES, &states));

    let res = call(&state, "GET", "/mgmt/v1/snapshot", "").await;
    assert_eq!(res.status, 200);
    let data = &res.json()["data"];
    assert_eq!(data["version"], json!(2), "swap 之后版本号必须加一");
    assert_eq!(data["failed_reloads"], json!(2));
}

/// 验证：routes 端点按匹配顺序输出与 snapshot 一致的路由摘要。
#[tokio::test]
async fn routes_lists_routes_in_match_order() {
    let state = mgmt_state(TWO_ROUTES);
    let res = call(&state, "GET", "/mgmt/v1/routes", "").await;

    assert_eq!(res.status, 200);
    assert_eq!(
        res.json(),
        json!({"success": true, "data": {"routes": expected_routes()}, "error": null})
    );
}

/// 验证：空配置下 snapshot / routes 输出空路由列表（边界）。
#[tokio::test]
async fn empty_config_reports_zero_routes() {
    let state = mgmt_state("{}");

    let snap = call(&state, "GET", "/mgmt/v1/snapshot", "").await;
    assert_eq!(snap.status, 200);
    assert_eq!(snap.json()["data"]["route_count"], json!(0));
    assert_eq!(snap.json()["data"]["routes"], json!([]));

    let routes = call(&state, "GET", "/mgmt/v1/routes", "").await;
    assert_eq!(
        routes.json(),
        json!({"success": true, "data": {"routes": []}, "error": null})
    );
}

/// 验证：stats 聚合版本、失败重载、路由数与熔断器（总数/打开数）。
#[tokio::test]
async fn stats_aggregates_counts() {
    let state = mgmt_state(TWO_ROUTES);
    let route_state = state.states().route_state("user-lb");
    let cfg = BreakerConfig {
        sliding_window: 1,
        failure_rate: 0.5,
        min_calls: 1,
        open_duration: Duration::from_secs(30),
        half_open_max_calls: 1,
    };
    let _closed = route_state.breaker_for("127.0.0.1:50051", cfg.clone());
    let tripped = route_state.breaker_for("127.0.0.1:50052", cfg);
    tripped.check().expect("permit").record(false);
    state.store().record_reload_failure();

    let res = call(&state, "GET", "/mgmt/v1/stats", "").await;
    assert_eq!(res.status, 200);
    assert_eq!(
        res.json(),
        json!({
            "success": true,
            "data": {
                "version": 1,
                "failed_reloads": 1,
                "route_count": 2,
                "breaker_count": 2,
                "breaker_open_count": 1
            },
            "error": null
        })
    );
}

/// 表格驱动：已知路径上只放行声明的方法，其余一律 405（带 Allow 头）。
#[tokio::test]
async fn method_matrix_allows_only_declared_methods() {
    let state = mgmt_state(TWO_ROUTES);
    let cases: &[(&str, &str, &str, u16, Option<&str>)] = &[
        ("GET", "/health", "", 200, None),
        ("POST", "/health", "", 405, Some("GET")),
        ("PUT", "/health", "", 405, Some("GET")),
        ("DELETE", "/health", "", 405, Some("GET")),
        ("PATCH", "/health", "", 405, Some("GET")),
        ("HEAD", "/health", "", 405, Some("GET")),
        ("GET", "/mgmt/v1/snapshot", "", 200, None),
        ("POST", "/mgmt/v1/snapshot", "", 405, Some("GET")),
        ("PUT", "/mgmt/v1/snapshot", "", 405, Some("GET")),
        ("GET", "/mgmt/v1/routes", "", 200, None),
        ("DELETE", "/mgmt/v1/routes", "", 405, Some("GET")),
        ("GET", "/mgmt/v1/breakers", "", 200, None),
        ("POST", "/mgmt/v1/breakers", "", 405, Some("GET")),
        ("GET", "/mgmt/v1/stats", "", 200, None),
        ("POST", "/mgmt/v1/stats", "", 405, Some("GET")),
        (
            "POST",
            "/mgmt/v1/breakers/reset",
            "{\"route_id\":\"no-such-route\"}",
            200,
            None,
        ),
        ("GET", "/mgmt/v1/breakers/reset", "", 405, Some("POST")),
        ("PUT", "/mgmt/v1/breakers/reset", "", 405, Some("POST")),
        ("DELETE", "/mgmt/v1/breakers/reset", "", 405, Some("POST")),
    ];
    for &(method, path, body, want_status, want_allow) in cases {
        let res = call(&state, method, path, body).await;
        assert_eq!(res.status, want_status, "{method} {path}");
        match want_allow {
            Some(allow) => {
                assert_eq!(
                    res.allow.as_deref(),
                    Some(allow),
                    "{method} {path} 的 Allow 头"
                );
                assert_eq!(
                    res.json(),
                    json!({"success": false, "data": null, "error": "method not allowed"}),
                    "{method} {path}"
                );
            }
            None => assert_eq!(res.json()["success"], json!(true), "{method} {path}"),
        }
    }
}

/// 表格驱动：未知路径（含根路径/尾斜杠/大小写/编码变体）一律 404。
#[tokio::test]
async fn unknown_paths_return_404() {
    let state = mgmt_state(TWO_ROUTES);
    let paths = [
        "/",
        "/health/",
        "/Health",
        "/health%2F",
        "/mgmt/v1/",
        "/mgmt/v1/snapshot/",
        "/mgmt/v2/snapshot",
        "/mgmt/v1/unknown",
        "/api/v1/snapshot",
    ];
    for path in paths {
        let res = call(&state, "GET", path, "").await;
        assert_eq!(res.status, 404, "GET {path}");
        assert_eq!(
            res.json(),
            json!({"success": false, "data": null, "error": "not found"}),
            "GET {path}"
        );
    }
}

/// 验证：查询串不参与路由（边界），命中同一端点。
#[tokio::test]
async fn query_string_is_ignored_for_routing() {
    let state = mgmt_state(TWO_ROUTES);
    for path in ["/health?x=1", "/mgmt/v1/routes?x=1&y=2"] {
        let res = call(&state, "GET", path, "").await;
        assert_eq!(res.status, 200, "GET {path}");
        assert_eq!(res.json()["success"], json!(true), "GET {path}");
    }
}

/// 验证：任何响应都不回显内部细节（哨兵串：TLS 证书路径、tracing 过滤器）。
#[tokio::test]
async fn internal_secrets_never_leak_in_responses() {
    let state = mgmt_state(SENTINEL_YAML);
    // 先证哨兵确实进了内部字段（否则断言是空洞的）。
    let snap = state.store().load();
    assert_eq!(
        snap.tracing_filter.as_deref(),
        Some("SECRET_DETAIL_SENTINEL"),
        "哨兵必须落在内部字段里"
    );
    assert!(
        snap.server
            .tls
            .cert
            .to_string_lossy()
            .contains("SECRET_TLS_SENTINEL"),
        "哨兵必须落在 TLS 证书路径里"
    );

    let calls: &[(&str, &str, &str)] = &[
        ("GET", "/health", ""),
        ("GET", "/mgmt/v1/snapshot", ""),
        ("GET", "/mgmt/v1/routes", ""),
        ("GET", "/mgmt/v1/breakers", ""),
        ("GET", "/mgmt/v1/stats", ""),
        ("POST", "/mgmt/v1/breakers/reset", "not-json"),
        ("GET", "/nope", ""),
        ("PUT", "/health", ""),
    ];
    for &(method, path, body) in calls {
        let res = call(&state, method, path, body).await;
        assert!(
            !res.body.contains("SECRET_DETAIL_SENTINEL"),
            "{method} {path} 泄露 tracing 过滤器: {}",
            res.body
        );
        assert!(
            !res.body.contains("SECRET_TLS_SENTINEL"),
            "{method} {path} 泄露证书路径: {}",
            res.body
        );
    }
}
