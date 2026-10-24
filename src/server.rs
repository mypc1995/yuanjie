use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};

use bytes::Bytes;

pub use connection::*;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::config::Config2;
use hbb_common::tcp::{self, new_listener};
use hbb_common::{
    allow_err,
    anyhow::Context,
    bail,
    config::{Config, CONNECT_TIMEOUT, RELAY_PORT},
    log,
    protobuf::{Enum, Message as _},
    rendezvous_proto::*,
    socket_client,
    sodiumoxide::crypto::{box_, sign},
    timeout, tokio,
    websocket::{check_ws, is_ws_endpoint},
    ResultType, Stream,
};
use base::message_proto::*;
use scrap::camera;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use service::ServiceTmpl;
use service::{EmptyExtraFieldService, GenericService, Service, Subscriber};
use video_service::VideoSource;

use crate::ipc::Data;

pub mod audio_service;
#[cfg(target_os = "windows")]
pub mod terminal_helper;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod terminal_service;
cfg_if::cfg_if! {
if #[cfg(not(target_os = "ios"))] {
mod clipboard_service;
#[cfg(target_os = "android")]
pub use clipboard_service::is_clipboard_service_ok;
#[cfg(target_os = "linux")]
pub(crate) mod wayland;
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) mod drm_capturer;
#[cfg(target_os = "linux")]
pub mod uinput;
#[cfg(target_os = "linux")]
pub mod rdp_input;
#[cfg(target_os = "linux")]
pub mod dbus;
#[cfg(not(target_os = "android"))]
pub mod input_service;
} else {
mod clipboard_service {
pub const NAME: &'static str = "";
}
}
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub mod input_service {
    pub const NAME_CURSOR: &'static str = "";
    pub const NAME_POS: &'static str = "";
    pub const NAME_WINDOW_FOCUS: &'static str = "";
}

mod connection;
mod login_failure_check;
pub(crate) mod port_forward_mux;
pub mod display_service;
#[cfg(windows)]
pub mod portable_service;
mod service;
mod video_qos;
pub mod video_service;

#[cfg(all(target_os = "windows", feature = "flutter"))]
pub mod printer_service;

pub type Childs = Arc<Mutex<Vec<std::process::Child>>>;
type ConnMap = HashMap<i32, ConnInner>;

#[derive(Clone, Default)]
pub struct ConnectionMeta {
    pub control_permissions: Option<ControlPermissions>,
    pub controlled_context: Option<ControlledContext>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
const CONFIG_SYNC_INTERVAL_SECS: f32 = 0.3;
#[cfg(any(target_os = "macos", target_os = "linux"))]
// 3s 足以完成至少一次初始同步尝试：
// 0.3s 退避 + 最多 1s 连接超时 + 最多 1s 响应超时。
const CONFIG_SYNC_INITIAL_WAIT_SECS: u64 = 3;

lazy_static::lazy_static! {
    pub static ref CHILD_PROCESS: Childs = Default::default();
    // 一个客户端服务器，用于提供本地服务（音频、视频、剪贴板等）
    // 适用于所有主动发起的连接。
    //
    // [注意]
    // 丑陋
    // 现在我们用这个 [`CLIENT_SERVER`] 来完成以下操作：
    // - 录制本地音频并发送到远端
    pub static ref CLIENT_SERVER: ServerPtr = new();
}

pub struct Server {
    connections: ConnMap,
    services: HashMap<String, Box<dyn Service>>,
    id_count: i32,
}

pub type ServerPtr = Arc<RwLock<Server>>;
pub type ServerPtrWeak = Weak<RwLock<Server>>;

#[cfg(test)]
pub fn new_for_test() -> ServerPtr {
    Arc::new(RwLock::new(Server {
        connections: HashMap::new(),
        services: HashMap::new(),
        id_count: 1000,
    }))
}

pub fn new() -> ServerPtr {
    let mut server = Server {
        connections: HashMap::new(),
        services: HashMap::new(),
        id_count: hbb_common::rand::random::<i32>() % 1000 + 1000, // ensure positive
    };
    server.add_service(Box::new(audio_service::new()));
    #[cfg(not(target_os = "ios"))]
    {
        server.add_service(Box::new(display_service::new()));
        server.add_service(Box::new(clipboard_service::new(
            clipboard_service::NAME.to_owned(),
        )));
        #[cfg(feature = "unix-file-copy-paste")]
        server.add_service(Box::new(clipboard_service::new(
            clipboard_service::FILE_NAME.to_owned(),
        )));
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if !display_service::capture_cursor_embedded() {
            server.add_service(Box::new(input_service::new_cursor()));
            server.add_service(Box::new(input_service::new_pos()));
            #[cfg(target_os = "linux")]
            if scrap::is_x11() {
                // wayland 目前不支持多显示器
                server.add_service(Box::new(input_service::new_window_focus()));
            }
            #[cfg(not(target_os = "linux"))]
            server.add_service(Box::new(input_service::new_window_focus()));
        }
    }
    #[cfg(all(target_os = "windows", feature = "flutter"))]
    {
        match printer_service::init(&crate::get_app_name()) {
            Ok(()) => {
                log::info!("printer service initialized");
                server.add_service(Box::new(printer_service::new(
                    printer_service::NAME.to_owned(),
                )));
            }
            Err(e) => {
                log::error!("printer service init failed: {}", e);
            }
        }
    }
    // 终端服务按连接创建，而不是全局创建
    Arc::new(RwLock::new(server))
}

async fn accept_connection_(
    server: ServerPtr,
    socket: Stream,
    secure: bool,
    meta: ConnectionMeta,
    slot: crate::rendezvous_mediator::PunchSlot,
) -> ResultType<()> {
    let local_addr = socket.local_addr();
    drop(socket);
    // 即使我们丢弃了套接字，下面仍可能失败，如果没有使用 reuse_addr，
    // 在套接字真正释放之前存在 TIME_WAIT，所以有时我们
    // 在 windows 上会看到 "Only one usage of each socket address is normally permitted"，
    let listener = new_listener(local_addr, true).await?;
    log::info!("Server listening on: {}", &listener.local_addr()?);
    if let Ok((stream, addr)) = timeout(CONNECT_TIMEOUT, listener.accept()).await? {
        // 对端已接入：槽位在会话运行前归还，就像每次打洞的那样。
        drop(slot);
        stream.set_nodelay(true).ok();
        let stream_addr = stream.local_addr()?;
        create_tcp_connection(
            server,
            Stream::from(stream, stream_addr),
            addr,
            secure,
            meta,
        )
        .await?;
    }
    Ok(())
}

pub async fn create_tcp_connection(
    server: ServerPtr,
    stream: Stream,
    addr: SocketAddr,
    secure: bool,
    meta: ConnectionMeta,
) -> ResultType<()> {
    let mut stream = stream;
    // 连接层所依据的地址，白名单和准入同样如此。
    let addr = hbb_common::try_into_v4(addr);
    let id = server.write().unwrap().get_new_id();
    // 在身份握手之前就被准入，所以卡在握手中、或握手后未
    // 登录的对端会一直占着它的槽位；超出份额的地址会被拒绝。
    let Some(unauthorized) = admit_unauthorized(id, addr.ip()) else {
        bail!("too many unauthenticated connections from {}", addr.ip());
    };
    // 在握手之前，所以它的读取也是有界的；在授权时再次解除。
    stream.set_max_packet_length(MAX_UNAUTHORIZED_MESSAGE);
    tokio::select! {
        handshake = identity_handshake(&mut stream, secure) => handshake?,
        _ = unauthorized.evicted() => {
            bail!("evicted to make room for a newer unauthenticated connection");
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        if let Ok(task) = Command::new("/usr/bin/caffeinate")
            .arg("-u")
            .arg("-t 5")
            .spawn()
        {
            super::CHILD_PROCESS.lock().unwrap().push(task);
        }
        log::info!("wake up macos");
    }
    Connection::start(
        addr,
        stream,
        id,
        Arc::downgrade(&server),
        meta,
        unauthorized,
    )
    .await;
    Ok(())
}

/// 我们签名的身份被发出，当 `secure` 时，控制端的回复以 `stream` 为键。
/// 单独存放，以便能与管理连接淘汰赛跑。
async fn identity_handshake(stream: &mut Stream, secure: bool) -> ResultType<()> {
    let (sk, pk) = Config::get_key_pair();
    if secure && pk.len() == sign::PUBLICKEYBYTES && sk.len() == sign::SECRETKEYBYTES {
        let mut sk_ = [0u8; sign::SECRETKEYBYTES];
        sk_[..].copy_from_slice(&sk);
        let sk = sign::SecretKey(sk_);
        let mut msg_out = Message::new();
        let (our_pk_b, our_sk_b) = box_::gen_keypair();
        // 在 WebRTC 传输上，把我们的 DTLS 证书指纹与我们的签名身份绑定，这样
        // 控制端就能验证它协商出的 DTLS 通道确实终结于我们这里
        // （而不是某个替换了 SDP 指纹的 rendezvous/relay）。其他传输上为空。
        // 在 WebRTC 上，如果本地指纹不可用就立即失败：签署 "" 只会
        // 让客户端在浪费一次往返之后才失败关闭。
        let dtls_fingerprint = stream.dtls_fingerprint(true).await.unwrap_or_default();
        if stream.is_webrtc() && dtls_fingerprint.is_empty() {
            bail!("WebRTC local DTLS fingerprint unavailable");
        }
        // WebRTC 流由 DTLS 加密，本身不携带流密钥，无论是否拆分，
        // 所以它宣告的就是它实际使用的：0。在那里写 1 会让将来
        // 在 WebRTC 上拆分密钥的控制端，与这一侧从未应用的版本达成一致。
        let advertised = if stream.is_webrtc() {
            0
        } else {
            tcp::KX_VERSION_LATEST
        };
        msg_out.set_signed_id(SignedId {
            id: sign::sign(
                &IdPk {
                    id: Config::get_id(),
                    pk: Bytes::from(our_pk_b.0.to_vec()),
                    dtls_fingerprint,
                    kx_version: advertised,
                    ..Default::default()
                }
                .write_to_bytes()
                .unwrap_or_default(),
                &sk,
            )
            .into(),
            ..Default::default()
        });
        timeout(CONNECT_TIMEOUT, stream.send(&msg_out)).await??;
        match timeout(CONNECT_TIMEOUT, stream.next()).await? {
            Some(res) => {
                let bytes = res?;
                if let Ok(msg_in) = Message::parse_from_bytes(&bytes) {
                    if let Some(message::Union::PublicKey(pk)) = msg_in.union {
                        if pk.asymmetric_value.len() == box_::PUBLICKEYBYTES {
                            let key = tcp::Encrypt::decode(
                                &pk.symmetric_value,
                                &pk.asymmetric_value,
                                &our_sk_b,
                            )?;
                            if pk.kx_version > advertised {
                                bail!(
                                    "Handshake failed: key exchange version {} not offered, {} was",
                                    pk.kx_version,
                                    advertised
                                );
                            }
                            stream.set_negotiated_key(
                                key,
                                false,
                                &tcp::KxTranscript {
                                    initiator_pk: &pk.asymmetric_value,
                                    responder_pk: &our_pk_b.0,
                                    advertised,
                                    picked: pk.kx_version,
                                },
                            )?;
                        } else if pk.asymmetric_value.is_empty() {
                            Config::set_key_confirmed(false);
                            log::info!("Force to update pk");
                        } else {
                            bail!("Handshake failed: invalid public sign key length from peer");
                        }
                    } else {
                        log::error!("Handshake failed: invalid message type");
                    }
                } else {
                    bail!("Handshake failed: invalid message format");
                }
            }
            None => {
                bail!("Failed to receive public key");
            }
        }
    }

    Ok(())
}

pub(crate) async fn accept_connection(
    server: ServerPtr,
    socket: Stream,
    peer_addr: SocketAddr,
    secure: bool,
    meta: ConnectionMeta,
    slot: crate::rendezvous_mediator::PunchSlot,
) {
    if let Err(err) = accept_connection_(server, socket, secure, meta, slot).await {
        log::warn!("Failed to accept connection from {}: {}", peer_addr, err);
    }
}

pub async fn create_relay_connection(
    server: ServerPtr,
    relay_server: String,
    uuid: String,
    peer_addr: SocketAddr,
    secure: bool,
    ipv4: bool,
    meta: ConnectionMeta,
) {
    if let Err(err) = create_relay_connection_(
        server,
        relay_server,
        uuid.clone(),
        peer_addr,
        secure,
        ipv4,
        meta,
    )
    .await
    {
        log::error!(
            "Failed to create relay connection for {} with uuid {}: {}",
            peer_addr,
            uuid,
            err
        );
    }
}

async fn create_relay_connection_(
    server: ServerPtr,
    relay_server: String,
    uuid: String,
    peer_addr: SocketAddr,
    secure: bool,
    ipv4: bool,
    meta: ConnectionMeta,
) -> ResultType<()> {
    // 代理的地址族不约束中继目标。
    let relay_server = crate::check_port(relay_server, RELAY_PORT);
    let use_ws_proxy = Config::is_proxy() && is_ws_endpoint(&check_ws(&relay_server));
    let mut stream = socket_client::connect_tcp(
        socket_client::ipv4_to_ipv6(relay_server, ipv4 || use_ws_proxy),
        CONNECT_TIMEOUT,
    )
    .await?;
    let mut msg_out = RendezvousMessage::new();
    let licence_key = crate::get_key(true).await;
    msg_out.set_request_relay(RequestRelay {
        licence_key,
        uuid,
        ..Default::default()
    });
    stream.send(&msg_out).await?;
    create_tcp_connection(server, stream, peer_addr, secure, meta).await?;
    Ok(())
}

impl Server {
    fn is_video_service_name(name: &str) -> bool {
        name.starts_with(VideoSource::Monitor.service_name_prefix())
            || name.starts_with(VideoSource::Camera.service_name_prefix())
    }

    pub fn try_add_primary_camera_service(&mut self) {
        if !camera::primary_camera_exists() {
            return;
        }
        let primary_camera_name =
            video_service::get_service_name(VideoSource::Camera, camera::PRIMARY_CAMERA_IDX);
        if !self.contains(&primary_camera_name) {
            self.add_service(Box::new(video_service::new(
                VideoSource::Camera,
                camera::PRIMARY_CAMERA_IDX,
            )));
        }
    }

    pub fn try_add_monitor_service(&mut self, display_idx: usize) {
        let monitor_service_name =
            video_service::get_service_name(VideoSource::Monitor, display_idx);
        if !self.contains(&monitor_service_name) {
            self.add_service(Box::new(video_service::new(
                VideoSource::Monitor,
                display_idx,
            )));
        }
    }

    pub fn add_camera_connection(&mut self, conn: ConnInner) {
        if camera::primary_camera_exists() {
            let primary_camera_name =
                video_service::get_service_name(VideoSource::Camera, camera::PRIMARY_CAMERA_IDX);
            if let Some(s) = self.services.get(&primary_camera_name) {
                s.on_subscribe(conn.clone());
            }
        }
        self.connections.insert(conn.id(), conn);
    }

    pub fn add_monitor_connection(
        &mut self,
        conn: ConnInner,
        noperms: &Vec<&'static str>,
        display_idx: usize,
    ) {
        let monitor_service_name =
            video_service::get_service_name(VideoSource::Monitor, display_idx);
        for s in self.services.values() {
            let name = s.name();
            if Self::is_video_service_name(&name) && name != monitor_service_name {
                continue;
            }
            if !noperms.contains(&(&name as _)) {
                s.on_subscribe(conn.clone());
            }
        }
        #[cfg(target_os = "macos")]
        self.update_enable_retina();
        self.connections.insert(conn.id(), conn);
    }

    pub fn remove_connection(&mut self, conn: &ConnInner) {
        for s in self.services.values() {
            s.on_unsubscribe(conn.id());
        }
        self.connections.remove(&conn.id());
        #[cfg(target_os = "macos")]
        self.update_enable_retina();
    }

    pub fn close_connections(&mut self) {
        let conn_inners: Vec<_> = self.connections.values_mut().collect();
        for c in conn_inners {
            let mut misc = Misc::new();
            misc.set_stop_service(true);
            let mut msg = Message::new();
            msg.set_misc(misc);
            c.send(Arc::new(msg));
        }
    }

    fn add_service(&mut self, service: Box<dyn Service>) {
        let name = service.name();
        self.services.insert(name, service);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.services.contains_key(name)
    }

    pub fn subscribe(&mut self, name: &str, conn: ConnInner, sub: bool) {
        if let Some(s) = self.services.get(name) {
            if s.is_subed(conn.id()) == sub {
                return;
            }
            if sub {
                s.on_subscribe(conn.clone());
            } else {
                s.on_unsubscribe(conn.id());
            }
            #[cfg(target_os = "macos")]
            self.update_enable_retina();
        }
    }

    // 获取一个新的唯一 id
    pub fn get_new_id(&mut self) -> i32 {
        self.id_count += 1;
        self.id_count
    }

    pub fn set_video_service_opt(
        &self,
        display: Option<(VideoSource, usize)>,
        opt: &str,
        value: &str,
    ) {
        for (k, v) in self.services.iter() {
            if let Some((source, display)) = display {
                if k != &video_service::get_service_name(source, display) {
                    continue;
                }
            }

            if Self::is_video_service_name(k) {
                v.set_option(opt, value);
            }
        }
    }

    fn get_subbed_displays_count(&self, conn_id: i32) -> usize {
        self.services
            .keys()
            .filter(|k| {
                Self::is_video_service_name(k)
                    && self
                        .services
                        .get(*k)
                        .map(|s| s.is_subed(conn_id))
                        .unwrap_or(false)
            })
            .count()
    }

    fn capture_displays(
        &mut self,
        conn: ConnInner,
        source: VideoSource,
        displays: &[usize],
        include: bool,
        exclude: bool,
    ) {
        let displays = displays
            .iter()
            .map(|d| video_service::get_service_name(source, *d))
            .collect::<Vec<_>>();
        let keys = self.services.keys().cloned().collect::<Vec<_>>();
        for name in keys.iter() {
            if Self::is_video_service_name(&name) {
                if displays.contains(&name) {
                    if include {
                        self.subscribe(&name, conn.clone(), true);
                    }
                } else {
                    if exclude {
                        self.subscribe(&name, conn.clone(), false);
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn update_enable_retina(&self) {
        let mut video_service_count = 0;
        for (name, service) in self.services.iter() {
            if Self::is_video_service_name(&name) && service.ok() {
                video_service_count += 1;
            }
        }
        * scrap::quartz::ENABLE_RETINA.lock().unwrap() = video_service_count < 2;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for s in self.services.values() {
            s.join();
        }
        #[cfg(target_os = "linux")]
        wayland::clear();
    }
}

pub fn check_zombie() {
    std::thread::spawn(|| loop {
        let mut lock = CHILD_PROCESS.lock().unwrap();
        let mut i = 0;
        while i != lock.len() {
            let c = &mut (*lock)[i];
            if let Ok(Some(_)) = c.try_wait() {
                lock.remove(i);
            } else {
                i += 1;
            }
        }
        drop(lock);
        std::thread::sleep(Duration::from_millis(100));
    });
}

/// 启动宿主服务，使远端对端可以控制当前机器。
///
/// # 参数
///
/// * `is_server` - 当前客户端是否确定是服务端。
/// 为 true 时将启动服务端。
/// 否则客户端会检查是否已存在服务端，若没有则启动一个。
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tokio::main]
pub async fn start_server(_is_server: bool) {
    crate::RendezvousMediator::start_all().await;
}

/// 启动宿主服务，使远端对端可以控制当前机器。
///
/// # 参数
///
/// * `is_server` - 当前客户端是否确定是服务端。
/// 为 true 时将启动服务端。
/// 否则客户端会检查是否已存在服务端，若没有则启动一个。
/// * `no_server` - 当 `is_server` 为 false 时，未找到服务端是否要启动一个。
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tokio::main]
pub async fn start_server(is_server: bool, no_server: bool) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "linux")]
        {
            log::info!("DISPLAY={:?}", std::env::var("DISPLAY"));
            log::info!("XAUTHORITY={:?}", std::env::var("XAUTHORITY"));
        }
        #[cfg(windows)]
        base::platform::windows::start_cpu_performance_monitor();
    });

    if is_server {
        crate::common::set_server_running(true);
        std::thread::spawn(move || {
            if let Err(err) = crate::ipc::start("") {
                log::error!("Failed to start ipc: {}", err);
                if crate::is_server() {
                    log::error!("ipc is occupied by another process, try kill it");
                    std::thread::spawn(stop_main_window_process).join().ok();
                }
                std::process::exit(-1);
            }
        });
        // 在任何客户端连接之前预热 DRM 可用性缓存，这样首次连接不会
        // 与冷启动的 `_drm` 探测赛跑而返回空的显示器列表（"No displays" + 重试）。
        // X11 会被跳过——在那里探测会让 root 服务为本会话
        // 根本走不到的路径打开 DRM reader——但这个决定属于 `warm_availability`，它已经
        // 做出了这个决定，而不是这个调用点。在这里决定就等于重复了启动时一次性的
        // 预热犯过的错误：只要 loginctl 还无法指出 seat0 会话，`is_x11()` 就回答 "x11"，
        // 而启动过程中恰恰正是它运行的时候，且没有任何东西会重新检查它——
        // 所以启动缓慢的 Wayland 宿主会在整个进程生命周期内跳过预热，
        // 又出现了预热本该消除的冷探测 "No displays" 症状。
        #[cfg(all(target_os = "linux", feature = "drm"))]
        if let Err(err) = std::thread::Builder::new()
            .name("drm-warm".into())
            .spawn(drm_capturer::warm_availability)
        {
            // 与 root 服务的启动线程同样的原因：`thread::spawn` 在 EAGAIN 时会 panic，
            // 那会中止 `start_server`。跳过预热会让首个会话付出
            // 冷探测的代价，而这正是预热出现之前的情况。
            log::warn!("drm: could not spawn the availability warm ({err}); skipping it");
        }
        input_service::fix_key_down_timeout_loop();
        #[cfg(target_os = "linux")]
        if input_service::wayland_use_uinput() {
            allow_err!(input_service::setup_uinput(0, 1920, 0, 1080).await);
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        wait_initial_config_sync().await;
        #[cfg(target_os = "windows")]
        crate::platform::try_kill_broker();
        #[cfg(feature = "hwcodec")]
        scrap::hwcodec::start_check_process();
        crate::RendezvousMediator::start_all().await;
    } else {
        match crate::ipc::connect(1000, "").await {
            Ok(mut conn) => {
                if conn.send(&Data::SyncConfig(None)).await.is_ok() {
                    if let Ok(Some(data)) = conn.next_timeout(1000).await {
                        match data {
                            Data::SyncConfig(Some(configs)) => {
                                let (config, config2) = *configs;
                                if Config::set(config) {
                                    log::info!("config synced");
                                }
                                if Config2::set(config2) {
                                    log::info!("config2 synced");
                                }
                            }
                            _ => {}
                        }
                    }
                }
                #[cfg(feature = "hwcodec")]
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                crate::ipc::client_get_hwcodec_config_thread(0);
            }
            Err(err) => {
                log::info!("server not started: {err:?}, no_server: {no_server}");
                if no_server {
                    hbb_common::sleep(1.0).await;
                    std::thread::spawn(|| start_server(false, true));
                } else {
                    log::info!("try start server");
                    std::thread::spawn(|| start_server(true, false));
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::main(flavor = "current_thread")]
pub async fn start_ipc_url_server() {
    log::debug!("Start an ipc server for listening to url schemes");
    match crate::ipc::new_listener("_url").await {
        Ok(mut incoming) => {
            while let Some(Ok(conn)) = incoming.next().await {
                let mut conn = crate::ipc::Connection::new(conn);
                match conn.next_timeout(1000).await {
                    Ok(Some(data)) => match data {
                        #[cfg(feature = "flutter")]
                        Data::UrlLink(url) => {
                            let mut m = HashMap::new();
                            m.insert("name", "on_url_scheme_received");
                            m.insert("url", url.as_str());
                            let event = serde_json::to_string(&m).unwrap_or("".to_owned());
                            match crate::flutter::push_global_event(
                                crate::flutter::APP_TYPE_MAIN,
                                event,
                            ) {
                                None => log::warn!("No main window app found!"),
                                Some(..) => {}
                            }
                        }
                        _ => {
                            log::warn!("An unexpected data was sent to the ipc url server.")
                        }
                    },
                    Err(err) => {
                        log::error!("{}", err);
                    }
                    _ => {}
                }
            }
        }
        Err(err) => {
            log::error!("{}", err);
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn wait_initial_config_sync() {
    if crate::platform::is_root() {
        return;
    }

    // 非服务端进程不应阻塞启动，但仍要保持后台同步/监控存活。
    if !crate::is_server() {
        tokio::spawn(async move {
            sync_and_watch_config_dir(None).await;
        });
        return;
    }

    let (sync_done_tx, mut sync_done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        sync_and_watch_config_dir(Some(sync_done_tx)).await;
    });

    // 服务端进程最多等待 N 秒完成初始的 root->local 同步，以缩短陈旧启动窗口。
    tokio::select! {
        _ = &mut sync_done_rx => {
        }
        _ = tokio::time::sleep(Duration::from_secs(CONFIG_SYNC_INITIAL_WAIT_SECS)) => {
            log::warn!(
                "timed out waiting {}s for initial config sync, continue startup and keep syncing in background",
                CONFIG_SYNC_INITIAL_WAIT_SECS
            );
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn sync_and_watch_config_dir(sync_done_tx: Option<tokio::sync::oneshot::Sender<()>>) {
    let mut cfg0 = (Config::get(), Config2::get());
    let mut synced = false;
    let mut is_root_config_empty = false;
    let mut sync_done_tx = sync_done_tx;
    let tries = if crate::is_server() { 30 } else { 3 };
    log::debug!("#tries of ipc service connection: {}", tries);
    use hbb_common::sleep;
    for i in 1..=tries {
        sleep(i as f32 * CONFIG_SYNC_INTERVAL_SECS).await;
        match crate::ipc::connect_service(1000).await {
            Ok(mut conn) => {
                if !synced {
                    if conn.send(&Data::SyncConfig(None)).await.is_ok() {
                        if let Ok(Some(data)) = conn.next_timeout(1000).await {
                            match data {
                                Data::SyncConfig(Some(configs)) => {
                                    let (config, config2) = *configs;
                                    let _chk = crate::ipc::CheckIfRestart::new();
                                    #[cfg(target_os = "macos")]
                                    let _chk_pk = crate::CheckIfResendPk::new();
                                    if !config.is_empty() {
                                        if cfg0.0 != config {
                                            cfg0.0 = config.clone();
                                            Config::set(config);
                                            log::info!("sync config from root");
                                        }
                                        if cfg0.1 != config2 {
                                            cfg0.1 = config2.clone();
                                            Config2::set(config2);
                                            log::info!("sync config2 from root");
                                        }
                                    } else {
                                        // 仅在 macos 上，因为这个问题只在 macos 上复现过
                                        #[cfg(target_os = "macos")]
                                        {
                                            // root 配置为空，标记以便在监控循环中同步
                                            // 以防止 root 在登录界面生成新配置
                                            is_root_config_empty = true;
                                        }
                                    }
                                    synced = true;
                                    // 初始同步阶段成功完成后通知启动等待者。
                                    if let Some(tx) = sync_done_tx.take() {
                                        let _ = tx.send(());
                                    }
                                }
                                _ => {}
                            };
                        };
                    }
                    if !synced {
                        log::warn!(
                            "initial config sync from root failed, reconnecting to ipc_service"
                        );
                        continue;
                    }
                }

                loop {
                    sleep(CONFIG_SYNC_INTERVAL_SECS).await;
                    let cfg = (Config::get(), Config2::get());
                    let should_sync = cfg != cfg0 || (is_root_config_empty && !cfg.0.is_empty());
                    if should_sync {
                        if is_root_config_empty {
                            log::info!("root config is empty, sync our config to root");
                        } else {
                            log::info!("config updated, sync to root");
                        }
                        match conn.send(&Data::SyncConfig(Some(cfg.clone().into()))).await {
                            Err(e) => {
                                log::error!("sync config to root failed: {}", e);
                                match crate::ipc::connect_service(1000).await {
                                    Ok(mut _conn) => {
                                        conn = _conn;
                                        log::info!("reconnected to ipc_service");
                                    }
                                    _ => {}
                                }
                            }
                            _ => {
                                cfg0 = cfg;
                                conn.next_timeout(1000).await.ok();
                                is_root_config_empty = false;
                            }
                        }
                    }
                }
            }
            Err(_) => {
                log::info!("#{} try: failed to connect to ipc_service", i);
            }
        }
    }
    // 即使初始同步被跳过或失败，也通知启动等待者，以免无谓等待。
    if let Some(tx) = sync_done_tx.take() {
        let _ = tx.send(());
    }
    log::warn!("skipped config sync");
}

#[tokio::main(flavor = "current_thread")]
pub async fn stop_main_window_process() {
    // 这也可能杀掉另一个 --server 进程，
    // 但 --server 通常可以由 --service 自动重启，所以没问题
    if let Ok(mut conn) = crate::ipc::connect(1000, "").await {
        conn.send(&crate::ipc::Data::Close).await.ok();
    }
    #[cfg(windows)]
    {
        // 应对上述失败情况，例如僵尸进程
        if let Err(e) = crate::platform::try_kill_rustdesk_main_window_process() {
            log::error!("kill failed: {}", e);
        }
    }
}
