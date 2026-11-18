//! 网关测试装配：直接调生产 [`rgateway_server::bootstrap::start`]。
//!
//! 为什么不再自拼装配：`bootstrap` 模块已落地，加载/校验、TLS 拒绝闸、
//! tracing 初始化、编译快照、热重载 watcher、accept 循环与优雅关闭全部
//! 走生产代码——测试侧只负责把 YAML 写进临时目录、取出实际监听地址，
//! 让端到端用例驱动的就是二进制启动的真实链路。
//!
//! 注意：`bootstrap::start` 恒挂文件监听器（热重载恒开），因此本夹具不再
//! 有 `watch` 开关；配置文件改写即触发热重载，与生产语义一致。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rgateway_server::bootstrap::RunningGateway;

/// 在跑的网关实例；drop 即发起优雅关闭（`RunningGateway` 的字段 drop 语义）。
pub struct TestGateway {
    /// 数据面监听地址（测试客户端连这里）。
    pub addr: SocketAddr,
    /// 管理面监听地址（REST + gRPC 同端口复用）。
    pub mgmt_addr: SocketAddr,
    /// 配置文件路径（reload 用例改写它触发热重载）。
    pub config_path: PathBuf,
    /// 生产运行句柄：优雅关闭走它的 `shutdown()`。
    running: RunningGateway,
}

impl TestGateway {
    /// 优雅关闭（消耗自身）：停止 accept → 在途请求在
    /// `server.timeouts.graceful_shutdown` 窗口内完成 → 收尾后返回。
    pub async fn shutdown(self) {
        let TestGateway { running, .. } = self;
        running.shutdown().await;
    }
}

/// 起一个网关：把 `yaml` 写进 `dir` 的配置文件，经生产 bootstrap 装配
/// 并监听（`server.listen` 建议写 `127.0.0.1:0`，实际端口从返回值取）。
pub async fn start(dir: &Path, yaml: &str) -> TestGateway {
    let config_path = dir.join("rgateway.yaml");
    std::fs::write(&config_path, yaml).expect("write config file");
    let running = rgateway_server::bootstrap::start(&config_path)
        .await
        .expect("gateway must start");
    let addr = running.local_addr;
    let mgmt_addr = running.mgmt_addr;
    TestGateway {
        addr,
        mgmt_addr,
        config_path,
        running,
    }
}
