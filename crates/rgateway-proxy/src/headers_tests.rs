//! [`crate::headers`] 的测试：hop-by-hop 头剥离、Connection token 治理、
//! X-Forwarded-* / X-Request-ID 代理头语义与 Host 保留。

use http::{HeaderMap, HeaderValue};

use super::*;

/// 构造头表：按 (名, 值) 列表写入；同名多次出现用 append 保留多值
/// （多条 Connection 头的用例依赖这一语义）。
fn build(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(*name, HeaderValue::from_str(value).unwrap());
    }
    headers
}

/// 取头值文本（缺失返回 None；测试辅助）。
fn text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|v| v.to_str().expect("测试头值必须是 UTF-8").to_string())
}

/// 解析测试用 SocketAddr（测试辅助）。
fn addr(s: &str) -> Option<std::net::SocketAddr> {
    Some(s.parse().unwrap())
}

// hop-by-hop 固定清单逐项剥离，业务头与 Host 原样保留。
// 列含义：(头名, 头值, 剥离后是否应保留)
#[test]
fn hop_by_hop_headers_are_stripped() {
    let cases: [(&'static str, &str, bool); 11] = [
        ("connection", "keep-alive", false),
        ("keep-alive", "timeout=5", false),
        ("proxy-authenticate", "Basic realm=x", false),
        ("proxy-authorization", "Basic dXNlcg==", false),
        ("proxy-connection", "keep-alive", false),
        ("te", "trailers", false),
        ("trailer", "grpc-status", false),
        ("transfer-encoding", "chunked", false),
        ("upgrade", "websocket", false),
        ("x-keep", "1", true),
        ("host", "a.example", true),
    ];
    let pairs: Vec<(&'static str, &str)> = cases.iter().map(|(n, v, _)| (*n, *v)).collect();
    let mut headers = build(&pairs);
    strip_hop_by_hop(&mut headers);
    for (name, value, keep) in cases {
        assert_eq!(
            text(&headers, name),
            if keep { Some(value.to_string()) } else { None },
            "头 {name} 的保留语义不符"
        );
    }
}

// Connection 头列出的 token（逗号分隔、大小写不敏感）对应的头全部剥离；
// 多条 Connection 头的 token 合并处理，未列出的头保留。
#[test]
fn connection_listed_tokens_are_stripped() {
    let mut headers = build(&[
        ("connection", "x-foo, X-BAR"),
        ("connection", "x-baz"),
        ("x-foo", "1"),
        ("x-bar", "2"),
        ("x-baz", "3"),
        ("x-qux", "4"),
    ]);
    strip_hop_by_hop(&mut headers);
    // 列含义：(头名, 剥离后是否应保留)
    let cases = [
        ("x-foo", false),
        ("x-bar", false),
        ("x-baz", false),
        ("x-qux", true),
    ];
    for (name, keep) in cases {
        assert_eq!(
            text(&headers, name).is_some(),
            keep,
            "Connection token {name} 的剥离语义不符"
        );
    }
}

// Connection 里的非法 token（非法头名、空 token）被忽略而不是 panic，
// Connection 头本身仍然剥离。
#[test]
fn invalid_connection_tokens_are_ignored_safely() {
    let mut headers = build(&[
        ("connection", "bad name!, :, ,,x-ok"),
        ("x-ok", "1"),
        ("x-keep", "2"),
    ]);
    strip_hop_by_hop(&mut headers);
    assert_eq!(text(&headers, "connection"), None);
    assert_eq!(text(&headers, "x-ok"), None, "合法 token 仍应剥离");
    assert_eq!(text(&headers, "x-keep"), Some("2".into()));
}

// prepare_upstream 的 X-Forwarded-For 追加链表驱动：有 peer 时追加 IP
//（不含端口），无 peer 时不动；既有链用逗号保持。
// 列含义：(既有 XFF 值, peer 地址, 期望 XFF 值)
#[test]
fn prepare_upstream_appends_forwarded_for_chain() {
    let cases: [(Option<&str>, Option<std::net::SocketAddr>, Option<&str>); 5] = [
        (None, addr("5.6.7.8:1234"), Some("5.6.7.8")),
        (
            Some("1.2.3.4"),
            addr("5.6.7.8:1234"),
            Some("1.2.3.4, 5.6.7.8"),
        ),
        (
            Some("1.2.3.4, 2.3.4.5"),
            addr("5.6.7.8:1234"),
            Some("1.2.3.4, 2.3.4.5, 5.6.7.8"),
        ),
        (None, None, None),
        (Some("1.2.3.4"), None, Some("1.2.3.4")),
    ];
    for (existing, peer, want) in cases {
        let mut headers = HeaderMap::new();
        if let Some(value) = existing {
            headers.insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        }
        prepare_upstream(&mut headers, peer, Some("a.example"), "req-1", "http");
        assert_eq!(text(&headers, "x-forwarded-for"), want.map(str::to_string));
    }
}

// prepare_upstream 的固定语义：X-Request-ID 覆写、X-Forwarded-Proto 按
// 客户端接入 scheme 覆写（不信客户端传入的值）、X-Forwarded-Host 无条件
// 覆写、Host 原样保留、hop-by-hop 头仍然剥离。
#[test]
fn prepare_upstream_rewrites_request_id_proto_and_host() {
    let mut headers = build(&[
        ("x-request-id", "old-id"),
        ("x-forwarded-proto", "https"),
        ("host", "a.example"),
        ("transfer-encoding", "chunked"),
    ]);
    prepare_upstream(
        &mut headers,
        addr("5.6.7.8:1234"),
        Some("a.example"),
        "req-42",
        "http",
    );
    assert_eq!(text(&headers, "x-request-id"), Some("req-42".into()));
    assert_eq!(
        text(&headers, "x-forwarded-proto"),
        Some("http".into()),
        "客户端伪造的 https 必须被接入 scheme 覆写"
    );
    assert_eq!(text(&headers, "x-forwarded-host"), Some("a.example".into()));
    assert_eq!(
        text(&headers, "host"),
        Some("a.example".into()),
        "Host 必须原样保留"
    );
    assert_eq!(text(&headers, "transfer-encoding"), None);

    // TLS 接入时 X-Forwarded-Proto 是 https
    let mut headers = build(&[("host", "a.example")]);
    prepare_upstream(&mut headers, addr("5.6.7.8:1234"), Some("a.example"), "req-42", "https");
    assert_eq!(text(&headers, "x-forwarded-proto"), Some("https".into()));
}

// Host 缺失时用 original_host 补写（HTTP/2 入站只有 :authority、无 Host，
// 出站归一为 HTTP/1.1 后必须携带 Host，否则上游虚拟主机路由失效）；
// Host 已存在时绝不覆盖；original_host 为 None 时不伪造。
#[test]
fn prepare_upstream_backfills_host_when_absent() {
    // 列含义：(客户端是否带 Host, original_host, 期望 Host)
    let cases: [(bool, Option<&str>, Option<&str>); 4] = [
        (true, Some("a.example"), Some("a.example")),
        (true, Some("b.example"), Some("a.example")), // 已有 Host 不覆盖
        (false, Some("b.example"), Some("b.example")), // 缺失时补写
        (false, None, None),                          // 无原始 Host 不伪造
    ];
    for (has_host, original, want_host) in cases {
        let mut headers = if has_host {
            build(&[("host", "a.example")])
        } else {
            build(&[])
        };
        prepare_upstream(&mut headers, addr("5.6.7.8:1234"), original, "req-42", "http");
        assert_eq!(
            text(&headers, "host"),
            want_host.map(String::from),
            "has_host={has_host} original={original:?}"
        );
    }
}

// X-Forwarded-Host 无条件覆写为本次请求原始 Host（与 X-Request-ID /
// X-Forwarded-Proto 同语义，不信客户端传入的值）；original_host 为 None
// 时移除客户端值（无原始 Host 就绝不转发任何 X-Forwarded-Host）；
// RFC 7239 的 Forwarded 头一并剥离（防同类伪造）。
// 列含义：(客户端传入的 X-Forwarded-Host, 客户端传入的 Forwarded,
//          original_host, 期望的 X-Forwarded-Host)
#[test]
fn prepare_upstream_overwrites_forwarded_host_and_strips_forwarded() {
    /// (客户端 X-Forwarded-Host, 客户端 Forwarded, original_host, 期望值)
    type Case<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );
    let cases: [Case<'_>; 4] = [
        (
            Some("evil.example"),
            Some("host=evil.example"),
            Some("a.example"),
            Some("a.example"),
        ),
        (
            None,
            Some("host=evil.example"),
            Some("a.example"),
            Some("a.example"),
        ),
        (Some("evil.example"), None, None, None),
        (None, None, Some("a.example"), Some("a.example")),
    ];
    for (client_host, forwarded, original, want) in cases {
        let mut headers = HeaderMap::new();
        if let Some(value) = client_host {
            headers.insert("x-forwarded-host", HeaderValue::from_str(value).unwrap());
        }
        if let Some(value) = forwarded {
            headers.insert("forwarded", HeaderValue::from_str(value).unwrap());
        }
        prepare_upstream(&mut headers, None, original, "req-1", "http");
        assert_eq!(
            text(&headers, "x-forwarded-host"),
            want.map(str::to_string),
            "X-Forwarded-Host 覆写语义不符（客户端={client_host:?} 原始={original:?}）"
        );
        assert_eq!(
            text(&headers, "forwarded"),
            None,
            "RFC 7239 Forwarded 必须剥离"
        );
    }
}

// header_value 脱敏回归（防头注入的唯一控制点）：CR/LF/NUL/DEL 等 ASCII
// 控制字符替换为 '?'，tab 与 obs-text（0x80-0xFF）原样保留；结果恒可装入
// HeaderValue，绝不 panic，也绝不残留 CR/LF/NUL/DEL。
// 列含义：(输入文本, 期望输出文本)
#[test]
fn header_value_sanitizes_control_characters() {
    let cases: [(&str, &str); 8] = [
        ("plain", "plain"),
        ("a\r\nb", "a??b"),
        ("a\nX-Injected: 1", "a?X-Injected: 1"),
        ("a\x00b", "a?b"),
        ("a\x7Fb", "a?b"),
        ("tab\tkeep", "tab\tkeep"),
        // U+0080 的 UTF-8 字节是 C2 80，即 obs-text 字节，必须原样保留。
        ("obs-\u{80}text", "obs-\u{80}text"),
        ("a\r\n\u{80}b", "a??\u{80}b"),
    ];
    for (input, want) in cases {
        let value = header_value(input);
        assert_eq!(value.as_bytes(), want.as_bytes(), "脱敏结果不符: {input:?}");
        for forbidden in [b'\r', b'\n', 0u8, 0x7Fu8] {
            assert!(
                !value.as_bytes().contains(&forbidden),
                "脱敏后仍残留控制字节 0x{forbidden:02x}: {input:?}"
            );
        }
        assert!(
            HeaderValue::from_bytes(value.as_bytes()).is_ok(),
            "脱敏结果必须可装入 HeaderValue: {input:?}"
        );
    }
}

// finalize_response：剥离 hop-by-hop 后覆写 X-Request-ID（响应侧不再改写
// X-Forwarded-*）。
#[test]
fn finalize_response_strips_and_overwrites_request_id() {
    let mut headers = build(&[
        ("x-request-id", "old-id"),
        ("transfer-encoding", "chunked"),
        ("connection", "x-extra"),
        ("x-extra", "gone"),
        ("content-type", "application/json"),
    ]);
    finalize_response(&mut headers, "req-7");
    assert_eq!(text(&headers, "x-request-id"), Some("req-7".into()));
    assert_eq!(text(&headers, "transfer-encoding"), None);
    assert_eq!(text(&headers, "x-extra"), None);
    assert_eq!(
        text(&headers, "content-type"),
        Some("application/json".into()),
        "业务头保留"
    );
}
