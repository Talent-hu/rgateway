//! 请求收尾的可观测性组装：访问日志记录组装、指标计量口径与并发 gauge 守卫。
//!
//! # 设计取舍（口径单点收口）
//!
//! * **指标计量**（[`record_metrics`]）：命中路由的完成请求计
//!   `rgateway_requests_total{route,method,status}` 与耗时直方图；上游类
//!   错误（`unavailable` / `deadline_exceeded`）再计
//!   `rgateway_upstream_errors_total{route,kind}`；路由未命中（NotFound 且
//!   无 route）计 `rgateway_route_not_found_total`；路由匹配**之前**的闸门
//!   拒绝（并发 429 / 请求体 400）不进任何计数器——此时没有 route 标签
//!   可用，编造 `route="-"` 之类的假值违反 rgateway-obs 的标签基数红线，
//!   事实由访问日志逐条记录。链内重试尝试次数在 dispatch 层不可见，上游
//!   类计数记的是「每完成请求的最终上游类错误」。
//! * **并发 gauge 严格配对**：[`GaugeGuard`] 建立时 +1、drop 时 -1，守卫
//!   挂进响应体包装层（与并发许可同一生命周期）——请求取消、连接中断也
//!   走 Drop 收尾，gauge 不会漂移。
//! * **RFC3339 时间戳自给**：[`rfc3339_utc`] 是纯整数的 civil-from-days
//!   算法（Howard Hinnant），不为一个函数引入时间格式化依赖；表驱动测试
//!   锚定闰年二月末、年界与前纪元。
//! * **错误分类标签**（[`kind_label`]）：`ErrorKind` 的小写 Debug 名
//!   （`rate_limited` 等），访问日志 `error_kind` 与指标 `kind` 共用同一
//!   标签，恰好落在 rgateway-obs 的 `[a-z0-9_]{1,64}` 格式红线内。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rgateway_core::{ErrorKind, RequestCtx};
use rgateway_obs::{
    record_active_requests_close, record_active_requests_open, record_request,
    record_route_not_found, record_upstream_error, AccessRecord,
};

/// 并发 gauge 守卫：建立 +1、drop -1（见模块文档）。
pub(crate) struct GaugeGuard;

impl GaugeGuard {
    /// 建立守卫（`rgateway_active_requests` +1）。
    pub(crate) fn new() -> Self {
        record_active_requests_open();
        Self
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        record_active_requests_close();
    }
}

/// 构造访问日志记录（纯函数）：ctx 与收尾事实 → 不可变快照。
pub(crate) fn access_record(
    ctx: &RequestCtx,
    method: &str,
    path: &str,
    status: u16,
    error: Option<ErrorKind>,
) -> AccessRecord {
    AccessRecord {
        request_id: ctx.request_id.to_string(),
        peer: ctx.peer_addr.map(|addr| addr.to_string()),
        method: method.to_string(),
        path: path.to_string(),
        route_id: ctx.route_id.as_ref().map(|id| id.to_string()),
        protocol: ctx.protocol.as_label().to_string(),
        status,
        upstream: ctx.target().map(|target| target.instance_id.to_string()),
        error_kind: error.map(|kind| kind_label(kind).to_string()),
        duration: ctx.elapsed(),
    }
}

/// 错误分类 → 稳定 snake_case 标签（访问日志 `error_kind` 与指标 `kind`
/// 共用）。显式映射表：`ErrorKind` 新增变体时编译器强制补全，不会出现
/// 「Debug 名直拼」把 `RateLimited` 拼成 `ratelimited` 的口径漂移。
pub(crate) fn kind_label(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Unavailable => "unavailable",
        ErrorKind::DeadlineExceeded => "deadline_exceeded",
        ErrorKind::RateLimited => "rate_limited",
        ErrorKind::CircuitOpen => "circuit_open",
        ErrorKind::Unauthenticated => "unauthenticated",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::NotFound => "not_found",
        ErrorKind::InvalidArgument => "invalid_argument",
        ErrorKind::Internal => "internal",
    }
}

/// 请求收尾指标计量（口径见模块文档第一条）。
///
/// `duration` 的口径是**请求进入到响应头就绪**（`ctx.elapsed()` 于收尾处
/// 取值）；流式响应体的传输时长不计入——直方图度量的是网关路由+链执行+
/// 上游首包的延迟，不是大文件下载时长。
pub(crate) fn record_metrics(
    route_id: Option<&str>,
    method: &str,
    status: u16,
    duration: Duration,
    error: Option<ErrorKind>,
) {
    match (route_id, error) {
        (Some(route), Some(error @ (ErrorKind::Unavailable | ErrorKind::DeadlineExceeded))) => {
            record_request(route, method, status, duration);
            record_upstream_error(route, kind_label(error));
        }
        (Some(route), _) => record_request(route, method, status, duration),
        (None, Some(ErrorKind::NotFound)) => record_route_not_found(),
        (None, _) => {}
    }
}

/// 渲染一行访问日志（格式由快照 `access_log_json` 决定，时间戳取当前时刻）。
pub(crate) fn render_access(record: &AccessRecord, json: bool) -> String {
    let ts = rfc3339_now();
    if json {
        rgateway_obs::render_json(record, &ts)
    } else {
        rgateway_obs::render_text(record, &ts)
    }
}

/// 当前时刻的 RFC3339 UTC；时钟早于纪元时退回纪元起点（防御性兜底）。
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    rfc3339_utc(secs)
}

/// 把 UNIX 秒格式化为 RFC3339 UTC（`YYYY-MM-DDTHH:MM:SSZ`）。
///
/// 纯整数的 civil-from-days 算法（Howard Hinnant），无外部依赖；`div_euclid`
/// 保证前纪元（负秒）的日期回退正确。
pub(crate) fn rfc3339_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    // 以 0000-03-01 为纪元平移（闰年集中在二月末的取整技巧）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400;
    // 一、二月属于上一个「三月纪元」年，回补 1。
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;
