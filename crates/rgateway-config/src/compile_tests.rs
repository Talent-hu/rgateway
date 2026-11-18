//! [`compile`](super::compile) 的测试：链组装次序（golden）、LB preserved
//! 跨热重载语义、transcoding 路径解析、mgmt/observability 透传与错误位置。
//! 测试替身在 `compile_tests_util`；`defaults.filters` 语义在
//! `compile_defaults_tests`；server 设置解析表在 `compile_settings_tests`。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rgateway_core::{
    empty_body, Protocol, ProtocolHint, RandomBalancer, RequestCtx, RoundRobin, StateRegistry,
};
use tower::ServiceExt;

use super::chain::{preserved_balancer, protocol_hint, BalancerSlot};
use crate::error::{ConfigError, Location};
use crate::spec::FilterRegistry;

#[path = "compile_tests_util.rs"]
mod util;
use self::util::*;

#[path = "compile_defaults_tests.rs"]
mod defaults_tests;

#[path = "compile_settings_tests.rs"]
mod settings_tests;

/// 验证：路由声明的过滤器按规范槽位顺序执行（golden 执行日志），handler 在最内层。
#[tokio::test]
async fn chain_order_golden_follows_canonical_slots() {
    let yaml = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    predicates: ["Path=/api/**"]
    filters:
      - "Retry=retries:1"
      - "RateLimit=key:client_ip,rate:100,period:1s"
      - "JwtAuth=issuer:https://idp"
      - "Compression=min_size:1024"
"#;
    let file = parse_yaml(yaml);
    let log = new_log();
    let registry = registry_with(&log, &["Retry", "RateLimit", "JwtAuth", "Compression"]);
    let states = Arc::new(StateRegistry::new());
    let handler = RecordingHandler::new(Arc::clone(&log));
    let dir = test_dir("chain-order");
    let compiled = compile_ok(&file, &registry, &states, &handler, &dir);
    assert_eq!(compiled.routes.len(), 1);
    send(&compiled.routes.routes()[0].chain_clone().expect("chain")).await;
    assert_eq!(
        drain(&log),
        vec!["Compression", "RateLimit", "JwtAuth", "Retry", "handler"],
        "declared filters must run in canonical slot order, handler innermost"
    );
}

/// 验证：round-robin 轮询计数器跨重新编译（热重载）继续推进；同算法名
/// 复用同一个 balancer 实例（`Arc` 指针同一），holder 记录算法名。
#[tokio::test]
async fn round_robin_counter_preserved_across_compiles() {
    let yaml = r#"
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
    uri: "lb://user-svc"
    predicates: ["Path=/api/**"]
"#;
    let file = parse_yaml(yaml);
    let registry = FilterRegistry::new();
    let states = Arc::new(StateRegistry::new());
    let handler = RecordingHandler::new(new_log());
    let dir = test_dir("lb-keep");

    let first = compile_ok(&file, &registry, &states, &handler, &dir);
    send(&first.routes.routes()[0].chain_clone().expect("chain")).await;
    send(&first.routes.routes()[0].chain_clone().expect("chain")).await;

    let second = compile_ok(&file, &registry, &states, &handler, &dir);
    send(&second.routes.routes()[0].chain_clone().expect("chain")).await;
    send(&second.routes.routes()[0].chain_clone().expect("chain")).await;

    assert_eq!(
        handler.targets(),
        vec![
            "127.0.0.1:50051".to_string(),
            "127.0.0.1:50052".to_string(),
            "127.0.0.1:50053".to_string(),
            "127.0.0.1:50051".to_string(),
        ],
        "the round-robin counter must continue across recompiles"
    );

    // 同一算法名复用同一个 holder 实例。
    let state = states.route_state("user-http");
    let rr_a = preserved_balancer(&state, "round_robin").expect("round_robin");
    let rr_b = preserved_balancer(&state, "round_robin").expect("round_robin");
    assert!(
        Arc::ptr_eq(&rr_a, &rr_b),
        "same algorithm name must reuse the balancer instance"
    );
    let holder: Arc<BalancerSlot<RoundRobin>> =
        state.preserved("lb", || panic!("holder must exist after compile"));
    assert_eq!(holder.name, "round_robin");
}

/// 验证：LB 算法名变化时 preserved 槽位被替换为新 balancer（指针不同），
/// 且切回原算法名也不会复活旧实例。
#[test]
fn changing_balancer_algorithm_replaces_the_holder() {
    let yaml = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    load_balancer: "random"
"#;
    let file = parse_yaml(yaml);
    let states = Arc::new(StateRegistry::new());
    let state = states.route_state("user-http");
    let rr = preserved_balancer(&state, "round_robin").expect("round_robin");
    let holder: Arc<BalancerSlot<RoundRobin>> =
        state.preserved("lb", || panic!("holder must exist"));

    compile_ok(
        &file,
        &FilterRegistry::new(),
        &states,
        &SlowHandler {
            latency: Duration::ZERO,
        },
        Path::new("."),
    );

    let random = preserved_balancer(&state, "random").expect("random");
    assert!(
        !Arc::ptr_eq(&rr, &random),
        "changing the algorithm name must produce a fresh balancer"
    );
    let new_holder: Arc<BalancerSlot<RandomBalancer>> =
        state.preserved("lb", || panic!("holder must be replaced"));
    assert_eq!(new_holder.name, "random");
    assert_eq!(holder.name, "round_robin");

    let fresh_rr = preserved_balancer(&state, "round_robin").expect("round_robin");
    assert!(
        !Arc::ptr_eq(&rr, &fresh_rr),
        "switching back must also produce a fresh balancer"
    );
    let again = preserved_balancer(&state, "round_robin").expect("round_robin");
    assert!(Arc::ptr_eq(&fresh_rr, &again), "same name reuses again");
}

/// 验证：transcoding 描述符解析为配置目录下的绝对路径；handler 只构建
/// 一次并拿到正确的 id 与协议提示。
#[test]
fn transcoding_descriptor_path_is_absolute() {
    let yaml = r#"
routes:
  - id: user-rest
    uri: "lb://user-svc"
    protocol: rest-json
    predicates: ["Path=/v1/**"]
    transcoding:
      descriptor: "proto/descriptor.bin"
"#;
    let file = parse_yaml(yaml);
    let handler = RecordingHandler::new(new_log());
    let dir = test_dir("transcoding");
    let compiled = compile_ok(
        &file,
        &FilterRegistry::new(),
        &Arc::new(StateRegistry::new()),
        &handler,
        &dir,
    );
    assert_eq!(compiled.routes.len(), 1);
    let infos = handler.infos();
    let info = infos.first().expect("handler must be built once");
    assert_eq!(info.id.as_ref(), "user-rest");
    assert_eq!(info.protocol_hint, ProtocolHint::Fixed(Protocol::RestJson));
    let transcoding = info.transcoding.as_ref().expect("transcoding info");
    let descriptor = Path::new(&transcoding.descriptor);
    assert!(descriptor.is_absolute(), "descriptor must be absolute");
    assert_eq!(descriptor, dir.join("proto/descriptor.bin").as_path());
}

/// 验证：未知过滤器名与未知 LB 算法的错误分别精确定位到
/// `routes[id].filters[i]` 与 `routes[id].load_balancer`。
#[test]
fn unknown_filter_and_balancer_errors_carry_exact_locations() {
    let filter_yaml = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    filters:
      - "RateLimit=key:client_ip,rate:1,period:1s"
      - "NoSuchFilter=x:1"
"#;
    let file = parse_yaml(filter_yaml);
    let log = new_log();
    let registry = registry_with(&log, &["RateLimit"]);
    let err = compile_err(
        &file,
        &registry,
        &Arc::new(StateRegistry::new()),
        &RecordingHandler::new(log),
        Path::new("."),
    );
    match err {
        ConfigError::UnknownFilter { location, name } => {
            assert_eq!(location.to_string(), "routes[user-http].filters[1]");
            assert_eq!(name, "NoSuchFilter");
        }
        other => panic!("expected UnknownFilter, got {other:?}"),
    }

    let lb_yaml = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    load_balancer: "least_conn"
"#;
    let file = parse_yaml(lb_yaml);
    let err = compile_err(
        &file,
        &FilterRegistry::new(),
        &Arc::new(StateRegistry::new()),
        &SlowHandler {
            latency: Duration::ZERO,
        },
        Path::new("."),
    );
    match err {
        ConfigError::Value { location, message } => {
            assert_eq!(location.to_string(), "routes[user-http].load_balancer");
            assert!(message.contains("least_conn"), "{message}");
        }
        other => panic!("expected ConfigError::Value, got {other:?}"),
    }
}

/// 验证：协议名 → ProtocolHint 的完整映射表；未知协议报错并定位到
/// `routes[id].protocol`。
#[test]
fn protocol_hint_table() {
    let cases = [
        ("auto", ProtocolHint::Auto),
        ("http1", ProtocolHint::Fixed(Protocol::Http1)),
        ("http2", ProtocolHint::Fixed(Protocol::Http2)),
        ("grpc", ProtocolHint::Fixed(Protocol::Grpc)),
        ("grpc-web", ProtocolHint::Fixed(Protocol::GrpcWeb)),
        ("grpc-web-text", ProtocolHint::Fixed(Protocol::GrpcWebText)),
        ("rest-json", ProtocolHint::Fixed(Protocol::RestJson)),
        ("websocket", ProtocolHint::Fixed(Protocol::WebSocket)),
    ];
    for (raw, want) in cases {
        let got = protocol_hint(raw, &Location::route("user-http")).expect("valid protocol");
        assert_eq!(got, want, "{raw}");
    }
    let err = protocol_hint("ftp", &Location::route("user-http").field("protocol"))
        .expect_err("unknown protocol");
    match err {
        ConfigError::Value { location, .. } => {
            assert_eq!(location.to_string(), "routes[user-http].protocol");
        }
        other => panic!("expected ConfigError::Value, got {other:?}"),
    }
}

/// 验证：5 类谓词（Path/Method/Header/Host/Query）都能编译出实例；
/// 未知谓词名定位到 `routes[id].predicates[i]`。
#[test]
fn predicates_compile_and_unknown_predicate_is_located() {
    let yaml = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    predicates:
      - "Path=/api/**"
      - "Method=GET,POST"
      - "Header=X-Tenant,\\d+"
      - "Host=api\\.example\\.com"
      - "Query=page,\\d+"
"#;
    let file = parse_yaml(yaml);
    let compiled = compile_ok(
        &file,
        &FilterRegistry::new(),
        &Arc::new(StateRegistry::new()),
        &SlowHandler {
            latency: Duration::ZERO,
        },
        Path::new("."),
    );
    assert_eq!(compiled.routes.routes()[0].predicates.len(), 5);

    let bad_yaml = r#"
routes:
  - id: user-http
    uri: "http://127.0.0.1:9000"
    predicates: ["Nope=x"]
"#;
    let file = parse_yaml(bad_yaml);
    let err = compile_err(
        &file,
        &FilterRegistry::new(),
        &Arc::new(StateRegistry::new()),
        &SlowHandler {
            latency: Duration::ZERO,
        },
        Path::new("."),
    );
    match err {
        ConfigError::UnknownPredicate { location, name } => {
            assert_eq!(location.to_string(), "routes[user-http].predicates[0]");
            assert_eq!(name, "Nope");
        }
        other => panic!("expected UnknownPredicate, got {other:?}"),
    }
}

/// 验证：mgmt/observability 设置原样拷入 [`CompiledConfig`]，states 的
/// `Arc` 与输入指针同一（透传而非复制）。
#[test]
fn compiled_copies_mgmt_and_observability_settings() {
    let yaml = r#"
mgmt: { listen: "127.0.0.1:9191", grpc: true }
observability:
  access_log: { enabled: true, format: text }
  metrics: { enabled: false, path: "/m" }
  tracing_filter: "rgateway=debug"
"#;
    let file = parse_yaml(yaml);
    let states = Arc::new(StateRegistry::new());
    let compiled = compile_ok(
        &file,
        &FilterRegistry::new(),
        &states,
        &SlowHandler {
            latency: Duration::ZERO,
        },
        Path::new("."),
    );
    assert_eq!(compiled.mgmt_listen, "127.0.0.1:9191".parse().unwrap());
    assert!(compiled.mgmt_grpc);
    assert!(compiled.access_log_enabled);
    assert!(!compiled.access_log_json, "text format is not json");
    assert!(!compiled.metrics_enabled);
    assert_eq!(compiled.metrics_path, "/m");
    assert_eq!(compiled.tracing_filter.as_deref(), Some("rgateway=debug"));
    assert!(
        Arc::ptr_eq(&compiled.states, &states),
        "states pass through"
    );
}

/// 验证：总 Deadline 预算的优先级 route > defaults > server 默认值
/// （tokio 虚拟时间下 10s 延迟 handler 是否超时报错）。
#[tokio::test(start_paused = true)]
async fn deadline_budget_precedence_route_then_defaults() {
    let handler = SlowHandler {
        latency: Duration::from_secs(10),
    };
    let cases = [
        // (route request_timeout, defaults request_timeout, 是否期望报错)
        (Some("1s"), Some("100s"), true),
        (None, Some("1s"), true),
        (Some("100s"), Some("1s"), false),
        (None, None, false), // server 默认 30s 大于 10s 延迟，不超时
    ];
    for (route_timeout, defaults_timeout, want_err) in cases {
        let mut yaml = String::from("defaults:\n");
        if let Some(t) = defaults_timeout {
            yaml.push_str(&format!("  request_timeout: \"{t}\"\n"));
        }
        yaml.push_str("routes:\n  - id: user-http\n    uri: \"http://127.0.0.1:9000\"\n");
        if let Some(t) = route_timeout {
            yaml.push_str(&format!("    request_timeout: \"{t}\"\n"));
        }
        let file = parse_yaml(&yaml);
        let compiled = compile_ok(
            &file,
            &FilterRegistry::new(),
            &Arc::new(StateRegistry::new()),
            &handler,
            Path::new("."),
        );
        let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
        let mut req = http::Request::new(empty_body());
        req.extensions_mut().insert(ctx);
        let result = compiled.routes.routes()[0]
            .chain_clone()
            .expect("chain")
            .oneshot(req)
            .await;
        assert_eq!(
            result.is_err(),
            want_err,
            "route={route_timeout:?} defaults={defaults_timeout:?} err={:?}",
            result.as_ref().err()
        );
    }
}
