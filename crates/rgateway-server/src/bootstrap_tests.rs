//! [`crate::bootstrap`] 的测试：启动装配端到端——最小配置起服务拿 404
//! JSON、TLS 拒绝闸、配置错误带 [`rgateway_config::Location`]、
//! `resolve_tracing_filter` 三级优先级，以及优雅关闭（在途请求窗口内完成 /
//! 超时 abort）。
//!
//! 装配路径全走生产入口 [`start`]（load → validate → TLS 闸 → tracing →
//! compile → 热重载 → 监听 accept），请求用裸 TCP HTTP/1.1 驱动真实 hyper
//! 栈；时间敏感断言只用「宽松上界 + 窗口下界」，不引入暂停时钟——生产 accept
//! 循环与网络 I/O 并存时 `start_paused` 的自动跳表会误触发关闭窗口定时器。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};

use super::net::{h2_window, is_grpc};
use super::*;

/// 优雅关闭窗口（用例 1/5）：留足余量让「窗口内完成」断言有容错。
const GRACE_WINDOW: &str = "5s";

/// shutdown 完成 / 在途请求完成的宽松上界。
const AWAIT_BOUND: Duration = Duration::from_secs(5);

/// 测试临时目录（`temp_dir` + uuid v7）：配置文件落盘位置，drop 即清理。
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rgateway-bootstrap-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// 写配置文件并返回其路径（start 的入参）。
    fn write_config(&self, yaml: &str) -> PathBuf {
        let path = self.path.join("rgateway.yaml");
        std::fs::write(&path, yaml).expect("write config file");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 最小可用配置：listen 127.0.0.1:0 + mgmt 127.0.0.1:0（并行测试不撞端口）
/// + 一条 Path=/api/** 直连路由。列含义：(路由上游 uri, 优雅关闭窗口)。
fn minimal_config(upstream_uri: &str, graceful_shutdown: &str) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\n  timeouts: {{ graceful_shutdown: {graceful_shutdown} }}\n\
         mgmt: {{ listen: \"127.0.0.1:0\" }}\n\
         routes:\n  - id: api\n    uri: \"{upstream_uri}\"\n    predicates: [\"Path=/api/**\"]\n"
    )
}

/// 裸 HTTP/1.1 GET（`Connection: close`），读到 EOF 返回原始响应文本。
async fn raw_get(addr: SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to gateway");
    let request = format!("GET {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    String::from_utf8_lossy(&buf).into_owned()
}

/// 挂起式上游：读完请求后挂起，`release()` 后才回写固定响应；
/// `wait_accepted()` 保证请求已「在途」。
struct StallUpstream {
    port: u16,
    accepted: Option<oneshot::Receiver<()>>,
    release: Arc<Notify>,
}

impl StallUpstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let port = listener.local_addr().expect("upstream addr").port();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let release = Arc::new(Notify::new());
        let release_task = Arc::clone(&release);
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = accepted_tx.send(());
            release_task.notified().await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await;
        });
        Self {
            port,
            accepted: Some(accepted_rx),
            release,
        }
    }

    /// 等到上游真正收到请求（在途状态成立）。
    async fn wait_accepted(&mut self) {
        if let Some(rx) = self.accepted.take() {
            rx.await.expect("upstream must accept");
        }
    }

    /// 放行挂起的响应。
    fn release(&self) {
        self.release.notify_one();
    }
}

// 最小装配端到端：路由外请求得 404 JSON（走真实 hyper 栈），随后
// shutdown() 干净退出。
#[tokio::test]
async fn start_serves_404_json_outside_routes_then_shuts_down() {
    let dir = TestDir::new();
    let config = dir.write_config(&minimal_config("http://127.0.0.1:1", GRACE_WINDOW));
    let gw = start(&config).await.expect("gateway must start");
    let resp = raw_get(gw.local_addr, "/nope").await;
    assert!(resp.starts_with("HTTP/1.1 404"), "status line: {resp}");
    assert!(
        resp.contains("{\"error\":\"not found\"}"),
        "404 JSON body: {resp}"
    );
    tokio::time::timeout(AWAIT_BOUND, gw.shutdown())
        .await
        .expect("shutdown must finish promptly");
}

// 「绝不降级明文」：tls.enabled=true 而证书/私钥缺失或坏必须启动失败
// 并点名出问题的材料——绝不在材料有问题时静默以明文监听。
#[tokio::test]
async fn start_rejects_tls_enabled_config() {
    let dir = TestDir::new();
    let config = dir.write_config(
        "server:\n  listen: \"127.0.0.1:0\"\n  tls: { enabled: true, cert: \"server.pem\", key: \"server.key\" }\n\
         mgmt: { listen: \"127.0.0.1:0\" }\n\
         routes:\n  - id: api\n    uri: \"http://127.0.0.1:1\"\n    predicates: [\"Path=/api/**\"]\n",
    );
    let err = start(&config).await.expect_err("missing material must fail");
    let text = format!("{err:#}");
    assert!(
        text.contains("server.pem") || text.contains("server.key"),
        "error must name the missing material: {text}"
    );
}

// 配置错误必须带 Location：解析失败在 <root>，语义校验失败在具体坐标。
// 列含义：(标签, 配置 YAML, 期望出现在错误里的位置坐标)
#[tokio::test]
async fn config_errors_carry_location() {
    let cases = [
        ("yaml-parse", "server: {\n", "<root>"),
        (
            "semantic",
            "server:\n  listen: \"999.0.0.0:0\"\n",
            "server.listen",
        ),
    ];
    for (label, yaml, want_location) in cases {
        let dir = TestDir::new();
        let config = dir.write_config(yaml);
        let err = start(&config).await.expect_err("bad config must fail");
        let text = format!("{err:#}");
        assert!(
            text.contains(want_location),
            "{label}: location {want_location} missing in: {text}"
        );
    }
}

// resolve_tracing_filter 三级优先级：config.observability.tracing_filter →
// RUST_LOG → "info"；空白串视为未设置。列含义：(配置过滤表达式, RUST_LOG, 期望)
#[test]
fn resolve_tracing_filter_priority() {
    let cases: [(Option<&str>, Option<&str>, &str); 6] = [
        (Some("rgateway=debug"), Some("warn"), "rgateway=debug"),
        (Some("rgateway=debug"), None, "rgateway=debug"),
        (None, Some("warn"), "warn"),
        (None, None, "info"),
        (Some("  "), Some("warn"), "warn"),
        (Some(""), None, "info"),
    ];
    for (config_filter, env_filter, want) in cases {
        let got = resolve_tracing_filter(config_filter, env_filter);
        assert_eq!(
            got.as_str(),
            want,
            "config={config_filter:?} env={env_filter:?}"
        );
    }
}

// 优雅关闭：挂起的在途请求在 shutdown() 之后于窗口内完成（放行上游），
// 且 shutdown() 干净返回。
#[tokio::test]
async fn graceful_shutdown_completes_inflight_request() {
    let mut upstream = StallUpstream::start().await;
    let dir = TestDir::new();
    let uri = format!("http://127.0.0.1:{}", upstream.port);
    let config = dir.write_config(&minimal_config(&uri, GRACE_WINDOW));
    let gw = start(&config).await.expect("gateway must start");
    let addr = gw.local_addr;
    let client = tokio::spawn(async move { raw_get(addr, "/api/x").await });
    upstream.wait_accepted().await;
    let shutdown = tokio::spawn(async move { gw.shutdown().await });
    // 给 shutdown 信号一点时间抵达 accept 循环，再放行在途请求
    tokio::time::sleep(Duration::from_millis(200)).await;
    upstream.release();
    let resp = tokio::time::timeout(AWAIT_BOUND, client)
        .await
        .expect("in-flight request must finish within the window")
        .expect("client task");
    assert!(resp.starts_with("HTTP/1.1 200"), "response: {resp}");
    assert!(resp.ends_with("ok"), "response: {resp}");
    tokio::time::timeout(AWAIT_BOUND, shutdown)
        .await
        .expect("shutdown must finish after in-flight completes")
        .expect("shutdown task");
}

// 超时 abort：上游永不响应时，shutdown() 在 graceful 窗口（300ms）过后
// abort 剩余连接任务并返回，客户端连接随 abort 断开。
#[tokio::test]
async fn shutdown_aborts_inflight_after_window() {
    let mut upstream = StallUpstream::start().await;
    let dir = TestDir::new();
    let uri = format!("http://127.0.0.1:{}", upstream.port);
    let config = dir.write_config(&minimal_config(&uri, "300ms"));
    let gw = start(&config).await.expect("gateway must start");
    let addr = gw.local_addr;
    let client = tokio::spawn(async move { raw_get(addr, "/api/x").await });
    upstream.wait_accepted().await;
    let started = Instant::now();
    tokio::time::timeout(AWAIT_BOUND, gw.shutdown())
        .await
        .expect("shutdown must return after the window");
    // 窗口下界（容 100ms 调度误差）：不许在窗口内就放弃在途连接
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "shutdown returned before the graceful window: {:?}",
        started.elapsed()
    );
    let resp = tokio::time::timeout(AWAIT_BOUND, client)
        .await
        .expect("aborted connection must end the client read")
        .expect("client task");
    assert!(
        !resp.contains("200"),
        "aborted request must not succeed: {resp}"
    );
}

/// h2 窗口转换 `usize → u32` 饱和封顶，不截断错位。
#[test]
fn h2_window_saturates_instead_of_truncating() {
    // cases 列：(输入 usize, 期望 u32)
    let cases: [(usize, u32); 3] = [(0, 0), (65_535, 65_535), (usize::MAX, u32::MAX)];
    for (input, want) in cases {
        assert_eq!(h2_window(input), want, "input={input}");
    }
}

/// `server.http2` 配置必须真实到达线上：裸 HTTP/2 prior-knowledge 握手读
/// 服务端 SETTINGS 帧，逐项断言 `max_concurrent_streams` 与
/// `initial_window_size` 恰为配置值——配置被静默忽略时本用例立即失败。
///
/// 连接级窗口用 1MiB（而非小值）：h2 只在累积增量 ≥ 当前窗口一半（65535/2）
/// 时才发 WINDOW_UPDATE（合并发送策略，见 h2 flow_control 的 unclaimed_capacity），
/// 小配置值只会在后续数据流动后才分批通告，握手期抓不到帧。
#[tokio::test]
async fn http2_settings_reach_the_wire() {
    let dir = TestDir::new();
    let config = dir.write_config(concat!(
        "server:\n",
        "  listen: \"127.0.0.1:0\"\n",
        "  http2: { max_concurrent_streams: 7, initial_stream_window_size: 12345,\n",
        "    initial_connection_window_size: 1048576 }\n",
        "  timeouts: { graceful_shutdown: 5s }\n",
        "mgmt: { listen: \"127.0.0.1:0\" }\n",
        "routes:\n",
        "  - id: api\n",
        "    uri: \"http://127.0.0.1:1\"\n",
        "    predicates: [\"Path=/api/**\"]\n",
    ));
    let gw = start(&config).await.expect("gateway must start");
    let (settings, conn_window_updates) = h2_read_server_settings(gw.local_addr, 1_048_576).await;
    let got = |id| {
        settings
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("SETTINGS 缺少 id={id}: {settings:?}"))
    };
    assert_eq!(got(3), 7, "max_concurrent_streams 必须等于配置值");
    assert_eq!(got(4), 12345, "initial_stream_window_size 必须等于配置值");
    // 连接级窗口经 stream 0 的 WINDOW_UPDATE 增量表达：65535(默认) + 增量 = 配置
    assert!(
        conn_window_updates
            .iter()
            .any(|inc| 65_535u32.saturating_add(*inc) == 1_048_576),
        "连接窗口增量应使窗口达到 1048576: {conn_window_updates:?}"
    );
    gw.shutdown().await;
}

/// 裸 h2 握手：发送 preface + 空 SETTINGS，读服务端帧直到拿到关键 SETTINGS
/// 项与连接窗口增量（或超时）。返回 (SETTINGS 键值表, stream 0 窗口增量表)。
async fn h2_read_server_settings(
    addr: SocketAddr,
    conn_window_target: u32,
) -> (Vec<(u16, u32)>, Vec<u32>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect to gateway");
    let mut opening = Vec::from(&b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"[..]);
    opening.extend_from_slice(&[0, 0, 0, 0x4, 0, 0, 0, 0, 0]); // 空 SETTINGS 帧
    stream.write_all(&opening).await.expect("write preface");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut settings = Vec::new();
    let mut conn_window_updates = Vec::new();
    loop {
        let mut header = [0u8; 9];
        let read = tokio::time::timeout_at(deadline, stream.read_exact(&mut header)).await;
        let Ok(Ok(_)) = read else { break };
        let len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0
            && !matches!(
                tokio::time::timeout_at(deadline, stream.read_exact(&mut payload)).await,
                Ok(Ok(_))
            )
        {
            break;
        }
        match header[3] {
            // SETTINGS（非 ACK）：解析 (id, value) 表
            0x4 if header[4] & 0x1 == 0 => {
                for chunk in payload.chunks_exact(6) {
                    let id = u16::from_be_bytes([chunk[0], chunk[1]]);
                    let value = u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]);
                    settings.push((id, value));
                }
            }
            // WINDOW_UPDATE 于 stream 0：连接级窗口增量（负载恰 4 字节）
            0x8 if u32::from_be_bytes([header[5], header[6], header[7], header[8]]) == 0
                && payload.len() == 4 =>
            {
                conn_window_updates.push(u32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
            }
            _ => {}
        }
        let has_core = settings.iter().any(|(id, _)| *id == 3);
        let has_conn = conn_window_updates
            .iter()
            .any(|inc| 65_535u32.saturating_add(*inc) == conn_window_target);
        if has_core && has_conn {
            break;
        }
    }
    (settings, conn_window_updates)
}

// ---- 过滤器注册表覆盖 / 指标安装 / 管理面监听 ----

/// 构造 `build` 所需的路由级上下文（注册表测试用）。
fn build_ctx() -> rgateway_config::FilterBuildCtx {
    rgateway_config::FilterBuildCtx {
        route_id: Arc::from("test-route"),
        route_state: Arc::new(rgateway_core::RouteState::new("test-route")),
        discovery: Arc::new(rgateway_core::StaticDiscovery::new(
            std::collections::HashMap::new(),
        )),
        max_retry_buffer: 1024,
        config_dir: Arc::new(std::path::PathBuf::from("conf")),
    }
}

// 运行时注册表 = no-op 底座 + 真实工厂覆盖：真实工厂的参数校验生效
// （Retry=retries:999 超上限必须报错——no-op 占位会放行），未实装的
// canonical 名仍是恒等占位（配置可加载、运行期零拦截）。
// 列含义：(过滤器 DSL 文本, 期望 build 成功?)
#[test]
fn runtime_registry_overlays_real_factories() {
    let registry = runtime_registry();
    let ctx = build_ctx();
    let cases: &[(&str, bool)] = &[
        ("Retry=retries:2", true),
        ("Retry=retries:999", false), // 真实工厂的上限校验（占位工厂会放行）
        ("RateLimit=key:client_ip,rate:100,period:1s", true),
        ("Compression", true), // 未实装 → 恒等占位
        ("JwtAuth", false),    // 已实装：缺 issuer/audience/jwks_file 必须报错
        ("ApiKeyAuth", false), // 已实装：缺 keys 必须报错
        ("IpFilter", false),   // 已实装：缺 allow/deny 必须报错
    ];
    for &(spec_text, want_ok) in cases {
        // 紧凑 DSL 必须带 `Name=...`；无参过滤器走结构化形态（name-only spec）
        let spec = if spec_text.contains('=') {
            rgateway_config::FilterSpec::parse_compact(spec_text).expect("spec parses")
        } else {
            rgateway_config::FilterSpec {
                name: spec_text.to_string(),
                params: Default::default(),
                source: spec_text.to_string(),
            }
        };
        let factory = registry.get(&spec.name).expect("factory registered");
        let built = factory.build(&spec, &ctx);
        assert_eq!(built.is_ok(), want_ok, "{spec_text}: build 结果与预期不符");
    }
    // 名单完整性：12 个 canonical 名一个不少
    assert_eq!(registry.names().len(), rgateway_config::FILTER_NAMES.len());
}

// 指标句柄进程级共享：多次装配（测试并发 / 进程内重启）拿到同一个
// recorder 句柄，第二次不因「重复安装」而丢指标面；禁用时返回 None。
#[test]
fn shared_metrics_handle_is_reused_across_calls() {
    let first = shared_metrics_handle(true).expect("首次装配必须拿到指标句柄");
    let second = shared_metrics_handle(true).expect("重复装配必须共享句柄而不是安装失败");
    // 复用语义的实质断言：往全局 recorder 写一条探针，两个句柄都必须看得见
    // ——若实现返回了两个不同 recorder，第二个渲染必然缺探针。
    metrics::counter!("rgateway_test_shared_probe").increment(1);
    let (a, b) = (first.render(), second.render());
    assert!(a.contains("rgateway_test_shared_probe"), "探针不可见: {a}");
    assert!(
        b.contains("rgateway_test_shared_probe"),
        "句柄未共享同一 recorder: {b}"
    );
    assert!(shared_metrics_handle(false).is_none(), "禁用指标时无句柄");
}

// 管理面监听端到端：/metrics 输出 Prometheus 文本（数据面计量可见），
// /mgmt/v1/stats 走统一信封；mgmt_addr 报告实际绑定端口。
#[tokio::test]
async fn mgmt_listener_serves_metrics_and_stats() {
    let dir = TestDir::new();
    let config = dir.write_config(&minimal_config("http://127.0.0.1:1", GRACE_WINDOW));
    let gw = start(&config).await.expect("gateway must start");
    // 打一发无路由请求，产生 route_not_found 计数
    let miss = raw_get(gw.local_addr, "/nope").await;
    assert!(miss.starts_with("HTTP/1.1 404"), "{miss}");
    let metrics = raw_get(gw.mgmt_addr, "/metrics").await;
    assert!(
        metrics.starts_with("HTTP/1.1 200"),
        "metrics 状态行: {metrics}"
    );
    assert!(
        metrics.contains("rgateway_route_not_found_total"),
        "指标应可见数据面计量: {metrics}"
    );
    let stats = raw_get(gw.mgmt_addr, "/mgmt/v1/stats").await;
    assert!(stats.starts_with("HTTP/1.1 200"), "{stats}");
    assert!(stats.contains("\"success\":true"), "stats 信封: {stats}");
    tokio::time::timeout(AWAIT_BOUND, gw.shutdown())
        .await
        .expect("shutdown must finish promptly");
}

// 管理面 content-type 分流表：gRPC（含 +codec 后缀与参数）进 gRPC 通道，
// gRPC-Web 家族是数据面协议必须走 REST 判定（不吞）。
// 列含义：(content-type 值, 是否判定为 gRPC)
#[test]
fn is_grpc_content_type_table() {
    let cases: &[(&str, bool)] = &[
        ("application/grpc", true),
        ("application/grpc+proto", true),
        ("application/grpc;charset=utf-8", true),
        (" application/grpc ", true),
        ("application/grpc-web", false),
        ("application/grpc-web+proto", false),
        ("application/grpc-web-text", false),
        ("application/grpcfoo", false),
        ("application/json", false),
    ];
    for &(content_type, want) in cases {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            content_type.parse().expect("header value"),
        );
        assert_eq!(is_grpc(&headers), want, "{content_type:?}");
    }
}
