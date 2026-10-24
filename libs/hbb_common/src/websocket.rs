use crate::{
    config::{
        keys::{OPTION_RELAY_SERVER, OPTION_WEBSOCKET_PORT},
        use_ws, Config, Socks5Server, EXE_RENDEZVOUS_SERVER, PROD_RENDEZVOUS_SERVER, RELAY_PORT,
        RENDEZVOUS_PORT,
    },
    protobuf::Message,
    socket_client::split_host_port,
    sodiumoxide::crypto::secretbox::Key,
    tcp::{DynTcpStream, Encrypt, KxTranscript},
    tls::{get_cached_tls_accept_invalid_cert, get_cached_tls_type, upsert_tls_cache, TlsType},
    ResultType,
};
use anyhow::bail;
use async_recursion::async_recursion;
use bytes::{Bytes, BytesMut};
use futures::{future::Either, SinkExt, StreamExt};
use std::{
    io::{self, Error, ErrorKind},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::timeout,
};
use tokio_native_tls::native_tls::TlsConnector;
use tokio_tungstenite::{
    client_async_tls_with_config, connect_async_tls_with_config,
    tungstenite::protocol::Message as WsMessage, Connector, MaybeTlsStream, WebSocketStream,
};
use tungstenite::client::IntoClientRequest;

/// tungstenite 自身的默认值。`set_max_packet_length` 会钳制到这些值，因此 `usize::MAX` 恰好把
/// 该传输恢复到它原来的位置，而不是高过它。
const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 << 20;
const DEFAULT_MAX_FRAME_SIZE: usize = 16 << 20;

// macOS/iOS 的 Native TLS 可能在等待响应时不刷新代理隧道的写缓冲。
struct FlushBeforeRead(DynTcpStream);

impl AsyncRead for FlushBeforeRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 被阻塞的写端不应妨碍读取能让对端继续推进的数据。
        if let Poll::Ready(Err(err)) = Pin::new(&mut self.0).poll_flush(cx) {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for FlushBeforeRead {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

// 在支持代理流的同时保留原有的非代理连接路径。
type WsStream = Either<
    WebSocketStream<MaybeTlsStream<TcpStream>>,
    WebSocketStream<MaybeTlsStream<DynTcpStream>>,
>;

pub struct WsFramedStream {
    stream: WsStream,
    addr: SocketAddr,
    encrypt: Option<Encrypt>,
    send_timeout: u64,
}

impl WsFramedStream {
    #[inline]
    fn get_connector(
        tls_type: &TlsType,
        danger_accept_invalid_certs: bool,
    ) -> ResultType<Option<Connector>> {
        match tls_type {
            TlsType::Plain => Ok(Some(Connector::Plain)),
            TlsType::NativeTls => {
                let connector = TlsConnector::builder()
                    .danger_accept_invalid_certs(danger_accept_invalid_certs)
                    .build()?;
                Ok(Some(Connector::NativeTls(connector)))
            }
            TlsType::Rustls => {
                let connector = match crate::verifier::client_config(danger_accept_invalid_certs) {
                    Ok(client_config) => Some(Connector::Rustls(Arc::new(client_config))),
                    Err(e) => {
                        log::warn!(
                            "Failed to get client config: {:?}, fallback to default connector",
                            e
                        );
                        None
                    }
                };
                Ok(connector)
            }
        }
    }

    async fn connect(
        url: &str,
        local_addr: Option<SocketAddr>,
        proxy_conf: Option<&Socks5Server>,
        ms_timeout: u64,
    ) -> ResultType<(WsStream, SocketAddr)> {
        let tls_type = get_cached_tls_type(url);
        let is_tls_type_cached = tls_type.is_some();
        let tls_type = tls_type.unwrap_or(TlsType::Rustls);
        let danger_accept_invalid_cert = get_cached_tls_accept_invalid_cert(&url);
        Self::try_connect(
            url,
            local_addr,
            proxy_conf,
            ms_timeout,
            tls_type,
            is_tls_type_cached,
            danger_accept_invalid_cert,
            danger_accept_invalid_cert,
        )
        .await
    }

    #[async_recursion]
    async fn try_connect(
        url: &str,
        local_addr: Option<SocketAddr>,
        proxy_conf: Option<&Socks5Server>,
        ms_timeout: u64,
        tls_type: TlsType,
        is_tls_type_cached: bool,
        danger_accept_invalid_cert: Option<bool>,
        original_danger_accept_invalid_certs: Option<bool>,
    ) -> ResultType<(WsStream, SocketAddr)> {
        let ws_config = None;
        let request = url
            .into_client_request()
            .map_err(|e| Error::new(ErrorKind::Other, e))?;
        let connector =
            Self::get_connector(&tls_type, danger_accept_invalid_cert.unwrap_or(false))?;
        let result = if let Some(proxy_conf) = proxy_conf {
            let host = request.uri().host().ok_or_else(|| {
                Error::new(ErrorKind::InvalidInput, "Invalid WebSocket URL: no host")
            })?;
            let port = request
                .uri()
                .port_u16()
                .or_else(|| match request.uri().scheme_str() {
                    Some("wss") => Some(443),
                    Some("ws") => Some(80),
                    _ => None,
                })
                .ok_or_else(|| {
                    Error::new(ErrorKind::InvalidInput, "Invalid WebSocket URL scheme")
                })?;
            let target = format!("{host}:{port}");
            timeout(Duration::from_millis(ms_timeout), async {
                let stream = crate::tcp::FramedStream::connect(
                    target.as_str(),
                    local_addr,
                    proxy_conf,
                    ms_timeout,
                )
                .await?;
                let addr = stream.1;
                let mut stream = stream.0.into_inner();
                if cfg!(any(target_os = "macos", target_os = "ios"))
                    && url.starts_with("wss://")
                    && matches!(tls_type, TlsType::NativeTls)
                    && url::Url::parse(&proxy_conf.proxy)
                        .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                {
                    stream = DynTcpStream(Box::new(FlushBeforeRead(stream)));
                }
                Ok::<_, anyhow::Error>(
                    client_async_tls_with_config(request, stream, ws_config, connector)
                        .await
                        .map(|(stream, _)| (stream, addr)),
                )
            })
            .await??
            .map(Either::Right)
        } else {
            let disable_nagle = false;
            timeout(
                Duration::from_millis(ms_timeout),
                connect_async_tls_with_config(request, ws_config, disable_nagle, connector),
            )
            .await?
            .map(|(stream, _)| Either::Left(stream))
        };
        match result {
            Ok(stream) => {
                upsert_tls_cache(url, tls_type, danger_accept_invalid_cert.unwrap_or(false));
                match stream {
                    Either::Left(stream) => {
                        // MaybeTlsStream::get_ref() 能处理 Plain、NativeTls 和 Rustls，
                        // 返回底层的 TcpStream 供 local_addr() 使用。
                        let addr = stream.get_ref().get_ref().local_addr()?;
                        Ok((Either::Left(stream), addr))
                    }
                    Either::Right((stream, addr)) => Ok((Either::Right(stream), addr)),
                }
            }
            Err(e) => match (tls_type, is_tls_type_cached, danger_accept_invalid_cert) {
                (TlsType::Rustls, _, None) => {
                    log::warn!(
                            "WebSocket connection with rustls-tls failed, try accept invalid certs: {}, {:?}",
                            url,
                            e
                        );
                    Self::try_connect(
                        url,
                        local_addr,
                        proxy_conf,
                        ms_timeout,
                        tls_type,
                        is_tls_type_cached,
                        Some(true),
                        original_danger_accept_invalid_certs,
                    )
                    .await
                }
                (TlsType::Rustls, false, Some(_)) => {
                    log::warn!(
                        "WebSocket connection with rustls-tls failed, try native-tls: {}, {:?}",
                        url,
                        e
                    );
                    Self::try_connect(
                        url,
                        local_addr,
                        proxy_conf,
                        ms_timeout,
                        TlsType::NativeTls,
                        is_tls_type_cached,
                        original_danger_accept_invalid_certs,
                        original_danger_accept_invalid_certs,
                    )
                    .await
                }
                (TlsType::NativeTls, _, None) => {
                    log::warn!(
                            "WebSocket connection with native-tls failed, try accept invalid certs: {}, {:?}",
                            url,
                            e
                        );
                    Self::try_connect(
                        url,
                        local_addr,
                        proxy_conf,
                        ms_timeout,
                        tls_type,
                        is_tls_type_cached,
                        Some(true),
                        original_danger_accept_invalid_certs,
                    )
                    .await
                }
                _ => {
                    log::error!(
                        "WebSocket connection failed with tls_type {:?}: {}, {:?}",
                        tls_type,
                        url,
                        e
                    );
                    if let tungstenite::Error::Http(response) = &e {
                        if response.status().is_redirection() {
                            bail!(
                                "WebSocket connection failed ({}). The server may not support WebSocket.",
                                e
                            )
                        }
                    }
                    bail!("WebSocket error: {}", e)
                }
            },
        }
    }

    pub async fn new<T: AsRef<str>>(
        url: T,
        local_addr: Option<SocketAddr>,
        ms_timeout: u64,
    ) -> ResultType<Self> {
        let proxy_conf = Config::get_socks();
        let (stream, addr) =
            Self::connect(url.as_ref(), local_addr, proxy_conf.as_ref(), ms_timeout).await?;

        let ws = Self {
            stream,
            addr,
            encrypt: None,
            send_timeout: ms_timeout,
        };

        Ok(ws)
    }

    #[inline]
    pub fn set_raw(&mut self) {
        self.encrypt = None;
    }

    /// 两个上限都要，不只是消息那个：`max_frame_size` 仅凭其
    /// 头部就会拒绝超大的帧（在它被缓冲之前），而 `max_message_size` 则限制跨
    /// 分片消息的重组。仅有头部时能分配的是 `read_buffer_size`，
    /// 这个打过补丁的版本每次增长一个分块，而不是增长到帧声明的长度。
    #[inline]
    pub fn set_max_packet_length(&mut self, n: usize) {
        let set_config = |c: &mut tungstenite::protocol::WebSocketConfig| {
            c.max_message_size = Some(n.min(DEFAULT_MAX_MESSAGE_SIZE));
            c.max_frame_size = Some(n.min(DEFAULT_MAX_FRAME_SIZE));
        };
        match &mut self.stream {
            Either::Left(stream) => stream.set_config(set_config),
            Either::Right(stream) => stream.set_config(set_config),
        }
    }

    #[inline]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    #[inline]
    pub fn set_send_timeout(&mut self, ms: u64) {
        self.send_timeout = ms;
    }

    #[inline]
    pub fn set_key(&mut self, key: Key) {
        self.encrypt = Some(Encrypt::new(key));
    }

    #[inline]
    pub fn set_key_split(
        &mut self,
        key: Key,
        is_initiator: bool,
        t: &KxTranscript,
    ) -> ResultType<()> {
        self.encrypt = Some(Encrypt::new_split(key, is_initiator, t)?);
        Ok(())
    }

    #[inline]
    pub fn is_secured(&self) -> bool {
        self.encrypt.is_some()
    }

    #[inline]
    pub async fn send(&mut self, msg: &impl Message) -> ResultType<()> {
        self.send_raw(msg.write_to_bytes()?).await
    }

    #[inline]
    pub async fn send_raw(&mut self, msg: Vec<u8>) -> ResultType<()> {
        let mut msg = msg;
        if let Some(key) = self.encrypt.as_mut() {
            msg = key.enc(&msg);
        }
        self.send_bytes(Bytes::from(msg)).await
    }

    pub async fn send_bytes(&mut self, bytes: Bytes) -> ResultType<()> {
        let msg = WsMessage::Binary(bytes);
        if self.send_timeout > 0 {
            timeout(
                Duration::from_millis(self.send_timeout),
                self.stream.send(msg),
            )
            .await??
        } else {
            self.stream.send(msg).await?
        };
        Ok(())
    }

    #[inline]
    pub async fn next(&mut self) -> Option<Result<BytesMut, Error>> {
        while let Some(msg) = self.stream.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(e) => {
                    log::error!("{}", e);
                    return Some(Err(Error::new(
                        ErrorKind::Other,
                        format!("WebSocket protocol error: {}", e),
                    )));
                }
            };

            match msg {
                WsMessage::Binary(data) => {
                    let mut bytes = BytesMut::from(&data[..]);
                    if let Some(key) = self.encrypt.as_mut() {
                        if let Err(err) = key.dec(&mut bytes) {
                            return Some(Err(err));
                        }
                    }
                    return Some(Ok(bytes));
                }
                WsMessage::Text(text) => {
                    if self.is_secured() {
                        return Some(Err(Error::new(
                            ErrorKind::InvalidData,
                            "WebSocket text frame received after encryption was enabled",
                        )));
                    }
                    let bytes = BytesMut::from(text.as_bytes());
                    return Some(Ok(bytes));
                }
                WsMessage::Close(_) => {
                    return None;
                }
                _ => {
                    continue;
                }
            }
        }

        None
    }

    #[inline]
    pub async fn next_timeout(&mut self, ms: u64) -> Option<Result<BytesMut, Error>> {
        match timeout(Duration::from_millis(ms), self.next()).await {
            Ok(res) => res,
            Err(_) => None,
        }
    }
}

pub fn is_ws_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with("ws://") || endpoint.starts_with("wss://")
}

/**
 * 把 endpoint 转换为 WebSocket 格式的核心函数
 *
 * 在不同地址格式之间转换：
 * 1. 带/不带端口的 IPv4 地址 -> ws://ipv4:port
 * 2. 带/不带端口的 IPv6 地址 -> ws://[ipv6]:port
 * 3. 带/不带端口的域名 -> ws(s)://domain/ws/path
 *
 * @param endpoint 要转换的 endpoint
 * @return 转换后的 WebSocket endpoint
 */
pub fn check_ws(endpoint: &str) -> String {
    if !use_ws() {
        return endpoint.to_string();
    }

    if endpoint.is_empty() {
        return endpoint.to_string();
    }

    if is_ws_endpoint(endpoint) {
        return endpoint.to_string();
    }

    let Some((endpoint_host, endpoint_port)) = split_host_port(endpoint) else {
        debug_assert!(false, "endpoint doesn't have port");
        return endpoint.to_string();
    };

    let custom_rendezvous_server = Config::get_rendezvous_server();
    let relay_server = Config::get_option(OPTION_RELAY_SERVER);
    let rendezvous_port = split_host_port(&custom_rendezvous_server)
        .map(|(_, p)| p)
        .unwrap_or(RENDEZVOUS_PORT);
    let relay_port = split_host_port(&relay_server)
        .map(|(_, p)| p)
        .unwrap_or(RELAY_PORT);

    let (relay, dst_port) = if endpoint_port == rendezvous_port {
        // rendezvous
        (false, endpoint_port + 2)
    } else if endpoint_port == rendezvous_port - 1 {
        // online
        (false, endpoint_port + 3)
    } else if endpoint_port == relay_port || endpoint_port == rendezvous_port + 1 {
        // relay
        // https://github.com/rustdesk/rustdesk/blob/6ffbcd1375771f2482ec4810680623a269be70f1/src/rendezvous_mediator.rs#L615
        // https://github.com/rustdesk/rustdesk-server/blob/235a3c326ceb665e941edb50ab79faa1208f7507/src/relay_server.rs#L83，基于 relay 端口。
        (true, endpoint_port + 2)
    } else {
        // 回退 relay
        // 对控制端而言，relay 服务器是从被控端传过来的，与本地配置无关。
        (true, endpoint_port + 2)
    };

    let (address, is_domain) = if crate::is_ip_str(endpoint) {
        (format!("{}:{}", endpoint_host, dst_port), false)
    } else {
        let domain_path = if relay { "/ws/relay" } else { "/ws/id" };
        let port = websocket_port(&endpoint_host, &relay_server)
            .map(|port| format!(":{}", port))
            .unwrap_or_default();
        (format!("{}{}{}", endpoint_host, port, domain_path), true)
    };
    let protocol = if is_domain {
        let api_server = Config::get_option("api-server");
        if api_server.starts_with("https") {
            "wss"
        } else {
            "ws"
        }
    } else {
        "ws"
    };
    format!("{}://{}", protocol, address)
}

/// 该客户端自有服务器上提供 `/ws/id` 和 `/ws/relay` 的反向代理端口，用于
/// 当它无法监听该 scheme 的默认端口时。任何其他主机，比如对端的服务器或
/// 另一台机器上的 relay，都保持默认端口。IP 形式的 endpoint 不会走到这里。
fn websocket_port(host: &str, relay_server: &str) -> Option<u16> {
    let port = Config::get_option(OPTION_WEBSOCKET_PORT);
    if port.is_empty() {
        return None;
    }
    // 只包括该客户端所配置的那些服务器，不包括 get_rendezvous_server() 的公开回退地址。
    let own_servers = [
        EXE_RENDEZVOUS_SERVER.read().unwrap().clone(),
        Config::get_option("custom-rendezvous-server"),
        PROD_RENDEZVOUS_SERVER.read().unwrap().clone(),
        relay_server.to_owned(),
    ];
    let is_own = own_servers.iter().any(|server| {
        split_host_port(server)
            .map(|(h, _)| h)
            .unwrap_or_else(|| server.to_string())
            .eq_ignore_ascii_case(host)
    });
    if !is_own {
        return None;
    }
    match port.parse::<u16>() {
        Ok(port) if port > 0 => Some(port),
        _ => {
            crate::throttled_log!(
                Duration::from_secs(60),
                warn,
                "Ignoring invalid {}: {}",
                OPTION_WEBSOCKET_PORT,
                port
            );
            None
        }
    }
}

#[cfg(test)]
mod proxy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{keys, Config};
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    use tungstenite::protocol::Role;

    async fn ws_from_tcp(stream: TcpStream, addr: SocketAddr) -> WsFramedStream {
        let ws_stream =
            WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(stream), Role::Client, None)
                .await;

        WsFramedStream {
            stream: Either::Left(ws_stream),
            addr,
            encrypt: None,
            send_timeout: 0,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_secured_stream_rejects_plaintext_text() {
        sodiumoxide::init().expect("failed to initialize sodiumoxide");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = WebSocketStream::from_raw_socket(tcp, Role::Server, None).await;
            stream
                .send(WsMessage::Text("plaintext".into()))
                .await
                .unwrap();
        });

        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut client = ws_from_tcp(tcp, addr).await;
        client.set_key(Key([0x42; sodiumoxide::crypto::secretbox::KEYBYTES]));

        let result = client.next().await;
        assert!(
            matches!(result, Some(Err(ref err)) if err.kind() == ErrorKind::InvalidData),
            "secured stream accepted plaintext Text frame: {:?}",
            result
        );
        server.await.unwrap();
    }

    #[test]
    fn test_check_ws() {
        let options = Config::get_options();
        // 启用 websocket
        Config::set_option(keys::OPTION_ALLOW_WEBSOCKET.to_string(), "Y".to_string());

        // 未设置 custom-rendezvous-server
        Config::set_option("custom-rendezvous-server".to_string(), "".to_string());
        Config::set_option("relay-server".to_string(), "".to_string());
        Config::set_option("api-server".to_string(), "".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        assert_eq!(check_ws("rustdesk.com:21115"), "ws://rustdesk.com/ws/id");
        assert_eq!(check_ws("rustdesk.com:21116"), "ws://rustdesk.com/ws/id");
        assert_eq!(check_ws("rustdesk.com:21117"), "ws://rustdesk.com/ws/relay");
        // 设置 relay-server 但不带端口
        Config::set_option("relay-server".to_string(), "127.0.0.1".to_string());
        Config::set_option(
            "api-server".to_string(),
            "https://api.rustdesk.com".to_string(),
        );
        assert_eq!(
            check_ws("[0:0:0:0:0:0:0:1]:21115"),
            "ws://[0:0:0:0:0:0:0:1]:21118"
        );
        assert_eq!(
            check_ws("[0:0:0:0:0:0:0:1]:21116"),
            "ws://[0:0:0:0:0:0:0:1]:21118"
        );
        assert_eq!(
            check_ws("[0:0:0:0:0:0:0:1]:21117"),
            "ws://[0:0:0:0:0:0:0:1]:21119"
        );
        assert_eq!(check_ws("rustdesk.com:21115"), "wss://rustdesk.com/ws/id");
        assert_eq!(check_ws("rustdesk.com:21116"), "wss://rustdesk.com/ws/id");
        assert_eq!(
            check_ws("rustdesk.com:21117"),
            "wss://rustdesk.com/ws/relay"
        );
        // 设置 relay-server 并带默认端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:21117".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带自定义端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:34567".to_string());
        assert_eq!(check_ws("rustdesk.com:21115"), "wss://rustdesk.com/ws/id");
        assert_eq!(check_ws("rustdesk.com:21116"), "wss://rustdesk.com/ws/id");
        assert_eq!(
            check_ws("rustdesk.com:34567"),
            "wss://rustdesk.com/ws/relay"
        );

        // 设置 custom-rendezvous-server 但不带端口
        Config::set_option(
            "custom-rendezvous-server".to_string(),
            "127.0.0.1".to_string(),
        );
        Config::set_option("relay-server".to_string(), "".to_string());
        Config::set_option("api-server".to_string(), "".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 但不带端口
        Config::set_option("relay-server".to_string(), "127.0.0.1".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带默认端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:21117".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带自定义端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:34567".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:34567"), "ws://127.0.0.1:34569");

        // 设置 custom-rendezvous-server 但不带默认端口
        Config::set_option(
            "custom-rendezvous-server".to_string(),
            "127.0.0.1".to_string(),
        );
        Config::set_option("relay-server".to_string(), "".to_string());
        Config::set_option("api-server".to_string(), "".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 但不带端口
        Config::set_option("relay-server".to_string(), "127.0.0.1".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带默认端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:21117".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带自定义端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:34567".to_string());
        assert_eq!(check_ws("127.0.0.1:21115"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        assert_eq!(check_ws("127.0.0.1:34567"), "ws://127.0.0.1:34569");

        // 设置 custom-rendezvous-server 并带自定义端口
        Config::set_option(
            "custom-rendezvous-server".to_string(),
            "127.0.0.1:23456".to_string(),
        );
        Config::set_option("relay-server".to_string(), "".to_string());
        Config::set_option("api-server".to_string(), "".to_string());
        assert_eq!(check_ws("127.0.0.1:23455"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:23456"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:23457"), "ws://127.0.0.1:23459");
        // 设置 relay-server 但不带端口
        Config::set_option("relay-server".to_string(), "127.0.0.1".to_string());
        assert_eq!(check_ws("127.0.0.1:23455"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:23456"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带默认端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:21117".to_string());
        assert_eq!(check_ws("127.0.0.1:23455"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:23456"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:21117"), "ws://127.0.0.1:21119");
        // 设置 relay-server 并带自定义端口
        Config::set_option("relay-server".to_string(), "127.0.0.1:34567".to_string());
        assert_eq!(check_ws("127.0.0.1:23455"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:23456"), "ws://127.0.0.1:23458");
        assert_eq!(check_ws("127.0.0.1:34567"), "ws://127.0.0.1:34569");

        // 设置 websocket-port，即在自有服务器上用非默认端口的反向代理
        Config::set_option(
            "custom-rendezvous-server".to_string(),
            "rustdesk.com".to_string(),
        );
        Config::set_option("relay-server".to_string(), "relay.rustdesk.com".to_string());
        Config::set_option(
            "api-server".to_string(),
            "https://rustdesk.com:8443".to_string(),
        );
        Config::set_option(keys::OPTION_WEBSOCKET_PORT.to_string(), "8443".to_string());
        assert_eq!(
            check_ws("rustdesk.com:21116"),
            "wss://rustdesk.com:8443/ws/id"
        );
        assert_eq!(
            check_ws("rustdesk.com:21117"),
            "wss://rustdesk.com:8443/ws/relay"
        );
        assert_eq!(
            check_ws("relay.rustdesk.com:21117"),
            "wss://relay.rustdesk.com:8443/ws/relay"
        );
        // 对端的服务器或另一台主机上的 relay 保持默认端口
        assert_eq!(check_ws("example.com:21116"), "wss://example.com/ws/id");
        assert_eq!(check_ws("example.com:21117"), "wss://example.com/ws/relay");
        assert_eq!(check_ws("127.0.0.1:21116"), "ws://127.0.0.1:21118");
        // 未设置 rendezvous 服务器时，公开回退地址不是我们自己的
        Config::set_option("custom-rendezvous-server".to_string(), "".to_string());
        let fallback = Config::get_rendezvous_server();
        let (fallback_host, _) = split_host_port(&fallback).unwrap();
        assert_eq!(
            check_ws(&fallback),
            format!("wss://{}/ws/id", fallback_host)
        );
        Config::set_options(options);
    }
    // 服务器到客户端的帧是不加掩码的，因此可以手工放到链路上：仅头部，
    // 就足以让 tungstenite 分配空间，或者连同负载一起。`head` 是
    // 第一个字节、FIN 和 opcode。
    fn ws_frame(head: u8, len: usize, with_payload: bool) -> Vec<u8> {
        let mut f = vec![head];
        if len < 126 {
            f.push(len as u8);
        } else if len <= u16::MAX as usize {
            f.push(0x7E);
            f.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            f.push(0x7F);
            f.extend_from_slice(&(len as u64).to_be_bytes());
        }
        if with_payload {
            f.resize(f.len() + len, 0xCD);
        }
        f
    }

    fn ws_binary_frame(len: usize, with_payload: bool) -> Vec<u8> {
        ws_frame(0x82, len, with_payload)
    }

    async fn ws_loopback() -> (WsFramedStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let ws = ws_from_tcp(client, addr).await;
        (ws, server)
    }

    #[tokio::test]
    async fn max_packet_length_refuses_a_frame_on_its_header() {
        const CAP: usize = 16 * 1024;
        let (mut ws, mut server) = ws_loopback().await;
        ws.set_max_packet_length(CAP);

        // 超出一个字节、仅头部：在有任何负载需要缓冲之前就被拒绝。
        server
            .write_all(&ws_binary_frame(CAP + 1, false))
            .await
            .unwrap();
        match timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Err(e))) => assert!(e.to_string().contains("Message too long"), "{}", e),
            Ok(other) => panic!(
                "expected a refusal, got {:?}",
                other.map(|r| r.map(|b| b.len()))
            ),
            Err(_) => panic!("next() waited for a payload the header should have refused"),
        }
    }

    #[tokio::test]
    async fn max_packet_length_lowered_then_restored() {
        const CAP: usize = 16 * 1024;
        let (mut ws, mut server) = ws_loopback().await;

        ws.set_max_packet_length(CAP);
        server
            .write_all(&ws_binary_frame(8 * 1024, true))
            .await
            .unwrap();
        let got = ws.next().await.unwrap().unwrap();
        assert_eq!(got.len(), 8 * 1024, "a message under the cap still arrives");

        ws.set_max_packet_length(usize::MAX);
        server
            .write_all(&ws_binary_frame(200_000, true))
            .await
            .unwrap();
        let got = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("next() hung after the cap was lifted")
            .unwrap()
            .unwrap();
        assert_eq!(
            got.len(),
            200_000,
            "lifting the cap lets a large message through again"
        );
    }

    // 两帧各自都在上限之下，但重组后的消息超过上限：帧上限允许
    // 两者都通过，因此这正是仅靠消息上限会被拒绝的情况。
    #[tokio::test]
    async fn max_packet_length_bounds_fragmented_message() {
        const CAP: usize = 16 * 1024;
        let (mut ws, mut server) = ws_loopback().await;
        ws.set_max_packet_length(CAP);

        // 先发一个 FIN 置 0 的二进制帧，再发一个 FIN 置 1 的续帧：各 12 KiB，合计 24 KiB。
        server
            .write_all(&ws_frame(0x02, 12 * 1024, true))
            .await
            .unwrap();
        server
            .write_all(&ws_frame(0x80, 12 * 1024, true))
            .await
            .unwrap();
        match timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Err(e))) => assert!(e.to_string().contains("Message too long"), "{}", e),
            Ok(other) => panic!(
                "expected a refusal, got {:?}",
                other.map(|r| r.map(|b| b.len()))
            ),
            Err(_) => panic!("next() hung on a fragmented message over the cap"),
        }
    }
}
