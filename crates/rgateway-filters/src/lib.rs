//! 内置过滤器运行时：RateLimit / Timeout / BodyLimit / CircuitBreaker / Retry。
//!
//! 本 crate 实现 `rgateway-config` 的 [`FilterFactory`] 契约，为五个内置
//! 过滤器提供真实的请求期行为，并通过 [`registry`] 在编译期一次性注册。
//! 每个过滤器产出一个 [`ServiceWrapper`]（`BoxedService -> BoxedService`
//! 闭包），由配置编译管线按 canonical 槽位（[`slot_for`]）叠进链里：
//!
//! `Compression → CORS → RateLimit → Auth → Route(rewrite/header/limit) →
//!  Deadline → Retry → Target → Breaker → Timeout → Handler`
//!
//! # 设计要点（为什么这样设计）
//!
//! * **治理状态跨热重载存活**：限流桶经 [`RouteState::preserved`] 槽保留，
//!   熔断器直接复用 core 的实例级 [`Breaker`]（`breaker_for` 热重载语义
//!   是 reconfigure + 保留滑动窗口）。一次 reload 绝不清零治理统计。
//! * **限流桶表有界**：桶键含客户端 IP 维度，攻击者可伪造海量源 IP 放大
//!   内存；因此桶表带容量上限常量与 LRU 淘汰策略（见 [`rate_limit`]）。
//! * **超时只约束响应头**：Timeout 与 Deadline 同契约——拿到响应头后
//!   响应体流式继续，不被超时截断（SSE / 大文件下载是常态）。
//! * **请求体错误归客户端故障**：BodyLimit 超限与请求体读错误统一映射
//!   [`GatewayError::InvalidArgument`]（HTTP 400），不归 Unavailable——
//!   断流/超限是客户端行为，不是上游故障。
//! * **重试一轮 = 重新走内层链**：Retry 包在 Target 外侧，重试时 Target
//!   重新选实例（熔断打开的实例被跳过）；可重放缓冲有界，超限禁用重试
//!   单次透传（见 [`retry`] / [`replay`]）。
//! * **错误不泄露内部细节**：对外文案一律经
//!   [`GatewayError::public_message`] / [`GatewayError::grpc_message`]；
//!   变体携带的字符串（路由 id、实例 id、超时预算等）只进日志。
//!
//! 测试策略：行为测试用假 inner 服务（计数/可控延迟/可控错误）驱动
//! `BoxedService` 链，不碰真实网络；时间敏感测试用
//! `#[tokio::test(start_paused = true)]` + `tokio::time::advance`，限流
//! 补充 governor 的 [`FakeRelativeClock`](governor::clock::FakeRelativeClock)
//! 精确推进 GCRA 时钟。

pub mod api_key;
mod backoff;
pub mod body_limit;
pub mod breaker;
pub mod ip_filter;
pub mod jwt_auth;
mod params;
pub mod rate_limit;
pub mod registry;
mod replay;
pub mod retry;
pub mod timeout;
mod util;

pub use api_key::ApiKeyAuthFactory;
pub use body_limit::BodyLimitFactory;
pub use breaker::CircuitBreakerFactory;
pub use ip_filter::IpFilterFactory;
pub use jwt_auth::JwtAuthFactory;
pub use rate_limit::RateLimitFactory;
pub use registry::registry;
pub use retry::RetryFactory;
pub use timeout::TimeoutFactory;

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

pub use rgateway_config::{
    slot_for, FilterBuildCtx, FilterFactory, FilterRegistry, FilterSpec, ServiceWrapper,
};
