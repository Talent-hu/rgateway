//! RateLimit 过滤器测试：参数解析表格、限流行为、桶表有界与淘汰、
//! 治理状态跨重载保留。

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::FakeRelativeClock;
use rgateway_core::{ErrorKind, RouteState, StateRegistry};
use tower::ServiceExt;

use super::*;
use crate::test_support::{build_ctx, counting_ok, request, spec};

/// 参数解析表格：每行是 (紧凑 spec 文本, 期望构建成功?, 失败消息应含)。
/// 列含义：spec = 过滤器 DSL；ok = build 是否应成功；err = 失败时消息应含的片段。
#[test]
fn build_param_table() {
    let cases: &[(&str, bool, &str)] = &[
        ("RateLimit=key:client_ip,rate:2,period:1s", true, ""),
        ("RateLimit=key:route,rate:5", true, ""),
        ("RateLimit=key:client_ip,rate:100,period:500ms", true, ""),
        // 缺 key / 缺 rate：必填参数缺失
        ("RateLimit=rate:2,period:1s", false, "key"),
        ("RateLimit=key:client_ip,period:1s", false, "rate"),
        // key 维度值只接受 client_ip / route
        ("RateLimit=key:bogus,rate:2", false, "client_ip"),
        // rate 必须 > 0 且放得进 NonZeroU32
        ("RateLimit=key:client_ip,rate:0,period:1s", false, "rate"),
        ("RateLimit=key:client_ip,rate:abc", false, "rate"),
        (
            "RateLimit=key:client_ip,rate:5000000000,period:1s",
            false,
            "rate",
        ),
        // period 必须 > 0，且不接受百分比语法
        ("RateLimit=key:client_ip,rate:2,period:0s", false, "period"),
        ("RateLimit=key:client_ip,rate:2,period:50%", false, "period"),
        // period/rate 截断为 0 时无法构造 Quota
        (
            "RateLimit=key:client_ip,rate:4294967295,period:1ms",
            false,
            "rate",
        ),
    ];
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    for (compact, ok, err) in cases {
        let result = RateLimitFactory.build(&spec(compact), &fctx);
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

/// helper：以给定 spec 构建限流层并包住计数假 inner。
fn rate_limited_chain(
    compact: &str,
    state: Arc<RouteState>,
    counter: Arc<AtomicUsize>,
) -> rgateway_core::BoxedService {
    let fctx = build_ctx("r1", state);
    let wrapper = RateLimitFactory
        .build(&spec(compact), &fctx)
        .expect("build");
    wrapper(counting_ok(counter))
}

/// 验证 client_ip 维度的桶按 IP 相互独立：每行 (源 IP, 期望放行?)。
/// rate=2 每 100s（测试期间不补给），10.0.0.1 两次后拒绝，10.0.0.2 独立放行。
#[tokio::test(start_paused = true)]
async fn client_ip_buckets_are_independent() {
    let states = StateRegistry::new();
    let chain = rate_limited_chain(
        "RateLimit=key:client_ip,rate:2,period:100s",
        states.route_state("r1"),
        Arc::new(AtomicUsize::new(0)),
    );
    // 每行 = (源地址, 期望放行)
    let cases = [
        ("10.0.0.1:1000", true),
        ("10.0.0.1:2000", true),
        ("10.0.0.1:3000", false),
        ("10.0.0.2:1000", true),
        ("10.0.0.2:2000", true),
        ("10.0.0.2:3000", false),
    ];
    for (addr, allowed) in cases {
        let peer = addr.parse().expect("addr");
        let result = chain.clone().oneshot(request(Some(peer), None)).await;
        assert_eq!(
            result.is_ok(),
            allowed,
            "{addr}: allowed={allowed}, got {result:?}"
        );
    }
}

/// 验证 route 维度整条路由共用一个桶：不同 IP 共享限额（rate=1 每 100s）。
#[tokio::test(start_paused = true)]
async fn route_dimension_shares_one_bucket() {
    let states = StateRegistry::new();
    let chain = rate_limited_chain(
        "RateLimit=key:route,rate:1,period:100s",
        states.route_state("r1"),
        Arc::new(AtomicUsize::new(0)),
    );
    let a = "10.0.0.1:1000".parse().expect("addr");
    let b = "10.0.0.2:1000".parse().expect("addr");
    assert!(chain.clone().oneshot(request(Some(a), None)).await.is_ok());
    let err = chain
        .clone()
        .oneshot(request(Some(b), None))
        .await
        .expect_err("second request must be rate limited");
    assert_eq!(err.kind(), ErrorKind::RateLimited);
}

/// 验证拒绝请求的错误映射：RateLimited / HTTP 429 / gRPC 8，文案泛化。
#[tokio::test(start_paused = true)]
async fn denied_request_maps_to_rate_limited() {
    let states = StateRegistry::new();
    let chain = rate_limited_chain(
        "RateLimit=key:client_ip,rate:1,period:100s",
        states.route_state("r1"),
        Arc::new(AtomicUsize::new(0)),
    );
    let peer = "10.0.0.1:1000".parse().expect("addr");
    assert!(chain
        .clone()
        .oneshot(request(Some(peer), None))
        .await
        .is_ok());
    let err = chain
        .clone()
        .oneshot(request(Some(peer), None))
        .await
        .expect_err("over-quota request must be denied");
    assert_eq!(err.kind(), ErrorKind::RateLimited);
    assert_eq!(err.http_status().as_u16(), 429);
    assert_eq!(err.grpc_status(), 8);
    assert_eq!(err.public_message(), "rate limit exceeded");
}

/// 验证缺请求上下文时归 Internal（分发层未注入 ctx 即 bug）。
#[tokio::test(start_paused = true)]
async fn missing_ctx_is_internal_error() {
    let states = StateRegistry::new();
    let chain = rate_limited_chain(
        "RateLimit=key:client_ip,rate:1,period:100s",
        states.route_state("r1"),
        Arc::new(AtomicUsize::new(0)),
    );
    let req = http::Request::new(rgateway_core::empty_body());
    let err = chain.oneshot(req).await.expect_err("missing ctx must fail");
    assert_eq!(err.kind(), ErrorKind::Internal);
}

/// 验证 GCRA 桶随时间补给：每行 = (本次检查前推进的时钟, 期望放行?)。
/// rate=2 每 1s（补给间隔 500ms，突发 2）。
#[test]
fn buckets_replenish_over_time() {
    let clock = FakeRelativeClock::default();
    let quota = make_quota(2, Duration::from_secs(1)).expect("quota");
    let table = BoundedBuckets::with_quota(16, quota, clock.clone());
    // 每行 = (检查前推进, 期望放行)
    let cases = [
        (Duration::ZERO, true),
        (Duration::ZERO, true),
        (Duration::ZERO, false),
        (Duration::from_millis(500), true),
        (Duration::ZERO, false),
        (Duration::from_millis(500), true),
    ];
    for (advance, allowed) in cases {
        clock.advance(advance);
        assert_eq!(
            table.check("k"),
            allowed,
            "advance {advance:?} -> {allowed}"
        );
    }
}

/// 验证桶表有界且按 LRU 淘汰：cap=8，插入 30 个不同键后表长不超过上限，
/// 最久未用的键被淘汰（重新检查获得全新额度），最近的键保留（额度已耗尽）。
#[test]
fn bucket_table_is_bounded_with_lru_eviction() {
    let clock = FakeRelativeClock::default();
    let quota = make_quota(1, Duration::from_secs(100)).expect("quota");
    let table = BoundedBuckets::with_quota(8, quota, clock);
    for i in 0..30 {
        let key = format!("k{i}");
        assert!(table.check(&key), "fresh key {key} must pass");
        assert!(
            table.len() <= 8,
            "table must stay bounded, len={}",
            table.len()
        );
    }
    // 最近使用的键仍在表里：额度已耗尽，应被拒绝
    assert!(
        !table.check("k29"),
        "recent key must be retained and exhausted"
    );
    // 最久未用的键已被淘汰：重新出现即全新桶，放行
    assert!(table.check("k0"), "oldest key must have been evicted");
    assert!(table.len() <= 8, "table must stay bounded after re-insert");
}

/// 验证限流桶经 preserved 槽跨「重载」保留：同一 RouteState 重建过滤器后，
/// 已耗尽的桶不复位（README 承诺「限流桶跨重载存活」）。
#[tokio::test(start_paused = true)]
async fn buckets_survive_filter_rebuild() {
    let states = StateRegistry::new();
    let compact = "RateLimit=key:client_ip,rate:1,period:100s";
    let peer = "10.0.0.1:1000".parse().expect("addr");

    // 第一版过滤器：耗尽该 IP 的唯一许可
    let chain = rate_limited_chain(
        compact,
        states.route_state("r1"),
        Arc::new(AtomicUsize::new(0)),
    );
    assert!(chain.oneshot(request(Some(peer), None)).await.is_ok());

    // 模拟热重载：同一 RouteState 上重建过滤器（preserved 槽复用桶表）
    let rebuilt = rate_limited_chain(
        compact,
        states.route_state("r1"),
        Arc::new(AtomicUsize::new(0)),
    );
    let err = rebuilt
        .oneshot(request(Some(peer), None))
        .await
        .expect_err("bucket must survive rebuild");
    assert_eq!(err.kind(), ErrorKind::RateLimited);
}
