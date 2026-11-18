//! [`crate::spec`] 的测试：紧凑 DSL 解析、类型化参数访问器、链槽位分类、
//! 列表参数与错误定位。

use std::time::Duration;

use super::*;
use serde_yaml;

/// 测试辅助：从 YAML 文本反序列化 `FilterConfig` 并归一化为 `FilterSpec`。
fn spec(yaml: &str) -> FilterSpec {
    let cfg: FilterConfig = serde_yaml::from_str(yaml).unwrap();
    FilterSpec::from_config(&cfg).unwrap()
}

/// 验证紧凑 DSL 过滤器串解析出 name 与 k:v 参数，且字符串/u64/时长
/// 各类型化访问器都能从字符串形态取到值。
#[test]
fn compact_filter_parses_kv_params() {
    let s = FilterSpec::parse_compact("RateLimit=key:client_ip,rate:100,period:1s").unwrap();
    assert_eq!(s.name, "RateLimit");
    assert_eq!(s.param_str("key").unwrap(), "client_ip");
    assert_eq!(s.param_u64("rate").unwrap(), 100);
    assert_eq!(s.param_duration("period").unwrap(), Duration::from_secs(1));
}

/// 验证 `\,` 转义逗号保留在参数值内，不被当作参数分隔符。
#[test]
fn compact_filter_escaped_comma() {
    let s = FilterSpec::parse_compact(r"RequestHeader=x-a:foo\,bar,x-b:2").unwrap();
    assert_eq!(s.name, "RequestHeader");
    assert_eq!(s.param_str("x-a").unwrap(), "foo,bar");
}

/// 验证谓词紧凑串解析出 name 与 args 列表；`\\` 去转义后正则参数
/// 原样保留。
#[test]
fn compact_predicate_parses_args() {
    let p = PredicateSpec::parse_compact("Method=GET,POST").unwrap();
    assert_eq!(p.name, "Method");
    assert_eq!(p.args, vec!["GET", "POST"]);

    let p = PredicateSpec::parse_compact(r"Header=X-Tenant,\\d+").unwrap();
    assert_eq!(p.name, "Header");
    assert_eq!(p.args, vec!["X-Tenant", r"\d+"]);
}

/// 验证结构化 YAML 的 number/百分比/时长参数经访问器正确转换
/// （`50%` 按百分比折算成 0.5）。
#[test]
fn structured_filter_params_typed_access() {
    let s = spec("{ name: CircuitBreaker, sliding_window: 20, failure_rate: 50%, min_calls: 10, open_duration: 5s, half_open_max_calls: 3 }");
    assert_eq!(s.name, "CircuitBreaker");
    assert_eq!(s.param_u64("sliding_window").unwrap(), 20);
    assert!((s.param_f64("failure_rate").unwrap() - 0.5).abs() < 1e-9);
    assert_eq!(
        s.param_duration("open_duration").unwrap(),
        Duration::from_secs(5)
    );
}

/// 验证 size/duration 访问器解析带单位的字符串（`10MB` = 10×1024² 字节、
/// `500ms` = 500 毫秒）。
#[test]
fn duration_and_size_parsing() {
    let s = spec("{ name: BodyLimit, max_body: 10MB, timeout: 500ms }");
    assert_eq!(s.param_size("max_body").unwrap(), 10 * 1024 * 1024);
    assert_eq!(
        s.param_duration("timeout").unwrap(),
        Duration::from_millis(500)
    );
}

/// 验证缺参错误消息同时含参数名与过滤器源文本，能定位到具体条目。
#[test]
fn missing_param_reports_location() {
    let s = FilterSpec::parse_compact("RateLimit=rate:100").unwrap();
    let err = s.param_str("key").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("key"), "{text}");
    assert!(text.contains("RateLimit"), "{text}");
}

/// 验证过滤器名 → 槽位映射、未知名返回 None，且槽位排序符合
/// canonical 链序。
#[test]
fn slot_classification() {
    assert_eq!(slot_for("CORS"), Some(FilterSlot::Cors));
    assert_eq!(slot_for("JwtAuth"), Some(FilterSlot::Auth));
    assert_eq!(slot_for("Retry"), Some(FilterSlot::Retry));
    assert_eq!(slot_for("Nope"), None);
    // 规范顺序（canonical order）：Compression < Cors < RateLimit < Auth < Route < Retry < Breaker < Timeout
    assert!(FilterSlot::Compression < FilterSlot::Cors);
    assert!(FilterSlot::Route < FilterSlot::Retry);
    assert!(FilterSlot::Retry < FilterSlot::Breaker);
}

/// 验证字符串/u64 列表参数访问（methods、HTTP status、grpc status），
/// 且缺失列表返回空表。
#[test]
fn list_params() {
    let s =
        spec("{ name: Retry, methods: [GET, HEAD], statuses: [502, 503], grpc_statuses: [14] }");
    assert_eq!(s.param_str_list("methods").unwrap(), vec!["GET", "HEAD"]);
    assert_eq!(s.param_u64_list("statuses").unwrap(), vec![502, 503]);
    assert_eq!(s.param_u64_list("grpc_statuses").unwrap(), vec![14]);
    assert!(s.param_str_list("missing").unwrap().is_empty());
}

/// 构造一个只带 `config_dir` 的最小 [`FilterBuildCtx`]（resolve_path 测试用）。
fn ctx_with_dir(dir: &str) -> FilterBuildCtx {
    use rgateway_core::StaticDiscovery;
    FilterBuildCtx {
        route_id: Arc::from("r1"),
        route_state: Arc::new(RouteState::new("r1")),
        discovery: Arc::new(StaticDiscovery::new(std::collections::HashMap::new())),
        max_retry_buffer: 1024,
        config_dir: Arc::new(std::path::PathBuf::from(dir)),
    }
}

/// 验证 [`FilterBuildCtx::resolve_path`] 的目录包含约束：目录内引用解析到
/// 配置目录之下；`..` 逃逸与绝对路径（POSIX 根、Windows 盘符）一律拒绝。
/// 语义必须与 `compile_settings::resolve_in_dir` 完全一致。
#[test]
fn build_ctx_resolve_path_containment() {
    use std::path::PathBuf;
    let ctx = ctx_with_dir("rgwbase");

    // 列含义：(引用, 期望的相对段序列——最终拼在配置目录之下)。
    let ok_cases: &[(&str, &[&str])] = &[
        ("jwks.json", &["jwks.json"]),
        ("./jwks.json", &["jwks.json"]),
        ("tls/a.pem", &["tls", "a.pem"]),
        ("a/../b.pem", &["b.pem"]),
    ];
    for (input, want) in ok_cases {
        let got = ctx.resolve_path(input).expect("must resolve");
        let mut expect = PathBuf::from("rgwbase");
        for seg in *want {
            expect.push(seg);
        }
        assert_eq!(got, expect, "{input:?}");
    }

    // 逃逸 / 绝对路径必须拒绝。
    for input in ["../evil.json", "/etc/passwd", "C:/x.pem", "\\evil"] {
        assert!(
            ctx.resolve_path(input).is_err(),
            "{input:?} must be rejected"
        );
    }
}
