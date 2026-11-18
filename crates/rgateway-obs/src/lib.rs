//! rgateway 的可观测性出口——访问日志、请求 span 与（下一阶段的）指标导出。
//!
//! # 为什么单独成 crate
//!
//! 访问日志格式、span 字段口径与指标命名都是**横切关注点**，会被 dispatch、
//! 过滤器、管理面多处调用；收进独立 crate 后可以脱离具体协议栈独立测试
//! （本 crate 不依赖任何 rgateway_* crate，全部字段用原语 `String`/`u16`/
//! [`Duration`](std::time::Duration)，调用方传什么记什么），也避免日志格式
//! 散落在服务层里各写一份。
//!
//! # 两条输出通道的口径
//!
//! 按 `docs/architecture.md` §8 的错误双通道约定：
//!
//! * **访问日志属日志通道**——可以记内部细节（上游地址、error_kind 等），
//!   供排障使用；
//! * 同一行**严禁混入密钥/令牌**——渲染与 span 字段写入前统一做 ASCII
//!   控制字符脱敏（防日志注入），密钥本就不该出现在这些字段里。
//!
//! # 模块导出图
//!
//! * [`access_log`]——[`AccessRecord`] 与 JSON/text 两种单行渲染
//! * [`span`]——请求级 / 上游尝试级 tracing span 构造
//! * [`metrics`]——Prometheus 指标：recorder 安装（[`metrics::install`]）与
//!   请求级仪表函数（指标名/标签口径与标签基数红线收口在该模块）

pub mod access_log;
pub mod metrics;
pub mod span;

pub use access_log::{render_json, render_text, AccessRecord};
pub use metrics::{
    install, record_active_requests_close, record_active_requests_open, record_request,
    record_route_not_found, record_upstream_error, MetricsHandle,
};
pub use span::{request_span, upstream_span};

/// 可观测性子系统的类型化错误。
///
/// 访问日志渲染与 span 构造都是纯函数、不会失败；会失败的是 metrics 的
/// recorder 安装（[`metrics::install`]）。按「库 crate 用 thiserror 定义
/// 类型化错误」的约定统一错误面，调用方错误处理口径保持一致。`Display`
/// 文案走日志通道，可携带内部细节，但绝不包含密钥/令牌。
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    /// 指标 recorder 安装或导出器初始化失败（如重复安装）。
    #[error("failed to install metrics recorder: {0}")]
    RecorderInstall(String),
}
