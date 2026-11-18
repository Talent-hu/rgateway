//! Retry 测试共用辅助：假 inner、响应构造器、重放断言收集器
//! （仅测试编译，供 [`super::tests`] 与 [`super::behavior_tests`] 共用）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use rgateway_core::body::stream_body;
use rgateway_core::{
    boxed_body, empty_body, BoxedService, ErrorKind, GatewayError, ProxyBody, ProxyError,
    StateRegistry,
};
use tower::service_fn;
use tower::util::BoxCloneService;

use super::*;
use crate::test_support::build_ctx_with_buffer;

/// helper：Retry 层包住假 inner（缓冲上限可指定）。
pub(super) fn chain_of(
    f: FilterSpec,
    inner: BoxedService,
    max_retry_buffer: usize,
) -> BoxedService {
    let states = StateRegistry::new();
    let fctx = build_ctx_with_buffer("r1", states.route_state("r1"), max_retry_buffer);
    RetryFactory.build(&f, &fctx).expect("build")(inner)
}

/// 指定状态码 + 静态体的响应。
pub(super) fn status_resp(status: u16, body: &'static [u8]) -> http::Response<ProxyBody> {
    let mut resp = http::Response::new(stream_body(futures_util::stream::iter(vec![Ok(
        Frame::data(Bytes::from_static(body)),
    )])));
    *resp.status_mut() = http::StatusCode::from_u16(status).expect("valid status");
    resp
}

/// gRPC 家族响应形态构造：`head:N` = 响应头 grpc-status:N；`trailer:N`
/// = 体尾 trailer；`plain` = 非 gRPC 响应但带 grpc-status 头；`big` =
/// 超限体（8 字节，无 trailer）。
pub(super) fn grpc_shape_resp(shape: &str) -> http::Response<ProxyBody> {
    let mut resp = match shape {
        "head:0" | "head:14" => {
            let code = shape.strip_prefix("head:").expect("prefix");
            let mut resp = http::Response::new(empty_body());
            resp.headers_mut().insert("grpc-status", header_value(code));
            resp
        }
        "trailer:0" | "trailer:14" => {
            let code = shape.strip_prefix("trailer:").expect("prefix");
            let mut trailers = http::HeaderMap::new();
            trailers.insert("grpc-status", header_value(code));
            let frames = futures_util::stream::iter(vec![
                Ok(Frame::data(Bytes::from_static(b"payload"))),
                Ok(Frame::trailers(trailers)),
            ]);
            http::Response::new(stream_body(frames))
        }
        "plain" => {
            let mut resp = http::Response::new(empty_body());
            resp.headers_mut().insert("grpc-status", header_value("14"));
            resp
        }
        "big" => http::Response::new(chunk_frames(&["12345678"])),
        other => panic!("unknown grpc shape {other}"),
    };
    if shape != "plain" {
        resp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/grpc+proto"),
        );
    }
    resp
}

/// 多帧数据请求体（未知长度，无 Content-Length）。
pub(super) fn chunk_frames(frames: &[&'static str]) -> ProxyBody {
    let items: Vec<Result<Frame<Bytes>, ProxyError>> = frames
        .iter()
        .map(|f| Ok(Frame::data(Bytes::from_static(f.as_bytes()))))
        .collect();
    stream_body(futures_util::stream::iter(items))
}

/// `grpc-status` 头值（纯数字段）。
fn header_value(code: &str) -> http::HeaderValue {
    http::HeaderValue::from_str(code).expect("ascii digits")
}

/// 按 kind 造错误（载荷统一 SECRET_DETAIL 伪装内部细节）。
pub(super) fn err_of(kind: ErrorKind) -> GatewayError {
    match kind {
        ErrorKind::Unavailable => GatewayError::Unavailable("SECRET_DETAIL".into()),
        ErrorKind::DeadlineExceeded => GatewayError::DeadlineExceeded("SECRET_DETAIL".into()),
        ErrorKind::RateLimited => GatewayError::RateLimited("SECRET_DETAIL".into()),
        ErrorKind::CircuitOpen => GatewayError::CircuitOpen("SECRET_DETAIL".into()),
        ErrorKind::Unauthenticated => GatewayError::Unauthenticated("SECRET_DETAIL".into()),
        ErrorKind::PermissionDenied => GatewayError::PermissionDenied("SECRET_DETAIL".into()),
        ErrorKind::NotFound => GatewayError::NotFound("SECRET_DETAIL".into()),
        ErrorKind::InvalidArgument => GatewayError::InvalidArgument("SECRET_DETAIL".into()),
        ErrorKind::Internal => GatewayError::Internal("SECRET_DETAIL".into()),
    }
}

/// Drop 时计数的响应体（验证重试间响应体被丢弃）。
pub(super) fn drop_count_body(data: &'static [u8], drops: Arc<AtomicUsize>) -> ProxyBody {
    boxed_body(DropCountBody {
        data: Some(Bytes::from_static(data)),
        drops,
    })
}

struct DropCountBody {
    data: Option<Bytes>,
    drops: Arc<AtomicUsize>,
}

impl Body for DropCountBody {
    type Data = Bytes;
    type Error = ProxyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, ProxyError>>> {
        let this = self.get_mut();
        match this.data.take() {
            Some(data) => std::task::Poll::Ready(Some(Ok(Frame::data(data)))),
            None => std::task::Poll::Ready(None),
        }
    }
}

impl Drop for DropCountBody {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// 记录每次尝试收到的请求体、按序号返回结果的假 inner（重放断言用）。
pub(super) fn body_scripted<F>(f: F) -> (BoxedService, Arc<Mutex<Vec<Vec<u8>>>>)
where
    F: Fn(usize) -> Result<http::Response<ProxyBody>, GatewayError> + Send + Sync + 'static,
{
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let f = Arc::new(f);
    let svc = BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
        let seen = Arc::clone(&seen2);
        let f = Arc::clone(&f);
        async move {
            let bytes = match req.into_body().collect().await {
                Ok(collected) => collected.to_bytes().to_vec(),
                Err(err) => panic!("request body must collect: {err}"),
            };
            let mut log = seen.lock().expect("lock");
            log.push(bytes);
            let i = log.len() - 1;
            drop(log);
            f.as_ref()(i)
        }
    }));
    (svc, seen)
}
