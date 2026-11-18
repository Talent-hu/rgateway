//! [`crate::governance`] 的测试：熔断器生命周期、保留状态与状态注册表。
//!
//! 时间相关用例统一用 `#[tokio::test(start_paused = true)]` + 手动 advance
//! 虚拟时钟，避免真实睡眠带来的抖动。

use super::*;

/// 测试用小参数配置：窗口 4、失败率 0.5、min_calls 2、open 5s、半开探测上限 2。
fn cfg() -> BreakerConfig {
    BreakerConfig {
        sliding_window: 4,
        failure_rate: 0.5,
        min_calls: 2,
        open_duration: Duration::from_secs(5),
        half_open_max_calls: 2,
    }
}

/// 验证：窗口样本数不足 min_calls 时，即使全失败也不熔断（防小样本误判）。
#[test]
fn breaker_stays_closed_below_min_calls() {
    let b = Breaker::new("r", "i", cfg());
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Closed);
}

/// 验证：达到 min_calls 且失败率越线时转入 Open，之后 check 被拒绝。
#[test]
fn breaker_opens_on_error_rate() {
    let b = Breaker::new("r", "i", cfg());
    b.check().unwrap().record(false);
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Open);
    assert!(matches!(b.check(), Err(GatewayError::CircuitOpen(_))));
}

/// 验证：permit 未 record 即 drop（被取消的调用）按失败计入窗口。
#[test]
fn dropped_permit_counts_as_failure() {
    let b = Breaker::new("r", "i", cfg());
    drop(b.check().unwrap());
    drop(b.check().unwrap());
    assert_eq!(b.state(), BreakerState::Open);
}

/// 验证：Open 到期前一直拒绝；到期后首次 check 转 HalfOpen，探测成功即回 Closed。
#[tokio::test(start_paused = true)]
async fn breaker_half_open_recovery() {
    let b = Breaker::new("r", "i", cfg());
    b.check().unwrap().record(false);
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Open);

    // open_duration 未到：仍然拒绝
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(b.check().is_err());

    // open_duration 已过：下一次调用转 HalfOpen 探测
    tokio::time::advance(Duration::from_secs(2)).await;
    let permit = b.check().unwrap();
    assert_eq!(b.state(), BreakerState::HalfOpen);
    permit.record(true);
    assert_eq!(b.state(), BreakerState::Closed);
}

/// 验证：HalfOpen 探测失败重新 Open，且 open_duration 计时重启。
#[tokio::test(start_paused = true)]
async fn breaker_half_open_failure_reopens() {
    let b = Breaker::new("r", "i", cfg());
    b.check().unwrap().record(false);
    b.check().unwrap().record(false);
    tokio::time::advance(Duration::from_secs(6)).await;
    let permit = b.check().unwrap();
    permit.record(false);
    assert_eq!(b.state(), BreakerState::Open);

    // 计时器已重启：时长再次到期前一直拒绝
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(b.check().is_err());
}

/// 验证：HalfOpen 并发探测受上限约束；迟到的探测结果不污染已 Closed 的状态。
#[tokio::test(start_paused = true)]
async fn breaker_half_open_caps_concurrent_probes() {
    let b = Breaker::new("r", "i", cfg());
    b.check().unwrap().record(false);
    b.check().unwrap().record(false);
    tokio::time::advance(Duration::from_secs(6)).await;
    let p1 = b.check().unwrap();
    let p2 = b.check().unwrap();
    assert!(b.check().is_err());
    p1.record(true); // 一次成功立即关闭熔断
    assert_eq!(b.state(), BreakerState::Closed);
    // 第二个探测的迟到结果不得破坏已 Closed 的状态
    p2.record(false);
    assert_eq!(b.state(), BreakerState::Closed);
}

/// 验证：breaker_open 的时间感知语义——探测期一到即翻 false（实例重新
/// 可选），恢复链路才能跑得起来；状态转移仍由 check() 执行。
#[tokio::test(start_paused = true)]
async fn breaker_open_stops_skipping_once_probe_is_due() {
    let state = RouteState::new("r");
    let b = state.breaker_for("i", cfg());
    b.check().unwrap().record(false);
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Open);
    assert!(
        state.breaker_open("i"),
        "skipped while the open period lasts"
    );

    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(
        b.state(),
        BreakerState::Open,
        "transition happens in check()"
    );
    assert!(
        !state.breaker_open("i"),
        "probe-due instances must stay selectable so check() can recover them"
    );
    let permit = b.check().expect("probe-due breaker must grant a permit");
    assert_eq!(b.state(), BreakerState::HalfOpen);
    permit.record(true);
    assert_eq!(b.state(), BreakerState::Closed);
}

/// 验证：reset 强制回 Closed 并清空窗口，之后的调用直接放行。
#[test]
fn breaker_reset_closes() {
    let b = Breaker::new("r", "i", cfg());
    b.check().unwrap().record(false);
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Open);
    b.reset();
    assert_eq!(b.state(), BreakerState::Closed);
    assert!(b.check().is_ok());
}

/// 验证：滑动窗口逐出最旧结果——旧失败老化出窗后错误率才可能再次越线。
#[test]
fn sliding_window_evicts_old_results() {
    // window=2：第一个失败必须老化出窗，否则累计历史 [F,T,T,F,F] 永远
    // 达不到 1.0 失败率，就无法解释本测试的最终 Open。
    let mut c = cfg();
    c.sliding_window = 2;
    c.failure_rate = 1.0;
    c.min_calls = 2;
    let b = Breaker::new("r", "i", c);
    b.check().unwrap().record(false);
    b.check().unwrap().record(true);
    b.check().unwrap().record(true);
    assert_eq!(b.state(), BreakerState::Closed);
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Closed);
    b.check().unwrap().record(false);
    assert_eq!(b.state(), BreakerState::Open);
}

/// 验证：preserved 槽第二次访问返回先前对象（`make` 不再执行），状态跨
/// 访问（即跨热重载）存活。
#[test]
fn preserved_state_survives_second_access() {
    let state = RouteState::new("r");
    let a: Arc<AtomicU64> = state.preserved("limiter", || Arc::new(AtomicU64::new(7)));
    a.fetch_add(1, Ordering::Relaxed);
    // 若错误地重建，值会是 999；保留语义下应仍是 8
    let b: Arc<AtomicU64> = state.preserved("limiter", || Arc::new(AtomicU64::new(999)));
    assert_eq!(b.load(Ordering::Relaxed), 8);
}

/// 验证：StateRegistry 对同一 route_id 复用同一 RouteState（指针相等）。
#[test]
fn registry_reuses_route_state_across_lookups() {
    let reg = StateRegistry::new();
    let s1 = reg.route_state("user-http");
    let s2 = reg.route_state("user-http");
    assert!(Arc::ptr_eq(&s1, &s2));
    assert_ne!(reg.route_state("other").route_id.as_ref(), "user-http");
    assert_eq!(reg.route_ids().len(), 2);
}

/// 验证：registry 可重置路由下全部熔断器，并按存活路由列表清理状态。
#[test]
fn registry_reset_and_retain() {
    let reg = StateRegistry::new();
    let s = reg.route_state("r");
    let breaker = s.breaker_for("127.0.0.1:1", cfg());
    breaker.check().unwrap().record(false);
    breaker.check().unwrap().record(false);
    assert_eq!(breaker.state(), BreakerState::Open);
    assert_eq!(reg.reset_breaker("r", None), 1);
    assert_eq!(breaker.state(), BreakerState::Closed);

    reg.retain_routes(&[Arc::from("other")]);
    assert!(reg.route_ids().iter().all(|id| id.as_ref() == "other"));
}

/// 验证：retain_instances 丢弃已不在存活列表中的实例的熔断器。
#[test]
fn route_state_retain_instances_drops_stale_breakers() {
    let s = RouteState::new("r");
    s.breaker_for("a:1", cfg());
    s.breaker_for("b:2", cfg());
    let live = vec![Instance::new("127.0.0.1:1".parse().unwrap(), 100)];
    // 把 id 改写为 "a:1" 以对齐 breaker 的 key（Instance::new 默认用 addr 作 id）。
    let live = vec![Instance {
        id: Arc::from("a:1"),
        ..live.into_iter().next().unwrap()
    }];
    s.retain_instances(&live);
    let states = s.breaker_states();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].0.as_ref(), "a:1");
}
