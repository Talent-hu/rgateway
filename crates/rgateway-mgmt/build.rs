//! 构建脚本：把 `proto/mgmt/v1/mgmt.proto` 编译成 prost 消息 + tonic
//! 服务端/客户端骨架（生成物落 `OUT_DIR`，由 `src/pb.rs` 引入）。
//!
//! 走 `tonic-build` 的 `configure()`（内部装配 prost-build 的
//! `service_generator`）：消息结构由 prost 派生，`MgmtServiceServer` /
//! `MgmtServiceClient` 骨架由 tonic 生成器产出，protoc 从 `PATH` 发现。

use std::path::PathBuf;

/// 编译管理面 proto 并生成 Rust 绑定。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_root = manifest_dir.join("../../proto");
    let proto_file = proto_root.join("mgmt/v1/mgmt.proto");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .emit_rerun_if_changed(true)
        .compile_protos(&[proto_file], &[proto_root])?;
    Ok(())
}
