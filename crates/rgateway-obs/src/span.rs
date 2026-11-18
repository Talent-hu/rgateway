//! 请求级 tracing span 构造——统一命名与字段口径，防止 span 字段成为注入面。
//!
//! span 的名字与字段就是日志/追踪管道的 schema：集中在这里定义，
//! 调用方（dispatch、重试、负载均衡）不再各自拼字段。

use tracing::{info_span, Span};

use crate::access_log::sanitize_text;

/// 构造请求根 span（name=`request`，info 级）。
///
/// 为什么 info 级：请求 span 是访问日志与指标的关联锚点，采样口径与访问
/// 日志一致（默认全量），debug 级会被环境过滤器丢掉、trace 级噪音过大。
/// 字段与 [`crate::access_log::AccessRecord`] 对齐（request_id/method/
/// path/protocol），并同样做 ASCII 控制字符脱敏——span 字段最终进日志
/// 管道，与访问日志共用同一个注入威胁模型。
///
/// 生命周期由调用方管理：本函数只**构造**不进入，调用方用
/// `span.instrument(future)` 挂到异步任务，或 `span.enter()` 手动进入；
/// 不返回 guard，避免 API 强持有借用。
pub fn request_span(request_id: &str, method: &str, path: &str, protocol: &str) -> Span {
    let request_id = sanitize_text(request_id);
    let method = sanitize_text(method);
    let path = sanitize_text(path);
    let protocol = sanitize_text(protocol);
    info_span!(
        "request",
        request_id = %request_id,
        method = %method,
        path = %path,
        protocol = %protocol
    )
}

/// 构造上游尝试 span（name=`upstream_attempt`，info 级）。
///
/// 重试/负载均衡下一次请求可能有多次上游尝试，每次尝试独立成 span 便于
/// 区分「哪一跳失败」；instance_id 同样脱敏——服务发现数据也是外部输入，
/// 不信任任何外部字符串。
pub fn upstream_span(instance_id: &str) -> Span {
    let instance_id = sanitize_text(instance_id);
    info_span!("upstream_attempt", instance_id = %instance_id)
}

#[cfg(test)]
#[path = "span_tests.rs"]
mod tests;
