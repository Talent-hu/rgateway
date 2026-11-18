//! IpFilter 过滤器：基于来源 IP 的准入控制（槽位 `Auth`，准入类）。
//!
//! 参数：`allow` / `deny`（字符串列表，元素是 IP 或 CIDR——裸 IP 自动
//! 归一为单主机网段）。两者至少一非空。
//!
//! # 判定语义：deny 优先
//!
//! 1. **deny 命中 → 拒绝**（403），优先于一切——白名单上叠临时黑名单
//!    是常见运维动作，「deny 优先」使 `allow: 全部 + deny: 单点` 直观可靠。
//! 2. allow 非空且未命中 → 拒绝。
//! 3. 其余放行。
//!
//! # 来源 IP：只看 `peer_addr`
//!
//! 取 socket 对端地址（TCP 连接的真实来源），**不读** `X-Forwarded-For`
//! ——该头可被客户端任意伪造，拿它做准入等于没有准入。peer 缺失
//! （非 TCP 传输/测试场景）：配了 allow 则 fail-closed 拒绝（无法证明
//! 在名单内），只配 deny 则放行（无法证明命中黑名单）。
//!
//! # 错误归类
//!
//! 拒绝映射 [`GatewayError::PermissionDenied`]（HTTP 403 / gRPC 7）；
//! 变体携带的细节（被拒 IP、判定原因）只进日志，客户端只见泛化的
//! "permission denied"。

use std::net::IpAddr;
use std::sync::Arc;

use ipnet::IpNet;
use rgateway_config::{
    parse_ip_net, ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, Location, ServiceWrapper,
};
use rgateway_core::{BoxedService, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::util::ctx_of;

/// IpFilter 过滤器工厂（canonical 名 `"IpFilter"`，槽 [`rgateway_config::FilterSlot::Auth`]）。
pub struct IpFilterFactory;

impl FilterFactory for IpFilterFactory {
    fn name(&self) -> &'static str {
        "IpFilter"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        _ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let allow = parse_list(spec, "allow")?;
        let deny = parse_list(spec, "deny")?;
        if allow.is_empty() && deny.is_empty() {
            return Err(ConfigError::value(
                Location::section(&spec.source),
                "IpFilter requires allow or deny (at least one non-empty)",
            ));
        }
        let allow = Arc::new(allow);
        let deny = Arc::new(deny);
        Ok(Box::new(move |inner: BoxedService| {
            let allow = Arc::clone(&allow);
            let deny = Arc::clone(&deny);
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let inner = inner.clone();
                let allow = Arc::clone(&allow);
                let deny = Arc::clone(&deny);
                async move {
                    let ctx = ctx_of(&req)?;
                    let ip = ctx.peer_addr.map(|a| a.ip());
                    if let Some(reason) = deny_reason(ip, &allow, &deny) {
                        return Err(GatewayError::PermissionDenied(reason));
                    }
                    inner.oneshot(req).await
                }
            }))
        }))
    }
}

/// 解析一个条目列表参数为 [`IpNet`] 集；非法条目报带定位的
/// [`ConfigError`]（消息含条目文本，方便运维对照改配置）。
fn parse_list(spec: &FilterSpec, key: &str) -> Result<Vec<IpNet>, ConfigError> {
    let entries = spec.param_str_list(key)?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in &entries {
        match parse_ip_net(entry) {
            Some(net) => out.push(net),
            None => {
                return Err(ConfigError::value(
                    Location::section(&spec.source),
                    format!("IpFilter {key} entry {entry:?} is not a valid IP or CIDR"),
                ));
            }
        }
    }
    Ok(out)
}

/// 判定是否拒绝；拒绝时给出内部日志用的原因（含 IP 等细节）。
/// `None` = 放行。语义见模块文档。
fn deny_reason(ip: Option<IpAddr>, allow: &[IpNet], deny: &[IpNet]) -> Option<String> {
    let ip = match ip {
        Some(ip) => ip,
        // peer 缺失：fail-closed 于 allow，fail-open 于 deny-only
        None => {
            return if allow.is_empty() {
                None
            } else {
                Some("missing peer address with allow list configured".into())
            };
        }
    };
    if deny.iter().any(|n| n.contains(&ip)) {
        return Some(format!("ip {ip} matched deny list"));
    }
    if !allow.is_empty() && !allow.iter().any(|n| n.contains(&ip)) {
        return Some(format!("ip {ip} not in allow list"));
    }
    None
}

#[cfg(test)]
#[path = "ip_filter_tests.rs"]
mod tests;
