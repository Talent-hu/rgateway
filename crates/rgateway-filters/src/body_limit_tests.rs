//! BodyLimit 过滤器测试：参数表格、Content-Length 预检、流式限长、
//! 请求体错误的客户端故障归类。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rgateway_core::{empty_body, ErrorKind, StateRegistry};
use tower::ServiceExt;

use super::*;
use crate::test_support::{
    build_ctx, chunk_body, echo_body, failing_body, request_with_body, spec,
};

/// 参数解析表格：每行 = (紧凑 spec 文本, 期望构建成功?, 失败消息应含)。
#[test]
fn build_param_table() {
    let cases: &[(&str, bool, &str)] = &[
        ("BodyLimit=max_body:1KB", true, ""),
        ("BodyLimit=max_body:1048576", true, ""),
        ("BodyLimit=max_body:10MB", true, ""),
        // 缺必填 / 类型错 / 百分比语法都拒绝
        ("BodyLimit=x:1", false, "max_body"),
        ("BodyLimit=max_body:50%", false, "max_body"),
        ("BodyLimit=max_body:abc", false, "max_body"),
    ];
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    for (compact, ok, err) in cases {
        let result = BodyLimitFactory.build(&spec(compact), &fctx);
        match result {
            Ok(_) => assert!(ok, "{compact}: build must fail"),
            Err(e) => {
                assert!(!ok, "{compact}: build must succeed, got {e}");
                assert!(
                    e.to_string().contains(err),
                    "{compact}: error {e} must mention {err:?}"
                );
            }
        }
    }
}

/// helper：以给定 spec 构建限长层，包住可计数的回显 inner。
fn body_limited_chain(compact: &str, counter: Arc<AtomicUsize>) -> rgateway_core::BoxedService {
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    let wrapper = BodyLimitFactory
        .build(&spec(compact), &fctx)
        .expect("build");
    // 计数 + 回显：既统计 inner 调用次数，又把请求体错误归一后上抛
    let echo = echo_body();
    wrapper(tower::util::BoxCloneService::new(tower::service_fn(
        move |req: http::Request<rgateway_core::ProxyBody>| {
            let counter = Arc::clone(&counter);
            let echo = echo.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                echo.oneshot(req).await
            }
        },
    )))
}

/// Content-Length 预检表格：每行 = (声明长度, 上限, 期望放行?)。
/// 超限的请求在进入 inner 前就被拒绝（400 语义）。
#[tokio::test(start_paused = true)]
async fn content_length_precheck_rejects_oversized() {
    // 每行 = (Content-Length 声明, max_body, 期望放行)
    let cases = [
        (50usize, 100usize, true),
        (100, 100, true),
        (200, 100, false),
    ];
    for (declared, max, allowed) in cases {
        let counter = Arc::new(AtomicUsize::new(0));
        let chain = body_limited_chain(&format!("BodyLimit=max_body:{max}"), Arc::clone(&counter));
        let req = request_with_body(None, empty_body(), Some(declared));
        let result = chain.oneshot(req).await;
        assert_eq!(result.is_ok(), allowed, "declared={declared} max={max}");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            if allowed { 1 } else { 0 },
            "declared={declared}: inner must not be reached when rejected"
        );
        if !allowed {
            let err = result.expect_err("must be rejected");
            assert_eq!(err.kind(), ErrorKind::InvalidArgument);
            assert_eq!(err.http_status().as_u16(), 400);
            assert_eq!(err.grpc_status(), 3);
        }
    }
}

/// 验证：限长内的请求体原样透传（回显字节一致），inner 正常应答。
#[tokio::test(start_paused = true)]
async fn body_within_limit_passes_through() {
    let counter = Arc::new(AtomicUsize::new(0));
    let chain = body_limited_chain("BodyLimit=max_body:1KB", counter);
    let req = request_with_body(None, chunk_body(b"hello"), None);
    let response = chain.oneshot(req).await.expect("within limit must pass");
    assert_eq!(response.status(), http::StatusCode::OK);
    use http_body_util::BodyExt;
    let collected = response.into_body().collect().await.expect("body");
    assert_eq!(collected.to_bytes(), bytes::Bytes::from_static(b"hello"));
}

/// 验证：无 Content-Length 的超限请求体在流式阶段被截断，错误归
/// InvalidArgument（400），不归 Unavailable。
#[tokio::test(start_paused = true)]
async fn oversized_streaming_body_maps_to_invalid_argument() {
    let counter = Arc::new(AtomicUsize::new(0));
    let chain = body_limited_chain("BodyLimit=max_body:10", counter);
    let req = request_with_body(None, chunk_body(b"0123456789ABCDEF"), None);
    let err = chain
        .oneshot(req)
        .await
        .expect_err("oversized body must fail");
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    assert_eq!(err.http_status().as_u16(), 400);
}

/// 验证：非限长的请求体错误（客户端断流等）按客户端故障归
/// InvalidArgument，绝不归 Unavailable（不是上游故障）。
#[tokio::test(start_paused = true)]
async fn non_limit_body_error_is_client_fault() {
    let counter = Arc::new(AtomicUsize::new(0));
    let chain = body_limited_chain("BodyLimit=max_body:1MB", counter);
    let req = request_with_body(None, failing_body("client aborted"), None);
    let err = chain
        .oneshot(req)
        .await
        .expect_err("broken request body must fail");
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    assert!(
        !err.to_string().contains("unavailable"),
        "must not be classified as upstream failure: {err}"
    );
}

/// 验证：请求体错误的内部消息不进客户端文案（泛化为 "invalid argument"）。
#[tokio::test(start_paused = true)]
async fn body_error_messages_stay_generic_for_clients() {
    let counter = Arc::new(AtomicUsize::new(0));
    let chain = body_limited_chain("BodyLimit=max_body:4", counter);
    let req = request_with_body(None, chunk_body(b"secret-payload"), None);
    let err = chain
        .oneshot(req)
        .await
        .expect_err("oversized body must fail");
    assert_eq!(err.public_message(), "invalid argument");
    assert!(!err.public_message().contains("secret"));
}
