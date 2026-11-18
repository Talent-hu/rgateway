//! [`FilterSpec`] 的 `param_*` 参数访问器族。
//!
//! 从 `spec.rs` 拆出（`spec.rs` 已接近 500 行硬限）：本文件只放参数
//! 读取/转换逻辑，spec 的解析与归一化留在 `spec.rs`。规则详见
//! [`FilterSpec`] 的「参数访问器族」一节。

use std::collections::BTreeMap;
use std::time::Duration;

use super::FilterSpec;
use crate::dsl;
use crate::error::{ConfigError, Location};
use crate::schema::ParamValue;

impl FilterSpec {
    /// 参数是否存在（不校验类型）。
    pub fn has(&self, key: &str) -> bool {
        self.params.contains_key(key)
    }

    /// 必填字符串参数。只接受字符串值——紧凑 DSL 的值都是字符串，而
    /// 结构化 YAML 里的 number 不会被强转成字符串。
    ///
    /// 错误：参数缺失或不是字符串时返回 [`ConfigError`]。
    pub fn param_str(&self, key: &str) -> Result<&str, ConfigError> {
        self.params
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| self.missing(key, "string"))
    }

    /// 可选字符串参数：缺失返回 `Ok(None)`；有值但不是字符串时返回
    /// [`ConfigError`]（类型错误不会被静默忽略）。
    pub fn param_str_opt(&self, key: &str) -> Result<Option<&str>, ConfigError> {
        match self.params.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(Some)
                .ok_or_else(|| self.missing(key, "string")),
        }
    }

    /// 必填无符号整数参数：接受 YAML number 或可解析为 u64 的字符串。
    ///
    /// 错误：参数缺失、不是整数或数值超出 u64 范围时返回 [`ConfigError`]。
    pub fn param_u64(&self, key: &str) -> Result<u64, ConfigError> {
        self.param_u64_opt(key)?
            .ok_or_else(|| self.missing(key, "u64"))
    }

    /// 可选无符号整数参数，语义同 [`Self::param_u64`]，缺失返回 `Ok(None)`。
    pub fn param_u64_opt(&self, key: &str) -> Result<Option<u64>, ConfigError> {
        match self.params.get(key) {
            None => Ok(None),
            Some(v) => {
                if let Some(n) = v.as_u64() {
                    return Ok(Some(n));
                }
                // 紧凑 DSL 的值全是字符串（"rate:100"），需回退到字符串解析
                if let Some(s) = v.as_str() {
                    if let Ok(n) = s.trim().parse::<u64>() {
                        return Ok(Some(n));
                    }
                }
                Err(self.missing(key, "u64"))
            }
        }
    }

    /// 必填浮点参数：接受 YAML number 或字符串；字符串可带 `%` 后缀。
    ///
    /// 错误：参数缺失或无法解析为数字时返回 [`ConfigError`]。
    pub fn param_f64(&self, key: &str) -> Result<f64, ConfigError> {
        self.param_f64_opt(key)?
            .ok_or_else(|| self.missing(key, "number"))
    }

    /// 可选浮点参数。字符串形态的 `%` 语义：`"50%"` → 0.5（除以 100），
    /// `"50"` → 50.0——百分比语义由调用方决定，访问器只做换算。
    pub fn param_f64_opt(&self, key: &str) -> Result<Option<f64>, ConfigError> {
        match self.params.get(key) {
            None => Ok(None),
            Some(v) => {
                if let Some(f) = v.as_f64() {
                    return Ok(Some(f));
                }
                if let Some(s) = v.as_str() {
                    let trimmed = s.trim_end_matches('%');
                    if let Ok(f) = trimmed.parse::<f64>() {
                        // "50%" → 0.5, "50" → 50.0（是否百分比由调用方决定）
                        return Ok(Some(if s.ends_with('%') { f / 100.0 } else { f }));
                    }
                }
                Err(self.missing(key, "number"))
            }
        }
    }

    /// 可选布尔参数：接受 YAML bool 或字符串 `"true"/"yes"/"on"`、
    /// `"false"/"no"/"off"`（大小写不敏感）。没有必填变体。
    ///
    /// 错误：有值但两种形态都解析不了时返回 [`ConfigError`]。
    pub fn param_bool_opt(&self, key: &str) -> Result<Option<bool>, ConfigError> {
        match self.params.get(key) {
            None => Ok(None),
            Some(v) => {
                if let Some(b) = v.as_bool() {
                    return Ok(Some(b));
                }
                if let Some(s) = v.as_str() {
                    match s.trim().to_ascii_lowercase().as_str() {
                        "true" | "yes" | "on" => return Ok(Some(true)),
                        "false" | "no" | "off" => return Ok(Some(false)),
                        _ => {}
                    }
                }
                Err(self.missing(key, "bool"))
            }
        }
    }

    /// 必填时长参数：number 按毫秒；字符串带 `ms/us/s/m/h/d` 后缀或
    /// 纯数字毫秒（语法详见 `dsl::parse_duration`）。
    ///
    /// 错误：参数缺失、不是 number/字符串或时长文本非法（负数、NaN、溢出）时返回 [`ConfigError`]。
    pub fn param_duration(&self, key: &str) -> Result<Duration, ConfigError> {
        self.param_duration_opt(key)?
            .ok_or_else(|| self.missing(key, "duration"))
    }

    /// 可选时长参数，语义同 [`Self::param_duration`]，缺失返回 `Ok(None)`。
    pub fn param_duration_opt(&self, key: &str) -> Result<Option<Duration>, ConfigError> {
        match self.params.get(key) {
            None => Ok(None),
            Some(v) => {
                let dur = match v {
                    ParamValue::Number(n) => Duration::from_millis(
                        n.as_u64().ok_or_else(|| self.missing(key, "duration"))?,
                    ),
                    ParamValue::String(s) => {
                        dsl::parse_duration(s).map_err(|m| self.bad(key, m))?
                    }
                    _ => return Err(self.missing(key, "duration")),
                };
                Ok(Some(dur))
            }
        }
    }

    /// 必填大小参数：number 按字节；字符串带 `B/KB/MB/GB` 后缀
    /// （1024 进制，语法详见 `dsl::parse_size`）。
    ///
    /// 错误：参数缺失、不是 number/字符串或大小文本非法时返回 [`ConfigError`]。
    pub fn param_size(&self, key: &str) -> Result<usize, ConfigError> {
        self.param_size_opt(key)?
            .ok_or_else(|| self.missing(key, "size"))
    }

    /// 可选大小参数，语义同 [`Self::param_size`]，缺失返回 `Ok(None)`。
    pub fn param_size_opt(&self, key: &str) -> Result<Option<usize>, ConfigError> {
        match self.params.get(key) {
            None => Ok(None),
            Some(v) => {
                let size = match v {
                    ParamValue::Number(n) => {
                        n.as_u64().ok_or_else(|| self.missing(key, "size"))? as usize
                    }
                    ParamValue::String(s) => dsl::parse_size(s).map_err(|m| self.bad(key, m))?,
                    _ => return Err(self.missing(key, "size")),
                };
                Ok(Some(size))
            }
        }
    }

    /// 字符串列表参数（接受 YAML sequence）。**缺失返回空 Vec**，把
    /// 「没配」视作空表而非错误。
    ///
    /// 错误：有值但不是序列、或元素不是字符串时返回 [`ConfigError`]。
    pub fn param_str_list(&self, key: &str) -> Result<Vec<String>, ConfigError> {
        match self.params.get(key) {
            None => Ok(vec![]),
            Some(ParamValue::Sequence(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| self.missing(key, "string list"))
                })
                .collect(),
            Some(_) => Err(self.missing(key, "string list")),
        }
    }

    /// u64 列表参数（HTTP status 等）。缺失返回空 Vec；元素必须是
    /// YAML number（字符串数字不做回退解析）。
    ///
    /// 错误：有值但不是序列、或元素不是 number 时返回 [`ConfigError`]。
    pub fn param_u64_list(&self, key: &str) -> Result<Vec<u64>, ConfigError> {
        match self.params.get(key) {
            None => Ok(vec![]),
            Some(ParamValue::Sequence(items)) => items
                .iter()
                .map(|v| v.as_u64().ok_or_else(|| self.missing(key, "u64 list")))
                .collect(),
            Some(_) => Err(self.missing(key, "u64 list")),
        }
    }

    /// 嵌套 map 参数（如 `backoff: {initial: 50ms, max: 2s}`）。
    /// 返回拥有所有权的 map（key 强制为字符串）；子键可再包装成临时
    /// `FilterSpec` 复用本族访问器解析（见 validate 对 `backoff` 的处理）。
    ///
    /// 错误：参数缺失、不是 mapping 或 key 不是字符串时返回 [`ConfigError`]。
    pub fn param_map(&self, key: &str) -> Result<BTreeMap<String, ParamValue>, ConfigError> {
        let value = self
            .params
            .get(key)
            .ok_or_else(|| self.missing(key, "map"))?;
        let mapping = value.as_mapping().ok_or_else(|| self.missing(key, "map"))?;
        let mut out = BTreeMap::new();
        for (k, v) in mapping {
            let k = k
                .as_str()
                .ok_or_else(|| self.missing(key, "map with string keys"))?
                .to_string();
            out.insert(k, v.clone());
        }
        Ok(out)
    }

    /// 「缺失或类型不符」错误：定位为本过滤器的 `source` 文本，
    /// 消息点名参数与期望类型。
    fn missing(&self, key: &str, want: &str) -> ConfigError {
        ConfigError::value(
            Location::section(&self.source),
            format!("parameter {key:?} missing or not a {want}"),
        )
    }

    /// 「值存在但解析失败」错误：定位同 [`Self::missing`]，
    /// 消息附带底层解析器的具体原因。
    fn bad(&self, key: &str, message: String) -> ConfigError {
        ConfigError::value(
            Location::section(&self.source),
            format!("parameter {key:?}: {message}"),
        )
    }
}

/// 把「裸 IP 或 CIDR」文本解析为 [`ipnet::IpNet`]；裸 IP 归一化为单主机
/// 网段（`/32`、`/128`）。非法返回 `None`。
///
/// validate（格式把关）与 IpFilter 工厂（运行时判定）共用此函数，
/// 保证两处对「合法条目」的判据不会漂移。
pub fn parse_ip_net(text: &str) -> Option<ipnet::IpNet> {
    text.parse::<ipnet::IpNet>().ok().or_else(|| {
        text.parse::<std::net::IpAddr>()
            .ok()
            .map(ipnet::IpNet::from)
    })
}
