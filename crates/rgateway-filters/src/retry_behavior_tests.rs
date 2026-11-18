//! Retry 行为测试：三类触发条件（status / grpc-status）、退避时序
//! （暂停时钟 + `tokio::time::advance`）、重放缓冲禁用语义、响应体丢弃、
//! trailer 重建与错误文案红线（参数/决策表格见 `retry_tests.rs`）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use rgateway_core::{empty_body, GatewayError};
use tower::ServiceExt;

use super::test_util::{
    body_scripted, chain_of, chunk_frames, drop_count_body, grpc_shape_resp, status_resp,
};
use crate::test_support::{
    failing_body, request, request_with_method, scripted_fn, spec, spec_yaml,
};

/// 触发条件：响应 status 表格。每行 = (statuses, 响应码, 期望调用次数)。
#[tokio::test(start_paused = true)]
async fn status_trigger_table() {
    // 每行 = (statuses YAML, 响应码, 期望调用次数)
    let cases: &[(&str, u16, usize)] = &[
        ("[502, 503]", 503, 2),
        ("[502, 503]", 502, 2),
        ("[502, 503]", 500, 1),
        ("[]", 503, 1),
    ];
    for (statuses, code, want_calls) in cases {
        let f = spec_yaml(&format!(
            "{{name: Retry, retries: 1, statuses: {statuses}, backoff: {{initial: 1ms}}}}"
        ));
        let (inner, counter) = scripted_fn(move |_| Ok(status_resp(*code, b"body")));
        let chain = chain_of(f, inner, 1024 * 1024);
        let result = chain.clone().oneshot(request(None, None)).await;
        let resp = result.expect("response");
        assert_eq!(
            resp.status().as_u16(),
            *code,
            "final response passes through"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            *want_calls,
            "statuses={statuses} code={code}: status trigger"
        );
    }
}

/// 触发条件：gRPC grpc-status 表格。每行 = (grpc_statuses, 响应形态,
/// 期望调用次数)。形态 = `head:N`（响应头）/ `trailer:N`（trailer）/
/// `plain`（非 gRPC 响应带 grpc-status 头）。
#[tokio::test(start_paused = true)]
async fn grpc_trigger_table() {
    // 每行 = (grpc_statuses YAML, 形态, 期望调用次数)
    let cases: &[(&str, &str, usize)] = &[
        ("[14]", "head:14", 2),
        ("[14]", "head:0", 1),
        ("[14]", "trailer:14", 2),
        ("[14]", "trailer:0", 1),
        ("[]", "head:14", 1),
        ("[14]", "plain", 1),
    ];
    for (codes, shape, want_calls) in cases {
        let f = spec_yaml(&format!(
            "{{name: Retry, retries: 1, grpc_statuses: {codes}, backoff: {{initial: 1ms}}}}"
        ));
        let shape_owned = shape.to_string();
        let (inner, counter) = scripted_fn(move |_| Ok(grpc_shape_resp(&shape_owned)));
        let chain = chain_of(f, inner, 1024 * 1024);
        let _ = chain.clone().oneshot(request(None, None)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            *want_calls,
            "codes={codes} shape={shape}: grpc trigger"
        );
    }
}

/// 耗尽次数后原样返回最后一次的响应（不伪造错误、不吞响应）。
#[tokio::test(start_paused = true)]
async fn exhausted_retries_return_last_response() {
    let f = spec_yaml(
        "{name: Retry, retries: 1, statuses: [503], backoff: {initial: 1ms, jitter: none}}",
    );
    let (inner, counter) = scripted_fn(|_| Ok(status_resp(503, b"last")));
    let chain = chain_of(f, inner, 1024 * 1024);
    let resp = chain
        .clone()
        .oneshot(request(None, None))
        .await
        .expect("resp");
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.into_body().collect().await.expect("body").to_bytes(),
        Bytes::from_static(b"last")
    );
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// 暂停时钟断言退避睡眠时长：jitter=none 的 50ms / 100ms 确定性序列，
/// 相邻尝试的虚拟时刻差精确等于退避，总耗时 150ms。
#[tokio::test(start_paused = true)]
async fn backoff_sleeps_between_attempts() {
    let f = spec_yaml(
        "{name: Retry, retries: 2, backoff: {initial: 50ms, max: 2s, multiplier: 2, jitter: none}}",
    );
    let stamps: Arc<Mutex<Vec<tokio::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&stamps);
    let (inner, counter) = scripted_fn(move |_| {
        recorded
            .lock()
            .expect("lock")
            .push(tokio::time::Instant::now());
        Err(GatewayError::Unavailable("down".into()))
    });
    let chain = chain_of(f, inner, 1024 * 1024);
    let start = tokio::time::Instant::now();
    let result = chain.clone().oneshot(request(None, None)).await;
    assert!(result.is_err(), "always fails");
    assert_eq!(counter.load(Ordering::SeqCst), 3, "1 + retries attempts");
    assert_eq!(start.elapsed(), Duration::from_millis(150), "50ms + 100ms");
    let stamps = stamps.lock().expect("lock");
    // 每行 = 期望的相邻尝试时刻差（毫秒）
    let want_gaps = [50u64, 100];
    for (gap, want) in stamps.windows(2).zip(want_gaps) {
        assert_eq!(
            gap[1] - gap[0],
            Duration::from_millis(want),
            "attempt gap must equal backoff"
        );
    }
}

/// 推进虚拟时钟：退避未满不发第二次尝试，推进到点后立即补发
/// （`tokio::time::advance` 精确控制）。
#[tokio::test(start_paused = true)]
async fn next_attempt_waits_for_full_backoff() {
    let f = spec_yaml(
        "{name: Retry, retries: 1, backoff: {initial: 50ms, max: 2s, multiplier: 2, jitter: none}}",
    );
    let (inner, counter) = scripted_fn(|_| Err(GatewayError::Unavailable("down".into())));
    let chain = chain_of(f, inner, 1024 * 1024);
    let handle = tokio::spawn(chain.oneshot(request(None, None)));
    // 让被 spawn 的链先跑完第一次尝试、注册好 50ms 定时器
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(49)).await;
    assert!(
        !handle.is_finished(),
        "attempt 2 must wait for the full 50ms"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1, "only attempt 1 ran");
    tokio::time::advance(Duration::from_millis(1)).await;
    let result = handle.await.expect("join");
    assert!(result.is_err(), "always fails");
    assert_eq!(counter.load(Ordering::SeqCst), 2, "attempt 2 fired at 50ms");
}

/// jitter=full 的睡眠落在 [0, 基础退避] 内（打散重试风暴，不超上限）。
#[tokio::test(start_paused = true)]
async fn jitter_full_sleeps_within_cap() {
    let f = spec_yaml(
        "{name: Retry, retries: 3, backoff: {initial: 50ms, max: 2s, multiplier: 2, jitter: full}}",
    );
    let (inner, _counter) = scripted_fn(|_| Err(GatewayError::Unavailable("down".into())));
    let chain = chain_of(f, inner, 1024 * 1024);
    let start = tokio::time::Instant::now();
    let _ = chain.clone().oneshot(request(None, None)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed <= Duration::from_millis(50 + 100 + 200),
        "full jitter must never exceed base delays, got {elapsed:?}"
    );
}

/// 已知长度超 max_retry_buffer：禁用重试，单次透传，请求体原样到达。
#[tokio::test(start_paused = true)]
async fn known_overlong_body_disables_retry() {
    let f = spec("Retry=retries:3");
    let (inner, seen) = body_scripted(|_| Err(GatewayError::Unavailable("down".into())));
    let chain = chain_of(f, inner, 4);
    let req = request_with_method("GET", None, chunk_frames(&["hello"]), Some(5));
    let _ = chain.clone().oneshot(req).await;
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1, "retry must be disabled for overlong body");
    assert_eq!(
        &seen[0][..],
        &b"hello"[..],
        "body must pass through untouched"
    );
}

/// 未知长度超 max_retry_buffer：禁用重试且前缀 + 剩余流透传不丢数据。
#[tokio::test(start_paused = true)]
async fn unknown_overlong_body_disables_retry_without_loss() {
    let f = spec("Retry=retries:3");
    let (inner, seen) = body_scripted(|_| Err(GatewayError::Unavailable("down".into())));
    let chain = chain_of(f, inner, 16);
    let req = request_with_method(
        "GET",
        None,
        chunk_frames(&["01234567", "89abcdef", "ghij"]),
        None,
    );
    let _ = chain.clone().oneshot(req).await;
    let seen = seen.lock().expect("lock");
    assert_eq!(
        seen.len(),
        1,
        "retry must be disabled once buffering overflows"
    );
    assert_eq!(
        &seen[0][..],
        &b"0123456789abcdefghij"[..],
        "prefix + rest, no loss"
    );
}

/// 限内请求体每轮重试完整重放：已知/未知长度两种形态都逐次见到全量体。
#[tokio::test(start_paused = true)]
async fn replay_each_attempt_sees_full_body() {
    // 每行 = (帧序列, Content-Length 声明)
    let cases: &[(&[&'static str], Option<usize>)] =
        &[(&["ab", "cde"], Some(5)), (&["ab", "cde"], None)];
    for (frames, content_length) in cases {
        let f = spec_yaml(
            "{name: Retry, retries: 2, backoff: {initial: 1ms, multiplier: 2, jitter: none}}",
        );
        let (inner, seen) = body_scripted(|_| Err(GatewayError::Unavailable("down".into())));
        let chain = chain_of(f, inner, 1024);
        let req = request_with_method("GET", None, chunk_frames(frames), *content_length);
        let _ = chain.clone().oneshot(req).await;
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 3, "1 + 2 retries, cl={content_length:?}");
        for body in seen.iter() {
            assert_eq!(
                &body[..],
                &b"abcde"[..],
                "each attempt must see the full body"
            );
        }
    }
}

/// 每次重试前丢弃已收下的响应体：上一轮的响应体被 drop，不进最终响应。
#[tokio::test(start_paused = true)]
async fn response_body_discarded_between_attempts() {
    let f = spec_yaml(
        "{name: Retry, retries: 1, statuses: [503], backoff: {initial: 1ms, jitter: none}}",
    );
    let drops = Arc::new(AtomicUsize::new(0));
    let (inner, counter) = scripted_fn({
        let drops = Arc::clone(&drops);
        move |i| {
            if i == 0 {
                let mut resp = http::Response::new(drop_count_body(b"stale", Arc::clone(&drops)));
                *resp.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
                Ok(resp)
            } else {
                Ok(status_resp(200, b"fresh"))
            }
        }
    });
    let chain = chain_of(f, inner, 1024 * 1024);
    let resp = chain
        .clone()
        .oneshot(request(None, None))
        .await
        .expect("resp");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "stale response body must be dropped"
    );
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(
        body,
        Bytes::from_static(b"fresh"),
        "final body must not carry stale bytes"
    );
}

/// trailer 判定后重建的响应完整回放：非重试码的 gRPC 响应（体 + trailer）
/// 原样交还客户端。
#[tokio::test(start_paused = true)]
async fn trailer_peek_rebuild_preserves_response() {
    let f = spec_yaml(
        "{name: Retry, retries: 1, grpc_statuses: [14], backoff: {initial: 1ms, jitter: none}}",
    );
    let (inner, counter) = scripted_fn(|_| Ok(grpc_shape_resp("trailer:0")));
    let chain = chain_of(f, inner, 1024 * 1024);
    let resp = chain
        .clone()
        .oneshot(request(None, None))
        .await
        .expect("resp");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "trailer 0 must not retry"
    );
    let collected = resp.into_body().collect().await.expect("body");
    let trailers = collected.trailers().expect("trailers kept");
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"0"[..]),
        "trailer semantics must survive the rebuild"
    );
    assert_eq!(collected.to_bytes(), Bytes::from_static(b"payload"));
}

/// 最后一次尝试的响应（即便仍可重试）原样返回：重建体完整，不丢弃。
#[tokio::test(start_paused = true)]
async fn last_attempt_returns_retryable_response_intact() {
    let f = spec_yaml(
        "{name: Retry, retries: 1, grpc_statuses: [14], backoff: {initial: 1ms, jitter: none}}",
    );
    let (inner, counter) = scripted_fn(|_| Ok(grpc_shape_resp("trailer:14")));
    let chain = chain_of(f, inner, 1024 * 1024);
    let resp = chain
        .clone()
        .oneshot(request(None, None))
        .await
        .expect("resp");
    assert_eq!(counter.load(Ordering::SeqCst), 2, "one retry then give up");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(
        body,
        Bytes::from_static(b"payload"),
        "rebuilt body must be intact"
    );
}

/// trailer 探测超限：放弃判定按流透传（不重试、不丢数据、不无界缓冲）。
#[tokio::test(start_paused = true)]
async fn grpc_response_overflow_streams_back_without_retry() {
    let f = spec_yaml(
        "{name: Retry, retries: 3, grpc_statuses: [14], backoff: {initial: 1ms, jitter: none}}",
    );
    let (inner, counter) = scripted_fn(|_| Ok(grpc_shape_resp("big")));
    let chain = chain_of(f, inner, 4);
    let resp = chain
        .clone()
        .oneshot(request(None, None))
        .await
        .expect("resp");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "overflow must not retry");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(
        body,
        Bytes::from_static(b"12345678"),
        "no data loss on overflow"
    );
}

/// 缺失 RequestCtx 的请求照常重试（PRNG 种子退化为常量，不引入新错误）。
#[tokio::test(start_paused = true)]
async fn missing_ctx_still_retries() {
    let f = spec_yaml("{name: Retry, retries: 1, backoff: {initial: 1ms, jitter: none}}");
    let (inner, counter) = scripted_fn(|_| Err(GatewayError::Unavailable("down".into())));
    let chain = chain_of(f, inner, 1024 * 1024);
    let req = http::Request::new(empty_body());
    let result = chain.clone().oneshot(req).await;
    assert!(result.is_err());
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// 错误文案红线：耗尽重试后返回的错误不向客户端泄露 SECRET_DETAIL，
/// 内部细节只走日志通道（Display）。
#[tokio::test(start_paused = true)]
async fn secret_detail_never_reaches_the_client() {
    let f = spec_yaml("{name: Retry, retries: 1, backoff: {initial: 1ms, jitter: none}}");
    let (inner, _counter) = scripted_fn(|_| Err(GatewayError::Unavailable("SECRET_DETAIL".into())));
    let chain = chain_of(f, inner, 1024 * 1024);
    let err = chain
        .clone()
        .oneshot(request(None, None))
        .await
        .expect_err("always fails");
    assert_eq!(err.public_message(), "upstream unavailable");
    assert_eq!(err.grpc_status(), 14);
    assert!(
        !err.grpc_message().contains("SECRET_DETAIL"),
        "grpc_message must stay generic"
    );
    assert!(
        err.to_string().contains("SECRET_DETAIL"),
        "Display keeps detail for logs"
    );
}

/// 丢弃观测器自检：DropCountBody 帧产完即止，drop 计数 +1——保证
/// 「响应体被丢弃」断言的观测器本身可靠。
#[tokio::test(start_paused = true)]
async fn drop_count_body_polls_to_end_and_counts_drop() {
    let drops = Arc::new(AtomicUsize::new(0));
    {
        let mut body = drop_count_body(b"data", Arc::clone(&drops));
        let frame = body.frame().await.expect("first frame").expect("ok");
        assert_eq!(
            frame.into_data().expect("data"),
            Bytes::from_static(b"data")
        );
        assert!(body.frame().await.is_none(), "stream must end");
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// 形态构造器自检：未知 gRPC 形态直接 panic（测试夹具用错必须立刻暴露）。
#[test]
#[should_panic(expected = "unknown grpc shape")]
fn grpc_shape_resp_rejects_unknown_shape() {
    let _ = grpc_shape_resp("bogus");
}

/// 收集器自检：请求体读错误让测试立刻 panic（绝不静默吞错）。
#[tokio::test(start_paused = true)]
#[should_panic(expected = "request body must collect")]
async fn body_scripted_panics_on_broken_request_body() {
    let (inner, _seen) = body_scripted(|_| Ok(status_resp(200, b"ok")));
    let chain = chain_of(spec("Retry=retries:1"), inner, 1024);
    let req = request_with_method("GET", None, failing_body("boom"), None);
    let _ = chain.oneshot(req).await;
}
