//! 手写 HTTP/1.1 报文的最小客户端。
//!
//! 为什么不用现成 HTTP 客户端：集成测试要精确控制发出去的每一个头
//! （`Connection` token、伪装的 `X-Request-ID`、chunked 分块边界等），
//! 手写报文让「上游收到了什么」的断言完全确定；响应解析只需覆盖本项目
//! 会产出的形状（Content-Length / chunked / 读到 EOF）。
//!
//! [`request_tls`] 是 HTTPS 变体：rustls 握手后复用同一套手写报文与
//! 响应解析（TLS 流与明文流共用泛型读写助手）。

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// 一次请求的 body 形态。
pub enum BodySpec {
    /// 无 body（Content-Length: 0）。
    Empty,
    /// 整体字节（自动写 Content-Length）。
    Bytes(Vec<u8>),
    /// 分块传输（Transfer-Encoding: chunked），每个 Vec 是一个 chunk。
    Chunked(Vec<Vec<u8>>),
}

/// 手写客户端拿到的响应（已解码 body）。
#[derive(Debug)]
pub struct RawResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 按出现顺序保存的响应头（小写名字，多值分别成行）。
    pub headers: Vec<(String, String)>,
    /// 响应体字节。
    pub body: Vec<u8>,
}

impl RawResponse {
    /// 取第一个匹配名字的响应头值（名字大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 响应体按 UTF-8 无损转文本（测试断言用）。
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 向 `addr` 发一个 HTTP/1.1 请求并读回响应。
///
/// `headers` 逐行原样写入报文（调用方负责给出 `Host` 等必需头）；
/// body 形态见 [`BodySpec`]。测试夹具代码：I/O 失败直接 panic，让用例失败。
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: BodySpec,
) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect to gateway");
    let head = build_head(method, path, headers, &body);
    stream.write_all(head.as_bytes()).await.expect("write head");
    write_body(&mut stream, &body).await;
    stream.flush().await.expect("flush request");
    read_response(&mut stream).await
}

/// 组请求报文头：请求行 + 调用方头 + body 相关的 framing 头。
fn build_head(method: &str, path: &str, headers: &[(&str, &str)], body: &BodySpec) -> String {
    let mut head = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        BodySpec::Empty => head.push_str("Content-Length: 0\r\n"),
        BodySpec::Bytes(bytes) => head.push_str(&format!("Content-Length: {}\r\n", bytes.len())),
        BodySpec::Chunked(_) => head.push_str("Transfer-Encoding: chunked\r\n"),
    }
    head.push_str("\r\n");
    head
}

/// 按 body 形态写出载荷（chunked 形态逐块写，块边界即写入边界）。
async fn write_body<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, body: &BodySpec) {
    match body {
        BodySpec::Empty => {}
        BodySpec::Bytes(bytes) => stream.write_all(bytes).await.expect("write body"),
        BodySpec::Chunked(chunks) => {
            for chunk in chunks {
                let size = format!("{:x}\r\n", chunk.len());
                stream.write_all(size.as_bytes()).await.expect("chunk size");
                stream.write_all(chunk).await.expect("chunk data");
                stream.write_all(b"\r\n").await.expect("chunk end");
            }
            stream.write_all(b"0\r\n\r\n").await.expect("last chunk");
        }
    }
}

/// 从连接上读一个完整响应：先读头（到空行为止），再按 framing 读 body。
/// I/O 失败 panic（明文用例的既有契约；失败即用例失败）。
pub async fn read_response<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> RawResponse {
    try_read_response(stream)
        .await
        .unwrap_or_else(|e| panic!("read response: {e}"))
}

/// [`read_response`] 的可失败内核：I/O 错误（含 TLS alert）归一为
/// `Err`，供 HTTPS 用例把「被服务端拒绝」当正常断言目标而非 panic。
pub async fn try_read_response<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<RawResponse, String> {
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        expect_more(stream, &mut buf, "response head").await?;
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|e| format!("head utf-8: {e}"))?;
    let (status, headers) = parse_head(head);
    let mut rest = buf[head_end + 4..].to_vec();
    let body = read_body(stream, &headers, &mut rest).await?;
    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

/// 解析状态行与头行；头名字统一转小写，便于大小写不敏感断言。
fn parse_head(head: &str) -> (u16, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse::<u16>()
        .expect("status code numeric");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers)
}

/// 按 Content-Length / chunked / EOF 三种 framing 之一读出 body。
async fn read_body<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    headers: &[(String, String)],
    rest: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    if let Some(len) = header_value(headers, "content-length") {
        let len: usize = len.parse().map_err(|e| format!("content-length: {e}"))?;
        while rest.len() < len {
            expect_more(stream, rest, "response body").await?;
        }
        return Ok(rest[..len].to_vec());
    }
    if header_value(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        return read_chunked(stream, rest).await;
    }
    loop {
        if read_more(stream, rest).await? == 0 {
            return Ok(std::mem::take(rest));
        }
    }
}

/// 解析 chunked body：块大小行 + 块数据 + 结尾 0 块（trailer 行忽略）。
async fn read_chunked<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    rest: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = loop {
            if let Some(pos) = find_subslice(rest, b"\r\n") {
                break pos;
            }
            expect_more(stream, rest, "chunk size line").await?;
        };
        let size_text = std::str::from_utf8(&rest[..line_end]).map_err(|e| format!("chunk size: {e}"))?;
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|e| format!("chunk size hex: {e}"))?;
        rest.drain(..line_end + 2);
        if size == 0 {
            return Ok(out);
        }
        while rest.len() < size + 2 {
            expect_more(stream, rest, "chunk data").await?;
        }
        out.extend_from_slice(&rest[..size]);
        rest.drain(..size + 2);
    }
}

/// 取首个匹配名字的头值。
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 在字节缓冲里找子串位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 再读一些字节进缓冲；连接提前关闭视为错误。
async fn expect_more<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    what: &str,
) -> Result<(), String> {
    if read_more(stream, buf).await? == 0 {
        return Err(format!("connection closed in {what}"));
    }
    Ok(())
}

/// 尽量读满一块；返回本次读到的字节数（0 = EOF）。I/O 错误（含 TLS
/// alert）归一为 `Err`。
async fn read_more<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
) -> Result<usize, String> {
    let mut chunk = [0u8; 4096];
    let n = stream.read(&mut chunk).await.map_err(|e| format!("socket read: {e}"))?;
    buf.extend_from_slice(&chunk[..n]);
    Ok(n)
}

/// HTTPS 变体：rustls 握手后走同一套手写报文。
///
/// `roots_pem` 是客户端信任的 CA / 自签证书 PEM；`client_cert` 是可选的
/// `(cert PEM, key PEM)` 客户端证书对（mTLS 用例）。握手失败返回
/// `Err(原因)` 而不 panic——负向用例（不带客户端证书连 required 服务端）
/// 需要断言失败本身。
pub async fn request_tls(
    addr: SocketAddr,
    server_name: &str,
    roots_pem: &str,
    client_cert: Option<(&str, &str)>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: BodySpec,
) -> Result<RawResponse, String> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let connector = tls_connector(roots_pem, client_cert);
    let server_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|e| format!("server name: {e:?}"))?;
    let mut stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| format!("TLS handshake: {e}"))?;
    let head = build_head(method, path, headers, &body);
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| format!("write head: {e}"))?;
    write_body(&mut stream, &body).await;
    stream
        .flush()
        .await
        .map_err(|e| format!("flush: {e}"))?;
    try_read_response(&mut stream).await
}

/// 构建 rustls 客户端 connector：信任 `roots_pem` 里的锚，可选出示
/// 客户端证书。provider 显式取 ring（与服务端一致）。
fn tls_connector(roots_pem: &str, client_cert: Option<(&str, &str)>) -> tokio_rustls::TlsConnector {
    use rustls::pki_types::CertificateDer;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut roots_pem.as_bytes()) {
        roots.add(cert.expect("root cert PEM")).expect("root add");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions");
    let config = match client_cert {
        None => builder
            .with_root_certificates(roots)
            .with_no_client_auth(),
        Some((cert_pem, key_pem)) => {
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
                .collect::<Result<_, _>>()
                .expect("client cert PEM");
            let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
                .expect("client key parse")
                .expect("client key present");
            builder
                .with_root_certificates(roots)
                .with_client_auth_cert(certs, key)
                .expect("client auth cert")
        }
    };
    tokio_rustls::TlsConnector::from(Arc::new(config))
}
