//! TLS 监听端到端：TLS GET 转发与 `X-Forwarded-Proto: https`、mTLS 三档
//! 语义、明文客户端被拒。证书用 rcgen 现签（CA + 叶子，支持 mTLS）。

mod common;

use common::client::request_tls;
use common::upstream::{Behavior, Upstream};
use common::TestDir;

/// 现签一套 CA + server + client 证书，写进 `dir`，返回客户端信任的 CA PEM
/// 与 mTLS 用的客户端 (cert, key) PEM。
struct TestCerts {
    ca_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

fn issue_certs(dir: &TestDir) -> TestCerts {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(vec!["Test CA".to_string()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("self sign ca");

    let server_key = rcgen::KeyPair::generate().expect("server key");
    let server_params =
        rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("server params");
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .expect("sign server");

    let client_key = rcgen::KeyPair::generate().expect("client key");
    let client_params =
        rcgen::CertificateParams::new(vec!["client".to_string()]).expect("client params");
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .expect("sign client");

    let write = |name: &str, pem: String| {
        std::fs::write(dir.path().join(name), &pem).expect("write pem");
    };
    write("ca.pem", ca_cert.pem());
    write("server.pem", server_cert.pem());
    write("server.key", server_key.serialize_pem());

    TestCerts {
        ca_pem: ca_cert.pem(),
        client_cert_pem: client_cert.pem(),
        client_key_pem: client_key.serialize_pem(),
    }
}

/// 带 TLS 块的最小配置。`tls_extra` 追加到 `tls: {}` 映射里
/// （如 `client_auth: required, client_ca: ./ca.pem`）。
fn tls_config(upstream: &Upstream, tls_extra: &str) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\n  tls: {{ enabled: true, cert: ./server.pem, key: ./server.key, {tls_extra} }}\n\
         mgmt: {{ listen: \"127.0.0.1:0\" }}\n\
         routes:\n  - id: api\n    uri: \"http://{}:{}\"\n    predicates: [\"Path=/api/**\"]\n",
        upstream.addr.ip(),
        upstream.addr.port(),
    )
}

/// TLS GET 转发到上游：响应 200，上游看到 `X-Forwarded-Proto: https`
/// （客户端伪造的值被逐跳覆写掉）。
#[tokio::test]
async fn tls_get_forwards_and_reports_https_scheme() {
    let dir = TestDir::new();
    let certs = issue_certs(&dir);
    let upstream = common::start_upstream(Behavior::EchoHeaders).await;
    let config = tls_config(&upstream, "client_auth: none");
    let gateway = common::start(dir.path(), &config).await;

    let resp = request_tls(
        gateway.addr,
        "localhost",
        &certs.ca_pem,
        None,
        "GET",
        "/api/x",
        &[
            ("Host", "localhost"),
            // 伪造入站 scheme：必须被覆写为 https
            ("X-Forwarded-Proto", "http"),
        ],
        common::BodySpec::Empty,
    )
    .await
    .expect("TLS request must succeed");
    assert_eq!(resp.status, 200);
    let echoed: serde_json::Value = serde_json::from_str(&resp.body_text()).expect("echo json");
    assert_eq!(
        echoed["headers"]["x-forwarded-proto"], "https",
        "TLS 接入必须上报 https，伪造值必须被覆写"
    );
    assert_eq!(echoed["headers"]["host"], "localhost");

    gateway.shutdown().await;
}

/// mTLS required：不出示客户端证书的握手必须失败；出示有效客户端证书
/// 则放行。
#[tokio::test]
async fn mtls_required_rejects_anonymous_and_accepts_certified() {
    let dir = TestDir::new();
    let certs = issue_certs(&dir);
    let upstream = common::start_upstream(Behavior::EchoHeaders).await;
    let config = tls_config(&upstream, "client_auth: required, client_ca: ./ca.pem");
    let gateway = common::start(dir.path(), &config).await;

    // 匿名握手必须失败（TLS 1.3 下拒绝可能延迟到读响应时才表现为
    // alert，两种形态都算「被拒」）
    let err = request_tls(
        gateway.addr,
        "localhost",
        &certs.ca_pem,
        None,
        "GET",
        "/api/x",
        &[("Host", "localhost")],
        common::BodySpec::Empty,
    )
    .await
    .expect_err("anonymous handshake must be rejected");
    assert!(
        err.contains("CertificateRequired") || err.contains("TLS handshake"),
        "{err}"
    );

    // 持有效客户端证书放行
    let resp = request_tls(
        gateway.addr,
        "localhost",
        &certs.ca_pem,
        Some((&certs.client_cert_pem, &certs.client_key_pem)),
        "GET",
        "/api/x",
        &[("Host", "localhost")],
        common::BodySpec::Empty,
    )
    .await
    .expect("certified handshake must pass");
    assert_eq!(resp.status, 200);

    gateway.shutdown().await;
}

/// 明文客户端打 TLS 端口：TLS 握手失败、连接被服务端丢弃（读到 EOF），
/// 绝不以明文服务。
#[tokio::test]
async fn plaintext_client_is_rejected_on_tls_port() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = TestDir::new();
    let _certs = issue_certs(&dir);
    let upstream = common::start_upstream(Behavior::EchoHeaders).await;
    let config = tls_config(&upstream, "client_auth: none");
    let gateway = common::start(dir.path(), &config).await;

    let mut stream = tokio::net::TcpStream::connect(gateway.addr)
        .await
        .expect("connect");
    // 明文 HTTP 请求：服务端按 TLS 解析必失败——按 TLS 规范回一条
    // alert 记录（首字节 0x15）后关连接，绝不回明文 HTTP。
    stream
        .write_all(b"GET /api/x HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write");
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.expect("read");
    assert!(n > 0, "应至少收到 TLS alert 记录");
    assert_eq!(
        buf[0],
        0x15,
        "TLS 端口绝不以明文响应，首字节应为 alert 记录: {:?}",
        &buf[..n]
    );

    gateway.shutdown().await;
}
