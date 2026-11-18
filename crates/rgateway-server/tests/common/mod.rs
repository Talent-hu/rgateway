//! 跨 crate 端到端集成测试的公共夹具入口。
//!
//! 组成：[`TestDir`]（临时配置目录）、[`client`]（手写 HTTP/1.1 客户端）、
//! [`upstream`]（可编程测试上游）、[`gateway`]（生产 `bootstrap::start`
//! 装配——tracing/TLS 闸/热重载/watcher 全走生产代码）。
//!
//! 网络 I/O 用例全部走真实时间；每个用例独立起自己的上游与网关
//! （`127.0.0.1:0`），不依赖执行顺序。
//
// 夹具被三个测试 crate 各自编译一份，每个 crate 只用到它的一个子集，
// 因此这里整体放行 dead_code / unused_imports，避免跨 crate 共享代码
// 触发与真实问题无关的告警。
#![allow(dead_code, unused_imports)]

use std::path::{Path, PathBuf};

pub mod client;
pub mod gateway;
pub mod upstream;

pub use client::{request, BodySpec, RawResponse};
pub use gateway::{start, TestGateway};
pub use upstream::{start as start_upstream, Behavior, SlowStreamUpstream, Upstream};

/// 泄露检测哨兵：混进错误的内部细节载荷，客户端可见通道绝不允许出现。
pub const SECRET_DETAIL: &str = "SECRET_DETAIL";

/// 一次性配置目录（drop 时清理），每个用例独立一份。
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// 在系统临时目录下建一个唯一命名的空目录。
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rgateway-it-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("create temp config dir");
        Self { path }
    }

    /// 目录路径（配置文件写在它下面）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl Default for TestDir {
    fn default() -> Self {
        Self::new()
    }
}

/// 断言响应的客户端可见通道（body + 全部头值）不含任何内部细节。
///
/// `needles` 是内部细节探针（`SECRET_DETAIL` 哨兵、上游地址、内部措辞等）；
/// 逐个对 body 与每个头值做包含断言，命中即泄露。
pub fn assert_no_leak(resp: &RawResponse, needles: &[&str]) {
    let body = resp.body_text();
    for needle in needles {
        assert!(!body.contains(needle), "body leaked {needle:?}: {body}");
        for (name, value) in &resp.headers {
            assert!(
                !value.contains(needle),
                "header {name} leaked {needle:?}: {value}"
            );
        }
    }
}

/// 组一份最小配置：`server.listen` 与 `mgmt.listen` 固定 `127.0.0.1:0`
/// （bootstrap 绑定实际端口后由 `TestGateway.addr` 暴露，并行用例不撞端口），
/// `routes_yaml` 是路由条目文本。
pub fn config_with_routes(routes_yaml: &str) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\nmgmt: {{ listen: \"127.0.0.1:0\" }}\nroutes:\n{routes_yaml}"
    )
}

/// 同 [`config_with_routes`]，但附加 `server.limits.max_request_body` 上限
/// （紧凑大小写法，如 `"1KB"`）。
pub fn config_with_limits(routes_yaml: &str, max_request_body: &str) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\n  limits:\n    max_request_body: \"{max_request_body}\"\nmgmt: {{ listen: \"127.0.0.1:0\" }}\nroutes:\n{routes_yaml}"
    )
}

/// 同 [`config_with_routes`]，但附加 `server.limits.max_concurrent_requests`
/// 并发上限（并发闸用例）。
pub fn config_with_concurrency(routes_yaml: &str, max_concurrent_requests: u32) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\n  limits:\n    max_concurrent_requests: {max_concurrent_requests}\nmgmt: {{ listen: \"127.0.0.1:0\" }}\nroutes:\n{routes_yaml}"
    )
}

/// 同 [`config_with_routes`]，但附加 `server.timeouts.graceful_shutdown`
/// 优雅关闭窗口（紧凑时长，如 `"5s"`）。
pub fn config_with_graceful(routes_yaml: &str, window: &str) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\n  timeouts:\n    graceful_shutdown: \"{window}\"\nmgmt: {{ listen: \"127.0.0.1:0\" }}\nroutes:\n{routes_yaml}"
    )
}

/// 生成一条路由 YAML 条目：Path 谓词 + 可选 Method 谓词 + 可选超时预算。
///
/// `target` 形如 `http://127.0.0.1:8080` 或 `lb://echo-svc`。
pub fn route_entry(
    id: &str,
    pattern: &str,
    target: &str,
    method: Option<&str>,
    timeout: Option<&str>,
) -> String {
    let mut predicates = format!("\"Path={pattern}\"");
    if let Some(m) = method {
        predicates.push_str(&format!(", \"Method={m}\""));
    }
    let mut entry =
        format!("  - id: {id}\n    uri: \"{target}\"\n    predicates: [{predicates}]\n");
    if let Some(t) = timeout {
        entry.push_str(&format!("    request_timeout: \"{t}\"\n"));
    }
    entry
}

/// 生成一条带固定协议提示的路由 YAML 条目：`protocol` 取 `"auto"` /
/// `"rest-json"` 等白名单名（协议闸用例）。
pub fn route_entry_with_protocol(id: &str, pattern: &str, target: &str, protocol: &str) -> String {
    format!(
        "  - id: {id}\n    uri: \"{target}\"\n    protocol: {protocol}\n    predicates: [\"Path={pattern}\"]\n"
    )
}
