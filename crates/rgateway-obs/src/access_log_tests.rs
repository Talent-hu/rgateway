//! [`crate::access_log`] 的测试：JSON/text 渲染、null 策略、引号转义与注入防护。
//!
//! 期望值全部硬编码（不通过被测脱敏逻辑推导），保证断言独立于实现细节。

use std::time::Duration;

use serde_json::{json, Value};

use super::*;

/// 测试统一时间戳（RFC3339 UTC 字符串由调用方传入，保证渲染确定性）。
const TS: &str = "2026-01-02T03:04:05Z";

/// 全字段填满的基准记录（error_kind 缺省，另行覆盖）。
fn full_record() -> AccessRecord {
    AccessRecord {
        request_id: "req-1".to_string(),
        peer: Some("10.0.0.1:52341".to_string()),
        method: "GET".to_string(),
        path: "/api/users".to_string(),
        route_id: Some("user-route".to_string()),
        protocol: "http".to_string(),
        status: 200,
        upstream: Some("127.0.0.1:8080".to_string()),
        error_kind: None,
        duration: Duration::from_millis(42),
    }
}

/// 可选字段全缺省的记录。
fn minimal_record() -> AccessRecord {
    AccessRecord {
        request_id: "r".to_string(),
        peer: None,
        method: "GET".to_string(),
        path: "/".to_string(),
        route_id: None,
        protocol: "http".to_string(),
        status: 502,
        upstream: None,
        error_kind: None,
        duration: Duration::from_millis(5),
    }
}

/// 验证：JSON 键名/值稳定，11 个键一个不少（含 null 占位），输出单行可解析。
#[test]
fn render_json_keys_and_values() {
    // cases 列：字段名 | 期望 JSON 值
    let cases: [(&str, Value); 11] = [
        ("ts", json!(TS)),
        ("request_id", json!("req-1")),
        ("peer", json!("10.0.0.1:52341")),
        ("method", json!("GET")),
        ("path", json!("/api/users")),
        ("route_id", json!("user-route")),
        ("protocol", json!("http")),
        ("status", json!(200)),
        ("upstream", json!("127.0.0.1:8080")),
        ("error_kind", Value::Null),
        ("duration_ms", json!(42)),
    ];
    let rendered = render_json(&full_record(), TS);
    let value: Value = serde_json::from_str(&rendered).expect("JSON 可解析");
    assert!(!rendered.contains('\n'), "应为单行: {rendered}");
    assert_eq!(value.as_object().map(|o| o.len()), Some(11), "键数恒定");
    for (key, expected) in cases {
        assert_eq!(value.get(key), Some(&expected), "字段 {key}");
    }
}

/// 验证：Option 字段缺失时键仍在、值为 null（日志管道 schema 稳定）。
#[test]
fn render_json_missing_fields_are_null() {
    // cases 列：字段名
    let optional_keys = ["peer", "route_id", "upstream", "error_kind"];
    let value: Value =
        serde_json::from_str(&render_json(&minimal_record(), TS)).expect("JSON 可解析");
    for key in optional_keys {
        assert_eq!(value.get(key), Some(&Value::Null), "字段 {key} 应为 null");
    }
}

/// 验证：duration → duration_ms 毫秒换算；亚毫秒截断、超界饱和封顶（不 panic）。
#[test]
fn render_json_duration_ms_conversion() {
    // cases 列：注释 | duration | 期望 duration_ms
    let cases = [
        ("零时长", Duration::ZERO, 0_u64),
        ("整毫秒", Duration::from_millis(42), 42),
        ("亚毫秒截断", Duration::from_micros(1500), 1),
        ("整秒换算", Duration::from_secs(2), 2000),
        ("超界饱和封顶", Duration::from_secs(u64::MAX), u64::MAX),
    ];
    for (label, duration, expected) in cases {
        let record = AccessRecord {
            duration,
            ..full_record()
        };
        let value: Value = serde_json::from_str(&render_json(&record, TS)).expect("JSON 可解析");
        assert_eq!(value.get("duration_ms"), Some(&json!(expected)), "{label}");
    }
}

/// 验证：text 单行 key=value 的字段顺序与缺失字段的裸 null 语义。
#[test]
fn render_text_lines() {
    // cases 列：注释 | 记录 | 期望整行
    let cases = [
        (
            "全字段",
            full_record(),
            "ts=2026-01-02T03:04:05Z request_id=req-1 peer=10.0.0.1:52341 method=GET path=/api/users route_id=user-route protocol=http status=200 upstream=127.0.0.1:8080 error_kind=null duration_ms=42",
        ),
        (
            "缺省字段",
            minimal_record(),
            "ts=2026-01-02T03:04:05Z request_id=r peer=null method=GET path=/ route_id=null protocol=http status=502 upstream=null error_kind=null duration_ms=5",
        ),
    ];
    for (label, record, expected) in cases {
        assert_eq!(render_text(&record, TS), expected, "{label}");
    }
}

/// 验证：值含空白/`=`/引号/反斜杠、空串与字面 null 时加双引号并转义，普通值裸输出。
#[test]
fn render_text_quoting_and_escaping() {
    // cases 列：注释 | path 输入 | 期望 path 片段
    let cases = [
        ("含空格加引号", "/a b", "path=\"/a b\""),
        ("含等号加引号", "/a=b", "path=\"/a=b\""),
        ("双引号转义", "/a\"b", "path=\"/a\\\"b\""),
        ("反斜杠转义", "/a\\b", "path=\"/a\\\\b\""),
        ("空串加引号", "", "path=\"\""),
        ("字面 null 加引号", "null", "path=\"null\""),
        ("普通值裸输出", "/plain", "path=/plain"),
    ];
    for (label, path, expected_fragment) in cases {
        let record = AccessRecord {
            path: path.to_string(),
            ..minimal_record()
        };
        let rendered = render_text(&record, TS);
        assert!(rendered.contains(expected_fragment), "{label}: {rendered}");
    }
}

/// 验证：注入载荷在两种格式里都被脱敏为 `?`，输出绝无控制字符与换行逃逸。
#[test]
fn log_injection_neutralized_in_both_formats() {
    // cases 列：注释 | 注入载荷 | 期望脱敏后
    let cases = [
        ("CRLF 组合注入", "evil\r\ninjected", "evil??injected"),
        ("单独 CR", "evil\rX", "evil?X"),
        ("单独 LF", "evil\nX", "evil?X"),
        ("NUL 截断", "evil\u{0}X", "evil?X"),
        ("DEL", "evil\u{7f}X", "evil?X"),
        ("ESC 终端序列", "evil\u{1b}[31m", "evil?[31m"),
        ("Tab", "evil\tX", "evil?X"),
        ("NEL C1", "evil\u{85}X", "evil?X"),
        ("U+2028 行分隔符", "evil\u{2028}X", "evil?X"),
        ("U+2029 段分隔符", "evil\u{2029}X", "evil?X"),
    ];
    for (label, payload, expected) in cases {
        let record = AccessRecord {
            request_id: payload.to_string(),
            peer: Some(payload.to_string()),
            method: payload.to_string(),
            path: payload.to_string(),
            route_id: Some(payload.to_string()),
            protocol: payload.to_string(),
            status: 200,
            upstream: Some(payload.to_string()),
            error_kind: Some(payload.to_string()),
            duration: Duration::from_millis(1),
        };
        let json_line = render_json(&record, payload);
        let text_line = render_text(&record, payload);
        for (fmt_name, output) in [("json", &json_line), ("text", &text_line)] {
            assert!(
                !output
                    .chars()
                    .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}'),
                "{label}/{fmt_name} 含控制字符/行分隔符: {output:?}"
            );
            assert_eq!(output.lines().count(), 1, "{label}/{fmt_name} 应为单行");
            assert!(
                output.contains(expected),
                "{label}/{fmt_name} 应含脱敏载荷: {output}"
            );
        }
    }
}

/// 验证：脱敏规则本身——ASCII/C1 控制符与 Unicode 行/段分隔符替换为 `?`，
/// 其余字符（含普通 Unicode 文本）原样。
#[test]
fn sanitize_text_replaces_ascii_controls() {
    // cases 列：注释 | 输入 | 期望输出
    let cases = [
        ("普通文本原样", "GET /api/users?x=1", "GET /api/users?x=1"),
        ("CRLF→??", "a\r\nb", "a??b"),
        ("NUL→?", "a\0b", "a?b"),
        ("DEL→?", "a\u{7f}b", "a?b"),
        ("Tab→?", "a\tb", "a?b"),
        ("ESC→?", "a\u{1b}b", "a?b"),
        ("NEL→?", "a\u{85}b", "a?b"),
        ("U+2028→?", "a\u{2028}b", "a?b"),
        ("U+2029→?", "a\u{2029}b", "a?b"),
        ("Unicode 原样", "路径/中文 ✓", "路径/中文 ✓"),
    ];
    for (label, input, expected) in cases {
        assert_eq!(sanitize_text(input), expected, "{label}");
    }
}
