//! [`crate::span`] 的测试：span 命名、字段存在性与字段脱敏。
//!
//! 用 tracing-subscriber 自定义 [`Layer`] 收集 span 字段做断言，不引第三方
//! 测试宏依赖；`%字段`（Display）路径由 `record_debug` 收集——`DisplayValue`
//! 的 `Debug` 转发到 `Display`，拿到的是无引号原文。

use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry;

use super::*;

/// 收集到的一条 span：元数据名 + 记录到的字段（名, 值）。
#[derive(Debug, Clone)]
struct CollectedSpan {
    name: &'static str,
    fields: Vec<(String, String)>,
}

/// 自定义收集层：on_new_span 时把字段抄写进共享缓冲。
#[derive(Clone, Default)]
struct CollectLayer {
    spans: Arc<Mutex<Vec<CollectedSpan>>>,
}

/// 字段访问器：`str` 与 Display/Debug 两条记录路径都收成字符串。
#[derive(Default)]
struct FieldVisitor {
    fields: Vec<(String, String)>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

impl<S: Subscriber> Layer<S> for CollectLayer {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CollectedSpan {
            name: attrs.metadata().name(),
            fields: visitor.fields,
        });
    }
}

/// 在自定义收集层下执行 f，返回收集到的 span 快照。
fn collect_spans(f: impl FnOnce()) -> Vec<CollectedSpan> {
    let layer = CollectLayer::default();
    let sink = Arc::clone(&layer.spans);
    let subscriber = registry().with(layer);
    tracing::subscriber::with_default(subscriber, f);
    // 锁守卫在本语句内即释放——不能放尾表达式（临时值析构晚于 `sink` 会触发借用错误）。
    let spans = sink.lock().unwrap().clone();
    drop(sink);
    spans
}

/// 取字段值（断言存在性与取值）。
fn field<'a>(collected: &'a CollectedSpan, key: &str) -> Option<&'a str> {
    collected
        .fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// 验证：请求 span 命名为 `request`，4 个字段齐全且值原样（非 ASCII 不动）。
#[test]
fn request_span_name_and_fields() {
    // cases 列：注释 | request_id | method | path | protocol | 期望(request_id, method, path, protocol)
    let cases = [
        (
            "常规值",
            "req-1",
            "GET",
            "/api/users",
            "http",
            "req-1",
            "GET",
            "/api/users",
            "http",
        ),
        (
            "非 ASCII 原样",
            "req-2",
            "POST",
            "/路径/中文",
            "grpc",
            "req-2",
            "POST",
            "/路径/中文",
            "grpc",
        ),
    ];
    for (label, request_id, method, path, protocol, e_req, e_m, e_p, e_proto) in cases {
        let spans = collect_spans(|| {
            let span = request_span(request_id, method, path, protocol);
            drop(span);
        });
        assert_eq!(spans.len(), 1, "{label}");
        let collected = &spans[0];
        assert_eq!(collected.name, "request", "{label}");
        assert_eq!(collected.fields.len(), 4, "{label} 字段数");
        assert_eq!(field(collected, "request_id"), Some(e_req), "{label}");
        assert_eq!(field(collected, "method"), Some(e_m), "{label}");
        assert_eq!(field(collected, "path"), Some(e_p), "{label}");
        assert_eq!(field(collected, "protocol"), Some(e_proto), "{label}");
    }
}

/// 验证：span 字段与访问日志共用脱敏策略——控制字符替换为 `?`，防日志注入。
#[test]
fn request_span_sanitizes_control_chars() {
    // cases 列：注释 | 注入载荷 | 期望脱敏后
    let cases = [
        ("CRLF 组合注入", "a\r\nb", "a??b"),
        ("单独 CR", "a\rb", "a?b"),
        ("单独 LF", "a\nb", "a?b"),
        ("NUL", "a\0b", "a?b"),
        ("DEL", "a\u{7f}b", "a?b"),
        ("ESC 序列", "a\u{1b}[0m", "a?[0m"),
        ("Tab", "a\tb", "a?b"),
    ];
    for (label, payload, expected) in cases {
        let spans = collect_spans(|| {
            let span = request_span(payload, payload, payload, payload);
            drop(span);
        });
        let collected = &spans[0];
        for key in ["request_id", "method", "path", "protocol"] {
            assert_eq!(field(collected, key), Some(expected), "{label} {key}");
        }
    }
}

/// 验证：上游尝试 span 命名为 `upstream_attempt`，instance_id 字段脱敏。
#[test]
fn upstream_span_name_and_instance_field() {
    // cases 列：注释 | instance_id 输入 | 期望字段值
    let cases = [
        ("常规值", "svc-1:8080", "svc-1:8080"),
        ("注入载荷脱敏", "svc\r\nx", "svc??x"),
    ];
    for (label, input, expected) in cases {
        let spans = collect_spans(|| {
            let span = upstream_span(input);
            drop(span);
        });
        assert_eq!(spans.len(), 1, "{label}");
        assert_eq!(spans[0].name, "upstream_attempt", "{label}");
        assert_eq!(field(&spans[0], "instance_id"), Some(expected), "{label}");
    }
}
