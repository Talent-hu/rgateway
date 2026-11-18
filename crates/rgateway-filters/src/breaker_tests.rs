//! CircuitBreaker 过滤器测试：参数表格、三态转移、结果分类、
//! permit 丢弃语义与跨重载窗口保留。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rgateway_core::{BreakerState, ErrorKind, RouteState, StateRegistry};
use tower::ServiceExt;

use super::*;
use crate::test_support::{build_ctx, empty_spec, request, slow_ok, spec, target, Outcome};

/// 参数解析表格：每行 = (参数 map 文本, 期望构建成功?, 失败消息应含)。
/// 空参数走全默认（20/0.5/10/5s/3），`failure_rate` 接受百分比写法。
#[test]
fn build_param_table() {
    // 每行 = (spec 文本或空串表示无参, 期望成功, 错误应含)
    let cases: &[(&str, bool, &str)] = &[
        ("", true, ""),
        (
            "CircuitBreaker=sliding_window:20,failure_rate:50%,min_calls:10,open_duration:5s,half_open_max_calls:3",
            true,
            "",
        ),
        ("CircuitBreaker=failure_rate:0.5", true, ""),
        ("CircuitBreaker=sliding_window:30,min_calls:30", true, ""),
        // 越界 / 类型错
        ("CircuitBreaker=sliding_window:0", false, "sliding_window"),
        ("CircuitBreaker=sliding_window:abc", false, "sliding_window"),
        ("CircuitBreaker=failure_rate:1.5", false, "failure_rate"),
        ("CircuitBreaker=failure_rate:50", false, "failure_rate"),
        ("CircuitBreaker=failure_rate:abc", false, "failure_rate"),
        ("CircuitBreaker=open_duration:0s", false, "open_duration"),
        ("CircuitBreaker=half_open_max_calls:0", false, "half_open_max_calls"),
        // min_calls > sliding_window（含只给 min_calls 超默认窗口 20）
        ("CircuitBreaker=sliding_window:10,min_calls:20", false, "min_calls"),
        ("CircuitBreaker=min_calls:21", false, "min_calls"),
    ];
    for (compact, ok, err) in cases {
        let f = if compact.is_empty() {
            empty_spec("CircuitBreaker")
        } else {
            spec(compact)
        };
        let states = StateRegistry::new();
        let fctx = build_ctx("r1", states.route_state("r1"));
        let result = CircuitBreakerFactory.build(&f, &fctx);
        match result {
            Ok(_) => assert!(ok, "{compact:?}: build must fail"),
            Err(e) => {
                assert!(!ok, "{compact:?}: build must succeed, got {e}");
                assert!(
                    e.to_string().contains(err),
                    "{compact:?}: error {e} must mention {err:?}"
                );
            }
        }
    }
}

/// helper：构建熔断层包住脚本 inner，返回 (链, 路由治理状态)。
fn breaker_chain(
    compact: &str,
    script: Vec<Outcome>,
    state: Arc<RouteState>,
) -> (rgateway_core::BoxedService, Arc<RouteState>) {
    let (inner, _log) = crate::test_support::scripted(script);
    let fctx = build_ctx("r1", Arc::clone(&state));
    let wrapper = CircuitBreakerFactory
        .build(&spec(compact), &fctx)
        .expect("build");
    (wrapper(inner), state)
}

/// 取路由里第一个（也是唯一一个）实例的熔断状态。
fn breaker_state(state: &RouteState) -> BreakerState {
    state
        .breaker_states()
        .first()
        .map(|(_, s)| *s)
        .unwrap_or(BreakerState::Closed)
}

/// 发一次带目标实例的请求：成功返回 `None`，失败返回错误分类。
async fn send_one(chain: &rgateway_core::BoxedService) -> Option<ErrorKind> {
    chain
        .clone()
        .oneshot(request(None, Some(target("inst-1"))))
        .await
        .err()
        .map(|e| e.kind())
}

/// 结果分类表格：每行 = (inner 单次结果, 记入窗口的成败)。
/// 配置 min_calls=1/failure_rate=50%：一次失败即熔断（Open），一次成功保持 Closed。
/// 列含义：outcome = inner 的结果；failure = 是否应记为失败。
#[tokio::test(start_paused = true)]
async fn outcome_classification_table() {
    // 每行 = (inner 结果, 应记为失败?)
    let cases: &[(Outcome, bool)] = &[
        (Outcome::Unavailable, true),
        (Outcome::Deadline, true),
        (Outcome::CircuitOpen, true),
        (Outcome::Internal, true),
        (Outcome::Status(500), true),
        (Outcome::Status(503), true),
        (Outcome::Ok200, false),
        (Outcome::Status(404), false),
        (Outcome::RateLimited, false),
        (Outcome::InvalidArgument, false),
    ];
    for (outcome, failure) in cases {
        let states = StateRegistry::new();
        let state = states.route_state("r1");
        let (chain, state) = breaker_chain(
            "CircuitBreaker=sliding_window:2,failure_rate:50%,min_calls:1",
            vec![*outcome],
            state,
        );
        let _ = send_one(&chain).await;
        let expected = if *failure {
            BreakerState::Open
        } else {
            BreakerState::Closed
        };
        assert_eq!(
            breaker_state(&state),
            expected,
            "outcome {outcome:?} failure={failure}"
        );
    }
}

/// 三态转移表格：每行 = (动作, 期望结果 ok/错误种类, 期望状态)。
/// 配置 window=4/min_calls=2/failure_rate=50%/open=5s/half_open_max=2。
#[tokio::test(start_paused = true)]
async fn three_state_transition_table() {
    let compact = "CircuitBreaker=sliding_window:4,failure_rate:50%,min_calls:2,open_duration:5s,half_open_max_calls:2";
    let states = StateRegistry::new();
    let (chain, state) = breaker_chain(
        compact,
        vec![
            Outcome::Unavailable, // 失败 1
            Outcome::Unavailable, // 失败 2 → 熔断
            Outcome::Ok200,       // 半开探测成功 → 闭合
            Outcome::Ok200,       // 闭合期正常
        ],
        states.route_state("r1"),
    );
    // 每行 = (推进时长, 期望结果 kind（None=成功）, 期望状态)
    let steps: &[(Duration, Option<ErrorKind>, BreakerState)] = &[
        (
            Duration::ZERO,
            Some(ErrorKind::Unavailable),
            BreakerState::Closed,
        ),
        (
            Duration::ZERO,
            Some(ErrorKind::Unavailable),
            BreakerState::Open,
        ),
        (
            Duration::ZERO,
            Some(ErrorKind::CircuitOpen),
            BreakerState::Open,
        ),
        (Duration::from_secs(5), None, BreakerState::Closed),
        (Duration::ZERO, None, BreakerState::Closed),
    ];
    for (advance, expect, want_state) in steps {
        tokio::time::advance(*advance).await;
        assert_eq!(send_one(&chain).await, *expect, "step advance={advance:?}");
        assert_eq!(
            &breaker_state(&state),
            want_state,
            "step advance={advance:?}"
        );
    }
}

/// 半开探测失败立即重开并重启计时：Open →(到期) HalfOpen →(失败) Open，
/// open_duration 内继续拒绝，到期后才再放探测。
#[tokio::test(start_paused = true)]
async fn half_open_failure_reopens_with_fresh_timer() {
    let compact = "CircuitBreaker=sliding_window:2,failure_rate:50%,min_calls:2,open_duration:5s,half_open_max_calls:2";
    let states = StateRegistry::new();
    let (chain, state) = breaker_chain(
        compact,
        vec![
            Outcome::Unavailable,
            Outcome::Unavailable,
            Outcome::Unavailable,
        ],
        states.route_state("r1"),
    );
    // 打到熔断
    let _ = send_one(&chain).await;
    let _ = send_one(&chain).await;
    assert_eq!(breaker_state(&state), BreakerState::Open);
    // 到期 → 半开探测失败 → 重开
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(
        send_one(&chain).await,
        Some(ErrorKind::Unavailable),
        "probe must run and fail"
    );
    assert_eq!(breaker_state(&state), BreakerState::Open);
    // 新的 open 计时内继续拒绝（1s < 5s）
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        send_one(&chain).await,
        Some(ErrorKind::CircuitOpen),
        "must reject inside the fresh open window"
    );
    // 计时走满后探测再次放行（脚本耗尽重复最后一项 = 继续失败，但探测要放）
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(
        send_one(&chain).await,
        Some(ErrorKind::Unavailable),
        "probe after the fresh window must run again"
    );
}

/// 半开探测并发上限：half_open_max_calls=1 时在途探测占满一席，
/// 第二个请求直接 CircuitOpen，不进 inner。
#[tokio::test(start_paused = true)]
async fn half_open_probe_limit_rejects_concurrent_probe() {
    let compact = "CircuitBreaker=sliding_window:2,failure_rate:50%,min_calls:2,open_duration:5s,half_open_max_calls:1";
    let states = StateRegistry::new();
    let state = states.route_state("r1");
    // 熔断状态挂在 RouteState 上，与链外壳无关：先用脚本链打到熔断，
    // 再换「慢成功」inner 的探测链跑并发探测。
    let (chain, _s) = breaker_chain(
        compact,
        vec![Outcome::Unavailable, Outcome::Unavailable],
        state.clone(),
    );
    let _ = send_one(&chain).await;
    let _ = send_one(&chain).await;
    assert_eq!(breaker_state(&state), BreakerState::Open);

    // 探测链：inner 慢（60s 后成功），并发第二发应被探测上限拒绝
    let fctx = build_ctx("r1", Arc::clone(&state));
    let wrapper = CircuitBreakerFactory
        .build(&spec(compact), &fctx)
        .expect("build");
    let probe_chain = wrapper(slow_ok(Duration::from_secs(60)));

    tokio::time::advance(Duration::from_secs(5)).await;
    let first = probe_chain
        .clone()
        .oneshot(request(None, Some(target("inst-1"))));
    let second = probe_chain
        .clone()
        .oneshot(request(None, Some(target("inst-1"))));
    let (first, second) = tokio::join!(first, second);
    assert!(first.is_ok(), "first probe must run: {first:?}");
    let err = second.expect_err("second concurrent probe must be rejected");
    assert_eq!(err.kind(), ErrorKind::CircuitOpen);
}

/// BreakerPermit 未 record 即 drop 按失败计入（core 语义）：
/// 中途取消的调用必须留下失败记录——过滤器不得绕过 permit 直连 inner。
#[tokio::test(start_paused = true)]
async fn dropped_call_counts_as_failure() {
    let compact = "CircuitBreaker=sliding_window:2,failure_rate:50%,min_calls:1,open_duration:5s";
    let states = StateRegistry::new();
    let state = states.route_state("r1");
    let fctx = build_ctx("r1", Arc::clone(&state));
    let wrapper = CircuitBreakerFactory
        .build(&spec(compact), &fctx)
        .expect("build");
    let chain = wrapper(slow_ok(Duration::from_secs(60)));
    // 1ms 超时把服务 future 连同 permit 一起丢弃
    let result = tokio::time::timeout(
        Duration::from_millis(1),
        chain.clone().oneshot(request(None, Some(target("inst-1")))),
    )
    .await;
    assert!(result.is_err(), "call must be cancelled");
    assert_eq!(
        breaker_state(&state),
        BreakerState::Open,
        "dropped permit must be recorded as failure and trip the breaker"
    );
}

/// 未选目标时直通 inner（熔断判的是实例，Target 槽负责选实例），
/// 不创建熔断器、不拦截。
#[tokio::test(start_paused = true)]
async fn missing_target_passes_through() {
    let counter = Arc::new(AtomicUsize::new(0));
    let inner = crate::test_support::counting_ok(Arc::clone(&counter));
    let states = StateRegistry::new();
    let state = states.route_state("r1");
    let fctx = build_ctx("r1", Arc::clone(&state));
    let wrapper = CircuitBreakerFactory
        .build(&empty_spec("CircuitBreaker"), &fctx)
        .expect("build");
    let chain = wrapper(inner);
    let result = chain.clone().oneshot(request(None, None)).await;
    assert!(result.is_ok(), "no target must pass through: {result:?}");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(
        state.breaker_states().is_empty(),
        "no breaker must be created without a target"
    );
}

/// 被熔断拒绝的错误映射：CircuitOpen / HTTP 503 / gRPC 14，文案泛化。
#[tokio::test(start_paused = true)]
async fn rejection_maps_to_circuit_open() {
    let states = StateRegistry::new();
    let (chain, _state) = breaker_chain(
        "CircuitBreaker=sliding_window:2,failure_rate:50%,min_calls:1",
        vec![Outcome::Unavailable],
        states.route_state("r1"),
    );
    let _ = send_one(&chain).await;
    let err = chain
        .clone()
        .oneshot(request(None, Some(target("inst-1"))))
        .await
        .expect_err("open breaker must reject");
    assert_eq!(err.kind(), ErrorKind::CircuitOpen);
    assert_eq!(err.http_status().as_u16(), 503);
    assert_eq!(err.grpc_status(), 14);
    assert_eq!(err.public_message(), "circuit breaker open");
}

/// 跨「重载」窗口保留：同一 RouteState 上换配置重建过滤器后，
/// 旧窗口样本仍在（reconfigure 只换阈值不重置窗口——core 语义）。
#[tokio::test(start_paused = true)]
async fn window_survives_rebuild_with_new_config() {
    let states = StateRegistry::new();
    let state = states.route_state("r1");
    // 第一版配置：1 次失败留在窗口里（min_calls=2 尚不熔断）
    let (chain, _s) = breaker_chain(
        "CircuitBreaker=sliding_window:4,failure_rate:50%,min_calls:2,open_duration:5s",
        vec![Outcome::Unavailable],
        state.clone(),
    );
    let _ = send_one(&chain).await;
    assert_eq!(breaker_state(&state), BreakerState::Closed);
    // 模拟热重载：同 state 换新配置重建；再来 1 次失败
    let (rebuilt, _s) = breaker_chain(
        "CircuitBreaker=sliding_window:8,failure_rate:50%,min_calls:2,open_duration:5s",
        vec![Outcome::Unavailable],
        state.clone(),
    );
    let _ = send_one(&rebuilt).await;
    // 若窗口被清零，此时只有 1 个样本、不会熔断；窗口保留则 2/2 全失败 → Open
    assert_eq!(
        breaker_state(&state),
        BreakerState::Open,
        "sliding window must survive the rebuild"
    );
}
