//! `defaults.filters` 生效语义的测试：每个路由的生效过滤器列表 = 路由
//! 自身声明的 filters 之后追加 `defaults.filters`（Spring Cloud Gateway
//! 语义：default filters 追加到每个路由之后），并按规范槽位
//! [`FilterSlot`](crate::spec::FilterSlot) 分组、槽内保持 config 顺序。
//! 默认过滤器与路由过滤器走完全相同的 [`FilterSpec`](crate::spec::FilterSpec)
//! 归一化、[`slot_for`](crate::spec::slot_for) 分类与
//! [`FilterFactory`](crate::spec::FilterFactory) 构建路径，只是错误位置
//! 指向 `defaults.filters[<i>]` 而不是 `routes[<id>].filters[<i>]`。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rgateway_core::StateRegistry;

use crate::error::ConfigError;

use super::util::*;

/// 验证：defaults 对每个路由生效并落在规范槽位次序上（RateLimit 无论
/// 声明顺序都包在 Breaker 外侧）；空的 `defaults.filters` 不改变任何行为。
#[tokio::test]
async fn defaults_filters_land_in_canonical_slot_order() {
    // (配置 YAML, 期望的外→内执行日志)
    let cases: &[(&str, &[&str])] = &[
        (
            r#"
defaults: { filters: [] }
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            &["handler"],
        ),
        (
            r#"
defaults:
  filters:
    - { name: CircuitBreaker, sliding_window: 20 }
    - "RateLimit=key:client_ip,rate:100,period:1s"
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            &["RateLimit", "CircuitBreaker", "handler"],
        ),
    ];
    for (yaml, want) in cases {
        let file = parse_yaml(yaml);
        let log = new_log();
        let registry = registry_with(&log, &["CircuitBreaker", "RateLimit"]);
        let handler = RecordingHandler::new(Arc::clone(&log));
        let compiled = compile_ok(
            &file,
            &registry,
            &Arc::new(StateRegistry::new()),
            &handler,
            &test_dir("defaults-slot-order"),
        );
        send(&compiled.routes.routes()[0].chain_clone().expect("chain")).await;
        assert_eq!(drain(&log), want.to_vec(), "{yaml}");
    }
}

/// 验证：同一槽位内路由声明的过滤器包在默认过滤器**外侧**（生效列表 =
/// 路由 filters 在前、defaults 在后，先列出者最外层），因此路由过滤器
/// 先于默认过滤器执行；跨槽位次序仍按规范槽序。
#[tokio::test]
async fn route_filters_stack_outside_defaults_within_a_slot() {
    let yaml = r#"
defaults:
  filters:
    - "RateLimit=key:client_ip,rate:2,period:1s,tag:defaults"
    - { name: CircuitBreaker, sliding_window: 20, tag: defaults }
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
    filters:
      - "RateLimit=key:client_ip,rate:1,period:1s,tag:route"
"#;
    let file = parse_yaml(yaml);
    let log = new_log();
    let registry = registry_with(&log, &["RateLimit", "CircuitBreaker"]);
    let handler = RecordingHandler::new(Arc::clone(&log));
    let compiled = compile_ok(
        &file,
        &registry,
        &Arc::new(StateRegistry::new()),
        &handler,
        &test_dir("defaults-slot-stack"),
    );
    send(&compiled.routes.routes()[0].chain_clone().expect("chain")).await;
    assert_eq!(
        drain(&log),
        vec![
            "RateLimit:route",
            "RateLimit:defaults",
            "CircuitBreaker:defaults",
            "handler",
        ],
        "route filter outermost within its slot; defaults appended behind it"
    );
}

/// 验证：`defaults.filters` 里的未知过滤器名以精确的
/// `defaults.filters[<i>]` 位置报错。
#[test]
fn unknown_default_filter_is_located_exactly() {
    // (配置 YAML, 期望错误位置, 期望过滤器名)
    let cases: &[(&str, &str, &str)] = &[
        (
            r#"
defaults:
  filters: ["Nope=a:1"]
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            "defaults.filters[0]",
            "Nope",
        ),
        (
            r#"
defaults:
  filters:
    - "RateLimit=key:client_ip,rate:100,period:1s"
    - { name: Nope }
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            "defaults.filters[1]",
            "Nope",
        ),
    ];
    for (yaml, want_loc, want_name) in cases {
        let file = parse_yaml(yaml);
        let err = compile_err(
            &file,
            &registry_with(&new_log(), &["RateLimit"]),
            &Arc::new(StateRegistry::new()),
            &SlowHandler {
                latency: Duration::ZERO,
            },
            Path::new("."),
        );
        match err {
            ConfigError::UnknownFilter { location, name } => {
                assert_eq!(location.to_string(), *want_loc, "{yaml}");
                assert_eq!(name, *want_name, "{yaml}");
            }
            other => panic!("expected UnknownFilter for {yaml}, got {other:?}"),
        }
    }
}

/// 验证：defaults 过滤器的工厂构建失败以 `defaults.filters[<i>]` 位置
/// 包装为 `ConfigError::Compile`，形态与路由过滤器构建错误一致。
#[test]
fn default_filter_build_error_is_wrapped_with_its_location() {
    let yaml = r#"
defaults:
  filters: [{ name: CircuitBreaker, sliding_window: 20 }]
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#;
    let file = parse_yaml(yaml);
    let err = compile_err(
        &file,
        &registry_failing(&["CircuitBreaker"]),
        &Arc::new(StateRegistry::new()),
        &SlowHandler {
            latency: Duration::ZERO,
        },
        Path::new("."),
    );
    match err {
        ConfigError::Compile { location, message } => {
            assert_eq!(location.to_string(), "defaults.filters[0]");
            assert!(message.contains("CircuitBreaker"), "{message}");
        }
        other => panic!("expected ConfigError::Compile, got {other:?}"),
    }
}

/// 验证：路由过滤器与 defaults 混合时的链序 golden——所有过滤器（无论
/// 路由声明还是默认）都按规范槽位顺序执行。
#[tokio::test]
async fn chain_order_golden_with_defaults_present() {
    let yaml = r#"
defaults:
  filters:
    - { name: CircuitBreaker, sliding_window: 20 }
    - "Timeout=per_attempt:2s"
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
    let registry = registry_with(
        &log,
        &[
            "Retry",
            "RateLimit",
            "JwtAuth",
            "Compression",
            "CircuitBreaker",
            "Timeout",
        ],
    );
    let handler = RecordingHandler::new(Arc::clone(&log));
    let compiled = compile_ok(
        &file,
        &registry,
        &Arc::new(StateRegistry::new()),
        &handler,
        &test_dir("defaults-golden"),
    );
    send(&compiled.routes.routes()[0].chain_clone().expect("chain")).await;
    assert_eq!(
        drain(&log),
        vec![
            "Compression",
            "RateLimit",
            "JwtAuth",
            "Retry",
            "CircuitBreaker",
            "Timeout",
            "handler",
        ],
        "defaults join the canonical slot order alongside route filters"
    );
}

/// 验证：`validate()` 对 `defaults.filters` 的参数按过滤器名逐一校验，
/// 与路由过滤器完全同规则，位置报 `defaults.filters[<i>]`。
#[test]
fn validate_flags_bad_default_filter_params() {
    // (配置 YAML, 期望的错误位置列表（收集顺序）；空 = 合法)
    let cases: &[(&str, &[&str])] = &[
        (
            r#"
defaults:
  filters:
    - { name: RateLimit, rate: 100 }
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            &["defaults.filters[0]"],
        ),
        (
            r#"
defaults:
  filters:
    - { name: CircuitBreaker, failure_rate: "200%" }
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            &["defaults.filters[0]"],
        ),
        (
            r#"
defaults:
  filters:
    - "RateLimit=key:client_ip,rate:100,period:1s"
    - { name: Nope }
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            &["defaults.filters[1]"],
        ),
        (
            r#"
defaults:
  filters:
    - { name: CircuitBreaker, sliding_window: 20, failure_rate: 50%, min_calls: 10,
        open_duration: 5s, half_open_max_calls: 3 }
    - "RateLimit=key:client_ip,rate:100,period:1s"
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
"#,
            &[],
        ),
        (
            r#"
defaults:
  filters: [{ name: BodyLimit }]
routes:
  - id: r
    uri: "http://127.0.0.1:9000"
    filters: ["RateLimit=rate:100"]
"#,
            &["defaults.filters[0]", "routes[r].filters[0]"],
        ),
    ];
    for (yaml, want_locs) in cases {
        let errors = crate::validate::validate(&parse_yaml(yaml));
        let got: Vec<String> = errors.iter().map(error_location).collect();
        assert_eq!(got, want_locs.to_vec(), "{yaml}");
    }
}

/// 取任意 [`ConfigError`] 变体携带的配置坐标。
fn error_location(err: &ConfigError) -> String {
    match err {
        ConfigError::Io { location, .. }
        | ConfigError::Yaml { location, .. }
        | ConfigError::Invalid { location, .. }
        | ConfigError::Value { location, .. }
        | ConfigError::UnknownFilter { location, .. }
        | ConfigError::UnknownPredicate { location, .. }
        | ConfigError::Compile { location, .. } => location.to_string(),
    }
}
