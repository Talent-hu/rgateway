//! server / TLS 设置解析：compact 时长与大小字符串 → typed 值。
//!
//! 职责：把 `server:` 段（HTTP/2 窗口、limits、timeouts、TLS）从 schema 的
//! 字符串形态解析为 [`ServerSettings`]。设计要点：
//! - 每个解析失败都携带 `server.*` 前缀的 [`Location`]，指向出错的具体
//!   字段，大规模 YAML 排错时可直接定位。
//! - 解析是防御性的：管线中 [`crate::validate::validate`] 先行，但 compile
//!   对坏输入仍必须干净地失败（带位置的 [`ConfigError`]），不能 panic。
//! - compact 语法由 [`crate::dsl`] 解析：大小如 `"512KB"`/`"1MiB"`，时长如
//!   `"500ms"`/`"1.5s"`；时长必须 > 0（`"0s"` 视为配置错误）。
//! - TLS 材料路径相对「配置文件所在目录」解析，且必须留在该目录之内
//!   （绝对路径与 `..` 逃逸都会被拒绝），见 [`resolve_in_dir`]。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::dsl;
use crate::error::{ConfigError, Location};
use crate::schema::{ServerConfig, TlsConfig};

use super::{parse_listen, ServerSettings, TlsSettings};

/// 把 `server:` 段解析为 typed 的 [`ServerSettings`]。
///
/// 字段映射：`listen` 解析为 socket 地址；`http2.*`/`limits.*` 中的 compact
/// 大小解析为字节数；`timeouts.*` 中的 compact 时长解析为 [`Duration`]；
/// `tls` 走 [`parse_tls`] 做路径解析。
///
/// # 错误
/// 任一字段非法时返回 [`ConfigError::Value`]，`Location` 精确到该字段
/// （如 `server.timeouts.connect`）。
pub(super) fn parse_server(
    server: &ServerConfig,
    config_dir: &Path,
) -> Result<ServerSettings, ConfigError> {
    let base = Location::section("server");
    Ok(ServerSettings {
        listen: parse_listen(&base.field("listen"), &server.listen)?,
        max_concurrent_streams: server.http2.max_concurrent_streams,
        initial_stream_window_size: parse_size_at(
            &base.field("http2.initial_stream_window_size"),
            &server.http2.initial_stream_window_size,
        )?,
        initial_connection_window_size: parse_size_at(
            &base.field("http2.initial_connection_window_size"),
            &server.http2.initial_connection_window_size,
        )?,
        max_request_body: parse_size_at(
            &base.field("limits.max_request_body"),
            &server.limits.max_request_body,
        )?,
        max_concurrent_requests: server.limits.max_concurrent_requests,
        max_retry_buffer: parse_size_at(
            &base.field("limits.max_retry_buffer"),
            &server.limits.max_retry_buffer,
        )?,
        connect_timeout: parse_duration_at(
            &base.field("timeouts.connect"),
            &server.timeouts.connect,
        )?,
        request_default: parse_duration_at(
            &base.field("timeouts.request_default"),
            &server.timeouts.request_default,
        )?,
        graceful_shutdown: parse_duration_at(
            &base.field("timeouts.graceful_shutdown"),
            &server.timeouts.graceful_shutdown,
        )?,
        ws_handshake: parse_duration_at(
            &base.field("timeouts.ws_handshake"),
            &server.timeouts.ws_handshake,
        )?,
        tls: parse_tls(&server.tls, config_dir)?,
    })
}

/// 解析 compact 大小（如 `"1MB"`/`"512KB"`）为字节数；
/// 失败时错误带上给定的 [`Location`]。
pub(super) fn parse_size_at(loc: &Location, raw: &str) -> Result<usize, ConfigError> {
    dsl::parse_size(raw).map_err(|m| ConfigError::value(loc.clone(), m))
}

/// 解析 compact 时长（如 `"3s"`/`"500ms"`）为 [`Duration`]；
/// 失败或时长为 0 时错误带上给定的 [`Location`]（0 时长无意义，直接拒绝）。
pub(super) fn parse_duration_at(loc: &Location, raw: &str) -> Result<Duration, ConfigError> {
    let duration = dsl::parse_duration(raw).map_err(|m| ConfigError::value(loc.clone(), m))?;
    if duration.is_zero() {
        return Err(ConfigError::value(loc.clone(), "duration must be > 0"));
    }
    Ok(duration)
}

/// 解析 TLS 材料路径，相对配置目录解析为绝对路径。
///
/// 绝对路径引用或逃出配置目录的引用（`..`）都会被拒绝——配置不应能把
/// 网关引向任意外部文件（防配置注入读取任意文件）。
///
/// # 错误
/// 引用非法时返回 [`ConfigError`]，位置即该引用文本。
fn parse_tls(cfg: &TlsConfig, config_dir: &Path) -> Result<TlsSettings, ConfigError> {
    Ok(TlsSettings {
        enabled: cfg.enabled,
        cert: resolve_or_empty(config_dir, &cfg.cert)?,
        key: resolve_or_empty(config_dir, &cfg.key)?,
        client_auth: cfg.client_auth,
        client_ca: if cfg.client_ca.is_empty() {
            None
        } else {
            Some(resolve_in_dir(config_dir, &cfg.client_ca)?)
        },
    })
}

/// 把相对配置**目录**的文件引用解析为绝对路径
/// （[`CompileInput::config_dir`](super::CompileInput)）。
///
/// [`crate::loader::resolve_path`] 接收的是配置**文件**路径并以其父目录为
/// 基准解析，因此这里拼一个合成文件名 `rgateway.yaml` 来复用 loader 的
/// 解析规则：引用必须留在配置目录之内——绝对引用与 `..` 逃逸都会被拒绝。
pub(super) fn resolve_in_dir(config_dir: &Path, relative: &str) -> Result<PathBuf, ConfigError> {
    crate::loader::resolve_path(&config_dir.join("rgateway.yaml"), relative)
}

/// 空字符串表示「未配置」，保持为空 `PathBuf`；非空才解析为路径（同样受
/// 配置目录约束）。
fn resolve_or_empty(config_dir: &Path, raw: &str) -> Result<PathBuf, ConfigError> {
    if raw.is_empty() {
        Ok(PathBuf::new())
    } else {
        resolve_in_dir(config_dir, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：目录内引用（含 `./`、`a/../b` 归一化）可解析；
    /// `..` 逃逸、Unix 绝对路径、Windows 盘符绝对路径必须被拒绝。
    #[test]
    fn resolve_in_dir_containment_table() {
        let dir = Path::new("conf");
        for input in ["tls/a.pem", "./a.pem", "a/../b.pem"] {
            assert!(resolve_in_dir(dir, input).is_ok(), "{input:?} must resolve");
        }
        for input in ["../a.pem", "/etc/passwd", "C:/x.pem"] {
            assert!(
                resolve_in_dir(dir, input).is_err(),
                "{input:?} must be rejected"
            );
        }
    }

    /// 验证：空字符串按「未配置」处理，保持为空 PathBuf 而不是解析出路径。
    #[test]
    fn resolve_or_empty_keeps_unset_empty() {
        let got = resolve_or_empty(Path::new("conf"), "").expect("empty is unset");
        assert_eq!(got, PathBuf::new());
    }

    /// 验证：TLS 材料引用逃出配置目录时以 `ConfigError::Value` 拒绝，
    /// 且错误位置就是该引用文本本身。
    #[test]
    fn parse_tls_rejects_escaping_material() {
        let tls = TlsConfig {
            cert: "../evil.pem".to_string(),
            ..TlsConfig::default()
        };
        let err = parse_tls(&tls, Path::new("conf")).expect_err("escape must be rejected");
        match err {
            ConfigError::Value { location, .. } => {
                assert_eq!(location.to_string(), "../evil.pem");
            }
            other => panic!("expected ConfigError::Value, got {other:?}"),
        }
    }
}
