//! `chain` 子模块的谓词编译测试：验证编译产出的 Header/Host/Query 谓词
//! 是「全值匹配」而非子串匹配（锚定为 `^(?:P)$`）。

use super::*;
use rgateway_core::RequestCtx;

/// 表驱动用例：(compact 谓词 spec, 请求 URI, 请求头, 期望是否匹配)。
type Case = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    bool,
);

/// 编译一条 compact-DSL 谓词并拿一个请求去匹配它。
fn matches(spec_text: &str, uri: &str, headers: &[(&str, &str)]) -> bool {
    let spec = PredicateSpec::parse_compact(spec_text).expect("spec parses");
    let pred = build_predicate(&spec, &Location::route("r")).expect("predicate builds");
    let mut builder = http::Request::builder().uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let parts = builder.body(()).expect("request").into_parts().0;
    pred.matches(&RequestCtx::new(Protocol::Http1, None), &parts)
}

/// 验证：编译出的 Host/Header/Query 谓词只做全值匹配——`api.example.com`
/// 不匹配 `xapi.example.com` / `api.example.com.evil` / `api-example.com`，
/// `a|b` 备选也各自锚定全值。
#[test]
fn compiled_predicates_are_full_value_matches() {
    let h = r"Host=api\.example\.com";
    let cases: &[Case] = &[
        (h, "/", &[("host", "api.example.com")], true),
        (h, "/", &[("host", "xapi.example.com")], false),
        (h, "/", &[("host", "api.example.com.evil")], false),
        (h, "/", &[("host", "api-example.com")], false),
        ("Host=a|b", "/", &[("host", "a")], true),
        ("Host=a|b", "/", &[("host", "xab")], false),
        ("Host=a|b", "http://b/", &[], true),
        (r"Header=X-Tenant,\d+", "/", &[("x-tenant", "42")], true),
        (r"Header=X-Tenant,\d+", "/", &[("x-tenant", "a42b")], false),
        ("Query=token,a|b", "/x?token=b", &[], true),
        ("Query=token,a|b", "/x?token=xab", &[], false),
    ];
    for &(spec_text, uri, headers, want) in cases {
        assert_eq!(matches(spec_text, uri, headers), want, "{spec_text} {uri}");
    }
}
