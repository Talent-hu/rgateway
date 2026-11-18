//! 文件监听 + 防抖（debounce）：配置热重载的触发源。
//!
//! # 设计要点（为什么这样设计）
//!
//! * **监听父目录（NonRecursive）+ 文件名过滤，而不是监听文件本身。**
//!   Windows 上的编辑器保存常是「写临时文件 + rename 替换」或直接替换
//!   文件：对旧文件 inode/句柄的 watch 会随替换失效、丢掉后续事件。
//!   监听**父目录**能捕获替换动作，再按文件名过滤出目标配置文件的事件。
//! * **防抖合并。** 编辑器一次保存往往产生一串事件（写入、改名、元数据
//!   变更……）。事件先攒着，安静窗口（默认 300ms）内没有新事件才触发
//!   **恰好一次** reload；窗口内每来一个相关事件就顺延窗口。
//! * **有界队列 + 溢出标记（安全修复点）。** notify 回调跑在 notify 自己
//!   的线程上，**绝不能阻塞**。队列满时 `try_send` 丢弃事件但置位
//!   overflow 标记，防抖泵把标记当成一个事件处理——突发风暴里被丢的
//!   最后一个触发也绝不会丢（最多多做一次 reload）。
//! * **reload 失败保旧。** 读文件/编译失败时旧快照继续生效，错误完整
//!   记日志，并在 store 上计一次失败。
//! * **无需 `LocalSet`。** 历史背景：曾因 [`BoxedService`] 非 `Sync`、
//!   泛型状态需要 `spawn_local` + `LocalSet` 跑防抖泵；`Route::chain` 改为
//!   `Mutex` 包装后，[`SnapshotStore`] 与 [`ReloadContext`] 都是
//!   `Send + Sync`，现在就是普通的 `tokio::spawn`（下方编译期断言守护
//!   这一性质，谁改回线程本地状态都编译不过）。
//! * **`Drop` 即停。** [`ConfigWatcher`] 被丢弃时 abort 防抖任务，notify
//!   watcher 随字段一起 drop，监听随之停止。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::compile::CompiledConfig;
use crate::error::{ConfigError, Location};
use crate::snapshot::{ReloadContext, SnapshotStore};

/// 防抖窗口：合并事件突发的安静期时长。
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// notify 事件队列的容量上限；溢出时丢事件、置 overflow 标记，而不是
/// 无界增长队列（notify 回调绝不能被背压阻塞）。
const EVENT_QUEUE_CAP: usize = 1024;

/// reload 回调签名：YAML 文本 + 配置目录进去，新编译的快照出来。默认是
/// [`crate::snapshot::reload`]（成功时已把新快照换入 store）。
type ReloadCallback = dyn Fn(&SnapshotStore, &ReloadContext, &str, &Path) -> Result<Arc<CompiledConfig>, ConfigError>
    + Send
    + Sync;

/// 编译期守护：防抖泵跨 `tokio::spawn` 边界携带的一切必须 `Send + Sync`。
/// 任何一项退化回线程本地状态，这一行就编译不过，而不是悄悄把运行时
/// 逼回 `LocalSet`。
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<SnapshotStore>>();
    assert_send_sync::<Arc<ReloadContext>>();
    assert_send_sync::<Arc<ReloadCallback>>();
    assert_send_sync::<Arc<AtomicBool>>();
};

/// 句柄：保活底层的 notify watcher 与防抖任务；drop 即停止监听。
pub struct ConfigWatcher {
    pub config_path: PathBuf,
    _watcher: notify::RecommendedWatcher,
    task: Option<JoinHandle<()>>,
    /// 测试专用钩子：事件队列的发送端，让测试能模拟 notify 回调（含队列
    /// 溢出），而不必真的碰磁盘。
    #[cfg(test)]
    tap: EventSink,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        // abort 防抖泵；_watcher 随后 drop，notify 停止监听。
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// 开始监听 `config_path`；任何相关变更都通过共享的 [`ReloadContext`]
/// 触发 [`crate::snapshot::reload`]。需要一个已运行的 tokio runtime
/// （任意 flavor 都行；防抖泵是普通 spawn 的任务）。
pub fn spawn_watcher(
    config_path: &Path,
    store: Arc<SnapshotStore>,
    ctx: Arc<ReloadContext>,
) -> Result<ConfigWatcher, ConfigError> {
    spawn_watcher_with_debounce(config_path, store, ctx, DEFAULT_DEBOUNCE)
}

/// 同 [`spawn_watcher`]，但可指定防抖窗口（测试用）。
pub fn spawn_watcher_with_debounce(
    config_path: &Path,
    store: Arc<SnapshotStore>,
    ctx: Arc<ReloadContext>,
    debounce: Duration,
) -> Result<ConfigWatcher, ConfigError> {
    let reload: Arc<ReloadCallback> = Arc::new(crate::snapshot::reload);
    spawn_watcher_with(config_path, store, ctx, debounce, reload)
}

/// 有界事件队列的发送端，notify 回调与测试 tap 共用。
/// [`EventSink::push`] **从不阻塞**：队列满时丢事件、置 overflow 标记，
/// 防抖泵仍会在突发之后恰好跑一次 reload（最后一个触发不丢）。
#[derive(Clone)]
struct EventSink {
    tx: mpsc::Sender<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
}

impl EventSink {
    /// 非阻塞推送。队列满：丢弃 `event` 并置 overflow 标记；队列已关
    /// （防抖任务已死）：静默丢弃。
    fn push(&self, event: notify::Result<Event>) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(event) {
            self.overflow.store(true, Ordering::SeqCst);
        }
    }
}

/// 核心构造函数，可注入 reload 回调。测试注入 stub，让行为检查不依赖
/// [`crate::compile::compile`]。
fn spawn_watcher_with(
    config_path: &Path,
    store: Arc<SnapshotStore>,
    ctx: Arc<ReloadContext>,
    debounce: Duration,
    reload: Arc<ReloadCallback>,
) -> Result<ConfigWatcher, ConfigError> {
    // 启动时配置文件缺失是硬错误（以 ConfigError::Io 冒出）。
    std::fs::metadata(config_path).map_err(|e| ConfigError::io(Location::section("watcher"), e))?;
    if config_path.file_name().is_none() {
        return Err(watcher_error("config path must name a file"));
    }
    // 监听的是父目录（NonRecursive）：Windows 编辑器替换保存的原因，见
    // 模块文档。
    let watch_dir = watch_dir_for(config_path);

    let (tx, rx) = mpsc::channel(EVENT_QUEUE_CAP);
    let sink = EventSink {
        tx,
        overflow: Arc::new(AtomicBool::new(false)),
    };
    let cb_sink = sink.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        // 绝不阻塞：队列满就置 overflow 标记（见 EventSink）。
        cb_sink.push(res);
    })
    .map_err(|e| watcher_error(format!("failed to create watcher: {e}")))?;
    watcher
        .watch(watch_dir, RecursiveMode::NonRecursive)
        .map_err(|e| watcher_error(format!("failed to watch {}: {e}", watch_dir.display())))?;

    // 不在 tokio runtime 里就干净地报错，而不是让 spawn 内部 panic。
    tokio::runtime::Handle::try_current()
        .map_err(|_| watcher_error("spawn_watcher requires a running tokio runtime"))?;
    // `Arc<SnapshotStore>` 与 `Arc<ReloadContext>` 都是 `Send + Sync`
    // （见上面的静态断言），所以防抖泵就是普通 spawn 的任务。
    let path = config_path.to_path_buf();
    let overflow = Arc::clone(&sink.overflow);
    let task = tokio::spawn(debounce_loop(
        rx, path, store, ctx, debounce, reload, overflow,
    ));
    Ok(ConfigWatcher {
        config_path: config_path.to_path_buf(),
        _watcher: watcher,
        task: Some(task),
        #[cfg(test)]
        tap: sink,
    })
}

/// 要监听的目录：配置文件的父目录。裸文件名（父目录为空）时监听当前
/// 目录。
fn watch_dir_for(config_path: &Path) -> &Path {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// notify 错误映射为 [`ConfigError`]，location 为 `watcher`。
fn watcher_error(message: impl Into<String>) -> ConfigError {
    ConfigError::invalid(Location::section("watcher"), message)
}

/// 事件泵：把事件突发合并成「每个安静窗口恰好一次 reload」。
///
/// overflow 标记被当作一个事件对待；它在 reload **之前**（而非之后）
/// 清零——reload 执行期间新产生的溢出会重新置位标记、强制再跑一轮，
/// 保证任何突发的最后一个触发都绝不会被吞掉。
async fn debounce_loop(
    mut rx: mpsc::Receiver<notify::Result<Event>>,
    config_path: PathBuf,
    store: Arc<SnapshotStore>,
    ctx: Arc<ReloadContext>,
    debounce: Duration,
    reload: Arc<ReloadCallback>,
    overflow: Arc<AtomicBool>,
) {
    // `spawn_watcher_with` 已校验过文件名；这里的空回退永远不会匹配任何
    // 事件，退化成「不重载」而不是 panic。
    let file_name = config_path.file_name().unwrap_or_else(|| OsStr::new(""));
    while wait_for_change(&mut rx, file_name, &overflow).await {
        drain_until_quiet(&mut rx, file_name, debounce, &overflow).await;
        // 先清 overflow 再 reload：reload 期间的溢出会重新置位，
        // 下一轮 wait_for_change 立刻再触发。
        overflow.store(false, Ordering::SeqCst);
        trigger_reload(&config_path, &store, &ctx, &*reload);
    }
}

/// 等下一个触及配置文件名的事件，或 overflow 标记（被丢的事件也算一次
/// 变更）。事件通道关闭（watcher 已 drop）时返回 `false`。
async fn wait_for_change(
    rx: &mut mpsc::Receiver<notify::Result<Event>>,
    file_name: &OsStr,
    overflow: &AtomicBool,
) -> bool {
    loop {
        // 先看标记再收事件：两条路径都可能携带「有变更」的信息。
        if overflow.swap(false, Ordering::SeqCst) {
            return true;
        }
        match rx.recv().await {
            Some(Ok(event)) if touches_config(&event, file_name) => return true,
            Some(Ok(_)) => {}
            Some(Err(err)) => tracing::warn!(error = %err, "config watcher event error"),
            None => return false,
        }
    }
}

/// 排空队列，直到 `debounce` 时长内没有新的相关事件。overflow 标记也算
/// 一个：被丢的事件同样要顺延安静窗口。
async fn drain_until_quiet(
    rx: &mut mpsc::Receiver<notify::Result<Event>>,
    file_name: &OsStr,
    debounce: Duration,
    overflow: &AtomicBool,
) {
    let mut deadline = Instant::now() + debounce;
    loop {
        if overflow.swap(false, Ordering::SeqCst) {
            deadline = Instant::now() + debounce;
        }
        // select 同时等「安静期满」与「下一个事件」：事件把 deadline
        // 顺延，期满则结束合并。
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            result = rx.recv() => match result {
                Some(Ok(event)) if touches_config(&event, file_name) => {
                    deadline = Instant::now() + debounce;
                }
                Some(_) => {}
                None => return,
            },
        }
    }
}

/// 事件是否触及配置文件名。替换文件的编辑器也会为临时名发事件：逐个
/// 检查事件里的每条路径，只有携带配置文件名的才转发。读（Access）事件
/// 被跳过——reload 自己要读配置文件，不跳过会自我触发无限循环。
fn touches_config(event: &Event, file_name: &OsStr) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|p| p.file_name() == Some(file_name))
}

/// 读配置文件并跑 reload 回调。失败时旧快照继续生效，错误完整记日志，
/// 并在 store 上计一次失败。
fn trigger_reload(
    config_path: &Path,
    store: &SnapshotStore,
    ctx: &ReloadContext,
    reload: &ReloadCallback,
) {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new(""));
    // 与 loader 同款有界读取：热重载路径同样受 MAX_CONFIG_BYTES 约束，
    // 防止超大文件被整个读进内存（安全加固点）。
    let outcome = crate::loader::read_capped(config_path, crate::loader::MAX_CONFIG_BYTES)
        .and_then(|yaml| reload(store, ctx, &yaml, config_dir));
    match outcome {
        Ok(_) => tracing::info!(
            version = store.version(),
            path = %config_path.display(),
            "config reloaded"
        ),
        Err(err) => {
            tracing::error!(
                error = %err,
                path = %config_path.display(),
                "config reload failed; keeping previous snapshot"
            );
            store.record_reload_failure();
        }
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
