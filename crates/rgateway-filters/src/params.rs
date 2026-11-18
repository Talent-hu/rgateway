//! [`FilterSpec`] 参数取值小助手：带定位的类型化读取 + 范围校验。
//!
//! 复用 [`FilterSpec::param_*`] 访问器的「缺失 vs 配错」区分，在其上补
//! 过滤器共用的范围规则（正数、`0..=1` 区间）。所有错误以过滤器的
//! `source` 为 [`Location`] 并点名参数，由编译管线改写到配置条目坐标。

use std::num::NonZeroU32;
use std::time::Duration;

use rgateway_config::{ConfigError, FilterSpec, Location};

/// 「值存在但范围不对」错误：定位为本过滤器的 `source`。
pub(crate) fn value_error(spec: &FilterSpec, message: String) -> ConfigError {
    ConfigError::value(Location::section(&spec.source), message)
}

/// 可选 `u32` 正数参数（`1..=u32::MAX`）。
///
/// 缺失返回 `Ok(None)`；数值为 0、超出 u32 范围或不是整数时报错并点名
/// 参数（整数解析失败复用访问器的既有文案）。
pub(crate) fn positive_u32(spec: &FilterSpec, key: &str) -> Result<Option<u32>, ConfigError> {
    match spec.param_u64_opt(key)? {
        None => Ok(None),
        Some(raw) => NonZeroU32::new(u32::try_from(raw).unwrap_or(0))
            .map(|n| Some(n.get()))
            .ok_or_else(|| {
                value_error(
                    spec,
                    format!("parameter {key:?}: must be within 1..={}", u32::MAX),
                )
            }),
    }
}

/// 可选 `usize` 参数（不做范围约束，0 由调用方决定是否合法）。
pub(crate) fn opt_usize(spec: &FilterSpec, key: &str) -> Result<Option<usize>, ConfigError> {
    match spec.param_u64_opt(key)? {
        None => Ok(None),
        Some(raw) => usize::try_from(raw)
            .map(Some)
            .map_err(|_| value_error(spec, format!("parameter {key:?}: out of range"))),
    }
}

/// 可选 `usize` 正数参数（必须 > 0）。
pub(crate) fn positive_usize(spec: &FilterSpec, key: &str) -> Result<Option<usize>, ConfigError> {
    match opt_usize(spec, key)? {
        None => Ok(None),
        Some(n) if n > 0 => Ok(Some(n)),
        Some(_) => Err(value_error(spec, format!("parameter {key:?}: must be > 0"))),
    }
}

/// 可选区间浮点参数（`0..=1`，字符串 `"50%"` 经访问器折算为 0.5）。
pub(crate) fn unit_f64(spec: &FilterSpec, key: &str) -> Result<Option<f64>, ConfigError> {
    match spec.param_f64_opt(key)? {
        None => Ok(None),
        Some(rate) if (0.0..=1.0).contains(&rate) => Ok(Some(rate)),
        Some(rate) => Err(value_error(
            spec,
            format!("parameter {key:?}: must be within 0..=1, got {rate}"),
        )),
    }
}

/// 可选正时长参数（必须 > 0：零时长视为配置笔误快速失败）。
pub(crate) fn positive_duration(
    spec: &FilterSpec,
    key: &str,
) -> Result<Option<Duration>, ConfigError> {
    match spec.param_duration_opt(key)? {
        None => Ok(None),
        Some(dur) if !dur.is_zero() => Ok(Some(dur)),
        Some(_) => Err(value_error(spec, format!("parameter {key:?}: must be > 0"))),
    }
}
