//! [`crate::target`] 的测试：候选过滤（healthy / weight / 熔断）、错误
//! 分类（Unavailable vs CircuitOpen）、熔断恢复回轮转、直连 URI 解析、
//! `wrap()` 的 set_target 覆写语义与 spawn_blocking DNS 路径。

use super::*;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use rgateway_core::{empty_body, BreakerConfig, ErrorKind, Protocol, RoundRobin, StaticDiscovery};

/// 造一个本机实例（weight=100、healthy），端口即身份。
fn inst(port: u16) -> Instance {
    Instance::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port), 100)
}

/// 一个空的 HTTP/1 请求上下文。
fn ctx() -> RequestCtx {
    RequestCtx::new(Protocol::Http1, None)
}

/// 构造 `lb://user-svc` 的选择器：静态发现里放给定实例，RoundRobin 均衡。
/// 一并返回 RouteState，测试可往里塞熔断状态。
fn lb_selector(instances: Vec<Instance>) -> (TargetSelector, Arc<RouteState>) {
    let state = Arc::new(RouteState::new("test-route"));
    let mut services = HashMap::new();
    services.insert(Arc::from("user-svc"), instances);
    let selector = TargetSelector {
        uri: RouteUri::Lb(Arc::from("user-svc")),
        discovery: Arc::new(StaticDiscovery::new(services)),
        balancer: Arc::new(RoundRobin::default()),
        state: Arc::clone(&state),
    };
    (selector, state)
}

/// 构造直连 URI 的选择器（发现为空，不影响直连路径）。
fn direct_selector(uri: &str) -> TargetSelector {
    TargetSelector {
        uri: RouteUri::parse(uri).expect("uri"),
        discovery: Arc::new(StaticDiscovery::default()),
        balancer: Arc::new(RoundRobin::default()),
        state: Arc::new(RouteState::new("test-route")),
    }
}

/// 打开实例的熔断：记录一次失败即触发（`min_calls: 1`、
/// `failure_rate: 0.0`），open 窗口取默认值。
fn trip_breaker(state: &RouteState, instance_id: &str) {
    trip_breaker_after(state, instance_id, BreakerConfig::default().open_duration);
}

/// 同 [`trip_breaker`]，但可指定 open 窗口长短。
fn trip_breaker_after(state: &RouteState, instance_id: &str, open_duration: Duration) {
    let config = BreakerConfig {
        min_calls: 1,
        failure_rate: 0.0,
        open_duration,
        ..BreakerConfig::default()
    };
    state
        .breaker_for(instance_id, config)
        .check()
        .expect("permit")
        .record(false);
}

/// 验证：连续两次 select 在两个健康实例间按 RoundRobin 轮转。
#[test]
fn select_round_robins_across_consecutive_calls() {
    let (selector, _state) = lb_selector(vec![inst(50051), inst(50052)]);
    let ctx = ctx();
    let first = selector.select(&ctx).expect("first pick");
    let second = selector.select(&ctx).expect("second pick");
    assert_eq!(first.instance_id.as_ref(), "127.0.0.1:50051");
    assert_eq!(second.instance_id.as_ref(), "127.0.0.1:50052");
}

/// 验证：熔断打开的实例被跳过，连续多次选择都落在健康的兄弟实例上。
#[test]
fn select_skips_instance_with_open_breaker() {
    let (selector, state) = lb_selector(vec![inst(50051), inst(50052)]);
    trip_breaker(&state, "127.0.0.1:50051");
    let ctx = ctx();
    for _ in 0..3 {
        let target = selector.select(&ctx).expect("one instance still usable");
        assert_eq!(target.instance_id.as_ref(), "127.0.0.1:50052");
    }
}

/// 验证：select 的错误分类（表驱动）。
///
/// 表列含义：`name` 用例名；`instances` 服务发现返回的实例集；`tripped`
/// 预先熔断的实例 id；`want` / `want_msg` 期望的错误分类与消息子串。
/// 四行分别覆盖：空发现 → Unavailable、全不健康 → Unavailable、
/// 全被熔断 → CircuitOpen、weight=0（out-of-rotation，安全修复语义）
/// → Unavailable 而非 CircuitOpen。
#[test]
fn select_failure_modes() {
    struct Case {
        name: &'static str,
        instances: Vec<Instance>,
        tripped: &'static [&'static str],
        want: ErrorKind,
        want_msg: &'static str,
    }
    let cases = vec![
        Case {
            name: "empty discovery",
            instances: vec![],
            tripped: &[],
            want: ErrorKind::Unavailable,
            want_msg: "no available instances for user-svc",
        },
        Case {
            name: "all instances unhealthy",
            instances: vec![Instance {
                healthy: false,
                ..inst(50051)
            }],
            tripped: &[],
            want: ErrorKind::Unavailable,
            want_msg: "no available instances for user-svc",
        },
        Case {
            name: "all breakers open",
            instances: vec![inst(50051), inst(50052)],
            tripped: &["127.0.0.1:50051", "127.0.0.1:50052"],
            want: ErrorKind::CircuitOpen,
            want_msg: "all instances open for user-svc",
        },
        Case {
            name: "healthy but out of rotation (weight 0)",
            instances: vec![Instance {
                weight: 0,
                ..inst(50051)
            }],
            tripped: &[],
            want: ErrorKind::Unavailable,
            want_msg: "no available instances for user-svc",
        },
    ];
    for case in cases {
        let (selector, state) = lb_selector(case.instances);
        for id in case.tripped {
            trip_breaker(&state, id);
        }
        let err = selector.select(&ctx()).expect_err(case.name);
        assert_eq!(err.kind(), case.want, "{}", case.name);
        assert!(
            err.to_string().contains(case.want_msg),
            "{}: {err}",
            case.name
        );
    }
}

/// 验证：open 窗口过后，被跳过的实例回到候选集（probe-due 可选），供
/// 链的 breaker 层跑半开恢复探测。
#[tokio::test(start_paused = true)]
async fn skipped_instance_returns_to_rotation_after_open_duration() {
    let (selector, state) = lb_selector(vec![inst(50051), inst(50052)]);
    trip_breaker_after(&state, "127.0.0.1:50051", Duration::from_secs(1));
    let ctx = ctx();
    // open 窗口内：只有兄弟实例可用
    for _ in 0..3 {
        let target = selector.select(&ctx).expect("sibling stays usable");
        assert_eq!(target.instance_id.as_ref(), "127.0.0.1:50052");
    }

    // 推进时钟越过 open 窗口，被熔断的实例应重新可选
    tokio::time::advance(Duration::from_secs(2)).await;
    let mut seen = Vec::new();
    for _ in 0..2 {
        let target = selector
            .select(&ctx)
            .expect("probe-due instance must be selectable");
        seen.push(target.instance_id.to_string());
    }
    assert!(
        seen.iter().any(|id| id == "127.0.0.1:50051"),
        "tripped instance must return to rotation once the open period \
         elapsed so the chain's breaker layer can run its half-open probe: {seen:?}"
    );
}

/// 验证：单实例被熔断时 open 窗口内报 CircuitOpen，窗口过后恢复可选。
#[tokio::test(start_paused = true)]
async fn single_open_instance_recovers_after_open_duration() {
    let (selector, state) = lb_selector(vec![inst(50051)]);
    trip_breaker_after(&state, "127.0.0.1:50051", Duration::from_secs(1));
    let err = selector
        .select(&ctx())
        .expect_err("skipped while the open period lasts");
    assert_eq!(err.kind(), ErrorKind::CircuitOpen);

    tokio::time::advance(Duration::from_secs(2)).await;
    let target = selector
        .select(&ctx())
        .expect("probe-due instance must be selectable");
    assert_eq!(target.instance_id.as_ref(), "127.0.0.1:50051");
}

/// 验证：直连 URI 的 authority 解析——实例 id 即 authority，端口缺省按
/// scheme 补 80/443。
///
/// 表列含义：(uri, 期望 instance id, 期望 socket 地址)。
#[test]
fn select_direct_resolves_authority() {
    // (uri, 期望 instance id, 期望 socket 地址)
    let cases = [
        (
            "http://127.0.0.1:50051",
            "127.0.0.1:50051",
            "127.0.0.1:50051",
        ),
        ("https://127.0.0.1", "127.0.0.1", "127.0.0.1:443"),
        ("http://127.0.0.1", "127.0.0.1", "127.0.0.1:80"),
    ];
    for (uri, want_id, want_addr) in cases {
        let target = direct_selector(uri).select(&ctx()).expect(uri);
        assert_eq!(target.instance_id.as_ref(), want_id, "{uri}");
        assert_eq!(target.addr.to_string(), want_addr, "{uri}");
    }
}

/// 验证：`wrap()` 每次调用都重新选目标并覆写 `ctx.target`（重试重选
/// 语义）——同一个 ctx 连调两次，内层服务看到两次不同的实例。
#[tokio::test]
async fn wrap_sets_target_and_overwrites_on_reselect() {
    let (selector, _state) = lb_selector(vec![inst(50051), inst(50052)]);
    let seen: Arc<Mutex<Vec<Arc<str>>>> = Arc::new(Mutex::new(Vec::new()));
    let for_inner = Arc::clone(&seen);
    let inner = service_fn(move |req: http::Request<ProxyBody>| {
        let seen = Arc::clone(&for_inner);
        async move {
            let ctx = request_ctx(&req).expect("ctx present");
            let target = ctx.target().expect("target set before inner runs");
            seen.lock().expect("lock").push(target.instance_id);
            Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
        }
    });
    let service = selector.wrap(BoxCloneService::new(inner));

    let ctx = Arc::new(ctx());
    for _ in 0..2 {
        let mut req = http::Request::new(empty_body());
        req.extensions_mut().insert(Arc::clone(&ctx));
        service.clone().oneshot(req).await.expect("call");
    }

    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert_ne!(
        seen[0], seen[1],
        "each attempt must re-select and overwrite the target"
    );
}

/// 验证：请求缺少 `Arc<RequestCtx>` 扩展时 `wrap()` 报 Internal
/// （"missing request ctx"），不触达内层服务。
#[tokio::test]
async fn wrap_without_ctx_is_internal_error() {
    let (selector, _state) = lb_selector(vec![inst(50051)]);
    let inner = service_fn(|_req: http::Request<ProxyBody>| async move {
        Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
    });
    let service = selector.wrap(BoxCloneService::new(inner));
    let err = service
        .oneshot(http::Request::new(empty_body()))
        .await
        .expect_err("missing ctx must fail");
    assert!(matches!(err, GatewayError::Internal(_)));
    assert!(err.to_string().contains("missing request ctx"), "{err}");
}

/// 验证：直连 URI 走 spawn_blocking DNS 路径（请求链实际用的路径），并把
/// 解析出的 target 写入 ctx。
#[tokio::test]
async fn wrap_direct_uri_sets_target() {
    // 走请求链实际使用的 spawn_blocking DNS 路径
    let selector = direct_selector("http://127.0.0.1:50051");
    let seen: Arc<Mutex<Vec<Arc<str>>>> = Arc::new(Mutex::new(Vec::new()));
    let for_inner = Arc::clone(&seen);
    let inner = service_fn(move |req: http::Request<ProxyBody>| {
        let seen = Arc::clone(&for_inner);
        async move {
            let ctx = request_ctx(&req).expect("ctx present");
            let target = ctx.target().expect("target set before inner runs");
            seen.lock().expect("lock").push(target.instance_id);
            Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
        }
    });
    let service = selector.wrap(BoxCloneService::new(inner));
    let mut req = http::Request::new(empty_body());
    req.extensions_mut().insert(Arc::new(ctx()));
    service.oneshot(req).await.expect("call");

    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].as_ref(), "127.0.0.1:50051");
}
