//! [`crate::validate`] 的测试：`observability.metrics.path` 的边界校验——
//! 绝对路径语法、保留路径冲突（与管理面 REST 端点对拍）。

use crate::error::ConfigError;
use crate::loader::load_from_str;
use crate::validate::{validate, MGMT_RESERVED_PATHS};

/// 解析 YAML 并校验，返回错误文案拼接（空串 = 通过）。
fn validate_yaml(yaml: &str) -> String {
    let file = load_from_str(yaml).expect("yaml must parse");
    validate(&file)
        .iter()
        .map(ConfigError::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// 表格驱动：metrics.path 的合法/非法形态。列含义：(路径, 期望通过)
#[test]
fn metrics_path_acceptance_table() {
    // cases 列：(配置里的 metrics.path, 是否合法)
    let cases: &[(&str, bool)] = &[
        ("/metrics", true),
        ("/custom/path", true),
        ("/m", true),
        ("/", false),
        ("", false),
        ("metrics", false),
        ("/metrics?x=1", false),
        ("/metrics#frag", false),
        ("/my metrics", false),
        ("/metrics\t", false),
        ("/health", false),
        ("/mgmt/v1/snapshot", false),
        ("/mgmt/v1/routes", false),
        ("/mgmt/v1/breakers", false),
        ("/mgmt/v1/stats", false),
        ("/mgmt/v1/breakers/reset", false),
    ];
    for &(path, want_ok) in cases {
        let yaml = format!("observability:\n  metrics:\n    path: \"{path}\"\n");
        let text = validate_yaml(&yaml);
        if want_ok {
            assert!(text.is_empty(), "path {path:?} 应合法，却报错: {text}");
        } else {
            assert!(
                text.contains("observability.metrics.path"),
                "path {path:?} 应报 metrics.path 错误，实际: {text}"
            );
        }
    }
}

/// 保留路径清单必须与管理面 REST 端点一致（双向：本清单不得多出端点）。
/// mgmt crate 侧另有对拍测试保证「端点新增而清单漏改」也会失败。
#[test]
fn reserved_paths_cover_management_endpoints() {
    // cases 列：(必须保留的路径)
    let must_reserve = [
        "/health",
        "/mgmt/v1/snapshot",
        "/mgmt/v1/routes",
        "/mgmt/v1/breakers",
        "/mgmt/v1/stats",
        "/mgmt/v1/breakers/reset",
    ];
    for path in must_reserve {
        assert!(
            MGMT_RESERVED_PATHS.contains(&path),
            "管理面端点 {path} 必须列入保留路径"
        );
    }
    assert_eq!(
        MGMT_RESERVED_PATHS.len(),
        must_reserve.len(),
        "保留路径不得多出未声明的端点"
    );
}

/// 用一段路由过滤器 YAML 构造完整配置并校验，返回错误文案拼接。
fn validate_filter(filter_yaml: &str) -> String {
    let yaml = format!(
        "routes:\n  - id: r1\n    uri: \"http://127.0.0.1:9000\"\n    filters:\n      - {filter_yaml}\n"
    );
    validate_yaml(&yaml)
}

/// 表格驱动：JwtAuth / ApiKeyAuth / IpFilter 的参数级校验。
/// 列含义：(过滤器结构化 YAML 片段, 期望通过?, 期望错误包含的子串)。
#[test]
fn auth_filter_param_validation_table() {
    let cases: &[(&str, bool, &str)] = &[
        // JwtAuth：issuer/audience/jwks_file 三者必填
        (
            "{ name: JwtAuth, issuer: \"https://idp\", audience: gw, jwks_file: ./jwks.json }",
            true,
            "",
        ),
        (
            "{ name: JwtAuth, audience: gw, jwks_file: ./jwks.json }",
            false,
            "issuer",
        ),
        (
            "{ name: JwtAuth, issuer: \"https://idp\", jwks_file: ./jwks.json }",
            false,
            "audience",
        ),
        (
            "{ name: JwtAuth, issuer: \"https://idp\", audience: gw }",
            false,
            "jwks_file",
        ),
        // ApiKeyAuth：keys 必须非空 map，value 是非空 subject；header 可选但必须是合法头名
        ("{ name: ApiKeyAuth, keys: { secret-1: alice } }", true, ""),
        (
            "{ name: ApiKeyAuth, header: x-api-key, keys: { s: alice } }",
            true,
            "",
        ),
        ("{ name: ApiKeyAuth }", false, "keys"),
        ("{ name: ApiKeyAuth, keys: {} }", false, "keys"),
        (
            "{ name: ApiKeyAuth, keys: { secret-1: \"\" } }",
            false,
            "subject",
        ),
        (
            "{ name: ApiKeyAuth, header: \"bad header name\", keys: { s: alice } }",
            false,
            "header",
        ),
        // IpFilter：allow/deny 至少一非空；每条必须可解析为 IP/CIDR
        ("{ name: IpFilter, allow: [\"10.0.0.0/8\"] }", true, ""),
        ("{ name: IpFilter, deny: [\"192.0.2.1\"] }", true, ""),
        (
            "{ name: IpFilter, allow: [\"10.0.0.0/8\"], deny: [\"10.1.1.1\"] }",
            true,
            "",
        ),
        ("{ name: IpFilter }", false, "allow or deny"),
        (
            "{ name: IpFilter, allow: [], deny: [] }",
            false,
            "allow or deny",
        ),
        (
            "{ name: IpFilter, allow: [\"not-an-ip\"] }",
            false,
            "not-an-ip",
        ),
        ("{ name: IpFilter, deny: [\"999.1.1.1\"] }", false, "deny"),
    ];
    for (filter, want_ok, want_sub) in cases {
        let text = validate_filter(filter);
        if *want_ok {
            assert!(text.is_empty(), "{filter} 应通过，却报错: {text}");
        } else {
            assert!(!text.is_empty(), "{filter} 应报错却通过");
            assert!(
                text.contains(want_sub),
                "{filter} 错误应含 {want_sub:?}，实际: {text}"
            );
        }
    }
}

/// TLS 联动：client_auth != none 时 client_ca 必填——没有 CA 就无法验证
/// 客户端证书，配置必须在加载期拒绝。
#[test]
fn tls_client_auth_requires_client_ca() {
    let yaml = "server:\n  tls:\n    enabled: true\n    cert: ./c.pem\n    key: ./k.pem\n    client_auth: required\n";
    let text = validate_yaml(yaml);
    assert!(
        text.contains("client_ca"),
        "client_auth=required 缺 client_ca 应报错，实际: {text}"
    );

    let yaml_ok = "server:\n  tls:\n    enabled: true\n    cert: ./c.pem\n    key: ./k.pem\n    client_auth: required\n    client_ca: ./ca.pem\n";
    let text = validate_yaml(yaml_ok);
    assert!(text.is_empty(), "补上 client_ca 后应通过，实际: {text}");

    // client_auth: none 时不需要 client_ca
    let yaml_none = "server:\n  tls:\n    enabled: true\n    cert: ./c.pem\n    key: ./k.pem\n    client_auth: none\n";
    let text = validate_yaml(yaml_none);
    assert!(
        text.is_empty(),
        "client_auth=none 不需要 client_ca，实际: {text}"
    );
}
