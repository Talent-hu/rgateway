//! rgateway 配置管线：YAML schema → 加载（load）→ 校验（validate）→
//! 编译（compile，产出不可变快照 + 过滤器链）→ 热重载（notify 文件
//! 监听与手动 reload），并通过 [`RuntimeStateRegistry`] 在重载之间保留
//! 治理状态（熔断器等），使一次配置变更不会清零运行时统计。
//!
//! # 模块导出图
//!
//! 数据流：`schema`（YAML 结构）→ `loader`（解析）→ `validate`（语义校验）
//! → `compile`（编译成 [`CompiledConfig`]）→ `snapshot` + `watcher`
//! （热重载）。运行时支撑：`target`（选上游）、`deadline`（总截止期）。
//!
//! * `schema` —— YAML 的 Serde 数据结构（[`ConfigFile`]），纯数据无逻辑。
//! * `loader` —— 从文件/字符串解析出 [`ConfigFile`]，错误带精确位置
//!   （[`Location`]）。
//! * `validate` —— 编译前的跨字段语义校验，快速失败（fail fast）。
//! * `dsl` —— `Path=/api/**` 这类谓词/过滤器 DSL 字符串的解析。
//! * `compile` —— 核心步骤：把校验后的配置编译成不可变的
//!   [`CompiledConfig`]（路由表、过滤器链、服务发现、服务器参数）。
//! * `spec` —— 编译期接口：过滤器注册表（[`FilterRegistry`]）、路由
//!   handler 工厂（[`RouteHandlerFactory`]）等。
//! * `target` —— 每次尝试的上游实例选择（`TargetSelector`）。
//! * `deadline` —— 总截止期（Deadline）层：只约束响应头，不约束响应体。
//! * `snapshot` —— 热重载状态：[`SnapshotStore`] 原子换快照、[`reload`]
//!   失败保旧、治理状态跨重载保留。
//! * `watcher` —— notify 文件监听 + 防抖（debounce）合并，触发 [`reload`]。
//! * `error` —— [`ConfigError`] 与 [`Location`]（配置错误定位）。
//!
//! 下方 `pub use` 把各模块的入口类型提升到 crate 根，调用方可
//! `use rgateway_config::{...}` 一站式引用。

pub mod compile;
pub mod deadline;
pub mod dsl;
pub mod error;
pub mod loader;
pub mod schema;
pub mod snapshot;
pub mod spec;
pub mod target;
pub mod validate;
pub mod watcher;

// ---- 编译管线入口：加载 / 校验 / 编译 ----
pub use compile::{compile, CompileInput, CompiledConfig, ServerSettings};
pub use error::{ConfigError, Location};
pub use loader::{load_from_file, load_from_str};
pub use schema::ConfigFile;
// ---- 热重载状态：快照存储、重载入口、治理状态注册表 ----
pub use snapshot::{reload, ReloadContext, RuntimeStateRegistry, SnapshotStore};
// ---- 过滤器/路由 handler 的编译期注册接口 ----
pub use spec::{
    parse_ip_net, slot_for, FilterBuildCtx, FilterFactory, FilterRegistry, FilterSlot, FilterSpec,
    PredicateSpec, RouteCompileInfo, RouteHandlerFactory, ServiceWrapper, FILTER_NAMES,
};
pub use validate::{ensure_valid, validate, MGMT_RESERVED_PATHS};
// ---- 文件监听（防抖后触发 reload） ----
pub use watcher::{spawn_watcher, ConfigWatcher};
