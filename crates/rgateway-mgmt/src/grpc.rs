//! 管理面 gRPC 服务（与 REST 同端口按 content-type 复用的 gRPC 半边）。
//!
//! # 设计要点（为什么这样设计）
//!
//! * **语义与 REST 完全一致。** 六个 RPC（Health/Snapshot/Routes/Breakers/
//!   ResetBreakers/Stats）复用 [`MgmtState`]（`store()` / `states()`），
//!   输出与 `rest.rs` 各端点的 `data` 一一对应：熔断器按实例 id 排序、
//!   reset 未知路由/实例幂等返回 0、stats 只把 Open 计入打开数。`rest.rs`
//!   是行为基准，本模块的映射辅助函数刻意与之逐行对拍。
//! * **协议骨架来自生成代码。** [`pb`] 的 [`MgmtServiceServer`] 负责方法
//!   路径分发与未知方法的 UNIMPLEMENTED 兜底；本文件只实现 trait、填语义。
//! * **`mgmt.grpc` 开关在本层短路。** 快照 `mgmt.grpc: false` 时任何 gRPC
//!   请求都返回 trailers-only UNIMPLEMENTED 占位（[`unimplemented`]），
//!   不触碰服务实现——开关语义在服务层可测，装配方只做 content-type 分流。
//! * **错误只给泛化文案。** 非法请求（空 route_id）回 `invalid request`，
//!   不回显任何内部细节；边界校验集中在入口，快速失败。

use std::sync::Arc;

use bytes::Bytes;

use rgateway_config::CompiledConfig;
use rgateway_core::{BreakerState, ProtocolHint, Route};

use crate::pb::mgmt_service_server::{MgmtService as MgmtServiceRpc, MgmtServiceServer};
use crate::pb::{
    BreakerEntry, BreakersRequest, BreakersResponse, HealthRequest, HealthResponse,
    ResetBreakersRequest, ResetBreakersResponse, RouteBreakers, RouteSummary, RoutesRequest,
    RoutesResponse, SnapshotRequest, SnapshotResponse, StatsRequest, StatsResponse,
};
use crate::rest::MAX_RESET_ID_LEN;
use crate::state::MgmtState;

/// gRPC 响应体类型：tonic 的装箱 body，REST/gRPC 分流后的统一出口形态
/// （装配方把 REST 的 `MgmtBody` 也折算进来，hyper 侧只面对一种 body）。
pub type GrpcBody = tonic::body::Body;

/// 管理面 gRPC 服务实现：持有 [`MgmtState`]，逐 RPC 读快照与治理状态。
pub struct MgmtService {
    state: Arc<MgmtState>,
}

impl MgmtService {
    /// 构造：注入管理面共享状态（快照存储 + 治理状态注册表）。
    pub fn new(state: Arc<MgmtState>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl MgmtServiceRpc for MgmtService {
    /// GET /health：存活探针，恒返回 `status="ok"`。
    async fn health(
        &self,
        _request: tonic::Request<HealthRequest>,
    ) -> Result<tonic::Response<HealthResponse>, tonic::Status> {
        Ok(tonic::Response::new(HealthResponse {
            status: "ok".to_string(),
        }))
    }

    /// GET /mgmt/v1/snapshot：版本、失败重载数、数据面监听、路由摘要。
    async fn snapshot(
        &self,
        _request: tonic::Request<SnapshotRequest>,
    ) -> Result<tonic::Response<SnapshotResponse>, tonic::Status> {
        let snap = self.state.store().load();
        let routes = route_summaries(&snap);
        let route_count = routes.len();
        Ok(tonic::Response::new(SnapshotResponse {
            version: self.state.store().version(),
            failed_reloads: self.state.store().failed_reloads(),
            listen: snap.server.listen.to_string(),
            route_count: to_u64(route_count),
            routes,
        }))
    }

    /// GET /mgmt/v1/routes：按匹配顺序的路由摘要（与 snapshot 同形态）。
    async fn routes(
        &self,
        _request: tonic::Request<RoutesRequest>,
    ) -> Result<tonic::Response<RoutesResponse>, tonic::Status> {
        let snap = self.state.store().load();
        Ok(tonic::Response::new(RoutesResponse {
            routes: route_summaries(&snap),
        }))
    }

    /// GET /mgmt/v1/breakers：逐路由逐实例熔断状态，实例按 id 排序。
    async fn breakers(
        &self,
        _request: tonic::Request<BreakersRequest>,
    ) -> Result<tonic::Response<BreakersResponse>, tonic::Status> {
        let snap = self.state.store().load();
        let mut routes = Vec::with_capacity(snap.routes.len());
        for route in snap.routes.routes() {
            let mut entries: Vec<BreakerEntry> = self
                .state
                .states()
                .route_state(&route.id)
                .breaker_states()
                .into_iter()
                .map(|(instance_id, state)| BreakerEntry {
                    instance_id: instance_id.to_string(),
                    state: state.as_label().to_string(),
                })
                .collect();
            // DashMap 迭代无序：排序后输出，保证同一状态两次查询得到同一份结果。
            entries.sort_unstable_by(|a, b| a.instance_id.cmp(&b.instance_id));
            routes.push(RouteBreakers {
                route_id: route.id.to_string(),
                breakers: entries,
            });
        }
        Ok(tonic::Response::new(BreakersResponse { routes }))
    }

    /// POST /mgmt/v1/breakers/reset：重置熔断器；未知路由/实例幂等返回 0。
    /// 空或超长的 `route_id` / 超长 `instance_id` 为非法请求（INVALID_ARGUMENT，
    /// 泛化文案）——长度边界与 REST 侧 [`MAX_RESET_ID_LEN`] 对称。
    async fn reset_breakers(
        &self,
        request: tonic::Request<ResetBreakersRequest>,
    ) -> Result<tonic::Response<ResetBreakersResponse>, tonic::Status> {
        let body = request.into_inner();
        if body.route_id.is_empty()
            || body.route_id.len() > MAX_RESET_ID_LEN
            || body
                .instance_id
                .as_deref()
                .is_some_and(|id| id.len() > MAX_RESET_ID_LEN)
        {
            return Err(tonic::Status::invalid_argument("invalid request"));
        }
        let reset = self
            .state
            .states()
            .reset_breaker(&body.route_id, body.instance_id.as_deref());
        Ok(tonic::Response::new(ResetBreakersResponse {
            reset: to_u64(reset),
        }))
    }

    /// GET /mgmt/v1/stats：版本/失败重载/路由数/熔断器（总数、打开数）。
    async fn stats(
        &self,
        _request: tonic::Request<StatsRequest>,
    ) -> Result<tonic::Response<StatsResponse>, tonic::Status> {
        let snap = self.state.store().load();
        let mut breaker_count = 0u64;
        let mut breaker_open_count = 0u64;
        for route in snap.routes.routes() {
            for (_, breaker) in self.state.states().route_state(&route.id).breaker_states() {
                breaker_count += 1;
                // 打开数只统计 Open 态；HalfOpen 是恢复探测，不算「拒绝流量」。
                if breaker == BreakerState::Open {
                    breaker_open_count += 1;
                }
            }
        }
        Ok(tonic::Response::new(StatsResponse {
            version: self.state.store().version(),
            failed_reloads: self.state.store().failed_reloads(),
            route_count: to_u64(snap.routes.len()),
            breaker_count,
            breaker_open_count,
        }))
    }
}

/// 处理一个已由装配方判定为 gRPC 的管理面请求（content-type 分流在
/// rgateway-server 的 bootstrap，REST 入口见 [`crate::handle`]）。
///
/// `mgmt.grpc` 关闭时返回 UNIMPLEMENTED 占位（[`unimplemented`]）；否则
/// 交给生成的 [`MgmtServiceServer`] 分发——未知方法路径同样以
/// UNIMPLEMENTED 应答（tonic 生成代码兜底）。
pub async fn handle<B>(state: Arc<MgmtState>, req: http::Request<B>) -> http::Response<GrpcBody>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
{
    if !state.store().load().mgmt_grpc {
        return unimplemented();
    }
    let mut server = MgmtServiceServer::new(MgmtService::new(state));
    match tonic::codegen::Service::call(&mut server, req).await {
        Ok(resp) => resp,
        // 生成的 Service::Error = Infallible（无实例），match 穷举即可。
        Err(never) => match never {},
    }
}

/// gRPC 未实现占位：HTTP 200 + `grpc-status: 12`（UNIMPLEMENTED）。
///
/// 按 gRPC 协议，服务端错误经 trailers-only 形态表达（HTTP 200 + 状态头），
/// 客户端（grpcurl / tonic）据此报告 UNIMPLEMENTED，而不是把它当传输故障。
/// 响应体经 tonic 的标准 [`tonic::Status::into_http`] 渲染：`grpc-message`
/// percent-encode、带 `Content-Type: application/grpc`，与服务路径的错误
/// 形态完全一致。
pub fn unimplemented() -> http::Response<GrpcBody> {
    tonic::Status::unimplemented("grpc management service not implemented").into_http()
}

/// 路由摘要列表（按路由表匹配顺序）；与 `rest.rs` 的映射逐行对拍。
fn route_summaries(snap: &CompiledConfig) -> Vec<RouteSummary> {
    snap.routes
        .routes()
        .iter()
        .map(|route| route_summary(route))
        .collect()
}

/// 单条路由 → 摘要。
fn route_summary(route: &Route) -> RouteSummary {
    RouteSummary {
        id: route.id.to_string(),
        order: route.order,
        uri: route.uri.as_label(),
        protocol: hint_label(route.protocol_hint).to_string(),
    }
}

/// 协议提示 → 稳定输出标签（`auto` 或协议标签）；与 `rest.rs` 一致。
fn hint_label(hint: ProtocolHint) -> &'static str {
    match hint {
        ProtocolHint::Auto => "auto",
        ProtocolHint::Fixed(protocol) => protocol.as_label(),
    }
}

/// `usize → u64` 饱和转换（计数字段不可能超界，防御性封顶而非截断错位）。
fn to_u64(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "grpc_tests.rs"]
mod tests;
