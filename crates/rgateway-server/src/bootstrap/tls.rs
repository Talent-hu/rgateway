//! TLS 监听装配：把 [`TlsSettings`] 构建为 rustls [`ServerConfig`] /
//! [`TlsAcceptor`]。
//!
//! 设计要点：
//!
//! * **绝不降级明文**：`tls.enabled = true` 而材料缺失/非法时启动失败
//!   （`anyhow` 错误打给运维），绝不静默以明文监听——降级把「以为加密」
//!   的流量裸奔上线，比拒绝启动危险得多。
//! * **显式 ring provider**：workspace 的 rustls 是 `default-features = false`
//!   + `ring`，不显式给 [`CryptoProvider`] 会缺默认 provider 起不来。
//! * **ALPN = `[h2, http/1.1]`**：hyper auto 按首字节探测协议而非 ALPN，
//!   TLS 包装后连接层零改动；ALPN 只为让客户端（curl --http2、gRPC）愿意
//!   升到 h2。
//! * **mTLS 三档**：`none` 不要求客户端证书；`optional` 允许匿名但有证
//!   必验；`required` 必须持有效客户端证书（`client_ca` 为信任锚）。

use std::path::Path;
use std::sync::Arc;

use rgateway_config::compile::TlsSettings;
use rgateway_config::schema::ClientAuth;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use tokio_rustls::TlsAcceptor;

/// 按 TLS 设置构建 acceptor：`enabled: false` → `None`（明文监听）；
/// `enabled: true` → 构建失败即启动失败。
pub(super) fn build_acceptor(settings: &TlsSettings) -> anyhow::Result<Option<TlsAcceptor>> {
    if !settings.enabled {
        return Ok(None);
    }
    Ok(Some(TlsAcceptor::from(build_server_config(settings)?)))
}

/// 构建 rustls 服务端配置：证书链 + 私钥 + 客户端证书校验策略 + ALPN。
pub(super) fn build_server_config(
    settings: &TlsSettings,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let certs = load_certs(&settings.cert)?;
    let key = load_key(&settings.key)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("TLS protocol versions: {e}"))?;
    let builder = match settings.client_auth {
        ClientAuth::None => builder.with_no_client_auth(),
        ClientAuth::Optional | ClientAuth::Required => {
            let roots = load_roots(settings.client_ca.as_deref())?;
            let verifier = WebPkiClientVerifier::builder(roots);
            let verifier = match settings.client_auth {
                ClientAuth::Optional => verifier.allow_unauthenticated(),
                _ => verifier,
            };
            let verifier = verifier
                .build()
                .map_err(|e| anyhow::anyhow!("client cert verifier: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = builder
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("server certificate/key rejected: {e}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// 读 PEM 证书链（至少一张）。
fn load_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let data = read_material(path, "certificate")?;
    let certs = rustls_pemfile::certs(&mut data.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("certificate {path:?} is not valid PEM: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("certificate {path:?} contains no certificates");
    }
    Ok(certs)
}

/// 读 PEM 私钥（PKCS#8 / PKCS#1 / SEC1 均可）。
fn load_key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    let data = read_material(path, "private key")?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .map_err(|e| anyhow::anyhow!("private key {path:?} is not valid PEM: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("private key {path:?} contains no key"))
}

/// 读 `client_ca` 证书（可多张）构建信任锚；mTLS 两档都必须有 CA。
fn load_roots(path: Option<&Path>) -> anyhow::Result<Arc<RootCertStore>> {
    let path = path.ok_or_else(|| {
        anyhow::anyhow!("tls.client_auth requires tls.client_ca to verify client certificates")
    })?;
    let data = read_material(path, "client CA")?;
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut data.as_slice()) {
        let cert = cert.map_err(|e| anyhow::anyhow!("client CA {path:?} is not valid PEM: {e}"))?;
        roots
            .add(cert)
            .map_err(|e| anyhow::anyhow!("client CA {path:?} rejected: {e}"))?;
    }
    Ok(Arc::new(roots))
}

/// 读 TLS 材料文件字节；读失败点名材料种类与路径（运维启动诊断）。
fn read_material(path: &Path, what: &str) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read {what} {path:?}: {e}"))
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
