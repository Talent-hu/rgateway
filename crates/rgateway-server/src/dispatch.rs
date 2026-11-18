//! 请求分发：并发闸 → 请求体限长 → 路由匹配 → 过滤器链执行 →
//! 协议感知错误渲染 + 可观测性收尾（访问日志 / 指标 / 并发 gauge）。
//!
//! 设计取舍（doc 注释写明，便于后续阶段接手）：
//!
//! * **并发闸用信号量 `try_acquire_owned`**：超限直接渲染
//!   [`GatewayError::RateLimited`]（429 / grpc-status 8），不让请求排队积压
//!   ——排队会把过载延迟传染给全部在途请求，快速失败对双方都更可控。
//!   许可的生命周期**跟随响应体**（[`PermitBody`]）：响应体耗尽或被 drop
//!   才释放——若响应头一返回就释放，流式响应可以在「已计入并发」的假象下
//!   无界突破 `max_concurrent_requests`。
//! * **`max_concurrent_requests` 的信号量在启动时按初始快照定容**：热重载
//!   修改该值在 P3 不动态生效（遗留项，见 [`Dispatcher::new`]）。
//! * **Content-Length 双保险**：先做头预检（超限直接拒），再用 body 流
//!   `Limited` 限长（防谎报/缺失 Content-Length 的超大体）。两道闸的错误
//!   都归一为 [`GatewayError::InvalidArgument`]（400 / grpc-status 3）。
//!   请求体错误分类（[`classify_body_error`]）：客户端请求体的错误一律归
//!   `InvalidArgument`，绝不归 `Unavailable`——客户端自身故障被渲染成 503
//!   是误导；只有显式内嵌的 [`ProxyError::Gateway`] 原样透传。
//! * **渲染协议 = 最终协议**（含路由 hint 覆写后）；无路由的 NotFound 用
//!   探测协议——此时还没有路由 hint 可用。
//! * **访问日志每个请求一行**（含 404 与闸门拒绝）：访问日志的职责是记录
//!   全部到达流量；JSON/text 渲染与指标计量口径收在 [`crate::access`]，
//!   本模块只负责把请求事实交出去。
//! * **请求 span 全程包裹**（[`rgateway_obs::request_span`]）：从进入
//!   [`Dispatcher::serve`] 到响应收尾，访问日志行落在 span 上下文里，
//!   与追踪管道按 `request_id` 关联。

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{request::Parts, Method, Request, Response};
use http_body::Frame;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use tokio::sync::OwnedSemaphorePermit;
use tracing::Instrument;

use rgateway_config::{CompiledConfig, SnapshotStore};
use rgateway_core::{
    boxed_body, ErrorKind, GatewayError, Protocol, ProxyBody, ProxyError, RequestCtx,
};
use rgateway_proxy::{error_render, headers};
use tower::ServiceExt;

use crate::{access, protocol};

/// 分发器的并发与体积上限（启动时从初始快照取值）。
pub struct DispatcherLimits {
    /// 单请求体字节上限：Content-Length 预检与 body 流限长共用。
    pub max_request_body: usize,
    /// 全局并发请求信号量，容量 = `server.limits.max_concurrent_requests`。
    ///
    /// 用 `Arc` 是为了 `try_acquire_owned`：许可以 [`OwnedSemaphorePermit`]
    /// 的形式被响应体包装层（[`PermitBody`]）持有，生命周期与响应体一致。
    pub semaphore: Arc<tokio::sync::Semaphore>,
}

/// 请求分发器：持有快照存储与并发/体积上限，逐请求执行完整分发流程。
pub struct Dispatcher {
    store: Arc<SnapshotStore>,
    limits: DispatcherLimits,
    /// 数据面监听的接入 scheme（TLS 与否），逐请求写入 [`RequestCtx`]
    /// 供 `X-Forwarded-Proto` 覆写；启动后固定（监听形态运行期不变）。
    client_scheme: rgateway_core::ClientScheme,
}

impl Dispatcher {
    /// 以初始快照构造分发器；limits 取自快照的 `server.limits`。
    ///
    /// 设计取舍：`max_concurrent_requests` 的信号量在此**一次性定容**，
    /// 热重载改该值 P3 不动态生效（遗留项）——换容量需要排空在途许可，
    /// 属后续阶段的动态治理能力。
    pub fn new(store: Arc<SnapshotStore>, client_scheme: rgateway_core::ClientScheme) -> Self {
        let snapshot = store.load();
        let limits = DispatcherLimits {
            max_request_body: snapshot.server.max_request_body,
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                snapshot.server.max_concurrent_requests as usize,
            )),
        };
        Self {
            store,
            limits,
            client_scheme,
        }
    }

    /// 处理一次请求，总是返回响应（错误也渲染成响应，绝不向调用方抛错）。
    ///
    /// 流程：协议探测 → 请求 span → 并发闸 → Content-Length 预检 →
    /// body 流限长 → 路由匹配（无命中按探测协议渲染 NotFound）→
    /// hint 覆写解析 → 过滤器链执行（链上错误按最终协议渲染，`Display`
    /// 只进日志）→ 响应收尾（`X-Request-ID` 覆写 + 访问日志 + 指标）。
    pub async fn serve(
        &self,
        req: Request<ProxyBody>,
        peer: Option<SocketAddr>,
    ) -> Response<ProxyBody> {
        let (parts, body) = req.into_parts();
        let detected = protocol::detect(&parts);
        let mut ctx = RequestCtx::new(detected, peer);
        ctx.client_scheme = self.client_scheme;
        let method = parts.method.clone();
        let path = parts.uri.path().to_string();
        let span = rgateway_obs::request_span(
            &ctx.request_id.to_string(),
            method.as_str(),
            &path,
            ctx.protocol.as_label(),
        );
        self.serve_inner(body, parts, ctx, method, path, peer)
            .instrument(span)
            .await
    }

    /// [`Self::serve`] 的主体（span 之外的部分）：闸门、链执行与收尾。
    #[allow(clippy::too_many_arguments)]
    async fn serve_inner(
        &self,
        body: ProxyBody,
        parts: Parts,
        ctx: RequestCtx,
        method: Method,
        path: String,
        peer: Option<SocketAddr>,
    ) -> Response<ProxyBody> {
        let detected = ctx.protocol;
        let snapshot = self.store.load();
        // gauge 守卫随响应体收尾（与 permit 同生命周期），保证严格配对。
        let gauge = Some(access::GaugeGuard::new());
        // 并发闸：permit 跨 await 安全（OwnedSemaphorePermit 是 Send），最终
        // 移交响应体包装层（PermitBody）持有，响应体耗尽或 drop 才释放。
        let permit = match Arc::clone(&self.limits.semaphore).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                let err = GatewayError::RateLimited("too many concurrent requests".to_string());
                let resp = error_render::render(detected, &err);
                let resp = finish(&snapshot, &ctx, &method, &path, resp, Some(err.kind()));
                return attach_body_guards(resp, None, gauge);
            }
        };
        if let Some(err) = content_length_error(&parts, self.limits.max_request_body) {
            let resp = error_render::render(detected, &err);
            let resp = finish(&snapshot, &ctx, &method, &path, resp, Some(err.kind()));
            return attach_body_guards(resp, permit, gauge);
        }
        let body = limited_body(body, self.limits.max_request_body);
        let (response, ctx, error) = self
            .run_chain(&snapshot, parts, body, ctx, detected, peer)
            .await;
        let resp = finish(&snapshot, &ctx, &method, &path, response, error);
        attach_body_guards(resp, permit, gauge)
    }

    /// 「路由匹配 → hint 解析 → 链执行」：返回最终响应、最终 ctx 与
    /// 错误分类（收尾计量与访问日志用；成功为 `None`）。
    ///
    /// 仅当 hint 覆写协议时重建 ctx：探测协议被路由强制改写后，上下文必须
    /// 与最终协议一致；重建保留 `request_id` 与起算时刻（见 [`rebuild_ctx`]），
    /// span / 访问日志 / `X-Request-ID` 同源不漂移。
    async fn run_chain(
        &self,
        snapshot: &CompiledConfig,
        parts: Parts,
        body: ProxyBody,
        mut ctx: RequestCtx,
        detected: Protocol,
        peer: Option<SocketAddr>,
    ) -> (Response<ProxyBody>, Arc<RequestCtx>, Option<ErrorKind>) {
        let Some(route) = snapshot.routes.find(&ctx, &parts) else {
            let err = GatewayError::NotFound(format!(
                "no route matched: {} {}",
                parts.method,
                parts.uri.path()
            ));
            // 日志通道记 Display（含 method/path 内部细节）；客户端只见泛化文案。
            tracing::warn!(request_id = %ctx.request_id, "no route matched: {err}");
            return (
                error_render::render(detected, &err),
                Arc::new(ctx),
                Some(err.kind()),
            );
        };
        let final_proto = protocol::resolve(route.protocol_hint, detected);
        if final_proto != detected {
            ctx = rebuild_ctx(&ctx, final_proto, peer);
        }
        ctx.route_id = Some(route.id.clone());
        let ctx = Arc::new(ctx);
        let (response, error) = match route.chain_clone() {
            Ok(chain) => match chain.oneshot(with_ctx(parts, body, &ctx)).await {
                Ok(response) => (response, None),
                Err(err) => {
                    tracing::error!(
                        request_id = %ctx.request_id,
                        route_id = %route.id,
                        "request failed: {err}"
                    );
                    (error_render::render(final_proto, &err), Some(err.kind()))
                }
            },
            // chain_clone 失败即链锁中毒，其错误本就是 Internal，直接渲染。
            Err(err) => (error_render::render(final_proto, &err), Some(err.kind())),
        };
        (response, ctx, error)
    }
}

/// hint 覆写协议时重建 ctx：**保留**关联 id 与起算时刻，只改写协议/对端。
///
/// `RequestCtx::new` 默认生成全新 `request_id` 与 `start`——直接拿来重建会
/// 让 span 与访问日志各记一个 id（关联断链）、duration 少计探测与匹配阶段。
/// 关联 id 的连续性比「一个 id 只出现一种协议」更有价值，故显式保留：
/// span 的 `protocol` 字段仍记探测协议，访问日志记最终协议，按 id 关联。
fn rebuild_ctx(ctx: &RequestCtx, protocol: Protocol, peer: Option<SocketAddr>) -> RequestCtx {
    let mut rebuilt = RequestCtx::new(protocol, peer);
    rebuilt.request_id = ctx.request_id;
    rebuilt.start = ctx.start;
    rebuilt
}

/// 重组请求并注入 `Arc<RequestCtx>` 扩展（链上各层经它取上下文）。
fn with_ctx(parts: Parts, body: ProxyBody, ctx: &Arc<RequestCtx>) -> Request<ProxyBody> {
    let mut req = Request::from_parts(parts, body);
    req.extensions_mut().insert(Arc::clone(ctx));
    req
}

/// Content-Length 预检：头存在且解析为 u64 超过上限时返回待渲染错误。
///
/// 解析不了的头值不在此拦截（hyper 传输层会拒收畸形头），这里只管超限。
fn content_length_error(parts: &Parts, max: usize) -> Option<GatewayError> {
    let raw = parts
        .headers
        .get(http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?;
    let len: u64 = raw.trim().parse().ok()?;
    if len > max as u64 {
        Some(GatewayError::InvalidArgument(format!(
            "request body exceeds {max} bytes"
        )))
    } else {
        None
    }
}

/// 把请求体限长到 `max` 字节，错误分类见 [`classify_body_error`]。
fn limited_body(body: ProxyBody, max: usize) -> ProxyBody {
    let limited = Limited::new(body, max).map_err(classify_body_error);
    boxed_body(limited)
}

/// 请求体（`Limited` 包装后）的错误分类：先把装箱错误 downcast 回原语义，
/// 再按来源归类——
///
/// * [`LengthLimitError`] → [`GatewayError::InvalidArgument`]（超限兜底）；
/// * [`ProxyError::Gateway`]（链上层显式内嵌的网关错误）→ 原样透传，
///   保持其错误种类与渲染形态；
/// * 其余（`Upstream`/`Io`/无法还原的装箱错误）→ `InvalidArgument`
///   （客户端请求体的故障就是客户端故障；渲染成 503 是误导）。
fn classify_body_error(e: Box<dyn std::error::Error + Send + Sync>) -> ProxyError {
    if e.downcast_ref::<LengthLimitError>().is_some() {
        return ProxyError::Gateway(GatewayError::InvalidArgument(
            "request body exceeds limit".to_string(),
        ));
    }
    match e.downcast::<ProxyError>() {
        Ok(pe) if matches!(*pe, ProxyError::Gateway(_)) => *pe,
        _ => ProxyError::Gateway(GatewayError::InvalidArgument(
            "request body error".to_string(),
        )),
    }
}

/// 响应体包装层：持有并发许可（[`OwnedSemaphorePermit`]）与并发 gauge
/// 守卫（[`access::GaugeGuard`]），直到响应体耗尽（`poll_frame` 取到流
/// 结束）或被 drop——流式响应未结束期间两者一直被占，`max_concurrent_requests`
/// 不会被无界突破，gauge 也不会漂移。
struct PermitBody {
    inner: ProxyBody,
    permit: Option<OwnedSemaphorePermit>,
    gauge: Option<access::GaugeGuard>,
}

impl http_body::Body for PermitBody {
    type Data = Bytes;
    type Error = ProxyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ProxyError>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(None) => {
                // 流已耗尽：立即释放许可与 gauge 守卫，不必等包装层被 drop。
                this.permit.take();
                this.gauge.take();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

/// 把并发许可与 gauge 守卫挂进响应体（见 [`PermitBody`]）；两者皆 `None`
/// 的路径原样返回。
fn attach_body_guards(
    response: Response<ProxyBody>,
    permit: Option<OwnedSemaphorePermit>,
    gauge: Option<access::GaugeGuard>,
) -> Response<ProxyBody> {
    if permit.is_none() && gauge.is_none() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let body = boxed_body(PermitBody {
        inner: body,
        permit,
        gauge,
    });
    Response::from_parts(parts, body)
}

/// 请求收尾：统一治理响应头（hop-by-hop 剥离 + `X-Request-ID` 覆写）→
/// 指标计量（口径见 [`access::record_metrics`]）→ 按快照开关输出一行
/// 访问日志（JSON/text，见 [`access::render_access`]）。
fn finish(
    snapshot: &CompiledConfig,
    ctx: &RequestCtx,
    method: &Method,
    path: &str,
    mut response: Response<ProxyBody>,
    error: Option<ErrorKind>,
) -> Response<ProxyBody> {
    let request_id = ctx.request_id.to_string();
    headers::finalize_response(response.headers_mut(), &request_id);
    let status = response.status().as_u16();
    access::record_metrics(
        ctx.route_id.as_deref(),
        method.as_str(),
        status,
        ctx.elapsed(),
        error,
    );
    if snapshot.access_log_enabled {
        let record = access::access_record(ctx, method.as_str(), path, status, error);
        let line = access::render_access(&record, snapshot.access_log_json);
        tracing::info!(target: "access", "{line}");
    }
    response
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
