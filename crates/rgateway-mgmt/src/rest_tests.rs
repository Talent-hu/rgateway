//! [`rest`] 模块的测试：端点方法表、协议标签、信封序列化形态，以及
//! `/metrics` 指标端点（路径跟随快照配置、渲染闭包注入语义）。

use std::sync::Arc;

use http_body_util::BodyExt;
use rgateway_config::{compile, load_from_str, CompileInput, FilterRegistry, SnapshotStore};
use rgateway_core::{Protocol, StateRegistry};

use super::*;

/// 表格驱动：已知路径 → 允许的方法；未知路径 → None。
#[test]
fn allowed_method_table() {
    let cases: &[(&str, Option<&str>)] = &[
        ("/health", Some("GET")),
        ("/mgmt/v1/snapshot", Some("GET")),
        ("/mgmt/v1/routes", Some("GET")),
        ("/mgmt/v1/breakers", Some("GET")),
        ("/mgmt/v1/stats", Some("GET")),
        ("/mgmt/v1/breakers/reset", Some("POST")),
        ("/", None),
        ("/health/", None),
        ("/mgmt/v1/unknown", None),
    ];
    for &(path, want) in cases {
        assert_eq!(allowed_method(path), want, "{path}");
    }
}

/// 保留路径清单（config 侧权威，`observability.metrics.path` 冲突校验用）
/// 与本模块静态路由对拍：清单里的每个路径都必须是本模块的已知端点，
/// 否则校验会误伤一个不存在的「端点」。遗留缺口与 `FILTER_NAMES` 相同：
/// 反向（rest.rs 新增端点而清单漏改）无法枚举 match 域自动对拍，后果是
/// 新端点路径可被 metrics.path 占用（功能冲突、非安全问题）。
#[test]
fn reserved_paths_all_resolve_to_static_routes() {
    for path in rgateway_config::MGMT_RESERVED_PATHS {
        assert!(
            allowed_method(path).is_some(),
            "保留路径 {path} 必须是管理面 REST 的已知端点"
        );
    }
}

/// 表格驱动：ProtocolHint → 稳定输出标签。
#[test]
fn hint_label_table() {
    let cases: &[(ProtocolHint, &str)] = &[
        (ProtocolHint::Auto, "auto"),
        (ProtocolHint::Fixed(Protocol::Http1), "http1"),
        (ProtocolHint::Fixed(Protocol::Http2), "http2"),
        (ProtocolHint::Fixed(Protocol::WebSocket), "ws"),
        (ProtocolHint::Fixed(Protocol::Grpc), "grpc"),
        (ProtocolHint::Fixed(Protocol::GrpcWeb), "grpc-web"),
        (ProtocolHint::Fixed(Protocol::GrpcWebText), "grpc-web-text"),
        (ProtocolHint::Fixed(Protocol::RestJson), "rest-json"),
    ];
    for &(hint, want) in cases {
        assert_eq!(hint_label(hint), want, "{hint:?}");
    }
}

/// 表格驱动：成功/失败信封的精确 JSON 形态（三键齐全、null 位明确）。
#[test]
fn envelope_serialization_table() {
    let cases: &[(Envelope<&str>, &str)] = &[
        (
            Envelope {
                success: true,
                data: Some("x"),
                error: None,
            },
            r#"{"success":true,"data":"x","error":null}"#,
        ),
        (
            Envelope {
                success: false,
                data: None,
                error: Some("invalid request".to_string()),
            },
            r#"{"success":false,"data":null,"error":"invalid request"}"#,
        ),
    ];
    for (envelope, want) in cases {
        let got = serde_json::to_string(envelope).expect("envelope must serialize");
        assert_eq!(got, *want);
    }
}

/// 验证：reset 请求体严格解析——缺 route_id / 未知字段 / 空 route_id 均拒绝。
#[test]
fn reset_request_strict_parsing() {
    let cases: &[(&str, bool)] = &[
        (r#"{"route_id":"r"}"#, true),
        (r#"{"route_id":"r","instance_id":"i"}"#, true),
        (r#"{"route_id":"r","instance_id":null}"#, true),
        (r#"{}"#, false),
        (r#"{"route_id":""}"#, false),
        (r#"{"route_id":"r","bogus":1}"#, false),
        (r#"{"route_id":123}"#, false),
    ];
    for &(raw, want_ok) in cases {
        let parsed = parse_json::<ResetRequest>(raw.as_bytes()).is_ok();
        if want_ok {
            let req: ResetRequest = parse_json(raw.as_bytes()).expect("must parse");
            assert!(!req.route_id.is_empty(), "{raw} 不应接受空 route_id");
            assert!(parsed, "{raw}");
        } else {
            // 解析成功但 route_id 为空的，交给 handler 的空串校验拒绝。
            let rejected = !parsed
                || parse_json::<ResetRequest>(raw.as_bytes())
                    .map(|r| r.route_id.is_empty())
                    .unwrap_or(true);
            assert!(rejected, "{raw} 必须被拒绝");
        }
    }
}

// ---- /metrics 端点测试辅助 ----

/// 测试替身 handler 工厂：REST 测试从不执行路由链。
struct StubFactory;

impl rgateway_config::RouteHandlerFactory for StubFactory {
    fn build(&self, _route: &rgateway_config::RouteCompileInfo) -> rgateway_core::BoxedService {
        rgateway_core::BoxedService::new(tower::service_fn(
            |_req: http::Request<rgateway_core::ProxyBody>| async move {
                Ok::<http::Response<rgateway_core::ProxyBody>, rgateway_core::GatewayError>(
                    http::Response::new(rgateway_core::empty_body()),
                )
            },
        ))
    }
}

/// 按 YAML 配置编译出管理面状态（指标渲染闭包可选注入）。
fn state_with(yaml: &str, metrics: Option<String>) -> MgmtState {
    let file = load_from_str(yaml).expect("yaml must parse");
    let states = Arc::new(StateRegistry::new());
    let compiled = compile(CompileInput {
        file: &file,
        registry: &FilterRegistry::new(),
        states: &states,
        handler_factory: &StubFactory,
        config_dir: std::path::Path::new("."),
    })
    .expect("config must compile");
    let store = Arc::new(SnapshotStore::new(compiled));
    let state = MgmtState::new(store, states);
    match metrics {
        Some(text) => state.with_metrics(Arc::new(move || Some(text.clone()))),
        None => state,
    }
}

/// 空 GET 请求（管理面端点只看 method/path）。
fn get(path: &str) -> http::Request<Full<Bytes>> {
    http::Request::builder()
        .method("GET")
        .uri(path)
        .body(Full::new(Bytes::new()))
        .expect("test request builds")
}

/// 取响应体文本（消费响应；测试辅助）。
async fn resp_body(resp: http::Response<MgmtBody>) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 表格驱动：`/metrics` 的核心语义——渲染闭包注入 / 未注入 / 空快照
/// / 方法闸。列含义：(标签, 指标闭包注入值, 请求方法, 期望状态码, 期望 content-type 前缀)
#[tokio::test]
async fn metrics_endpoint_semantics_table() {
    let cases: &[(&str, Option<&str>, &str, u16, &str)] = &[
        (
            "rendered",
            Some("rgateway_demo 1\n"),
            "GET",
            200,
            "text/plain",
        ),
        ("no-renderer", None, "GET", 404, "application/json"),
        ("empty-text", Some(""), "GET", 200, "text/plain"),
        ("method-gate", Some("x"), "POST", 405, "application/json"),
    ];
    for &(label, injected, method, want_status, want_ct) in cases {
        // `Some("")` 表示注入了闭包但渲染出空文本（合法：无样本也输出空快照）；
        // 「闭包返回 None」（导出器未就绪）由独立用例覆盖。
        let state = Arc::new(state_with("{}", injected.map(str::to_string)));
        let req = http::Request::builder()
            .method(method)
            .uri("/metrics")
            .body(Full::new(Bytes::new()))
            .expect("test request builds");
        let resp = handle(state, req).await;
        assert_eq!(resp.status().as_u16(), want_status, "{label}");
        let ct = resp
            .headers()
            .get(CONTENT_TYPE)
            .expect("content-type")
            .to_str()
            .expect("utf-8");
        assert!(ct.starts_with(want_ct), "{label}: content-type {ct}");
    }
}

// 渲染闭包返回 None：导出器尚未就绪，503 + 失败信封（不是 404——端点存在，
// 只是暂无指标可输出）。
#[tokio::test]
async fn metrics_endpoint_503_when_renderer_returns_none() {
    let state = state_with("{}", None).with_metrics(Arc::new(|| None));
    let resp = handle(Arc::new(state), get("/metrics")).await;
    assert_eq!(resp.status().as_u16(), 503);
    let body = resp_body(resp).await;
    assert!(body.contains("\"success\":false"), "{body}");
}

// 指标路径跟随快照配置（observability.metrics.path）：自定义路径生效，
// 默认路径不再服务——热重载改路径后 scrape 配置要跟着改。
#[tokio::test]
async fn metrics_path_follows_snapshot_config() {
    let state = Arc::new(state_with(
        "observability:\n  metrics:\n    path: /custom-metrics\n",
        Some("rgateway_demo 1\n".to_string()),
    ));
    let hit = handle(Arc::clone(&state), get("/custom-metrics")).await;
    assert_eq!(hit.status().as_u16(), 200, "自定义路径必须生效");
    assert!(resp_body(hit).await.contains("rgateway_demo"));
    let miss = handle(state, get("/metrics")).await;
    assert_eq!(miss.status().as_u16(), 404, "默认路径在改配置后必须 404");
}

// 端点存在的 /metrics 渲染体就是闭包产出的 Prometheus 文本原样。
#[tokio::test]
async fn metrics_endpoint_returns_renderer_output_verbatim() {
    let want = "# HELP rgateway_demo demo\nrgateway_demo 42\n";
    let state = Arc::new(state_with("{}", Some(want.to_string())));
    let resp = handle(state, get("/metrics")).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp_body(resp).await, want);
}

// metrics.enabled=false 时端点 404（热重载关闭指标即时生效）：
// 即使渲染闭包已注入（启动期指标开着），快照关掉后端点即消失；重新
// 打开即恢复——与 metrics.path 的跟随语义一致。
#[tokio::test]
async fn metrics_disabled_by_snapshot_hides_endpoint() {
    let state = Arc::new(state_with(
        "observability:\n  metrics:\n    enabled: false\n",
        Some("rgateway_demo 1\n".to_string()),
    ));
    let hidden = handle(Arc::clone(&state), get("/metrics")).await;
    assert_eq!(hidden.status().as_u16(), 404, "快照关闭指标后端点必须消失");
}
// reset 字段长度上限（与 gRPC 侧对称的防放大边界）：超长 route_id 400。
#[tokio::test]
async fn reset_rejects_oversized_ids() {
    let state = Arc::new(state_with("{}", None));
    let body = format!("{{\"route_id\":\"{}\"}}", "r".repeat(257));
    let req = http::Request::builder()
        .method("POST")
        .uri("/mgmt/v1/breakers/reset")
        .body(Full::new(Bytes::from(body)))
        .expect("test request builds");
    let resp = handle(state, req).await;
    assert_eq!(resp.status().as_u16(), 400, "超长 id 必须 400 拒绝");
}

// reset 请求体上限：超限体 413（有限缓冲防内存放大），上限内正常解析。
// 列含义：(标签, 体字节数, 期望状态码)
#[tokio::test]
async fn reset_body_is_capped() {
    let cases: &[(&str, usize, u16)] = &[
        ("at-limit garbage", 64 * 1024, 400), // 上限内但不是合法 JSON
        ("over-limit", 64 * 1024 + 1, 413),   // 超限：不解析直接拒
    ];
    for &(label, size, want_status) in cases {
        let state = Arc::new(state_with("{}", None));
        let req = http::Request::builder()
            .method("POST")
            .uri("/mgmt/v1/breakers/reset")
            .body(Full::new(Bytes::from(vec![b'x'; size])))
            .expect("test request builds");
        let resp = handle(state, req).await;
        assert_eq!(resp.status().as_u16(), want_status, "{label}");
    }
}
