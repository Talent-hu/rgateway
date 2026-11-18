//! rgateway 管理面（mgmt）REST 层：快照 / 路由 / 熔断器状态查询与熔断器
//! 重置，全部走项目统一信封 `{"success", "data", "error"}`。
//!
//! # 设计要点（为什么这样设计）
//!
//! * **请求级纯函数、不自带 accept 循环。** [`rest::handle`] 只消费一个
//!   `http::Request`、产出一个 `http::Response`，由装配方（rgateway-server）
//!   负责监听与分发。REST 与 gRPC 在同一端口按 content-type 复用：分流在
//!   装配方的 `net::mgmt_service`，gRPC 半边见 [`grpc::handle`]——seam 就在
//!   这层纯函数上，不需要再拆 accept 循环。
//! * **依赖全部注入。** [`state::MgmtState`] 持有 `Arc<SnapshotStore>`、
//!   `Arc<StateRegistry>` 与指标渲染闭包（`/metrics` 端点用）；本 crate 不做
//!   任何全局状态，测试可用真实编译快照驱动。
//! * **错误文案泛化。** 管理面错误只回显可行动的泛化信息（如
//!   `invalid request`），绝不回显文件系统路径、内部地址或配置原文——
//!   管理面默认只监听回环（由配置 `mgmt.listen` 决定），但仍按最小泄露
//!   原则输出。
//! * **读路径无副作用。** 所有 GET 端点只读快照与治理状态，不改动注册表。

pub mod grpc;
pub mod pb;
pub mod rest;
pub mod state;

pub use rest::{handle, MgmtBody};
pub use state::{MetricsRenderer, MgmtState};
