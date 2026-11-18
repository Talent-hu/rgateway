//! Timeout 过滤器测试：参数表格、暂停时钟下的超时行为、
//! 「只约束响应头」契约（响应体在超时时刻之后继续流式）。

use std::time::Duration;

use rgateway_core::{boxed_body, empty_body, ErrorKind, GatewayError, StateRegistry};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use super::*;
use crate::test_support::{build_ctx, request, slow_ok, spec, DelayedResponseBody};

/// 参数解析表格：每行 = (紧凑 spec 文本, 期望构建成功?, 失败消息应含)。
#[test]
fn build_param_table() {
    let cases: &[(&str, bool, &str)] = &[
        ("Timeout=per_attempt:5s", true, ""),
        ("Timeout=timeout:5s", true, ""),
        // 两者都给时 per_attempt 优先（validate 的二选一约定）
        ("Timeout=per_attempt:5s,timeout:9s", true, ""),
        // 两个都没有 → 报 per_attempt（提示二选一）
        ("Timeout=foo:1", false, "per_attempt"),
        ("Timeout=per_attempt:abc", false, "per_attempt"),
        // 时长必须 > 0，且不接受百分比语法
        ("Timeout=per_attempt:0s", false, "per_attempt"),
        ("Timeout=timeout:0ms", false, "timeout"),
        ("Timeout=per_attempt:50%", false, "per_attempt"),
    ];
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    for (compact, ok, err) in cases {
        let result = TimeoutFactory.build(&spec(compact), &fctx);
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

/// helper：以给定 spec 构建超时层并包住指定 inner。
fn timeout_chain(compact: &str, inner: rgateway_core::BoxedService) -> rgateway_core::BoxedService {
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    let wrapper = TimeoutFactory.build(&spec(compact), &fctx).expect("build");
    wrapper(inner)
}

/// 验证：内层慢于预算时超时，错误为 DeadlineExceeded
/// （gRPC 4 / HTTP 504），内部消息点名 per-attempt timeout。
#[tokio::test(start_paused = true)]
async fn slow_inner_times_out_with_deadline_exceeded() {
    let chain = timeout_chain("Timeout=per_attempt:1s", slow_ok(Duration::from_secs(60)));
    let err = chain
        .oneshot(request(None, None))
        .await
        .expect_err("inner slower than per_attempt must time out");
    assert_eq!(err.kind(), ErrorKind::DeadlineExceeded);
    assert_eq!(err.grpc_status(), 4);
    assert_eq!(err.http_status().as_u16(), 504);
    assert!(err.to_string().contains("per-attempt timeout"), "{err}");
}

/// 验证：`per_attempt` 与 `timeout` 同时给出时 **per_attempt 优先**——
/// 若实现误取 `timeout`，慢 60s 的内层会在 60s 处成功返回，本用例就会失败。
#[tokio::test(start_paused = true)]
async fn per_attempt_takes_priority_over_timeout() {
    let start = tokio::time::Instant::now();
    let chain = timeout_chain(
        "Timeout=per_attempt:1s,timeout:60s",
        slow_ok(Duration::from_secs(60)),
    );
    let err = chain
        .oneshot(request(None, None))
        .await
        .expect_err("per_attempt=1s 必须先于 timeout=60s 生效");
    assert_eq!(err.kind(), ErrorKind::DeadlineExceeded);
    // 超时发生在 1s 而不是 60s：钉死优先级取的是 per_attempt
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(1),
        "应在 per_attempt 处超时"
    );
}

/// 验证 `timeout` 参数（per_attempt 缺席时）同样生效，且只约束响应头。
#[tokio::test(start_paused = true)]
async fn timeout_alias_works() {
    let chain = timeout_chain("Timeout=timeout:1s", slow_ok(Duration::from_secs(60)));
    let err = chain
        .oneshot(request(None, None))
        .await
        .expect_err("inner slower than timeout must time out");
    assert_eq!(err.kind(), ErrorKind::DeadlineExceeded);
    assert!(err.to_string().contains("per-attempt timeout"), "{err}");
}

/// 验证：内层快于预算时原样透传（状态码、body 不被改动）。
#[tokio::test(start_paused = true)]
async fn fast_inner_passes_through_untouched() {
    let chain = timeout_chain("Timeout=per_attempt:5s", slow_ok(Duration::from_millis(1)));
    let response = chain
        .oneshot(request(None, None))
        .await
        .expect("inner within budget must pass through");
    assert_eq!(response.status(), http::StatusCode::OK);
}

/// 验证核心契约：超时只把关响应 **headers**——响应头在预算内到达的调用
/// 原样通过，其响应体在超时时刻之后仍继续流到结束（流式路径不受影响）。
#[tokio::test(start_paused = true)]
async fn passing_body_keeps_streaming_after_the_timeout() {
    let budget = Duration::from_secs(1);
    let chain = timeout_chain(
        "Timeout=per_attempt:1s",
        BoxCloneService::new(service_fn(
            move |_req: http::Request<rgateway_core::ProxyBody>| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let body = boxed_body(DelayedResponseBody::new(Duration::from_secs(30)));
                Ok::<http::Response<rgateway_core::ProxyBody>, GatewayError>(http::Response::new(
                    body,
                ))
            },
        )),
    );

    // (1) 响应头在预算内到达：调用成功
    let response = chain
        .oneshot(request(None, None))
        .await
        .expect("head within budget must pass");
    assert_eq!(response.status(), http::StatusCode::OK);

    // 帧 1 立刻流出（超时时刻之前）
    use http_body_util::BodyExt;
    let mut body = response.into_body();
    let frame1 = body.frame().await.expect("frame 1").expect("data");
    assert_eq!(
        frame1.into_data().expect("data"),
        bytes::Bytes::from_static(b"frame-1")
    );

    // (2) 推进时钟越过 per-attempt 超时时刻，帧 2 仍流出并正常结束
    tokio::time::advance(budget * 30).await;
    let frame2 = body.frame().await.expect("frame 2").expect("data");
    assert_eq!(
        frame2.into_data().expect("data"),
        bytes::Bytes::from_static(b"frame-2")
    );
    assert!(body.frame().await.is_none(), "body must end without error");
}

/// 验证缺请求上下文不是错误：超时层不依赖 ctx（预算编译期已定），
/// 无 ctx 的请求也能正常走链（内层需要 ctx 的是别的过滤器）。
#[tokio::test(start_paused = true)]
async fn works_without_request_ctx() {
    let chain = timeout_chain("Timeout=per_attempt:5s", slow_ok(Duration::from_millis(1)));
    let response = chain
        .oneshot(http::Request::new(empty_body()))
        .await
        .expect("timeout layer must not require ctx");
    assert_eq!(response.status(), http::StatusCode::OK);
}
