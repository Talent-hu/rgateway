//! rgateway-proxy：HTTP 上游代理、协议感知错误渲染与 hop-by-hop 头治理。
//!
//! 本 crate 处于数据面核心：请求经 `rgateway-config` 编译出的过滤器链到达这里，
//! 由代理层转发到上游，并把链上产出的错误按线上协议（HTTP / gRPC / gRPC-Web）
//! 渲染回客户端。P3 落地五个模块：
//!
//! * [`framing`] —— gRPC-Web trailer 帧编解码（帧格式：flags + 大端长度 + payload），
//!   文本变体走 base64；
//! * [`error_render`] —— 把 [`rgateway_core::GatewayError`] 按 [`rgateway_core::Protocol`]
//!   渲染成 `http::Response<ProxyBody>`：HTTP 家族走状态码 + JSON，gRPC 家族走
//!   trailers-only 语义；
//! * [`headers`] —— hop-by-hop 头剥离、`X-Request-ID` / `X-Forwarded-*` 代理头治理；
//! * [`upstream`] —— 上游客户端：连接池、出站 URI 组装、请求/响应头治理与
//!   流式透传，hyper 错误链的 `ProxyError` 还原；
//! * [`handler`] —— 协议处理器工厂（[`handler::HttpHandlerFactory`]，实现
//!   `rgateway_config::spec::RouteHandlerFactory`）：scheme / 协议 / 目标三个
//!   准入闸门后转发到上游。
//!
//! # 错误的双通道分工（引用 `rgateway-core` 的 `body.rs` 契约）
//!
//! * **控制面** [`rgateway_core::GatewayError`]：服务链（过滤器链）在响应尚未
//!   开始时返回的错误，由 [`error_render::render`] 完整渲染；对外只走
//!   `public_message()` / `grpc_message()`（泛化静态文案），内部细节（上游地址、
//!   实例 id、io 错误原文）仅进日志通道（`Display` / `Debug`）。
//! * **数据面** [`rgateway_core::ProxyError`]：body 流在转发过程中抛出的错误，
//!   随帧冒泡；此时状态码往往已发出，只能中断流或补写 gRPC trailer，经
//!   `into_gateway_error()` 归一后复用同一渲染映射。

pub mod error_render;
pub mod framing;
pub mod handler;
pub mod headers;
pub mod upstream;

// 常用入口提升到 crate 根：调用方通常直接 `use rgateway_proxy::…` 即可。
pub use error_render::render;
pub use framing::{decode_trailer_frame, encode_trailer_frame, encode_trailer_frame_text};
pub use handler::HttpHandlerFactory;
pub use headers::{finalize_response, prepare_upstream, strip_hop_by_hop};
pub use upstream::UpstreamClient;
