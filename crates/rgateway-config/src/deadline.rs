//! 总截止期（total deadline）层：约束该路由链处理的每一个请求。
//!
//! # 契约：只约束响应头，不约束响应体
//!
//! 预算（budget）约束的是拿到响应 **headers** 的时间——覆盖整条内层链，
//! 包含所有重试（重试都发生在返回响应之前，所以它们共享同一个预算）；
//! 它**不**给响应体的流式传输限时：在预算内完成的调用原样透传，其响应体
//! 在截止期时刻之后继续流动。这背后的取舍是：响应体可能是长流（SSE、
//! 大文件下载），把它纳入总截止期会截断正常的长流；Deadline 承诺的是
//! 「多久拿到响应头」，流的生命周期由连接层/客户端管理。
//!
//! 超时的调用以 [`rgateway_core::GatewayError::DeadlineExceeded`] 失败
//! （gRPC status 4 / HTTP 504）。实现基于 `tokio::time`，测试可以用
//! `time::pause`/`advance` 精确驱动时钟。

use std::sync::Arc;
use std::time::Duration;

use rgateway_core::{BoxedService, GatewayError, ProxyBody, RequestCtx};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

/// Deadline 包装层，编译期每条路由构建一次。
///
/// * `budget` —— 获取响应头的总预算；超时映射为
///   [`GatewayError::DeadlineExceeded`]。
pub struct DeadlineLayer {
    pub budget: Duration,
}

impl DeadlineLayer {
    /// 构造一个预算为 `budget` 的截止期层。
    pub fn new(budget: Duration) -> Self {
        Self { budget }
    }

    /// 包装 `inner`：拿到响应头受构造时的预算约束。抢在截止期之前完成
    /// 的调用，其响应体原样返回并在之后继续流式传输（见模块文档的
    /// 「只约束响应头」契约）。
    ///
    /// 超时错误消息里带路由 id（取自请求上下文，缺失时为 `<unknown>`），
    /// 便于定位是哪条路由超时。
    pub fn wrap(&self, inner: BoxedService) -> BoxedService {
        let budget = self.budget;
        BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
            let inner = inner.clone();
            async move {
                let route_id = route_id_of(&req);
                match tokio::time::timeout(budget, inner.oneshot(req)).await {
                    Ok(result) => result,
                    Err(_elapsed) => Err(deadline_error(&route_id)),
                }
            }
        }))
    }
}

/// 把超时结果映射为网关标准错误：`DeadlineExceeded`，消息携带路由 id。
pub fn deadline_error(route_id: &str) -> GatewayError {
    GatewayError::DeadlineExceeded(format!("total deadline exceeded: {route_id}"))
}

/// 取请求命中的路由 id；上下文缺失或未设路由时回退为 `"<unknown>"`
/// （仅影响错误消息的可读性，不影响错误分类）。
fn route_id_of(req: &http::Request<ProxyBody>) -> Arc<str> {
    req.extensions()
        .get::<Arc<RequestCtx>>()
        .and_then(|ctx| ctx.route_id.clone())
        .unwrap_or_else(|| Arc::from("<unknown>"))
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http_body::{Body, Frame};
    use rgateway_core::{boxed_body, empty_body, ErrorKind, Protocol, ProxyError};

    use super::*;

    /// 造一个带指定路由 id 的请求（id 注入到 `Arc<RequestCtx>` 扩展里）。
    fn request(route_id: Option<&str>) -> http::Request<ProxyBody> {
        let mut ctx = RequestCtx::new(Protocol::Http1, None);
        ctx.route_id = route_id.map(Arc::from);
        let mut req = http::Request::new(empty_body());
        req.extensions_mut().insert(Arc::new(ctx));
        req
    }

    /// 内层服务 stub：延迟 `latency` 后返回 200 + 空 body。
    fn ok_after(latency: Duration) -> BoxedService {
        BoxCloneService::new(service_fn(
            move |_req: http::Request<ProxyBody>| async move {
                tokio::time::sleep(latency).await;
                Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
            },
        ))
    }

    /// 验证：内层慢于预算时超时，错误分类为 DeadlineExceeded
    /// （gRPC status 4 / HTTP 504），消息带路由 id。
    #[tokio::test(start_paused = true)]
    async fn slow_inner_exceeds_the_deadline() {
        let layer = DeadlineLayer::new(Duration::from_secs(2));
        let err = layer
            .wrap(ok_after(Duration::from_secs(60)))
            .oneshot(request(Some("user-http")))
            .await
            .expect_err("inner slower than the budget must time out");
        assert_eq!(err.kind(), ErrorKind::DeadlineExceeded);
        assert_eq!(err.grpc_status(), 4);
        assert_eq!(err.http_status(), http::StatusCode::GATEWAY_TIMEOUT);
        assert!(err.to_string().contains("user-http"), "{err}");
    }

    /// 验证：内层快于预算时原样透传，状态码 200 不被层改动。
    #[tokio::test(start_paused = true)]
    async fn fast_inner_passes_through_untouched() {
        let layer = DeadlineLayer::new(Duration::from_secs(2));
        let response = layer
            .wrap(ok_after(Duration::from_millis(1)))
            .oneshot(request(Some("user-http")))
            .await
            .expect("inner within budget must pass through");
        assert_eq!(response.status(), http::StatusCode::OK);
    }

    /// 验证：请求上下文没有路由 id 时，超时消息回退为 `<unknown>`。
    #[tokio::test(start_paused = true)]
    async fn missing_route_id_falls_back_to_unknown() {
        let layer = DeadlineLayer::new(Duration::from_secs(1));
        let err = layer
            .wrap(ok_after(Duration::from_secs(60)))
            .oneshot(request(None))
            .await
            .expect_err("inner slower than the budget must time out");
        assert!(err.to_string().contains("<unknown>"), "{err}");
    }

    /// 验证：`deadline_error` 的错误分类与状态码映射
    /// （DeadlineExceeded / gRPC 4 / HTTP 504）并携带路由 id。
    #[test]
    fn deadline_error_mentions_route_id() {
        let err = deadline_error("user-http");
        assert_eq!(err.kind(), ErrorKind::DeadlineExceeded);
        assert_eq!(err.grpc_status(), 4);
        assert_eq!(err.http_status(), http::StatusCode::GATEWAY_TIMEOUT);
        assert!(err.to_string().contains("user-http"), "{err}");
    }

    /// [`ProxyBody`] 单次 `poll_frame` 的结果类型别名。
    type FrameResult = Option<Result<Frame<Bytes>, ProxyError>>;

    /// 测试专用 body：立刻吐出第 1 帧，第 2 帧延迟 `delay` 后才产出。
    ///
    /// 第二帧由 `tokio::time` 的 sleep 把关，测试可用 `time::advance`
    /// 在两次 poll 之间明确跨过截止期时刻——验证「响应体在截止期之后
    /// 继续流动」的契约。
    struct DelayedBody {
        delay: Duration,
        state: DelayedState,
    }

    /// `DelayedBody` 的产出状态机：
    /// `FirstFrame` →（吐帧 1）`Gap(sleep)` →（睡眠到期）`SecondFrame`
    /// →（吐帧 2）`Done`（流结束）。
    enum DelayedState {
        FirstFrame,
        Gap(Pin<Box<tokio::time::Sleep>>),
        SecondFrame,
        Done,
    }

    impl DelayedBody {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                state: DelayedState::FirstFrame,
            }
        }
    }

    impl Body for DelayedBody {
        type Data = Bytes;
        type Error = ProxyError;

        fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<FrameResult> {
            let this = self.get_mut();
            let delay = this.delay;
            loop {
                // 先把状态「拿走」（置为 Done 占位）：状态里含 Pin<Box<Sleep>>，
                // 必须 move 出来才能 poll；未完成的分支再把状态放回去。
                // 若哪天漏了放回，下一轮循环会看到 Done、干净地结束流，
                // 不会卡死或重复吐帧。
                match std::mem::replace(&mut this.state, DelayedState::Done) {
                    DelayedState::FirstFrame => {
                        this.state = DelayedState::Gap(Box::pin(tokio::time::sleep(delay)));
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"frame-1")))));
                    }
                    DelayedState::Gap(mut gap) => match gap.as_mut().poll(cx) {
                        Poll::Ready(()) => this.state = DelayedState::SecondFrame,
                        Poll::Pending => {
                            this.state = DelayedState::Gap(gap);
                            return Poll::Pending;
                        }
                    },
                    DelayedState::SecondFrame => {
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"frame-2")))));
                    }
                    DelayedState::Done => return Poll::Ready(None),
                }
            }
        }
    }

    /// 对 pin 住的 [`ProxyBody`] 驱动一次 `poll_frame` 直到完成。
    async fn next_frame(body: &mut Pin<Box<ProxyBody>>) -> FrameResult {
        std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await
    }

    /// 取 body 的下一个数据块；流出错直接 panic（仅测试用）。
    async fn next_chunk(body: &mut Pin<Box<ProxyBody>>) -> Option<Bytes> {
        next_frame(body).await.map(|frame| {
            frame
                .expect("body must not fail")
                .into_data()
                .expect("data frame")
        })
    }

    /// 验证核心契约：总截止期只把关响应 **headers**——响应头在预算内到达
    /// 的调用原样通过，其响应体在截止期时刻之后仍继续流到结束。
    #[tokio::test(start_paused = true)]
    async fn passing_body_keeps_streaming_after_the_deadline() {
        let budget = Duration::from_secs(2);
        let layer = DeadlineLayer::new(budget);
        let inner = BoxCloneService::new(service_fn(
            move |_req: http::Request<ProxyBody>| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let body = boxed_body(DelayedBody::new(Duration::from_secs(30)));
                Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(body))
            },
        ));

        // (1) 响应头在预算内到达：调用成功
        let started = tokio::time::Instant::now();
        let response = layer
            .wrap(inner)
            .oneshot(request(Some("user-http")))
            .await
            .expect("head within budget must pass the deadline layer");
        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(
            started.elapsed() <= budget,
            "head must arrive within the {:?} budget, took {:?}",
            budget,
            started.elapsed()
        );

        // 帧 1 立刻流出（尚在截止期时刻之前）
        let mut body = Box::pin(response.into_body());
        assert_eq!(
            next_chunk(&mut body).await,
            Some(Bytes::from_static(b"frame-1"))
        );

        // 推进时钟，明确跨过截止期时刻（t0 + budget）且远超
        tokio::time::advance(budget * 30).await;
        assert!(
            started.elapsed() > budget,
            "advance must cross the deadline instant"
        );

        // (2) 截止期之后 body 仍产出帧 2 并正常结束：流式路径没有
        // DeadlineExceeded
        assert_eq!(
            next_chunk(&mut body).await,
            Some(Bytes::from_static(b"frame-2"))
        );
        assert!(
            next_frame(&mut body).await.is_none(),
            "body must end without error after the deadline"
        );
    }
}
