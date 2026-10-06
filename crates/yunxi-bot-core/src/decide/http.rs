//! 最小 HTTP/1.1 客户端，只服务本机回环上的 JSON 接口。
//!
//! 为什么不引 HTTP 库：这是唯一需要联网的地方，而且只对 `127.0.0.1` 说
//! 一个固定形状的请求。为它拉进一整棵依赖树不划算（本项目运行时依赖目前
//! 只有 `chrono`）。
//!
//! 刻意不支持：TLS、重定向、分块传输、keep-alive。用不到，且每多支持一样
//! 就多一处出错的地方。遇到不支持的响应直接报错，而不是猜。
//!
//! ## 它不只服务决策模型
//!
//! 邮件 sidecar 走的是同一个回环契约，所以也用这个客户端。
//! **模块位置（`decide::http`）是历史原因**，它实质上是一个通用的
//! "本机回环 JSON"客户端。
//!
//! 而 [`parse_endpoint`] 里的**回环限制才是它最值钱的地方**：决策 state 和
//! 邮件内容都是私人信息，这个限制保证它们不可能被发到外部主机。加一个
//! 新 sidecar 就白拿这条保证——比每个 sidecar 各写一份客户端可靠得多。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 响应体上限。防止一个坏掉的服务把常驻进程的内存吃掉。
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// 回环地址的**连接**超时。
///
/// ## 为什么回环要单独一个更短的超时
///
/// 这是真机上量出来的：Windows 上连 `127.0.0.1` 上一个没人监听的端口，
/// 内核要**约 2070 ms** 才回"积极拒绝"。而默认超时是 2000 ms——
/// 于是**先超时了**，真实原因（没人监听）被报成"调用超时"。
///
/// 两种原因的下一步动作完全不同：
///
/// | 报什么 | 该去查什么 |
/// |---|---|
/// | 超时 | 服务为什么慢 |
/// | 没人监听 | 服务为什么没起来 |
///
/// 而且每次白等两秒——`check` 一轮 20 封邮件里那些要走决策层的，
/// 会白等十几秒。**本机服务要么立刻接受，要么就是没起来**，
/// 300 ms 足够区分。
const LOOPBACK_CONNECT_MS: u64 = 300;

/// 连接超时：回环用短的，其他用调用方给的。
fn connect_timeout_for(addr: &SocketAddr, asked_ms: u64) -> u64 {
    if addr.ip().is_loopback() {
        LOOPBACK_CONNECT_MS.min(asked_ms)
    } else {
        asked_ms
    }
}

/// 连不上时给一句**指向正确方向**的话。
fn connect_error(addr: &SocketAddr, e: &std::io::Error) -> HttpError {
    // `TimedOut` 在回环上几乎总是"没人监听"——内核回报拒绝比超时慢，
    // 所以消息里两种可能都要提，并给出该查什么。
    if e.kind() == std::io::ErrorKind::TimedOut && addr.ip().is_loopback() {
        return HttpError::Connect(format!(
            "{addr} 连不上（回环地址 {LOOPBACK_CONNECT_MS} ms 内没接受连接）\
             ——多半是这个服务没起来"
        ));
    }
    if e.kind() == std::io::ErrorKind::TimedOut {
        return HttpError::Timeout(format!(
            "连 {addr} 超时（{ }ms 内没连上）",
            connect_timeout_for(addr, 0)
        ));
    }
    HttpError::Connect(format!("{addr} 连不上：{e}"))
}

#[derive(Debug)]
pub enum HttpError {
    /// 无法解析或连接到目标地址。
    Connect(String),
    /// 读写超时。
    ///
    /// **带上地址和阶段。** 只说"调用超时"的话，拿到日志的人不知道
    /// 是连不上还是读不动——而这两件事的下一步完全不同。
    Timeout(String),
    Io(String),
    /// 响应不是合法的 HTTP/1.x。
    Malformed(String),
    /// 非 2xx 状态码。
    Status(u16, String),
    /// 响应体超过上限。
    TooLarge,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Connect(m) => write!(f, "连接失败: {m}"),
            HttpError::Timeout(d) => write!(f, "请求超时: {d}"),
            HttpError::Io(m) => write!(f, "网络读写错误: {m}"),
            HttpError::Malformed(m) => write!(f, "响应格式非法: {m}"),
            HttpError::Status(code, msg) => write!(f, "服务返回 {code}: {msg}"),
            HttpError::TooLarge => write!(f, "响应体超过上限"),
        }
    }
}

impl std::error::Error for HttpError {}

/// 向本机回环地址发一个 JSON POST，返回响应体字符串。
///
/// `endpoint` 形如 `http://127.0.0.1:17870/decide`。只接受 http 与回环地址——
/// 决策模型按 ADR 是**本地** sidecar，不允许指向外部主机。
pub fn post_json(endpoint: &str, body: &str, timeout_ms: u64) -> Result<String, HttpError> {
    let (addr, path) = parse_endpoint(endpoint)?;

    let timeout = Duration::from_millis(timeout_ms);
    // 连接用**回环专属**的超时：本机服务要么立刻接受，要么就是没起来
    let connect_ms = connect_timeout_for(&addr, timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(connect_ms))
        .map_err(|e| connect_error(&addr, &e))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| HttpError::Io(e.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| HttpError::Io(e.to_string()))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        host = addr.ip(),
        port = addr.port(),
        len = body.len(),
    );

    stream.write_all(request.as_bytes()).map_err(map_io)?;
    stream.flush().map_err(map_io)?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err(HttpError::TooLarge);
                }
            }
            Err(e) => return Err(map_io(e)),
        }
    }

    parse_response(&raw)
}

/// 向本机回环地址发一个 GET，返回响应体字符串。
///
/// 邮件 sidecar 的 `/health` 用它。与 [`post_json`] 共用端点解析
/// （因而共用**回环限制**）和响应解析。
pub fn get_json(endpoint: &str, timeout_ms: u64) -> Result<String, HttpError> {
    let (addr, path) = parse_endpoint(endpoint)?;
    let timeout = Duration::from_millis(timeout_ms);

    // 连接用**回环专属**的超时：本机服务要么立刻接受，要么就是没起来
    let connect_ms = connect_timeout_for(&addr, timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(connect_ms))
        .map_err(|e| connect_error(&addr, &e))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| HttpError::Io(e.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| HttpError::Io(e.to_string()))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\
         \r\n",
        host = addr.ip(),
        port = addr.port(),
    );
    stream.write_all(request.as_bytes()).map_err(map_io)?;
    stream.flush().map_err(map_io)?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err(HttpError::TooLarge);
                }
            }
            Err(e) => return Err(map_io(e)),
        }
    }

    parse_response(&raw)
}

fn map_io(e: std::io::Error) -> HttpError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            HttpError::Timeout(format!("读写超时（{e}）"))
        }
        _ => HttpError::Io(e.to_string()),
    }
}

/// 解析 `http://host:port/path`，并强制回环限制。
fn parse_endpoint(endpoint: &str) -> Result<(SocketAddr, String), HttpError> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| HttpError::Connect("只支持 http:// 前缀".into()))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| HttpError::Connect(format!("端口非法: {p}")))?,
        ),
        None => return Err(HttpError::Connect("必须显式给出端口".into())),
    };

    // 决策模型是本地 sidecar：拒绝指向外部主机，避免把私人 state 发出去
    let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
    if !is_loopback {
        return Err(HttpError::Connect(format!(
            "拒绝非回环地址 {host}：决策模型的 state 可能含私人信息，只允许本机"
        )));
    }

    let sock = (host, port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Connect(e.to_string()))?
        .next()
        .ok_or_else(|| HttpError::Connect("无法解析地址".into()))?;

    Ok((sock, path.to_string()))
}

/// 解析 HTTP 响应：状态行 + 头 + 体。
fn parse_response(raw: &[u8]) -> Result<String, HttpError> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Malformed("缺少头部与正文的分隔".into()))?;
    let (head, body) = raw.split_at(sep);
    let body = &body[4..];

    let head = String::from_utf8_lossy(head);
    let mut lines = head.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Malformed("空响应".into()))?;

    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let code: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| HttpError::Malformed(format!("状态行非法: {status_line}")))?;

    // 分块传输必须显式拒绝：静默把 chunk 头当正文会得到看似成功的坏数据
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            return Err(HttpError::Malformed(
                "不支持分块传输编码，请让 sidecar 返回 Content-Length 或 Connection: close".into(),
            ));
        }
    }

    let text = String::from_utf8_lossy(body).to_string();
    if !(200..300).contains(&code) {
        let snippet: String = text.chars().take(200).collect();
        return Err(HttpError::Status(code, snippet));
    }
    Ok(text)
}

#[cfg(test)]
mod loopback_tests {
    use super::*;

    fn addr(ip: &str, port: u16) -> SocketAddr {
        // IPv6 字面量要方括号，`::1:17870` 是解析不了的
        let host = if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.to_string()
        };
        format!("{host}:{port}").parse().unwrap()
    }

    #[test]
    fn loopback_gets_a_short_connect_timeout() {
        // **真机上量出来的：** Windows 连 `127.0.0.1` 上没人监听的端口，
        // 内核要**约 2070 ms** 才回"积极拒绝"。而默认超时是 2000 ms——
        // 于是先超时了，真实原因（没人监听）被报成"调用超时"，
        // 而且每次白等两秒。本机服务要么立刻接受，要么就是没起来。
        assert_eq!(
            connect_timeout_for(&addr("127.0.0.1", 17870), 2_000),
            LOOPBACK_CONNECT_MS
        );
        assert_eq!(
            connect_timeout_for(&addr("::1", 17870), 2_000),
            LOOPBACK_CONNECT_MS
        );
    }

    #[test]
    fn a_shorter_asked_timeout_still_wins_on_loopback() {
        // 调用方要的更短就听调用方的——它可能有个更紧的预算
        assert_eq!(connect_timeout_for(&addr("127.0.0.1", 1), 100), 100);
    }

    #[test]
    fn non_loopback_keeps_the_asked_timeout() {
        assert_eq!(connect_timeout_for(&addr("10.0.0.1", 1), 2_000), 2_000);
    }

    #[test]
    fn a_loopback_timeout_is_reported_as_unreachable_not_timeout() {
        // **两者指向的下一步完全不同**：超时 -> 查为什么慢；
        // 没人监听 -> 查为什么服务没起来。
        let e = connect_error(
            &addr("127.0.0.1", 17870),
            &std::io::Error::from(std::io::ErrorKind::TimedOut),
        );
        match e {
            HttpError::Connect(m) => {
                assert!(m.contains("没起来"), "要说清该去查什么: {m}");
                assert!(m.contains("17870"), "要带上地址: {m}");
            }
            other => panic!("回环超时该报成连接问题，实际 {other:?}"),
        }
    }

    #[test]
    fn a_remote_timeout_is_still_a_timeout() {
        // 远端超时确实可能是"慢"，不能一概说成"没起来"
        let e = connect_error(
            &addr("10.0.0.1", 1),
            &std::io::Error::from(std::io::ErrorKind::TimedOut),
        );
        assert!(matches!(e, HttpError::Timeout(_)), "{e:?}");
    }

    #[test]
    fn a_refused_loopback_connection_keeps_the_os_message() {
        let e = connect_error(
            &addr("127.0.0.1", 17870),
            &std::io::Error::other("积极拒绝"),
        );
        match e {
            HttpError::Connect(m) => assert!(m.contains("积极拒绝"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_endpoints() {
        // 私人 state 不得发到外部主机
        let err = parse_endpoint("http://example.com:80/decide").unwrap_err();
        match err {
            HttpError::Connect(m) => assert!(m.contains("非回环"), "{m}"),
            other => panic!("应拒绝非回环地址，实际: {other:?}"),
        }
    }

    #[test]
    fn rejects_https_and_missing_port() {
        assert!(parse_endpoint("https://127.0.0.1:443/x").is_err());
        assert!(parse_endpoint("http://127.0.0.1/x").is_err());
    }

    #[test]
    fn accepts_loopback() {
        let (addr, path) = parse_endpoint("http://127.0.0.1:17870/decide").unwrap();
        assert_eq!(addr.port(), 17870);
        assert_eq!(path, "/decide");
        assert!(parse_endpoint("http://localhost:1234/").is_ok());
    }

    #[test]
    fn parses_a_normal_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(raw).unwrap(), "{}");
    }

    #[test]
    fn surfaces_non_2xx_status() {
        let raw = b"HTTP/1.1 500 Boom\r\n\r\nserver exploded";
        match parse_response(raw).unwrap_err() {
            HttpError::Status(500, body) => assert!(body.contains("exploded")),
            other => panic!("应返回状态错误，实际: {other:?}"),
        }
    }

    #[test]
    fn rejects_chunked_transfer_encoding() {
        // 静默接受分块会把 chunk 头当正文，得到"看起来成功"的坏数据
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert!(matches!(
            parse_response(raw).unwrap_err(),
            HttpError::Malformed(_)
        ));
    }

    #[test]
    fn missing_separator_is_malformed() {
        assert!(matches!(
            parse_response(b"HTTP/1.1 200 OK").unwrap_err(),
            HttpError::Malformed(_)
        ));
    }
}
