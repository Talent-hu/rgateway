//! prost/tonic 生成的管理面协议绑定（[`crate`] 根 `proto/mgmt/v1/mgmt.proto`
//! 经 `build.rs` 编译产出）。
//!
//! 生成代码整体引入、只消费不修饰：消息结构、`mgmt_service_server`（服务端
//! 骨架与 [`MgmtService`](mgmt_service_server::MgmtService) trait）与
//! `mgmt_service_client`（测试/面板用客户端）都来自工具产出，手写的服务
//! 实现在 [`crate::grpc`]。风格类 lint 对生成代码豁免。
#![allow(clippy::all)]

tonic::include_proto!("rgateway.mgmt.v1");
