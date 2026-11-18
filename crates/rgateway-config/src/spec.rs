//! 归一化的过滤器/谓词规格与过滤器工厂契约。
//!
//! YAML 里每个谓词/过滤器有两种写法——紧凑 DSL 字符串（`"Method=GET,POST"`、
//! `"RateLimit=key:client_ip,rate:100,period:1s"`）与结构化 map——本模块把两者
//! 统一归一化为 [`PredicateSpec`] / [`FilterSpec`]。工厂接收归一化 spec，返回
//! 一个 [`ServiceWrapper`]，把一层 tower layer 叠加到已组装的内层链上。
//!
//! 设计要点：
//!
//! - **参数访问器兼容两种来源**：紧凑 DSL 解析出的值全部是字符串，结构化
//!   YAML 则携带 number/bool/sequence/mapping 等原生类型；`param_*` 访问器
//!   （实现见 `spec_params.rs`）对两种形态统一做转换，过滤器实现无需关心
//!   用户写了哪种 YAML 形状（详见 [`FilterSpec`] 文档）。
//! - **依赖倒置**：[`FilterFactory`] / [`RouteHandlerFactory`] 这两个 trait
//!   定义在 config crate 而不是 core——core 不感知过滤器的存在，proxy crate
//!   实现这些 trait 并在编译期注入。依赖方向 proxy → config → core，
//!   保持 crate 依赖无环。
//! - **链槽位**：[`FilterSlot`] 固定各过滤器在 canonical 链序中的位置；
//!   槽位间顺序由枚举声明序（`Ord`）决定，槽内保持配置顺序。

use std::collections::BTreeMap;
use std::sync::Arc;

use rgateway_core::{BoxedService, ProtocolHint, RouteState, RouteUri, ServiceDiscovery};

use crate::dsl;
use crate::error::{ConfigError, Location};
use crate::schema::{FilterConfig, ParamValue, PredicateConfig, TranscodingConfig};

#[path = "spec_params.rs"]
mod params;

pub use params::parse_ip_net;

/// 归一化的谓词：`"Method=GET,POST"` → `{name: "Method", args: [...]}`。
/// `source` 保留原始文本，仅用于错误诊断展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateSpec {
    /// 谓词名（`Path` / `Method` / `Header` / `Host` / `Query`）。
    pub name: String,
    /// 位置参数（如 `Method` 的方法名列表、`Header` 的 name + regex 二元组）。
    pub args: Vec<String>,
    /// 原始文本，仅用于诊断。
    pub source: String,
}

impl PredicateSpec {
    /// 解析紧凑 DSL 谓词串（`Name=arg1,arg2`；`\,` 是字面逗号、`\\` 是字面
    /// 反斜杠，先按未转义逗号切分再去转义）。
    ///
    /// 错误：缺少 `=`、名字为空、末尾悬挂转义符时返回 [`ConfigError`]（定位 `predicate` 段）。
    pub fn parse_compact(text: &str) -> Result<Self, ConfigError> {
        let (name, args) = dsl::parse_named_args(text)
            .map_err(|m| ConfigError::value(Location::section("predicate"), m))?;
        Ok(Self {
            name,
            args,
            source: text.to_string(),
        })
    }

    /// 归一化任一 YAML 形状：紧凑串走 [`Self::parse_compact`]；结构化形态
    /// 直接取 name/args，并重建 `source`（`name=arg1,arg2`）用于诊断。
    pub fn from_config(cfg: &PredicateConfig) -> Result<Self, ConfigError> {
        match cfg {
            PredicateConfig::Compact(text) => Self::parse_compact(text),
            PredicateConfig::Structured(s) => Ok(Self {
                name: s.name.clone(),
                args: s.args.clone(),
                source: format!("{}={}", s.name, s.args.join(",")),
            }),
        }
    }
}

/// 归一化的过滤器规格：名字 + 参数表 + 原始文本（仅用于诊断）。
///
/// # 参数访问器族
///
/// `param_*` 系列（实现拆在 `spec_params.rs`）抹平两种 YAML 来源的差异，
/// 规则统一：
///
/// - 紧凑 DSL（`RateLimit=rate:100`）的值全是字符串；结构化 map 可能是
///   number/bool/sequence/mapping。每个访问器先取原生类型，失败再回退
///   字符串解析，因此两种写法等价。
/// - `param_*`（无 `_opt` 后缀）是必填：缺失或类型不符返回 [`ConfigError`]；
///   `param_*_opt` 是可选：缺失返回 `Ok(None)`，仅在「有值但类型/格式
///   不符」时报错——严格区分「没配」与「配错」。
/// - 字符串形态的单位语法：`param_duration` 数字按毫秒、字符串带
///   `ms/s/m/h/d` 后缀；`param_size` 数字按字节、字符串带
///   `B/KB/MB/GB` 后缀（均 1024 进制）；`param_f64` 接受尾部 `%`
///   按百分比折算。
/// - 列表类访问器（`param_str_list` / `param_u64_list`）
///   把「缺失」视作空表返回，与其他访问器的 `Ok(None)` 风格不同。
/// - 所有错误消息都以 `source` 为 [`Location`] 并点名出错参数，保证诊断
///   能定位到具体过滤器条目。
#[derive(Debug, Clone, PartialEq)]
pub struct FilterSpec {
    pub name: String,
    pub params: BTreeMap<String, ParamValue>,
    /// 原始文本，仅用于诊断。
    pub source: String,
}

impl FilterSpec {
    /// 解析紧凑 DSL 过滤器串（`Name=k1:v1,k2:v2`）：逗号转义规则同谓词；
    /// 每段按**第一个**冒号切成 key/value（值内可再含冒号，如
    /// `x-uri:http://a:8080/v`），参数值一律是字符串。
    ///
    /// 错误：缺少 `=`、key 为空、段内没有冒号时返回 [`ConfigError`]（定位 `filter` 段）。
    pub fn parse_compact(text: &str) -> Result<Self, ConfigError> {
        let (name, params) = dsl::parse_compact_filter(text)
            .map_err(|m| ConfigError::value(Location::section("filter"), m))?;
        Ok(Self {
            name,
            params,
            source: text.to_string(),
        })
    }

    /// 归一化任一 YAML 形状。结构化形态的 `source` 只记录名字（原始 map
    /// 体积大，诊断只需定位到条目）。
    pub fn from_config(cfg: &FilterConfig) -> Result<Self, ConfigError> {
        match cfg {
            FilterConfig::Compact(text) => Self::parse_compact(text),
            FilterConfig::Structured(s) => Ok(Self {
                name: s.name.clone(),
                params: s.params.clone(),
                source: s.name.clone(),
            }),
        }
    }
}

/// 过滤器在 canonical 链序中的槽位（外 → 内）：
/// `Compression → CORS → RateLimit → Auth → Route → Deadline → Retry →
/// Breaker → Timeout`。
///
/// - 顺序固定的原因：横切关注点按语义分层——表示层（Compression、CORS）
///   最先处理响应与预检；流量治理（RateLimit、Auth）在进入路由改写前拒掉
///   超额/非法请求；Route 槽做请求改写；Retry/Breaker/Timeout 越靠近内层，
///   越贴近每次上游尝试的执行语义。链序里的 Deadline 不是过滤器槽位，
///   编译期固定插在 Route 与 Retry 之间约束整体请求预算。
/// - 变体声明序即排序序（derive `Ord`），compile 按槽位分组组装；同一
///   槽位内保持配置顺序（路由声明的过滤器先于 `defaults.filters`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilterSlot {
    Compression,
    Cors,
    RateLimit,
    Auth,
    Route,
    Retry,
    Breaker,
    Timeout,
}

/// 全部 canonical 过滤器名（与 [`slot_for`] 的映射域严格一致——单测对拍）。
///
/// 装配方（server 的注册表）与测试以本表为准做双向核对：`slot_for` 新增
/// 名字而注册表漏挂占位时测试立即失败，而不是等到示例配置加载才报
/// `UnknownFilter`。
pub const FILTER_NAMES: [&str; 12] = [
    "Compression",
    "CORS",
    "RateLimit",
    "JwtAuth",
    "ApiKeyAuth",
    "IpFilter",
    "RewritePath",
    "RequestHeader",
    "BodyLimit",
    "Retry",
    "CircuitBreaker",
    "Timeout",
];

/// 过滤器名 → 链槽位映射；未知名返回 `None`（validate/compile 据此报
/// UnknownFilter）。安全/准入类（`JwtAuth`/`ApiKeyAuth`/`IpFilter`）归
/// Auth 槽，请求改写类（`RewritePath`/`RequestHeader`/`BodyLimit`）归
/// Route 槽——同槽内仍按配置顺序执行。
pub fn slot_for(name: &str) -> Option<FilterSlot> {
    Some(match name {
        "Compression" => FilterSlot::Compression,
        "CORS" => FilterSlot::Cors,
        "RateLimit" => FilterSlot::RateLimit,
        "JwtAuth" | "ApiKeyAuth" | "IpFilter" => FilterSlot::Auth,
        "RewritePath" | "RequestHeader" | "BodyLimit" => FilterSlot::Route,
        "Retry" => FilterSlot::Retry,
        "CircuitBreaker" => FilterSlot::Breaker,
        "Timeout" => FilterSlot::Timeout,
        _ => return None,
    })
}

/// 过滤器工厂为单个路由构建一层所需的上下文。
#[derive(Clone)]
pub struct FilterBuildCtx {
    /// 路由 id（限流 key、诊断日志等用）。
    pub route_id: Arc<str>,
    /// 跨配置重载保留的路由状态（限流桶、熔断器等），重载不清零。
    pub route_state: Arc<RouteState>,
    /// 服务发现句柄（Target 选实例时使用）。
    pub discovery: Arc<dyn ServiceDiscovery>,
    /// 可重放重试的最大请求体缓冲字节数。
    pub max_retry_buffer: usize,
    /// 配置文件所在目录；过滤器引用的相对路径（如 JwtAuth 的 `jwks_file`）
    /// 以此解析，且必须留在该目录之内。
    pub config_dir: Arc<std::path::PathBuf>,
}

impl FilterBuildCtx {
    /// 把过滤器参数里的相对路径引用解析为配置目录内的路径。
    ///
    /// 语义与 `compile_settings::resolve_in_dir` 完全一致（同走
    /// [`crate::loader::resolve_path`]）：相对引用以配置目录为基准做纯词法
    /// 归一化，必须留在目录内——绝对路径（POSIX 根、盘符、UNC）与 `..`
    /// 逃逸一律拒绝；错误消息只回显引用文本本身，不回显解析后的目标路径。
    ///
    /// 错误：引用逃逸或为绝对路径时返回 [`ConfigError`]。
    pub fn resolve_path(&self, raw: &str) -> Result<std::path::PathBuf, ConfigError> {
        // loader::resolve_path 以「配置文件」为基准取其父目录；这里拼一个
        // 合成文件名复用同一套包含规则。
        crate::loader::resolve_path(&self.config_dir.join("rgateway.yaml"), raw)
    }
}

/// 为归一化的过滤器规格构建一层 tower layer 的工厂。
///
/// trait 定义在 config crate 是依赖倒置的关键：具体过滤器实现（在网关
/// 二进制 crate 里）依赖 config 并在编译期注册，config 反向不依赖任何
/// 实现。工厂实例被多路由并发调用，故要求 `Send + Sync`。
pub trait FilterFactory: Send + Sync {
    /// canonical 过滤器名，必须与 [`slot_for`] 的映射一致。
    fn name(&self) -> &'static str;

    /// 为单个路由构建一层。返回的 wrapper 把该层套在已组装的内层链
    /// 之外（内层服务作参数传入）。
    ///
    /// 错误：spec 参数缺失/非法时返回带定位的 [`ConfigError`]。
    fn build(&self, spec: &FilterSpec, ctx: &FilterBuildCtx)
        -> Result<ServiceWrapper, ConfigError>;
}

/// 层包装闭包：`BoxedService -> BoxedService`，把过滤器套在传入的内层
/// 服务外面。用闭包而非 tower `Layer` trait 对象表达，便于直接叠加到
/// 已动态分发的 `BoxedService` 链上。
pub type ServiceWrapper = Box<dyn Fn(BoxedService) -> BoxedService + Send + Sync>;

/// 以 canonical 名为键的过滤器工厂注册表（`BTreeMap` 保证 [`Self::names`]
/// 有序）。
#[derive(Default, Clone)]
pub struct FilterRegistry {
    factories: BTreeMap<&'static str, Arc<dyn FilterFactory>>,
}

impl FilterRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个工厂（按 [`FilterFactory::name`] 入表，同名覆盖）。
    pub fn register(&mut self, factory: Arc<dyn FilterFactory>) {
        self.factories.insert(factory.name(), factory);
    }

    /// 按 canonical 名取工厂。
    pub fn get(&self, name: &str) -> Option<&Arc<dyn FilterFactory>> {
        self.factories.get(name)
    }

    /// 所有已注册的 canonical 名（字典序）。
    pub fn names(&self) -> Vec<&'static str> {
        self.factories.keys().copied().collect()
    }
}

/// 交给最内层 handler 工厂的路由编译期信息。
#[derive(Clone)]
pub struct RouteCompileInfo {
    /// 路由 id。
    pub id: Arc<str>,
    /// 归一化后的目标 URI（`lb://服务名` 或直连地址）。
    pub uri: RouteUri,
    /// 协议提示（auto/http1/http2/grpc/... 的判定结果）。
    pub protocol_hint: ProtocolHint,
    /// 跨重载保留的路由状态。
    pub state: Arc<RouteState>,
    /// gRPC transcoding 配置（如有）。
    pub transcoding: Option<Arc<TranscodingConfig>>,
}

/// 构建最内层 per-route 服务（协议分发/代理 handler）的工厂。
/// 由 `rgateway-proxy` 实现并注入 config 编译期，使 config crate 不依赖
/// proxy——与 [`FilterFactory`] 同属依赖倒置设计。
pub trait RouteHandlerFactory: Send + Sync {
    /// 为路由构建最内层服务。
    fn build(&self, route: &RouteCompileInfo) -> BoxedService;
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
