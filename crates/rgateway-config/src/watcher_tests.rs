//! [`crate::watcher`] 的测试：防抖合并、事件过滤、队列溢出、reload 行为
//! （含真实 [`crate::snapshot::reload`] 管线的端到端用例）。

use std::sync::atomic::{AtomicUsize, Ordering};

use notify::event::ModifyKind;
use tokio::time::sleep;

use super::*;
use crate::compile::{ServerSettings, TlsSettings};
use crate::spec::{FilterRegistry, RouteCompileInfo, RouteHandlerFactory};
use rgateway_core::{
    empty_body, BoxedService, GatewayError, ProxyBody, StateRegistry, StaticDiscovery,
};
use rgateway_router::RouteTable;

/// 系统临时目录下的临时目录，drop 时递归删除。
struct TestDir(PathBuf);

impl TestDir {
    /// 建一个带标签（且全局唯一，靠 uuid）的临时目录。
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rgateway-watcher-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("create test dir");
        Self(path)
    }

    /// 目录内的目标配置文件路径（监听器要盯的文件名）。
    fn config_path(&self) -> PathBuf {
        self.0.join("rgateway.yaml")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 路由 handler stub：watcher 测试从不真正调用它。
struct StubHandler;

impl RouteHandlerFactory for StubHandler {
    fn build(&self, _route: &RouteCompileInfo) -> BoxedService {
        BoxedService::new(tower::service_fn(
            |_req: http::Request<ProxyBody>| async move {
                Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
            },
        ))
    }
}

/// 空的 ReloadContext（stub handler + 空注册表 + 独立状态注册表）。
fn stub_ctx() -> Arc<ReloadContext> {
    Arc::new(ReloadContext {
        registry: FilterRegistry::new(),
        states: Arc::new(StateRegistry::new()),
        handler_factory: Arc::new(StubHandler),
    })
}

/// 手工拼的最小 CompiledConfig（不走编译管线，字段取无害的默认值）。
fn stub_config() -> CompiledConfig {
    CompiledConfig {
        server: ServerSettings {
            listen: "127.0.0.1:0".parse().expect("addr"),
            max_concurrent_streams: 1024,
            initial_stream_window_size: 1024 * 1024,
            initial_connection_window_size: 1024 * 1024,
            max_request_body: 10 * 1024 * 1024,
            max_concurrent_requests: 4096,
            max_retry_buffer: 1024 * 1024,
            connect_timeout: Duration::from_secs(3),
            request_default: Duration::from_secs(30),
            graceful_shutdown: Duration::from_secs(30),
            ws_handshake: Duration::from_secs(10),
            tls: TlsSettings::default(),
        },
        routes: RouteTable::new(Vec::new()),
        discovery: Arc::new(StaticDiscovery::default()),
        states: Arc::new(StateRegistry::new()),
        mgmt_listen: "127.0.0.1:0".parse().expect("addr"),
        mgmt_grpc: false,
        access_log_enabled: false,
        access_log_json: true,
        metrics_enabled: false,
        metrics_path: "/metrics".to_string(),
        tracing_filter: None,
    }
}

/// 装着 stub_config 的 SnapshotStore。
fn stub_store() -> Arc<SnapshotStore> {
    Arc::new(SnapshotStore::new(stub_config()))
}

/// 计数调用次数且永远成功的 reload 回调（返回 stub 快照）。
fn counting_reload(calls: &Arc<AtomicUsize>) -> Arc<ReloadCallback> {
    let calls = Arc::clone(calls);
    Arc::new(move |_s, _c, _y, _d| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(stub_config()))
    })
}

/// 测试用防抖窗口（比默认 300ms 短，缩短等待）。
const TEST_DEBOUNCE: Duration = Duration::from_millis(150);

/// 起一个带计数 reload 的监听器（不走真实编译）。
fn spawn_counting(config: &Path, calls: &Arc<AtomicUsize>) -> ConfigWatcher {
    spawn_watcher_with(
        config,
        stub_store(),
        stub_ctx(),
        TEST_DEBOUNCE,
        counting_reload(calls),
    )
    .expect("spawn watcher")
}

/// 起一个走真实 `crate::snapshot::reload` 管线的监听器。
fn spawn_real(config: &Path, store: &Arc<SnapshotStore>) -> ConfigWatcher {
    spawn_watcher_with_debounce(config, Arc::clone(store), stub_ctx(), TEST_DEBOUNCE)
        .expect("spawn watcher")
}

// 两份可编译的最小 YAML：VALID_YAML_V2 比 VALID_YAML 多一个 order: 1
// 字段，内容不同但都合法，用来触发「真实 reload 成功」的版本号递增。
const VALID_YAML: &str =
    "server:\n  listen: \"127.0.0.1:0\"\nroutes:\n  - id: r1\n    uri: \"http://127.0.0.1:9000\"\n    predicates: [\"Path=/api/**\"]\n";
const VALID_YAML_V2: &str =
    "server:\n  listen: \"127.0.0.1:0\"\nroutes:\n  - id: r1\n    order: 1\n    uri: \"http://127.0.0.1:9000\"\n    predicates: [\"Path=/api/**\"]\n";

/// 验证：监听器搬进 `tokio::spawn` 任务的状态必须保持 `Send + Sync`。
/// 与 watcher.rs 里编译期断言（`const _: fn()`）互为表里，形状对齐
/// rgateway-core route.rs 的静态断言测试。
#[test]
fn spawned_state_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<SnapshotStore>>();
    assert_send_sync::<Arc<ReloadContext>>();
    assert_send_sync::<Arc<ReloadCallback>>();
}

/// 验证：启动时配置文件不存在返回 `ConfigError::Io`（location 为
/// `watcher`），而不是默默起一个永远不工作的监听器。
#[test]
fn missing_file_at_startup_returns_io_error() {
    let dir = TestDir::new("missing");
    let p = dir.config_path();
    let result = spawn_watcher_with_debounce(&p, stub_store(), stub_ctx(), DEFAULT_DEBOUNCE);
    match result {
        Err(ConfigError::Io { location, .. }) => {
            assert_eq!(location.to_string(), "watcher");
        }
        Err(other) => panic!("expected ConfigError::Io, got {other:?}"),
        Ok(_) => panic!("expected ConfigError::Io, got a watcher"),
    }
}

/// 验证：短时间内的写入突发被防抖合并成**恰好一次** reload。
#[tokio::test]
async fn burst_of_writes_triggers_exactly_one_reload() {
    let dir = TestDir::new("burst");
    let config = dir.config_path();
    std::fs::write(&config, "v0").expect("write file");
    let calls = Arc::new(AtomicUsize::new(0));
    let _watcher = spawn_counting(&config, &calls);
    sleep(Duration::from_millis(50)).await;
    for n in 0..3 {
        std::fs::write(&config, format!("v{n}")).expect("write file");
        sleep(Duration::from_millis(7)).await; // 突发跨度 < 20ms，均在安静窗口内
    }
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "burst must coalesce into one reload"
    );
}

/// 验证：其他文件的事件被文件名过滤掉，不触发 reload。
#[tokio::test]
async fn events_for_other_files_are_ignored() {
    let dir = TestDir::new("filter");
    let config = dir.config_path();
    std::fs::write(&config, "v0").expect("write file");
    let calls = Arc::new(AtomicUsize::new(0));
    let _watcher = spawn_counting(&config, &calls);
    sleep(Duration::from_millis(50)).await;
    std::fs::write(dir.0.join("other.txt"), "not the config").expect("write file");
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "unrelated file must not reload"
    );
}

/// 验证：drop 掉句柄即停止监听——之后的写入不再触发 reload。
#[tokio::test]
async fn dropping_the_handle_stops_watching() {
    let dir = TestDir::new("drop");
    let config = dir.config_path();
    std::fs::write(&config, "v0").expect("write file");
    let calls = Arc::new(AtomicUsize::new(0));
    let watcher = spawn_counting(&config, &calls);
    sleep(Duration::from_millis(50)).await;
    drop(watcher);
    std::fs::write(&config, "v1").expect("write file");
    sleep(Duration::from_millis(500)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no reloads after drop");
}

/// 验证：reload 失败时失败计数加一、快照版本不变（旧快照继续生效）。
#[tokio::test]
async fn failed_reload_records_failure_and_keeps_version() {
    let dir = TestDir::new("err");
    let config = dir.config_path();
    std::fs::write(&config, "v0").expect("write file");
    let store = stub_store();
    let version = store.version();
    let reload: Arc<ReloadCallback> =
        Arc::new(|_s, _c, _y, _d| Err(ConfigError::invalid(Location::root(), "boom")));
    let _watcher = spawn_watcher_with(
        &config,
        Arc::clone(&store),
        stub_ctx(),
        TEST_DEBOUNCE,
        reload,
    )
    .expect("spawn watcher");
    sleep(Duration::from_millis(50)).await;
    std::fs::write(&config, "v1").expect("write file");
    sleep(Duration::from_millis(500)).await;
    assert_eq!(store.failed_reloads(), 1);
    assert_eq!(store.version(), version, "failed reload must keep snapshot");
}

// real_reload_* 用真实的 crate::snapshot::reload 管线（编译 + 换快照）。

/// 验证：真实 reload 成功时快照版本号递增（新快照换入）。
#[tokio::test]
async fn real_reload_happy_path_increases_version() {
    let dir = TestDir::new("real-ok");
    let config = dir.config_path();
    std::fs::write(&config, VALID_YAML).expect("write file");
    let store = stub_store();
    let version = store.version();
    let _watcher = spawn_real(&config, &store);
    sleep(Duration::from_millis(50)).await;
    std::fs::write(&config, VALID_YAML_V2).expect("write file");
    sleep(Duration::from_millis(500)).await;
    assert!(
        store.version() > version,
        "successful reload swaps snapshot"
    );
}

/// 验证：坏 YAML 计一次失败且保旧快照，修好后下一次 reload 恢复正常
/// 递增版本号。
#[tokio::test]
async fn real_reload_bad_yaml_counts_failures_then_fixes() {
    let dir = TestDir::new("real-err");
    let config = dir.config_path();
    std::fs::write(&config, VALID_YAML).expect("write file");
    let store = stub_store();
    let version = store.version();
    let _watcher = spawn_real(&config, &store);
    sleep(Duration::from_millis(50)).await;
    std::fs::write(&config, "routes: [oops").expect("write file");
    sleep(Duration::from_millis(500)).await;
    assert_eq!(store.failed_reloads(), 1, "bad YAML must count a failure");
    assert_eq!(store.version(), version, "bad YAML must keep the snapshot");
    std::fs::write(&config, VALID_YAML_V2).expect("write file");
    sleep(Duration::from_millis(500)).await;
    assert!(
        store.version() > version,
        "fixed YAML must bump the version"
    );
}

/// 验证（安全修复点）：队列溢出后 overflow 标记兜底，突发仍触发**恰好
/// 一次** reload——被 try_send 丢掉的最后一个触发不会丢。
#[tokio::test]
async fn queue_overflow_still_triggers_exactly_one_reload() {
    let dir = TestDir::new("overflow");
    let config = dir.config_path();
    std::fs::write(&config, "v0").expect("write file");
    let calls = Arc::new(AtomicUsize::new(0));
    let watcher = spawn_counting(&config, &calls);
    sleep(Duration::from_millis(50)).await;

    // 用「其他文件名」的合成事件灌满有界队列：接收侧按文件名过滤永远
    // 不会认它们，所以最后那一次 reload 只可能来自 overflow 标记路径。
    let other = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(dir.0.join("other.txt"));
    for _ in 0..EVENT_QUEUE_CAP + 16 {
        watcher.tap.push(Ok(other.clone()));
    }
    assert!(
        watcher.tap.overflow.load(Ordering::SeqCst),
        "queue must overflow"
    );

    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "overflow burst must coalesce into exactly one reload"
    );
}
