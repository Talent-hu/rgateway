//! 目标选择层（target select）：为**一次尝试**（attempt）挑选上游实例。
//!
//! # 职责
//!
//! * `lb://name` 路由：向 [`ServiceDiscovery`] 取实例列表，过滤出健康、
//!   在轮转中（`weight > 0`）、未被熔断跳过的候选，再交给路由自己的
//!   [`LoadBalancer`] 挑一个。
//! * `http(s)://host[:port]` 直连路由：把固定的 authority 解析成一个
//!   socket 地址。
//!
//! 选中的 [`UpstreamTarget`] 每次尝试写入请求上下文一次；重试会**重新
//! 选择并覆写**上一次的结果（见 [`TargetSelector::wrap`]）。
//!
//! # 设计要点（为什么这样设计）
//!
//! * **过滤顺序：先 healthy / weight，再熔断，最后才轮到 LB。**
//!   熔断过滤发生在 [`LoadBalancer::pick`] 之前，LB 只在真正可用的
//!   集合里做均衡——每次重试都要跳过熔断打开的实例，而不是让 LB 选中
//!   再被链上的 breaker 层拒绝、白白浪费一次尝试。
//! * **probe-due / half-open 的实例保持可选。** 熔断处于 Open 但 open
//!   窗口已过的实例会被放回候选集：链的 breaker 层要靠后续请求发恢复
//!   探测（半开探测）。若在选目标时一直跳过它们，探测永远发不出去，
//!   熔断器就永远无法恢复。
//! * **错误分类**（安全修复后的语义）：无任何「健康且在轮转中」的实例
//!   （空列表 / 全不健康 / `weight == 0` 的 out-of-rotation）→
//!   [`GatewayError::Unavailable`]；有健康在轮转的候选但全部被「仍在
//!   open 窗口内」的熔断跳过 → [`GatewayError::CircuitOpen`]；LB 的
//!   `pick` 返回 `None` → [`GatewayError::Unavailable`]。`weight == 0`
//!   是运维**主动摘除流量**（out-of-rotation），不代表后端故障，因此
//!   归入 `Unavailable` 而不是 `CircuitOpen`。
//! * **直连 URI 的 DNS 解析不阻塞 async 运行时。** 同步入口
//!   [`TargetSelector::select`] 会阻塞在 DNS 上（仅供测试/编译期用）；
//!   请求路径走 [`select_async`]，把解析扔进 `tokio::task::spawn_blocking`
//!   阻塞线程池，慢解析不会占死 tokio worker。

use std::net::ToSocketAddrs;
use std::sync::Arc;

use http::Request;
use rgateway_core::{
    BoxedService, GatewayError, Instance, LoadBalancer, ProxyBody, RequestCtx, RouteState,
    RouteUri, ServiceDiscovery, UpstreamTarget,
};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

/// 每条路由的目标选择策略（编译期构建一次，运行期每请求复用）。
///
/// * `uri` —— 路由目标形式：`lb://服务名` 或直连 `http(s)://host[:port]`。
/// * `discovery` —— 服务发现，`lb://` 路由的实例来源。
/// * `balancer` —— 路由自己的负载均衡器（如 RoundRobin），只在可用
///   候选集上挑选。
/// * `state` —— 路由的治理状态，按实例 id 存熔断器。
#[derive(Clone)]
pub struct TargetSelector {
    pub uri: RouteUri,
    pub discovery: Arc<dyn ServiceDiscovery>,
    pub balancer: Arc<dyn LoadBalancer>,
    pub state: Arc<RouteState>,
}

impl TargetSelector {
    /// 为一次尝试挑选上游目标。
    ///
    /// * `lb://name` —— 候选须健康且在轮转中（`weight > 0`）；熔断处于
    ///   Open **且仍在 open 窗口内**的实例被跳过（probe-due 与 half-open
    ///   的实例保持可选，让链的 breaker 层能跑恢复探测）。可用集非空时
    ///   由 [`LoadBalancer`] 挑一个（`pick` 返回 `None` 视为
    ///   `Unavailable`）；存在健康在轮转的候选但全被熔断跳过 →
    ///   `CircuitOpen`；完全没有健康在轮转的实例（空 / 不健康 /
    ///   out-of-rotation）→ `Unavailable`。
    /// * `http(s)://host[:port]` —— authority 解析成一个 socket 地址
    ///   （端口缺省按 scheme 取 80/443）；解析失败为 `Unavailable`。
    ///   实例 id 即 authority 字符串。
    ///
    /// 这是**同步**入口：直连 URI 会阻塞在 DNS 上。请求路径不直接调它，
    /// 而是走 [`TargetSelector::wrap`] → [`select_async`]，把 DNS 解析
    /// 卸载到阻塞线程池（见 [`select_async`] 的说明）。
    ///
    /// # 错误
    ///
    /// [`GatewayError::Unavailable`]（无可用实例 / DNS 解析失败）或
    /// [`GatewayError::CircuitOpen`]（候选全被熔断跳过）。
    pub fn select(&self, ctx: &RequestCtx) -> Result<UpstreamTarget, GatewayError> {
        match &self.uri {
            RouteUri::Lb(name) => self.select_lb(name, ctx),
            RouteUri::Direct(uri) => select_direct(uri),
        }
    }

    /// 包装内层服务：先选上游目标、写入 `ctx.target`（**覆写**上一次
    /// 尝试的选择，使重试能重跑选目标），再委托给 `inner`。
    ///
    /// 选择走 [`select_async`]：直连 URI 的 DNS 解析绝不会阻塞 async
    /// 运行时的 worker。请求上下文从 request extensions 取出
    /// （[`request_ctx`]）；缺失视为内部错误（分发层必然注入，缺失即
    /// bug）。
    pub fn wrap(&self, inner: BoxedService) -> BoxedService {
        let this = Arc::new(self.clone());
        BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
            let this = Arc::clone(&this);
            let inner = inner.clone();
            async move {
                let ctx = request_ctx(&req)
                    .ok_or_else(|| GatewayError::Internal("missing request ctx".into()))?;
                let target = select_async(&this, &ctx).await?;
                ctx.set_target(target);
                inner.oneshot(req).await
            }
        }))
    }

    /// `lb://` 路径的候选过滤 + 负载均衡（纯内存操作，可在 async 上下文
    /// 内联执行）。
    fn select_lb(&self, name: &str, ctx: &RequestCtx) -> Result<UpstreamTarget, GatewayError> {
        let instances = self.discovery.get(name);
        // 第一级过滤：只留「健康 + 在轮转中」（weight > 0）。
        // weight == 0 是 out-of-rotation，安全修复后与不健康同级处理。
        let candidates: Vec<Instance> = instances
            .iter()
            .filter(|i| i.healthy && i.weight > 0)
            .cloned()
            .collect();
        // 第二级过滤：跳过熔断仍处于 open 窗口内的实例。
        // breaker_open() 在 open 窗口过后返回 false（probe-due），
        // 于是实例回到可用集，交给链的 breaker 层做半开恢复探测。
        let usable: Vec<Instance> = candidates
            .iter()
            .filter(|i| !self.state.breaker_open(&i.id))
            .cloned()
            .collect();
        if usable.is_empty() {
            return if candidates.is_empty() {
                // 无健康在轮转实例（空 / 不健康 / weight==0）→ Unavailable
                Err(GatewayError::Unavailable(format!(
                    "no available instances for {name}"
                )))
            } else {
                // 有健康在轮转的候选，但全被「仍在 open 窗口内」的熔断跳过
                // → CircuitOpen（区别于 Unavailable：后端还在，只是被熔断挡住）
                Err(GatewayError::CircuitOpen(format!(
                    "all instances open for {name}"
                )))
            };
        }
        // 最后才轮到 LB：只在可用集上均衡；pick 返回 None 视为瞬时不可用。
        let picked = self.balancer.pick(&usable, ctx).ok_or_else(|| {
            GatewayError::Unavailable(format!("no available instances for {name}"))
        })?;
        Ok(UpstreamTarget {
            instance_id: picked.id,
            addr: picked.addr,
        })
    }
}

/// [`TargetSelector::wrap`] 的每次尝试入口：`lb://` 选目标是纯内存工作、
/// 内联执行；直连 URI 的 DNS 解析放到阻塞线程池
/// （`tokio::task::spawn_blocking`），慢解析不会拖住 async 运行时的
/// worker。
async fn select_async(
    selector: &TargetSelector,
    ctx: &RequestCtx,
) -> Result<UpstreamTarget, GatewayError> {
    match &selector.uri {
        RouteUri::Lb(name) => selector.select_lb(name, ctx),
        RouteUri::Direct(uri) => {
            let uri = uri.clone();
            tokio::task::spawn_blocking(move || select_direct(&uri))
                .await
                .map_err(|e| {
                    GatewayError::Internal(format!("target resolution task failed: {e}"))
                })?
        }
    }
}

/// 把直连 `http(s)://host[:port]` URI 解析成一个 [`UpstreamTarget`]。
///
/// 取 authority 的 host + port（端口缺省按 scheme：https→443，其余→80），
/// 解析出第一个 socket 地址。实例 id 直接用 authority 字符串（无服务发现，
/// 稳定且可读）。解析失败（无 authority / DNS 失败 / 结果为空）均为
/// `Unavailable`。此函数阻塞在 DNS 上，只应经 [`select_async`] 调用。
fn select_direct(uri: &http::Uri) -> Result<UpstreamTarget, GatewayError> {
    let authority = uri
        .authority()
        .ok_or_else(|| GatewayError::Unavailable(format!("direct uri has no authority: {uri}")))?;
    let host = authority.host();
    let port = authority
        .port_u16()
        .unwrap_or_else(|| match uri.scheme_str() {
            Some("https") => 443,
            _ => 80,
        });
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| GatewayError::Unavailable(format!("cannot resolve {host}:{port}: {e}")))?;
    let addr = addrs
        .next()
        .ok_or_else(|| GatewayError::Unavailable(format!("cannot resolve {host}:{port}")))?;
    Ok(UpstreamTarget {
        instance_id: Arc::from(authority.as_str()),
        addr,
    })
}

/// 从 request extensions 取出注入的 `Arc<RequestCtx>`（测试与编译期的
/// 便捷提取器；`None` 表示分发层未注入上下文）。
pub fn request_ctx<B>(req: &Request<B>) -> Option<Arc<RequestCtx>> {
    req.extensions().get::<Arc<RequestCtx>>().cloned()
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
