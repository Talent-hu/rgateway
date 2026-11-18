//! Retry 参数与决策表格：参数解析、次数边界、方法闸、错误分类
//! （触发条件 / 退避 / 缓冲行为见 `retry_behavior_tests.rs`）。

use std::sync::atomic::Ordering;

use rgateway_core::{empty_body, ErrorKind, GatewayError, StateRegistry};
use tower::ServiceExt;

use super::test_util::{chain_of, err_of, status_resp};
use super::*;
use crate::test_support::{
    build_ctx_with_buffer, request, request_with_method, scripted_fn, spec, spec_yaml,
};

/// 参数解析表格：每行 = (spec 文本, 期望构建成功?, 失败消息应含)。
/// 列含义：yaml?（`{` 开头走结构化）/ 紧凑 DSL；ok = build 成败；err = 消息片段。
#[test]
fn build_param_table() {
    // 每行 = (spec 文本, 期望成功, 错误应含)
    let cases: &[(&str, bool, &str)] = &[
        // retries 可选（缺省 3）、给出时 <= 10
        ("Retry=retries:0", true, ""),
        ("Retry=retries:3", true, ""),
        ("Retry=retries:10", true, ""),
        ("Retry=retries:11", false, "retries"),
        ("Retry=retries:abc", false, "retries"),
        ("{name: Retry, methods: [GET]}", true, ""),
        ("{name: Retry}", true, ""),
        // 列表与 backoff 全量形态
        (
            "{name: Retry, retries: 3, methods: [GET, HEAD], statuses: [502, 503], grpc_statuses: [14], backoff: {initial: 50ms, max: 2s, multiplier: 2, jitter: full}}",
            true,
            "",
        ),
        ("{name: Retry, retries: 2, backoff: {initial: 10ms}}", true, ""),
        // backoff 子键越界 / 类型错
        ("{name: Retry, retries: 2, backoff: {initial: 0s}}", false, "initial"),
        ("{name: Retry, retries: 2, backoff: {max: 0s}}", false, "max"),
        ("{name: Retry, retries: 2, backoff: {multiplier: 0.5}}", false, "multiplier"),
        ("{name: Retry, retries: 2, backoff: {multiplier: abc}}", false, "multiplier"),
        ("{name: Retry, retries: 2, backoff: {jitter: weird}}", false, "jitter"),
        ("{name: Retry, retries: 2, backoff: 5}", false, "map"),
        // 列表元素类型 / 状态码范围
        ("{name: Retry, retries: 2, statuses: [700]}", false, "statuses"),
        ("{name: Retry, retries: 2, statuses: [abc]}", false, "u64 list"),
        ("{name: Retry, retries: 2, methods: 5}", false, "string list"),
    ];
    for (text, ok, err) in cases {
        let f = if text.starts_with('{') {
            spec_yaml(text)
        } else {
            spec(text)
        };
        let states = StateRegistry::new();
        let fctx = build_ctx_with_buffer("r1", states.route_state("r1"), 1024);
        let result = RetryFactory.build(&f, &fctx);
        match result {
            Ok(_) => assert!(ok, "{text:?}: build must fail"),
            Err(e) => {
                assert!(!ok, "{text:?}: build must succeed, got {e}");
                assert!(
                    e.to_string().contains(err),
                    "{text:?}: error {e} must mention {err:?}"
                );
            }
        }
    }
}

/// 次数边界表格：每行 = (retries, 前几次失败?, 期望调用次数, 期望最终结果)。
/// 列含义：fails = 失败次数（>=retries 即全程失败）；calls = inner 精确
/// 调用次数（总尝试 = 1 + retries）；want = None 表示最终成功。
#[tokio::test(start_paused = true)]
async fn attempt_count_table() {
    // 每行 = (retries, 失败次数, 期望调用次数, 期望最终 kind(None=200))
    let cases: &[(u64, usize, usize, Option<ErrorKind>)] = &[
        (0, 99, 1, Some(ErrorKind::Unavailable)),
        (1, 99, 2, Some(ErrorKind::Unavailable)),
        (2, 99, 3, Some(ErrorKind::Unavailable)),
        (2, 2, 3, None),
        (1, 1, 2, None),
        (3, 1, 2, None),
    ];
    for (retries, fails, want_calls, want_kind) in cases {
        let f = spec(&format!("Retry=retries:{retries}"));
        let (inner, counter) = scripted_fn({
            let fails = *fails;
            move |i| {
                if i < fails {
                    Err(GatewayError::Unavailable("SECRET_DETAIL".into()))
                } else {
                    Ok(http::Response::new(empty_body()))
                }
            }
        });
        let chain = chain_of(f, inner, 1024 * 1024);
        let result = chain.clone().oneshot(request(None, None)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            *want_calls,
            "retries={retries} fails={fails}: exact attempts"
        );
        match (result, want_kind) {
            (Ok(resp), None) => assert_eq!(resp.status().as_u16(), 200, "retries={retries}"),
            (Err(err), Some(kind)) => assert_eq!(err.kind(), *kind, "retries={retries}"),
            (other, _) => panic!("retries={retries} fails={fails}: unexpected {other:?}"),
        }
    }
}

/// 验证：`retries` 缺省生效值为 3（SCG 对齐）——不写 retries 时总尝试
/// 恰好 1 + 3 次，与显式 `retries:3` 行为一致。
#[tokio::test(start_paused = true)]
async fn default_retries_is_three() {
    // cases 列：spec 文本 | 期望 inner 调用次数（1 + 默认/显式 retries）
    let cases: &[(&str, usize)] = &[
        ("{name: Retry, statuses: [503]}", 4),
        ("{name: Retry, retries: 3, statuses: [503]}", 4),
    ];
    for (text, want_calls) in cases {
        let f = spec_yaml(text);
        let (inner, counter) = scripted_fn(|_| Ok::<_, GatewayError>(status_resp(503, b"trouble")));
        let chain = chain_of(f, inner, 1024 * 1024);
        let result = chain.oneshot(request(None, None)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            *want_calls,
            "{text:?}: 缺省 retries 应等价 retries:3"
        );
        assert_eq!(
            result.expect("响应非错误").status().as_u16(),
            503,
            "{text:?}"
        );
    }
}

/// 方法闸表格：每行 = (methods, 请求方法, 期望调用次数)。
/// methods 非空时方法不在列表内绝不重试（1 次即透传）。
#[tokio::test(start_paused = true)]
async fn method_gate_table() {
    // 每行 = (methods YAML, 请求方法, 期望调用次数)
    let cases: &[(&str, &str, usize)] = &[
        ("[GET, HEAD]", "POST", 1),
        ("[GET, HEAD]", "GET", 2),
        ("[GET]", "HEAD", 1),
        ("[]", "POST", 2),
    ];
    for (methods, method, want_calls) in cases {
        let f = spec_yaml(&format!(
            "{{name: Retry, retries: 1, methods: {methods}, statuses: [503], backoff: {{initial: 1ms}}}}"
        ));
        let (inner, counter) = scripted_fn(|_| Ok::<_, GatewayError>(status_resp(503, b"trouble")));
        let chain = chain_of(f, inner, 1024 * 1024);
        let req = request_with_method(method, None, empty_body(), None);
        let _ = chain.clone().oneshot(req).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            *want_calls,
            "methods={methods} method={method}: method gate"
        );
    }
}

/// 错误分类表格：每行 = (错误 kind, 期望调用次数)。只有 Unavailable /
/// CircuitOpen 触发重试；RateLimited / DeadlineExceeded 等一律不重试。
#[tokio::test(start_paused = true)]
async fn error_kind_table() {
    // 每行 = (错误 kind, 期望调用次数)
    let cases: &[(ErrorKind, usize)] = &[
        (ErrorKind::Unavailable, 2),
        (ErrorKind::CircuitOpen, 2),
        (ErrorKind::DeadlineExceeded, 1),
        (ErrorKind::RateLimited, 1),
        (ErrorKind::Unauthenticated, 1),
        (ErrorKind::PermissionDenied, 1),
        (ErrorKind::NotFound, 1),
        (ErrorKind::InvalidArgument, 1),
        (ErrorKind::Internal, 1),
    ];
    for (kind, want_calls) in cases {
        let f = spec("Retry=retries:1");
        let (inner, counter) = scripted_fn(move |_| Err(err_of(*kind)));
        let chain = chain_of(f, inner, 1024 * 1024);
        let result = chain.clone().oneshot(request(None, None)).await;
        let err = result.expect_err("always fails");
        assert_eq!(err.kind(), *kind, "kind={kind:?} must pass through");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            *want_calls,
            "kind={kind:?}: retry only Unavailable/CircuitOpen"
        );
    }
}
