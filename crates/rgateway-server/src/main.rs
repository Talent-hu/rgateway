//! `rgateway` 可执行二进制入口：解析命令行 → [`bootstrap::start`] 装配
//! 网关 → 等 Ctrl-C → 优雅关闭。启动失败的错误（配置坐标、TLS 拒绝闸等）
//! 原样打给运维，进程以非零码退出。

use std::path::PathBuf;

use clap::Parser;

use rgateway_server::bootstrap;

/// rgateway：Rust API 网关（对标 Spring Cloud Gateway）。
#[derive(Parser)]
#[command(name = "rgateway", version, about)]
struct Args {
    /// 配置文件路径。
    #[arg(long, default_value = "config/rgateway.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let gateway = bootstrap::start(&args.config).await?;
    tokio::signal::ctrl_c().await?;
    tracing::info!("ctrl-c received, shutting down");
    gateway.shutdown().await;
    Ok(())
}
