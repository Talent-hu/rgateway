//! Prometheus 指标——recorder 安装、请求级仪表函数与标签基数红线。
//!
//! # 为什么收口在本 crate
//!
//! 指标名、标签键与计量口径是横切约定，dispatch / 过滤器 / 管理面都会用到。
//! 全部仪表函数收在本模块，调用方只传事实（route、method、status……），
//! 指标命名与标签集合由这里统一决定，避免各处拼出不同口径的同义指标。
//!
//! # 标签基数红线（安全/性能）
//!
//! 标签值只允许来自**有界集合**：route id（配置中的路由集合）、method
//! （经 [`normalize_method`] 白名单归一，域外一律 `"OTHER"`）、status（状态
//! 码集合）、kind（经 [`normalize_kind`] 格式归一）。
//! **严禁**把 path、query、request_id、上游地址等无界值放进标签——每个新
//! 标签值都会在 recorder 里永久生成一条时间序列，无界标签 = 基数爆炸 =
//! 内存放大攻击面（伪造路径即可耗尽内存）。无标签可用的场景（如路由未命中）
//! 使用无标签专用计数器，而不是编造 `route="unmatched"` 之类的假标签值。
//!
//! # 计量口径
//!
//! * `status` 标签是十进制字符串（`"200"` / `"503"`），与 Prometheus 生态
//!   状态标签惯例一致，避免数值/字符串口径漂移；
//! * 请求耗时以**秒**记入直方图（Prometheus 秒制惯例），桶界见
//!   [`DURATION_BUCKETS`]；
//! * 所有指标名以 `rgateway_` 前缀收口，便于多实例聚合与告警规则复用。

use std::time::Duration;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::ObsError;

/// 请求计数器：按 route/method/status 累计完成的请求。
const METRIC_REQUESTS_TOTAL: &str = "rgateway_requests_total";
/// 请求耗时直方图（秒）：按 route/method 分序列。
const METRIC_REQUEST_DURATION_SECONDS: &str = "rgateway_request_duration_seconds";
/// 上游错误计数器：按 route/kind 累计上游交互失败。
const METRIC_UPSTREAM_ERRORS_TOTAL: &str = "rgateway_upstream_errors_total";
/// 活跃请求 gauge：并发中的请求数，请求开始 +1、结束 -1。
const METRIC_ACTIVE_REQUESTS: &str = "rgateway_active_requests";
/// 路由未命中计数器：无标签专用计数器（见 [`record_route_not_found`]）。
const METRIC_ROUTE_NOT_FOUND_TOTAL: &str = "rgateway_route_not_found_total";

/// 请求耗时直方图桶界（秒）。
///
/// 覆盖从 5ms 到 10s 的网关常见延迟区间；按 metric 精确匹配配置（而非全局
/// 桶），避免未来新增的非延迟直方图（如字节数）被套上秒制桶界。
pub const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// 已安装的 Prometheus recorder 句柄。
///
/// [`PrometheusHandle`] 的薄包装：只暴露渲染快照（供 `/metrics` 端点）与
/// 周期维护，不暴露 recorder 本体——防止调用方绕过仪表函数直接写指标、
/// 破坏命名收口。
///
/// # 上游维护契约（必读）
///
/// metrics-exporter-prometheus **不会自动派生 upkeep 任务**：直方图样本
/// 在内存桶里逐条累积，只有 [`Self::render`] 或 [`Self::run_upkeep`] 才会
/// 把样本折叠成分布。若部署方既不抓 `/metrics` 也不跑 upkeep，直方图缓
/// 冲会无界增长。调用方义务二选一：**周期调用 [`Self::run_upkeep`]（建议
/// 10s 一次）**，或保证 `render()` 被定期调用（Prometheus 正常刮取即可）。
#[derive(Debug, Clone)]
pub struct MetricsHandle {
    inner: PrometheusHandle,
}

impl MetricsHandle {
    /// 渲染当前全部指标的 Prometheus 文本（exposition format）。
    ///
    /// 每次调用都是全量快照，调用方（`/metrics` 端点）直接把结果作为响应体
    /// 即可；本方法不失败，快照写入内存缓冲区，不受慢客户端影响。渲染同时
    /// 会清空直方图累积桶（等价 upkeep 效果），正常刮取即天然满足维护义务。
    pub fn render(&self) -> String {
        self.inner.render()
    }

    /// 周期维护：把累积的直方图样本折叠进分布，防止内存无界增长。
    ///
    /// 与 [`Self::render`] 二选一（见类型文档的上游维护契约）；部署方不刮
    /// `/metrics` 时（如指标仅按需开启）必须由后台任务周期调用本方法。
    pub fn run_upkeep(&self) {
        self.inner.run_upkeep();
    }
}

/// 构建带本 crate 桶界配置的 Prometheus recorder 工厂。
///
/// `install()` 与测试共用同一份配置，保证测试断言的桶界/分布形态与生产
/// 渲染一致。失败路径只有桶界为空（本文件的 [`DURATION_BUCKETS`] 为非空
/// 常量，实际不会触发）。
fn recorder_builder() -> Result<PrometheusBuilder, ObsError> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(METRIC_REQUEST_DURATION_SECONDS.to_string()),
            DURATION_BUCKETS,
        )
        .map_err(|e| ObsError::RecorderInstall(e.to_string()))
}

/// 安装全局 Prometheus recorder 并返回渲染句柄（**一次性**）。
///
/// # 一次性语义
///
/// 底层 `metrics::set_global_recorder` 只允许安装一次：**首次调用成功，
/// 重复调用返回 [`ObsError::RecorderInstall`]，且首个 recorder 保持生效**。
/// 不做成幂等是刻意的——进程里出现两个「安装点」是启动编排的 bug，应当在
/// 启动期炸出来而不是悄悄吞掉。调用方（server 启动引导）应在启动时调用
/// 一次，把 [`MetricsHandle`] 交给 `/metrics` 端点渲染。
///
/// # 错误
///
/// 重复安装（或底层构建失败）返回 [`ObsError::RecorderInstall`]；`Display`
/// 文案走日志通道，可携带内部细节但不含密钥。
pub fn install() -> Result<MetricsHandle, ObsError> {
    let handle = recorder_builder()?
        .install_recorder()
        .map_err(|e| ObsError::RecorderInstall(e.to_string()))?;
    Ok(MetricsHandle { inner: handle })
}

/// 归一化 method 标签：白名单内的标准方法原样保留，域外一律 `"OTHER"`。
///
/// HTTP 方法是**客户端可控的线上输入**（自定义方法如 `PURGE`、伪造 token
/// 都可能出现），而标签基数红线（见模块文档）要求标签值来自有界集合——
/// 红线不能只停在文档里，必须在代码处强制：域外值收敛为单一 `"OTHER"`
/// 序列，攻击者无法靠伪造方法名制造无限时间序列。
pub fn normalize_method(method: &str) -> &str {
    match method {
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "PATCH" | "TRACE" => {
            method
        }
        _ => "OTHER",
    }
}

/// 归一化 kind 标签：`[a-z0-9_]{1,64}` 内保留，否则收敛为 `"invalid"`。
///
/// `kind` 理论上来自错误映射表的固定分类，但防御纵深要求本模块不信任
/// 调用方：格式校验把意外的无界文本（错误文案、地址等）挡在标签之外。
pub fn normalize_kind(kind: &str) -> &str {
    let ok = !kind.is_empty()
        && kind.len() <= 64
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        kind
    } else {
        "invalid"
    }
}

/// 记录一次完成的请求：计数器 + 耗时直方图。
///
/// * 计数器 `rgateway_requests_total{route,method,status}`：每请求 +1；
///   `status` 为十进制字符串（`"200"` / `"503"`）。
/// * 直方图 `rgateway_request_duration_seconds{route,method}`：耗时**秒**
///   浮点，桶界 [`DURATION_BUCKETS`]。
///
/// 标签基数红线：`method` 经 [`normalize_method`] 白名单归一（域外
/// `"OTHER"`），`status` 天然有界；**调用方不得把 path/query/request_id 等
/// 无界值塞进 `route` 参数**（见模块文档）。
pub fn record_request(route: &str, method: &str, status: u16, duration: Duration) {
    // 标签值必须是拥有所有权的 String：metrics 0.24 的 SharedString 只接受
    // &'static str 或 String（registry 实测），函数参数的 &str 无法直接进标签。
    let method = normalize_method(method);
    counter!(
        METRIC_REQUESTS_TOTAL,
        "route" => route.to_string(),
        "method" => method.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        METRIC_REQUEST_DURATION_SECONDS,
        "route" => route.to_string(),
        "method" => method.to_string()
    )
    .record(duration.as_secs_f64());
}

/// 记录一次上游交互失败：`rgateway_upstream_errors_total{route,kind}` +1。
///
/// `kind` 是错误分类（如 `connect_timeout`、`reset`、`tls_error`），经
/// [`normalize_kind`] 格式归一后才进标签——不要把上游响应体、异常文案等
/// 无界文本当 kind（会被归一为 `"invalid"`）。
pub fn record_upstream_error(route: &str, kind: &str) {
    counter!(
        METRIC_UPSTREAM_ERRORS_TOTAL,
        "route" => route.to_string(),
        "kind" => normalize_kind(kind).to_string()
    )
    .increment(1);
}

/// 活跃请求 gauge +1：请求进入 dispatch 时调用。
///
/// 与 [`record_active_requests_close`] 严格配对（响应体耗尽/失败收尾处都要
/// 调用），否则 gauge 会持续漂移。gauge 无标签——并发数是实例级标量。
pub fn record_active_requests_open() {
    gauge!(METRIC_ACTIVE_REQUESTS).increment(1.0);
}

/// 活跃请求 gauge -1：请求收尾时调用（与 [`record_active_requests_open`] 配对）。
pub fn record_active_requests_close() {
    gauge!(METRIC_ACTIVE_REQUESTS).decrement(1.0);
}

/// 记录一次路由未命中：`rgateway_route_not_found_total` +1（无标签）。
///
/// # 为什么是无标签专用计数器
///
/// 路由未命中时**没有 route id 可用**；把 path 当标签会触发基数爆炸（见
/// 模块文档的标签红线），编造 `route="unmatched"` 之类的假值又会污染 route
/// 维度的聚合。因此未命中走独立的无标签计数器：与 `rgateway_requests_total`
/// 正交，可直接用于「路由命中率」告警（未命中率 = 本计数器增速 / 总请求增速）。
pub fn record_route_not_found() {
    counter!(METRIC_ROUTE_NOT_FOUND_TOTAL).increment(1);
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
