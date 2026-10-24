use std::sync::{Arc, RwLock};

use crate::client::*;
use crate::port_forward_mux::{Claim, Tunnel, CHANNEL_WINDOW};
use hbb_common::{
    allow_err, bail,
    config::READ_TIMEOUT,
    futures::{SinkExt, StreamExt},
    log,
    protobuf::Message as _,
    rendezvous_proto::ConnType,
    tcp, timeout,
    tokio::{self, net::TcpStream, sync::mpsc},
    tokio_util::codec::{BytesCodec, Framed},
    ResultType, Stream,
};
use base::message_proto::*;

fn run_rdp(port: u16, name: &str) {
    std::process::Command::new("cmdkey")
        .arg("/delete:localhost")
        .output()
        .ok();
    let username = std::env::var("rdp_username").unwrap_or_default();
    let password = std::env::var("rdp_password").unwrap_or_default();
    if !username.is_empty() || !password.is_empty() {
        let mut args = vec!["/generic:localhost".to_owned()];
        if !username.is_empty() {
            args.push(format!("/user:{}", username));
        }
        if !password.is_empty() {
            args.push(format!("/pass:{}", password));
        }
        std::process::Command::new("cmdkey")
            .args(&args)
            .output()
            .ok();
    }
    // 继续使用 /v 而不是生成 .rdp 文件：这样 mstsc 会保留
    // 用户的 Default.rdp 设置，并避免未签名文件的警告或策略限制。
    match std::process::Command::new("mstsc")
        .arg(format!("/v:localhost:{}", port))
        .spawn()
    {
        Ok(child) => {
            #[cfg(windows)]
            crate::platform::set_rdp_window_title(child, name.to_owned());
            #[cfg(not(windows))]
            let _ = (child, name);
        }
        Err(err) => log::warn!("Failed to launch mstsc: {}", err),
    }
}

// 显示对端身份及其主机名，没有别名时使用 ID。
fn rdp_display_name(lc: &Arc<RwLock<LoginConfigHandler>>, id: &str) -> String {
    let lc = lc.read().unwrap();
    let alias = lc
        .options
        .get("alias")
        .map(|s| s.trim())
        .unwrap_or_default();
    let hostname = lc.info.hostname.trim();
    let identity = if !alias.is_empty() { alias } else { id };
    if hostname.is_empty() || hostname == identity {
        identity.to_owned()
    } else {
        format!("{} ({})", identity, hostname)
    }
}

pub async fn listen(
    id: String,
    password: String,
    port: i32,
    interface: impl Interface,
    ui_receiver: mpsc::UnboundedReceiver<Data>,
    key: &str,
    token: &str,
    lc: Arc<RwLock<LoginConfigHandler>>,
    remote_host: String,
    remote_port: i32,
) -> ResultType<()> {
    let listener = tcp::new_listener(format!("127.0.0.1:{}", port), true).await?;
    let addr = listener.local_addr()?;
    log::info!("listening on port {:?}", addr);
    let is_rdp = port == 0;
    if is_rdp {
        run_rdp(addr.port(), &rdp_display_name(&lc, &id));
    }
    let mut ui_receiver = ui_receiver;
    // 每个映射对应一条隧道；监听器在它往外走的过程中把它丢掉，而这
    // 就结束了隧道。
    let tunnel = Tunnel::new();
    loop {
        tokio::select! {
            Ok((forward, addr)) = listener.accept() => {
                log::info!("new connection from {:?}", addr);
                // 多路复用窗口会在该映射的
                // 隧道上接管该连接，或在第一次 accept 时探测一条隧道。其他一切
                // 情况——设置关闭或对端不支持该特性——都走
                // 下面的 raw pipe，和以前一样。
                let claim = if lc.read().unwrap().port_forward_mux { tunnel.claim() } else { Claim::Legacy };
                match claim {
                    Claim::Muxed(handle) => {
                        if let Err(e) = handle.open(&remote_host, remote_port, forward, Vec::new()) {
                            log::debug!("cannot open channel for {:?}: {}", addr, e);
                        }
                        continue;
                    }
                    Claim::Claimed => {
                        if establish_tunnel(&tunnel, &id, &password, &mut ui_receiver, &interface, forward, addr, key, token, is_rdp, &remote_host, remote_port).await {
                            break;
                        }
                        continue;
                    }
                    Claim::Legacy => {}
                }
                let id = id.clone();
                let password = password.clone();
                let mut forward = Framed::new(forward, BytesCodec::new());
                let mut close_port_forward = false;
                match connect_and_login(&id, &password, &mut ui_receiver, interface.clone(), &mut forward, key, token, is_rdp, &mut close_port_forward, &remote_host, remote_port).await {
                    Ok(Some(stream)) => {
                        let interface = interface.clone();
                        tokio::spawn(async move {
                            if let Err(err) = run_forward(forward, stream).await {
                                interface.msgbox("error", "Error", &err.to_string(), "");
                            }
                            log::info!("connection from {:?} closed", addr);
                       });
                    }
                    _ if close_port_forward => {
                        break;
                    }
                    Err(err) => {
                        interface.on_establish_connection_error(err.to_string());
                    }
                    _ => {}
                }
            }
            d = ui_receiver.recv() => {
                match d {
                    Some(Data::Close) => {
                        break;
                    }
                    Some(Data::NewRDP) => {
                        println!("receive run_rdp from ui_receiver");
                        run_rdp(addr.port(), &rdp_display_name(&lc, &id));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn connect_and_login(
    id: &str,
    password: &str,
    ui_receiver: &mut mpsc::UnboundedReceiver<Data>,
    interface: impl Interface,
    forward: &mut Framed<TcpStream, BytesCodec>,
    key: &str,
    token: &str,
    is_rdp: bool,
    close_port_forward: &mut bool,
    remote_host: &str,
    remote_port: i32,
) -> ResultType<Option<Stream>> {
    let conn_type = if is_rdp {
        ConnType::RDP
    } else {
        ConnType::PORT_FORWARD
    };
    let ((mut stream, direct, _pk, _kcp, _stream_type), (feedback, rendezvous_server)) =
        Client::start(id, key, token, conn_type, interface.clone()).await?;
    interface.update_direct(Some(direct));
    if !stream.is_secured() && !crate::common::is_direct_ip_access(id) {
        if !confirm_insecure_connection(&interface, ui_receiver).await {
            * close_port_forward = true;
            return Ok(None);
        }
    }
    let mut buffer = Vec::new();
    let mut received = false;
    let mut challenge = None;
    let mut pending_login = None;

    let _keep_it = hc_connection(feedback, rendezvous_server, token).await;

    loop {
        tokio::select! {
            res = timeout(READ_TIMEOUT, stream.next()) => match res {
                Err(_) => {
                    bail!("Timeout");
                }
                Ok(Some(Ok(bytes))) => {
                    if !received {
                        received = true;
                        interface.update_received(true);
                    }
                    let msg_in = Message::parse_from_bytes(&bytes)?;
                    match msg_in.union {
                        Some(message::Union::Hash(hash)) => {
                            challenge = Some(hash.clone());
                            if !hash_arrived(&interface, password, hash, pending_login.take(), remote_host, remote_port, false, &mut stream).await {
                                return Ok(None);
                            }
                        }
                        Some(message::Union::LoginResponse(lr)) => match lr.union {
                            Some(login_response::Union::Error(err)) => {
                                if !interface.handle_login_error(&err) {
                                    return Ok(None);
                                }
                            }
                            Some(login_response::Union::PeerInfo(pi)) => {
                                interface.handle_peer_info(pi);
                                break;
                            }
                            _ => {}
                        }
                        Some(message::Union::TestDelay(t)) => {
                            interface.handle_test_delay(t, &mut stream).await;
                        }
                        _ => {}
                    }
                }
                Ok(Some(Err(err))) => {
                    bail!("Connection closed: {}", err);
                }
                _ => {
                    bail!("Reset by the peer");
                }
            },
            d = ui_receiver.recv() => {
                match d {
                    Some(Data::Login(login)) => match &challenge {
                        Some(hash) => login_from_ui(&interface, hash, login, remote_host, remote_port, false, &mut stream).await,
                        None => pending_login = Some(login),
                    },
                    Some(Data::Message(msg)) => {
                        allow_err!(stream.send(&msg).await);
                    }
                    _ => {}
                }
            },
            res = forward.next() => {
                if let Some(Ok(bytes)) = res {
                    buffer.extend(bytes);
                } else {
                    return Ok(None);
                }
            },
        }
    }
    stream.set_raw();
    if !buffer.is_empty() {
        allow_err!(stream.send_bytes(buffer.into()).await);
    }
    Ok(Some(stream))
}


/// 映射的登录信息由窗口的共享处理器构建：
/// `create_login_msg` 读取 `port_forward` 和 `port_forward_multiplex`，
/// `handle_login_from_ui` 读取 `hash`。映射会并发登录，因此每个
/// 都在窗口的轮次锁下填充并发送，或者一次登录被
/// 带到了另一个映射的目标上，或应答了另一个映射的质询。
async fn login_with_hash(
    interface: &impl Interface,
    password: &str,
    hash: Hash,
    remote_host: &str,
    remote_port: i32,
    mux: bool,
    stream: &mut Stream,
) -> bool {
    let lc = interface.get_lch();
    let turn = lc.read().unwrap().port_forward_login_turn.clone();
    let _turn = turn.lock().await;
    lc.write().unwrap().port_forward = (remote_host.to_owned(), remote_port);
    lc.write().unwrap().port_forward_multiplex = mux;
    interface.handle_hash(password, hash, stream).await
}

type UiLogin = (String, String, String, bool);

/// 该连接的 `Hash`。窗口的密码提示会广播给
/// 每个映射，且可能先到达这个映射，因此如果密码是在
/// `Hash` 还在路上时输入的，就会被保留下来并现在就应答它，而不是
/// 被丢弃，指望发起提示的那个映射已经把它
/// 存进了共享处理器。
async fn hash_arrived(
    interface: &impl Interface,
    password: &str,
    hash: Hash,
    pending_login: Option<UiLogin>,
    remote_host: &str,
    remote_port: i32,
    mux: bool,
    stream: &mut Stream,
) -> bool {
    match pending_login {
        Some(login) => {
            login_from_ui(interface, &hash, login, remote_host, remote_port, mux, stream).await;
            true
        }
        None => login_with_hash(interface, password, hash, remote_host, remote_port, mux, stream).await,
    }
}

/// 窗口的密码提示会广播给每个映射；这个映射
/// 用自己的质询来应答它。
async fn login_from_ui(
    interface: &impl Interface,
    hash: &Hash,
    login: UiLogin,
    remote_host: &str,
    remote_port: i32,
    mux: bool,
    stream: &mut Stream,
) {
    let lc = interface.get_lch();
    let turn = lc.read().unwrap().port_forward_login_turn.clone();
    let _turn = turn.lock().await;
    {
        let mut lc = lc.write().unwrap();
        lc.port_forward = (remote_host.to_owned(), remote_port);
        lc.port_forward_multiplex = mux;
        lc.set_hash(hash.clone());
    }
    let (os_username, os_password, password, remember) = login;
    interface
        .handle_login_from_ui(os_username, os_password, password, remember, stream)
        .await;
}

/// 多路复用映射的第一次 accept。它登录时索要
/// 隧道，而对端的回答会固定该监听器的模式直到它关闭：
/// 支持该特性的对端之后每次 accept 都会接入一条隧道，不支持的
/// 对端则本连接使用今天的 raw pipe，其余使用 `Legacy`。
/// 重新添加该映射就是用户接上升级后对端的方式；
/// 不会在活动连接之下切换模式。返回 `true`，当
/// 监听器应当停止时。
async fn establish_tunnel(
    tunnel: &Tunnel,
    id: &str,
    password: &str,
    ui_receiver: &mut mpsc::UnboundedReceiver<Data>,
    interface: &impl Interface,
    forward: TcpStream,
    addr: std::net::SocketAddr,
    key: &str,
    token: &str,
    is_rdp: bool,
    remote_host: &str,
    remote_port: i32,
) -> bool {
    let mut forward = Framed::new(forward, BytesCodec::new());
    let mut close_port_forward = false;
    match connect_and_login_mux(id, password, ui_receiver, interface.clone(), &mut forward, key, token, is_rdp, &mut close_port_forward, remote_host, remote_port).await {
        Ok(Some(outcome)) if outcome.mux => {
            let handle = tunnel.set_muxed(outcome.stream, interface.clone());
            if !outcome.local_eof {
                let (socket, prebuf) = take_socket(forward, outcome.prebuf);
                if let Err(e) = handle.open(remote_host, remote_port, socket, prebuf) {
                    log::debug!("cannot open channel for {:?}: {}", addr, e);
                }
            }
        }
        Ok(Some(outcome)) => {
            tunnel.set_legacy();
            if outcome.local_eof {
                log::debug!("legacy peer and local {:?} already gone", addr);
            } else {
                run_legacy(outcome, forward, addr, interface.clone());
            }
        }
        _ if close_port_forward => {
            tunnel.set_failed();
            return true;
        }
        Err(err) => {
            tunnel.set_failed();
            interface.on_establish_connection_error(err.to_string());
        }
        _ => tunnel.set_failed(),
    }
    false
}

/// 为需要隧道的映射调用 `connect_and_login`：预读
/// 会停在一个窗口大小，而不是无限增长，并且本地 EOF
/// 不再结束登录，因为隧道可能仍然需要。它
/// 报告对端的应答内容而不是原始流，因为
/// 调用方的下一步取决于它。登录本身是 raw pipe 的，
/// 被告知要索要隧道。
async fn connect_and_login_mux(
    id: &str,
    password: &str,
    ui_receiver: &mut mpsc::UnboundedReceiver<Data>,
    interface: impl Interface,
    forward: &mut Framed<TcpStream, BytesCodec>,
    key: &str,
    token: &str,
    is_rdp: bool,
    close_port_forward: &mut bool,
    remote_host: &str,
    remote_port: i32,
) -> ResultType<Option<LoginOutcome>> {
    let conn_type = if is_rdp {
        ConnType::RDP
    } else {
        ConnType::PORT_FORWARD
    };
    let ((mut stream, direct, _pk, _kcp, _stream_type), (feedback, rendezvous_server)) =
        Client::start(id, key, token, conn_type, interface.clone()).await?;
    interface.update_direct(Some(direct));
    if !stream.is_secured() && !crate::common::is_direct_ip_access(id) {
        if !confirm_insecure_connection(&interface, ui_receiver).await {
            * close_port_forward = true;
            return Ok(None);
        }
    }
    let mut buffer = Vec::new();
    let mut local_eof = false;
    let mux;
    let mut received = false;
    let mut challenge = None;
    let mut pending_login = None;

    let _keep_it = hc_connection(feedback, rendezvous_server, token).await;

    loop {
        tokio::select! {
            res = timeout(READ_TIMEOUT, stream.next()) => match res {
                Err(_) => {
                    bail!("Timeout");
                }
                Ok(Some(Ok(bytes))) => {
                    if !received {
                        received = true;
                        interface.update_received(true);
                    }
                    let msg_in = Message::parse_from_bytes(&bytes)?;
                    match msg_in.union {
                        Some(message::Union::Hash(hash)) => {
                            challenge = Some(hash.clone());
                            if !hash_arrived(&interface, password, hash, pending_login.take(), remote_host, remote_port, true, &mut stream).await {
                                return Ok(None);
                            }
                        }
                        Some(message::Union::LoginResponse(lr)) => match lr.union {
                            Some(login_response::Union::Error(err)) => {
                                if !interface.handle_login_error(&err) {
                                    return Ok(None);
                                }
                            }
                            Some(login_response::Union::PeerInfo(pi)) => {
                                mux = peer_supports_mux(&pi);
                                interface.handle_peer_info(pi);
                                break;
                            }
                            _ => {}
                        }
                        Some(message::Union::TestDelay(t)) => {
                            interface.handle_test_delay(t, &mut stream).await;
                        }
                        _ => {}
                    }
                }
                Ok(Some(Err(err))) => {
                    bail!("Connection closed: {}", err);
                }
                _ => {
                    bail!("Reset by the peer");
                }
            },
            d = ui_receiver.recv() => {
                match d {
                    Some(Data::Login(login)) => match &challenge {
                        Some(hash) => login_from_ui(&interface, hash, login, remote_host, remote_port, true, &mut stream).await,
                        None => pending_login = Some(login),
                    },
                    Some(Data::Message(msg)) => {
                        allow_err!(stream.send(&msg).await);
                    }
                    _ => {}
                }
            },
            // 一旦预读缓冲区达到一个窗口深度就停止拉取；其余的
            // 在内核中等待，直到通道打开。本地 EOF
            // 不再中止登录：隧道可能仍然需要。
            res = forward.next(), if !local_eof && buffer.len() < CHANNEL_WINDOW as usize => {
                if let Some(Ok(bytes)) = res {
                    buffer.extend(bytes);
                } else {
                    local_eof = true;
                }
            },
        }
    }
    Ok(Some(LoginOutcome {
        stream,
        mux,
        prebuf: buffer,
        local_eof,
    }))
}

/// 今天的 raw pipe，用于不支持多路复用的对端。
fn run_legacy(
    outcome: LoginOutcome,
    forward: Framed<TcpStream, BytesCodec>,
    addr: std::net::SocketAddr,
    interface: impl Interface,
) {
    let mut stream = outcome.stream;
    let prebuf = outcome.prebuf;
    tokio::spawn(async move {
        stream.set_raw();
        if !prebuf.is_empty() {
            allow_err!(stream.send_bytes(prebuf.into()).await);
        }
        if let Err(err) = run_forward(forward, stream).await {
            interface.msgbox("error", "Error", &err.to_string(), "");
        }
        log::info!("connection from {:?} closed", addr);
    });
}

struct LoginOutcome {
    stream: Stream,
    mux: bool,
    prebuf: Vec<u8>,
    local_eof: bool,
}

fn peer_supports_mux(pi: &PeerInfo) -> bool {
    pi.features.as_ref().map(|f| f.port_forward_mux).unwrap_or(false)
}

/// `into_inner()` 会丢掉 codec 已取出但从未交付的字节。
fn take_socket(forward: Framed<TcpStream, BytesCodec>, mut prebuf: Vec<u8>) -> (TcpStream, Vec<u8>) {
    let parts = forward.into_parts();
    prebuf.extend_from_slice(&parts.read_buf);
    (parts.io, prebuf)
}

/// 控制端的 `enable-port-forward-mux`：除非设为 `N`，否则为开启。
pub fn mux_enabled() -> bool {
    use hbb_common::config::{option2bool, LocalConfig};
    use base::config::keys;
    option2bool(
        keys::OPTION_ENABLE_PORT_FORWARD_MUX,
        &LocalConfig::get_option(keys::OPTION_ENABLE_PORT_FORWARD_MUX),
    )
}

async fn run_forward(forward: Framed<TcpStream, BytesCodec>, stream: Stream) -> ResultType<()> {
    log::info!("new port forwarding connection started");
    let mut forward = forward;
    let mut stream = stream;
    loop {
        tokio::select! {
            res = forward.next() => {
                if let Some(Ok(bytes)) = res {
                    allow_err!(stream.send_bytes(bytes.into()).await);
                } else {
                    break;
                }
            },
            res = stream.next() => {
                if let Some(Ok(bytes)) = res {
                    allow_err!(forward.send(bytes).await);
                } else {
                    break;
                }
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod login_tests {
    use super::*;
    use async_trait::async_trait;
    use hbb_common::{
        tcp::FramedStream,
        tokio::time::{sleep, Duration},
    };
    use sha2::{Digest, Sha256};

    /// 窗口对其共享处理器的接口。`handle_hash` 可以在
    /// 构建登录信息之前暂停，真正的实现则在其中查找密码。
    #[derive(Clone)]
    struct Ui {
        lc: Arc<RwLock<LoginConfigHandler>>,
        pause: Duration,
    }

    #[async_trait]
    impl Interface for Ui {
        fn send(&self, _data: Data) {}
        fn msgbox(&self, _msgtype: &str, _title: &str, _text: &str, _link: &str) {}
        fn handle_login_error(&self, _err: &str) -> bool {
            false
        }
        fn handle_peer_info(&self, _pi: PeerInfo) {}
        fn set_multiple_windows_session(&self, _sessions: Vec<WindowsSession>) {}
        async fn handle_hash(&self, pass: &str, hash: Hash, peer: &mut Stream) -> bool {
            sleep(self.pause).await;
            crate::client::handle_hash(self.lc.clone(), pass, hash, self, peer).await
        }
        async fn handle_login_from_ui(
            &self,
            os_username: String,
            os_password: String,
            password: String,
            remember: bool,
            peer: &mut Stream,
        ) {
            crate::client::handle_login_from_ui(
                self.lc.clone(),
                os_username,
                os_password,
                password,
                remember,
                peer,
            )
            .await
        }
        async fn handle_test_delay(&self, _t: TestDelay, _peer: &mut Stream) {}
        fn get_lch(&self) -> Arc<RwLock<LoginConfigHandler>> {
            self.lc.clone()
        }
    }

    fn window() -> Ui {
        let mut lc = LoginConfigHandler::default();
        lc.conn_type = ConnType::PORT_FORWARD;
        Ui {
            lc: Arc::new(RwLock::new(lc)),
            pause: Duration::ZERO,
        }
    }

    /// 一条连接的两端（我方、对端）。
    async fn loopback() -> (Stream, Stream) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = l.accept().await.unwrap();
        (
            Stream::Tcp(FramedStream::from(client, addr)),
            Stream::Tcp(FramedStream::from(server, addr)),
        )
    }

    async fn login_at(peer: &mut Stream) -> LoginRequest {
        let bytes = peer.next().await.unwrap().unwrap();
        Message::parse_from_bytes(&bytes)
            .unwrap()
            .login_request()
            .clone()
    }

    fn target(lr: &LoginRequest) -> (String, i32) {
        (lr.port_forward().host.clone(), lr.port_forward().port)
    }

    fn hash(challenge: &str) -> Hash {
        Hash {
            salt: "salt".to_owned(),
            challenge: challenge.to_owned(),
            ..Default::default()
        }
    }

    /// 在 `hash(challenge)` 下对端期望的密码 `pw` 对应值。
    fn digest(challenge: &str) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update("pw");
        h.update("salt");
        let salted = h.finalize();
        let mut h2 = Sha256::new();
        h2.update(&salted[..]);
        h2.update(challenge);
        h2.finalize()[..].to_vec()
    }

    #[test]
    fn mappings_logging_in_at_once_each_carry_their_own_target() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut ui = window();
            ui.pause = Duration::from_millis(50);
            let (mut a, mut a_peer) = loopback().await;
            let (mut b, mut b_peer) = loopback().await;
            tokio::join!(
                login_with_hash(&ui, "pw", hash("a"), "a", 1, false, &mut a),
                login_with_hash(&ui, "pw", hash("b"), "b", 2, false, &mut b),
            );
            assert_eq!(target(&login_at(&mut a_peer).await), ("a".to_owned(), 1));
            assert_eq!(target(&login_at(&mut b_peer).await), ("b".to_owned(), 2));
        });
    }

    #[test]
    fn a_mapping_answers_the_prompt_with_its_own_challenge() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ui = window();
            let (mut a, mut a_peer) = loopback().await;
            let (mut b, mut b_peer) = loopback().await;
            // A 的 hash 最后到达，所以处理器持有的是它。
            assert!(login_with_hash(&ui, "pw", hash("a"), "a", 1, false, &mut a).await);
            login_at(&mut a_peer).await;
            let typed = (String::new(), String::new(), "pw".to_owned(), false);
            login_from_ui(&ui, &hash("b"), typed, "b", 2, false, &mut b).await;
            let lr = login_at(&mut b_peer).await;
            assert_eq!(lr.password, digest("b"));
            assert_eq!(target(&lr), ("b".to_owned(), 2));
        });
    }

    #[test]
    fn a_probing_login_asks_for_the_tunnel() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ui = window();
            let (mut a, mut a_peer) = loopback().await;
            assert!(login_with_hash(&ui, "pw", hash("a"), "a", 1, true, &mut a).await);
            assert!(login_at(&mut a_peer).await.port_forward().multiplex);
        });
    }
}
