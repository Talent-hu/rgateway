//! rgateway-server：网关二进制的库层——协议探测、请求分发、过滤器占位
//! 与启动装配（bootstrap）。
//!
//! 本 crate 是 `rgateway` 可运行二进制的载体，把下层各 crate 装配成一次
//! 完整的请求分发：
//!
//! * [`protocol`]——请求协议探测（content-type / upgrade / HTTP 版本）与
//!   路由 `protocol` 提示的覆写解析；
//! * [`dispatch`]——请求分发入口 [`Dispatcher`]：并发闸、请求体限长、路由
//!   匹配、过滤器链执行、协议感知错误渲染与访问日志；
//! * `access`——请求收尾的可观测性组装：访问日志记录、指标计量口径与
//!   并发 gauge 守卫（`rgateway-obs` 的装配面）；
//! * [`noop_filters`]——12 个规范过滤器名的恒等（no-op）占位工厂：让示例
//!   配置在过滤器运行时实现（P4 的 `rgateway-filters`）落地前也能加载生效，
//!   槽位排入链骨架但无拦截效果；
//! * [`bootstrap`]——启动装配：配置加载/校验 → TLS 拒绝闸 → tracing 初始化
//!   → 编译快照 + 热重载 → 监听 accept 循环 → 优雅关闭（[`bootstrap::start`]）。

mod access;

pub mod bootstrap;
pub mod dispatch;
pub mod noop_filters;
pub mod protocol;

// 常用入口提升到 crate 根：装配层通常直接 `use rgateway_server::Dispatcher`。
pub use dispatch::Dispatcher;
