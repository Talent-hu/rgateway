//! 集成测试：真实配置管线（load_from_str → validate → compile）编排五个
//! 内置过滤器 + `defaults.filters` 追加，断言编译成功、链可跑通、错误
//! 定位精确到 `routes[id].filters[i]`。
//!
//! handler 工厂用假实现替身（返回 200），discovery 用 `type: static`——
//! 只验证过滤器编译/组合语义，不碰真实网络。

use std::net::SocketAddr;
use std::sync::Arc;

use rgateway_config::{
    compile, load_from_str, validate, CompileInput, ConfigError, RouteCompileInfo,
    RouteHandlerFactory,
};
use rgateway_core::{
    empty_body, BoxedService, ErrorKind, GatewayError, Protocol, ProxyBody, RequestCtx,
    StateRegistry,
};
use rgateway_filters::registry;
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

/// 假 handler 工厂：任何路由都产出返回 200 的最内层服务。
struct StubHandler;

impl RouteHandlerFactory for StubHandler {
    fn build(&self, _route: &RouteCompileInfo) -> BoxedService {
        BoxCloneService::new(service_fn(|_req: http::Request<ProxyBody>| async move {
            Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
        }))
    }
}

/// 覆盖五个内置过滤器（结构化 + 紧凑两种写法）与 `defaults.filters`
/// 追加项的完整配置。
const FULL_CONFIG: &str = r#"
server: { listen: "127.0.0.1:0" }
discovery:
  providers:
    - type: static
      services:
        user-svc: [ { address: "127.0.0.1:50051", weight: 1 } ]
defaults:
  filters:
    - "RateLimit=key:route,rate:1000,period:1s"
routes:
  - id: r1
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
    filters:
      - { name: RateLimit, key: client_ip, rate: 1, period: 100s }
      - { name: Retry, retries: 2, methods: [GET, HEAD], statuses: [502, 503],
          backoff: { initial: 10ms, max: 100ms, multiplier: 2, jitter: none } }
      - { name: Timeout, per_attempt: 3s }
      - "BodyLimit=max_body:1KB"
      - { name: CircuitBreaker, failure_rate: "50%" }
"#;

/// helper：编译一段 YAML，返回 (编译产物 或 期望成功失败断言见调用方)。
fn compile_yaml(yaml: &str) -> Result<rgateway_config::CompiledConfig, ConfigError> {
    let file = load_from_str(yaml).expect("yaml must parse");
    let errors = validate(&file);
    assert!(errors.is_empty(), "config must validate: {errors:?}");
    let states = Arc::new(StateRegistry::new());
    let registry = registry();
    let handler = StubHandler;
    compile(CompileInput {
        file: &file,
        registry: &registry,
        states: &states,
        handler_factory: &handler,
        config_dir: &std::env::temp_dir(),
    })
}

/// 验证：五过滤器 + defaults 追加的配置编译成功，产物含一条路由。
#[test]
fn compiles_all_builtin_filters_with_defaults() {
    let compiled = compile_yaml(FULL_CONFIG).expect("compile must succeed");
    assert_eq!(compiled.routes.routes().len(), 1);
    assert_eq!(compiled.routes.routes()[0].id.as_ref(), "r1");
}

/// 验证：编译出的链可跑通真实组合语义——client_ip 限流（rate=1/100s）
/// 第二发 429，换 IP 独立放行；Target/Breaker/Timeout 叠加无冲突。
#[tokio::test(start_paused = true)]
async fn compiled_chain_enforces_rate_limit_end_to_end() {
    let compiled = compile_yaml(FULL_CONFIG).expect("compile must succeed");
    let route = compiled.routes.routes()[0].clone();
    let chain = route.chain_clone().expect("chain");

    let send = |peer: SocketAddr| {
        let chain = chain.clone();
        async move {
            let ctx = Arc::new(RequestCtx::new(Protocol::Http1, Some(peer)));
            let mut req = http::Request::new(empty_body());
            req.extensions_mut().insert(ctx);
            chain.oneshot(req).await
        }
    };

    let a: SocketAddr = "10.0.0.1:1000".parse().expect("addr");
    let b: SocketAddr = "10.0.0.2:1000".parse().expect("addr");
    // 每行 = (源地址, 期望 HTTP 状态)
    let cases = [(a, 200u16), (a, 429), (b, 200)];
    for (peer, want) in cases {
        let result = send(peer).await;
        match result {
            Ok(resp) => assert_eq!(resp.status().as_u16(), want, "from {peer}"),
            Err(err) => {
                assert_eq!(want, 429, "from {peer}: unexpected error {err}");
                assert_eq!(err.kind(), ErrorKind::RateLimited);
            }
        }
    }
}

/// 验证：缺参数的校验错误精确定位到 `routes[id].filters[i]`。
#[test]
fn missing_param_error_points_to_route_filter_item() {
    let yaml = r#"
discovery:
  providers:
    - type: static
      services:
        user-svc: [ { address: "127.0.0.1:50051" } ]
routes:
  - id: r1
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
    filters:
      - { name: Timeout, per_attempt: 3s }
      - { name: RateLimit, key: client_ip, period: 1s }
"#;
    let file = load_from_str(yaml).expect("yaml must parse");
    let errors = validate(&file);
    assert!(!errors.is_empty(), "missing rate must be rejected");
    let located: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        located.iter().any(|e| e.contains("routes[r1].filters[1]")),
        "error must point at routes[r1].filters[1]: {located:?}"
    );
    assert!(
        located.iter().any(|e| e.contains("rate")),
        "error must name the missing parameter: {located:?}"
    );
}

/// 验证：defaults.filters 的缺参错误定位到 `defaults.filters[i]`
/// （默认过滤器追加到每条路由，缺参同样致命）。
#[test]
fn missing_param_error_points_to_defaults_filter_item() {
    let yaml = r#"
discovery:
  providers:
    - type: static
      services:
        user-svc: [ { address: "127.0.0.1:50051" } ]
defaults:
  filters:
    - { name: BodyLimit, max_body: 1KB }
    - { name: RateLimit, key: route }
routes:
  - id: r1
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
"#;
    let file = load_from_str(yaml).expect("yaml must parse");
    let errors = validate(&file);
    let located: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        located.iter().any(|e| e.contains("defaults.filters[1]")),
        "error must point at defaults.filters[1]: {located:?}"
    );
}

/// 验证：构建期参数语义错误（validate 放行的非法 key 维度）在 compile
/// 阶段报错，且定位精确到该过滤器条目。
#[test]
fn build_error_points_to_the_offending_filter_item() {
    let yaml = r#"
discovery:
  providers:
    - type: static
      services:
        user-svc: [ { address: "127.0.0.1:50051" } ]
routes:
  - id: r1
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
    filters:
      - { name: RateLimit, key: bogus, rate: 5 }
"#;
    let file = load_from_str(yaml).expect("yaml must parse");
    let errors = validate(&file);
    assert!(errors.is_empty(), "key:bogus passes validate: {errors:?}");
    let err = match compile_yaml(yaml) {
        Err(err) => err,
        Ok(_) => panic!("compile must fail on unknown key dimension"),
    };
    match &err {
        ConfigError::Compile { location, message } => {
            assert_eq!(
                location.to_string(),
                "routes[r1].filters[0]",
                "compile error must locate the filter item"
            );
            assert!(message.contains("RateLimit"), "{message}");
        }
        other => panic!("expected ConfigError::Compile, got {other:?}"),
    }
}
