#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::clipboard::clipboard_listener::{self, ClipboardEvent};
use async_trait::async_trait;
use bytes::Bytes;
#[cfg(not(target_os = "linux"))]
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Host, StreamConfig,
};
use crossbeam_queue::ArrayQueue;
use magnum_opus::{Channels::*, Decoder as AudioDecoder};
#[cfg(not(target_os = "linux"))]
use ringbuf::{ring_buffer::RbBase, Rb};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::c_void,
    net::SocketAddr,
    ops::Deref,
    str::FromStr,
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, RwLock,
    },
};
use uuid::Uuid;

use crate::{
    check_port,
    common::input::{MOUSE_BUTTON_LEFT, MOUSE_BUTTON_RIGHT, MOUSE_TYPE_DOWN, MOUSE_TYPE_UP},
    create_symmetric_key_msg, decode_id_pk, decode_id_pk_dtls, dtls_fingerprint_bound, get_rs_pk,
    is_keyboard_mode_supported,
    kcp_stream::KcpStream,
    secure_tcp, secure_tcp_required,
    ui_interface::{get_builtin_option, resolve_avatar_url, use_texture_render},
    ui_session_interface::{InvokeUiSession, Session},
};
#[cfg(feature = "unix-file-copy-paste")]
use crate::{clipboard::check_clipboard_files, clipboard_file::unix_file_clip};
pub use file_trait::FileManager;
#[cfg(not(feature = "flutter"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::tokio::sync::mpsc::UnboundedSender;
use hbb_common::{
    allow_err,
    anyhow::{anyhow, Context},
    bail,
    config::{
        self, use_ws, Config, LocalConfig, PeerConfig, PeerInfoSerde, Resolution,
        CONNECT_TIMEOUT, READ_TIMEOUT, RELAY_PORT, RENDEZVOUS_PORT, RENDEZVOUS_SERVERS,
    },
    futures::future::{select_ok, BoxFuture, FutureExt},
    get_version_number, log,
    protobuf::{Message as _, MessageField},
    rand,
    rendezvous_proto::*,
    sha2::{Digest, Sha256},
    socket_client::{connect_tcp, connect_tcp_local, ipv4_to_ipv6, new_direct_udp_for_unverified},
    sodiumoxide::{base64, crypto::sign},
    timeout,
    tokio::{
        self,
        net::UdpSocket,
        sync::{
            mpsc::{error::TryRecvError, unbounded_channel, UnboundedReceiver},
            oneshot,
        },
        time::{interval, Duration, Instant},
    },
    webrtc::WebRTCStream,
    websocket::{check_ws, is_ws_endpoint},
    AddrMangle, ResultType, Stream,
};
use base::{
    config::keys,
    fs::JobType,
    message_proto::{option_message::BoolOption, *},
};
pub use helper::*;
use scrap::{
    codec::Decoder,
    record::{Recorder, RecorderContext},
    CodecFormat, ImageFormat, ImageRgb, ImageTexture,
};

#[cfg(not(target_os = "ios"))]
use crate::clipboard::CLIPBOARD_INTERVAL;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::clipboard::{check_clipboard, ClipboardSide};
#[cfg(not(feature = "flutter"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::ui_session_interface::SessionPermissionConfig;

pub use super::lang::*;

#[cfg(not(target_os = "linux"))]
mod audio_playback;
#[cfg(target_os = "windows")]
mod audio_playback_recovery;
#[cfg(all(test, not(target_os = "linux")))]
#[path = "client/tests/audio_state_tests.rs"]
mod audio_state_tests;
pub mod file_trait;
pub mod helper;
pub mod io_loop;
pub mod screenshot;

pub const MILLI1: Duration = Duration::from_millis(1);
pub const SEC30: Duration = Duration::from_secs(30);
// 经验得出的重启重连宽限期。
const RESTART_REMOTE_DEVICE_GRACE: Duration = Duration::from_secs(5 * 60);
pub const VIDEO_QUEUE_SIZE: usize = 120;
const MAX_DECODE_FAIL_COUNTER: usize = 3;

pub const LOGIN_MSG_PASSWORD_EMPTY: &str = "Empty Password";
pub const LOGIN_MSG_PASSWORD_WRONG: &str = "Wrong Password";
pub const LOGIN_MSG_2FA_WRONG: &str = "Wrong 2FA Code";
pub const REQUIRE_2FA: &'static str = "2FA Required";
pub const LOGIN_MSG_NO_PASSWORD_ACCESS: &str = "No Password Access";
pub const LOGIN_MSG_OFFLINE: &str = "Offline";
pub const LOGIN_SCREEN_WAYLAND: &str = "Wayland login screen is not supported";
#[cfg(target_os = "linux")]
pub const SCRAP_UBUNTU_HIGHER_REQUIRED: &str = "ubuntu-21-04-required";
#[cfg(target_os = "linux")]
pub const SCRAP_OTHER_VERSION_OR_X11_REQUIRED: &str =
    "wayland-requires-higher-linux-version";
#[cfg(target_os = "linux")]
pub const SCRAP_XDP_PORTAL_UNAVAILABLE: &str =
    "xdp-portal-unavailable";
pub const SCRAP_X11_REQUIRED: &str = "x11 expected";
pub const SCRAP_X11_REF_URL: &str = "https://rustdesk.com/docs/en/manual/linux/#x11-required";

#[cfg(not(target_os = "linux"))]
pub const AUDIO_BUFFER_MS: usize = 3000;

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) struct ClientClipboardContext;

#[cfg(not(feature = "flutter"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) struct ClientClipboardContext {
    pub cfg: SessionPermissionConfig,
    pub tx: UnboundedSender<Data>,
    #[cfg(feature = "unix-file-copy-paste")]
    pub is_file_supported: bool,
}

/// 远程桌面的客户端。
pub struct Client;

#[cfg(not(target_os = "ios"))]
struct ClipboardState {
    #[cfg(feature = "flutter")]
    is_text_required: bool,
    #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
    is_file_required: bool,
    running: bool,
}

#[cfg(not(target_os = "linux"))]
lazy_static::lazy_static! {
    static ref AUDIO_HOST: Host = cpal::default_host();
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
lazy_static::lazy_static! {
    static ref ENIGO: Arc<Mutex<enigo::Enigo>> = Arc::new(Mutex::new(enigo::Enigo::new()));
}

#[cfg(not(target_os = "ios"))]
lazy_static::lazy_static! {
    static ref CLIPBOARD_STATE: Arc<Mutex<ClipboardState>> = Arc::new(Mutex::new(ClipboardState::new()));
}

const PUBLIC_SERVER: &str = "public";

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_key_state(key: enigo::Key) -> bool {
    use enigo::KeyboardControllable;
    #[cfg(target_os = "macos")]
    if key == enigo::Key::NumLock {
        return true;
    }
    ENIGO.lock().unwrap().get_key_state(key)
}

/// 在 drop 时关闭未被接管的 WebRTC offerer 的 pc。没有应答时它会一直停留在 ICE `New` 状态，
/// 导致它的状态处理函数永远不会被触发、无法把自己从 `SESSIONS` 中移除；这覆盖了
/// 那些会泄漏它的提前返回和被取消的竞态。被接管时 `into_inner` 会解除武装。
struct OffererGuard(Option<WebRTCStream>);

impl OffererGuard {
    fn new(stream: WebRTCStream) -> Self {
        Self(Some(stream))
    }

    fn stream(&self) -> Option<&WebRTCStream> {
        self.0.as_ref()
    }

    fn into_inner(mut self) -> Option<WebRTCStream> {
        self.0.take()
    }
}

impl Drop for OffererGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.0.take() {
            stream.close_detached();
        }
    }
}

/// 让 WebRTC 与其他传输方式竞速，优先选择 P2P：`select_ok` 总是会选中
/// 中继，而中继的 TCP 连接比 ICE + DTLS + SCTP 快了一个数量级。`is_p2p` 的结果
/// 直接获胜；而中继的结果——无论来自哪一侧，因为 `webrtc_fut` 是一次完整的打洞尝试，
/// 它自身也可能以中继告终——会被暂存 `window_ms` 时间，给另一侧一次机会。
///
/// `others` 必须非空（`select_ok` 要求如此）。
async fn race_transports_prefer_webrtc<'a, T: 'a>(
    webrtc_fut: BoxFuture<'a, ResultType<T>>,
    others: Vec<BoxFuture<'a, ResultType<T>>>,
    window_ms: u64,
    is_p2p: impl Fn(&T) -> bool,
) -> ResultType<T> {
    let mut webrtc_fut = Some(webrtc_fut);
    let mut others_fut = Some(select_ok(others));
    let mut held: Option<T> = None;
    let mut webrtc_err: Option<hbb_common::anyhow::Error> = None;
    let mut others_err: Option<hbb_common::anyhow::Error> = None;
    let window = tokio::time::sleep(Duration::from_millis(window_ms));
    tokio::pin!(window);
    let mut window_started = false;
    loop {
        tokio::select! {
            res = async {
                match webrtc_fut.as_mut() {
                    Some(fut) => fut.await,
                    None => std::future::pending().await,
                }
            }, if webrtc_fut.is_some() => {
                webrtc_fut = None;
                match res {
                    // 直连正是这场竞速要保护的结果：直接提交它，
                    // 而被暂存的中继连接就直接丢弃。
                    Ok(conn) if is_p2p(&conn) || others_fut.is_none() => return Ok(conn),
                    // `webrtc_fut` 是一次完整的打洞尝试，而不只是 WebRTC 的连接，所以它
                    // 自身也可能以中继收场。立刻提交它就会抢先于
                    // 另一分支上仍在进行中的直连打洞——这恰恰是本函数
                    // 要防止的倒挂——所以按对待其他中继的同样条件把它暂存。
                    Ok(conn) => {
                        if held.is_none() {
                            held = Some(conn);
                            window.as_mut().reset(
                                Instant::now() + Duration::from_millis(window_ms),
                            );
                            window_started = true;
                        }
                    }
                    Err(e) => {
                        // 只有在不再有任何直连仍在竞速时才提交被暂存的中继；否则
                        // 继续保留它，让胜出的那个（或等待窗口结束）来决定。
                        if others_fut.is_none() {
                            if let Some(conn) = held.take() {
                                return Ok(conn);
                            }
                        }
                        match others_err.take() {
                            // 像 `select_ok` 那样只报告一个错误：UI 会对整段文本做匹配和翻译
                            // （例如离线自动重试），而两个分支通常
                            // 本来就会带上同样的 rendezvous 拒绝信息。
                            Some(oe) => {
                                log::warn!("fallback leg also failed: {}", oe);
                                return Err(e);
                            }
                            None if others_fut.is_none() => bail!("WebRTC failed: {}", e),
                            None => webrtc_err = Some(e),
                        }
                    }
                }
            }
            res = async {
                match others_fut.as_mut() {
                    Some(fut) => fut.await,
                    None => std::future::pending().await,
                }
            }, if others_fut.is_some() => {
                others_fut = None;
                match res {
                    Ok((conn, unfinished)) => {
                        if is_p2p(&conn) {
                            return Ok(conn);
                        }
                        // 中继结果：只有在不可能再有直连到达时才立刻提交。如果
                        // 还有直连尝试正在进行（无论是这里还是 `unfinished` 里的），就把它暂存，
                        // 并继续在偏好窗口内竞速，而不是把它们丢弃。
                        if webrtc_fut.is_none() && unfinished.is_empty() {
                            return Ok(conn);
                        }
                        if held.is_none() {
                            held = Some(conn);
                            window.as_mut().reset(
                                Instant::now() + Duration::from_millis(window_ms),
                            );
                            window_started = true;
                        }
                        if !unfinished.is_empty() {
                            others_fut = Some(select_ok(unfinished));
                        }
                    }
                    Err(e) => match webrtc_err.take() {
                        // 已经没有其他可能胜出的了，但被暂存的中继依然是一个有效结果——
                        // 在判定连接失败之前先采用它。
                        Some(we) => match held.take() {
                            Some(conn) => return Ok(conn),
                            None => {
                                log::warn!("WebRTC leg also failed: {}", we);
                                return Err(e);
                            }
                        },
                        None if webrtc_fut.is_none() => match held.take() {
                            Some(conn) => return Ok(conn),
                            None => return Err(e),
                        },
                        None => others_err = Some(e),
                    },
                }
            }
            _ = &mut window, if window_started => {
                if let Some(conn) = held.take() {
                    return Ok(conn);
                }
                window_started = false;
            }
        }
    }
}

// 对端自行决定发送多少 ICE 候选，而承载它们的 rendezvous 通道
// 无需事先打洞就能访问，因此这些站点否则就会让别人决定这台机器
// 往自己的日志文件里写多少。每个站点每分钟一行，携带被抑制的计数。
use hbb_common::log_throttle::LogThrottle;
const ICE_LOG_INTERVAL: Duration = Duration::from_secs(60);
static REJECTED_ICE_LOG: LogThrottle = LogThrottle::new(ICE_LOG_INTERVAL);
static UDP_UAT_ERR_LOG: LogThrottle = LogThrottle::new(ICE_LOG_INTERVAL);
static UNEXPECTED_ICE_LOG: LogThrottle = LogThrottle::new(ICE_LOG_INTERVAL);
static PENDING_ICE_FULL_LOG: LogThrottle = LogThrottle::new(ICE_LOG_INTERVAL);

fn request_allows_tcp_punch(webrtc_sdp_offer: &str) -> bool {
    // WebRTC 的 trickle ICE 会保留 rendezvous 套接字作为信令桥。只有请求
    // 不带 offer 时才可以关闭该套接字，并复用它的本地地址做 TCP 打洞。
    webrtc_sdp_offer.is_empty()
}

/// TCP 打洞和别的直连传输一样是用户可选项，但它同时也是兜底手段：
/// 如果把所有直连传输都关掉，就没有任何东西可以用来打洞了，所以它
/// 无论如何都会运行。只有那些开关能决定这一点——某个已启用但未能
/// 真正建立起来的传输方式（没有公网 v6 地址、offerer 设置出错）不影响这里，因为
/// 中继回退已经覆盖了最终没有可用直连传输的情况。
fn tcp_punch_allowed() -> bool {
    crate::get_tcp_punch_enabled()
        || !(crate::get_udp_punch_enabled()
            || crate::get_ipv6_punch_enabled()
            || crate::get_webrtc_enabled())
}

impl Client {
    const CLIENT_CLIPBOARD_NAME: &'static str = "client-clipboard";

    /// 发起一个新连接。
    pub async fn start(
        peer: &str,
        key: &str,
        token: &str,
        conn_type: ConnType,
        interface: impl Interface,
    ) -> ResultType<(
        (
            Stream,
            bool,
            Option<Vec<u8>>,
            Option<KcpStream>,
            &'static str,
        ),
        (i32, String),
    )> {
        debug_assert!(peer == interface.get_id());
        interface.update_direct(None);
        interface.update_received(false);
        match Self::_start(peer, key, token, conn_type, interface.clone()).await {
            Err(err) => {
                let err_str = err.to_string();
                if err_str.starts_with("Failed") {
                    bail!(err_str + ": Please try later");
                } else {
                    return Err(err);
                }
            }
            Ok(x) => {
                // 只在 connect() 函数里把 x.2 设为 true，表示 direct_failures 需要更新；其他地方都应设为 false。
                if x.2 {
                    let direct_failures = interface.get_lch().read().unwrap().direct_failures;
                    let direct = x.0 .1;
                    if !interface.is_force_relay() && (direct_failures == 0) != direct {
                        let n = if direct { 0 } else { 1 };
                        log::info!("direct_failures updated to {}", n);
                        interface.get_lch().write().unwrap().set_direct_failure(n);
                    }
                }
                Ok((x.0, x.1))
            }
        }
    }

    /// 发起一个新连接。
    async fn _start(
        peer: &str,
        key: &str,
        token: &str,
        conn_type: ConnType,
        interface: impl Interface,
    ) -> ResultType<(
        (
            Stream,
            bool,
            Option<Vec<u8>>,
            Option<KcpStream>,
            &'static str,
        ),
        (i32, String),
        bool,
    )> {
        if config::is_incoming_only() && !is_switch_sides_back(conn_type, &interface).await {
            bail!("Incoming only mode");
        }
        // 待办：记住每个对端的端口，这样重试会更容易
        if hbb_common::is_ip_str(peer) {
            return Ok((
                (
                    connect_tcp_local(check_port(peer, RELAY_PORT + 1), None, CONNECT_TIMEOUT)
                        .await?,
                    true,
                    None,
                    None,
                    "TCP",
                ),
                (0, "".to_owned()),
                false,
            ));
        }
        // 允许连接到 {domain}:{port}
        if hbb_common::is_domain_port_str(peer) {
            return Ok((
                (
                    connect_tcp_local(peer, None, CONNECT_TIMEOUT).await?,
                    true,
                    None,
                    None,
                    "TCP",
                ),
                (0, "".to_owned()),
                false,
            ));
        }

        let other_server = interface.get_lch().read().unwrap().other_server.clone();
        let (peer, other_server, key, token) = if let Some((a, b, c)) = other_server.as_ref() {
            (a.as_ref(), b.as_ref(), c.as_ref(), "")
        } else {
            (peer, "", key, token)
        };
        let (rendezvous_server, servers, contained) = if other_server.is_empty() {
            crate::get_rendezvous_server(1_000).await
        } else {
            if other_server == PUBLIC_SERVER {
                (
                    check_port(RENDEZVOUS_SERVERS[0], RENDEZVOUS_PORT),
                    RENDEZVOUS_SERVERS[1..]
                        .iter()
                        .map(|x| x.to_string())
                        .collect(),
                    true,
                )
            } else {
                (check_port(other_server, RENDEZVOUS_PORT), Vec::new(), true)
            }
        };

        // 与下面的 v6 套接字使用同样的中继门控：在任何强制中继下，v6 打洞都无法
        // 使用，所以在每次这类连接上探测 v6 可达性都是白费力气。
        if crate::get_ipv6_punch_enabled() && !interface.is_force_relay() {
            crate::test_ipv6().await;
        }

        let (stop_udp_tx, stop_udp_rx) = oneshot::channel::<()>();
        let udp =
        // 不需要考虑多台 rendezvous 服务器的情况，因为实际上已经不再使用了。
        // UDP NAT 测试结果的共享状态
        if crate::get_udp_punch_enabled() && !interface.is_force_relay() {
            if let Ok((socket, addr)) = new_direct_udp_for_unverified(&rendezvous_server).await {
                let udp_port = Arc::new(Mutex::new(0));
                let up_cloned = udp_port.clone();
                let socket_cloned = socket.clone();
                let func = async move {
                    allow_err!(test_udp_uat(socket_cloned, addr, up_cloned, stop_udp_rx).await);
                };
                tokio::spawn(func);
                (Some(socket), Some(udp_port))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        // 在强制中继下不允许直连 IPv6，所以不要绑定 v6 套接字；
        // 被控端在中继时同样会跳过 v6。
        let ipv6 = if crate::get_ipv6_punch_enabled() && !interface.is_force_relay() {
            crate::get_ipv6_socket().await
        } else {
            None
        };
        // WebRTC 使用自己的 ICE 套接字，不依赖旧版的 UDP 打洞套接字。
        // 当这个请求携带 offer 时，`_start_inner` 保留其 rendezvous 套接字仅仅
        // 用于 trickle 信令；任何 TCP 打洞尝试都由另一个不带 offer 的请求负责。
        let webrtc_offerer = if Self::should_create_webrtc_offerer(&interface) {
            // ICE 策略跟随“按策略中继”，而不是 force_relay：在 WebSocket 下
            // 后者会针对传统路径被设置，但 ICE 会自己开套接字，仍然可能
            // 走直连——这是 ws 部署唯一拥有的 P2P 通路。
            match WebRTCStream::new("", interface.is_policy_relay(), CONNECT_TIMEOUT).await {
                Ok(stream) => Some(stream),
                Err(err) => {
                    log::warn!("webrtc offerer setup failed: {}", err);
                    None
                }
            }
        } else {
            None
        };
        let has_webrtc_offerer = webrtc_offerer.is_some();
        let fut = Self::_start_inner(
            peer.to_owned(),
            key.to_owned(),
            token.to_owned(),
            conn_type,
            interface.clone(),
            udp.clone(),
            Some(stop_udp_tx),
            ipv6,
            webrtc_offerer,
            rendezvous_server.clone(),
            servers.clone(),
            contained,
        );
        // 这个回退请求存在的唯一目的就是承载 TCP 打洞，所以一旦 TCP
        // 打洞被关闭，它就毫无意义——它会带着无从尝试的内容走到 `connect()`，白白再开一条
        // 中继。
        if interface.is_force_relay()
            || (udp.0.is_none() && !has_webrtc_offerer)
            || !tcp_punch_allowed()
        {
            return fut.await;
        }
        let preferred_fut = fut.boxed();
        // 这刻意是一个纯 TCP 打洞请求：它的 WebRTC 参数必须保持 `None`。
        // TCP 打洞会先关闭 rendezvous 套接字，再把新连接绑定到同一个
        // 本地地址上；WebRTC 的 ICE 桥会保留该套接字，从而破坏端口复用。
        // 优先的那个请求则保留自己的套接字用于 WebRTC 信令。
        let fallback_fut = Self::_start_inner(
            peer.to_owned(),
            key.to_owned(),
            token.to_owned(),
            conn_type,
            interface,
            (None, None),
            None,
            None,
            None,
            rendezvous_server,
            servers,
            contained,
        )
        .boxed();
        if has_webrtc_offerer {
            return race_transports_prefer_webrtc(
                preferred_fut,
                vec![fallback_fut],
                Self::relay_fallback_delay_ms(),
                |result| result.0 .1,
            )
            .await;
        }
        let connect_futures = vec![preferred_fut, fallback_fut];
        match select_ok(connect_futures).await {
            Ok(conn) => Ok((conn.0 .0, conn.0 .1, conn.0 .2)),
            Err(e) => Err(e),
        }
    }

    fn is_expected_webrtc_ice_candidate(ice: &IceCandidate, session_key: &str) -> bool {
        !session_key.is_empty() && ice.session_key == session_key && !ice.candidate.is_empty()
    }

    /// 是否为这个连接构建 WebRTC offerer。
    ///
    /// SOCKS 代理会排除这种可能：ICE 自己绑定 UDP 套接字并直接说 STUN，绕过
    /// 代理并使用真实 IP。按策略中继但没有 TURN 也会排除它，因为
    /// 此时 Relay-only ICE 采集不到任何候选。WebSocket 不会：它只隧道信令
    /// 和中继这两条通路，所以 offer 保留完整的 ICE，而直连正是它存在的意义。
    fn should_create_webrtc_offerer(interface: &impl Interface) -> bool {
        if !crate::get_webrtc_enabled() {
            return false;
        }
        if Config::is_proxy() {
            return false;
        }
        if interface.is_policy_relay() && !WebRTCStream::has_turn_server() {
            return false;
        }
        true
    }

    /// 在打洞窗口内、应用应答之前缓冲的 ICE 候选上限。
    /// 当行为异常的 rendezvous 大量灌入候选时用来限制内存。溢出时淘汰最旧的，
    /// 因为收集顺序是 host、然后 srflx、再到 relay，所以最新到达的才是
    /// 能够穿越 NAT 的那些。
    const MAX_PENDING_WEBRTC_ICE: usize = 64;

    /// 默认的中继回退延迟：一个已经建立成功的中继结果会被暂存多久，
    /// 同时 WebRTC 尝试仍在进行中；它也是打洞通路上的 WebRTC 尝试
    /// （其竞速超时是按裸 TCP SYN 调校的）的下限。要足够长以容纳候选 trickle + ICE
    /// 检查 + DTLS 在高延迟链路上所需的时间；又要足够短，使被 UDP 阻断的网络能
    /// 不至于明显等待就落到中继上。RFC 8305 把同样的角色称为连接尝试延迟。
    const RELAY_FALLBACK_DELAY_MS: u64 = 2500;

    /// 按用户配置取该延迟，缺省回退到 `RELAY_FALLBACK_DELAY_MS`。
    /// 设置项里存的是秒，这是用户习惯思考的单位；而这里的一切都是
    /// 毫秒。无法解析、为零或为负都表示“未设置”，所以清空该字段
    /// 会恢复默认值，而不是把延迟压到零、把每场竞速都交给
    /// 中继。
    fn relay_fallback_delay_ms() -> u64 {
        match LocalConfig::get_option(keys::OPTION_RELAY_FALLBACK_DELAY)
            .trim()
            .parse::<f64>()
        {
            Ok(secs) if secs.is_finite() && secs > 0.0 => (secs * 1000.0).round() as u64,
            _ => Self::RELAY_FALLBACK_DELAY_MS,
        }
    }

    /// 当 TCP 时钟不合理时的 UDP NAT 测试等待时间（参见 TCP_RTT_PLAUSIBLE_MIN）。
    /// 正常情况下上限是 `rtt / 2`：这个测试从 TCP 连接之前就已经在跑了，所以
    /// 在 TCP RTT ≈ UDP RTT 的网络上，它的响应早已到达。透明 TCP
    /// 代理——主机上的 TUN 模式 VPN，或局域网网关上的重定向模式代理（软
    /// 路由）——会替身后的每台设备伪造握手——这破坏了上述假设，因为它
    /// 在约 3ms 内就应答，而真实的 UDP 往返是几百毫秒：窗口被压缩到约 1.5ms，
    /// udp_port 一直是 0，UDP 打洞在这类网络上从未运行过。不过一旦端口到达，等待仍会立即退出，
    /// 所以真正在附近的服务器（局域网内的 hbbs）完全不付出代价；只有 UDP 完全不通的网络
    /// 才需要等满整个宽限期，而且只在这一轮——纯 TCP 的回退轮次从不
    /// 等待。它按真实场景中 rendezvous RTT 的上限再加一次 20ms 重传来设定
    /// （洲际约 300ms）；比这更慢的路径在代理下会失去 UDP 打洞，而这
    /// 就是当前的行为，并非回归。
    const UDP_NAT_TEST_GRACE: Duration = Duration::from_millis(400);

    /// 低于这个值时，测得的 TCP 连接时间就不是可信的 WAN 往返——它
    /// 是在局域网内就被应答的（主机 TUN 代理、网关透明代理，或真正在本地的
    /// 服务器）——不能拿它来推算 UDP 等待窗口。
    /// 这里刻意放宽：触发过头没有任何代价（端口一到等待就退出，而
    /// UDP 不通的轮次反正也会输给竞速中的回退方案），而上限过紧会让
    /// 繁忙代理偶尔出现的约 80ms 握手蒙混过关，导致 UDP 打洞被悄悄丢掉。
    const TCP_RTT_PLAUSIBLE_MIN: Duration = Duration::from_millis(100);

    /// 通过 rendezvous 通道重发一次 ICE 候选前的延迟。到对端的
    /// 一跳可能是 UDP（被控端的中介通道），所以候选可能在传输中丢失；
    /// 远端 ICE agent 会去重重复项，所以第二份拷贝是免费的。
    const WEBRTC_ICE_RESEND_DELAY: Duration = Duration::from_millis(400);

    /// 通过打洞套接字把本地 ICE 候选桥接给对端，并把对端的候选回灌
    /// 到 pc 中。
    ///
    /// 绝不重连这个套接字：它的地址*就是*回程路径（服务器会把它变形写入
    /// `PunchHole.socket_addr`，并通过 `tcp_punch` 解析回显），所以一个新地址是
    /// 没有任何东西指向的。它一旦失效，双向都断了——放弃 WebRTC，不要重试。
    fn spawn_webrtc_ice_bridge(
        mut socket: Stream,
        mut local_ice_rx: Option<UnboundedReceiver<String>>,
        webrtc: WebRTCStream,
        peer: String,
        session_key: String,
    ) -> oneshot::Sender<()> {
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut pending_resend: Vec<(Instant, RendezvousMessage)> = Vec::new();
            loop {
                match stop_rx.try_recv() {
                    Ok(_) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => break,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                }

                if let Some(rx) = local_ice_rx.as_mut() {
                    loop {
                        match rx.try_recv() {
                            Ok(candidate) => {
                                let mut msg = RendezvousMessage::new();
                                msg.set_ice_candidate(IceCandidate {
                                    id: peer.clone(),
                                    session_key: session_key.clone(),
                                    candidate,
                                    ..Default::default()
                                });
                                // 给发送设上限，避免卡住的 rendezvous 套接字把
                                // 桥接逻辑阻塞到超过它的停止信号。
                                match timeout(3000, socket.send(&msg)).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(err)) => {
                                        log::warn!("failed to send WebRTC ICE candidate: {}", err);
                                        return;
                                    }
                                    Err(_) => {
                                        log::warn!("WebRTC ICE candidate send timed out");
                                        return;
                                    }
                                }
                                pending_resend.push((Instant::now(), msg));
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                local_ice_rx = None;
                                break;
                            }
                        }
                    }
                }

                // 在短暂延迟后把每个候选重发一次：到对端的 rendezvous 一跳
                // 可能是 UDP，所以第一份拷贝可能丢失；远端 ICE agent 会去重重复项。
                let mut i = 0;
                while i < pending_resend.len() {
                    if pending_resend[i].0.elapsed() >= Self::WEBRTC_ICE_RESEND_DELAY {
                        let (_, msg) = pending_resend.swap_remove(i);
                        match timeout(3000, socket.send(&msg)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                log::warn!("failed to re-send WebRTC ICE candidate: {}", err);
                                return;
                            }
                            Err(_) => {
                                log::warn!("WebRTC ICE candidate re-send timed out");
                                return;
                            }
                        }
                    } else {
                        i += 1;
                    }
                }

                // 读取一条传入消息，最多阻塞到轮询窗口结束。要区分真正的
                // 超时（继续循环以排空待发候选）和流 EOF/错误：
                // rendezvous 服务器在应答之后会关闭打洞连接，而
                // 不认识的消息的服务器会在我们发出第一个候选时就关闭它。如果没有这一处理，
                // EOF 返回 None 的写法会让循环空转到占满一个核心，直到 stop。
                match timeout(100, socket.next()).await {
                    Err(_) => {}
                    Ok(None) => break,
                    Ok(Some(Ok(bytes))) => {
                        if let Ok(msg_in) = RendezvousMessage::parse_from_bytes(&bytes) {
                            if let Some(rendezvous_message::Union::IceCandidate(ice)) = msg_in.union
                            {
                                if Self::is_expected_webrtc_ice_candidate(&ice, &session_key) {
                                    if let Err(err) =
                                        webrtc.add_remote_ice_candidate(&ice.candidate).await
                                    {
                                        if let Some(n) = REJECTED_ICE_LOG.due() {
                                            log::warn!(
                                                "failed to add {} WebRTC ICE candidate(s), last: {}",
                                                n,
                                                err
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Some(Err(err))) => {
                        log::debug!("WebRTC ICE bridge socket read ended: {}", err);
                        break;
                    }
                }
            }
        });
        stop_tx
    }

    async fn _start_inner(
        peer: String,
        key: String,
        token: String,
        conn_type: ConnType,
        interface: impl Interface,
        mut udp: (Option<Arc<UdpSocket>>, Option<Arc<Mutex<u16>>>),
        stop_udp_tx: Option<oneshot::Sender<()>>,
        mut ipv6: Option<(Arc<UdpSocket>, bytes::Bytes)>,
        webrtc_offerer: Option<WebRTCStream>,
        mut rendezvous_server: String,
        servers: Vec<String>,
        contained: bool,
    ) -> ResultType<(
        (
            Stream,
            bool,
            Option<Vec<u8>>,
            Option<KcpStream>,
            &'static str,
        ),
        (i32, String),
        bool,
    )> {
        // 把 offerer 包一层，使得下面任何提前返回（?/bail）或外层
        // select_ok 取消这个 future 时，会关闭它的 pc，而不是把它泄漏在 SESSIONS 里。通过
        // into_inner() 在流被接管进一次连接尝试后解除武装。
        let mut webrtc_offerer = webrtc_offerer.map(OffererGuard::new);
        let mut start = Instant::now();
        let mut socket = connect_tcp(&*rendezvous_server, CONNECT_TIMEOUT).await;
        debug_assert!(!servers.contains(&rendezvous_server));
        let rtt = start.elapsed();
        log::debug!("TCP connection establishment time used: {:?}", rtt);
        if socket.is_err() && !servers.is_empty() {
            log::info!("try the other servers: {:?}", servers);
            for server in servers {
                let server = check_port(server, RENDEZVOUS_PORT);
                socket = connect_tcp(&*server, CONNECT_TIMEOUT).await;
                if socket.is_ok() {
                    rendezvous_server = server;
                    break;
                }
            }
            crate::refresh_rendezvous_server();
        } else if !contained {
            crate::refresh_rendezvous_server();
        }
        log::info!("rendezvous server: {}", rendezvous_server);
        let mut socket = socket?;
        let mut my_addr = socket.local_addr();
        let mut signed_id_pk = Vec::new();
        let mut relay_server = "".to_owned();
        let mut peer_addr = Config::get_any_listen_addr(true);
        let mut peer_nat_type = NatType::UNKNOWN_NAT;
        let my_nat_type = crate::get_nat_type(100).await;
        let mut is_local = false;
        let mut feedback = 0;
        use hbb_common::protobuf::Enum;
        let nat_type = if interface.is_force_relay() {
            NatType::SYMMETRIC
        } else {
            NatType::from_i32(my_nat_type).unwrap_or(NatType::UNKNOWN_NAT)
        };

        let switch_code = interface.get_switch_code();
        let legacy_secure = !key.is_empty() && (!token.is_empty() || !switch_code.is_empty());
        let carries_offer = webrtc_offerer.as_ref().and_then(|g| g.stream()).is_some();
        // 从密钥交换之前开始计时，这样密钥交换会消耗掉 UDP NAT 测试自己的
        // 等待时间，而不是取代它：这个测试与两者并行运行。
        let udp_nat_wait_from = Instant::now();
        let mut exchanged = false;
        if carries_offer {
            // offer 会把双方的 ICE 候选、两台机器的每一个网络接口地址
            // 都放到这个套接字上，所以只有在服务器的密钥交换
            // 加密之后才能发出。当服务器没有完成密钥交换（比如来自交换之前的
            // hbbs），offer 会被丢弃，这次就变成不含 WebRTC 的打洞，并使用一个全新的
            // 套接字，因为失败的交换可能已经在这个套接字上消耗掉一条消息。只能
            // 降级为不使用 WebRTC，绝不能降级为明文的 WebRTC 信令。
            match secure_tcp_required(&mut socket, &key).await {
                Ok(()) => exchanged = true,
                Err(err) => {
                    log::warn!(
                        "WebRTC signalling to {} cannot be encrypted, punching without WebRTC: {}",
                        rendezvous_server,
                        err
                    );
                    webrtc_offerer = None;
                    socket = connect_tcp(&*rendezvous_server, CONNECT_TIMEOUT).await?;
                    my_addr = socket.local_addr();
                }
            }
        }
        if !exchanged && legacy_secure {
            secure_tcp(&mut socket, &key)
                .await
                .map_err(|e| anyhow!("Failed to secure tcp: {}", e))?;
        }
        // 令牌或切换码历来都是直接用这个套接字进入打洞，而不
        // 等待 UDP NAT 测试。WebRTC 交换并不取代那段等待，它只是
        // 花掉了同一份预算的一部分，所以剩下的部分在这里等完，而已经
        // 到达的结果会被立刻采用。
        if let Some(udp) = udp.1.as_ref().filter(|_| !legacy_secure) {
            let tm = udp_nat_wait_from;
            // rtt 是 TCP 连接耗时。当它短到不可能是真实的 WAN 往返时，它
            // 对 UDP 通路毫无说明力（应答握手的是 TUN VPN 或局域网网关，
            // 而不是服务器），所以回退到固定的宽限时间；否则就信任它。
            let udp_nat_wait = if rtt < Self::TCP_RTT_PLAUSIBLE_MIN {
                Self::UDP_NAT_TEST_GRACE
            } else {
                rtt / 2
            };
            loop {
                let port = *udp.lock().unwrap();
                if port > 0 {
                    break;
                }
                if tm.elapsed() > udp_nat_wait {
                    break;
                }
                hbb_common::sleep(0.001).await;
            }
        }
        // 如果 UDP NAT 测试任务还在运行就停止它
        stop_udp_tx.map(|tx| tx.send(()));
        let mut msg_out = RendezvousMessage::new();
        let mut ipv6 = ipv6
            .take()
            .map(|(socket, addr)| (Some(socket), Some(addr)))
            .unwrap_or((None, None));
        let udp_nat_port = udp.1.map(|x| *x.lock().unwrap()).unwrap_or(0);
        let webrtc_sdp_offer = webrtc_offerer
            .as_ref()
            .and_then(|g| g.stream())
            .map(|stream| stream.local_endpoint().to_owned())
            .unwrap_or_default();
        let allow_tcp_punch = tcp_punch_allowed() && request_allows_tcp_punch(&webrtc_sdp_offer);
        // 这一轮携带的每一种直连传输，而不是其中某一种：一轮可以同时携带多种
        // （一个 NAT 端口、一个 offer 和一个 v6 地址），而自从 TCP 打洞变成
        // 开关之后它也可能一种都不带——之前用单个名字不得不同时误报两种情况。`relay` 不是打洞，
        // 它是一轮在没有任何可打洞目标时仍然可能得到的结局。
        let mut transports = Vec::new();
        if udp_nat_port > 0 {
            transports.push("UDP");
        }
        if allow_tcp_punch {
            transports.push("TCP");
        }
        if ipv6.1.is_some() {
            transports.push("IPv6");
        }
        if !webrtc_sdp_offer.is_empty() {
            transports.push("WebRTC");
        }
        let punch_type = if transports.is_empty() {
            "Relay".to_owned()
        } else {
            transports.join("+")
        };
        msg_out.set_punch_hole_request(PunchHoleRequest {
            id: peer.to_owned(),
            token: token.to_owned(),
            nat_type: nat_type.into(),
            licence_key: key.to_owned(),
            conn_type: conn_type.into(),
            version: crate::VERSION.to_owned(),
            udp_port: udp_nat_port as _,
            force_relay: interface.is_force_relay(),
            socket_addr_v6: ipv6.1.unwrap_or_default(),
            switch_code,
            // offer 的信封本身就声明了它的 ICE 策略（纯 ws 下为 `ice_policy: "all"`，
            // 也就是告诉被控端，它的应答可以收集所有类型的候选，
            // 即便存在 force_relay 也不必依赖 TURN。
            webrtc_sdp_offer,
            ..Default::default()
        });
        let webrtc_session_key = webrtc_offerer
            .as_ref()
            .and_then(|guard| guard.stream())
            .map(|stream| stream.session_key().to_owned())
            .unwrap_or_default();
        let mut webrtc_sdp_answer = String::new();
        let mut pending_webrtc_ice = Vec::<String>::new();
        'punch_attempts: for i in 1..=3 {
            log::info!(
                "#{} {} punch attempt with {}, id: {}",
                i,
                punch_type,
                my_addr,
                peer
            );
            socket.send(&msg_out).await?;
            // 下面的超时不应大于 hbbs 的连接超时。
            let attempt_deadline = Instant::now() + Duration::from_millis((i * 3000) as u64);
            loop {
                let remaining = attempt_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let timeout_ms = remaining
                    .as_millis()
                    .clamp(1, u64::MAX as u128) as u64;
                let Some(msg_in) =
                    crate::get_next_nonkeyexchange_msg(&mut socket, Some(timeout_ms)).await
                else {
                    break;
                };
                match msg_in.union {
                    Some(rendezvous_message::Union::PunchHoleResponse(ph)) => {
                        if ph.socket_addr.is_empty() {
                            if !ph.other_failure.is_empty() {
                                bail!(ph.other_failure);
                            }
                            match ph.failure.enum_value() {
                                Ok(punch_hole_response::Failure::ID_NOT_EXIST) => {
                                    bail!("ID does not exist");
                                }
                                Ok(punch_hole_response::Failure::OFFLINE) => {
                                    bail!("Remote desktop is offline");
                                }
                                Ok(punch_hole_response::Failure::LICENSE_MISMATCH) => {
                                    bail!("Key mismatch");
                                }
                                Ok(punch_hole_response::Failure::LICENSE_OVERUSE) => {
                                    bail!("Key overuse");
                                }
                                _ => bail!("other punch hole failure"),
                            }
                        } else {
                            peer_nat_type = ph.nat_type();
                            is_local = ph.is_local();
                            signed_id_pk = ph.pk.into();
                            relay_server = ph.relay_server;
                            peer_addr = AddrMangle::decode(&ph.socket_addr);
                            feedback = ph.feedback;
                            webrtc_sdp_answer = ph.webrtc_sdp_answer;
                            let s = udp.0.take();
                            if udp_nat_port > 0 && ph.is_udp && s.is_some() {
                                if let Some(s) = s {
                                    allow_err!(s.connect(peer_addr).await);
                                    udp.0 = Some(s);
                                }
                            }
                            let s = ipv6.0.take();
                            if !ph.socket_addr_v6.is_empty() && s.is_some() {
                                let addr = AddrMangle::decode(&ph.socket_addr_v6);
                                if addr.port() > 0 {
                                    if let Some(s) = s {
                                        allow_err!(s.connect(addr).await);
                                        ipv6.0 = Some(s);
                                    }
                                }
                            }
                            log::info!("{} Hole Punched {} = {}", punch_type, peer, peer_addr);
                            break 'punch_attempts;
                        }
                    }
                    Some(rendezvous_message::Union::RelayResponse(rr)) => {
                        log::info!(
                            "relay requested from peer, time used: {:?}, relay_server: {}",
                            start.elapsed(),
                            rr.relay_server
                        );
                        start = Instant::now();
                        let mut connect_futures = Vec::new();
                        if let Some(s) = ipv6.0 {
                            let addr = AddrMangle::decode(&rr.socket_addr_v6);
                            if addr.port() > 0 {
                                if s.connect(addr).await.is_ok() {
                                    connect_futures.push(
                                        async move {
                                            let (conn, kcp, typ) =
                                                udp_nat_connect(s, "IPv6", CONNECT_TIMEOUT).await?;
                                            Ok((conn, kcp, typ, true))
                                        }
                                        .boxed(),
                                    );
                                }
                            }
                        }
                        signed_id_pk = rr.pk().into();
                        let mut webrtc_bridge_stop = None;
                        let mut webrtc_for_connect = None;
                        if !rr.webrtc_sdp_answer.is_empty() {
                            if let Some(guard) = webrtc_offerer.take() {
                                // 在 guard 借用的流上执行这些 await，这样在
                                // 这些 await 期间发生取消时，仍能通过
                                // guard 的 drop 关闭 pc；只在同步交接时才取得所有权。
                                let setup_ok = if let Some(webrtc) = guard.stream() {
                                    if let Err(err) =
                                        webrtc.set_remote_endpoint(&rr.webrtc_sdp_answer).await
                                    {
                                        log::warn!("failed to set WebRTC relay answer: {}", err);
                                        false
                                    } else {
                                        for candidate in pending_webrtc_ice.drain(..) {
                                            if let Err(err) =
                                                webrtc.add_remote_ice_candidate(&candidate).await
                                            {
                                                log::warn!(
                                                    "failed to add buffered WebRTC ICE candidate: {}",
                                                    err
                                                );
                                            }
                                        }
                                        true
                                    }
                                } else {
                                    false
                                };
                                if let Some(webrtc) = setup_ok.then(|| guard.into_inner()).flatten()
                                {
                                    let session_key = webrtc.session_key().to_owned();
                                    let local_ice_rx = webrtc.take_local_ice_rx();
                                    webrtc_bridge_stop = Some(Self::spawn_webrtc_ice_bridge(
                                        socket,
                                        local_ice_rx,
                                        webrtc.clone(),
                                        peer.clone(),
                                        session_key,
                                    ));
                                    webrtc_for_connect = Some(webrtc);
                                }
                                // 如果 setup 失败，`guard` 在这里被 drop 并关闭 pc。
                            }
                        }
                        // 未被接管进中继竞速的 offerer（应答为空）会被
                        // guard 在这里的 drop 关闭。
                        drop(webrtc_offerer.take());
                        // 保留 relay_server 以便在 WebRTC 安全握手失败时回退：request_relay
                        // 通过 rendezvous 服务器协商出一个全新的 uuid，所以即使
                        // 竞速中的 create_relay 已经消耗掉原来的 uuid 配对，它依然可用。
                        let relay_server_rr = rr.relay_server.clone();
                        let fut = Self::create_relay(
                            &peer,
                            rr.uuid,
                            rr.relay_server,
                            &key,
                            conn_type,
                            my_addr.is_ipv4(),
                        );
                        connect_futures.push(
                            async move {
                                let conn = fut.await?;
                                Ok((
                                    conn,
                                    None,
                                    if use_ws() { "WebSocket" } else { "Relay" },
                                    false,
                                ))
                            }
                            .boxed(),
                        );
                        // 把已接管的 offerer 放在一个 guard 里，它在竞速和
                        // secure_connection 之间始终保持武装，这样外层竞速取消这个 future（或
                        // 安全握手失败）会关闭 pc，而不是泄漏它。它
                        // 只有在确认 WebRTC 是胜出且已加密的传输方式之后才解除武装。
                        let mut webrtc_guard = None;
                        let race_result = if let Some(webrtc) = webrtc_for_connect {
                            webrtc_guard = Some(OffererGuard::new(webrtc.clone()));
                            let mut raced = webrtc;
                            let webrtc_fut = async move {
                                raced.wait_connected(CONNECT_TIMEOUT).await?;
                                // 在这里判定是否走中继，而不是依据标签：WebRTC 只有在
                                // ICE 选定了非 TURN 候选对时才算 P2P 通路，而竞速必须
                                // 知道自己拿到的是哪一种。把 TURN 候选对当作直连提交，
                                // 就会取消仍在进行中的真正直连尝试——这正是
                                // 偏好窗口要防止的倒挂，只不过发生在更上一层。
                                let relayed = raced.is_relayed().await.unwrap_or(true);
                                Ok((Stream::WebRTC(raced), None, "WebRTC", !relayed))
                            }
                            .boxed();
                            if interface.is_policy_relay() {
                                // Relay-only 的 WebRTC 只能用 TURN，所以相比 RustDesk 的中继它没有任何 P2P 优势。
                                // 直接采用第一个成功的中继，
                                // 而不是为一个已经就绪的结果多等一个偏好窗口。
                                // 是策略，而不是 force_relay：在 ws 下 offer 是完整 ICE，而
                                // 直连通路正是偏好窗口存在的意义。
                                connect_futures.push(webrtc_fut);
                                select_ok(connect_futures).await.map(|r| r.0)
                            } else {
                                // 对端应答了 WebRTC：优先选择 P2P。中继结果会被暂存
                                // 一个偏好窗口，好让 WebRTC 有机会胜出，尽管中继
                                // 的 TCP 连接比 ICE + DTLS 建立快得多。
                                race_transports_prefer_webrtc(
                                    webrtc_fut,
                                    connect_futures,
                                    Self::relay_fallback_delay_ms(),
                                    |result| result.3,
                                )
                                .await
                            }
                        } else {
                            // 并发运行所有连接尝试，采用第一个成功的结果。
                            select_ok(connect_futures).await.map(|r| r.0)
                        };
                        if let Some(stop) = webrtc_bridge_stop {
                            let _ = stop.send(());
                        }
                        // 下面的 ? / secure_connection 失败会提前返回；webrtc_guard 仍然
                        // 在作用域内，会在任何这类退出时关闭 offerer（丢失、错误、取消）。
                        let (mut conn, kcp, mut typ, mut direct) = race_result?;
                        feedback = rr.feedback;
                        log::info!("{:?} used to establish {typ} connection", start.elapsed());
                        let pk = match Self::secure_connection(
                            &peer,
                            signed_id_pk.clone(),
                            &key,
                            &mut conn,
                        )
                        .await
                        {
                            Ok(pk) => pk,
                            Err(e) if typ == "WebRTC" => {
                                // WebRTC 赢得了竞速，但身份/DTLS 绑定失败。回退
                                // 到重新协商的中继（request_relay 会协商出一个新的
                                // uuid，不受竞速中的 create_relay 已经消耗掉
                                // 原配对影响），这样一次糟糕的 WebRTC 握手不会毁掉
                                // 整个会话——只要中继可用。
                                log::warn!(
                                    "WebRTC secure handshake failed ({}), falling back to relay",
                                    e
                                );
                                drop(webrtc_guard.take());
                                let mut relay_conn = Self::request_relay(
                                    &peer,
                                    relay_server_rr,
                                    &rendezvous_server,
                                    !signed_id_pk.is_empty(),
                                    &key,
                                    &token,
                                    conn_type,
                                    &interface.get_switch_code(),
                                )
                                .await
                                .map_err(|relay_e| {
                                    anyhow!(
                                        "WebRTC secure failed ({}); relay fallback also failed: {}",
                                        e,
                                        relay_e
                                    )
                                })?;
                                let pk = Self::secure_connection(
                                    &peer,
                                    signed_id_pk,
                                    &key,
                                    &mut relay_conn,
                                )
                                .await?;
                                conn = relay_conn;
                                typ = if use_ws() { "WebSocket" } else { "Relay" };
                                // 传输方式现在变成了中继：它所取代的那个 WebRTC 胜出结果
                                // 不能把它的 direct 标志带进返回值，否则中继会被
                                // 报告成 P2P，外层竞速也会这么对待它。
                                direct = false;
                                pk
                            }
                            Err(e) => return Err(e),
                        };
                        // `direct` 来自胜出的那个 future，它在 pc 确实
                        // 还活着时就解析出了它——竞速要选出胜者本来就离不开它。
                        // 已加密且 WebRTC 胜出：解除武装，让返回的 conn 保持 pc 存活。
                        if typ == "WebRTC" {
                            if let Some(guard) = webrtc_guard.take() {
                                let _ = guard.into_inner();
                            }
                        }
                        return Ok((
                            (conn, direct, pk, kcp, typ),
                            (feedback, rendezvous_server),
                            false,
                        ));
                    }
                    Some(rendezvous_message::Union::IceCandidate(ice)) => {
                        if Self::is_expected_webrtc_ice_candidate(&ice, &webrtc_session_key) {
                            // 淘汰最旧的，而不是最新的。候选按收集
                            // 顺序到达——先 host，再 srflx，最后 relay——所以丢弃后到的
                            // 恰恰会丢掉那些能穿越 NAT 的候选，却留下那些
                            // 只在同一局域网内才有效的 host 候选。
                            if pending_webrtc_ice.len() >= Self::MAX_PENDING_WEBRTC_ICE {
                                if let Some(n) = PENDING_ICE_FULL_LOG.due() {
                                    log::warn!(
                                        "WebRTC ICE pending buffer full ({}), evicted {} oldest",
                                        Self::MAX_PENDING_WEBRTC_ICE,
                                        n
                                    );
                                }
                                pending_webrtc_ice.remove(0);
                            }
                            pending_webrtc_ice.push(ice.candidate);
                        } else if let Some(n) = UNEXPECTED_ICE_LOG.due() {
                            log::debug!(
                                "dropped {} ICE candidate(s) for unexpected WebRTC session key, last: {}",
                                n,
                                ice.session_key,
                            );
                        }
                    }
                    _ => {
                        log::error!("Unexpected protobuf msg received: {:?}", msg_in);
                    }
                }
            }
        }
        let mut webrtc_bridge_stop = None;
        let mut webrtc_for_connect = None;
        if !webrtc_sdp_answer.is_empty() {
            if let Some(guard) = webrtc_offerer.take() {
                // 在 guard 借用的流上执行这些 await，这样在
                // 这些 await 期间发生取消时仍能通过 guard 的 drop 关闭 pc；只在
                // 下面的同步交接处才取得所有权。
                let setup_ok = if let Some(webrtc) = guard.stream() {
                    if let Err(err) = webrtc.set_remote_endpoint(&webrtc_sdp_answer).await {
                        log::warn!("failed to set WebRTC answer: {}", err);
                        false
                    } else {
                        for candidate in pending_webrtc_ice.drain(..) {
                            if let Err(err) = webrtc.add_remote_ice_candidate(&candidate).await {
                                log::warn!("failed to add buffered WebRTC ICE candidate: {}", err);
                            }
                        }
                        true
                    }
                } else {
                    false
                };
                if let Some(webrtc) = setup_ok.then(|| guard.into_inner()).flatten() {
                    let session_key = webrtc.session_key().to_owned();
                    let local_ice_rx = webrtc.take_local_ice_rx();
                    webrtc_bridge_stop = Some(Self::spawn_webrtc_ice_bridge(
                        socket,
                        local_ice_rx,
                        webrtc.clone(),
                        peer.clone(),
                        session_key,
                    ));
                    webrtc_for_connect = Some(webrtc);
                } else {
                    // setup 失败（guard 被 drop -> pc 已关闭）或没有流：释放套接字。
                    drop(socket);
                }
            } else {
                drop(socket);
            }
        } else {
            drop(socket);
        }
        // 从未被接管进任何连接尝试的 offerer（例如对端没有返回 WebRTC
        // 应答）会被 guard 在这里的 drop 关闭，这样它的 pc 不会滞留在 SESSIONS 里。
        drop(webrtc_offerer.take());
        if peer_addr.port() == 0 {
            // 在 connect() 之前放弃：已经被接管进 webrtc_for_connect 的 offerer
            // 已从它的 guard 中解除武装，所以在这里显式关闭它（并停止它的桥接）。
            if let Some(webrtc) = webrtc_for_connect.take() {
                webrtc.close_detached();
            }
            if let Some(stop) = webrtc_bridge_stop.take() {
                let _ = stop.send(());
            }
            bail!("Failed to connect via rendezvous server");
        }
        let time_used = start.elapsed().as_millis() as u64;
        log::info!(
            "{} ms used to {} punch hole, relay_server: {}, {}",
            time_used,
            punch_type,
            relay_server,
            if is_local {
                "is_local: true".to_owned()
            } else {
                format!("nat_type: {:?}", peer_nat_type)
            }
        );
        Ok((
            Self::connect(
                my_addr,
                peer_addr,
                &peer,
                signed_id_pk,
                &relay_server,
                &rendezvous_server,
                time_used,
                peer_nat_type,
                my_nat_type,
                is_local,
                &key,
                &token,
                conn_type,
                interface,
                udp.0,
                ipv6.0,
                webrtc_for_connect,
                webrtc_bridge_stop,
                allow_tcp_punch,
                &punch_type,
            )
            .await?,
            (feedback, rendezvous_server),
            true,
        ))
    }

    /// 连接到对端。
    async fn connect(
        local_addr: SocketAddr,
        peer: SocketAddr,
        peer_id: &str,
        signed_id_pk: Vec<u8>,
        relay_server: &str,
        rendezvous_server: &str,
        punch_time_used: u64,
        peer_nat_type: NatType,
        my_nat_type: i32,
        is_local: bool,
        key: &str,
        token: &str,
        conn_type: ConnType,
        interface: impl Interface,
        udp_socket_nat: Option<Arc<UdpSocket>>,
        udp_socket_v6: Option<Arc<UdpSocket>>,
        webrtc_offerer: Option<WebRTCStream>,
        webrtc_bridge_stop: Option<oneshot::Sender<()>>,
        allow_tcp_punch: bool,
        punch_type: &str,
    ) -> ResultType<(
        Stream,
        bool,
        Option<Vec<u8>>,
        Option<KcpStream>,
        &'static str,
    )> {
        // 在整个 connect() 期间守护 offerer：任何提前返回——包括这些
        // await 期间的取消、中继覆盖，或 secure_connection 失败——都会通过
        // guard 的 drop 关闭它的 pc。只有在确认 WebRTC 是胜出且已加密的传输方式后才解除武装。
        let mut webrtc_guard = webrtc_offerer.map(OffererGuard::new);
        let direct_failures = interface.get_lch().read().unwrap().direct_failures;
        let mut connect_timeout = 0;
        const MIN: u64 = 1000;
        if is_local || peer_nat_type == NatType::SYMMETRIC {
            connect_timeout = MIN;
        } else {
            if relay_server.is_empty() {
                connect_timeout = CONNECT_TIMEOUT;
            } else {
                if peer_nat_type == NatType::ASYMMETRIC {
                    let mut my_nat_type = my_nat_type;
                    if my_nat_type == NatType::UNKNOWN_NAT as i32 {
                        my_nat_type = crate::get_nat_type(100).await;
                    }
                    if my_nat_type == NatType::ASYMMETRIC as i32 {
                        connect_timeout = CONNECT_TIMEOUT;
                        if direct_failures > 0 {
                            connect_timeout = punch_time_used * 6;
                        }
                    } else if my_nat_type == NatType::SYMMETRIC as i32 {
                        connect_timeout = MIN;
                    }
                }
                if connect_timeout == 0 {
                    let n = if direct_failures > 0 { 3 } else { 6 };
                    connect_timeout = punch_time_used * (n as u64);
                }
            }
            if connect_timeout < MIN {
                connect_timeout = MIN;
            }
        }
        log::info!("peer address: {}, timeout: {}", peer, connect_timeout);
        let start = std::time::Instant::now();

        // 每次尝试都携带其路径是否为直连（第 4 个字段）。TCP/UDP/IPv6 打洞
        // 始终是直连；WebRTC 只有在 ICE 选定了非 TURN 候选对时才算直连。
        let mut direct_futures = Vec::new();
        if allow_tcp_punch {
            let fut = connect_tcp_local(peer, Some(local_addr), connect_timeout);
            direct_futures.push(
                async move {
                    let conn = fut.await?;
                    Ok((conn, None, "TCP", true))
                }
                .boxed(),
            );
        }
        if let Some(udp_socket_nat) = udp_socket_nat {
            direct_futures.push(
                async move {
                    let (conn, kcp, typ) =
                        udp_nat_connect(udp_socket_nat, "UDP", connect_timeout).await?;
                    Ok((conn, kcp, typ, true))
                }
                .boxed(),
            );
        }
        if let Some(udp_socket_v6) = udp_socket_v6 {
            direct_futures.push(
                async move {
                    let (conn, kcp, typ) =
                        udp_nat_connect(udp_socket_v6, "IPv6", connect_timeout).await?;
                    Ok((conn, kcp, typ, true))
                }
                .boxed(),
            );
        }
        // 用 offerer 的克隆参与竞速；guard 保留自己的克隆，这样落败或被取消的竞速
        // 仍会关闭 pc（select_ok 只是丢弃 future 的克隆而不关闭它）。
        let webrtc_fut = webrtc_guard
            .as_ref()
            .and_then(|g| g.stream())
            .map(|stream| {
                let mut raced = stream.clone();
                // 按打洞调校的超时可能低到 1 秒——够一次裸 TCP SYN，但不够
                // 候选 trickle + ICE 检查 + DTLS。给 WebRTC 自己设一个下限（prefer-P2P），
                // 这样一条可行的 P2P 通路不会在完成之前就被放弃；TCP/UDP 仍保持
                // 更紧的超时，因此可用的直连连接仍会立刻胜出，而
                // 中继回退只有在所有直连尝试都失败时才会多等那段时间。
                let webrtc_timeout = connect_timeout.max(Self::relay_fallback_delay_ms());
                async move {
                    raced.wait_connected(webrtc_timeout).await?;
                    // 在这里判定候选对：TURN 胜出属于走中继，不是直连，必须
                    // 排在仍在竞速的直连尝试之后，而不能直接当作 P2P 提交。
                    let relayed = raced.is_relayed().await.unwrap_or(true);
                    Ok((Stream::WebRTC(raced), None, "WebRTC", !relayed))
                }
                .boxed()
            });
        // 优先 P2P：直连结果直接胜出，走中继的 WebRTC（TURN）会被暂存
        // 一个窗口，好让直连打洞仍有机会落地。当只有一种尝试存在时，退回普通的 select_ok；
        // 有多种时则按上面的 P2P 优先规则处理。
        let direct_result = match (webrtc_fut, direct_futures.is_empty()) {
            (Some(webrtc_fut), false) => {
                race_transports_prefer_webrtc(
                    webrtc_fut,
                    direct_futures,
                    Self::relay_fallback_delay_ms(),
                    |r| r.3,
                )
                .await
            }
            (Some(webrtc_fut), true) => webrtc_fut.await,
            (None, false) => select_ok(direct_futures).await.map(|c| c.0),
            (None, true) => Err(anyhow!("No direct transport available")),
        };
        let (mut conn, kcp, mut typ, mut direct) = match direct_result {
            Ok((conn, kcp, typ, direct)) => (Ok(conn), kcp, typ, direct),
            Err(e) => (Err(e), None, "", false),
        };
        if let Some(stop) = webrtc_bridge_stop {
            let _ = stop.send(());
        }
        // webrtc_guard 在下面的中继覆盖和 secure_connection 之间始终保持武装；它
        // 只在成功返回、且保留 WebRTC 作为传输方式时才解除武装。

        // 保留 WebRTC 的胜出结果，而不是用 RustDesk 中继替换它：在按策略
        // 中继的情况下，pc 是用 Relay-only ICE（TURN 已配置）构建的，这本身已经满足
        // 中继要求；而在 ws 强制中继的情况下，一条直连的完整 ICE 连接恰恰是
        // preferred 的结果，而不是违规。
        if (interface.is_force_relay() && typ != "WebRTC") || conn.is_err() {
            if !relay_server.is_empty() {
                let switch_code = interface.get_switch_code();
                conn = Self::request_relay(
                    peer_id,
                    relay_server.to_owned(),
                    rendezvous_server,
                    !signed_id_pk.is_empty(),
                    key,
                    token,
                    conn_type,
                    &switch_code,
                )
                .await;
                if let Err(e) = conn {
                    // 这个 direct 主要被 on_establish_connection_error 使用，所以在 bail 之前先在这里更新它
                    interface.update_direct(Some(false));
                    bail!("Failed to connect via relay server: {}", e);
                }
                typ = "Relay";
                direct = false;
            } else {
                bail!("Failed to make direct connection to remote desktop");
            }
        }
        let mut conn = conn?;
        log::info!(
            "{:?} used to establish {typ} connection with {} punch",
            start.elapsed(),
            punch_type
        );
        let res = Self::secure_connection(peer_id, signed_id_pk.clone(), key, &mut conn).await;
        let pk: Option<Vec<u8>> = match res {
            Ok(pk) => pk,
            Err(e) if typ == "WebRTC" && !relay_server.is_empty() => {
                // WebRTC 赢得了竞速，但身份/DTLS 绑定失败；回退到一个重新
                // 协商的中继，而不是让整个尝试失败。guard 会被
                // 优先 drop，以便那个坏掉的 pc 尽快关闭。
                log::warn!("WebRTC secure handshake failed ({}), falling back to relay", e);
                drop(webrtc_guard.take());
                match Self::request_relay(
                    peer_id,
                    relay_server.to_owned(),
                    rendezvous_server,
                    !signed_id_pk.is_empty(),
                    key,
                    token,
                    conn_type,
                    &interface.get_switch_code(),
                )
                .await
                {
                    Ok(mut relay_conn) => {
                        match Self::secure_connection(peer_id, signed_id_pk, key, &mut relay_conn)
                            .await
                        {
                            Ok(pk) => {
                                conn = relay_conn;
                                typ = "Relay";
                                direct = false;
                                pk
                            }
                            Err(e) => {
                                interface.update_direct(Some(false));
                                bail!(e);
                            }
                        }
                    }
                    Err(relay_e) => {
                        interface.update_direct(Some(direct));
                        bail!(
                            "WebRTC secure failed ({}); relay fallback also failed: {}",
                            e,
                            relay_e
                        );
                    }
                }
            }
            Err(e) => {
                // 这个 direct 主要被 on_establish_connection_error 使用，所以在 bail 之前先在这里更新它
                interface.update_direct(Some(direct));
                // webrtc_guard 在这里仍然处于武装状态，所以那个安全握手
                // 失败的 WebRTC 胜出结果会在我们 bail 时被 guard 的 drop 关闭（无需显式关闭）。
                bail!(e);
            }
        };
        if typ == "WebRTC" {
            // 经由 TURN 服务器的 WebRTC（force_relay，或在 All
            // 策略下由 TURN 候选对胜出）属于中继流量；要相应地报告 direct 标志。无法识别的应答
            // 也算走中继——声称有一条 P2P 通路需要有证据。
            if conn.webrtc_relayed().await.unwrap_or(true) {
                direct = false;
            }
            // 已加密：解除武装，让返回的 conn 保持 pc 存活。
            if let Some(guard) = webrtc_guard.take() {
                let _ = guard.into_inner();
            }
        }
        log::debug!("{} punch secure_connection ok", punch_type);
        Ok((conn, direct, pk, kcp, typ))
    }

    /// 与服务器建立安全连接。
    async fn secure_connection(
        peer_id: &str,
        signed_id_pk: Vec<u8>,
        key: &str,
        conn: &mut Stream,
    ) -> ResultType<Option<Vec<u8>>> {
        let rs_pk = get_rs_pk(if key.is_empty() {
            config::RS_PUB_KEY
        } else {
            key
        });
        // 只有当 WebRTC 通道的 DTLS 指纹绑定到下面经过校验的
        // 对端身份之后，它才算完成了对端认证。一旦建立了可信身份，任何绑定
        // 失败都按“失败即关闭”处理：任何能应答 WebRTC 的对端都会签署自己的指纹，所以
        // 不匹配就是 rendezvous/中继 MITM 的确凿证据（而不是旧版对端），
        // 调用方会回退到一条全新的中继连接，而不是继续用该通道。
        // 没有可信身份（缺失，或在我们配置的根下无法校验）时，根本
        // 无法建立任何绑定；此时 WebRTC 的处理方式与 TCP 的非安全回退相同——
        // 经 DTLS 加密，但报告为未加密。TCP/UDP/中继保持原有行为。
        let is_webrtc = conn.is_webrtc();
        let mut sign_pk = None;
        let mut option_pk = None;
        if !signed_id_pk.is_empty() {
            if let Some(rs_pk) = rs_pk {
                if let Ok((id, pk)) = decode_id_pk(&signed_id_pk, &rs_pk) {
                    if id == peer_id {
                        sign_pk = Some(sign::PublicKey(pk));
                        option_pk = Some(pk.to_vec());
                    }
                }
            }
            if sign_pk.is_none() {
                log::error!("Handshake failed: invalid public key from rendezvous server");
            }
        }
        let sign_pk = match sign_pk {
            Some(v) => v,
            None => {
                // 没有可信的对端身份（无密钥部署，或某个 blob 在我们的根下无法
                // 校验），因此无法进行指纹绑定。像 TCP 的
                // 非安全路径一样继续，is_secured() 为 false：直接放弃只会把会话转移到
                // 一个同样通不过检查、然后明文运行的中继上。
                // 发一条空消息出去，以防服务器正在建立安全连接并等待第一条消息
                conn.send(&Message::new()).await?;
                return Ok(option_pk);
            }
        };
        match timeout(READ_TIMEOUT, conn.next()).await? {
            Some(res) => {
                let bytes = res?;
                if let Ok(msg_in) = Message::parse_from_bytes(&bytes) {
                    if let Some(message::Union::SignedId(si)) = msg_in.union {
                        if let Ok((id, their_pk_b, signed_fp, kx_version)) =
                            decode_id_pk_dtls(&si.id, &sign_pk)
                        {
                            if id == peer_id {
                                // 仅 WebRTC：把 DTLS 通道绑定到已校验的对端身份。
                                // webrtc-rs 已经把证书绑定到了远端 SDP，因此
                                // 要求对端“签署”该指纹就能挫败一种
                                // 通过替换 SDP 实施的 rendezvous/中继 MITM。按“失败即关闭”处理。
                                if is_webrtc {
                                    let actual_fp = conn.dtls_fingerprint(false).await.ok_or_else(
                                        || anyhow!("WebRTC DTLS fingerprint unavailable"),
                                    )?;
                                    if !dtls_fingerprint_bound(&signed_fp, &actual_fp) {
                                        bail!("WebRTC DTLS fingerprint not bound to peer identity (possible MITM)");
                                    }
                                }
                                let (asymmetric_value, symmetric_value, key) =
                                    create_symmetric_key_msg(their_pk_b);
                                // WebRTC 流由 DTLS 加密，本身不带任何流
                                // 密钥（无论是否分片），所以这个选择决定实际运行什么：0。
                                let picked = if is_webrtc {
                                    0
                                } else {
                                    hbb_common::tcp::kx_version_for(kx_version)
                                };
                                let mut msg_out = Message::new();
                                msg_out.set_public_key(PublicKey {
                                    asymmetric_value: asymmetric_value.clone(),
                                    symmetric_value,
                                    kx_version: picked,
                                    ..Default::default()
                                });
                                timeout(CONNECT_TIMEOUT, conn.send(&msg_out)).await??;
                                conn.set_negotiated_key(
                                    key,
                                    true,
                                    &hbb_common::tcp::KxTranscript {
                                        initiator_pk: &asymmetric_value,
                                        responder_pk: &their_pk_b,
                                        advertised: kx_version,
                                        picked,
                                    },
                                )?;
                            } else {
                                if is_webrtc {
                                    bail!("WebRTC handshake id mismatch (possible MITM)");
                                }
                                log::error!("Handshake failed: sign failure");
                                conn.send(&Message::new()).await?;
                            }
                        } else {
                            if is_webrtc {
                                bail!("WebRTC peer identity could not be verified (refusing unbound channel)");
                            }
                            // 在公钥不匹配时回退到非安全连接
                            log::info!("pk mismatch, fall back to non-secure");
                            let mut msg_out = Message::new();
                            msg_out.set_public_key(PublicKey::new());
                            conn.send(&msg_out).await?;
                        }
                    } else {
                        if is_webrtc {
                            bail!("WebRTC handshake received an unexpected message type");
                        }
                        log::error!("Handshake failed: invalid message type");
                        conn.send(&Message::new()).await?;
                    }
                } else {
                    if is_webrtc {
                        bail!("WebRTC handshake received a malformed message");
                    }
                    log::error!("Handshake failed: invalid message format");
                    conn.send(&Message::new()).await?;
                }
            }
            None => {
                bail!("Reset by the peer");
            }
        }
        Ok(option_pk)
    }

    /// 向服务器请求一条中继连接。
    async fn request_relay(
        peer: &str,
        relay_server: String,
        rendezvous_server: &str,
        secure: bool,
        key: &str,
        token: &str,
        conn_type: ConnType,
        switch_code: &str,
    ) -> ResultType<Stream> {
        let mut succeed = false;
        let mut uuid = "".to_owned();
        let mut ipv4 = true;

        for i in 1..=3 {
            // 由于当前 hbbs 实现要求每次尝试使用不同的 nat 地址，所以使用不同的套接字
            let mut socket = connect_tcp(rendezvous_server, CONNECT_TIMEOUT)
                .await
                .with_context(|| "Failed to connect to rendezvous server")?;

            if !key.is_empty() && (!token.is_empty() || !switch_code.is_empty()) {
                secure_tcp(&mut socket, key).await?;
            }

            ipv4 = socket.local_addr().is_ipv4();
            let mut msg_out = RendezvousMessage::new();
            uuid = Uuid::new_v4().to_string();
            log::info!(
                "#{} request relay attempt, id: {}, uuid: {}, relay_server: {}, secure: {}",
                i,
                peer,
                uuid,
                relay_server,
                secure,
            );
            msg_out.set_request_relay(RequestRelay {
                id: peer.to_owned(),
                token: token.to_owned(),
                uuid: uuid.clone(),
                relay_server: relay_server.clone(),
                secure,
                switch_code: switch_code.to_owned(),
                ..Default::default()
            });
            socket.send(&msg_out).await?;

            if let Some(msg_in) =
                crate::get_next_nonkeyexchange_msg(&mut socket, Some(CONNECT_TIMEOUT)).await
            {
                if let Some(rendezvous_message::Union::RelayResponse(rs)) = msg_in.union {
                    if !rs.refuse_reason.is_empty() {
                        bail!(rs.refuse_reason);
                    }
                    succeed = true;
                    break;
                }
            }
        }
        if !succeed {
            bail!("Timeout");
        }
        Self::create_relay(peer, uuid, relay_server, key, conn_type, ipv4).await
    }

    /// 创建一条到服务器的中继连接。
    async fn create_relay(
        peer: &str,
        uuid: String,
        relay_server: String,
        key: &str,
        conn_type: ConnType,
        ipv4: bool,
    ) -> ResultType<Stream> {
        // 代理的地址族并不约束中继目标。
        let relay_server = check_port(relay_server, RELAY_PORT);
        let use_ws_proxy = Config::is_proxy() && is_ws_endpoint(&check_ws(&relay_server));
        let mut conn = connect_tcp(
            ipv4_to_ipv6(relay_server, ipv4 || use_ws_proxy),
            CONNECT_TIMEOUT,
        )
        .await
        .with_context(|| "Failed to connect to relay server")?;
        let mut msg_out = RendezvousMessage::new();
        msg_out.set_request_relay(RequestRelay {
            licence_key: key.to_owned(),
            id: peer.to_owned(),
            uuid,
            conn_type: conn_type.into(),
            ..Default::default()
        });
        conn.send(&msg_out).await?;
        Ok(conn)
    }

    #[inline]
    #[cfg(feature = "flutter")]
    #[cfg(not(target_os = "ios"))]
    pub fn set_is_text_clipboard_required(b: bool) {
        CLIPBOARD_STATE.lock().unwrap().is_text_required = b;
    }

    #[inline]
    #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
    pub fn set_is_file_clipboard_required(b: bool) {
        CLIPBOARD_STATE.lock().unwrap().is_file_required = b;
    }

    #[cfg(not(target_os = "ios"))]
    fn try_stop_clipboard() {
        // 已断开的 Flutter 会话可能仍让 UI 处理器存活，所以只有已连接的会话
        // 才应阻止剪贴板清理。
        #[cfg(feature = "flutter")]
        if crate::flutter::sessions::has_connected_sessions_running(ConnType::DEFAULT_CONN) {
            return;
        }
        #[cfg(not(target_os = "android"))]
        clipboard_listener::unsubscribe(Self::CLIENT_CLIPBOARD_NAME);
        CLIPBOARD_STATE.lock().unwrap().running = false;
        #[cfg(all(feature = "unix-file-copy-paste", target_os = "linux"))]
        if let Err(e) = crate::clipboard::try_empty_clipboard_files_sync(
            crate::clipboard::ClipboardSide::Client,
            0,
        ) {
            log::error!("Failed to empty client clipboard files: {}", e);
        }
        #[cfg(all(feature = "unix-file-copy-paste", target_os = "linux"))]
        clipboard::platform::unix::fuse::uninit_fuse_context(true);
    }

    // 连接建立时所有会话都会调用 `try_start_clipboard`。（在处理对端信息时）。
    // 这个函数只创建一个带循环的线程，该循环由所有会话共享。
    // 所有会话结束后，循环就退出。
    //
    // 检测到剪贴板更新时，文本会通过 `send_clipboard_msg` 发送给所有会话。
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn try_start_clipboard(
        _client_clip_ctx: Option<ClientClipboardContext>,
    ) -> Option<UnboundedReceiver<()>> {
        let mut clipboard_lock = CLIPBOARD_STATE.lock().unwrap();
        if clipboard_lock.running {
            return None;
        }

        let (tx_cb_result, rx_cb_result) = mpsc::channel();
        if let Err(e) =
            clipboard_listener::subscribe(Self::CLIENT_CLIPBOARD_NAME.to_owned(), tx_cb_result)
        {
            log::error!("Failed to subscribe clipboard listener: {}", e);
            return None;
        }

        clipboard_lock.running = true;
        let (tx_started, rx_started) = unbounded_channel();

        log::info!("Start client clipboard loop");
        std::thread::spawn(move || {
            let mut handler = ClientClipboardHandler {
                ctx: None,
                #[cfg(not(feature = "flutter"))]
                client_clip_ctx: _client_clip_ctx,
            };

            tx_started.send(()).ok();
            loop {
                if !CLIPBOARD_STATE.lock().unwrap().running {
                    break;
                }
                match rx_cb_result.recv_timeout(Duration::from_millis(CLIPBOARD_INTERVAL)) {
                    Ok(ClipboardEvent::Changed) => {
                        handler.check_clipboard(true);
                    }
                    #[cfg(all(target_os = "linux", feature = "unix-file-copy-paste"))]
                    Ok(ClipboardEvent::InitialSelection) => {
                        // 保留文件启动同步，同时不绕过文本初始同步的设置。
                        handler.check_clipboard(false);
                    }
                    Ok(ClipboardEvent::Stop) => {
                        log::debug!("Clipboard listener stopped");
                        break;
                    }
                    Ok(ClipboardEvent::StopWithError(err)) => {
                        log::error!("Clipboard listener stopped with error: {}", err);
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        log::error!("Clipboard listener disconnected");
                        break;
                    }
                }
            }
            log::info!("Stop client clipboard loop");
            CLIPBOARD_STATE.lock().unwrap().running = false;
        });

        Some(rx_started)
    }

    #[cfg(target_os = "android")]
    fn try_start_clipboard(_p: Option<()>) -> Option<UnboundedReceiver<()>> {
        let mut clipboard_lock = CLIPBOARD_STATE.lock().unwrap();
        if clipboard_lock.running {
            return None;
        }
        clipboard_lock.running = true;

        log::info!("Start client clipboard loop");
        std::thread::spawn(move || {
            loop {
                if !CLIPBOARD_STATE.lock().unwrap().running {
                    break;
                }
                if !CLIPBOARD_STATE.lock().unwrap().is_text_required {
                    std::thread::sleep(Duration::from_millis(CLIPBOARD_INTERVAL));
                    continue;
                }

                if let Some(msg) = crate::clipboard::get_clipboards_msg(true) {
                    crate::flutter::send_clipboard_msg(msg, false);
                }

                std::thread::sleep(Duration::from_millis(CLIPBOARD_INTERVAL));
            }
            log::info!("Stop client clipboard loop");
            CLIPBOARD_STATE.lock().unwrap().running = false;
        });

        None
    }
}

#[cfg(not(target_os = "ios"))]
impl ClipboardState {
    fn new() -> Self {
        Self {
            #[cfg(feature = "flutter")]
            is_text_required: true,
            #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
            is_file_required: true,
            running: false,
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct ClientClipboardHandler {
    ctx: Option<crate::clipboard::ClipboardContext>,
    #[cfg(not(feature = "flutter"))]
    client_clip_ctx: Option<ClientClipboardContext>,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl ClientClipboardHandler {
    fn is_text_required(&self) -> bool {
        #[cfg(feature = "flutter")]
        {
            CLIPBOARD_STATE.lock().unwrap().is_text_required
        }
        #[cfg(not(feature = "flutter"))]
        {
            self.client_clip_ctx
                .as_ref()
                .map(|ctx| ctx.cfg.is_text_clipboard_required())
                .unwrap_or(false)
        }
    }

    #[cfg(feature = "unix-file-copy-paste")]
    fn is_file_required(&self) -> bool {
        #[cfg(feature = "flutter")]
        {
            CLIPBOARD_STATE.lock().unwrap().is_file_required
        }
        #[cfg(not(feature = "flutter"))]
        {
            self.client_clip_ctx
                .as_ref()
                .map(|ctx| ctx.cfg.is_file_clipboard_required())
                .unwrap_or(false)
        }
    }

    fn check_clipboard(&mut self, check_text: bool) {
        if CLIPBOARD_STATE.lock().unwrap().running {
            #[cfg(feature = "unix-file-copy-paste")]
            if self.is_file_required() {
                if let Some(urls) = check_clipboard_files(&mut self.ctx, ClipboardSide::Client, false) {
                    if !urls.is_empty() {
                        #[cfg(target_os = "macos")]
                        if crate::clipboard::is_file_url_set_by_rustdesk(&urls) {
                            return;
                        }
                        match clipboard::platform::unix::serv_files::sync_files(&urls) {
                            Ok(()) => {
                                let msg = crate::clipboard_file::clip_2_msg(
                                    unix_file_clip::get_format_list(),
                                );
                                self.send_msg(msg, true);
                            }
                            Err(e) => {
                                log::error!("Failed to sync clipboard files: {}", e);
                            }
                        }
                        return;
                    }
                }
            }

            if check_text && self.is_text_required() {
                if let Some(msg) = check_clipboard(&mut self.ctx, ClipboardSide::Client, false) {
                    self.send_msg(msg, false);
                }
            }
        }
    }

    #[inline]
    #[cfg(feature = "flutter")]
    fn send_msg(&self, msg: Message, _is_file: bool) {
        crate::flutter::send_clipboard_msg(msg, _is_file);
    }

    #[cfg(not(feature = "flutter"))]
    fn send_msg(&self, msg: Message, _is_file: bool) {
        if let Some(ctx) = &self.client_clip_ctx {
            #[cfg(feature = "unix-file-copy-paste")]
            if _is_file {
                if ctx.is_file_supported {
                    let _ = ctx.tx.send(Data::Message(msg));
                }
                return;
            }

            let pi = ctx.cfg.lc.read().unwrap().peer_info.clone();
            if let Some(pi) = pi.as_ref() {
                if let Some(message::Union::MultiClipboards(multi_clipboards)) = &msg.union {
                    if let Some(msg_out) = crate::clipboard::get_msg_if_not_support_multi_clip(
                        &pi.version,
                        &pi.platform,
                        multi_clipboards,
                    ) {
                        let _ = ctx.tx.send(Data::Message(msg_out));
                        return;
                    }
                }
            }
            let _ = ctx.tx.send(Data::Message(msg));
        }
    }
}

/// [`Client`] 的音频处理器。
#[derive(Default)]
pub struct AudioHandler {
    audio_decoder: Option<(AudioDecoder, Vec<f32>)>,
    #[cfg(target_os = "linux")]
    simple: Option<psimple::Simple>,
    #[cfg(not(target_os = "linux"))]
    audio_buffer: AudioBuffer,
    #[cfg(not(target_os = "linux"))]
    audio_resampler: Option<crate::audio_resampler::AudioResampler>,
    sample_rate: (u32, u32),
    #[cfg(not(target_os = "linux"))]
    audio_stream: Option<Box<dyn StreamTrait>>,
    channels: u16,
    #[cfg(not(target_os = "linux"))]
    device_channel: u16,
    #[cfg(not(target_os = "linux"))]
    playback_status: Arc<audio_playback::AudioPlaybackStatus>,
    #[cfg(target_os = "windows")]
    playback_recovery: audio_playback_recovery::PlaybackRecovery,
}

#[cfg(not(target_os = "linux"))]
#[derive(Clone, Copy)]
struct DecodedAudioConfig {
    sample_rate: u32,
    input_channels: u16,
    output_channels: u16,
}

#[cfg(not(target_os = "linux"))]
fn create_audio_resampler(
    input_rate: u32,
    output_rate: u32,
    channels: u16,
) -> ResultType<Option<crate::audio_resampler::AudioResampler>> {
    if input_rate == output_rate {
        return Ok(None);
    }
    Ok(Some(crate::audio_resampler::AudioResampler::new(
        crate::audio_resampler::AudioResamplerConfig {
            input_rate,
            output_rate,
            channels,
        },
    )?))
}

#[cfg(not(target_os = "linux"))]
fn prepare_decoded_audio(
    input: &[f32],
    resampler: Option<&mut crate::audio_resampler::AudioResampler>,
    config: DecodedAudioConfig,
) -> Result<Vec<f32>, crate::audio_resampler::AudioResamplerError> {
    let mut output = match resampler {
        Some(resampler) => resampler.process(input)?,
        None => input.to_owned(),
    };
    if config.input_channels != config.output_channels {
        output = crate::audio_rechannel(
            output,
            config.sample_rate,
            config.sample_rate,
            config.input_channels,
            config.output_channels,
        );
    }
    Ok(output)
}

#[cfg(not(target_os = "linux"))]
struct AudioBuffer(
    pub Arc<std::sync::Mutex<ringbuf::HeapRb<f32>>>,
    usize,
    [usize; 30],
    Arc<std::sync::atomic::AtomicUsize>,
);

#[cfg(not(target_os = "linux"))]
impl Default for AudioBuffer {
    fn default() -> Self {
        Self(
            Arc::new(std::sync::Mutex::new(
                ringbuf::HeapRb::<f32>::new(48000 * 2 * AUDIO_BUFFER_MS / 1000), // 48000hz, 2 channel
            )),
            48000 * 2,
            [0; 30],
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
    }
}

#[cfg(not(target_os = "linux"))]
impl AudioBuffer {
    pub fn resize(&mut self, sample_rate: usize, channels: usize) {
        let capacity = sample_rate * channels * AUDIO_BUFFER_MS / 1000;
        let old_capacity = self.0.lock().unwrap().capacity();
        if capacity != old_capacity {
            * self.0.lock().unwrap() = ringbuf::HeapRb::<f32>::new(capacity);
            self.1 = sample_rate * channels;
            log::info!("Audio buffer resized from {old_capacity} to {capacity}");
        }
    }

    fn try_shrink(&mut self, having: usize) {
        extern crate chrono;
        use chrono::prelude::*;

        let mut i = (having * 10) / self.1;
        if i > 29 {
            i = 29;
        }
        self.2[i] += 1;

        #[allow(non_upper_case_globals)]
        static mut tms: i64 = 0;
        let dt = Local::now().timestamp_millis();
        unsafe {
            if tms == 0 {
                tms = dt;
                return;
            } else if dt < tms + 12000 {
                return;
            }
            tms = dt;
        }

        // 更安全的水位线，超过就丢弃
        let mut zero = 0;
        // 耗时最多的水位线
        let mut max = 0;
        for i in 0..30 {
            if self.2[i] == 0 && zero == i {
                zero += 1;
            }

            if self.2[i] > self.2[max] {
                self.2[max] = 0;
                max = i;
            } else {
                self.2[i] = 0;
            }
        }
        zero = zero * 2 / 3;

        // 可以丢弃多少数据：
        // 1. 缓冲数据少于 600ms 时不丢弃
        // 2. 基于 min(zero, max) 选择
        const N: usize = 4;
        self.2[max] = 0;
        if max < 6 {
            return;
        } else if max > zero * N {
            max = zero * N;
        }

        let mut lock = self.0.lock().unwrap();
        let cap = lock.capacity();
        let having = lock.occupied_len();
        let skip = (cap * max / (30 * N) + 1) & (!1);
        if (having > skip * 3) && (skip > 0) {
            lock.skip(skip);
            let generation = self.signal_discontinuity();
            drop(lock);
            log::info!("skip {skip}, based {max} {zero}, generation={generation}");
        }
    }

    /// 调用方在发出丢弃信号时必须持有 PCM 缓冲区锁。
    fn signal_discontinuity(&self) -> usize {
        self.3
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// 把 pcm 追加到音频缓冲区，如果缓冲数据
    /// 超过 AUDIO_BUFFER_MS，就只保留 AUDIO_BUFFER_MS
    /// 的部分。
    fn append_pcm2(&self, buffer: &[f32]) -> usize {
        let mut lock = self.0.lock().unwrap();
        let cap = lock.capacity();
        let having = lock.occupied_len() + buffer.len();
        lock.push_slice_overwrite(buffer);
        let discard = (having > cap).then(|| (having - cap, self.signal_discontinuity()));
        let occupied = lock.occupied_len();
        drop(lock);
        if let Some((discarded, generation)) = discard {
            hbb_common::throttled_log!(
                audio_playback::AUDIO_PLAYBACK_LOG_INTERVAL,
                debug,
                "Audio buffer capacity discard: samples={discarded}, generation={generation}"
            );
        }
        occupied
    }

    /// 把 pcm 追加到音频缓冲区，尝试丢弃数据
    /// 当数据过多时（按 12 秒统计）依据
    /// 统计结果。
    pub fn append_pcm(&mut self, buffer: &[f32]) {
        let having = self.append_pcm2(buffer);
        self.try_shrink(having);
    }
}

impl AudioHandler {
    #[cfg(target_os = "linux")]
    fn start_audio(&mut self, format0: AudioFormat) -> ResultType<()> {
        use psimple::Simple;
        use pulse::sample::{Format, Spec};
        use pulse::stream::Direction;

        let spec = Spec {
            format: Format::F32le,
            channels: format0.channels as _,
            rate: format0.sample_rate as _,
        };
        if !spec.is_valid() {
            bail!("Invalid audio format");
        }

        self.simple = Some(Simple::new(
            None,                   // Use the default server
            &crate::get_app_name(), // Our application’s name
            Direction::Playback,    // We want a playback stream
            None,                   // Use the default device
            "playback",             // Description of our stream
            &spec,                  // Our sample format
            None,                   // Use default channel map
            None,                   // Use default buffering attributes
        )?);
        self.sample_rate = (format0.sample_rate, format0.sample_rate);
        Ok(())
    }

    /// 启动音频播放。
    #[cfg(not(target_os = "linux"))]
    fn start_audio(&mut self, format0: AudioFormat) -> ResultType<()> {
        let device = AUDIO_HOST
            .default_output_device()
            .with_context(|| "Failed to get default output device")?;
        log::info!(
            "Using default output device: \"{}\"",
            device.name().unwrap_or("".to_owned())
        );
        let config = device.default_output_config().map_err(|e| anyhow!(e))?;
        let sample_format = config.sample_format();
        log::info!("Default output format: {:?}", config);
        log::info!("Remote input format: {:?}", format0);
        #[allow(unused_mut)]
        let mut config: StreamConfig = config.into();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            // 这会导致 ios 和 android 的音频输出不工作
            config.buffer_size = cpal::BufferSize::Fixed(64);
        }

        self.sample_rate = (format0.sample_rate, config.sample_rate.0);
        let audio_resampler = create_audio_resampler(
            format0.sample_rate, config.sample_rate.0, format0.channels as _,
        )?;
        let mut build_output_stream = |config: StreamConfig| match sample_format {
            cpal::SampleFormat::I8 => self.build_output_stream::<i8>(&config, &device),
            cpal::SampleFormat::I16 => self.build_output_stream::<i16>(&config, &device),
            cpal::SampleFormat::I32 => self.build_output_stream::<i32>(&config, &device),
            cpal::SampleFormat::I64 => self.build_output_stream::<i64>(&config, &device),
            cpal::SampleFormat::U8 => self.build_output_stream::<u8>(&config, &device),
            cpal::SampleFormat::U16 => self.build_output_stream::<u16>(&config, &device),
            cpal::SampleFormat::U32 => self.build_output_stream::<u32>(&config, &device),
            cpal::SampleFormat::U64 => self.build_output_stream::<u64>(&config, &device),
            cpal::SampleFormat::F32 => self.build_output_stream::<f32>(&config, &device),
            cpal::SampleFormat::F64 => self.build_output_stream::<f64>(&config, &device),
            f => bail!("unsupported audio format: {:?}", f),
        };
        if config.channels > format0.channels as _ {
            let no_rechannel_config = StreamConfig {
                channels: format0.channels as _,
                ..config.clone()
            };
            if let Err(_) = build_output_stream(no_rechannel_config) {
                build_output_stream(config)?;
            }
        } else {
            build_output_stream(config)?;
        }
        self.audio_resampler = audio_resampler;

        Ok(())
    }

    /// 处理音频格式并创建音频解码器。
    pub fn handle_format(&mut self, f: AudioFormat) {
        self.handle_format_with_start(f, Self::start_audio);
    }

    fn handle_format_with_start(
        &mut self,
        f: AudioFormat,
        start: impl FnOnce(&mut Self, AudioFormat) -> ResultType<()>,
    ) {
        if !is_supported_audio_channel_count(f.channels) {
            log::error!("Unsupported audio channel count: {}", f.channels);
            return;
        }
        match AudioDecoder::new(f.sample_rate, if f.channels > 1 { Stereo } else { Mono }) {
            Ok(d) => {
                #[cfg(target_os = "windows")]
                let playback_failed = self.cancel_pending_playback();
                #[cfg(target_os = "linux")]
                let keep_existing_stream = self.simple.is_some()
                    && self.sample_rate.0 == f.sample_rate
                    && u32::from(self.channels) == f.channels;
                #[cfg(not(target_os = "linux"))]
                let keep_existing_stream = self.audio_stream.is_some()
                    && self.sample_rate.0 == f.sample_rate
                    && u32::from(self.channels) == f.channels;
                let buffer = vec![0.; f.sample_rate as usize * f.channels as usize];
                #[cfg(not(target_os = "linux"))]
                let mut previous = std::mem::take(self);
                #[cfg(target_os = "windows")]
                self.prepare_playback(&f);
                self.audio_decoder = Some((d, buffer));
                self.channels = f.channels as _;
                let result = start(self, f);
                #[cfg(target_os = "windows")]
                let keep_existing_stream = keep_existing_stream
                    && !playback_failed
                    && !previous.playback_recovery.report_pending();
                #[cfg(not(target_os = "linux"))]
                if result.is_err() && keep_existing_stream {
                    // 即使输出启动失败，重新开始的采集也会带上新的 Opus 历史。
                    previous.audio_decoder = self.audio_decoder.take();
                    * self = previous;
                    self.handle_audio_start_result(result, true);
                    return;
                }
                #[cfg(target_os = "windows")]
                self.finish_playback_replacement(result, keep_existing_stream.then_some(previous));
                #[cfg(not(target_os = "windows"))]
                self.handle_audio_start_result(result, keep_existing_stream);
            }
            Err(err) => {
                log::error!("Failed to create audio decoder: {}", err);
            }
        }
    }

    fn handle_audio_start_result(&mut self, result: ResultType<()>, keep_existing_stream: bool) {
        if let Err(error) = result {
            if keep_existing_stream {
                log::error!(
                    "Failed to replace audio playback stream; keeping the existing compatible stream: {error:#}"
                );
            } else {
                * self = Self::default();
                log::error!("Failed to start audio playback: {error:#}");
            }
        }
    }

    /// 处理音频帧并播放。
    #[inline]
    pub fn handle_frame(&mut self, frame: AudioFrame) {
        #[cfg(not(target_os = "linux"))]
        self.playback_status.report_errors();
        #[cfg(not(target_os = "linux"))]
        if self.audio_stream.is_none()
            || !self
                .playback_status
                .ready
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        #[cfg(target_os = "linux")]
        if self.simple.is_none() {
            log::trace!("PulseAudio simple binding does not exists");
            return;
        }
        self.audio_decoder.as_mut().map(|(d, buffer)| {
            let decoded_frames = match d.decode_float(&frame.data, buffer, false) {
                Ok(decoded_frames) => decoded_frames,
                Err(error) => {
                    log::warn!("Failed to decode audio frame: {error:?}");
                    return;
                }
            };
            let channels = self.channels;
            let n = decoded_frames * channels as usize;
            #[cfg(not(target_os = "linux"))]
            {
                let config = DecodedAudioConfig {
                    sample_rate: self.sample_rate.1,
                    input_channels: self.channels,
                    output_channels: self.device_channel,
                };
                let buffer = match prepare_decoded_audio(
                    &buffer[0..n],
                    self.audio_resampler.as_mut(),
                    config,
                ) {
                    Ok(output) => output,
                    Err(error) => {
                        log::error!("Failed to resample decoded audio: {error:#}");
                        return;
                    }
                };
                self.audio_buffer.append_pcm(&buffer);
            }
            #[cfg(target_os = "linux")]
            {
                let data_u8 =
                    unsafe { std::slice::from_raw_parts::<u8>(buffer.as_ptr() as _, n * 4) };
                self.simple.as_mut().map(|x| x.write(data_u8));
            }
        });
    }

    /// 为当前设备构建音频输出流。
    #[cfg(not(target_os = "linux"))]
    fn build_output_stream<T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>>(
        &mut self,
        config: &StreamConfig,
        device: &Device,
    ) -> ResultType<()> {
        self.device_channel = config.channels;
        #[cfg(target_os = "windows")]
        let err_fn = self.playback_recovery.new_error_callback();
        #[cfg(not(target_os = "windows"))]
        let err_fn = move |err| {
            // 错误太多，以后再改进
            log::trace!("an error occurred on stream: {}", err);
        };
        self.audio_buffer
            .resize(config.sample_rate.0 as _, config.channels as _);
        let audio_buffer = self.audio_buffer.0.clone();
        let discontinuity_generation = self.audio_buffer.3.clone();
        let mut playback_writer = audio_playback::AudioPlaybackWriter::new(
            audio_playback::AudioPlaybackConfig {
                sample_rate: config.sample_rate.0,
                channels: config.channels as usize,
            },
            audio_buffer,
            discontinuity_generation,
        )?;
        let playback_status = playback_writer.status.clone();
        let timeout = None;
        let stream = device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                playback_writer.write_output(data);
            },
            err_fn,
            timeout,
        )?;
        stream.play()?;
        self.audio_stream = Some(Box::new(stream));
        self.playback_status = playback_status;
        Ok(())
    }
}

fn is_supported_audio_channel_count(channels: u32) -> bool {
    (1..=2).contains(&channels)
}

#[cfg(test)]
mod audio_format_tests {
    use super::is_supported_audio_channel_count;

    #[test]
    fn only_mono_and_stereo_are_supported() {
        assert!(is_supported_audio_channel_count(1));
        assert!(is_supported_audio_channel_count(2));
        assert!(!is_supported_audio_channel_count(0));
        assert!(!is_supported_audio_channel_count(u32::MAX));
    }

    #[test]
    fn failed_audio_start_discards_format_state() {
        use super::{anyhow, AudioDecoder, AudioHandler, Stereo};

        const SAMPLE_RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;
        let decoder = AudioDecoder::new(SAMPLE_RATE, Stereo).unwrap();
        let mut handler = AudioHandler {
            audio_decoder: Some((decoder, Vec::new())),
            sample_rate: (SAMPLE_RATE, SAMPLE_RATE),
            channels: CHANNELS,
            ..Default::default()
        };

        handler.handle_audio_start_result(Err(anyhow!("Injected playback startup failure")), false);

        assert!(handler.audio_decoder.is_none());
        assert_eq!(handler.channels, 0);
        assert_eq!(handler.sample_rate, (0, 0));
    }
}

/// [`Client`] 的视频处理器。
pub struct VideoHandler {
    decoder: Decoder,
    pub rgb: ImageRgb,
    pub texture: ImageTexture,
    recorder: Arc<Mutex<Option<Recorder>>>,
    record: bool,
    _display: usize, // useful for debug
    fail_counter: usize,
    first_frame: bool,
}

impl VideoHandler {
    #[cfg(feature = "flutter")]
    pub fn get_adapter_luid() -> Option<i64> {
        crate::flutter::get_adapter_luid()
    }

    #[cfg(not(feature = "flutter"))]
    pub fn get_adapter_luid() -> Option<i64> {
        None
    }

    /// 创建一个新的视频处理器。
    pub fn new(format: CodecFormat, _display: usize) -> Self {
        let luid = Self::get_adapter_luid();
        log::info!("new video handler for display #{_display}, format: {format:?}, luid: {luid:?}");
        let rgba_format =
            if cfg!(feature = "flutter") && (cfg!(windows) || cfg!(target_os = "linux")) {
                ImageFormat::ABGR
            } else {
                ImageFormat::ARGB
            };
        VideoHandler {
            decoder: Decoder::new(format, luid),
            rgb: ImageRgb::new(rgba_format, crate::get_dst_align_rgba()),
            texture: Default::default(),
            recorder: Default::default(),
            record: false,
            _display,
            fail_counter: 0,
            first_frame: true,
        }
    }

    /// 处理新的视频帧。
    #[inline]
    pub fn handle_frame(
        &mut self,
        vf: VideoFrame,
        pixelbuffer: &mut bool,
        chroma: &mut Option<Chroma>,
    ) -> ResultType<bool> {
        let format = CodecFormat::from(&vf);
        if format != self.decoder.format() {
            self.reset(Some(format));
        }
        match &vf.union {
            Some(frame) => {
                let res = self.decoder.handle_video_frame(
                    frame,
                    &mut self.rgb,
                    &mut self.texture,
                    pixelbuffer,
                    chroma,
                );
                if res.as_ref().is_ok_and(|x| *x) {
                    self.fail_counter = 0;
                } else {
                    if self.fail_counter < usize::MAX {
                        if self.first_frame && self.fail_counter < MAX_DECODE_FAIL_COUNTER {
                            log::error!("decode first frame failed");
                            self.fail_counter = MAX_DECODE_FAIL_COUNTER;
                        } else {
                            self.fail_counter += 1;
                        }
                        log::error!(
                            "Failed to handle video frame, fail counter: {}",
                            self.fail_counter
                        );
                    }
                }
                self.first_frame = false;
                if self.record {
                    self.recorder.lock().unwrap().as_mut().map(|r| {
                        let (w, h) = if *pixelbuffer {
                            (self.rgb.w, self.rgb.h)
                        } else {
                            (self.texture.w, self.texture.h)
                        };
                        r.write_frame(frame, w, h).ok();
                    });
                }
                res
            }
            _ => Ok(false),
        }
    }

    /// 重置解码器，若格式为 Some 则更换格式
    pub fn reset(&mut self, format: Option<CodecFormat>) {
        log::info!(
            "reset video handler for display #{}, format: {format:?}",
            self._display
        );
        #[cfg(target_os = "macos")]
        self.rgb.set_align(crate::get_dst_align_rgba());
        let luid = Self::get_adapter_luid();
        let format = format.unwrap_or(self.decoder.format());
        self.decoder = Decoder::new(format, luid);
        self.fail_counter = 0;
        self.first_frame = true;
    }

    /// 开始或停止屏幕录制。
    pub fn record_screen(&mut self, start: bool, id: String, display_idx: usize, camera: bool) {
        self.record = false;
        if start {
            self.recorder = Recorder::new(RecorderContext {
                server: false,
                id,
                dir: crate::ui_interface::video_save_directory(false),
                display_idx,
                camera,
                tx: None,
            })
            .map_or(Default::default(), |r| Arc::new(Mutex::new(Some(r))));
        } else {
            self.recorder = Default::default();
        }

        self.record = start;
    }
}

// 发送密码的来源
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PasswordSource {
    PersonalAb(Vec<u8>),
    SharedAb(String),
    Undefined,
}

impl Default for PasswordSource {
    fn default() -> Self {
        PasswordSource::Undefined
    }
}

impl PasswordSource {
    // 密码是否为个人 ab 密码
    pub fn is_personal_ab(&self, password: &[u8]) -> bool {
        if password.is_empty() {
            return false;
        }
        match self {
            PasswordSource::PersonalAb(p) => p == password,
            _ => false,
        }
    }

    // 密码是否为共享 ab 密码
    pub fn is_shared_ab(&self, password: &[u8], hash: &Hash) -> bool {
        if password.is_empty() {
            return false;
        }
        match self {
            PasswordSource::SharedAb(p) => Self::equal(p, password, hash),
            _ => false,
        }
    }

    //  密码是否与已连接的密码相同
    fn equal(password: &str, connected_password: &[u8], hash: &Hash) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(password);
        hasher.update(&hash.salt);
        let res = hasher.finalize();
        connected_password[..] == res[..]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConnToken {
    password: Vec<u8>,
    password_source: PasswordSource,
    session_id: u64,
}

/// [`Client`] 的登录配置处理器。
#[derive(Default)]
pub struct LoginConfigHandler {
    id: String,
    pub conn_type: ConnType,
    pub is_terminal_admin: bool,
    hash: Hash,
    password: Vec<u8>, // remember password for reconnect
    pub remember: bool,
    config: PeerConfig,
    pub port_forward: (String, i32),
    /// 本次登录的 `multiplex`，在 turn 锁之下由 `port_forward` 填充。
    /// `port_forward_mux` 表示某个映射是否需要探测隧道；
    /// 某个已被探测锁定的映射会直接登录而不询问，因此
    /// 升级后的对端仍会把它分到原始管道。
    pub(crate) port_forward_multiplex: bool,
    /// 每个窗口设置一次，在其映射启动之前：每次 accept 的认领
    /// 都会读取它。
    pub(crate) port_forward_mux: bool,
    /// 被某个端口转发映射持有，不让它填入 `port_forward` 和 `hash`，
    /// 直到用它构建出登录信息；一个窗口的各个映射是并发
    /// 登录的。
    pub(crate) port_forward_login_turn: Arc<hbb_common::tokio::sync::Mutex<()>>,
    pub version: i64,
    features: Option<Features>,
    pub session_id: u64, // used for local <-> server communication
    pub supported_encoding: SupportedEncoding,
    restarting_remote_device: bool,
    // 重启宽限期的起始时间。在 Windows 上，对端可能会短暂地
    // 在真正重启断线之前先重连。
    restart_remote_device_at: Option<Instant>,
    pub force_relay: bool,
    // force_relay 减去 WebSocket 传输方式。ws 会为传统打洞强制中继，但对
    // ICE 只字未提，所以所有 WebRTC 决策都以此为准：策略意味着 Relay-only ICE，
    // 而传输方式仍可能走直连。
    pub policy_relay: bool,
    // 其中属于对端作用域的部分，也是唯一可以写回给对端的部分。
    pub peer_relay: bool,
    pub direct: Option<bool>,
    pub received: bool,
    switch_uuid: Option<String>,
    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    switch_back_allowed: bool,
    pub save_ab_password_to_recent: bool, // true: connected with ab password
    pub other_server: Option<(String, String, String)>,
    pub custom_fps: Arc<Mutex<Option<usize>>>,
    pub last_auto_fps: Option<usize>,
    pub adapter_luid: Option<i64>,
    pub mark_unsupported: Vec<CodecFormat>,
    pub selected_windows_session_id: Option<u32>,
    pub peer_info: Option<PeerInfo>,
    password_source: PasswordSource, // where the sent password comes from
    shared_password: Option<String>, // Store the shared password
    pub enable_trusted_devices: bool,
    pub record_state: bool,
    pub record_permission: bool,
}

impl Deref for LoginConfigHandler {
    type Target = PeerConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl LoginConfigHandler {
    pub(crate) fn set_hash(&mut self, hash: Hash) {
        self.hash = hash;
    }

    /// 初始化登录配置处理器。
    ///
    /// # 参数
    ///
    /// * `id` - 对端的 id
    /// * `conn_type` - 连接类型枚举。
    pub fn initialize(
        &mut self,
        id: String,
        conn_type: ConnType,
        switch_uuid: Option<String>,
        mut force_relay: bool,
        adapter_luid: Option<i64>,
        shared_password: Option<String>,
        conn_token: Option<String>,
    ) {
        let mut id = id;
        if id.contains("@") {
            let mut v = id.split("@");
            let raw_id: &str = v.next().unwrap_or_default();
            let mut server_key = v.next().unwrap_or_default().split('?');
            let server = server_key.next().unwrap_or_default();
            let args = server_key.next().unwrap_or_default();
            let key = if server == PUBLIC_SERVER {
                config::RS_PUB_KEY.to_owned()
            } else {
                let mut args_map: HashMap<String, &str> = HashMap::new();
                for arg in args.split('&') {
                    if let Some(kv) = arg.find('=') {
                        let k = arg[0..kv].to_lowercase();
                        let v = &arg[kv + 1..];
                        args_map.insert(k, v);
                    }
                }
                let key = args_map.remove("key").unwrap_or_default();
                key.to_owned()
            };

            // 在这里可以检查 <id>/r@server
            let real_id = crate::ui_interface::handle_relay_id(raw_id).to_string();
            if real_id != raw_id {
                force_relay = true;
            }
            self.other_server = Some((real_id.clone(), server.to_owned(), key));
            id = format!("{real_id}@{server}");
        } else {
            let real_id = crate::ui_interface::handle_relay_id(&id);
            if real_id != id {
                force_relay = true;
                id = real_id.to_owned();
            }
        }

        self.id = id;
        self.conn_type = conn_type;
        let config = self.load_config();
        self.remember = !config.password.is_empty();
        self.config = config;

        let conn_token = conn_token
            .map(|x| serde_json::from_str::<ConnToken>(&x).ok())
            .flatten();
        let mut sid = 0;
        if let Some(token) = conn_token {
            sid = token.session_id;
            self.password = token.password; // use as last password
            self.password_source = token.password_source;
        }
        if sid == 0 {
            sid = rand::random();
            if sid == 0 {
                // 你中大奖了
                sid = 1;
            }
        }
        self.session_id = sid;
        self.supported_encoding = Default::default();
        self.clear_restarting_remote_device();
        // 三个作用域：关于这个对端确定了什么、这个客户端被配置成什么（代理）、
        // 以及它的传输方式强制了什么（ws）。只有第一个可以写回对端的
        // 配置——持久化其余两个会把本地设置变成对端的永久属性。
        self.peer_relay =
            config::option2bool("force-always-relay", &self.get_option("force-always-relay"))
                || force_relay;
        self.policy_relay = self.peer_relay || Config::is_proxy();
        self.force_relay = self.policy_relay || use_ws();
        if let Some((real_id, server, key)) = &self.other_server {
            let other_server_key = self.get_option("other-server-key");
            if !other_server_key.is_empty() && key.is_empty() {
                self.other_server = Some((real_id.to_owned(), server.to_owned(), other_server_key));
            }
        }

        self.direct = None;
        self.received = false;
        #[cfg(feature = "flutter")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            self.switch_back_allowed = false;
        }
        self.switch_uuid = switch_uuid;
        self.adapter_luid = adapter_luid;
        self.selected_windows_session_id = None;
        self.shared_password = shared_password;
        self.record_state = false;
        self.record_permission = true;

        // `std::env::remove_var("IS_TERMINAL_ADMIN");` 在 `session_add_sync()` 中调用 —— `flutter_ffi.rs`。
        let is_terminal_admin = conn_type == ConnType::TERMINAL
            && std::env::var("IS_TERMINAL_ADMIN").map_or(false, |v| v == "Y");
        self.is_terminal_admin = is_terminal_admin;
    }

    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn allow_switch_back_once(&mut self) {
        self.switch_back_allowed = true;
    }

    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn consume_switch_back_permission(&mut self) -> bool {
        if self.switch_back_allowed {
            self.switch_back_allowed = false;
            true
        } else {
            false
        }
    }

    /// 检查客户端是否应该自动登录。
    /// 如果客户端应自动登录则返回密码，否则返回空字符串。
    pub fn should_auto_login(&self) -> String {
        let l = self.lock_after_session_end.v;
        let a = !self.get_option("auto-login").is_empty();
        let p = self.get_option("os-password");
        if !p.is_empty() && l && a {
            p
        } else {
            "".to_owned()
        }
    }

    /// 加载 [`PeerConfig`]。
    pub fn load_config(&self) -> PeerConfig {
        debug_assert!(self.id.len() > 0);
        PeerConfig::load(&self.id)
    }

    /// 把 [`PeerConfig`] 保存到处理器中。
    ///
    /// # 参数
    ///
    /// * `config` - 要保存的 [`PeerConfig`]。
    pub fn save_config(&mut self, config: PeerConfig) {
        config.store(&self.id);
        self.config = config;
    }

    /// 为处理器的 [`PeerConfig`] 设置一个选项。
    ///
    /// # 参数
    ///
    /// * `k` - 选项的键
    /// * `v` - 选项的值
    pub fn set_option(&mut self, k: String, v: String) {
        let mut config = self.load_config();
        if v == self.get_option(&k) {
            return;
        }
        config.options.insert(k, v);
        self.save_config(config);
    }

    // 待办：下面有太多重复代码。

    /// 把视图样式保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - 要保存的视图样式。
    pub fn save_view_style(&mut self, value: String) {
        let mut config = self.load_config();
        config.view_style = value;
        self.save_config(config);
    }

    /// 把键盘模式保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - 要保存的视图样式。
    pub fn save_keyboard_mode(&mut self, value: String) {
        let mut config = self.load_config();
        config.keyboard_mode = value;
        self.save_config(config);
    }

    /// 把反向鼠标滚轮（"", "Y"）保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - 反向鼠标滚轮（"", "Y"）。
    pub fn save_reverse_mouse_wheel(&mut self, value: String) {
        let mut config = self.load_config();
        config.reverse_mouse_wheel = value;
        self.save_config(config);
    }

    /// 把 "displays_as_individual_windows"（"", "Y"）保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - "displays_as_individual_windows" 的值（"", "Y"）。
    pub fn save_displays_as_individual_windows(&mut self, value: String) {
        let mut config = self.load_config();
        config.displays_as_individual_windows = value;
        self.save_config(config);
    }

    /// 把 "use_all_my_displays_for_the_remote_session"（"", "Y"）保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - "use_all_my_displays_for_the_remote_session" 的值（"", "Y"）。
    pub fn save_use_all_my_displays_for_the_remote_session(&mut self, value: String) {
        let mut config = self.load_config();
        config.use_all_my_displays_for_the_remote_session = value;
        self.save_config(config);
    }

    /// 把滚动样式保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - 要保存的滚动样式。
    pub fn save_scroll_style(&mut self, value: String) {
        let mut config = self.load_config();
        config.scroll_style = value;
        self.save_config(config);
    }

    /// 把边缘滚动的边缘厚度保存到当前配置。
    ///
    /// # 参数
    ///
    /// * `value` - 要保存的边缘厚度。
    pub fn save_edge_scroll_edge_thickness(&mut self, value: i32) {
        let mut config = self.load_config();
        config.edge_scroll_edge_thickness = value;
        self.save_config(config);
    }

    /// 为处理器的 [`PeerConfig`] 设置一项 flutter 的 ui 配置。
    ///
    /// # 参数
    ///
    /// * `k` - 选项的键
    /// * `v` - 选项的值
    pub fn save_ui_flutter(&mut self, k: String, v: String) {
        let mut config = self.load_config();
        if v.is_empty() {
            config.ui_flutter.remove(&k);
        } else {
            config.ui_flutter.insert(k, v);
        }
        self.save_config(config);
    }

    pub fn set_direct_failure(&mut self, value: i32) {
        let mut config = self.load_config();
        config.direct_failures = value;
        self.save_config(config);
    }

    /// 获取处理器的 [`PeerConfig`] 的某项 flutter ui 配置。
    /// 找到该选项时返回 String，否则返回 ""。
    ///
    /// # 参数
    ///
    /// * `k` - 选项的键
    pub fn get_ui_flutter(&self, k: &str) -> String {
        if let Some(v) = self.config.ui_flutter.get(k) {
            v.clone()
        } else {
            "".to_owned()
        }
    }

    /// 切换处理器中的某个选项。
    ///
    /// # 参数
    ///
    /// * `name` - 要切换的选项名。
    ///
    // 在这个函数里检查选项是否为空是可以的。
    // `toggle_option()` 只在会话中调用。
    // 客户端自定义高级设置不会影响这个函数。
    pub fn toggle_option(&mut self, name: String) -> Option<Message> {
        let mut option = OptionMessage::default();
        let mut config = self.load_config();
        if name == "show-remote-cursor" {
            config.show_remote_cursor.v = !config.show_remote_cursor.v;
            option.show_remote_cursor = (if config.show_remote_cursor.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "follow-remote-cursor" {
            config.follow_remote_cursor.v = !config.follow_remote_cursor.v;
            option.follow_remote_cursor = (if config.follow_remote_cursor.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "follow-remote-window" {
            config.follow_remote_window.v = !config.follow_remote_window.v;
            option.follow_remote_window = (if config.follow_remote_window.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "disable-audio" {
            config.disable_audio.v = !config.disable_audio.v;
            option.disable_audio = (if config.disable_audio.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "disable-clipboard" {
            config.disable_clipboard.v = !config.disable_clipboard.v;
            option.disable_clipboard = (if config.disable_clipboard.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "lock-after-session-end" {
            config.lock_after_session_end.v = !config.lock_after_session_end.v;
            option.lock_after_session_end = (if config.lock_after_session_end.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == keys::OPTION_TERMINAL_PERSISTENT {
            config.terminal_persistent.v = !config.terminal_persistent.v;
            option.terminal_persistent = (if config.terminal_persistent.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "privacy-mode" {
            // 尝试切换隐私模式
            option.privacy_mode = (if config.privacy_mode.v {
                BoolOption::No
            } else {
                BoolOption::Yes
            })
            .into();
        } else if name == "enable-file-copy-paste" {
            config.enable_file_copy_paste.v = !config.enable_file_copy_paste.v;
            option.enable_file_transfer = (if config.enable_file_copy_paste.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            })
            .into();
        } else if name == "block-input" {
            option.block_input = BoolOption::Yes.into();
        } else if name == "unblock-input" {
            option.block_input = BoolOption::No.into();
        } else if name == "show-quality-monitor" {
            config.show_quality_monitor.v = !config.show_quality_monitor.v;
        } else if name == "allow_swap_key" {
            config.allow_swap_key.v = !config.allow_swap_key.v;
        } else if name == "view-only" {
            config.view_only.v = !config.view_only.v;
            let f = |b: bool| {
                if b {
                    BoolOption::Yes.into()
                } else {
                    BoolOption::No.into()
                }
            };
            if config.view_only.v {
                option.disable_keyboard = f(true);
                option.disable_clipboard = f(true);
                option.show_remote_cursor = f(true);
                option.enable_file_transfer = f(false);
                option.lock_after_session_end = f(false);
            } else {
                option.disable_keyboard = f(false);
                option.disable_clipboard = f(self.get_toggle_option("disable-clipboard"));
                option.show_remote_cursor = f(self.get_toggle_option("show-remote-cursor"));
                option.enable_file_transfer = f(self.config.enable_file_copy_paste.v);
                option.lock_after_session_end = f(self.config.lock_after_session_end.v);
                if config.show_my_cursor.v {
                    config.show_my_cursor.v = false;
                    option.show_my_cursor = BoolOption::No.into();
                }
            }
        } else if name == "show-my-cursor" {
            config.show_my_cursor.v = !config.show_my_cursor.v;
            option.show_my_cursor = if config.show_my_cursor.v {
                BoolOption::Yes
            } else {
                BoolOption::No
            }
            .into();
        } else {
            let is_set = self
                .options
                .get(&name)
                .map(|o| !o.is_empty())
                .unwrap_or(false);
            if is_set {
                self.config.options.remove(&name);
            } else {
                self.config.options.insert(name, "Y".to_owned());
            }
            self.config.store(&self.id);
            return None;
        }

        #[cfg(feature = "unix-file-copy-paste")]
        if option.enable_file_transfer.enum_value() == Ok(BoolOption::No) {
            crate::clipboard::try_empty_clipboard_files(crate::clipboard::ClipboardSide::Client, 0);
        }

        if !name.contains("block-input") {
            self.save_config(config);
        }
        let mut misc = Misc::new();
        misc.set_option(option);
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        Some(msg_out)
    }

    /// 获取当前 [`LoginConfigHandler`] 的 [`PeerConfig`]。
    ///
    /// # 参数
    pub fn get_config(&mut self) -> &mut PeerConfig {
        &mut self.config
    }

    /// 获取当前 [`LoginConfigHandler`] 的 [`OptionMessage`]。
    /// 没有选项时返回 `None`，例如会话仅用于文件传输时。
    ///
    /// # 参数
    ///
    /// * `ignore_default` - 若为 `true`，忽略该选项的默认值。
    fn get_option_message(&self, ignore_default: bool) -> Option<OptionMessage> {
        if self.conn_type.eq(&ConnType::PORT_FORWARD)
            || self.conn_type.eq(&ConnType::RDP)
            || self.conn_type.eq(&ConnType::FILE_TRANSFER)
        {
            return None;
        }
        let mut msg = OptionMessage::new();
        if self.conn_type.eq(&ConnType::TERMINAL) {
            if self.get_toggle_option(keys::OPTION_TERMINAL_PERSISTENT) {
                msg.terminal_persistent = BoolOption::Yes.into();
                return Some(msg);
            } else {
                return None;
            }
        }
        let q = self.image_quality.clone();
        if let Some(q) = self.get_image_quality_enum(&q, ignore_default) {
            msg.image_quality = q.into();
        } else if q == "custom" {
            let config = self.load_config();
            let allow_more = !crate::using_public_server() || self.direct == Some(true);
            let quality = if config.custom_image_quality.is_empty() {
                50
            } else {
                let mut quality = config.custom_image_quality[0];
                if !allow_more && quality > 100 {
                    quality = 50;
                }
                quality
            };
            msg.custom_image_quality = quality << 8;
            #[cfg(feature = "flutter")]
            if let Some(custom_fps) = self.options.get("custom-fps") {
                let mut custom_fps = custom_fps.parse().unwrap_or(30);
                if !allow_more && custom_fps > 30 {
                    custom_fps = 30;
                }
                msg.custom_fps = custom_fps;
                * self.custom_fps.lock().unwrap() = Some(custom_fps as _);
            }
        }
        let view_only = self.get_toggle_option("view-only");
        if view_only {
            msg.disable_keyboard = BoolOption::Yes.into();
        }
        if view_only || self.get_toggle_option("show-remote-cursor") {
            msg.show_remote_cursor = BoolOption::Yes.into();
        }
        if view_only && self.get_toggle_option("show-my-cursor") {
            msg.show_my_cursor = BoolOption::Yes.into();
        }
        if self.get_toggle_option("follow-remote-cursor") {
            msg.follow_remote_cursor = BoolOption::Yes.into();
        }
        if self.get_toggle_option("follow-remote-window") {
            msg.follow_remote_window = BoolOption::Yes.into();
        }
        if !view_only && self.get_toggle_option("lock-after-session-end") {
            msg.lock_after_session_end = BoolOption::Yes.into();
        }
        if self.get_toggle_option("disable-audio") {
            msg.disable_audio = BoolOption::Yes.into();
        }
        if !view_only && self.get_toggle_option(keys::OPTION_ENABLE_FILE_COPY_PASTE) {
            msg.enable_file_transfer = BoolOption::Yes.into();
        }
        if view_only || self.get_toggle_option("disable-clipboard") {
            msg.disable_clipboard = BoolOption::Yes.into();
        }
        msg.supported_decoding = MessageField::some(self.get_supported_decoding());
        Some(msg)
    }

    pub fn get_supported_decoding(&self) -> SupportedDecoding {
        Decoder::supported_decodings(
            Some(&self.id),
            use_texture_render(),
            self.adapter_luid,
            &self.mark_unsupported,
        )
    }

    /// 解析图像质量选项。
    /// 选项有效时返回 [`ImageQuality`]，否则返回 `None`。
    ///
    /// # 参数
    ///
    /// * `q` - 图像质量选项。
    /// * `ignore_default` - 忽略默认值。
    fn get_image_quality_enum(&self, q: &str, ignore_default: bool) -> Option<ImageQuality> {
        if q == "low" {
            Some(ImageQuality::Low)
        } else if q == "best" {
            Some(ImageQuality::Best)
        } else if q == "balanced" {
            if ignore_default {
                None
            } else {
                Some(ImageQuality::Balanced)
            }
        } else {
            None
        }
    }

    /// 获取某个开关选项的状态。
    ///
    /// # 参数
    ///
    /// * `name` - 开关选项的名称。
    ///
    // 在这个函数里检查选项是否为空是可以的。
    // `get_toggle_option()` 只在会话中调用。
    // 客户端自定义高级设置不会影响这个函数。
    pub fn get_toggle_option(&self, name: &str) -> bool {
        if name == "show-remote-cursor" {
            self.config.show_remote_cursor.v
        } else if name == "lock-after-session-end" {
            self.config.lock_after_session_end.v
        } else if name == keys::OPTION_TERMINAL_PERSISTENT {
            self.config.terminal_persistent.v
        } else if name == "privacy-mode" {
            self.config.privacy_mode.v
        } else if name == keys::OPTION_ENABLE_FILE_COPY_PASTE {
            self.config.enable_file_copy_paste.v
        } else if name == "disable-audio" {
            self.config.disable_audio.v
        } else if name == "disable-clipboard" {
            self.config.disable_clipboard.v
        } else if name == "show-quality-monitor" {
            self.config.show_quality_monitor.v
        } else if name == "allow_swap_key" {
            self.config.allow_swap_key.v
        } else if name == "view-only" {
            self.config.view_only.v
        } else if name == "show-my-cursor" {
            self.config.show_my_cursor.v
        } else if name == "follow-remote-cursor" {
            self.config.follow_remote_cursor.v
        } else if name == "follow-remote-window" {
            self.config.follow_remote_window.v
        } else {
            !self.get_option(name).is_empty()
        }
    }

    pub fn is_privacy_mode_supported(&self) -> bool {
        if let Some(features) = &self.features {
            features.privacy_mode
        } else {
            false
        }
    }

    /// 创建一个用于刷新视频的 [`Message`]。
    pub fn refresh() -> Message {
        let mut misc = Misc::new();
        misc.set_refresh_video(true);
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        msg_out
    }

    /// 创建一个用于刷新视频的 [`Message`]。
    pub fn refresh_display(display: usize) -> Message {
        let mut misc = Misc::new();
        misc.set_refresh_video_display(display as _);
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        msg_out
    }

    /// 创建一个用于保存自定义图像质量的 [`Message`]。
    ///
    /// # 参数
    ///
    /// * `bitrate` - 给定的码率。
    /// * `quantizer` - 给定的量化参数。
    pub fn save_custom_image_quality(&mut self, image_quality: i32) -> Message {
        let mut misc = Misc::new();
        misc.set_option(OptionMessage {
            custom_image_quality: image_quality << 8,
            ..Default::default()
        });
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        let mut config = self.load_config();
        config.image_quality = "custom".to_owned();
        config.custom_image_quality = vec![image_quality as _];
        self.save_config(config);
        msg_out
    }

    /// 把给定的图像质量保存到配置。
    /// 返回一个包含图像质量的 [`Message`]，若图像质量无效则返回 `None`。
    /// # 参数
    ///
    /// * `value` - 图像质量。
    pub fn save_image_quality(&mut self, value: String) -> Option<Message> {
        let mut res = None;
        if let Some(q) = self.get_image_quality_enum(&value, false) {
            let mut misc = Misc::new();
            misc.set_option(OptionMessage {
                image_quality: q.into(),
                ..Default::default()
            });
            let mut msg_out = Message::new();
            msg_out.set_misc(misc);
            res = Some(msg_out);
        }
        let mut config = self.load_config();
        config.image_quality = value;
        self.save_config(config);
        res
    }

    pub fn save_trackpad_speed(&mut self, speed: i32) {
        let mut config = self.load_config();
        config.trackpad_speed = speed;
        self.save_config(config);
    }

    /// 创建一个用于保存自定义 fps 的 [`Message`]。
    ///
    /// # 参数
    ///
    /// * `fps` - 给定的 fps。
    /// * `save_config` - 保存配置。
    pub fn set_custom_fps(&mut self, fps: i32, save_config: bool) -> Message {
        let mut misc = Misc::new();
        misc.set_option(OptionMessage {
            custom_fps: fps,
            ..Default::default()
        });
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        if save_config {
            let mut config = self.load_config();
            config
                .options
                .insert("custom-fps".to_owned(), fps.to_string());
            self.save_config(config);
        }
        * self.custom_fps.lock().unwrap() = Some(fps as _);
        msg_out
    }

    pub fn get_option(&self, k: &str) -> String {
        if let Some(v) = self.config.options.get(k) {
            v.clone()
        } else {
            "".to_owned()
        }
    }

    #[inline]
    pub fn get_custom_resolution(&self, display: i32) -> Option<(i32, i32)> {
        self.config
            .custom_resolutions
            .get(&display.to_string())
            .map(|r| (r.w, r.h))
    }

    #[inline]
    pub fn set_custom_resolution(&mut self, display: i32, wh: Option<(i32, i32)>) {
        let display = display.to_string();
        let mut config = self.load_config();
        match wh {
            Some((w, h)) => {
                config
                    .custom_resolutions
                    .insert(display, Resolution { w, h });
            }
            None => {
                config.custom_resolutions.remove(&display);
            }
        }
        self.save_config(config);
    }

    /// 获取用户名。
    /// 返回给定对端的名称。如果该对端没有名称，则返回配置中的名称。
    ///
    /// # 参数
    ///
    /// * `pi` - 对端信息。
    pub fn get_username(&self, pi: &PeerInfo) -> String {
        return if pi.username.is_empty() {
            self.info.username.clone()
        } else {
            pi.username.clone()
        };
    }

    /// 处理对端信息。
    ///
    /// # 参数
    ///
    /// * `username` - 对端的名称。
    /// * `pi` - 对端信息。
    pub fn handle_peer_info(&mut self, pi: &PeerInfo) {
        if !pi.version.is_empty() {
            self.version = hbb_common::get_version_number(&pi.version);
        }
        self.features = pi.features.clone().into_option();
        let serde = PeerInfoSerde {
            username: pi.username.clone(),
            hostname: pi.hostname.clone(),
            platform: pi.platform.clone(),
        };
        let mut config = self.load_config();
        config.info = serde;
        let password = self.password.clone();
        let password0 = config.password.clone();
        let remember = self.remember;
        let hash = self.hash.clone();
        if remember {
            // remember 为 true：使用 PeerConfig 中的密码或走 ui 登录
            // 不把共享密码同步到 recent
            if !password.is_empty()
                && password != password0
                && !self.password_source.is_shared_ab(&password, &hash)
            {
                config.password = password.clone();
                log::debug!("remember password of {}", self.id);
            }
        } else {
            if self.password_source.is_personal_ab(&password) {
                // 自动把个人 ab 密码同步到 recent
                config.password = password.clone();
                log::debug!("save ab password of {} to recent", self.id);
            } else if !password0.is_empty() {
                config.password = Default::default();
                log::debug!("remove password of {}", self.id);
            }
        }
        if let Some((_, b, c)) = self.other_server.as_ref() {
            if b != PUBLIC_SERVER {
                config
                    .options
                    .insert("other-server-key".to_owned(), c.clone());
            }
        }
        // 仅 peer_relay —— 参见 `initialize`：代理和 WebSocket 传输方式都不是
        // 关于这个对端的事实，在这里写入就会变成永久性的。
        if self.peer_relay {
            config
                .options
                .insert("force-always-relay".to_owned(), "Y".to_owned());
        }
        #[cfg(feature = "flutter")]
        {
            // 如果连接用的密码不是共享密码，就自动同步到个人 ab
            if !config.password.is_empty()
                && !self.password_source.is_shared_ab(&password, &hash)
                && !self.password_source.is_personal_ab(&password)
            {
                let hash = base64::encode(config.password.clone(), base64::Variant::Original);
                let evt: HashMap<&str, String> = HashMap::from([
                    ("name", "sync_peer_hash_password_to_personal_ab".to_string()),
                    ("id", self.id.clone()),
                    ("hash", hash),
                ]);
                let evt = serde_json::ser::to_string(&evt).unwrap_or("".to_owned());
                crate::flutter::push_global_event(crate::flutter::APP_TYPE_MAIN, evt);
            }
        }
        if config.keyboard_mode.is_empty() {
            if is_keyboard_mode_supported(
                &KeyboardMode::Map,
                get_version_number(&pi.version),
                &pi.platform,
            ) {
                config.keyboard_mode = KeyboardMode::Map.to_string();
            } else {
                config.keyboard_mode = KeyboardMode::Legacy.to_string();
            }
        } else {
            let keyboard_modes =
                crate::get_supported_keyboard_modes(get_version_number(&pi.version), &pi.platform);
            let current_mode = &KeyboardMode::from_str(&config.keyboard_mode).unwrap_or_default();
            if !keyboard_modes.contains(current_mode) {
                config.keyboard_mode = KeyboardMode::Legacy.to_string();
            }
        }
        // 无论是否发生变化，都是为了更新文件时间
        self.save_config(config);
        self.supported_encoding = pi.encoding.clone().unwrap_or_default();
        log::info!("peer info supported_encoding:{:?}", self.supported_encoding);
    }

    pub fn get_remote_dir(&self) -> String {
        serde_json::from_str::<HashMap<String, String>>(&self.get_option("remote_dir"))
            .unwrap_or_default()
            .remove(&self.info.username)
            .unwrap_or_default()
    }

    pub fn get_all_remote_dir(&self, path: String) -> String {
        let d = self.get_option("remote_dir");
        let user = self.info.username.clone();
        let mut x = serde_json::from_str::<HashMap<String, String>>(&d).unwrap_or_default();
        if path.is_empty() {
            x.remove(&user);
        } else {
            x.insert(user, path);
        }
        serde_json::to_string::<HashMap<String, String>>(&x).unwrap_or_default()
    }

    /// 创建一个用于登录的 [`Message`]。
    fn create_login_msg(
        &self,
        os_username: String,
        os_password: String,
        password: Vec<u8>,
    ) -> Message {
        let my_id = Config::get_id();
        let (my_id, pure_id) = if let Some((id, _, _)) = self.other_server.as_ref() {
            let server = Config::get_rendezvous_server();
            (format!("{my_id}@{server}"), id.clone())
        } else {
            (my_id, self.id.clone())
        };
        let mut avatar = get_builtin_option(keys::OPTION_AVATAR);
        if avatar.is_empty() {
            avatar = serde_json::from_str::<serde_json::Value>(&LocalConfig::get_option(
                "user_info",
            ))
            .ok()
            .and_then(|x| {
                x.get("avatar")
                    .and_then(|x| x.as_str())
                    .map(|x| x.trim().to_owned())
            })
            .unwrap_or_default();
        }
        avatar = resolve_avatar_url(avatar);
        let mut display_name = get_builtin_option(keys::OPTION_DISPLAY_NAME);
        if display_name.is_empty() {
            display_name =
                serde_json::from_str::<serde_json::Value>(&LocalConfig::get_option("user_info"))
                    .map(|x| {
                        x.get("display_name")
                            .and_then(|x| x.as_str())
                            .map(|x| x.trim())
                            .filter(|x| !x.is_empty())
                            .or_else(|| x.get("name").and_then(|x| x.as_str()))
                            .map(|x| x.to_owned())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
        }
        if display_name.is_empty() {
            display_name = crate::username();
        }
        let display_name = display_name
            .split_whitespace()
            .map(|word| {
                word.chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if i == 0 {
                            c.to_uppercase().to_string()
                        } else {
                            c.to_string()
                        }
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        #[cfg(not(target_os = "android"))]
        let my_platform = hbb_common::whoami::platform().to_string();
        #[cfg(target_os = "android")]
        let my_platform = "Android".into();
        let hwid = if self.get_option("trust-this-device") == "Y" {
            crate::get_hwid()
        } else {
            Bytes::new()
        };
        let os_login: MessageField<OSLogin> = if self.conn_type == ConnType::TERMINAL {
            Some(OSLogin {
                username: os_username,
                password: os_password,
                ..Default::default()
            })
            .into()
        } else {
            Default::default()
        };
        let mut lr = LoginRequest {
            username: pure_id,
            password: password.into(),
            my_id,
            my_name: display_name,
            my_platform,
            option: self.get_option_message(true).into(),
            session_id: self.session_id,
            version: crate::VERSION.to_string(),
            os_login,
            hwid,
            avatar,
            ..Default::default()
        };
        match self.conn_type {
            ConnType::FILE_TRANSFER => lr.set_file_transfer(FileTransfer {
                dir: self.get_remote_dir(),
                show_hidden: !self.get_option("remote_show_hidden").is_empty(),
                ..Default::default()
            }),
            ConnType::VIEW_CAMERA => lr.set_view_camera(Default::default()),
            ConnType::PORT_FORWARD | ConnType::RDP => lr.set_port_forward(PortForward {
                host: self.port_forward.0.clone(),
                port: self.port_forward.1,
                multiplex: self.port_forward_multiplex,
                ..Default::default()
            }),
            ConnType::TERMINAL => {
                let mut terminal = Terminal::new();
                terminal.service_id = self.get_option(self.get_key_terminal_service_id());
                lr.set_terminal(terminal);
            }
            _ => {}
        }

        let mut msg_out = Message::new();
        msg_out.set_login_request(lr);
        msg_out
    }

    pub fn update_supported_decodings(&self) -> Message {
        let decoding = scrap::codec::Decoder::supported_decodings(
            Some(&self.id),
            use_texture_render(),
            self.adapter_luid,
            &self.mark_unsupported,
        );
        let mut misc = Misc::new();
        misc.set_option(OptionMessage {
            supported_decoding: hbb_common::protobuf::MessageField::some(decoding),
            ..Default::default()
        });
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        msg_out
    }

    pub fn restart_remote_device(&self) -> Message {
        let mut misc = Misc::new();
        misc.set_restart_remote_device(true);
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        msg_out
    }

    pub fn mark_restarting_remote_device(&mut self) {
        self.restarting_remote_device = true;
        self.restart_remote_device_at = Some(Instant::now());
    }

    pub fn clear_restarting_remote_device(&mut self) {
        self.restarting_remote_device = false;
        self.restart_remote_device_at = None;
    }

    pub fn is_restarting_remote_device(&self) -> bool {
        if !self.restarting_remote_device {
            return false;
        }
        // 让这个标志保持一小段宽限期，而不是在
        // connection_ready 或对端的首批字节到达时就清除它。在操作系统重启期间，对端可能会
        // 在真正重启断线之前短暂重连，而过早清除它
        // 会让下一次断线逃出重启流程，退回到
        // 常规的错误对话框 / 手动重连路径。
        self.restart_remote_device_at
            .map(|started_at| started_at.elapsed() < RESTART_REMOTE_DEVICE_GRACE)
            .unwrap_or(false)
    }

    pub fn get_conn_token(&self) -> Option<String> {
        if self.password.is_empty() {
            return None;
        }
        serde_json::to_string(&ConnToken {
            password: self.password.clone(),
            password_source: self.password_source.clone(),
            session_id: self.session_id,
        })
        .ok()
    }

    pub fn get_id(&self) -> &str {
        &self.id
    }

    pub fn get_key_terminal_service_id(&self) -> &'static str {
        if self.is_terminal_admin {
            "terminal-admin-service-id"
        } else {
            "terminal-service-id"
        }
    }
}

/// 媒体数据。
pub enum MediaData {
    VideoQueue,
    VideoFrame(Box<VideoFrame>),
    AudioFrame(Box<AudioFrame>),
    AudioFormat(AudioFormat),
    Reset,
    RecordScreen(bool),
}

pub type MediaSender = mpsc::Sender<MediaData>;

/// 启动视频线程。
///
/// # 参数
///
/// * `video_callback` - 视频帧的回调。视频帧就绪时会被调用。
pub fn start_video_thread<F, T>(
    session: Session<T>,
    display: usize,
    video_receiver: mpsc::Receiver<MediaData>,
    video_queue: Arc<RwLock<ArrayQueue<VideoFrame>>>,
    fps: Arc<RwLock<Option<usize>>>,
    chroma: Arc<RwLock<Option<Chroma>>>,
    discard_queue: Arc<RwLock<bool>>,
    video_callback: F,
) where
    F: 'static + FnMut(usize, &mut scrap::ImageRgb, *mut c_void, bool) + Send,
    T: InvokeUiSession,
{
    let mut video_callback = video_callback;
    let mut last_chroma = None;
    let is_view_camera = session.is_view_camera();

    std::thread::spawn(move || {
        #[cfg(windows)]
        sync_cpu_usage();
        get_hwcodec_config();
        let mut video_handler = None;
        let mut count = 0;
        let mut duration = std::time::Duration::ZERO;
        let mut skip_beginning = 0;
        loop {
            if let Ok(data) = video_receiver.recv() {
                match data {
                    MediaData::VideoFrame(_) | MediaData::VideoQueue => {
                        let vf = match data {
                            MediaData::VideoFrame(vf) => {
                                * discard_queue.write().unwrap() = false;
                                *vf
                            }
                            MediaData::VideoQueue => {
                                if let Some(vf) = video_queue.read().unwrap().pop() {
                                    if discard_queue.read().unwrap().clone() {
                                        continue;
                                    }
                                    vf
                                } else {
                                    continue;
                                }
                            }
                            _ => {
                                // unreachable!();
                                continue;
                            }
                        };
                        let display = vf.display as usize;
                        let start = std::time::Instant::now();
                        let format = CodecFormat::from(&vf);
                        if video_handler.is_none() {
                            let mut handler = VideoHandler::new(format, display);
                            let record_state = session.lc.read().unwrap().record_state;
                            let record_permission = session.lc.read().unwrap().record_permission;
                            let id = session.lc.read().unwrap().id.clone();
                            if record_state && record_permission {
                                handler.record_screen(true, id, display, is_view_camera);
                            }
                            video_handler = Some(handler);
                        }
                        if let Some(handler) = video_handler.as_mut() {
                            let mut pixelbuffer = true;
                            let mut tmp_chroma = None;
                            let format_changed = handler.decoder.format() != format;
                            match handler.handle_frame(vf, &mut pixelbuffer, &mut tmp_chroma) {
                                Ok(true) => {
                                    video_callback(
                                        display,
                                        &mut handler.rgb,
                                        handler.texture.texture,
                                        pixelbuffer,
                                    );

                                    // chroma
                                    if tmp_chroma.is_some() && last_chroma != tmp_chroma {
                                        last_chroma = tmp_chroma;
                                        * chroma.write().unwrap() = tmp_chroma;
                                    }

                                    // fps 计算
                                    fps_calculate(
                                        &mut skip_beginning,
                                        &fps,
                                        format_changed,
                                        start.elapsed(),
                                        &mut count,
                                        &mut duration,
                                    );
                                }
                                Err(e) => {
                                    // 这是一个简单的变通方案。
                                    //
                                    // 我只看到下面这个错误：
                                    // FailedCall("errcode=1 scrap::common::vpxcodec:libs\\scrap\\src\\common\\vpxcodec.rs:433:9")
                                    // 当从所有显示器切换到单台显示器时，会出现该错误。
                                    // eg:
                                    // 1. 连接到一台有两个显示器（A 和 B）的设备。
                                    // 2. 切换到显示器 A。错误出现。
                                    // 3. 如果错误没有出现，从 A 切换到显示器 B。错误出现。
                                    //
                                    // 待办：修复这个错误
                                    log::error!("handle video frame error, {}", e);
                                    session.refresh_video(display as _);
                                }
                                _ => {}
                            }
                        }

                        // 检查无效的解码器
                        let mut should_update_supported = false;
                        if let Some(handler) = video_handler.as_mut() {
                            if !handler.decoder.valid()
                                || handler.fail_counter >= MAX_DECODE_FAIL_COUNTER
                            {
                                let mut lc = session.lc.write().unwrap();
                                let format = handler.decoder.format();
                                if !lc.mark_unsupported.contains(&format) {
                                    lc.mark_unsupported.push(format);
                                    should_update_supported = true;
                                    log::info!("mark {format:?} decoder as unsupported, valid:{}, fail_counter:{}, all unsupported:{:?}", handler.decoder.valid(), handler.fail_counter, lc.mark_unsupported);
                                }
                            }
                        }
                        if should_update_supported {
                            session.send(Data::Message(
                                session.lc.read().unwrap().update_supported_decodings(),
                            ));
                        }
                    }
                    MediaData::Reset => {
                        if let Some(handler) = video_handler.as_mut() {
                            handler.reset(None);
                        }
                    }
                    MediaData::RecordScreen(start) => {
                        let id = session.lc.read().unwrap().id.clone();
                        if let Some(handler) = video_handler.as_mut() {
                            handler.record_screen(start, id, display, is_view_camera);
                        }
                    }
                    _ => {}
                }
            } else {
                break;
            }
        }
        log::info!("Video decoder loop exits");
    });
}

/// 启动音频线程
/// 返回一个音频 [`MediaSender`]
pub fn start_audio_thread() -> MediaSender {
    let (audio_sender, audio_receiver) = mpsc::channel::<MediaData>();
    std::thread::spawn(move || {
        let mut audio_handler = AudioHandler::default();
        loop {
            #[cfg(target_os = "windows")]
            let received = audio_handler.receive_audio(&audio_receiver);
            #[cfg(not(target_os = "windows"))]
            let received = audio_receiver.recv();
            if let Ok(data) = received {
                match data {
                    MediaData::AudioFrame(af) => {
                        audio_handler.handle_frame(*af);
                    }
                    MediaData::AudioFormat(f) => {
                        log::debug!("recved audio format, sample rate={}", f.sample_rate);
                        audio_handler.handle_format(f);
                    }
                    _ => {}
                }
            } else {
                break;
            }
        }
        log::info!("Audio decoder loop exits");
    });
    audio_sender
}

#[inline]
fn fps_calculate(
    skip_beginning: &mut usize,
    fps: &Arc<RwLock<Option<usize>>>,
    format_changed: bool,
    elapsed: std::time::Duration,
    count: &mut usize,
    duration: &mut std::time::Duration,
) {
    if format_changed {
        * count = 0;
        * duration = std::time::Duration::ZERO;
        * skip_beginning = 0;
    }
    // 第一帧会非常慢
    if *skip_beginning < 3 {
        * skip_beginning += 1;
        return;
    }
    * duration += elapsed;
    * count += 1;
    let ms = duration.as_millis();
    if *count % 10 == 0 && ms > 0 {
        * fps.write().unwrap() = Some((*count as usize) * 1000 / (ms as usize));
    }
    // 清空以获得实时 fps
    if *count >= 30 {
        * count = 0;
        * duration = Duration::ZERO;
    }
}

fn get_hwcodec_config() {
    // 用于 sciter 和 unilink
    #[cfg(feature = "hwcodec")]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let start = std::time::Instant::now();
            if let Err(e) = crate::ipc::get_hwcodec_config_from_server() {
                log::error!(
                    "Failed to get hwcodec config: {e:?}, elapsed: {:?}",
                    start.elapsed()
                );
            } else {
                log::info!("{:?} used to get hwcodec config", start.elapsed());
            }
        });
    }
}

#[cfg(windows)]
fn sync_cpu_usage() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let t = std::thread::spawn(do_sync_cpu_usage);
        t.join().ok();
    });
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn do_sync_cpu_usage() {
    use crate::ipc::{connect, Data};
    let start = std::time::Instant::now();
    match connect(50, "").await {
        Ok(mut conn) => {
            if conn.send(&&Data::SyncWinCpuUsage(None)).await.is_ok() {
                if let Ok(Some(data)) = conn.next_timeout(50).await {
                    match data {
                        Data::SyncWinCpuUsage(cpu_usage) => {
                            base::platform::windows::sync_cpu_usage(cpu_usage);
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
    log::info!("{:?} used to sync cpu usage", start.elapsed());
}

/// 处理延迟测试。
///
/// # 参数
///
/// * `t` - 延迟测试消息。
/// * `peer` - 对端。
pub async fn handle_test_delay(t: TestDelay, peer: &mut Stream) {
    if !t.from_client {
        let mut msg_out = Message::new();
        msg_out.set_test_delay(t);
        allow_err!(peer.send(&msg_out).await);
    }
}

/// 是否是触控板滚动。
#[inline]
#[cfg(all(target_os = "macos", not(feature = "flutter")))]
fn check_scroll_on_mac(mask: i32, x: i32, y: i32) -> bool {
    // 在 flutter 版本中，触控板滚动时我们把 mask 的 type 位设为 4。
    if mask & 7 == crate::input::MOUSE_TYPE_TRACKPAD {
        return true;
    }
    if mask & 3 != crate::input::MOUSE_TYPE_WHEEL {
        return false;
    }
    let btn = mask >> 3;
    if y == -1 {
        btn != 0xff88 && btn != -0x780000
    } else if y == 1 {
        btn != 0x78 && btn != 0x780000
    } else if x != 0 {
        // 鼠标不支持水平滚动。
        true
    } else {
        false
    }
}

/// 发送鼠标数据。
///
/// # 参数
///
/// * `mask` - 鼠标事件。
///     * mask = buttons << 3 | type
///     * type，1: 按下，2: 抬起，3: 滚轮，4: 触控板
///     * buttons，1: 左键，2: 右键，4: 中键
/// * `x` - X 坐标。
/// * `y` - Y 坐标。
/// * `alt` - alt 键是否按下。
/// * `ctrl` - ctrl 键是否按下。
/// * `shift` - shift 键是否按下。
/// * `command` - command 键是否按下。
/// * `interface` - 用于发送数据的接口。
#[inline]
pub fn send_mouse(
    mask: i32,
    x: i32,
    y: i32,
    alt: bool,
    ctrl: bool,
    shift: bool,
    command: bool,
    interface: &impl Interface,
) {
    let mut msg_out = Message::new();
    let mut mouse_event = MouseEvent {
        mask,
        x,
        y,
        ..Default::default()
    };
    if alt {
        mouse_event.modifiers.push(ControlKey::Alt.into());
    }
    if shift {
        mouse_event.modifiers.push(ControlKey::Shift.into());
    }
    if ctrl {
        mouse_event.modifiers.push(ControlKey::Control.into());
    }
    if command {
        mouse_event.modifiers.push(ControlKey::Meta.into());
    }
    #[cfg(all(target_os = "macos", not(feature = "flutter")))]
    if check_scroll_on_mac(mask, x, y) {
        let factor = 3;
        mouse_event.mask = crate::input::MOUSE_TYPE_TRACKPAD;
        mouse_event.x *= factor;
        mouse_event.y *= factor;
    }
    interface.swap_modifier_mouse(&mut mouse_event);
    msg_out.set_mouse_event(mouse_event);
    interface.send(Data::Message(msg_out));
}

#[inline]
pub fn send_pointer_device_event(
    mut evt: PointerDeviceEvent,
    alt: bool,
    ctrl: bool,
    shift: bool,
    command: bool,
    interface: &impl Interface,
) {
    let mut msg_out = Message::new();
    if alt {
        evt.modifiers.push(ControlKey::Alt.into());
    }
    if shift {
        evt.modifiers.push(ControlKey::Shift.into());
    }
    if ctrl {
        evt.modifiers.push(ControlKey::Control.into());
    }
    if command {
        evt.modifiers.push(ControlKey::Meta.into());
    }
    msg_out.set_pointer_device_event(evt);
    interface.send(Data::Message(msg_out));
}

/// 通过发送鼠标移动来激活操作系统。
///
/// # 参数
///
/// * `interface` - 用于发送数据的接口。
/// * `send_left_click` - 是否发送点击事件。
fn activate_os(interface: &impl Interface, send_left_click: bool) {
    let left_down = MOUSE_BUTTON_LEFT << 3 | MOUSE_TYPE_DOWN;
    let left_up = MOUSE_BUTTON_LEFT << 3 | MOUSE_TYPE_UP;
    let right_down = MOUSE_BUTTON_RIGHT << 3 | MOUSE_TYPE_DOWN;
    let right_up = MOUSE_BUTTON_RIGHT << 3 | MOUSE_TYPE_UP;
    send_mouse(left_up, 0, 0, false, false, false, false, interface);
    std::thread::sleep(Duration::from_millis(50));
    send_mouse(0, 0, 0, false, false, false, false, interface);
    std::thread::sleep(Duration::from_millis(50));
    send_mouse(0, 3, 3, false, false, false, false, interface);
    let (click_down, click_up) = if send_left_click {
        (left_down, left_up)
    } else {
        (right_down, right_up)
    };
    std::thread::sleep(Duration::from_millis(50));
    send_mouse(click_down, 0, 0, false, false, false, false, interface);
    send_mouse(click_up, 0, 0, false, false, false, false, interface);
    /*
    let mut key_event = KeyEvent::new();
    // 不要使用 Esc，它在 Linux 上有问题
    key_event.set_control_key(ControlKey::RightArrow);
    key_event.press = true;
    let mut msg_out = Message::new();
    msg_out.set_key_event(key_event.clone());
    interface.send(Data::Message(msg_out.clone()));
    */
}

/// 输入操作系统的密码。
///
/// # 参数
///
/// * `p` - 密码。
/// * `activate` - 是否激活操作系统。
/// * `interface` - 用于发送数据的接口。
pub fn input_os_password(p: String, activate: bool, interface: impl Interface) {
    std::thread::spawn(move || {
        _input_os_password(p, activate, interface);
    });
}

/// 输入操作系统的密码。
///
/// # 参数
///
/// * `p` - 密码。
/// * `activate` - 是否激活操作系统。
/// * `interface` - 用于发送数据的接口。
fn _input_os_password(p: String, activate: bool, interface: impl Interface) {
    let input_password = !p.is_empty();
    if activate {
        // 点击事件用于唤起密码输入框。
        activate_os(&interface, input_password);
        std::thread::sleep(Duration::from_millis(1200));
    }
    if !input_password {
        return;
    }
    let mut key_event = KeyEvent::new();
    key_event.mode = KeyboardMode::Legacy.into();
    key_event.press = true;
    let mut msg_out = Message::new();
    key_event.set_seq(p);
    msg_out.set_key_event(key_event.clone());
    interface.send(Data::Message(msg_out.clone()));
    key_event.set_control_key(ControlKey::Return);
    msg_out.set_key_event(key_event);
    interface.send(Data::Message(msg_out));
}

/// 自动登录。能检测自身登录或锁屏状态的对端会拿到密码并自行
/// 输入，前提是此刻确实存在锁屏。如果在按键送达前屏幕还没锁定，
/// 这些按键就会落到当前获得焦点的窗口上。
pub fn auto_login(p: String, pi: &PeerInfo, interface: impl Interface) {
    if !is_peer_handling_os_login(pi) {
        input_os_password(p, true, interface);
        return;
    }
    std::thread::spawn(move || {
        activate_os(&interface, true);
        std::thread::sleep(Duration::from_millis(1200));
        let mut msg_out = Message::new();
        msg_out.set_os_login(OSLogin {
            password: p,
            ..Default::default()
        });
        interface.send(Data::Message(msg_out));
    });
}

/// 1.5.1 版本起的 Windows 和 macOS 对端支持 `os_login`。
fn is_peer_handling_os_login(pi: &PeerInfo) -> bool {
    (pi.platform == crate::common::PLATFORM_WINDOWS || pi.platform == crate::common::PLATFORM_MACOS)
        && hbb_common::get_version_number(&pi.version) >= hbb_common::get_version_number("1.5.1")
}

#[derive(Copy, Clone)]
struct LoginErrorMsgBox {
    msgtype: &'static str,
    title: &'static str,
    text: &'static str,
    link: &'static str,
    try_again: bool,
}

lazy_static::lazy_static! {
    static ref LOGIN_ERROR_MAP: Arc<HashMap<&'static str, LoginErrorMsgBox>> = {
        let map = HashMap::from([(LOGIN_SCREEN_WAYLAND, LoginErrorMsgBox{
            msgtype: "error",
            title: "Login Error",
            text: "Login screen using Wayland is not supported",
            link: "https://rustdesk.com/docs/en/manual/linux/#login-screen",
            try_again: true,
        }), (LOGIN_MSG_NO_PASSWORD_ACCESS, LoginErrorMsgBox{
            msgtype: "wait-remote-accept-nook",
            title: "Prompt",
            text: "Please wait for the remote side to accept your session request...",
            link: "",
            try_again: true,
        })]);
        Arc::new(map)
    };
}

/// 处理登录错误。
/// 密码错误时返回 true，存在真正的错误时返回 false。
pub fn handle_login_error(
    lc: Arc<RwLock<LoginConfigHandler>>,
    err: &str,
    interface: &impl Interface,
) -> bool {
    if err == LOGIN_MSG_PASSWORD_EMPTY {
        lc.write().unwrap().password = Default::default();
        interface.msgbox("input-password", "Password Required", "", "");
        true
    } else if err == LOGIN_MSG_PASSWORD_WRONG {
        lc.write().unwrap().password = Default::default();
        interface.msgbox("re-input-password", err, "Do you want to enter again?", "");
        true
    } else if err == LOGIN_MSG_2FA_WRONG || err == REQUIRE_2FA {
        let enabled = lc.read().unwrap().get_option("trust-this-device") == "Y";
        if enabled {
            lc.write()
                .unwrap()
                .set_option("trust-this-device".to_string(), "".to_string());
        }
        interface.msgbox("input-2fa", err, "", "");
        true
    } else if LOGIN_ERROR_MAP.contains_key(err) {
        if let Some(msgbox_info) = LOGIN_ERROR_MAP.get(err) {
            interface.msgbox(
                msgbox_info.msgtype,
                msgbox_info.title,
                msgbox_info.text,
                msgbox_info.link,
            );
            msgbox_info.try_again
        } else {
            // unreachable!
            false
        }
    } else {
        if err.contains(SCRAP_X11_REQUIRED) {
            interface.msgbox("error", "Login Error", err, SCRAP_X11_REF_URL);
        } else {
            interface.msgbox("error", "Login Error", err, "");
        }
        false
    }
}

// “切换双方”要求仅接收的客户端反向连接到它的
// 控制端对端；在建立连接之前先校验本地的待定 uuid。
#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn is_switch_sides_back(conn_type: ConnType, interface: &impl Interface) -> bool {
    if conn_type != ConnType::DEFAULT_CONN {
        return false;
    }
    let (id, uuid) = {
        let lch = interface.get_lch();
        let lc = lch.read().unwrap();
        let Some(uuid) = lc.switch_uuid.as_deref() else {
            return false;
        };
        let Ok(uuid) = Uuid::parse_str(uuid) else {
            return false;
        };
        (lc.id.clone(), uuid)
    };
    if !request_local_switch_sides_uuid(
        &id,
        &uuid,
        crate::ipc::SwitchSidesUuidAction::Check,
    )
    .await
    {
        return false;
    }
    let lch = interface.get_lch();
    let lc = lch.read().unwrap();
    let current_uuid = lc
        .switch_uuid
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok());
    lc.id == id && current_uuid.as_ref() == Some(&uuid)
}

#[cfg(not(all(feature = "flutter", not(any(target_os = "android", target_os = "ios")))))]
async fn is_switch_sides_back(_conn_type: ConnType, _interface: &impl Interface) -> bool {
    false
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn request_local_switch_sides_uuid(
    id: &str,
    uuid: &Uuid,
    action: crate::ipc::SwitchSidesUuidAction,
) -> bool {
    let Ok(mut conn) = crate::ipc::connect(1000, "").await else {
        return false;
    };
    let uuid = uuid.to_string();
    if conn
        .send(&crate::ipc::Data::SwitchSidesUuid(
            uuid.clone(),
            id.to_owned(),
            action,
            None,
        ))
        .await
        .is_err()
    {
        return false;
    }
    match conn.next_timeout(1000).await {
        Ok(Some(crate::ipc::Data::SwitchSidesUuid(
            returned_uuid,
            returned_id,
            returned_action,
            Some(true),
        ))) => {
            returned_uuid == uuid && returned_id == id && returned_action == action
        }
        _ => false,
    }
}

/// 处理对端发来的 hash 消息。
/// Hash 将用于登录。
///
/// # 参数
///
/// * `lc` - 登录配置。
/// * `hash` - 对端发来的 Hash。
/// * `interface` - 用于发送数据的 [`Interface`]。
/// * `peer` - 用于与对端通信的 [`Stream`]。
pub async fn handle_hash(
    lc: Arc<RwLock<LoginConfigHandler>>,
    password_preset: &str,
    hash: Hash,
    interface: &impl Interface,
    peer: &mut Stream,
) -> bool {
    lc.write().unwrap().hash = hash.clone();
    // 注意密码的应用顺序

    // switch_uuid
    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let uuid = lc.write().unwrap().switch_uuid.take();
        if let Some(uuid) = uuid {
            if let Ok(uuid) = uuid::Uuid::from_str(&uuid) {
                let id = lc.read().unwrap().id.clone();
                if !request_local_switch_sides_uuid(
                    &id,
                    &uuid,
                    crate::ipc::SwitchSidesUuidAction::Consume,
                )
                .await
                {
                    log::warn!("Ignored untrusted switch_uuid");
                } else {
                    lc.write().unwrap().allow_switch_back_once();
                    send_switch_login_request(lc.clone(), peer, uuid).await;
                    lc.write().unwrap().password_source = Default::default();
                    return true;
                }
            }
        }
        // 仅接收的一方可能只是为了验证后的切回而主动连出；
        // 绝不能因此落入密码登录流程，即使收到重复的 hash 也不行。
        if config::is_incoming_only() {
            interface.msgbox("error", "Connection Error", "Incoming only mode", "");
            let mut misc = Misc::new();
            misc.set_close_reason(
                "Connection not allowed in incoming-only mode".to_owned(),
            );
            let mut msg = Message::new();
            msg.set_misc(misc);
            allow_err!(peer.send(&msg).await);
            return false;
        }
    }
    // 上一次的密码
    let mut password = lc.read().unwrap().password.clone();
    // 预设密码
    if password.is_empty() {
        if !password_preset.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(password_preset);
            hasher.update(&hash.salt);
            let res = hasher.finalize();
            password = res[..].into();
            lc.write().unwrap().password_source = Default::default();
        }
    }
    // 共享密码
    // 目前它只在点击共享 ab 对端卡片时使用
    let shared_password = lc.write().unwrap().shared_password.take();
    if let Some(shared_password) = shared_password {
        if !shared_password.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(shared_password.clone());
            hasher.update(&hash.salt);
            let res = hasher.finalize();
            password = res[..].into();
            lc.write().unwrap().password_source = PasswordSource::SharedAb(shared_password);
        }
    }
    // 对端配置中的密码
    if password.is_empty() {
        password = lc.read().unwrap().config.password.clone();
        if !password.is_empty() {
            lc.write().unwrap().password_source = Default::default();
        }
    }
    // 个人 ab 密码
    if password.is_empty() {
        try_get_password_from_personal_ab(lc.clone(), &mut password);
    }

    if password.is_empty() {
        let p = crate::ui_interface::get_builtin_option(keys::OPTION_DEFAULT_CONNECT_PASSWORD);
        if !p.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(p.clone());
            hasher.update(&hash.salt);
            let res = hasher.finalize();
            password = res[..].into();
            lc.write().unwrap().password_source = PasswordSource::SharedAb(p); // reuse SharedAb here
        }
    }

    lc.write().unwrap().password = password.clone();

    let is_terminal_admin = lc.read().unwrap().is_terminal_admin;
    let is_terminal = lc.read().unwrap().conn_type.eq(&ConnType::TERMINAL);
    if is_terminal && is_terminal_admin {
        if password.is_empty() {
            interface.msgbox("terminal-admin-login-password", "", "", "");
        } else {
            interface.msgbox("terminal-admin-login", "", "", "");
        }
        lc.write().unwrap().hash = hash;
        return true;
    }

    let password = if password.is_empty() {
        // 无密码登录，远端可以点击接受
        interface.msgbox("input-password", "Password Required", "", "");
        Vec::new()
    } else {
        let mut hasher = Sha256::new();
        hasher.update(&password);
        hasher.update(&hash.challenge);
        hasher.finalize()[..].into()
    };

    send_login(lc.clone(), String::new(), String::new(), password, peer).await;
    lc.write().unwrap().hash = hash;
    true
}

#[inline]
fn try_get_password_from_personal_ab(lc: Arc<RwLock<LoginConfigHandler>>, password: &mut Vec<u8>) {
    let access_token = LocalConfig::get_option("access_token");
    let ab = config::Ab::load();
    if !access_token.is_empty() && access_token == ab.access_token {
        let id = lc.read().unwrap().id.clone();
        if let Some(ab) = ab.ab_entries.iter().find(|a| a.personal()) {
            if let Some(p) = ab
                .peers
                .iter()
                .find_map(|p| if p.id == id { Some(p) } else { None })
            {
                if let Ok(hash_password) = base64::decode(p.hash.clone(), base64::Variant::Original)
                {
                    if !hash_password.is_empty() {
                        * password = hash_password.clone();
                        lc.write().unwrap().password_source =
                            PasswordSource::PersonalAb(hash_password);
                    }
                }
            }
        }
    }
}

/// 向对端发送登录消息。
///
/// # 参数
///
/// * `lc` - 登录配置。
/// * `os_username` - 操作系统用户名。
/// * `os_password` - 操作系统密码。
/// * `password` - 密码。
/// * `peer` - 用于与对端通信的 [`Stream`]。
async fn send_login(
    lc: Arc<RwLock<LoginConfigHandler>>,
    os_username: String,
    os_password: String,
    password: Vec<u8>,
    peer: &mut Stream,
) {
    let msg_out = lc
        .read()
        .unwrap()
        .create_login_msg(os_username, os_password, password);
    allow_err!(peer.send(&msg_out).await);
}

/// 处理来自 ui 的登录请求。
///
/// # 参数
///
/// * `lc` - 登录配置。
/// * `os_username` - 操作系统用户名。
/// * `os_password` - 操作系统密码。
/// * `password` - 密码。
/// * `remember` - 是否记住密码。
/// * `peer` - 用于与对端通信的 [`Stream`]。
pub async fn handle_login_from_ui(
    lc: Arc<RwLock<LoginConfigHandler>>,
    os_username: String,
    os_password: String,
    password: String,
    remember: bool,
    peer: &mut Stream,
) {
    let mut hash_password = if password.is_empty() {
        let mut password2 = lc.read().unwrap().password.clone();
        if password2.is_empty() {
            password2 = lc.read().unwrap().config.password.clone();
            if !password2.is_empty() {
                lc.write().unwrap().password_source = Default::default();
            }
        }
        password2
    } else {
        lc.write().unwrap().password_source = Default::default();
        let mut hasher = Sha256::new();
        hasher.update(password);
        hasher.update(&lc.read().unwrap().hash.salt);
        let res = hasher.finalize();
        lc.write().unwrap().remember = remember;
        res[..].into()
    };
    lc.write().unwrap().password = hash_password.clone();
    let mut hasher2 = Sha256::new();
    hasher2.update(&hash_password[..]);
    hasher2.update(&lc.read().unwrap().hash.challenge);
    hash_password = hasher2.finalize()[..].to_vec();

    send_login(lc.clone(), os_username, os_password, hash_password, peer).await;
}

async fn send_switch_login_request(
    lc: Arc<RwLock<LoginConfigHandler>>,
    peer: &mut Stream,
    uuid: Uuid,
) {
    let mut msg_out = Message::new();
    msg_out.set_switch_sides_response(SwitchSidesResponse {
        uuid: Bytes::from(uuid.as_bytes().to_vec()),
        lr: hbb_common::protobuf::MessageField::some(
            lc.read()
                .unwrap()
                .create_login_msg("".to_owned(), "".to_owned(), vec![])
                .login_request()
                .to_owned(),
        ),
        ..Default::default()
    });
    allow_err!(peer.send(&msg_out).await);
}

/// 客户端用来发送数据和命令的接口。
#[async_trait]
pub trait Interface: Send + Clone + 'static + Sized {
    /// 向远端对端发送消息数据。
    fn send(&self, data: Data);
    fn msgbox(&self, msgtype: &str, title: &str, text: &str, link: &str);
    fn handle_login_error(&self, err: &str) -> bool;
    fn handle_peer_info(&self, pi: PeerInfo);
    fn set_multiple_windows_session(&self, sessions: Vec<WindowsSession>);
    fn on_error(&self, err: &str) {
        self.msgbox("error", "Error", err, "");
    }
    async fn handle_hash(&self, pass: &str, hash: Hash, peer: &mut Stream) -> bool;
    async fn handle_login_from_ui(
        &self,
        os_username: String,
        os_password: String,
        password: String,
        remember: bool,
        peer: &mut Stream,
    );
    async fn handle_test_delay(&self, t: TestDelay, peer: &mut Stream);

    fn get_lch(&self) -> Arc<RwLock<LoginConfigHandler>>;

    fn get_id(&self) -> String {
        self.get_lch().read().unwrap().id.clone()
    }

    fn is_force_relay(&self) -> bool {
        self.get_lch().read().unwrap().force_relay
    }

    fn is_policy_relay(&self) -> bool {
        self.get_lch().read().unwrap().policy_relay
    }

    fn get_switch_code(&self) -> String {
        match self.get_lch().read().unwrap().switch_uuid.clone() {
            Some(u) if !u.is_empty() => {
                use hbb_common::sodiumoxide::crypto::hash::sha256;
                crate::encode64(sha256::hash(u.as_bytes()).0)
            }
            _ => String::new(),
        }
    }

    fn swap_modifier_mouse(&self, _msg: &mut base::protos::message::MouseEvent) {}

    fn update_direct(&self, direct: Option<bool>) {
        self.get_lch().write().unwrap().direct = direct;
    }

    fn update_received(&self, received: bool) {
        self.get_lch().write().unwrap().received = received;
    }

    fn on_establish_connection_error(&self, err: String) {
        let title = "Connection Error";
        let text = err.to_string();
        let lch = self.get_lch();
        let (is_restarting, direct, received) = {
            let lc = lch.read().unwrap();
            (lc.is_restarting_remote_device(), lc.direct, lc.received)
        };
        if is_restarting {
            log::info!("Restart remote device, suppress connection error: {err}");
            // flutter 把它当作一个重连控制事件。这里保留该文本
            // 是为了兼容旧版 UI 以及复用已有的翻译。
            self.msgbox("restarting", "Restarting remote device", "Connection in progress. Please wait.", "");
            return;
        }

        let mut relay_hint = false;
        let mut relay_hint_type = "relay-hint";
        // 强制中继
        let errno = errno::errno().0;
        log::error!("Connection closed: {err}({errno})");
        if direct == Some(true)
            && ((cfg!(windows) && (errno == 10054 || err.contains("10054")))
                || (!cfg!(windows) && (errno == 104 || err.contains("104")))
                || (!err.contains("Failed") && err.contains("deadline")))
        // 截止时间：https://github.com/rustdesk/rustdesk-server-pro/discussions/325，多半来自安全 TCP 超时
        {
            relay_hint = true;
            if !received {
                relay_hint_type = "relay-hint2"
            }
        }

        // relay-hint
        if cfg!(feature = "flutter") && relay_hint {
            self.msgbox(relay_hint_type, title, &text, "");
        } else {
            self.msgbox("error", title, &text, "");
        }
    }
}

/// 客户端接口使用的数据。
#[derive(Clone)]
pub enum Data {
    Close,
    RejectInsecureConnection,
    Login((String, String, String, bool)),
    Message(Message),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    InitialClipboard(usize, Option<Message>),
    SendFiles((i32, JobType, String, String, i32, bool, bool)),
    RemoveDirAll((i32, String, bool, bool)),
    ConfirmDeleteFiles((i32, i32)),
    SetNoConfirm(i32),
    RemoveDir((i32, String)),
    RemoveFile((i32, String, i32, bool)),
    CreateDir((i32, String, bool)),
    CancelJob(i32),
    RemovePortForward(i32),
    AddPortForward((i32, String, i32)),
    #[cfg(all(target_os = "windows", not(feature = "flutter")))]
    ToggleClipboardFile,
    NewRDP,
    SetConfirmOverrideFile((i32, i32, bool, bool, bool)),
    AddJob((i32, JobType, String, String, i32, bool, bool)),
    ResumeJob((i32, bool)),
    RecordScreen(bool),
    ElevateDirect,
    ElevateWithLogon(String, String),
    NewVoiceCall,
    CloseVoiceCall,
    ContinueInsecureConnection,
    ResetDecoder(Option<usize>),
    RenameFile((i32, String, String, bool)),
    TakeScreenshot((i32, String)),
}

pub async fn confirm_insecure_connection(
    interface: &impl Interface,
    receiver: &mut UnboundedReceiver<Data>,
) -> bool {
    interface.msgbox(
        "insecure-connection-nocancel-hasclose",
        "Insecure Connection",
        "conn-e2ee-unavailable-tip",
        "",
    );
    while let Some(data) = receiver.recv().await {
        match data {
            Data::ContinueInsecureConnection => return true,
            Data::RejectInsecureConnection => return false,
            Data::Close => return false,
            _ => {}
        }
    }
    false
}

/// 按键事件的键码。
#[derive(Clone, Debug)]
pub enum Key {
    ControlKey(ControlKey),
    Chr(u32),
    _Raw(u32),
}

lazy_static::lazy_static! {
    pub static ref KEY_MAP: HashMap<&'static str, Key> =
    [
        ("VK_A", Key::Chr('a' as _)),
        ("VK_B", Key::Chr('b' as _)),
        ("VK_C", Key::Chr('c' as _)),
        ("VK_D", Key::Chr('d' as _)),
        ("VK_E", Key::Chr('e' as _)),
        ("VK_F", Key::Chr('f' as _)),
        ("VK_G", Key::Chr('g' as _)),
        ("VK_H", Key::Chr('h' as _)),
        ("VK_I", Key::Chr('i' as _)),
        ("VK_J", Key::Chr('j' as _)),
        ("VK_K", Key::Chr('k' as _)),
        ("VK_L", Key::Chr('l' as _)),
        ("VK_M", Key::Chr('m' as _)),
        ("VK_N", Key::Chr('n' as _)),
        ("VK_O", Key::Chr('o' as _)),
        ("VK_P", Key::Chr('p' as _)),
        ("VK_Q", Key::Chr('q' as _)),
        ("VK_R", Key::Chr('r' as _)),
        ("VK_S", Key::Chr('s' as _)),
        ("VK_T", Key::Chr('t' as _)),
        ("VK_U", Key::Chr('u' as _)),
        ("VK_V", Key::Chr('v' as _)),
        ("VK_W", Key::Chr('w' as _)),
        ("VK_X", Key::Chr('x' as _)),
        ("VK_Y", Key::Chr('y' as _)),
        ("VK_Z", Key::Chr('z' as _)),
        ("VK_0", Key::Chr('0' as _)),
        ("VK_1", Key::Chr('1' as _)),
        ("VK_2", Key::Chr('2' as _)),
        ("VK_3", Key::Chr('3' as _)),
        ("VK_4", Key::Chr('4' as _)),
        ("VK_5", Key::Chr('5' as _)),
        ("VK_6", Key::Chr('6' as _)),
        ("VK_7", Key::Chr('7' as _)),
        ("VK_8", Key::Chr('8' as _)),
        ("VK_9", Key::Chr('9' as _)),
        ("VK_COMMA", Key::Chr(',' as _)),
        ("VK_SLASH", Key::Chr('/' as _)),
        ("VK_SEMICOLON", Key::Chr(';' as _)),
        ("VK_QUOTE", Key::Chr('\'' as _)),
        ("VK_LBRACKET", Key::Chr('[' as _)),
        ("VK_RBRACKET", Key::Chr(']' as _)),
        ("VK_BACKSLASH", Key::Chr('\\' as _)),
        ("VK_MINUS", Key::Chr('-' as _)),
        ("VK_PLUS", Key::Chr('=' as _)), // it is =, but sciter return VK_PLUS
        ("VK_DIVIDE", Key::ControlKey(ControlKey::Divide)), // numpad
        ("VK_MULTIPLY", Key::ControlKey(ControlKey::Multiply)), // numpad
        ("VK_SUBTRACT", Key::ControlKey(ControlKey::Subtract)), // numpad
        ("VK_ADD", Key::ControlKey(ControlKey::Add)), // numpad
        ("VK_DECIMAL", Key::ControlKey(ControlKey::Decimal)), // numpad
        ("VK_F1", Key::ControlKey(ControlKey::F1)),
        ("VK_F2", Key::ControlKey(ControlKey::F2)),
        ("VK_F3", Key::ControlKey(ControlKey::F3)),
        ("VK_F4", Key::ControlKey(ControlKey::F4)),
        ("VK_F5", Key::ControlKey(ControlKey::F5)),
        ("VK_F6", Key::ControlKey(ControlKey::F6)),
        ("VK_F7", Key::ControlKey(ControlKey::F7)),
        ("VK_F8", Key::ControlKey(ControlKey::F8)),
        ("VK_F9", Key::ControlKey(ControlKey::F9)),
        ("VK_F10", Key::ControlKey(ControlKey::F10)),
        ("VK_F11", Key::ControlKey(ControlKey::F11)),
        ("VK_F12", Key::ControlKey(ControlKey::F12)),
        ("VK_ENTER", Key::ControlKey(ControlKey::Return)),
        ("VK_CANCEL", Key::ControlKey(ControlKey::Cancel)),
        ("VK_BACK", Key::ControlKey(ControlKey::Backspace)),
        ("VK_TAB", Key::ControlKey(ControlKey::Tab)),
        ("VK_CLEAR", Key::ControlKey(ControlKey::Clear)),
        ("VK_RETURN", Key::ControlKey(ControlKey::Return)),
        ("VK_SHIFT", Key::ControlKey(ControlKey::Shift)),
        ("VK_CONTROL", Key::ControlKey(ControlKey::Control)),
        ("VK_MENU", Key::ControlKey(ControlKey::Alt)),
        ("VK_PAUSE", Key::ControlKey(ControlKey::Pause)),
        ("VK_CAPITAL", Key::ControlKey(ControlKey::CapsLock)),
        ("VK_KANA", Key::ControlKey(ControlKey::Kana)),
        ("VK_HANGUL", Key::ControlKey(ControlKey::Hangul)),
        ("VK_JUNJA", Key::ControlKey(ControlKey::Junja)),
        ("VK_FINAL", Key::ControlKey(ControlKey::Final)),
        ("VK_HANJA", Key::ControlKey(ControlKey::Hanja)),
        ("VK_KANJI", Key::ControlKey(ControlKey::Kanji)),
        ("VK_ESCAPE", Key::ControlKey(ControlKey::Escape)),
        ("VK_CONVERT", Key::ControlKey(ControlKey::Convert)),
        ("VK_SPACE", Key::ControlKey(ControlKey::Space)),
        ("VK_PRIOR", Key::ControlKey(ControlKey::PageUp)),
        ("VK_NEXT", Key::ControlKey(ControlKey::PageDown)),
        ("VK_END", Key::ControlKey(ControlKey::End)),
        ("VK_HOME", Key::ControlKey(ControlKey::Home)),
        ("VK_LEFT", Key::ControlKey(ControlKey::LeftArrow)),
        ("VK_UP", Key::ControlKey(ControlKey::UpArrow)),
        ("VK_RIGHT", Key::ControlKey(ControlKey::RightArrow)),
        ("VK_DOWN", Key::ControlKey(ControlKey::DownArrow)),
        ("VK_SELECT", Key::ControlKey(ControlKey::Select)),
        ("VK_PRINT", Key::ControlKey(ControlKey::Print)),
        ("VK_EXECUTE", Key::ControlKey(ControlKey::Execute)),
        ("VK_SNAPSHOT", Key::ControlKey(ControlKey::Snapshot)),
        ("VK_SCROLL", Key::ControlKey(ControlKey::Scroll)),
        ("VK_INSERT", Key::ControlKey(ControlKey::Insert)),
        ("VK_DELETE", Key::ControlKey(ControlKey::Delete)),
        ("VK_HELP", Key::ControlKey(ControlKey::Help)),
        ("VK_SLEEP", Key::ControlKey(ControlKey::Sleep)),
        ("VK_SEPARATOR", Key::ControlKey(ControlKey::Separator)),
        ("VK_NUMPAD0", Key::ControlKey(ControlKey::Numpad0)),
        ("VK_NUMPAD1", Key::ControlKey(ControlKey::Numpad1)),
        ("VK_NUMPAD2", Key::ControlKey(ControlKey::Numpad2)),
        ("VK_NUMPAD3", Key::ControlKey(ControlKey::Numpad3)),
        ("VK_NUMPAD4", Key::ControlKey(ControlKey::Numpad4)),
        ("VK_NUMPAD5", Key::ControlKey(ControlKey::Numpad5)),
        ("VK_NUMPAD6", Key::ControlKey(ControlKey::Numpad6)),
        ("VK_NUMPAD7", Key::ControlKey(ControlKey::Numpad7)),
        ("VK_NUMPAD8", Key::ControlKey(ControlKey::Numpad8)),
        ("VK_NUMPAD9", Key::ControlKey(ControlKey::Numpad9)),
        ("Apps", Key::ControlKey(ControlKey::Apps)),
        ("Meta", Key::ControlKey(ControlKey::Meta)),
        ("RAlt", Key::ControlKey(ControlKey::RAlt)),
        ("RWin", Key::ControlKey(ControlKey::RWin)),
        ("RControl", Key::ControlKey(ControlKey::RControl)),
        ("RShift", Key::ControlKey(ControlKey::RShift)),
        ("CTRL_ALT_DEL", Key::ControlKey(ControlKey::CtrlAltDel)),
        ("LOCK_SCREEN", Key::ControlKey(ControlKey::LockScreen)),
    ].iter().cloned().collect();
}

/// 检查给定消息是否是可重试的错误。
///
/// # 参数
///
/// * `msgtype` - 消息类型。
/// * `title` - 消息标题。
/// * `text` - 消息正文。
#[inline]
pub fn check_if_retry(msgtype: &str, title: &str, text: &str, retry_for_relay: bool) -> bool {
    msgtype == "error"
        && title == "Connection Error"
        && ((text.contains("10054") || text.contains("104")) && retry_for_relay
            || (!text.to_lowercase().contains("offline")
                && !text.to_lowercase().contains("not exist")
                && (!text.to_lowercase().contains("handshake")
                    // https://github.com/snapview/tungstenite-rs/blob/e7e060a89a72cb08e31c25a6c7284dc1bd982e23/src/error.rs#L248
                    || text
                        .to_lowercase()
                        .contains("connection reset without closing handshake") && use_ws())
                && !text.to_lowercase().contains("failed")
                && !text.to_lowercase().contains("resolve")
                && !text.to_lowercase().contains("mismatch")
                && !text.to_lowercase().contains("manually")
                && !text.to_lowercase().contains("restricted")
                && !text.to_lowercase().contains("incoming only")
                && !text.to_lowercase().contains("not allowed")))
}

#[cfg(test)]
mod retry_tests {
    use super::check_if_retry;

    #[test]
    fn incoming_only_error_is_not_retryable() {
        assert!(!check_if_retry(
            "error",
            "Connection Error",
            "Incoming only mode",
            false,
        ));
    }
}

pub async fn hc_connection(
    feedback: i32,
    rendezvous_server: String,
    token: &str,
) -> Option<tokio::sync::mpsc::UnboundedSender<()>> {
    if feedback == 0 || rendezvous_server.is_empty() || token.is_empty() {
        return None;
    }
    let (tx, rx) = unbounded_channel::<()>();
    let token = token.to_owned();
    tokio::spawn(async move {
        allow_err!(hc_connection_(rendezvous_server, rx, token).await);
    });
    Some(tx)
}

async fn hc_connection_(
    rendezvous_server: String,
    mut rx: UnboundedReceiver<()>,
    token: String,
) -> ResultType<()> {
    let mut timer = crate::rustdesk_interval(interval(crate::TIMER_OUT));
    let mut last_recv_msg = Instant::now();
    let mut keep_alive = crate::DEFAULT_KEEP_ALIVE;

    let host = check_port(&rendezvous_server, RENDEZVOUS_PORT);
    let mut conn = connect_tcp(host.clone(), CONNECT_TIMEOUT).await?;
    let key = crate::get_key(true).await;
    crate::secure_tcp(&mut conn, &key).await?;
    let mut msg_out = RendezvousMessage::new();
    msg_out.set_hc(HealthCheck {
        token,
        ..Default::default()
    });
    conn.send(&msg_out).await?;
    loop {
        tokio::select! {
            res = rx.recv() => {
                if res.is_none() {
                    log::debug!("HC connection is closed as controlling connection exits");
                    break;
                }
            }
            res = conn.next() => {
                last_recv_msg = Instant::now();
                let bytes = res.ok_or_else(|| anyhow!("Rendezvous connection is reset by the peer"))??;
                if bytes.is_empty() {
                    conn.send_bytes(bytes::Bytes::new()).await?;
                    continue; // heartbeat
                }
                let msg = RendezvousMessage::parse_from_bytes(&bytes)?;
                match msg.union {
                    Some(rendezvous_message::Union::RegisterPkResponse(rpr)) => {
                        if rpr.keep_alive > 0 {
                            keep_alive = rpr.keep_alive * 1000;
                            log::info!("keep_alive: {}ms", keep_alive);
                        }
                    }
                    _ => {}
                }
            }
            _  = timer.tick() => {
                // https://www.emqx.com/en/blog/mqtt-keep-alive
                if last_recv_msg.elapsed().as_millis() as u64 > keep_alive as u64 * 3 / 2 {
                    bail!("HC connection is timeout");
                }
            }
        }
    }
    Ok(())
}

pub mod peer_online {
    use hbb_common::{
        anyhow::bail,
        config::{Config, CONNECT_TIMEOUT, READ_TIMEOUT},
        log,
        rendezvous_proto::*,
        sleep,
        socket_client::connect_tcp,
        ResultType, Stream,
    };

    pub async fn query_online_states<F: FnOnce(Vec<String>, Vec<String>)>(ids: Vec<String>, f: F) {
        let test = false;
        if test {
            sleep(1.5).await;
            let mut onlines = ids;
            let offlines = onlines.drain((onlines.len() / 2)..).collect();
            f(onlines, offlines)
        } else {
            let query_timeout = std::time::Duration::from_millis(3_000);
            match query_online_states_(&ids, query_timeout).await {
                Ok((onlines, offlines)) => {
                    f(onlines, offlines);
                }
                Err(e) => {
                    log::debug!("query onlines, {}", &e);
                }
            }
        }
    }

    async fn create_online_stream() -> ResultType<Stream> {
        let (rendezvous_server, _servers, _contained) =
            crate::get_rendezvous_server(READ_TIMEOUT).await;
        let tmp: Vec<&str> = rendezvous_server.split(":").collect();
        if tmp.len() != 2 {
            bail!("Invalid server address: {}", rendezvous_server);
        }
        let port: u16 = tmp[1].parse()?;
        if port == 0 {
            bail!("Invalid server address: {}", rendezvous_server);
        }
        let online_server = format!("{}:{}", tmp[0], port - 1);
        connect_tcp(online_server, CONNECT_TIMEOUT).await
    }

    async fn query_online_states_(
        ids: &Vec<String>,
        timeout: std::time::Duration,
    ) -> ResultType<(Vec<String>, Vec<String>)> {
        let mut msg_out = RendezvousMessage::new();
        msg_out.set_online_request(OnlineRequest {
            id: Config::get_id(),
            peers: ids.clone(),
            ..Default::default()
        });

        let mut socket = match create_online_stream().await {
            Ok(s) => s,
            Err(e) => {
                log::debug!("Failed to create peers online stream, {e}");
                return Ok((vec![], ids.clone()));
            }
        };
        // TODO: Use long connections to avoid socket creation
        // 如果我们用 Arc<Mutex<Option<FramedStream>>> 持有并复用上一个套接字，
        // 可能会遇到下面这个错误：
        // 已建立的连接被主机上的软件中止。（os error 10053）
        if let Err(e) = socket.send(&msg_out).await {
            log::debug!("Failed to send peers online states query, {e}");
            return Ok((vec![], ids.clone()));
        }
        // 重试 2 次以获取在线响应
        for _ in 0..2 {
            if let Some(msg_in) =
                crate::get_next_nonkeyexchange_msg(&mut socket, Some(timeout.as_millis() as _))
                    .await
            {
                match msg_in.union {
                    Some(rendezvous_message::Union::OnlineResponse(online_response)) => {
                        let states = online_response.states;
                        // 每个 ID 一位，向上取整到整字节：
                        // 0 个 ID -> 0 字节；1..=8 -> 1；9..=16 -> 2；17..=24 -> 3。
                        let required_len = ids.len().div_ceil(u8::BITS as usize);
                        if states.len() < required_len {
                            bail!(
                                "Invalid online response: expected at least {required_len} state bytes, got {}",
                                states.len()
                            );
                        }
                        let mut onlines = Vec::new();
                        let mut offlines = Vec::new();
                        for i in 0..ids.len() {
                            // 字节索引从左到右
                            let bit_value = 0x01 << (7 - i % 8);
                            if (states[i / 8] & bit_value) == bit_value {
                                onlines.push(ids[i].clone());
                            } else {
                                offlines.push(ids[i].clone());
                            }
                        }
                        return Ok((onlines, offlines));
                    }
                    _ => {
                        // 忽略
                    }
                }
            } else {
                // TODO: Make sure socket closed?
                bail!("Online stream receives None");
            }
        }

        bail!("Failed to query online states, no online response");
    }

    #[cfg(test)]
    mod tests {
        use crate::client::Client;
        use hbb_common::rendezvous_proto::IceCandidate;

        #[test]
        fn accepts_webrtc_ice_by_session_key_only() {
            let ice = IceCandidate {
                session_key: "session-a".to_owned(),
                candidate: "candidate-json".to_owned(),
                ..Default::default()
            };

            assert!(Client::is_expected_webrtc_ice_candidate(&ice, "session-a"));
            assert!(!Client::is_expected_webrtc_ice_candidate(&ice, "session-b"));
        }
    }
}

async fn test_udp_uat(
    udp_socket: Arc<UdpSocket>,
    server_addr: SocketAddr,
    udp_port: Arc<Mutex<u16>>,
    mut stop_udp_rx: oneshot::Receiver<()>,
) -> ResultType<()> {
    // 打洞端口必须只来自 rendezvous 服务器的 TestNatResponse，它
    // 观察的是这个套接字自己的公网映射。而 STUN 探测绑定的是另一个套接字，报告的
    // 是另一个 NAT 映射，所以在这里与它竞速可能会公布一个对端永远
    // 无法到达的端口（在对称 NAT 下还会悄无声息地毁掉整个 UDP 打洞）。
    let start = Instant::now();
    let mut msg_out = RendezvousMessage::new();
    msg_out.set_test_nat_request(TestNatRequest {
        ..Default::default()
    });
    // 在 TCP RTT 约束内可行的自适应重试策略
    // 一开始积极发送，然后逐步退避
    let mut retry_interval = Duration::from_millis(20); // Start fast
    const MAX_INTERVAL: Duration = Duration::from_millis(200);
    let mut packets_sent = 0;

    // 先发送一轮突发数据以提高可靠性
    let data = msg_out.write_to_bytes()?;
    for _ in 0..2 {
        if let Err(e) = udp_socket.send_to(&data, server_addr).await {
            log::warn!("Failed to send initial UDP NAT test packet: {}", e);
        } else {
            packets_sent += 1;
        }
    }
    let mut last_send_time = Instant::now();
    let mut buf = [0u8; 1500];

    loop {
        tokio::select! {
            _ = &mut stop_udp_rx => {
                log::debug!("UDP NAT test received stop signal after {} packets", packets_sent);
                break;
            }
            _ = hbb_common::sleep(retry_interval.as_secs_f32()) => {
                // 自适应重试：随着时间推移发送更少的包
                let elapsed = last_send_time.elapsed();

                if elapsed >= retry_interval {
                    // 发送单个包（而不是两个）以降低网络负载
                    if let Err(e) = udp_socket.send_to(&data, server_addr).await {
                        log::warn!("Failed to send UDP NAT test retry packet: {}", e);
                    } else {
                        packets_sent += 1;
                    }

                    // 指数式拉长间隔以减轻网络压力
                    retry_interval = std::cmp::min(
                        Duration::from_millis((retry_interval.as_millis() as f64 * 1.5) as u64),
                        MAX_INTERVAL
                    );
                    last_send_time = Instant::now();
                }
            }
            res = udp_socket.recv(&mut buf[..]) => {
                match res {
                    Ok(n) => {
                        match RendezvousMessage::parse_from_bytes(&buf[0..n]) {
                            Ok(msg_in) => {
                                if let Some(rendezvous_message::Union::TestNatResponse(response)) = msg_in.union {
                                    * udp_port.lock().unwrap() = response.port as u16;
                                    break;
                                }
                            }
                            Err(e) => {
                                log::warn!("Failed to parse UDP NAT test response: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        // 与 punch_udp 会遇到的同类 ICMP 触发的错误。没有这个暂停，本分支
                        // 会立刻重新武装 recv，让循环以 CPU 速度空转。
                        if let Some(n) = UDP_UAT_ERR_LOG.due() {
                            log::warn!("UDP NAT test socket error x{n}, last: {e}");
                        }
                        hbb_common::sleep(0.01).await;
                    }
                }
            }
        }
    }

    let final_port = *udp_port.lock().unwrap();
    log::debug!(
        "UDP NAT test to {:?} finished: time={:?}, port={}, packets_sent={}, success={}",
        server_addr,
        start.elapsed(),
        final_port,
        packets_sent,
        final_port > 0
    );
    Ok(())
}

#[inline]
async fn udp_nat_connect(
    socket: Arc<UdpSocket>,
    typ: &'static str,
    ms_timeout: u64,
) -> ResultType<(Stream, Option<KcpStream>, &'static str)> {
    crate::punch_udp(socket.clone(), false)
        .await
        .map_err(|err| {
            log::debug!("{err}");
            anyhow!(err)
        })?;
    let res = KcpStream::connect(socket, Duration::from_millis(ms_timeout))
        .await
        .map_err(|err| {
            log::debug!("Failed to connect KCP stream: {}", err);
            anyhow!(err)
        })?;
    Ok((res.1, Some(res.0), typ))
}

#[cfg(test)]
mod webrtc_race_tests {
    use super::race_transports_prefer_webrtc;
    use hbb_common::{
        anyhow::anyhow,
        futures::future::{BoxFuture, FutureExt},
        tokio,
        tokio::time::{sleep, Duration, Instant},
        ResultType,
    };

    fn ok_after(ms: u64, tag: &'static str) -> BoxFuture<'static, ResultType<&'static str>> {
        async move {
            sleep(Duration::from_millis(ms)).await;
            Ok(tag)
        }
        .boxed()
    }

    fn err_after(ms: u64, what: &'static str) -> BoxFuture<'static, ResultType<&'static str>> {
        async move {
            sleep(Duration::from_millis(ms)).await;
            Err(anyhow!(what))
        }
        .boxed()
    }

    const NOT_P2P: fn(&&'static str) -> bool = |_| false;

    // 直连结果即使最先到达也必须胜出——局域网场景下 ICE 的建立比中继
    // 的 TCP 连接更快。把它当作中继暂存，会在它到达时直接提交中继。
    #[tokio::test]
    async fn direct_result_wins_even_when_it_arrives_first() {
        let got = race_transports_prefer_webrtc(
            ok_after(10, "direct"),
            vec![ok_after(120, "relay")],
            60_000,
            |result| *result == "direct",
        )
        .await
        .unwrap();
        assert_eq!(got, "direct");
    }

    #[tokio::test]
    async fn webrtc_preferred_over_faster_relay_within_window() {
        let got = race_transports_prefer_webrtc(
            ok_after(120, "webrtc"),
            vec![ok_after(10, "relay")],
            60_000,
            NOT_P2P,
        )
        .await
        .unwrap();
        assert_eq!(got, "webrtc");
    }

    // preferred 分支运行的是一次完整的打洞尝试，所以它自身也可能以中继收场。这
    // 不能抢先于回退分支上仍在进行中的直连打洞。
    #[tokio::test]
    async fn relay_from_preferred_branch_does_not_preempt_a_direct_fallback() {
        let got = race_transports_prefer_webrtc(
            ok_after(10, "preferred-relay"),
            vec![ok_after(120, "direct")],
            60_000,
            |tag| *tag == "direct",
        )
        .await
        .unwrap();
        assert_eq!(got, "direct");
    }

    // ……但一旦不可能再有直连到达，它仍然会被提交。
    #[tokio::test]
    async fn relay_from_preferred_branch_committed_when_fallback_fails() {
        let got = race_transports_prefer_webrtc(
            ok_after(10, "preferred-relay"),
            vec![err_after(50, "punch failed")],
            60_000,
            NOT_P2P,
        )
        .await
        .unwrap();
        assert_eq!(got, "preferred-relay");
    }

    #[tokio::test]
    async fn unfinished_ipv6_still_beats_held_relay() {
        let start = Instant::now();
        let got = race_transports_prefer_webrtc(
            ok_after(60_000, "webrtc"),
            vec![ok_after(10, "relay"), ok_after(100, "ipv6")],
            1_000,
            |t| *t == "ipv6",
        )
        .await
        .unwrap();
        assert_eq!(got, "ipv6");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    // WebRTC 先失败（其他分支仍在竞速），随后中继到达，而此时仍有一个直连尝试
    // 在它后面未完成。只要那个直连尝试还有可能胜出，就不能提交中继。
    #[tokio::test]
    async fn relay_does_not_preempt_unfinished_direct_after_webrtc_fails() {
        let got = race_transports_prefer_webrtc(
            err_after(5, "webrtc dead"),
            vec![ok_after(10, "relay"), ok_after(100, "ipv6")],
            60_000,
            |t| *t == "ipv6",
        )
        .await
        .unwrap();
        assert_eq!(got, "ipv6");
    }

    #[tokio::test]
    async fn held_relay_committed_when_window_expires() {
        let start = Instant::now();
        let got = race_transports_prefer_webrtc(
            ok_after(60_000, "webrtc"),
            vec![ok_after(10, "relay")],
            150,
            NOT_P2P,
        )
        .await
        .unwrap();
        assert_eq!(got, "relay");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn held_relay_committed_when_webrtc_fails() {
        let start = Instant::now();
        let got = race_transports_prefer_webrtc(
            err_after(50, "webrtc dead"),
            vec![ok_after(10, "relay")],
            60_000,
            NOT_P2P,
        )
        .await
        .unwrap();
        assert_eq!(got, "relay");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn relay_committed_directly_after_webrtc_failed() {
        let got = race_transports_prefer_webrtc(
            err_after(5, "webrtc dead"),
            vec![ok_after(100, "relay")],
            60_000,
            NOT_P2P,
        )
        .await
        .unwrap();
        assert_eq!(got, "relay");
    }

    #[tokio::test]
    async fn webrtc_still_wins_past_window_when_relay_dead() {
        let got = race_transports_prefer_webrtc(
            ok_after(300, "webrtc"),
            vec![err_after(10, "relay dead")],
            50,
            NOT_P2P,
        )
        .await
        .unwrap();
        assert_eq!(got, "webrtc");
    }

    #[tokio::test]
    async fn p2p_transport_committed_immediately() {
        let start = Instant::now();
        let got = race_transports_prefer_webrtc(
            ok_after(60_000, "webrtc"),
            vec![ok_after(10, "ipv6")],
            60_000,
            |t| *t == "ipv6",
        )
        .await
        .unwrap();
        assert_eq!(got, "ipv6");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    // 两个分支拿到的是同样的 rendezvous 拒绝信息；UI 需要原文，比如
    // 离线自动重试是整段匹配的。任何一个分支都可能是最后失败的那个。
    #[tokio::test]
    async fn same_failure_on_both_legs_reported_once() {
        for (webrtc_ms, fallback_ms) in [(10, 20), (20, 10)] {
            let err = race_transports_prefer_webrtc(
                err_after(webrtc_ms, "Remote desktop is offline"),
                vec![err_after(fallback_ms, "Remote desktop is offline")],
                60_000,
                NOT_P2P,
            )
            .await
            .unwrap_err();
            assert_eq!(err.to_string(), "Remote desktop is offline");
        }
    }
}

#[cfg(test)]
mod kx_tests {
    use super::*;
    use hbb_common::{
        sodiumoxide::crypto::box_,
        tcp::{Encrypt, FramedStream, KxTranscript, KX_VERSION_LATEST},
    };

    const PEER_ID: &str = "123456789";

    /// 替身被控端看到的内容：控制端选择的版本，以及控制端发出的
    /// 第一条应用消息是否解密成功。
    struct Seen {
        picked: u32,
        decrypted: bool,
    }

    fn test_delay(time: i64) -> Message {
        let mut msg = Message::new();
        msg.set_test_delay(TestDelay {
            time,
            ..Default::default()
        });
        msg
    }

    fn delay_in(bytes: &[u8]) -> Option<i64> {
        match Message::parse_from_bytes(bytes).ok()?.union? {
            message::Union::TestDelay(t) => Some(t.time),
            _ => None,
        }
    }

    /// 一个运行在回环地址上的替身被控端。它在签名的身份中公布 `advertised`
    /// 并以 `run` 运行该密钥；当 `run` 为
    /// `None` 时则使用控制端所选的版本，然后发送一条消息并读取一条。返回主机、它按
    /// rendezvous 服务器的方式签署后的身份，以及它看到的内容。
    async fn controlled_stub(
        advertised: u32,
        run: Option<u32>,
        rs_sk: sign::SecretKey,
    ) -> (String, Vec<u8>, oneshot::Receiver<Seen>) {
        let (sign_pk, sign_sk) = sign::gen_keypair();
        let id_pk = |pk: &[u8], kx_version| {
            IdPk {
                id: PEER_ID.to_owned(),
                pk: pk.to_vec().into(),
                kx_version,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap()
        };
        let signed_id_pk = sign::sign(&id_pk(&sign_pk.0, 0), &rs_sk);
        let listener = hbb_common::tcp::new_listener("127.0.0.1:0", false)
            .await
            .unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, addr) = listener.accept().await.unwrap();
            let mut s = FramedStream::from(stream, addr);
            let (our_pk_b, our_sk_b) = box_::gen_keypair();
            let mut msg = Message::new();
            msg.set_signed_id(SignedId {
                id: sign::sign(&id_pk(&our_pk_b.0, advertised), &sign_sk).into(),
                ..Default::default()
            });
            s.send(&msg).await.unwrap();
            let bytes = s.next().await.unwrap().unwrap();
            let Some(message::Union::PublicKey(pk)) =
                Message::parse_from_bytes(&bytes).unwrap().union
            else {
                panic!("expected the controller's public key");
            };
            let key =
                Encrypt::decode(&pk.symmetric_value, &pk.asymmetric_value, &our_sk_b).unwrap();
            let t = KxTranscript {
                initiator_pk: &pk.asymmetric_value,
                responder_pk: &our_pk_b.0,
                advertised,
                picked: run.unwrap_or(pk.kx_version),
            };
            if t.picked == 0 {
                s.set_key(key);
            } else {
                s.set_key_split(key, false, &t).unwrap();
            }
            s.send(&test_delay(2)).await.unwrap();
            let decrypted = matches!(s.next().await, Some(Ok(b)) if delay_in(&b) == Some(1));
            tx.send(Seen {
                picked: pk.kx_version,
                decrypted,
            })
            .ok();
        });
        (host, signed_id_pk, rx)
    }

    /// 控制端与这个桩的握手，然后双向各一条应用消息。
    /// 返回桩看到的内容，以及桩的消息在这一侧是否解密成功。
    async fn handshake(advertised: u32, run: Option<u32>) -> (Seen, bool) {
        let (rs_pk, rs_sk) = sign::gen_keypair();
        let (host, signed_id_pk, seen) = controlled_stub(advertised, run, rs_sk).await;
        let mut conn = connect_tcp(host, 3000).await.unwrap();
        let pk =
            Client::secure_connection(PEER_ID, signed_id_pk, &crate::encode64(rs_pk.0), &mut conn)
                .await
                .unwrap();
        assert!(pk.is_some() && conn.is_secured());
        conn.send(&test_delay(1)).await.unwrap();
        let decrypted = matches!(conn.next().await, Some(Ok(b)) if delay_in(&b) == Some(2));
        (seen.await.unwrap(), decrypted)
    }

    #[tokio::test]
    async fn test_new_peers_pick_version_1_and_exchange_application_data() {
        let (seen, decrypted) = handshake(KX_VERSION_LATEST, None).await;
        assert_eq!(seen.picked, 1);
        assert!(seen.decrypted && decrypted);
    }

    #[tokio::test]
    async fn test_a_peer_without_versions_gets_version_0() {
        let (seen, decrypted) = handshake(0, Some(0)).await;
        assert_eq!(seen.picked, 0);
        assert!(seen.decrypted && decrypted);
    }

    #[tokio::test]
    async fn test_a_pick_lowered_in_transit_decrypts_nothing() {
        // 没有版本信息的控制端在受控端那里会被当作 0，因此一个被降低的选择
        // 要靠双方密钥在第一条应用消息上不一致才能发现，而不是更早。
        let (seen, decrypted) = handshake(KX_VERSION_LATEST, Some(0)).await;
        assert_eq!(seen.picked, 1);
        assert!(!seen.decrypted && !decrypted);
    }
}
