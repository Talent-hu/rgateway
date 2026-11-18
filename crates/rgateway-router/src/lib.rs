//! 路由匹配：ant 风格路径模式与有序路由表。
//!
//! 本 crate 在 rgateway-core 的领域类型之上提供「请求 → 命中路由」的匹配
//! 能力，位于 crate 分层的第二层（core ← **router** ← config ← …）：只依赖
//! rgateway-core 与 http，不感知配置格式与代理行为——路由表如何从 YAML
//! 编译而来是 rgateway-config 的职责，这里只关心匹配本身。
//!
//! # 模块导出图
//!
//! * [`ant_path`]——ant 通配模式 [`AntPattern`] 与 Path 谓词
//!   [`PathPredicate`]；`?`/`*`/`**` 的语义、尾部斜杠行为与匹配算法的
//!   完整说明见该模块文档
//! * [`matcher`]——按 `(order, id)` 排序的路由表 [`RouteTable`]，
//!   确定性的首条匹配（first-match-wins）查找
//!
//! 公开入口只有三个类型，全部在 crate 根重导出。

pub mod ant_path;
pub mod matcher;

pub use ant_path::{AntPattern, PathPredicate};
pub use matcher::RouteTable;
