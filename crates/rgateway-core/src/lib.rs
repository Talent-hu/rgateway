//! rgateway 的核心领域模型——crate 分层的最底层。
//!
//! 本 crate 定义所有其他 crate 共享的**协议无关**类型：请求上下文
//! （[`RequestCtx`]）、路由与谓词（[`Route`]、[`RoutePredicate`]）、负载均衡
//! （[`LoadBalancer`]、[`Instance`]）、服务发现（[`ServiceDiscovery`]）、
//! 治理状态（熔断器 Circuit Breaker 与保留的过滤器状态，见 [`governance`]）
//! 以及网关错误层次（[`GatewayError`]）。它有意不依赖任何其他 rgateway
//! crate：领域模型不需要感知路由语法、配置格式或代理行为。
//!
//! # crate 分层与依赖方向
//!
//! 依赖自底向上单向流动，下层永不依赖上层：
//!
//! ```text
//! rgateway-core      协议无关领域模型，无任何 rgateway 内部依赖
//!     ↑
//! rgateway-router    路由匹配（ant 路径模式、有序路由表）
//!     ↑
//! rgateway-config    YAML 配置 → 校验 → 编译为不可变快照
//!     ↑
//! 更上层（filters、proxy、mgmt/server 等）在链条之上继续叠加
//! ```
//!
//! core 因此可以脱离路由、配置与代理独立测试和复用；上层通过这里重导出的
//! trait（[`RoutePredicate`]、[`LoadBalancer`]、[`ServiceDiscovery`]）与
//! 领域类型组合能力，而不是让 core 反向感知上层。
//!
//! # 模块导出图
//!
//! * [`body`]——代理响应体（[`ProxyBody`]）、Service 链装箱（[`BoxedService`]）
//!   与响应体工具函数
//! * [`discovery`]——服务发现 trait 与静态实现（[`StaticDiscovery`]）
//! * [`error`]——错误层次（[`GatewayError`]、[`ErrorKind`]）
//! * [`governance`]——熔断器与路由状态注册表（[`Breaker`]、[`StateRegistry`]）
//! * [`lb`]——负载均衡策略：随机、轮询、平滑加权轮询 SWRR
//! * [`predicate`]——路由谓词 trait 及常用实现（方法、Host、Header、Query、
//!   组合）
//! * [`route`]——路由、URI 与请求上下文（[`Route`]、[`RequestCtx`]）
//!
//! 下面的 `pub use` 把各模块的核心类型提升到 crate 根，调用方通常直接
//! `use rgateway_core::…` 即可，无需关心内部模块划分。

pub mod body;
pub mod discovery;
pub mod error;
pub mod governance;
pub mod lb;
pub mod predicate;
pub mod route;

// 根级重导出：上方「模块导出图」列出的核心类型，按所属模块分组。
pub use body::{
    boxed_body, empty_body, full_body, map_body_err, BoxedService, ProxyBody, ProxyError,
};
pub use discovery::{ServiceDiscovery, StaticDiscovery};
pub use error::{ErrorKind, GatewayError};
pub use governance::{
    Breaker, BreakerConfig, BreakerPermit, BreakerState, RouteState, StateKey, StateRegistry,
};
pub use lb::{Instance, LoadBalancer, RandomBalancer, RoundRobin, SmoothWeightedRoundRobin};
pub use predicate::{
    AlwaysPredicate, AndPredicate, HeaderPredicate, HostPredicate, MethodPredicate, QueryPredicate,
    RoutePredicate,
};
pub use route::{
    AuthContext, AuthKind, ClientScheme, Protocol, ProtocolHint, RequestCtx, Route, RouteUri,
    UpstreamTarget,
};
