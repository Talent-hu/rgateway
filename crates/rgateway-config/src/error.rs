//! 配置错误与精确的来源定位。
//!
//! 每个校验 / 编译错误都携带一个 `location` 字符串（如
//! `routes[user-http].filters[2]`），让配置错误直接指向出问题的条目
//! （plan requirement: "校验错误含定位"）。
//!
//! 定位体系（[`Location`]）按配置坐标逐级构造：
//! `root`（整文件）→ `section`（顶层段）→ `route` / `indexed`（列表项）
//! → `field`（字段）→ `route_item`（路由内条目）。所有变体都带
//! [`Location`]，包括 [`ConfigError::Io`] / [`ConfigError::Yaml`]——
//! 这两类错误也发生在某个配置坐标上（文件本身读不到就是 `<root>`，
//! 嵌套读取如 descriptor 路径则是具体的 `routes[id].transcoding.descriptor`），
//! 统一带坐标可以让上层日志 / 报告的格式完全一致，不用按错误种类特判。

use std::fmt;

/// 配置条目的定位坐标（形如 `routes[user-http].filters[2]`）。
///
/// 就是一段拼好的字符串，只提供构造方法保证各段格式统一；
/// 展示时直接输出（[`fmt::Display`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location(String);

impl Location {
    /// 整文件级位置 `<root>`：I/O / YAML 解析这类还定位不到内部条目的错误。
    pub fn root() -> Self {
        Self("<root>".to_string())
    }

    /// 顶层配置段位置，如 `server`、`defaults`。
    pub fn section(name: &str) -> Self {
        Self(name.to_string())
    }

    /// 路由条目位置 `routes[id]`。
    pub fn route(id: &str) -> Self {
        Self(format!("routes[{id}]"))
    }

    /// 路由内列表条目位置 `routes[id].predicates[i]`——`kind` 是列表名
    /// （`predicates` / `filters` / `transcoding.rules` 等），`index` 是下标。
    pub fn route_item(route: &str, kind: &str, index: usize) -> Self {
        Self(format!("routes[{route}].{kind}[{index}]"))
    }

    /// 任意列表条目位置 `section[index]`（如 `defaults.filters[1]`）。
    pub fn indexed(section: &str, index: usize) -> Self {
        Self(format!("{section}[{index}]"))
    }

    /// 在当前位置上追加字段名：`current.field`（逐级下钻用）。
    pub fn field(&self, name: &str) -> Self {
        Self(format!("{}.{}", self.0, name))
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 加载、校验、编译配置过程中产生的错误。
///
/// 七个变体各自携带的信息：
/// * [`ConfigError::Io`] / [`ConfigError::Yaml`]——底层错误（`source`）
///   加出错坐标。**为什么也带 Location**：I/O 与解析错误同样发生在某个
///   配置位置上（多半是 `<root>`，也可能是具体字段引用的外部文件），
///   统一带坐标后所有错误的展示格式一致，上层无需特判。
/// * [`ConfigError::Invalid`] / [`ConfigError::Value`]——语义错误
///   （`message`）加坐标；`Value` 偏单个取值不合法，`Invalid` 偏结构性
///   不合法（缺必填、重复 id 等）。
/// * [`ConfigError::UnknownFilter`] / [`ConfigError::UnknownPredicate`]——
///   名字未注册（`name`）加坐标，坐标指向声明该名字的条目。
/// * [`ConfigError::Compile`]——编译成运行时结构失败（`message`）加坐标。
///
/// 所有变体都带 [`Location`]，诊断信息总能指向一个配置坐标
/// （整文件的 I/O / YAML 解析失败是 `<root>`，条目级问题是
/// `routes[<id>].filters[<i>]` 这类坐标）。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// I/O 错误（如配置文件读不到），坐标多为 `<root>`。
    #[error("io error at {location}: {source}")]
    Io {
        location: Location,
        source: std::io::Error,
    },

    /// YAML 解析错误，坐标多为 `<root>`。
    #[error("yaml parse error at {location}: {source}")]
    Yaml {
        location: Location,
        source: serde_yaml::Error,
    },

    /// 结构性 / 语义不合法（缺必填、重复 id 等）。
    #[error("invalid config at {location}: {message}")]
    Invalid { location: Location, message: String },

    /// 单个取值不合法（格式错、越界等）。
    #[error("invalid value at {location}: {message}")]
    Value { location: Location, message: String },

    /// 过滤器名未在注册表中注册。
    #[error("unknown filter {name} at {location}")]
    UnknownFilter { location: Location, name: String },

    /// 谓词名未在注册表中注册。
    #[error("unknown predicate {name} at {location}")]
    UnknownPredicate { location: Location, name: String },

    /// 把配置编译成运行时结构（过滤链、快照等）失败。
    #[error("compile failed at {location}: {message}")]
    Compile { location: Location, message: String },
}

impl ConfigError {
    /// 构造 [`ConfigError::Invalid`]：`location` 处结构性 / 语义不合法。
    pub fn invalid(location: Location, message: impl Into<String>) -> Self {
        Self::Invalid {
            location,
            message: message.into(),
        }
    }

    /// 构造 [`ConfigError::Value`]：`location` 处取值不合法。
    pub fn value(location: Location, message: impl Into<String>) -> Self {
        Self::Value {
            location,
            message: message.into(),
        }
    }

    /// 构造 [`ConfigError::Compile`]：`location` 处编译失败。
    pub fn compile(location: Location, message: impl Into<String>) -> Self {
        Self::Compile {
            location,
            message: message.into(),
        }
    }

    /// 构造带坐标的 I/O 失败（配置文件读不到等）。
    pub fn io(location: Location, source: std::io::Error) -> Self {
        Self::Io { location, source }
    }

    /// 构造带坐标的 YAML 解析失败。
    pub fn yaml(location: Location, source: serde_yaml::Error) -> Self {
        Self::Yaml { location, source }
    }
}

/// 整文件级的失败经 `From` 转换时落在 [`Location::root()`]；已知更精确
/// 坐标的调用方改用 [`ConfigError::io`] / [`ConfigError::yaml`]，或让
/// `validate::relocate` 替换位置。
impl From<std::io::Error> for ConfigError {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            location: Location::root(),
            source,
        }
    }
}

impl From<serde_yaml::Error> for ConfigError {
    fn from(source: serde_yaml::Error) -> Self {
        Self::Yaml {
            location: Location::root(),
            source,
        }
    }
}
