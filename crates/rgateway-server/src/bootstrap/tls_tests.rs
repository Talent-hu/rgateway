//! TLS 装配测试：三档 client_auth、坏/缺材料、ALPN 与「绝不降级」语义。
//! 测试证书用 rcgen 现签现用（不落盘静态 PEM，不过期）。

use std::path::{Path, PathBuf};

use rgateway_config::compile::TlsSettings;
use rgateway_config::schema::ClientAuth;

use super::*;

/// rcgen 现签的自签证书对，写进独立临时目录，返回 (目录, cert 路径, key 路径)。
fn self_signed_material() -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rgw-tls-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("self sign");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).expect("write key");
    (dir, cert_path, key_path)
}

/// 现签一个 CA 与它签发的 server/client 证书，全部写进 `dir`。
/// 返回 (ca 路径, server cert, server key, client cert, client key)。
fn ca_material(dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
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

    let write = |name: &str, pem: String| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, pem).expect("write pem");
        path
    };
    (
        write("ca.pem", ca_cert.pem()),
        write("server.pem", server_cert.pem()),
        write("server.key", server_key.serialize_pem()),
        write("client.pem", client_cert.pem()),
        write("client.key", client_key.serialize_pem()),
    )
}

fn tls_settings(cert: &Path, key: &Path) -> TlsSettings {
    TlsSettings {
        enabled: true,
        cert: cert.to_path_buf(),
        key: key.to_path_buf(),
        client_auth: ClientAuth::None,
        client_ca: None,
    }
}

/// `enabled: false` 返回 None（明文监听）；材料合法时构建成功且 ALPN
/// 为 [h2, http/1.1]。
#[test]
fn disabled_is_none_and_valid_material_builds() {
    let disabled = TlsSettings {
        enabled: false,
        ..TlsSettings::default()
    };
    assert!(build_acceptor(&disabled).expect("disabled must be ok").is_none());

    let (dir, cert, key) = self_signed_material();
    let config = build_server_config(&tls_settings(&cert, &key)).expect("valid material must build");
    assert_eq!(config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    let _ = std::fs::remove_dir_all(dir);
}

/// 坏/缺材料必须报错——「绝不降级明文」：enabled=true 而构建失败即
/// 启动失败，错误点名出问题的材料。
#[test]
fn bad_or_missing_material_is_error() {
    let (dir, cert, key) = self_signed_material();

    // key 文件内容坏
    let bad_key = dir.join("bad.key");
    std::fs::write(&bad_key, "not a pem").expect("write");
    let err = build_acceptor(&tls_settings(&cert, &bad_key))
        .err()
        .expect("bad key must fail");
    assert!(format!("{err:#}").contains("bad.key"), "{err:#}");

    // 文件缺失
    let missing = dir.join("missing.pem");
    let err = build_acceptor(&tls_settings(&missing, &key))
        .err()
        .expect("missing cert must fail");
    assert!(format!("{err:#}").contains("missing.pem"), "{err:#}");

    // PEM 存在但无证书条目
    let empty = dir.join("empty.pem");
    std::fs::write(&empty, "").expect("write");
    let err = build_acceptor(&tls_settings(&empty, &key))
        .err()
        .expect("empty cert must fail");
    assert!(format!("{err:#}").contains("no certificates"), "{err:#}");
    let _ = std::fs::remove_dir_all(dir);
}

/// mTLS 三档：optional/required 必须有 client_ca（否则无信任锚）；
/// 有 CA 时三档全部构建成功。
#[test]
fn client_auth_matrix() {
    let dir = std::env::temp_dir().join(format!("rgw-tls-mtls-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create");
    let (ca, server, server_key, _client, _client_key) = ca_material(&dir);

    // 缺 client_ca：optional / required 都报错
    for mode in [ClientAuth::Optional, ClientAuth::Required] {
        let settings = TlsSettings {
            client_auth: mode,
            client_ca: None,
            ..tls_settings(&server, &server_key)
        };
        let err = build_server_config(&settings)
            .err()
            .expect("client_ca missing must fail");
        assert!(format!("{err:#}").contains("client_ca"), "{err:#}");
    }

    // 有 client_ca：三档全部构建成功
    for mode in [ClientAuth::None, ClientAuth::Optional, ClientAuth::Required] {
        let settings = TlsSettings {
            client_auth: mode,
            client_ca: Some(ca.clone()),
            ..tls_settings(&server, &server_key)
        };
        build_server_config(&settings)
            .unwrap_or_else(|e| panic!("{mode:?} must build with client_ca: {e:#}"));
    }
    let _ = std::fs::remove_dir_all(dir);
}
