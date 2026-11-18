//! ApiKeyAuth 过滤器：基于共享密钥的准入认证（槽位 `Auth`，准入类）。
//!
//! 参数：
//! - `keys`（必填 map）：`{ "<secret>": "<subject>", ... }`——密钥 → 身份
//!   名的内联映射。API Key 集合小（个位数~几十），内联随热重载自动生效，
//!   少一个秘密文件的路径包含/轮换语义面。
//! - `header`（可选字符串，默认 `x-api-key`）：携带密钥的请求头名。
//!
//! # 常数时间比较
//!
//! 持证密钥与配置密钥逐条做**常数时间字节比较**（[`ct_eq`]，遍历全部
//! 条目不提前退出）——不用 `BTreeMap::get`：有序比较的提前返回会让
//! 「命中前缀长度」通过响应时序外泄。比较的是字节，不要求头值是 UTF-8。
//!
//! # 认证结果
//!
//! 命中后以 [`AuthContext::set_auth`] 写入 `subject`（`kind: ApiKey`）。
//! set-once 语义：两个认证过滤器同时命中（双认证）按内部错误处理——
//! 一条路由配置两个互相竞争的认证器是配置错误，不静默放行。
//!
//! # 错误归类
//!
//! 缺失/不匹配一律 [`GatewayError::Unauthenticated`]（HTTP 401 / gRPC 16）；
//! 密钥值永不进错误文案与日志。

use std::sync::Arc;

use rgateway_config::{
    ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, Location, ServiceWrapper,
};
use rgateway_core::{AuthContext, AuthKind, BoxedService, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::util::ctx_of;

/// 默认的密钥承载头。
const DEFAULT_HEADER: &str = "x-api-key";

/// ApiKeyAuth 过滤器工厂（canonical 名 `"ApiKeyAuth"`，槽 [`rgateway_config::FilterSlot::Auth`]）。
pub struct ApiKeyAuthFactory;

impl FilterFactory for ApiKeyAuthFactory {
    fn name(&self) -> &'static str {
        "ApiKeyAuth"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let header_raw = spec.param_str_opt("header")?.unwrap_or(DEFAULT_HEADER);
        let header: http::HeaderName = header_raw.parse().map_err(|e| {
            ConfigError::value(
                Location::section(&spec.source),
                format!("ApiKeyAuth header {header_raw:?} is not a valid header name: {e}"),
            )
        })?;
        let keys = parse_keys(spec)?;
        let keys = Arc::new(keys);
        Ok(Box::new(move |inner: BoxedService| {
            let keys = Arc::clone(&keys);
            let header = header.clone();
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let inner = inner.clone();
                let keys = Arc::clone(&keys);
                let header = header.clone();
                async move {
                    let ctx = ctx_of(&req)?;
                    let presented = req.headers().get(&header).map(|v| v.as_bytes().to_vec());
                    let subject = match presented {
                        Some(bytes) => lookup(&bytes, &keys),
                        None => None,
                    };
                    let subject = subject.ok_or_else(|| {
                        GatewayError::Unauthenticated(format!(
                            "api key missing or invalid (header {header})"
                        ))
                    })?;
                    let auth = AuthContext {
                        subject: subject.to_string(),
                        kind: AuthKind::ApiKey,
                        claims: std::collections::HashMap::new(),
                    };
                    if ctx.set_auth(auth).is_err() {
                        // 双认证：两个认证过滤器都试图建立身份——配置错误，
                        // 不静默取其一。
                        return Err(GatewayError::Internal("auth already set".into()));
                    }
                    inner.oneshot(req).await
                }
            }))
        }))
    }
}

/// 解析 `keys` 参数为 `(secret, subject)` 列表；secret 与 subject 都必须
/// 是非空字符串。返回有序列表（map 的字典序），供常数时间遍历。
fn parse_keys(spec: &FilterSpec) -> Result<Vec<(String, String)>, ConfigError> {
    let map = spec.param_map("keys")?;
    if map.is_empty() {
        return Err(ConfigError::value(
            Location::section(&spec.source),
            "ApiKeyAuth keys must not be empty",
        ));
    }
    let mut out = Vec::with_capacity(map.len());
    for (secret, value) in map {
        if secret.is_empty() {
            return Err(ConfigError::value(
                Location::section(&spec.source),
                "ApiKeyAuth keys must not contain empty secrets",
            ));
        }
        let subject = value.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            ConfigError::value(
                Location::section(&spec.source),
                format!("ApiKeyAuth subject for key {secret:?} must not be empty"),
            )
        })?;
        out.push((secret, subject.to_string()));
    }
    Ok(out)
}

/// 在密钥表里查找持证密钥；命中返回 subject。
/// **遍历全部条目、不提前退出**——时序不随命中位置变化（见模块文档）。
fn lookup<'a>(presented: &[u8], keys: &'a [(String, String)]) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for (secret, subject) in keys {
        if ct_eq(presented, secret.as_bytes()) && found.is_none() {
            found = Some(subject.as_str());
        }
    }
    found
}

/// 常数时间字节比较：长度不同直接否（长度不是秘密内容），同长则逐字节
/// 累积异或，最后统一判零——比较耗时与内容匹配程度无关。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
#[path = "api_key_tests.rs"]
mod tests;
