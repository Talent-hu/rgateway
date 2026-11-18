//! [`crate::access`] 的测试：RFC3339 格式化表、访问记录字段组装、
//! 请求收尾指标计量口径（本地 recorder 隔离，不污染全局）。

use std::net::SocketAddr;
use std::time::Duration;

use metrics_exporter_prometheus::PrometheusBuilder;
use rgateway_core::{ErrorKind, Protocol, RequestCtx, UpstreamTarget};

use super::*;

/// 构造带路由与上游目标的请求上下文（访问记录字段组装用）。
fn ctx_with_target() -> RequestCtx {
    let mut ctx = RequestCtx::new(
        Protocol::Http1,
        Some("10.0.0.5:5555".parse::<SocketAddr>().expect("addr")),
    );
    ctx.route_id = Some(std::sync::Arc::from("r1"));
    ctx.set_target(UpstreamTarget {
        instance_id: std::sync::Arc::from("127.0.0.1:9000"),
        addr: "127.0.0.1:9000".parse().expect("addr"),
    });
    ctx
}

/// 表格驱动：UNIX 秒 → RFC3339 UTC（覆盖闰年二月末、整百年非闰、年界、
/// 秒界、前纪元）。
#[test]
fn rfc3339_table() {
    // cases 列：(UNIX 秒, 期望 RFC3339 UTC)
    let cases: &[(i64, &str)] = &[
        (0, "1970-01-01T00:00:00Z"),
        (-1, "1969-12-31T23:59:59Z"),
        (-86_401, "1969-12-30T23:59:59Z"),       // 前纪元跨日
        (951_782_400, "2000-02-29T00:00:00Z"),   // 闰年二月末（400 整除年闰）
        (1_582_934_400, "2020-02-29T00:00:00Z"), // 另一个闰年
        (1_709_164_800, "2024-02-29T00:00:00Z"),
        (-2_208_988_800, "1900-01-01T00:00:00Z"), // 整百年非闰（100 整除不闰）
        (-2_203_891_200, "1900-03-01T00:00:00Z"), // 1900 二月只有 28 天
        (4_102_444_800, "2100-01-01T00:00:00Z"),  // 未来整百年非闰
        (1_735_689_599, "2024-12-31T23:59:59Z"),  // 年界最后一秒
        (1_735_689_600, "2025-01-01T00:00:00Z"),
        (1_700_000_000, "2023-11-14T22:13:20Z"), // 非整日锚点
    ];
    for &(secs, want) in cases {
        assert_eq!(rfc3339_utc(secs), want, "unix_secs={secs}");
    }
}

/// 表格驱动：ErrorKind → 访问日志/指标共用的小写标签（9 个变体全覆盖）。
#[test]
fn kind_label_table() {
    // cases 列：(错误分类, 期望标签)
    let cases: &[(ErrorKind, &str)] = &[
        (ErrorKind::Unavailable, "unavailable"),
        (ErrorKind::DeadlineExceeded, "deadline_exceeded"),
        (ErrorKind::RateLimited, "rate_limited"),
        (ErrorKind::CircuitOpen, "circuit_open"),
        (ErrorKind::Unauthenticated, "unauthenticated"),
        (ErrorKind::PermissionDenied, "permission_denied"),
        (ErrorKind::NotFound, "not_found"),
        (ErrorKind::InvalidArgument, "invalid_argument"),
        (ErrorKind::Internal, "internal"),
    ];
    for &(kind, want) in cases {
        assert_eq!(kind_label(kind), want, "{kind:?}");
    }
}

// 访问记录字段组装：ctx 里的路由/上游/对端/协议/关联 id 全部落到记录上。
#[test]
fn access_record_captures_context_fields() {
    let ctx = ctx_with_target();
    let record = access_record(&ctx, "GET", "/api/x", 200, None);
    assert_eq!(record.request_id, ctx.request_id.to_string());
    assert_eq!(record.peer.as_deref(), Some("10.0.0.5:5555"));
    assert_eq!(record.method, "GET");
    assert_eq!(record.path, "/api/x");
    assert_eq!(record.route_id.as_deref(), Some("r1"));
    assert_eq!(record.protocol, "http1");
    assert_eq!(record.status, 200);
    assert_eq!(record.upstream.as_deref(), Some("127.0.0.1:9000"));
    assert_eq!(record.error_kind, None);
    assert!(record.duration < Duration::from_secs(1));
}

// 无路由/无上游/无对端：Option 字段全 None，error_kind 按分类落标签。
#[test]
fn access_record_missing_fields_stay_none() {
    let ctx = RequestCtx::new(Protocol::Grpc, None);
    let record = access_record(&ctx, "POST", "/nope", 404, Some(ErrorKind::NotFound));
    assert_eq!(record.peer, None);
    assert_eq!(record.route_id, None);
    assert_eq!(record.upstream, None);
    assert_eq!(record.protocol, "grpc");
    assert_eq!(record.error_kind.as_deref(), Some("not_found"));
}

/// 在本地 recorder 里跑一次闭包并渲染指标文本（全局隔离，测试可并行）。
fn render_recorded(f: impl FnOnce()) -> String {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, f);
    handle.render()
}

/// 表格驱动：请求收尾指标计量口径。列含义：
/// (标签, 有路由?, 错误分类, 期望出现的指标子串, 期望不出现的指标名)
#[test]
fn record_metrics_semantics_table() {
    let cases: &[(&str, bool, Option<ErrorKind>, &str, &str)] = &[
        (
            "routed-ok",
            true,
            None,
            "rgateway_requests_total",
            "rgateway_upstream_errors_total",
        ),
        (
            "routed-upstream",
            true,
            Some(ErrorKind::Unavailable),
            "rgateway_upstream_errors_total",
            "rgateway_route_not_found_total",
        ),
        (
            "routed-deadline",
            true,
            Some(ErrorKind::DeadlineExceeded),
            "rgateway_upstream_errors_total",
            "rgateway_route_not_found_total",
        ),
        (
            "routed-rate-limited",
            true,
            Some(ErrorKind::RateLimited),
            "rgateway_requests_total",
            "rgateway_upstream_errors_total",
        ),
        (
            "unrouted-not-found",
            false,
            Some(ErrorKind::NotFound),
            "rgateway_route_not_found_total",
            "rgateway_requests_total",
        ),
    ];
    for &(label, routed, error, want, absent) in cases {
        let route = routed.then_some("r1");
        let text = render_recorded(|| {
            record_metrics(route, "GET", 200, Duration::from_millis(5), error);
        });
        assert!(text.contains(want), "{label}: 缺 {want}: {text}");
        assert!(!text.contains(absent), "{label}: 不该有 {absent}: {text}");
    }
}

// 未路由的闸门拒绝（并发 429 / 请求体 400）：不进任何请求指标（口径见
// access 模块文档——无 route 标签可用，编造假标签违反基数红线），
// 事实仍由访问日志记录。
#[test]
fn unrouted_gate_rejection_records_nothing() {
    let text = render_recorded(|| {
        record_metrics(
            None,
            "GET",
            429,
            Duration::from_millis(1),
            Some(ErrorKind::RateLimited),
        );
    });
    assert!(!text.contains("rgateway_"), "闸门拒绝不该进指标: {text}");
}

// 上游错误的 kind 标签进指标（小写、下划线形态）。
#[test]
fn upstream_error_kind_reaches_label() {
    let text = render_recorded(|| {
        record_metrics(
            Some("r1"),
            "GET",
            503,
            Duration::from_millis(1),
            Some(ErrorKind::DeadlineExceeded),
        );
    });
    assert!(
        text.contains("kind=\"deadline_exceeded\""),
        "kind 标签缺失: {text}"
    );
}

// 方法标签基数红线在计量处生效：白名单外方法收敛为 OTHER。
#[test]
fn forged_method_normalizes_to_other() {
    let text = render_recorded(|| {
        record_metrics(Some("r1"), "PURGE", 200, Duration::from_millis(1), None);
    });
    assert!(text.contains("method=\"OTHER\""), "method 未归一: {text}");
    assert!(!text.contains("PURGE"), "白名单外方法不得进标签: {text}");
}

// 并发 gauge 严格配对：守卫建立 +1、drop -1（渲染值逐一断言，避免
// 数值格式差异 "0" vs "0.0" 导致脆弱断言）。
#[test]
fn gauge_guard_pairs_open_and_close() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        let guard = GaugeGuard::new();
        assert_eq!(
            gauge_value(&handle.render(), "rgateway_active_requests"),
            1.0
        );
        drop(guard);
    });
    assert_eq!(
        gauge_value(&handle.render(), "rgateway_active_requests"),
        0.0
    );
}

/// 从 Prometheus 文本里取 gauge 的当前值（匹配行的末段浮点；缺失返回 NaN，
/// 断言会显式失败而不是误判通过）。
fn gauge_value(text: &str, name: &str) -> f64 {
    text.lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|token| token.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}
