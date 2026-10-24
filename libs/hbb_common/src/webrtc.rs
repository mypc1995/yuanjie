//! RustDesk 流的 WebRTC 传输。
//!
//! # 升级 webrtc crate
//!
//! 本模块依赖 webrtc-rs 的内部实现，而其公共 API 并未对此作出承诺。
//! 请对照新的源码重新核验（最后核验于：webrtc 0.13、-data 0.11、-sctp 0.12），然后运行
//! `cargo test --features webrtc webrtc::tests`。
//!
//! - `DataChannel::write` 在 PendingQueue 满时会挂起，而不是无限制地缓冲。
//! - SCTP 最大消息长度为 65536：`MAX_FRAGMENT_PAYLOAD` + 1 必须小于它。
//! - `read_sctp` 在出队后不再 await，因此被丢弃的 `next()` 不会丢失分片。
//! - `detach()` 是幂等的 Arc 克隆，drop 时不会关闭。
//! - `on_*` 处理器存活在 pc 中：持有强引用 `Arc<RTCPeerConnection>` 的处理器会造成泄漏。
//! - `close()` 在首次 await 之前就锁定 `is_closed`，因此被取消的关闭无法重试。
//! - `Disconnected` 是暂时的；只有 `Failed`/`Closed` 才是终止状态。
//! - `RTCIceCandidatePair` 的 `Display` 布局——`is_relayed()` 之所以要解析它，是因为 0.13 把
//!   candidates 保持为私有。0.17+ 公开了它们；届时改用新接口并删掉这段解析。

use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data::data_channel::DataChannel as DetachedDataChannel;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use percent_encoding::percent_decode_str;
use tokio::sync::{mpsc, watch, Mutex, Semaphore};
use tokio::time::{timeout, timeout_at, Instant};
use url::Url;

use crate::config;
use crate::protobuf::Message;
use crate::rx_probe::RxProbe;
use crate::sodiumoxide::crypto::secretbox::Key;
use crate::ResultType;

#[derive(Clone, Debug, PartialEq, Eq)]
enum WebRTCConnectionState {
    Pending,
    Open,
    Closed(String),
}

/// 一个共享句柄，而不是所有者：每个克隆都指向同一个 `pc`，而 `SESSIONS` 持有其中
/// 一个克隆。因此没有 `Drop`——按克隆逐个关闭会在落败的竞态 future 被丢弃时
/// 直接杀死一个仍存活的会话，而在最后一个克隆上关闭又会去等待那个
/// 负责驱逐缓存条目的关闭。所有权在连接尝试接管之前属于 `OffererGuard`，
/// 之后属于 `Stream::WebRTC`；在这两者之外持有它就意味着要手动关闭。
pub struct WebRTCStream {
    pc: Arc<RTCPeerConnection>,
    stream: Arc<Mutex<Arc<RTCDataChannel>>>,
    state_notify: watch::Receiver<WebRTCConnectionState>,
    local_ice_rx: Arc<StdMutex<Option<mpsc::UnboundedReceiver<String>>>>,
    session_key: String,
    send_timeout: u64,
    // 以仅 Relay 的 ICE 策略（force_relay）构建：每个被选中的候选对都经由 TURN。
    relay_only: bool,
    // 分离的数据通道，在首次 `detach()` 之后缓存，这样 send/recv 不必反复加锁，
    // 也不必为每条消息重新获取。所有克隆共享它；`detach()` 是幂等的。
    detached: Arc<Mutex<Option<Arc<DetachedDataChannel>>>>,
    // 在多个克隆之间串行化一条完整的逻辑消息。每个分片都是一条独立的 SCTP
    // 消息，因此只对单次写入做串行化会让两条大消息交错。
    send_gate: Arc<Semaphore>,
    // 放在 Arc 之后而不是放在 future 里，这样被取消的 `next()` 也能保留部分消息。
    // 仅允许单一读取者：重组跨越多次调用，而分片头不携带长度或
    // 序列号，因此两个读取者会把无关的分片拼接成一条错误却看似合法的消息。
    recv_state: Arc<Mutex<RecvState>>,
    // 当控制器完成 RustDesk 身份绑定（DTLS 指纹
    // 通过 `set_key` 与已签名的对端 id 匹配）后为 true。DTLS 始终加密；该标志与 TCP 的
    // 「密钥交换后即安全」保持一致，从而不会把无密钥/未绑定的 WebRTC 显示为已完成对端鉴权。
    peer_verified: Arc<AtomicBool>,
    // 是否还有人需要这个 pc——参见 `HandoffState`。由每个克隆共享，包括
    // `SESSIONS` 条目，因为这个问题问的是 pc，而不是某一个句柄。
    handoff: Arc<HandoffState>,
    // 在构造时取一次：`pc.local_description()` 会随每个收集到的 candidate 增长，
    // 而它不能增长（原因见 `UDP_ENDPOINT_BUDGET`）。共享起来，这样克隆该结构体依然廉价。
    local_endpoint: Arc<String>,
    // 统计到达的分片，`recv_state` 无法回答这个问题：那把锁是刻意
    // 在整个重组过程中一直持有（见该字段），因此采样器会在其上阻塞。
    rx_probe: RxProbe,
    // ICE 已停止收到对端数据，但连接尚未进入终止状态。刻意不放进
    // `state_notify`——参见它被设置的位置。
    disconnected: Arc<AtomicBool>,
    // 该流将要重组的最大消息长度，在对端未鉴权期间会被调低。
    // 由每个克隆共享，包括 `SESSIONS` 条目，因为接收路径是运行在
    // 某个克隆之上的，而这个上限针对的是 pc，而不是某一个句柄。
    max_recv_message: Arc<AtomicUsize>,
}

/// `new_inner` 构建出的 pc 是否仍被需要，由发出的每个 `NewStreamHandoff` 共享
/// 。`new_inner` 会在它的 `new()` 返回之前缓存 pc，因此 builder 的 handoff 与任何
/// 数量的缓存命中会同时在途；只有当它们全部
/// 被丢弃且无一被接管时，pc 才算被放弃，而在任何单个 Drop 上关闭都会杀死某个存活调用方的会话。
#[derive(Default)]
struct HandoffState {
    // 已发出、但既未被接管也未被丢弃的 handoff 数量。在 `SESSIONS`
    // 锁内递增，这样仍在到达过程中的命中不会被下面的放弃关闭抢先。
    pending: AtomicUsize,
    // 由第一次 `into_inner` 锁定：某个调用方已拥有该流，而由于 `WebRTCStream`
    // 没有 `Drop`，没有任何东西能取消它的所有权。因此之后任何命中的放弃都只是
    // 空操作，这正是让缓存的 pc 在调用方被取消后依然存活的原因。
    adopted: AtomicBool,
    // 在放弃关闭提交后于 `SESSIONS` 锁内设置，因此之后才加锁命中该 map 的
    // （驱逐只会更晚发生，来自状态处理器）会拒绝该条目，而不是
    // 接管一个已经要被丢弃的 pc。
    closing: AtomicBool,
}

#[derive(Default)]
struct RecvState {
    // 当前正在重组的逻辑消息的已累积负载。只有多分片
    // 消息才会到达这里；完整到达的消息会直接从 `scratch` 中切分出来。
    acc: BytesMut,
    // 读缓冲，按 `SCRATCH_REFILL` 分块补充而不是每次读取都补充。整条消息
    // 分片通过 `split_to` 交给调用方，它共享这块内存而不是
    // 复制，因此缓冲区从头部被消耗，只有当剩余部分
    // 再也装不下一个分片时才重新初始化——把这笔开销摊到许多小消息上。
    scratch: BytesMut,
}

// SCTP 数据通道 65536 字节的最大消息长度通过以下方式处理：
// 把逻辑消息拆分成带 1 字节头的分片。分片负载被保持在
// 远低于该上限，因此头+负载永远不会达到那个正好 65536 的边界，而接收方
// 的重组会在那里截断并丢数据。
const MAX_FRAGMENT_PAYLOAD: usize = 60000;
/// 接收暂存区大小：必须 >= 1（分片头）+ `MAX_FRAGMENT_PAYLOAD`，并且要能容纳
/// 协商出的 SCTP 最大消息长度。
const RECV_BUF_SIZE: usize = 64 * 1024;
/// 每次初始化多少读缓冲。完整消息会从中直接切分出来而不需要
/// 复制，因此它是被消耗而不是被复用；分块补充能把唯一剩下的开销
/// （清零）摊到每个装得下的小消息上。交给调用方的切片会保留
/// 整个分块，因此这是用有界的保留量来换取免于复制。
const SCRATCH_REFILL: usize = 4 * RECV_BUF_SIZE;
/// 分片头字节：这条逻辑消息后面还有更多分片。
const FRAG_MORE: u8 = 1;
/// 分片头字节：一条逻辑消息的最后一个（或唯一）分片。
const FRAG_END: u8 = 0;
/// 该传输将要发送或重组的最大逻辑消息长度，也是一个
/// 未鉴权对端能让接收方占用的内存量。刻意与 TCP 保持一致：一个会话
/// 会在两条路径之间切换，因此针对某一传输设置的上限会在另一条路径上要了它的命。
const MAX_RECV_MESSAGE: usize = crate::bytes_codec::MAX_FRAME_LENGTH;
// 是四个网络，而不是四个名字：webrtc-ice 用各自的 socket 查询每个 URL，因此
// 共用同一主机的条目并不能带来冗余——这个列表原先带的两个就是一起失败的，在同一
// 毫秒内，在一条通往那个主机的路由已断的对端上。两个 anycast、两个 unicast；443
// 那一条是给不放开其他 UDP 端口的网络用的。
static DEFAULT_ICE_SERVERS: [&str; 4] = [
    "stun:stun.cloudflare.com:3478",
    "stun:stun.l.google.com:19302",
    "stun:stun.antisip.com:3478",
    "stun:stun.nextcloud.com:443",
];

lazy_static::lazy_static! {
    static ref SESSIONS: Arc::<Mutex<HashMap<String, WebRTCStream>>> = Default::default();

    // 拥有全部 WebRTC I/O 的进程级运行时。pc 的 UDP socket 在其中注册，
    // 而它的 ICE/DTLS/SCTP 泵任务也生成在该运行时上——这个运行时在
    // `new_inner` 及其安装的处理器执行期间是当前有效的；如果它是某个会话的
    // `#[tokio::main]` 运行时（io_loop 返回的瞬间就被 drop），那么这些 socket 和泵
    // 会随之消亡，连在别处驱动的关闭也会「成功」，却从未把
    // close_notify 放到链路上——于是对端只能等 ICE 衰减结束。因此 `new()` 在这里执行其函数体，
    // 分离的关闭也在这里运行：作为独立任务（没有截止时间的 `close()`
    // 不能头阻塞其他会话的拆除）且永不被取消（在
    // `RTCPeerConnection::close` 锁定 `is_closed` 之后被取消的关闭无法重试，会让
    // pc 滞留在 SESSIONS 中）。数据面的 future（send/recv/close）可以在任何调用方的
    // 运行时上被 poll——跨运行时 poll 没问题；只有拥有它的驱动者必须保持存活。用两个
    // 工作线程，这样一个繁忙的会话不会饿死其余会话。这是对 AGENTS.md 中
    // 「库中不得有运行时」这条规则唯一一处有意的例外：一个惰性构建、用于 I/O 且
    // 生命周期超过任何调用方的运行时。
    static ref WEBRTC_RT: Option<tokio::runtime::Runtime> = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("webrtc-io")
        .enable_all()
        .build()
        .map_err(|err| log::error!("failed to build the WebRTC I/O runtime: {}", err))
        .ok();
}

/// 把 `new_inner`（在 `WEBRTC_RT` 上分离运行）产生的流交回它的 `new()`
/// 调用方。如果该调用方在 await 期间被取消，运行时就会丢弃该任务的输出——
/// 就是这个 guard——而且一个没人应答的 offerer 自身永远不会进入终止性 ICE 状态，所以
/// 没有这个 Drop 关闭，它的 pc、socket 和泵任务就会永远留在 SESSIONS 中。认领
/// 在共享的 `HandoffState` 中计数，因此只有最后一个在流仍未被接管时被丢弃的认领
/// 才会关闭它；已被其他调用方使用的命中则不予处理。
struct NewStreamHandoff {
    stream: Option<WebRTCStream>,
}

impl NewStreamHandoff {
    /// 对 `stream` 取得一个认领。必须在 `SESSIONS` 锁内调用：在该 map
    /// 解锁之后才登记的认领，可能落在最后一个未结认领的放弃关闭之后。
    fn claim(stream: WebRTCStream) -> Self {
        stream.handoff.pending.fetch_add(1, Ordering::AcqRel);
        Self {
            stream: Some(stream),
        }
    }

    fn into_inner(mut self) -> Option<WebRTCStream> {
        let stream = self.stream.take()?;
        // 在释放认领之前，否则并发的 Drop 可能看到计数归零，却没看到
        // 这个调用方已接管所有权。
        stream.handoff.adopted.store(true, Ordering::Release);
        stream.handoff.pending.fetch_sub(1, Ordering::AcqRel);
        Some(stream)
    }
}

impl Drop for NewStreamHandoff {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let last = stream.handoff.pending.fetch_sub(1, Ordering::AcqRel) == 1;
            if last && !stream.handoff.adopted.load(Ordering::Acquire) {
                stream.close_if_abandoned();
            }
        }
    }
}

impl Clone for WebRTCStream {
    fn clone(&self) -> Self {
        WebRTCStream {
            pc: self.pc.clone(),
            stream: self.stream.clone(),
            state_notify: self.state_notify.clone(),
            local_ice_rx: self.local_ice_rx.clone(),
            session_key: self.session_key.clone(),
            send_timeout: self.send_timeout,
            relay_only: self.relay_only,
            detached: self.detached.clone(),
            send_gate: self.send_gate.clone(),
            recv_state: self.recv_state.clone(),
            peer_verified: self.peer_verified.clone(),
            handoff: self.handoff.clone(),
            local_endpoint: self.local_endpoint.clone(),
            rx_probe: self.rx_probe.clone(),
            disconnected: self.disconnected.clone(),
            max_recv_message: self.max_recv_message.clone(),
        }
    }
}

impl WebRTCStream {
    #[inline]
    fn get_remote_offer(endpoint: &str) -> ResultType<String> {
        // 确保 endpoint 以 "webrtc://" 开头
        if !endpoint.starts_with("webrtc://") {
            return Err(
                Error::new(ErrorKind::InvalidInput, "Invalid WebRTC endpoint format").into(),
            );
        }

        // 提取 Base64 编码的 SDP 部分
        let encoded_sdp = &endpoint["webrtc://".len()..];
        // 解码 Base64 字符串
        let decoded_bytes = BASE64_STANDARD
            .decode(encoded_sdp)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "Failed to decode Base64 SDP"))?;
        Ok(String::from_utf8(decoded_bytes).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "Failed to convert decoded bytes to UTF-8",
            )
        })?)
    }

    #[inline]
    fn sdp_to_endpoint(sdp: &str) -> String {
        let encoded_sdp = BASE64_STANDARD.encode(sdp);
        format!("webrtc://{}", encoded_sdp)
    }

    // 信封 JSON 中携带本地描述 ICE 传输策略的键，与
    // RTCSessionDescription 字段并列（参见 `local_endpoint`）。
    // 显式写出 `'static`：这里省略生命周期在 CI 固定的 1.75 工具链上是
    // 默认警告、未来会变成硬错误的写法（elided_lifetimes_in_associated_constant）。
    const ICE_POLICY_KEY: &'static str = "ice_policy";
    const ICE_POLICY_ALL: &'static str = "all";

    #[inline]
    fn get_key_for_sdp(sdp: &RTCSessionDescription) -> ResultType<String> {
        let binding = sdp.unmarshal()?;
        let Some(fingerprint) = binding.attribute("fingerprint") else {
            // 在媒体描述中查找 fingerprint 属性
            for media in &binding.media_descriptions {
                if media.media_name.media != "application" {
                    continue;
                }
                if let Some(fp) = media
                    .attributes
                    .iter()
                    .find(|x| x.key == "fingerprint")
                    .and_then(|x| x.value.clone())
                {
                    return Ok(fp);
                }
            }
            return Err(anyhow::anyhow!("SDP fingerprint attribute not found"));
        };
        Ok(fingerprint.to_string())
    }

    /// 进程内 SESSIONS map 的键：加上角色前缀的 DTLS 指纹。一个 offerer 和一个
    /// 共享同一指纹的 answerer（同一进程连接到自己的 id）否则
    /// 会发生冲突，把 offerer 当作 answerer 返回。链路层的 `session_key`
    /// 仍用于 ICE candidate 路由，保持为不带前缀的指纹，这样两个对端依然能匹配。
    #[inline]
    fn cache_key(fingerprint: &str, is_offerer: bool) -> String {
        format!(
            "{}:{}",
            if is_offerer { "offer" } else { "answer" },
            fingerprint
        )
    }

    /// 在 `on_data_channel` 内部拒绝一个数据通道，在那里直接关闭它并不
    /// 奏效：webrtc-rs 会先让该处理器完整跑完，然后 `handle_open` 才绑定 SCTP 流，
    /// 因此 `close()` 只是翻转就绪状态，之后 `handle_open` 又把它设回 Open。
    fn close_unbound_channel(dc: Arc<RTCDataChannel>) {
        let dc_for_close = dc.clone();
        dc.on_open(Box::new(move || {
            let dc = dc_for_close.clone();
            Box::pin(async move {
                if let Err(err) = dc.close().await {
                    log::debug!("failed to close rejected data channel: {}", err);
                }
            })
        }));
    }

    /// 缓存的 `SESSIONS` 条目是否可以交给要求 `force_relay` 的调用方：它
    /// 是为第一个调用方构建的，因此其 ICE 策略可能并不匹配，而状态处理器
    /// 驱逐得足够晚，死条目会滞留。查询和插入两处都必须使用它。
    fn is_reusable_for(&self, force_relay: bool) -> bool {
        self.relay_only == force_relay
            && !self.handoff.closing.load(Ordering::Acquire)
            // 这一个上 ICE 已停止收到对端数据。不是终止状态，因此其他任何地方都
            // 不会拒绝它，但把它交给正要判断存活状态的调用者，会让对方一开始就
            // 怀疑一个自己从未用过的 pc。
            && !self.is_disconnected()
            && !matches!(
                * self.state_notify.borrow(),
                WebRTCConnectionState::Closed(_)
            )
    }

    #[inline]
    fn get_key_for_sdp_json(sdp_json: &str) -> ResultType<String> {
        if sdp_json.is_empty() {
            return Ok("".to_string());
        }
        let sdp = serde_json::from_str::<RTCSessionDescription>(sdp_json)?;
        Self::get_key_for_sdp(&sdp)
    }

    fn remove_sessions_for_peer(
        sessions: &mut HashMap<String, WebRTCStream>,
        pc: &Arc<RTCPeerConnection>,
    ) -> Vec<String> {
        let keys: Vec<String> = sessions
            .iter()
            .filter_map(|(key, session)| {
                Arc::ptr_eq(&session.pc, pc).then(|| key.clone())
            })
            .collect();
        for key in &keys {
            sessions.remove(key);
        }
        keys
    }

    #[inline]
    async fn get_key_for_peer(pc: &Arc<RTCPeerConnection>, is_local: bool) -> ResultType<String> {
        let Some(desc) = (match is_local {
            true => pc.local_description().await,
            false => pc.remote_description().await,
        }) else {
            return Err(anyhow::anyhow!("PeerConnection description is not set"));
        };
        Self::get_key_for_sdp(&desc)
    }

    #[inline]
    fn get_ice_server_from_url(url: &str) -> Option<RTCIceServer> {
        let u = Url::parse(url).ok()?;
        if !matches!(u.scheme(), "turn" | "turns" | "stun" | "stuns") {
            return None;
        }
        // 两种被接受的写法解析结果不同：`turn://user:pass@host:port` 带 authority，
        // 也是唯一能携带凭据的形式；而 RFC 7065 的 `turn:host:port` 是
        // cannot-be-a-base，因此 `host_str()` 为 None，整个 `host:port` 都落到了 path 中。
        let (host, port) = match u.host_str() {
            Some(host) => (host.to_owned(), u.port().unwrap_or(3478)),
            None => Self::split_host_port(u.path())?,
        };
        if host.is_empty() {
            return None;
        }
        // `Url` 返回的 userinfo 是百分号编码的；TURN 服务器期望的是原始值，
        // 因此 `p%40ss` 必须以 `p@ss` 的形式送达，长期凭据才能校验通过。
        let username = percent_decode_str(u.username())
            .decode_utf8_lossy()
            .into_owned();
        let credential = percent_decode_str(u.password().unwrap_or_default())
            .decode_utf8_lossy()
            .into_owned();
        // 没有凭据的 TURN 服务器不只是无用：webrtc-rs 会校验每一个
        // 在 `new_peer_connection` 中配置的服务器，因此一个缺少凭据的条目会让**每一个**
        // peer connection 失败——包括那些根本不需要 TURN 的普通非中继连接。RFC
        // 7065 的写法没有地方放凭据，因此在这里丢弃这类条目，而不是
        // 让它们污染整份配置；`turn://user:pass@host:port` 才能携带凭据。
        if matches!(u.scheme(), "turn" | "turns") && (username.is_empty() || credential.is_empty())
        {
            log::warn!(
                "Ignoring TURN server without credentials: {}:{}:{} (use {}://user:pass@host:port)",
                u.scheme(),
                host,
                port,
                u.scheme()
            );
            return None;
        }
        Some(RTCIceServer {
            urls: vec![format!("{}:{}:{}", u.scheme(), host, port)],
            username,
            credential,
            ..Default::default()
        })
    }

    /// 从 RFC 7065 的 URL path 中切分出 `host[:port]`，并给端口默认值。加括号的 IPv6
    /// 字面量保留其方括号，这正是 webrtc-rs 重新解析时所用的形式。
    fn split_host_port(path: &str) -> Option<(String, u16)> {
        let rest = path.split(['?', '#']).next().unwrap_or_default();
        if rest.is_empty() {
            return None;
        }
        if let Some(after_open) = rest.strip_prefix('[') {
            let (host, tail) = after_open.split_once(']')?;
            let port = tail
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(3478);
            return Some((format!("[{host}]"), port));
        }
        // 不加括号的 IPv6 字面量没有无歧义的切分点——`2001:db8::1` 会在
        // 最后一个冒号处被切成主机 `2001:db8:` 和端口 `1`——因此这类地址要求使用加括号的形式，
        // 而不是输出一个 webrtc-ice 无法解析的主机。
        if rest.matches(':').count() > 1 {
            log::warn!("Ignoring ICE server {rest}: bracket IPv6 literals as [addr]:port");
            return None;
        }
        match rest.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => match port.parse() {
                Ok(port) => Some((host.to_owned(), port)),
                // 存在但不可用的端口是笔误，不是主机：把它折回去
                // 会产生 `host:99999:3478`，webrtc-ice 会直接拒绝它——连带着
                // 拖垮每一个 peer connection，而不只是这一个服务器。
                Err(_) => {
                    log::warn!("Ignoring ICE server {rest}: invalid port");
                    None
                }
            },
            _ => Some((rest.to_owned(), 3478)),
        }
    }

    /// ICE 配置中是否包含可用的 TURN 服务器。Relay 策略的 peer
    /// connection（force_relay）只能收集 relay candidate，因此没有 TURN 服务器时它
    /// 永远连不上——调用方用这一点来跳过构建一个注定失败的 pc。
    pub fn has_turn_server() -> bool {
        // 正是 `get_ice_server_from_url` 让只检查 scheme 就足够了：它会丢弃
        // 没有主机的条目和没有凭据的 TURN 条目，也就是那些
        // 在这里会回答 `true` 却又无法使用的条目——这比回答 `false` 更糟，因为
        // 调用方会听信我们的话而跳过「不要构建注定失败的仅 Relay pc」这道保护。
        Self::get_ice_servers().iter().any(|s| {
            s.urls
                .iter()
                .any(|u| u.starts_with("turn:") || u.starts_with("turns:"))
        })
    }

    #[inline]
    fn get_cc_enabled() -> bool {
        let k = config::keys::OPTION_ALLOW_WEBRTC_CC;
        config::option2bool(k, &config::Config::get_option(k))
    }

    #[inline]
    fn get_ice_servers() -> Vec<RTCIceServer> {
        Self::parse_ice_servers(&config::Config::get_option(
            config::keys::OPTION_ICE_SERVERS,
        ))
    }

    /// 默认的 UDP STUN 服务器，格式为裸的 `host:port`，用于应用层的地址探测。
    pub fn default_stun_servers() -> Vec<String> {
        DEFAULT_ICE_SERVERS
            .iter()
            .filter_map(|url| url.strip_prefix("stun:").map(str::to_owned))
            .collect()
    }

    /// 从 `get_ice_servers` 中拆出来，这样无需触碰
    /// 进程级、持久化到磁盘的选项就能测试解析——测试在同一个
    /// 进程的并行线程中运行，因此一个改写它的测试会与另一个测试正在构建的每个 peer connection 发生竞态。
    fn parse_ice_servers(cfg: &str) -> Vec<RTCIceServer> {
        let mut ice_servers = Vec::new();
        let mut has_stun = false;

        for url in cfg.split(',').map(str::trim) {
            if let Some(ice_server) = Self::get_ice_server_from_url(url) {
                // 在用户配置中检测 STUN
                if ice_server
                    .urls
                    .iter()
                    .any(|u| u.starts_with("stun:") || u.starts_with("stuns:"))
                {
                    has_stun = true;
                }

                ice_servers.push(ice_server);
            }
        }

        // 如果没有 STUN（只有 TURN 或配置为空）→ 在前面加上默认值
        if !has_stun {
            ice_servers.insert(
                0,
                RTCIceServer {
                    urls: DEFAULT_ICE_SERVERS.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            );
        }
        ice_servers
    }

    /// 响应方的 ICE agent 在某个候选对没有响应时发送多少个 binding request
    /// 才放弃它：按 200ms 的检查间隔，74 次约为 14.8 秒，刚好落在 15 秒
    /// 持久化窗口（libwebrtc 给候选对的 `CONNECTION_WRITE_TIMEOUT`）之内，因此会话两端
    /// 以浏览器端会话也会在差不多的时间里继续检查。ICE crate 的默认值 7
    /// （约 1.4 秒）是服务端的选择，假定对端会持续敲门；在
    /// 地址受限的 NAT 后面，这些请求同时也是让本端的映射对
    /// 远端的检查保持开放的原因，因此过早停止就等于对某个本来会
    /// 在路径疏通后连上的候选对关上了门，无论远端尝试多久。其上限由
    /// `CONNECT_TIMEOUT`（18 秒）限定。
    const ICE_MAX_BINDING_REQUESTS: u16 = 74;

    /// 在 `WEBRTC_RT` 上构建和驱动：pc 创建的每个 socket 和后台任务都必须
    /// 属于一个生命周期超过会话的运行时（参见 `WEBRTC_RT`）。一个在
    /// await 中途放弃这个 future 的调用方，会让设置任务在那里跑完；被放弃的结果随后
    /// 会关闭一个刚构建好的 pc（而共享的缓存命中则不动）——参见 `NewStreamHandoff`。
    pub async fn new(
        remote_endpoint: &str,
        force_relay: bool,
        ms_timeout: u64,
    ) -> ResultType<Self> {
        let Some(rt) = WEBRTC_RT.as_ref() else {
            return Err(anyhow::anyhow!("WebRTC I/O runtime unavailable"));
        };
        let remote_endpoint = remote_endpoint.to_owned();
        rt.spawn(Self::new_inner(remote_endpoint, force_relay, ms_timeout))
            .await
            .map_err(|err| anyhow::anyhow!("WebRTC setup task failed: {}", err))??
            .into_inner()
            .ok_or_else(|| anyhow::anyhow!("WebRTC setup handoff was empty"))
    }

    async fn new_inner(
        remote_endpoint: String,
        force_relay: bool,
        ms_timeout: u64,
    ) -> ResultType<NewStreamHandoff> {
        // 该 endpoint 含有 Base64 编码的 SDP，其中带有主机地址和实时的 ICE
        // 凭据。只记录它的大小，这样调试日志不会泄露这些信息。
        log::debug!(
            "New webrtc stream (remote endpoint: {} bytes)",
            remote_endpoint.len()
        );
        let remote_offer = if remote_endpoint.is_empty() {
            "".into()
        } else {
            Self::get_remote_offer(&remote_endpoint)?
        };

        let mut key = Self::get_key_for_sdp_json(&remote_offer)?;
        let start_local_offer = remote_offer.is_empty();
        if !key.is_empty() {
            // 在锁内认领：`close_if_abandoned` 也在该锁内提交，因此这个命中
            // 要么及时登记以取消那次关闭，要么发现该条目已被拒绝。
            let cached = {
                let sessions_lock = SESSIONS.lock().await;
                match sessions_lock.get(&Self::cache_key(&key, start_local_offer)) {
                    Some(session) if session.is_reusable_for(force_relay) => {
                        Some(NewStreamHandoff::claim(session.clone()))
                    }
                    Some(session) => {
                        log::debug!(
                            "Ignoring cached webrtc peer (relay_only {}, wanted {})",
                            session.relay_only,
                            force_relay
                        );
                        None
                    }
                    None => None,
                }
            };
            if let Some(handoff) = cached {
                log::debug!("Start webrtc with cached peer");
                return Ok(handoff);
            }
        }
        // 创建 SettingEngine 并启用 Detach
        let mut s = SettingEngine::default();
        s.detach_data_channels();
        s.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        // 仅响应方：本端是 offerer 时 `remote_endpoint` 为空。桌面
        // 控制器保持 crate 的默认值，以便在持久化实验运行于受控端时，它仍是一个
        // 不变的对照点。
        if !remote_endpoint.is_empty() {
            s.set_ice_max_binding_requests(Some(Self::ICE_MAX_BINDING_REQUESTS));
        }
        // fe80::/10 只能与 scope id 一起绑定，而 `IpAddr` 无法携带它，因此
        // 收集它永远得不到 candidate——只会得到一次绑定失败和每个地址一条警告。
        // 之所以写出来，是因为 `is_unicast_link_local` 在我们的 MSRV 上尚未稳定。
        s.set_ip_filter(Box::new(|ip: IpAddr| match ip {
            IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 != 0xfe80,
            IpAddr::V4(_) => true,
        }));

        // 默认使用 KCP 的 nc=1 配置，并且与
        // `allow-kcp-congestion-control` 一样可选择开启拥塞窗口，原因见 `get_kcp_cc_enabled`。按连接设置，
        // 这样该选项对下一个会话生效，而不是等到下次启动。
        webrtc::sctp::association::set_no_congestion_control(!Self::get_cc_enabled());

        // 创建 API 对象
        let api = APIBuilder::new().with_setting_engine(s).build();

        // 准备配置，从配置中获取 ICE 服务器
        let config = RTCConfiguration {
            ice_servers: Self::get_ice_servers(),
            ice_transport_policy: if force_relay {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            },
            ..Default::default()
        };

        let (notify_tx, notify_rx) = watch::channel(WebRTCConnectionState::Pending);
        let (ice_tx, ice_rx) = mpsc::unbounded_channel::<String>();
        // 创建新的 RTCPeerConnection
        let pc = Arc::new(api.new_peer_connection(config).await?);
        // `on_*` 处理器永远不会被清除——`close()` 不会，`SESSIONS` 驱逐也不会——因此
        // 这个发送者的存活期长于 pc，会让调用方的 ICE 转发器永远阻塞在 `recv()` 上，
        // 并把 pc 一起吊住。收集完成（`None`）是该 agent 在发生
        // ICE 重启（这里没做）之前的最后一句话，因此发送者也随之结束，而转发器
        // 在那里结束，而不是随会话结束；下面的终止状态处理器会丢弃这个闭包，
        // 以应对在到达那一步之前就死掉的 pc。
        let mut local_ice_tx = Some(ice_tx);
        pc.on_ice_candidate(Box::new(move |candidate| {
            let Some(candidate) = candidate else {
                local_ice_tx = None;
                return Box::pin(async {});
            };
            let Some(local_ice_tx) = local_ice_tx.clone() else {
                return Box::pin(async {});
            };
            Box::pin(async move {
                match candidate.to_json() {
                    Ok(candidate) => match serde_json::to_string(&candidate) {
                        Ok(candidate_json) => {
                            let _ = local_ice_tx.send(candidate_json);
                        }
                        Err(err) => {
                            log::warn!("failed to serialize local ICE candidate: {}", err);
                        }
                    },
                    Err(err) => {
                        log::warn!("failed to convert local ICE candidate to JSON: {}", err);
                    }
                }
            })
        }));

        let bootstrap_dc = if start_local_offer {
            let dc_open_notify = notify_tx.clone();
            // 创建一个标签为 "bootstrap" 的数据通道
            let dc = match pc.create_data_channel("bootstrap", None).await {
                Ok(dc) => dc,
                Err(e) => {
                    // 在向外传播之前先关闭：pc 还活着，否则会泄漏。
                    pc.close().await.ok();
                    return Err(e.into());
                }
            };
            dc.on_open(Box::new(move || {
                log::debug!("Local data channel bootstrap open.");
                let _ = dc_open_notify.send(WebRTCConnectionState::Open);
                Box::pin(async {})
            }));
            dc
        } else {
            // 等待远端对端创建数据通道
            // 这里创建一个虚拟数据通道以满足类型系统
            Arc::new(RTCDataChannel::default())
        };

        let stream = Arc::new(Mutex::new(bootstrap_dc));
        if !start_local_offer {
            // 注册数据通道创建的处理
            let dc_open_notify = notify_tx.clone();
            let stream_for_dc = stream.clone();
            // 远端可能打开任意数量的通道，但只有第一个会被绑定。
            let dc_bound = Arc::new(AtomicBool::new(false));
            pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let d_label = dc.label().to_owned();
                let dc_open_notify2 = dc_open_notify.clone();
                let stream_for_dc_clone = stream_for_dc.clone();
                let dc_bound = dc_bound.clone();
                Box::pin(async move {
                    // 重组只有在有序且完全可靠的通道上才正确：分片
                    // 头没有序列号，因此乱序或丢失会悄无声息地混合
                    // 消息。webrtc-rs 从未鉴权的远端 DCEP OPEN 中取用这些属性。
                    if !dc.ordered()
                        || dc.max_retransmits().is_some()
                        || dc.max_packet_lifetime().is_some()
                    {
                        log::warn!(
                            "Rejecting WebRTC data channel {}: not ordered and fully reliable",
                            d_label
                        );
                        Self::close_unbound_channel(dc);
                        return;
                    }
                    // 只绑定第一个通道。`detached` 缓存第一个分离句柄，
                    // 其生命周期与该流相同，因此重新绑定会让拆除过程去关闭一个
                    // send/recv 已经不再使用的通道；更糟的是，第二个通道的 `on_open`
                    // 会把 Open 推到同一个 watch 上，从而重新武装一个已经锁定为
                    // Closed 的会话——而那个 watch 同时把守 `send_bytes_inner` 和 `next()`。
                    if dc_bound.swap(true, Ordering::SeqCst) {
                        log::warn!("Ignoring extra WebRTC data channel {}", d_label);
                        Self::close_unbound_channel(dc);
                        return;
                    }
                    log::debug!("Remote data channel {} ready", d_label);
                    let mut stream_lock = stream_for_dc_clone.lock().await;
                    * stream_lock = dc.clone();
                    drop(stream_lock);
                    dc.on_open(Box::new(move || {
                        let _ = dc_open_notify2.send(WebRTCConnectionState::Open);
                        Box::pin(async {})
                    }));
                })
            }));
        }

        // 这会在对端连接/断开时通知你
        let stream_for_close = stream.clone();
        // 用弱引用而非强引用：存放在 pc 内部、捕获了强引用
        // `Arc<RTCPeerConnection>` 的处理器会形成 pc -> 内部 -> 处理器 -> pc 的环，而 `close()`
        // 从不打破它（它只是触发处理器），也没有 `Drop` 去清理，会永久泄漏每一个
        // pc 以及 ICE candidate 发送者的转发任务。在处理器内部再升级；如果该 pc
        // 已经不在了，SESSIONS 里就没有可驱逐的东西了。
        let pc_for_close = Arc::downgrade(&pc);
        // 刻意**不**做成 `WebRTCConnectionState` 的一个变体：该枚举把守发送和接收的
        // 就绪状态（`wait_for_connect_result`）以及 session 缓存的可接纳性，因此第四个状态
        // 必须在这两处都被赋予含义。`Disconnected` 是暂时的，不能
        // 改变其中任何一个，因此在它们旁边单独上报，而不是通过它们上报。
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnected_for_state = disconnected.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let stream_for_close2 = stream_for_close.clone();
            let on_connection_notify = notify_tx.clone();
            let pc_for_close2 = pc_for_close.clone();
            let disconnected_for_state = disconnected_for_state.clone();
            Box::pin(async move {
                log::debug!("WebRTC session peer connection state: {}", s);
                Self::note_health(&disconnected_for_state, s);
                match s {
                    // `Disconnected` 是一种暂时的、可恢复的 ICE 状态（webrtc-ice 在
                    // 约 5 秒收不到 consent 时触发，并在流量恢复后回到 `Connected`）。
                    // 只在终止状态下才拆除，这样短暂的网络抖动（Wi-Fi 漫游、
                    // 睡眠/唤醒、基站切换）不会永久杀死一个已建立的会话。
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        let _ =
                            on_connection_notify.send(WebRTCConnectionState::Closed(s.to_string()));
                        log::debug!("WebRTC session closing due to {}", s);
                        let stream = stream_for_close2.lock().await.clone();
                        let _ = stream.close().await;
                        log::debug!("WebRTC session stream closed");

                        let Some(pc_for_close2) = pc_for_close2.upgrade() else {
                            return;
                        };

                        // 没有任何其他地方会丢弃这个发送者（`close()` 不会清除任何处理器），因此
                        // 要通过替换处理器来关闭该通道：调用方的转发循环随后会
                        // 结束，而不是阻塞在 `recv()` 上把这个 pc 一直吊着。
                        pc_for_close2.on_ice_candidate(Box::new(|_| Box::pin(async {})));

                        // 按 pc 的身份判断，而不是重新推导键：重复的 offer 会与
                        // 存活的那个共享同一个键，而这里绝不能把后者驱逐掉。
                        let mut sessions_lock = SESSIONS.lock().await;
                        for key in Self::remove_sessions_for_peer(
                            &mut sessions_lock,
                            &pc_for_close2,
                        ) {
                            log::debug!("WebRTC session removed key: {}", key);
                        }
                    }
                    _ => {}
                }
            })
        }));

        // Trickle ICE：只做本地工作，不等待收集——受控端在
        // 打洞响应的关键路径上内联等待 answer 创建。下面的失败会留下一个存活的 pc，
        // 而它的状态处理器只在终止性 ICE 状态时才触发，因此在向外传播之前先关闭。
        // 在 `set_local_description` 之前对每个 endpoint 编码，正是它启动了收集，因此
        // `trickle_endpoint` 一开始就没有东西可以剥离。
        let offer_answer: ResultType<(String, String)> = async {
            if start_local_offer {
                let sdp = pc.create_offer(None).await?;
                let endpoint = Self::trickle_endpoint(&sdp, force_relay)?;
                pc.set_local_description(sdp.clone()).await?;
                // SDP 带有 host/srflx 地址和 ICE ufrag/pwd；只记录它的大小，不记录内容。
                log::debug!(
                    "local offer SDP built ({} bytes, endpoint {} bytes)",
                    sdp.sdp.len(),
                    endpoint.len()
                );
                let k = Self::get_key_for_sdp(&sdp)?;
                log::debug!("Start webrtc with local key: {}", k);
                Ok((k, endpoint))
            } else {
                let sdp = serde_json::from_str::<RTCSessionDescription>(&remote_offer)?;
                pc.set_remote_description(sdp.clone()).await?;
                let answer = pc.create_answer(None).await?;
                let endpoint = Self::trickle_endpoint(&answer, force_relay)?;
                pc.set_local_description(answer).await?;
                log::debug!(
                    "remote offer SDP received ({} bytes), local answer endpoint {} bytes",
                    sdp.sdp.len(),
                    endpoint.len()
                );
                let k = Self::get_key_for_sdp(&sdp)?;
                log::debug!("Start webrtc with remote key: {}", k);
                Ok((k, endpoint))
            }
        }
        .await;
        let (new_key, local_endpoint) = match offer_answer {
            Ok(x) => x,
            Err(e) => {
                pc.close().await.ok();
                return Err(e);
            }
        };
        // 只有 UDP 这条链路有上限，而承载它的是哪条链路由 rendezvous 服务器
        // 决定，因此这里只做上报而不拒绝：走 TCP/WS 路径时更大的 endpoint
        // 也能顺利送达，在这里失败只会白白葬送 WebRTC。
        if local_endpoint.len() > Self::UDP_ENDPOINT_BUDGET {
            log::warn!(
                "WebRTC endpoint is {} bytes, over the {} byte UDP budget; a peer reached over UDP \
                 may never receive it",
                local_endpoint.len(),
                Self::UDP_ENDPOINT_BUDGET
            );
        }
        key = new_key;

        let webrtc_stream = Self {
            pc,
            stream,
            state_notify: notify_rx,
            local_ice_rx: Arc::new(StdMutex::new(Some(ice_rx))),
            session_key: key.clone(),
            send_timeout: ms_timeout,
            relay_only: force_relay,
            detached: Arc::new(Mutex::new(None)),
            send_gate: Arc::new(Semaphore::new(1)),
            recv_state: Arc::new(Mutex::new(RecvState::default())),
            peer_verified: Arc::new(AtomicBool::new(false)),
            handoff: Default::default(),
            local_endpoint: Arc::new(local_endpoint),
            rx_probe: RxProbe::new(),
            disconnected,
            max_recv_message: Arc::new(AtomicUsize::new(MAX_RECV_MESSAGE)),
        };
        // 插入 session 缓存，但绝不要在持有该锁时 `await pc.close()`：
        // `close()` 会就地触发 peer-connection-state 处理器，而该处理器自身会锁 SESSIONS，
        // 从而让整个进程自死锁。任何重复项都要在锁外解决。
        let cache_key = Self::cache_key(&key, start_local_offer);
        // 在锁内认领，插入也包括在内：如果 pc 已缓存但未被认领，一个命中
        // 可能取走它唯一的认领又把它释放，从而在这个调用方脚下把它关掉。
        let mut duplicate = None;
        let handoff = {
            let mut final_lock = SESSIONS.lock().await;
            // 与上面的查询相同的可接纳性判断，否则那个查询就是死代码：一个
            // 在那里被拒绝的条目到这里时仍在 map 中，因此无条件返回它
            // 就会恰恰因为缓存的那个不可用而丢弃我们刚构建的 pc。
            match final_lock.get(&cache_key) {
                Some(session) if session.is_reusable_for(force_relay) => {
                    duplicate = Some(webrtc_stream.clone());
                    NewStreamHandoff::claim(session.clone())
                }
                _ => {
                    final_lock.insert(cache_key, webrtc_stream.clone());
                    NewStreamHandoff::claim(webrtc_stream)
                }
            }
        };
        if let Some(loser) = duplicate {
            // 并发的 `new()` 已经缓存了一个等价的流；丢弃这个 pc 的
            // 资源（在关闭线程上——在这里 await 会在这个
            // `new()` 于关闭中途被取消时搁浅 pc），并返回缓存的那个。
            loser.close_detached();
        }
        Ok(handoff)
    }

    /// 一次性 endpoint：等待 ICE 收集完成，使 SDP 已经带上 candidates。
    /// 这个等待刻意没有上限——这里的截止时间会返回只收集了一半的 SDP，
    /// 从而破坏约定；需要截止时间的调用方应当自己包装这次调用或改用 trickle 变体，
    /// rustdesk 的两条信令路径都这么做。剩余的调用方是回环测试和
    /// `examples/webrtc.rs`，两者都不设上限。
    #[inline]
    pub async fn get_local_endpoint(&self) -> ResultType<String> {
        // 保持原有的一次性 endpoint 约定：只交换这份 SDP 的调用方
        // 没有针对 `take_local_ice_rx` 的独立路径，因此它们的 endpoint 必须包含
        // 已收集到的 host/srflx/relay candidates。
        let mut gather_complete = self.pc.gathering_complete_promise().await;
        let _gathering_channel_closed = gather_complete.recv().await;
        let Some(local_desc) = self.pc.local_description().await else {
            return Err(anyhow::anyhow!("Local desc is not set"));
        };
        Self::encode_endpoint(local_desc.sdp_type, &local_desc.sdp, self.relay_only)
    }

    /// 与创建时完全一致的 offer/answer，供那些通过
    /// `take_local_ice_rx` 传递 candidate 的调用方使用。在构造时取得，因此它不会失败也不会阻塞——不同于
    /// `get_local_endpoint`，后者读取实时描述并等待收集完成。
    ///
    /// 刻意不使用 `pc.local_description()`，它会执行 `populate_local_candidates`，因而
    /// 返回 SDP 加上截至那一刻收集到的每个 candidate——而调用方是在
    /// `new` 之后经过一个网络往返才读取它，那时收集早已把它填满。为什么大小
    /// 很重要：`UDP_ENDPOINT_BUDGET`。
    #[inline]
    pub fn local_endpoint(&self) -> &str {
        self.local_endpoint.as_str()
    }

    /// trickle endpoint 必须小于的大小，才能保证其路径的 UDP 链路安全。它是
    /// 唯一会增长的信令消息：注册在 UDP 上的对端会在
    /// `PunchHole`（带着变形后的地址和权限数据）内以单个数据报的形式收到它，
    /// 而需要 IP 分片的会在丢弃分片的路径上无声无息地被丢掉。
    /// 随后到达的 candidates 各自只是一条小消息，永远不会接近这个大小。
    ///
    /// 这不是 MTU 计算：`PunchHole` 的其他字段在这里并没有上限，因此接近
    /// 这个值的消息仍然可能分片。它是一个绊线，监视着 `trickle_endpoint` 固定在
    /// 约 700 字节的量，这个大小既留出了余量，又仍能捕获让
    /// candidates 混回来的回归——以一行日志的形式报告，而不是又一次静默丢弃。没有任何机制强制它：
    /// TCP/WS 链路没有包大小上限。
    const UDP_ENDPOINT_BUDGET: usize = 1024;

    /// trickling 对端所信令的 endpoint：启动 ICE 和 DTLS 所需的会话参数
    /// （ice-ufrag、ice-pwd、fingerprint、setup、sctp m-line），不含 candidates——那些
    /// 通过 `take_local_ice_rx` 送出，并以一条条 `IceCandidate` 消息到达。
    ///
    /// candidates 是本地描述中唯一会增长的部分。把它们剥离掉就让
    /// 大小成为值本身的属性，而不是取值时刻的属性，因此它不会随
    /// 收集过程而漂移，无论以后如何重构。
    fn trickle_endpoint(local_desc: &RTCSessionDescription, relay_only: bool) -> ResultType<String> {
        let mut sdp = String::with_capacity(local_desc.sdp.len());
        for line in local_desc.sdp.lines() {
            // `end-of-candidates` 会告诉远端 agent 不要再等待 trickle。
            if line.starts_with("a=candidate:") || line.starts_with("a=end-of-candidates") {
                continue;
            }
            sdp.push_str(line);
            sdp.push_str("\r\n");
        }
        Self::encode_endpoint(local_desc.sdp_type, &sdp, relay_only)
    }

    /// 本地描述的 Base64 信封，除非该 pc 是
    /// 仅 Relay 的（参见 `ICE_POLICY_KEY`），否则会携带其 ICE 传输策略。
    /// 由会话描述所序列化的两个字段构建，而不是由该值本身构建，因此
    /// 改写了 SDP 的调用方不会让一棵过时的 `parsed` 树跟着一起走。
    fn encode_endpoint(sdp_type: RTCSdpType, sdp: &str, relay_only: bool) -> ResultType<String> {
        let mut envelope = serde_json::Map::new();
        envelope.insert("type".to_owned(), serde_json::to_value(sdp_type)?);
        envelope.insert("sdp".to_owned(), serde_json::Value::from(sdp));
        if !relay_only {
            // 它随信封走，而不是作为一个 rendezvous 服务器会转发的 proto 字段，
            // 因为它是 offer 自身的属性：接收方必须知道
            // force_relay 是策略（保持仅 Relay）还是传输方式。旧版对端会忽略它。
            envelope.insert(
                Self::ICE_POLICY_KEY.to_owned(),
                serde_json::Value::from(Self::ICE_POLICY_ALL),
            );
        }
        Ok(Self::sdp_to_endpoint(&serde_json::to_string(&envelope)?))
    }

    /// 对端的 endpoint 是否声明它是以 ICE 传输策略 `all`
    /// （W3C RTCIceTransportPolicy）构建的，即它会收集 host/srflx/relay candidates 并且直连
    /// 候选对可能形成，即使请求中带有 force_relay。键缺失、格式不符
    /// 或解析失败都表示「未声明」——也就是旧有的仅 Relay 解读。
    pub fn endpoint_declares_all_ice(endpoint: &str) -> bool {
        let Ok(sdp_json) = Self::get_remote_offer(endpoint) else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&sdp_json)
            .ok()
            .and_then(|v| {
                v.get(Self::ICE_POLICY_KEY)?
                    .as_str()
                    .map(|p| p == Self::ICE_POLICY_ALL)
            })
            .unwrap_or(false)
    }

    #[inline]
    pub async fn set_remote_endpoint(&self, endpoint: &str) -> ResultType<()> {
        let offer = Self::get_remote_offer(endpoint)?;
        log::debug!("WebRTC set remote sdp ({} bytes)", offer.len());
        let sdp = serde_json::from_str::<RTCSessionDescription>(&offer)?;
        self.pc.set_remote_description(sdp).await?;
        Ok(())
    }

    /// 本地描述的 DTLS 证书指纹（该 endpoint 自己的证书）。
    #[inline]
    pub async fn local_dtls_fingerprint(&self) -> ResultType<String> {
        Self::get_key_for_peer(&self.pc, true).await
    }

    /// 远端描述的 DTLS 证书指纹（对端的证书）。webrtc-rs
    /// 在 DTLS 握手期间用这个指纹校验协商出的对端证书，
    /// 因此一旦通道打开，匹配的指纹就标识出对端的证书。
    #[inline]
    pub async fn remote_dtls_fingerprint(&self) -> ResultType<String> {
        Self::get_key_for_peer(&self.pc, false).await
    }

    /// 连接是否经由 TURN 中继；用于 UI 的直连/中继标志。
    /// 在 Relay 策略下答案由构造方式决定，因此该分支直接返回 `Some(true)`，
    /// 不查询 ICE——包括在选出候选对之前，而另一个分支返回的是 `None`。
    /// 两个调用方都是在连接之后询问，那时两个分支结果一致；让 relay
    /// 分支去等待候选对只会多一次 stats 往返。
    pub async fn is_relayed(&self) -> Option<bool> {
        if self.relay_only {
            return Some(true);
        }
        let dtls = self.pc.sctp().transport();
        let pair = dtls.ice_transport().get_selected_candidate_pair().await?;

        // 不是 stats 报告里的 `nominated` 标志：按检查清单条目，webrtc-ice 从不清除它，
        // 因此在候选对切换后会有多个条目带着它，map 的顺序决定了答案。
        // 0.13 把 candidates 保持为私有，因此按位置从 `Display` 中读出类型。
        let relay = RTCIceCandidateType::Relay.to_string();
        Some(
            pair.to_string()
                .split(" <-> ")
                .filter_map(|side| side.split_whitespace().nth(2))
                .any(|typ| typ == relay),
        )
    }

    /// 被选中的候选对是否通过 IPv6 到达对端——选出之前为 `None`。
    /// 刻意取远端一侧：它才是对端实际被到达的地址，而
    /// 会话通常据以识别的 rendezvous 观测地址无法报告这一点。
    pub async fn is_remote_ipv6(&self) -> Option<bool> {
        let dtls = self.pc.sctp().transport();
        let pair = dtls.ice_transport().get_selected_candidate_pair().await?;
        // 与 `is_relayed` 相同的 `Display` 解析，只是多一个 token：一侧渲染为
        // `(remote) <protocol> <typ> <address>:<port>`，而 `protocol` 只有 udp/tcp，因此
        // 地址族只能从地址本身推断——只有 IPv6 字面量带两个冒号。
        let pair = pair.to_string();
        let remote = pair.split(" <-> ").nth(1)?;
        Some(remote.split_whitespace().nth(3)?.matches(':').count() > 1)
    }

    #[inline]
    pub fn take_local_ice_rx(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.local_ice_rx.lock().ok().and_then(|mut rx| rx.take())
    }

    #[inline]
    pub async fn add_remote_ice_candidate(&self, candidate_json: &str) -> ResultType<()> {
        if candidate_json.is_empty() {
            return Ok(());
        }
        let candidate = serde_json::from_str::<RTCIceCandidateInit>(candidate_json)?;
        self.pc.add_ice_candidate(candidate).await?;
        Ok(())
    }

    #[inline]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    pub async fn wait_connected(&mut self, ms: u64) -> ResultType<()> {
        if ms > 0 {
            match timeout(Duration::from_millis(ms), self.wait_for_connect_result()).await {
                Ok(result) => result?,
                Err(_) => return Err(anyhow::anyhow!("WebRTC wait_connected timeout")),
            }
        } else {
            self.wait_for_connect_result().await?;
        }
        Ok(())
    }

    /// 显式拆除 peer connection。仅 drop 句柄是不够的：`SESSIONS`
    /// 持有一个克隆，因此 pc 及其 ICE/DTLS 资源会一直存活，直到它碰巧进入
    /// 终止性 ICE 状态。关闭会触发状态处理器，它会驱逐 `SESSIONS` 条目——
    /// 放弃某个流的调用方（例如在传输竞态中落败的 offerer）必须调用它。
    #[inline]
    pub async fn close(&self) {
        self.pc.close().await.ok();
    }

    /// 在 `WEBRTC_RT` 上作为独立任务拆除 pc——这个运行时拥有
    /// pc 的 socket 和泵任务，因此无论哪些
    /// 调用方运行时已经消亡，关闭都能真正到达链路上——同时让 `keep` 存活到拆除完成。可从
    /// 任何上下文调用，包括在没有运行时的线程上执行 `Drop`，而且永不被取消，因此
    /// `close()` 的 `is_closed` 锁定只会被真正完成的关闭设置。发送路径
    /// 把自己的 permit 作为 `keep` 传下去。
    pub fn close_detached_with<T: Send + 'static>(&self, keep: T) {
        // 如果运行时根本没有构建出来，那 pc 也不存在（`new` 会先失败）；没什么可关闭的。
        let Some(rt) = WEBRTC_RT.as_ref() else {
            return;
        };
        let pc = self.pc.clone();
        rt.spawn(async move {
            if let Err(err) = pc.close().await {
                log::debug!("WebRTC close failed: {}", err);
            }
            drop(keep);
        });
    }

    #[inline]
    pub fn close_detached(&self) {
        self.close_detached_with(());
    }

    /// 关闭一个从未被任何调用方接管的 pc。像 `close_detached` 一样在 `WEBRTC_RT` 上生成，但
    /// 这个决定要在缓存命中认领所用的那把 `SESSIONS` 锁下重新做出：要么该命中
    /// 先登记而本次调用退让，要么 `closing` 先提交，
    /// 由 `is_reusable_for` 拒绝该条目，而状态处理器只是更晚才驱逐它。
    fn close_if_abandoned(&self) {
        let Some(rt) = WEBRTC_RT.as_ref() else {
            return;
        };
        let stream = self.clone();
        rt.spawn(async move {
            {
                let _sessions_lock = SESSIONS.lock().await;
                if stream.handoff.pending.load(Ordering::Acquire) != 0
                    || stream.handoff.adopted.load(Ordering::Acquire)
                {
                    return;
                }
                stream.handoff.closing.store(true, Ordering::Release);
            }
            // 在锁外：`close()` 会就地触发状态处理器，而该处理器会锁 SESSIONS。
            if let Err(err) = stream.pc.close().await {
                log::debug!("WebRTC close of an abandoned peer failed: {}", err);
            }
        });
    }

    #[inline]
    pub fn set_raw(&mut self) {
        // not-supported
    }

    /// 参见 `Stream::set_max_packet_length`。会被钳制到 `MAX_RECV_MESSAGE`：发送路径是按
    /// 该常量检查的，因此高于它的上限只会接受没有任何对端会发送的内容。
    #[inline]
    pub fn set_max_packet_length(&mut self, n: usize) {
        self.max_recv_message
            .store(n.min(MAX_RECV_MESSAGE), Ordering::Relaxed);
    }

    #[inline]
    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    }

    #[inline]
    pub fn set_send_timeout(&mut self, ms: u64) {
        self.send_timeout = ms;
    }

    /// 参见 `Stream::rx_progress`。
    #[inline]
    pub fn rx_progress(&self) -> u64 {
        self.rx_probe.get()
    }

    /// 跟踪 ICE 是否仍能收到对端数据。`Disconnected` 在停止后约 5 秒
    /// 被置起，流量恢复时又被清除，因此它只是一个提示，由提出询问的人
    /// 自行处理，而绝不是终止状态；终止状态由本函数的调用方处理，而它
    /// 刻意不去动这个标志，因为已关闭的会话用不上提示。
    fn note_health(disconnected: &AtomicBool, s: RTCPeerConnectionState) {
        match s {
            RTCPeerConnectionState::Disconnected => disconnected.store(true, Ordering::SeqCst),
            RTCPeerConnectionState::Connected => disconnected.store(false, Ordering::SeqCst),
            _ => {}
        }
    }

    /// 参见 `Stream::webrtc_disconnected`。
    #[inline]
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn set_key(&mut self, _key: Key) {
        // 无论怎样 WebRTC 流量都由 DTLS 加密；secretbox 密钥并未使用。
        // 调用方只有在控制器已把 DTLS 指纹绑定到
        // 已验证的对端身份（或受控端已完成对应的握手）之后才调用 set_key。
        // 标记为已完成对端验证，使 is_secured() 与 TCP 在密钥交换之后的含义一致。
        self.peer_verified.store(true, Ordering::Release);
    }

    #[inline]
    pub fn is_secured(&self) -> bool {
        self.peer_verified.load(Ordering::Acquire)
    }

    #[inline]
    pub async fn send(&mut self, msg: &impl Message) -> ResultType<()> {
        self.send_raw(msg.write_to_bytes()?).await
    }

    #[inline]
    pub async fn send_raw(&mut self, msg: Vec<u8>) -> ResultType<()> {
        self.send_bytes(Bytes::from(msg)).await
    }

    #[inline]
    async fn wait_for_connect_result(&mut self) -> ResultType<()> {
        loop {
            match self.state_notify.borrow().clone() {
                WebRTCConnectionState::Open => return Ok(()),
                WebRTCConnectionState::Closed(reason) => {
                    return Err(anyhow::anyhow!("WebRTC connection closed: {}", reason));
                }
                WebRTCConnectionState::Pending => {}
            }
            self.state_notify.changed().await?;
        }
    }

    /// 获取（并缓存）分离的数据通道。`detach()` 是幂等的，返回的是
    /// 同一个底层通道的克隆，因此缓存它只是避免为每条消息反复加锁。
    async fn detached_dc(&self) -> ResultType<Arc<DetachedDataChannel>> {
        {
            let cache = self.detached.lock().await;
            if let Some(dc) = cache.as_ref() {
                return Ok(dc.clone());
            }
        }
        let raw = self.stream.lock().await.clone();
        let dc = raw.detach().await?;
        let mut cache = self.detached.lock().await;
        // 在我们分离期间，另一个任务可能已经缓存了它。
        if let Some(existing) = cache.as_ref() {
            return Ok(existing.clone());
        }
        * cache = Some(dc.clone());
        Ok(dc)
    }

    /// **不**具备取消安全性：在消息中途丢弃这个 future（例如把它包进 `select!`/`timeout`）
    /// 可能在链路上留下一段不完整的分片序列，破坏此后该流上每一条
    /// 消息的重组。放弃一次发送的调用方必须把该流视为已死并
    /// 关闭它；下面内置的 `send_timeout` 路径已经这么做了（它会关闭 pc）。
    pub async fn send_bytes(&mut self, bytes: Bytes) -> ResultType<()> {
        let send_timeout = self.send_timeout;
        let send_gate = self.send_gate.clone();
        // 用 send_timeout 限制整个发送过程（等待打开、在另一个克隆后排队、每一次写入）：
        // 否则一次写入会因 SCTP 背压而无限期挂起，而
        // connection.rs 的定时器根本不会运行。这种挂起同时也限制了发送方的内存——
        // PendingQueue 允许 128 KiB，in-flight 受 cwnd/rwnd 限制——因此这个超时就是
        // TCP 发送超时的等价物。参见检查清单中关于发送背压的条目。
        if send_timeout > 0 {
            // 等待连接打开不算这条消息的进度。把它算到
            // 发送计时上会导致，在信令之后的第一次发送时，缓慢的 ICE/DTLS
            // 握手完成会吃掉大部分预算，写入只能继承剩下的部分——然后
            // 超时并关闭一个只差一个 RTT 就能工作的 peer connection。应当给它
            // 自己的预算，并在通道真正可用之后再启动发送计时。
            timeout(
                Duration::from_millis(send_timeout),
                self.wait_for_connect_result(),
            )
            .await
            .map_err(|_| Error::new(ErrorKind::TimedOut, "WebRTC connect timeout"))??;

            let gate_deadline = Instant::now() + Duration::from_millis(send_timeout);
            let send_permit = match timeout_at(gate_deadline, send_gate.acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(err)) => {
                    return Err(Error::new(
                        ErrorKind::BrokenPipe,
                        format!("WebRTC send gate closed: {}", err),
                    )
                    .into());
                }
                Err(_) => {
                    // 刻意不做拆除：这个任务从未持有该 permit，所以
                    // 发生的只是另一个克隆正合法地处于消息中间。从这里
                    // 关闭会中止一个健康发送者的分片序列——而这正是
                    // 该 permit 存在的目的所要防止的破坏。
                    return Err(Error::new(ErrorKind::TimedOut, "WebRTC send gate timeout").into());
                }
            };
            // 拿到 permit 后使用全新的预算：在另一个克隆的消息后排队
            // 不算这条消息的进度，因此在这里计费意味着一个刚刚才输掉
            // 闸门竞态的调用方只获得了寥寥几毫秒的写入时间——然后
            // 就因错失这点时间而拆掉了整个 peer connection。错失得越少，
            // 拆除就越确定，而这恰好是弄反了。
            let write_deadline = Instant::now() + Duration::from_millis(send_timeout);
            let mut desynced = false;
            match timeout_at(write_deadline, self.send_bytes_inner(bytes, &mut desynced)).await {
                // 一次中途失败的写入会留下与超时相同的孤立前缀，因此
                // 它需要同样的拆除——以及同样把 permit 交出去，否则一个正在等待的
                // 克隆会在关闭还在进行时把它的消息追加到那个前缀上。
                Ok(res) => {
                    if desynced {
                        self.close_detached_with(send_permit);
                    }
                    res
                }
                Err(_) => {
                    // 把逻辑消息的 permit 交给拆除过程，这样任何等待中的克隆都无法
                    // 在一段只写了一部分的分片序列之后追加新消息。在一次 await 的
                    // 关闭期间持有它，会在取消时把两者都丢掉。
                    self.close_detached_with(send_permit);
                    Err(Error::new(ErrorKind::TimedOut, "WebRTC send timeout").into())
                }
            }
        } else {
            let send_permit = send_gate.acquire_owned().await.map_err(|err| {
                Error::new(
                    ErrorKind::BrokenPipe,
                    format!("WebRTC send gate closed: {}", err),
                )
            })?;
            let mut desynced = false;
            let res = self.send_bytes_inner(bytes, &mut desynced).await;
            if desynced {
                self.close_detached_with(send_permit);
            }
            res
        }
    }

    /// 当失败在链路上留下不完整的分片序列时设置 `desynced`，也就是当
    /// 该流再也无法恢复一致、调用方必须关闭它时。
    async fn send_bytes_inner(&mut self, bytes: Bytes, desynced: &mut bool) -> ResultType<()> {
        // 与接收方强制执行的上限相同，因此我们绝不会发出同版本对端
        // 不得不因此断开连接的消息。
        if bytes.len() > MAX_RECV_MESSAGE {
            return Err(Error::new(ErrorKind::InvalidInput, "Overflow").into());
        }
        self.wait_for_connect_result().await?;
        let dc = self.detached_dc().await?;
        let data = bytes.as_ref();
        let mut offset = 0;
        // 总是至少发出一个分片（空消息则是一个单独的 FRAG_END 头），这样
        // 零长度的 data channel 消息（接收方无法与 EOF 区分）就
        // 永远不会被发出。
        let mut wrote_any = false;
        loop {
            let end = (offset + MAX_FRAGMENT_PAYLOAD).min(data.len());
            let is_last = end >= data.len();
            let chunk = &data[offset..end];
            let mut framed = BytesMut::with_capacity(1 + chunk.len());
            framed.put_u8(if is_last { FRAG_END } else { FRAG_MORE });
            framed.put_slice(chunk);
            if let Err(err) = dc.write(&framed.freeze()).await {
                // 已经上了链路的分片会给对端留下一个未终结的前缀，而这种
                // 分帧格式没有长度可供它察觉，因此下一条消息会被追加
                // 到它上面。调用方把发送包在 `allow_err!` 里，所以仅有错误还不够。
                * desynced = wrote_any;
                return Err(err.into());
            }
            wrote_any = true;
            offset = end;
            if is_last {
                break;
            }
        }
        Ok(())
    }

    #[inline]
    pub async fn next(&mut self) -> Option<Result<BytesMut, Error>> {
        if let Err(err) = self.wait_for_connect_result().await {
            self.close_detached();
            return Some(Err(Error::new(ErrorKind::Other, err.to_string())));
        }
        let dc = match self.detached_dc().await {
            Ok(dc) => dc,
            Err(err) => {
                self.close_detached();
                return Some(Err(Error::new(ErrorKind::Other, err.to_string())));
            }
        };
        // 刻意在 `dc.read().await` 期间持有，违背通常「不要跨 await 持锁」
        // 的规则：正是它让这次重组所要求的单一读取者（见该字段）成为
        // 一种互斥而不是一种约定。在读取前后释放它就正好会放进
        // 那个破坏 `acc` 的第二个读取者。代价——一个潜在的第二个读取者阻塞
        // 直到有包到达——本来就是这套设计所不允许的状态。
        let mut st = self.recv_state.lock().await;
        loop {
            let RecvState { acc, scratch } = &mut *st;
            if scratch.len() < RECV_BUF_SIZE {
                scratch.resize(SCRATCH_REFILL, 0);
            }
            let n = match dc.read(&mut scratch[..RECV_BUF_SIZE]).await {
                Ok(n) => n,
                Err(err) => {
                    // 释放部分消息，就像下面那些分帧违规路径所做的那样：
                    // 缓冲区最多可持有上限大小的数据，而 `recv_state` 通过
                    // `SESSIONS` 克隆比这次调用活得更久。
                    * acc = BytesMut::new();
                    self.close_detached();
                    return Some(Err(Error::new(
                        ErrorKind::Other,
                        format!("data channel read error: {}", err),
                    )));
                }
            };
            // 在下面的分帧检查之前：一个后来被发现是畸形的分片仍然
            // 证明了是对端发来的，而这正是这个计数器所声称的全部。
            if n > 0 {
                self.rx_probe.add(n);
            }
            if n == 0 {
                // 这不仅仅是流重置：webrtc-data 把空消息的 PPID 映射为
                // n == 0，并且 `read` 丢掉了本可区分两者的标志。两者都意味着
                // 后面没有可分帧的内容，因此一视同仁，但不要当成对端的正常关闭。
                log::debug!("WebRTC data channel ended (reset or empty message)");
                * acc = BytesMut::new();
                self.close_detached();
                return None;
            }
            // 否则两者都会被读成「还有更多分片」。其中没有负载的 FRAG_MORE 更
            // 麻烦：它不会往 `acc` 里添任何东西，因此下面的上限永远不会触发，循环会
            // 在对端持续发送期间一直空转。
            let header = scratch[0];
            let bad = match header {
                FRAG_END => None,
                FRAG_MORE if n > 1 => None,
                FRAG_MORE => Some("FRAG_MORE fragment carries no payload".to_owned()),
                other => Some(format!(
                    "fragment header {other} is neither FRAG_END nor FRAG_MORE"
                )),
            };
            if let Some(why) = bad {
                * acc = BytesMut::new();
                self.close_detached();
                return Some(Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("WebRTC {why}"),
                )));
            }
            // 在扩容**之前**设限：这种分帧不携带长度，因此超大的消息只有
            // 在累积之后才被发现，而在追加之后再检查会让 `BytesMut` 的
            // 重新分配并复制同时持有旧缓冲和新缓冲。
            if acc.len() + (n - 1) > self.max_recv_message.load(Ordering::Relaxed) {
                // 释放缓冲区，而不只是截断它：按定义它已接近上限，
                // 而 `recv_state` 通过 `SESSIONS` 克隆比这次调用活得更久。
                * acc = BytesMut::new();
                self.close_detached();
                return Some(Err(Error::new(
                    ErrorKind::InvalidData,
                    "WebRTC reassembled message exceeded maximum frame size",
                )));
            }
            // 完整到达的消息以读缓冲的一个切片交出：既没有
            // 分配也没有复制，而每个输入事件和大多数音频包都属于这种情况。
            if header == FRAG_END && acc.is_empty() {
                scratch.advance(1);
                return Some(Ok(scratch.split_to(n - 1)));
            }
            acc.extend_from_slice(&scratch[1..n]);
            if header == FRAG_END {
                // 用 split 而不是 take：到下一个
                // 分片到达时调用方已经丢掉了该消息，因此 `reserve` 会回收这块内存，
                // 而不是为每个分片帧从零开始重新增长。
                let msg = acc.split();
                return Some(Ok(msg));
            }
        }
    }

    #[inline]
    pub async fn next_timeout(&mut self, ms: u64) -> Option<Result<BytesMut, Error>> {
        match timeout(Duration::from_millis(ms), self.next()).await {
            Ok(res) => res,
            Err(_) => None,
        }
    }
}

pub fn is_webrtc_endpoint(endpoint: &str) -> bool {
    // 用 sdp base64 json 字符串作为 endpoint，或加上前缀 webrtc:
    endpoint.starts_with("webrtc://")
}

#[cfg(test)]
mod tests {
    use crate::webrtc::WebRTCStream;
    use crate::webrtc::{NewStreamHandoff, DEFAULT_ICE_SERVERS, FRAG_MORE, SESSIONS, WEBRTC_RT};
    use bytes::{BufMut, Bytes, BytesMut};
    use std::{collections::HashMap, sync::Arc, time::Duration};
    use tokio::sync::Barrier;
    use tokio::time::timeout;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

    async fn new_test_stream() -> WebRTCStream {
        WebRTCStream::new("", false, 10000).await.unwrap()
    }

    #[test]
    fn test_webrtc_ice_url() {
        let turn = WebRTCStream::get_ice_server_from_url("turn://123:321@example.com:3478")
            .expect("credentialed turn is usable");
        assert_eq!(turn.urls[0], "turn:example.com:3478");
        assert_eq!(turn.username, "123");
        assert_eq!(turn.credential, "321");

        assert_eq!(
            WebRTCStream::get_ice_server_from_url("turn://123:321@example.com")
                .unwrap_or_default()
                .urls[0],
            "turn:example.com:3478"
        );

        // userinfo 从 URL 解析器出来时是百分号编码的；TURN 服务器要的是
        // 原始值，因此写成 `p%40ss` 的密码会以 `p@ss` 认证。
        let encoded =
            WebRTCStream::get_ice_server_from_url("turn://us%20er:p%40s%2Fs%25@example.com")
                .expect("encoded credentials are usable");
        assert_eq!(encoded.username, "us er");
        assert_eq!(encoded.credential, "p@s/s%");

        // 缺少凭据两半的 TURN 会被丢弃而不是继续传递：
        // webrtc-rs 在构建 peer connection 时会校验每一个配置的服务器，因此
        // 这样一个条目会让**每一个**连接失败，包括那些根本不需要 TURN 的连接。
        for missing in [
            "turn://example.com:3478",
            "turn://example.com",
            "turn://123@example.com",
            "turns://example.com:5349",
            "turn:example.com:3478",
        ] {
            assert_eq!(
                WebRTCStream::get_ice_server_from_url(missing),
                None,
                "credential-less {missing} must not reach the configuration"
            );
        }

        // STUN 不需要凭据，因此两种写法都仍然可用。
        assert_eq!(
            WebRTCStream::get_ice_server_from_url("stun://example.com:3478")
                .unwrap_or_default()
                .urls[0],
            "stun:example.com:3478"
        );

        assert_eq!(
            WebRTCStream::get_ice_server_from_url("http://123:123@example.com:3478"),
            None
        );

        // RFC 7065 的写法（`scheme:host:port`，没有 authority）——STUN/TURN 文档给出的就是它。
        // `url` 是 cannot-be-a-base，因此主机和端口都来自 path；这里搞错
        // 就会产生一个没有主机的 "stun::3478"，任何 ICE agent 都无法解析。
        for (input, expected) in [
            ("stun:example.com:19302", "stun:example.com:19302"),
            ("stun:example.com", "stun:example.com:3478"),
            ("stun:[2001:db8::1]:19302", "stun:[2001:db8::1]:19302"),
            ("stun:[2001:db8::1]", "stun:[2001:db8::1]:3478"),
            (
                "stun:example.com:19302?transport=udp",
                "stun:example.com:19302",
            ),
        ] {
            assert_eq!(
                WebRTCStream::get_ice_server_from_url(input)
                    .unwrap_or_default()
                    .urls[0],
                expected,
                "parsing {input}"
            );
        }

        // 畸形的条目会被丢弃，绝不会被「修补」成让 webrtc-ice 难以招架的
        // 东西：折进去的坏端口（"host:99999:3478"）或未加括号的 IPv6 在其最后一个
        // 冒号处被切分，都会直接让 peer connection 的构建失败，连带把每个服务器都拖垮。
        for bad in [
            "stun:",
            "stun:example.com:99999",
            "stun:example.com:abc",
            "stun:2001:db8::1",
        ] {
            assert_eq!(
                WebRTCStream::get_ice_server_from_url(bad),
                None,
                "malformed {bad} must be dropped"
            );
        }
    }

    // 解析通过 `parse_ice_servers` 来验证，而不是通过改写全局的
    // `ice-servers` 选项：`set_option` 会持久化到真实的配置文件，而测试共享
    // 同一个进程，因此改写它会与回环测试正在构建的每个 peer connection 发生竞态。
    #[test]
    fn test_webrtc_ice_server_list() {
        assert_eq!(
            WebRTCStream::parse_ice_servers("")[0].urls[0],
            DEFAULT_ICE_SERVERS[0].to_string()
        );

        // 不可用的条目会从列表中掉出去；配置的其他部分仍然生效。
        let parsed = WebRTCStream::parse_ice_servers(
            ",stun://example.com,turn://u:p@example.com,turn://nocreds.example.com,sdf",
        );
        assert_eq!(parsed[0].urls[0], "stun:example.com:3478");
        assert_eq!(parsed[1].urls[0], "turn:example.com:3478");
        assert_eq!(parsed.len(), 2);

        // 仅 TURN 的配置仍然会在前面加上默认的 STUN 服务器。
        let turn_only = WebRTCStream::parse_ice_servers("turn://u:p@example.com:3478");
        assert_eq!(turn_only[0].urls[0], DEFAULT_ICE_SERVERS[0].to_string());
        assert_eq!(turn_only[1].urls[0], "turn:example.com:3478");
    }

    #[test]
    fn test_endpoint_ice_policy_declaration() {
        // 带标记的信封声明的是完整 ICE；其他一切——没有标记、
        // 值不对、外来的 scheme、垃圾内容——都按旧有的仅 Relay 语义解读。
        let marked =
            WebRTCStream::sdp_to_endpoint(r#"{"type":"offer","sdp":"v=0","ice_policy":"all"}"#);
        assert!(WebRTCStream::endpoint_declares_all_ice(&marked));

        let unmarked = WebRTCStream::sdp_to_endpoint(r#"{"type":"offer","sdp":"v=0"}"#);
        assert!(!WebRTCStream::endpoint_declares_all_ice(&unmarked));

        let wrong =
            WebRTCStream::sdp_to_endpoint(r#"{"type":"offer","sdp":"v=0","ice_policy":"relay"}"#);
        assert!(!WebRTCStream::endpoint_declares_all_ice(&wrong));

        assert!(!WebRTCStream::endpoint_declares_all_ice(""));
        assert!(!WebRTCStream::endpoint_declares_all_ice(
            "webrtc://not-base64!"
        ));
        assert!(!WebRTCStream::endpoint_declares_all_ice(
            "https://example.com"
        ));

        // 该标记必须对旧版对端所做的普通 RTCSessionDescription 解析不可见。
        let sdp_json = WebRTCStream::get_remote_offer(&marked).unwrap();
        serde_json::from_str::<
            webrtc::peer_connection::sdp::session_description::RTCSessionDescription,
        >(&sdp_json)
        .expect("extra envelope key must not break RTCSessionDescription parsing");
    }

    /// 收集到内容之后的实时描述。轮询而不是 await
    /// `gathering_complete_promise`：一个流在整个等待期间都留在全局 `SESSIONS` 中，而
    /// 在那里耗着等收集完成，足够长到会触发 `test_cancelled_new_does_not_leak_the_pc`
    /// （当测试套件并行运行测试时）。这些测试只需要一个 candidate。
    /// 超时时返回 `None` 而不是 panic：调用方必须先走到 `close()`，然后
    /// 才能断言，否则一个滞留在外的 pc 同样会让 `test_cancelled_new_does_not_leak_the_pc` 失败，
    /// 一个根因失败就被报成两个。
    async fn first_gathered_description(stream: &WebRTCStream) -> Option<RTCSessionDescription> {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(desc) = stream.pc.local_description().await {
                    if desc.sdp.contains("a=candidate:") {
                        return desc;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .ok()
    }

    /// 被信令的 endpoint 与实时描述必须有所分歧。`pc.local_description()`
    /// 会追加截至目前收集到的每个 candidate；交给对端的 endpoint 不能随之增长，
    /// 因为 rendezvous 服务器会把它作为一个数据报交付给注册在 UDP 上的对端，
    /// 而分片的会在丢弃分片的路径上无声无息地被丢掉。
    #[tokio::test]
    async fn test_stored_endpoint_does_not_grow_with_the_live_description() {
        let offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let live = first_gathered_description(&offerer).await;
        let endpoint = offerer.local_endpoint().to_owned();
        offerer.close().await;

        // `Some(_)` 就是「实时描述增长了」这一断言：该辅助函数只有在
        // 带上了一个 candidate 之后才返回，因此下面的 endpoint 确实有东西可长。
        live.expect("no ICE candidate gathered within 10s");
        assert!(
            !WebRTCStream::get_remote_offer(&endpoint)
                .unwrap()
                .contains("a=candidate"),
            "the signalled endpoint grew candidates alongside the live description"
        );
        assert!(
            endpoint.len() <= WebRTCStream::UDP_ENDPOINT_BUDGET,
            "signalled endpoint is {} bytes, over the {} byte budget",
            endpoint.len(),
            WebRTCStream::UDP_ENDPOINT_BUDGET
        );
    }

    /// 拆分本身：把 candidates 拿出去，把启动 ICE 和 DTLS 所需的一切留下来。一个被剥掉
    /// 候选、让对端无法消费的 endpoint，会把静默丢弃换成静默失败。
    #[tokio::test]
    async fn test_trickle_endpoint_splits_candidates_off() {
        let offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let gathered = first_gathered_description(&offerer).await;
        offerer.close().await;
        let gathered = gathered.expect("no ICE candidate gathered within 10s");

        let endpoint = WebRTCStream::trickle_endpoint(&gathered, false).unwrap();
        let json = WebRTCStream::get_remote_offer(&endpoint).unwrap();
        assert!(!json.contains("a=candidate:"), "candidate survived the split");
        assert!(
            !json.contains("a=end-of-candidates"),
            "end-of-candidates would stop the remote agent waiting for the trickle"
        );
        for keep in [
            "m=application",
            "a=ice-ufrag:",
            "a=ice-pwd:",
            "a=fingerprint:",
            "a=setup:",
            "a=mid:",
            "a=sctp-port:",
        ] {
            assert!(json.contains(keep), "trickle endpoint dropped {}", keep);
        }
        assert!(
            endpoint.len() <= WebRTCStream::UDP_ENDPOINT_BUDGET,
            "stripped endpoint is {} bytes",
            endpoint.len()
        );
        // `parsed` 是 #[serde(skip)] 的，因此仅靠反序列化会把任何字符串都当成 SDP
        // 主体接受下来。要反序列化它（对端也是这么做的），并要求会话键所派生的
        // 那个 fingerprint 仍然存在。
        let desc: RTCSessionDescription =
            serde_json::from_str(&json).expect("a stripped endpoint must deserialize");
        WebRTCStream::get_key_for_sdp(&desc)
            .expect("a stripped endpoint must still parse as SDP and keep its fingerprint");

        // `encode_endpoint`（`get_local_endpoint` 也要经过它）持有的是相反的
        // 约定：原样传递交给它的一切，包括 candidates。（`get_local_endpoint`
        // 额外加上的东西——等待收集完成——由 test_webrtc_loopback_gathered_endpoints 覆盖。）
        let kept = WebRTCStream::encode_endpoint(gathered.sdp_type, &gathered.sdp, false).unwrap();
        assert!(
            WebRTCStream::get_remote_offer(&kept)
                .unwrap()
                .contains("a=candidate:"),
            "encode_endpoint dropped the candidates it was handed"
        );
    }

    #[test]
    fn test_webrtc_session_key() {
        let mut sdp_str = "".to_owned();
        assert_eq!(
            WebRTCStream::get_key_for_sdp(
                &RTCSessionDescription::offer(sdp_str).unwrap_or_default()
            )
            .unwrap_or_default(),
            ""
        );

        sdp_str = "\
v=0
o=- 7400546379179479477 208696200 IN IP4 0.0.0.0
s=-
t=0 0
a=fingerprint:sha-256 97:52:D6:1F:1E:87:6C:DA:B8:21:95:64:A5:85:89:FA:02:71:C7:4D:B3:FD:25:92:40:FB:6B:65:24:3C:79:88
a=group:BUNDLE 0
a=extmap-allow-mixed
m=application 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=setup:actpass
a=mid:0
a=sendrecv
a=sctp-port:5000
a=ice-ufrag:RMWjjpXfpXbDPdMz
a=ice-pwd:BtIqlWHfwhsJdFiBROeLuEbNmYfHxRfT".to_owned();
        assert_eq!(
            WebRTCStream::get_key_for_sdp(
                &RTCSessionDescription::offer(sdp_str).unwrap_or_default()
            ).unwrap_or_default(),
            "sha-256 97:52:D6:1F:1E:87:6C:DA:B8:21:95:64:A5:85:89:FA:02:71:C7:4D:B3:FD:25:92:40:FB:6B:65:24:3C:79:88"
        );

        sdp_str = "\
v=0
o=- 7400546379179479477 208696200 IN IP4 0.0.0.0
s=-
t=0 0
a=group:BUNDLE 0
a=extmap-allow-mixed
m=application 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=fingerprint:sha-256 97:52:D6:1F:1E:87:6C:DA:B8:21:95:64:A5:85:89:FA:02:71:C7:4D:B3:FD:25:92:40:FB:6B:65:24:3C:79:88
a=setup:actpass
a=mid:0
a=sendrecv
a=sctp-port:5000
a=ice-ufrag:RMWjjpXfpXbDPdMz
a=ice-pwd:BtIqlWHfwhsJdFiBROeLuEbNmYfHxRfT".to_owned();
        assert_eq!(
            WebRTCStream::get_key_for_sdp(
                &RTCSessionDescription::offer(sdp_str).unwrap_or_default()
            ).unwrap_or_default(),
            "sha-256 97:52:D6:1F:1E:87:6C:DA:B8:21:95:64:A5:85:89:FA:02:71:C7:4D:B3:FD:25:92:40:FB:6B:65:24:3C:79:88"
        );

        sdp_str = "\
v=0
o=- 7400546379179479477 208696200 IN IP4 0.0.0.0
s=-
t=0 0
a=group:BUNDLE 0
a=extmap-allow-mixed
m=application 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=setup:actpass
a=mid:0
a=sendrecv
a=sctp-port:5000
a=ice-ufrag:RMWjjpXfpXbDPdMz
a=ice-pwd:BtIqlWHfwhsJdFiBROeLuEbNmYfHxRfT"
            .to_owned();
        assert!(
            WebRTCStream::get_key_for_sdp(
                &RTCSessionDescription::offer(sdp_str).unwrap_or_default()
            )
            .is_err(),
            "can not find fingerprint attribute"
        );

        sdp_str = "\
v=0
o=- 7400546379179479477 208696200 IN IP4 0.0.0.0
s=-
t=0 0
a=group:BUNDLE 0
a=extmap-allow-mixed
m=audio 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=fingerprint:sha-256 97:52:D6:1F:1E:87:6C:DA:B8:21:95:64:A5:85:89:FA:02:71:C7:4D:B3:FD:25:92:40:FB:6B:65:24:3C:79:88
a=setup:actpass
a=mid:0
a=sendrecv
a=sctp-port:5000
a=ice-ufrag:RMWjjpXfpXbDPdMz
a=ice-pwd:BtIqlWHfwhsJdFiBROeLuEbNmYfHxRfT".to_owned();
        assert!(
            WebRTCStream::get_key_for_sdp(
                &RTCSessionDescription::offer(sdp_str).unwrap_or_default()
            )
            .is_err(),
            "can not find datachannel fingerprint attribute"
        );

        assert!(
            WebRTCStream::get_key_for_sdp(
                &RTCSessionDescription::offer("".to_owned()).unwrap_or_default()
            )
            .is_err(),
            "invalid sdp should error"
        );

        assert!(
            WebRTCStream::get_key_for_sdp_json("{}").is_err(),
            "empty sdp json should error"
        );

        assert!(
            WebRTCStream::get_key_for_sdp_json("{ss}").is_err(),
            "invalid sdp json should error"
        );

        let endpoint = "webrtc://eyJ0eXBlIjoiYW5zd2VyIiwic2RwIjoidj0wXHJcbm89LSA0MTA1NDk3NTY2NDgyMTQzODEwIDYwMzk1NzQw\
MCBJTiBJUDQgMC4wLjAuMFxyXG5zPS1cclxudD0wIDBcclxuYT1maW5nZXJwcmludDpzaGEtMjU2IDYxOjYwOjc0OjQwOjI4OkNFOjBCOjBDOjc1OjRCOj\
EwOjlBOkVFOjc3OkY1OjQ0OjU3Ojg0OjUxOkRCOjA0OjkyOjRBOjEwOjFDOjRFOjVGOjdFOkYxOkIzOjcxOjIyXHJcbmE9Z3JvdXA6QlVORExFIDBcclxu\
YT1leHRtYXAtYWxsb3ctbWl4ZWRcclxubT1hcHBsaWNhdGlvbiA5IFVEUC9EVExTL1NDVFAgd2VicnRjLWRhdGFjaGFubmVsXHJcbmM9SU4gSVA0IDAuMC\
4wLjBcclxuYT1zZXR1cDphY3RpdmVcclxuYT1taWQ6MFxyXG5hPXNlbmRyZWN2XHJcbmE9c2N0cC1wb3J0OjUwMDBcclxuYT1pY2UtdWZyYWc6SHlnU1Rr\
V2RsRlpHRG1XWlxyXG5hPWljZS1wd2Q6SkJneFZWaGZveVhHdHZha1VWcnBQeHVOSVpMU3llS1pcclxuYT1jYW5kaWRhdGU6OTYzOTg4MzQ4IDEgdWRwID\
IxMzA3MDY0MzEgMTkyLjE2OC4xLjIgNjQwMDcgdHlwIGhvc3RcclxuYT1jYW5kaWRhdGU6OTYzOTg4MzQ4IDIgdWRwIDIxMzA3MDY0MzEgMTkyLjE2OC4x\
LjIgNjQwMDcgdHlwIGhvc3RcclxuYT1jYW5kaWRhdGU6MTg2MTA0NTE5MCAxIHVkcCAxNjk0NDk4ODE1IDE0LjIxMi42OC4xMiAyNzAwNCB0eXAgc3JmbH\
ggcmFkZHIgMC4wLjAuMCBycG9ydCA2NDAwOFxyXG5hPWNhbmRpZGF0ZToxODYxMDQ1MTkwIDIgdWRwIDE2OTQ0OTg4MTUgMTQuMjEyLjY4LjEyIDI3MDA0\
IHR5cCBzcmZseCByYWRkciAwLjAuMC4wIHJwb3J0IDY0MDA4XHJcbmE9ZW5kLW9mLWNhbmRpZGF0ZXNcclxuIn0=".to_owned();
        assert_eq!(
            WebRTCStream::get_key_for_sdp_json(
                &WebRTCStream::get_remote_offer(&endpoint).unwrap_or_default()
            ).unwrap_or_default(),
            "sha-256 61:60:74:40:28:CE:0B:0C:75:4B:10:9A:EE:77:F5:44:57:84:51:DB:04:92:4A:10:1C:4E:5F:7E:F1:B3:71:22"
        );
    }

    #[tokio::test]
    async fn test_webrtc_new_stream() {
        let mut endpoint = "webrtc://sdfsdf".to_owned();
        assert!(
            WebRTCStream::new(&endpoint, false, 10000).await.is_err(),
            "invalid webrtc endpoint should error"
        );

        endpoint = "wss://sdfsdf".to_owned();
        assert!(
            WebRTCStream::new(&endpoint, false, 10000).await.is_err(),
            "invalid webrtc endpoint should error"
        );

        let stream = WebRTCStream::new("", false, 10000)
            .await
            .expect("local webrtc endpoint should ok");
        // 原始的 WebRTCStream 没有 Drop：要关闭它，否则它的会话会一直缓存在 SESSIONS 中，
        // 并污染关于该缓存的跨测试断言。
        stream.close().await;

        endpoint = "webrtc://eyJ0eXBlIjoiYW5zd2VyIiwic2RwIjoidj0wXHJcbm89LSA0MTA1NDk3NTY2NDgyMTQzODEwIDYwMzk1NzQw\
MCBJTiBJUDQgMC4wLjAuMFxyXG5zPS1cclxudD0wIDBcclxuYT1maW5nZXJwcmludDpzaGEtMjU2IDYxOjYwOjc0OjQwOjI4OkNFOjBCOjBDOjc1OjRCOj\
EwOjlBOkVFOjc3OkY1OjQ0OjU3Ojg0OjUxOkRCOjA0OjkyOjRBOjEwOjFDOjRFOjVGOjdFOkYxOkIzOjcxOjIyXHJcbmE9Z3JvdXA6QlVORExFIDBcclxu\
YT1leHRtYXAtYWxsb3ctbWl4ZWRcclxubT1hcHBsaWNhdGlvbiA5IFVEUC9EVExTL1NDVFAgd2VicnRjLWRhdGFjaGFubmVsXHJcbmM9SU4gSVA0IDAuMC\
4wLjBcclxuYT1zZXR1cDphY3RpdmVcclxuYT1taWQ6MFxyXG5hPXNlbmRyZWN2XHJcbmE9c2N0cC1wb3J0OjUwMDBcclxuYT1pY2UtdWZyYWc6SHlnU1Rr\
V2RsRlpHRG1XWlxyXG5hPWljZS1wd2Q6SkJneFZWaGZveVhHdHZha1VWcnBQeHVOSVpMU3llS1pcclxuYT1jYW5kaWRhdGU6OTYzOTg4MzQ4IDEgdWRwID\
IxMzA3MDY0MzEgMTkyLjE2OC4xLjIgNjQwMDcgdHlwIGhvc3RcclxuYT1jYW5kaWRhdGU6OTYzOTg4MzQ4IDIgdWRwIDIxMzA3MDY0MzEgMTkyLjE2OC4x\
LjIgNjQwMDcgdHlwIGhvc3RcclxuYT1jYW5kaWRhdGU6MTg2MTA0NTE5MCAxIHVkcCAxNjk0NDk4ODE1IDE0LjIxMi42OC4xMiAyNzAwNCB0eXAgc3JmbH\
ggcmFkZHIgMC4wLjAuMCBycG9ydCA2NDAwOFxyXG5hPWNhbmRpZGF0ZToxODYxMDQ1MTkwIDIgdWRwIDE2OTQ0OTg4MTUgMTQuMjEyLjY4LjEyIDI3MDA0\
IHR5cCBzcmZseCByYWRkciAwLjAuMC4wIHJwb3J0IDY0MDA4XHJcbmE9ZW5kLW9mLWNhbmRpZGF0ZXNcclxuIn0=".to_owned();
        assert!(
            WebRTCStream::new(&endpoint, false, 10000).await.is_err(),
            "connect to an 'answer' webrtc endpoint should error"
        );
    }

    #[tokio::test]
    async fn test_session_cleanup_only_removes_matching_peer() {
        let key = "test-session-cleanup".to_owned();
        let old_key = "test-session-cleanup-old".to_owned();
        let old = new_test_stream().await;
        let replacement = new_test_stream().await;
        let mut sessions = HashMap::new();
        sessions.insert(key.clone(), replacement.clone());

        assert!(WebRTCStream::remove_sessions_for_peer(&mut sessions, &old.pc).is_empty());
        assert!(Arc::ptr_eq(
            &sessions.get(&key).unwrap().pc,
            &replacement.pc,
        ));

        sessions.insert(old_key.clone(), old.clone());
        assert_eq!(
            WebRTCStream::remove_sessions_for_peer(&mut sessions, &old.pc),
            vec![old_key.clone()],
        );
        assert!(!sessions.contains_key(&old_key));
        assert!(Arc::ptr_eq(
            &sessions.get(&key).unwrap().pc,
            &replacement.pc,
        ));

        assert_eq!(
            WebRTCStream::remove_sessions_for_peer(&mut sessions, &replacement.pc),
            vec![key.clone()],
        );
        assert!(!sessions.contains_key(&key));

        old.pc.close().await.ok();
        replacement.pc.close().await.ok();
    }

    #[tokio::test]
    async fn test_webrtc_wait_connected_timeout() {
        let mut stream = WebRTCStream::new("", false, 100).await.unwrap();
        let err = stream.wait_connected(10).await.unwrap_err();
        assert!(err.to_string().contains("timeout"));
        stream.close().await; // see test_webrtc_new_stream: no Drop on a raw WebRTCStream
    }

    async fn connect_loopback() -> (WebRTCStream, WebRTCStream) {
        let mut offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let offer = offerer.local_endpoint().to_owned();
        let answerer = WebRTCStream::new(&offer, false, 20000).await.unwrap();
        let answer = answerer.local_endpoint().to_owned();
        offerer.set_remote_endpoint(&answer).await.unwrap();

        // 在两个对端之间直接桥接 trickle candidates，双向进行。
        let mut off_ice = offerer.take_local_ice_rx().unwrap();
        let mut ans_ice = answerer.take_local_ice_rx().unwrap();
        let answerer_for_ice = answerer.clone();
        let offerer_for_ice = offerer.clone();
        tokio::spawn(async move {
            while let Some(c) = off_ice.recv().await {
                let _ = answerer_for_ice.add_remote_ice_candidate(&c).await;
            }
        });
        tokio::spawn(async move {
            while let Some(c) = ans_ice.recv().await {
                let _ = offerer_for_ice.add_remote_ice_candidate(&c).await;
            }
        });

        offerer.wait_connected(20000).await.unwrap();
        let mut answerer = answerer;
        answerer.wait_connected(20000).await.unwrap();
        (offerer, answerer)
    }

    // next() 的拆除过程不能被取消：一个在 select! 竞态中落败的 await 关闭
    // 会搁浅 pc（is_closed 已经锁定）以及它的 SESSIONS
    // 条目。`close_detached` 不是 async，因此需要证明的是交接能够完成。
    #[tokio::test]
    async fn test_close_detached_completes_without_the_caller() {
        let (offerer, answerer) = connect_loopback().await;
        let key = format!("offer:{}", offerer.session_key());
        assert!(
            SESSIONS.lock().await.contains_key(&key),
            "offerer should be cached while live"
        );

        offerer.close_detached();
        drop(offerer);

        for _ in 0..200 {
            if !SESSIONS.lock().await.contains_key(&key) {
                answerer.close().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("detached close never evicted the peer connection from SESSIONS");
    }

    // 完整消息是从读缓冲中切分出来而不是被复制的，因此缓冲区
    // 从头部被消耗，并定期重新初始化。完整消息与分片消息交替
    // 可以对照累积路径检验这一点，而运行远超一次补充的量
    // 也能检验补充本身——这是定长缓冲区从未有过的边界。
    #[tokio::test]
    async fn test_webrtc_mixed_message_sizes_survive_scratch_refill() {
        let (mut offerer, mut answerer) = connect_loopback().await;

        // 大小分别落在 MAX_FRAGMENT_PAYLOAD 两侧，并且轮数足以消耗好几个
        // SCRATCH_REFILL 分块。
        let sizes = [1usize, 64, 60_000, 60_001, 130_000, 7];
        for round in 0..40u8 {
            for &len in &sizes {
                let payload = Bytes::from(vec![round; len]);
                offerer.send_bytes(payload.clone()).await.unwrap();
                let got = timeout(Duration::from_secs(10), answerer.next())
                    .await
                    .expect("receiver starved")
                    .expect("stream ended")
                    .expect("read failed");
                assert_eq!(got.len(), len, "round {round}, len {len}");
                assert!(
                    got.iter().all(|&b| b == round),
                    "round {}, len {}: content or boundary corrupted",
                    round,
                    len
                );
            }
        }

        offerer.close().await;
        answerer.close().await;
    }

    // 拥有该 Stream 就等于拥有该 peer connection：drop 它必须关闭 pc 并驱逐
    // 该会话，而无需拥有者记着去做。以前每个退出路径都带着这个
    // 义务，而受控端从未遵守过。
    #[tokio::test]
    async fn dropping_the_stream_closes_the_peer_connection() {
        let offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let key = format!("offer:{}", offerer.session_key());
        assert!(
            SESSIONS.lock().await.contains_key(&key),
            "offerer should be cached while live"
        );

        // 任何地方都没有显式关闭：该流只是离开作用域，就像在那些
        // 忘记关闭的退出路径上一样。
        drop(crate::Stream::WebRTC(offerer));

        for _ in 0..200 {
            if !SESSIONS.lock().await.contains_key(&key) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("dropping the stream left the peer connection in SESSIONS");
    }

    // 一个要求不同 ICE 策略的重放 offer 不能拿到缓存的那个 peer
    // connection。查询和插入时的重复检查读的是同一个键，因此这个测试
    // 在两者中任一个不再应用 `is_reusable_for` 时就会失败——当年这道保护就是死
    // 代码：查询拒绝了该条目，而插入又把同一个条目交了回来。
    #[tokio::test]
    async fn test_cached_peer_is_not_reused_across_ice_policies() {
        let offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let offer = offerer.local_endpoint().to_owned();

        let all_ice = WebRTCStream::new(&offer, false, 20000).await.unwrap();
        assert!(!all_ice.relay_only);

        let relay_only = WebRTCStream::new(&offer, true, 20000).await.unwrap();
        assert!(
            relay_only.relay_only,
            "a Relay-only request was answered with the cached All-policy peer connection"
        );

        relay_only.close().await;
        all_ice.close().await;
        offerer.close().await;
    }

    // 本地 candidate 通道必须随 pc 一起关闭。它唯一的发送者位于
    // on_ice_candidate 处理器中，而 close() 不会清除它，因此没有拆除的话
    // 接收端会永远保持打开——而这个 API 要求调用方编写的转发循环会
    // 在阻塞于 recv() 时持有一个流的克隆，让 pc 在自身关闭之后依然存活。收集
    // 完成同样会丢弃发送者，而且可能抢在这里的关闭之前；在这之前就被关闭的 pc
    // 只剩下拆除过程，这正是这里要钉住的行为。
    #[tokio::test]
    async fn test_local_ice_channel_closes_with_the_peer_connection() {
        let offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let mut ice_rx = offerer.take_local_ice_rx().unwrap();

        // 正是每个消费者都会写的那个循环，持有一个流的克隆。
        let forwarder_stream = offerer.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(candidate) = ice_rx.recv().await {
                let _ = forwarder_stream.add_remote_ice_candidate(&candidate).await;
            }
        });

        offerer.close().await;
        drop(offerer);

        tokio::time::timeout(Duration::from_secs(20), forwarder)
            .await
            .expect("forwarder task outlived the peer connection")
            .unwrap();
    }

    // 通道必须在收集完成时关闭，而此时 pc 仍然打开：不会有任何东西跟在
    // agent 最后一个 candidate 之后，而一个阻塞在 recv() 上直到会话结束的转发器会
    // 在这整段时间里持有其信令连接——远早于服务器的空闲关闭，作为一个 socket
    // 被留在 CLOSE_WAIT 状态。
    #[tokio::test]
    async fn test_local_ice_channel_closes_when_gathering_completes() {
        let offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        let mut ice_rx = offerer.take_local_ice_rx().unwrap();
        let drained = timeout(Duration::from_secs(20), async {
            while ice_rx.recv().await.is_some() {}
        })
        .await;
        // 在断言之前先关闭，否则这里的失败会搁浅 pc 并让
        // `test_cancelled_new_does_not_leak_the_pc` 同样失败。
        offerer.close().await;
        drained.expect("local ICE channel outlived gathering");
    }

    // 额外的通道不能顶替已绑定的那个：新来的 on_open 会把 Open 推到
    // 把守 send 和 recv 的那个 watch 上，从而重新武装一个已锁定为 Closed 的会话。只有
    // 只绑定一次的守卫被检验到；有序+可靠的检查在这里无法决定任何事情。
    #[tokio::test]
    async fn test_webrtc_answerer_binds_only_the_first_data_channel() {
        use webrtc::data_channel::data_channel_init::RTCDataChannelInit;

        let mut offerer = WebRTCStream::new("", false, 20000).await.unwrap();
        // 替换 bootstrap 通道的兄弟通道：一个无序的和一个重复的。
        let unordered = offerer
            .pc
            .create_data_channel(
                "unordered",
                Some(RTCDataChannelInit {
                    ordered: Some(false),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let duplicate = offerer
            .pc
            .create_data_channel("duplicate", None)
            .await
            .unwrap();

        let offer = offerer.local_endpoint().to_owned();
        let answerer = WebRTCStream::new(&offer, false, 20000).await.unwrap();
        let answer = answerer.local_endpoint().to_owned();
        offerer.set_remote_endpoint(&answer).await.unwrap();

        let mut off_ice = offerer.take_local_ice_rx().unwrap();
        let mut ans_ice = answerer.take_local_ice_rx().unwrap();
        let answerer_for_ice = answerer.clone();
        let offerer_for_ice = offerer.clone();
        tokio::spawn(async move {
            while let Some(c) = off_ice.recv().await {
                let _ = answerer_for_ice.add_remote_ice_candidate(&c).await;
            }
        });
        tokio::spawn(async move {
            while let Some(c) = ans_ice.recv().await {
                let _ = offerer_for_ice.add_remote_ice_candidate(&c).await;
            }
        });

        offerer.wait_connected(20000).await.unwrap();
        let mut answerer = answerer;
        answerer.wait_connected(20000).await.unwrap();

        // bootstrap 通道仍在承载数据：被拒绝的兄弟通道没有顶替它。
        let payload = Bytes::from_static(b"bootstrap still bound");
        offerer.send_bytes(payload.clone()).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(10), answerer.next())
            .await
            .expect("answerer starved")
            .expect("stream ended")
            .expect("read failed");
        assert_eq!(&got[..], &payload[..]);

        drop(unordered);
        drop(duplicate);
        offerer.close().await;
        answerer.close().await;
    }

    // 一次性调用方只交换 endpoint，从不消费 `take_local_ice_rx`。
    #[tokio::test]
    async fn test_webrtc_loopback_gathered_endpoints() {
        let connect = async {
            let mut offerer = WebRTCStream::new("", false, 20000).await.unwrap();
            let offer = offerer.get_local_endpoint().await.unwrap();
            let mut answerer = WebRTCStream::new(&offer, false, 20000).await.unwrap();
            let answer = answerer.get_local_endpoint().await.unwrap();
            offerer.set_remote_endpoint(&answer).await.unwrap();

            offerer.wait_connected(20000).await.unwrap();
            answerer.wait_connected(20000).await.unwrap();
            offerer.close().await;
            answerer.close().await;
        };
        timeout(Duration::from_secs(40), connect)
            .await
            .expect("gathered-endpoint WebRTC loopback did not complete in time");
    }

    // 健康标志所依据的状态映射。ICE 只在真正的
    // consent 失败时才置起 `Disconnected`，而进程内回环无法产生这种情况，因此这里直接驱动该映射，
    // 下面的回环测试覆盖从该标志到调用方的接线。
    #[test]
    fn test_note_health_maps_only_the_transient_states() {
        use crate::webrtc::RTCPeerConnectionState;
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = AtomicBool::new(false);

        WebRTCStream::note_health(&flag, RTCPeerConnectionState::Disconnected);
        assert!(flag.load(Ordering::SeqCst), "Disconnected must raise the hint");

        WebRTCStream::note_health(&flag, RTCPeerConnectionState::Connected);
        assert!(!flag.load(Ordering::SeqCst), "Connected must clear it");

        // 终止状态有各自的路径，不能去动这个提示：一个
        // 结束的会话用不上它，而在这里清除会掩盖掉先一步陷入沉默的对端。
        for terminal in [
            RTCPeerConnectionState::Failed,
            RTCPeerConnectionState::Closed,
        ] {
            WebRTCStream::note_health(&flag, RTCPeerConnectionState::Disconnected);
            WebRTCStream::note_health(&flag, terminal);
            assert!(
                flag.load(Ordering::SeqCst),
                "{} must leave the hint as it found it",
                terminal
            );
        }
    }

    // 钉住接线和健康标志的极性：刚刚连上的候选对绝不能
    // 显得可疑。
    #[tokio::test]
    async fn test_webrtc_disconnected_is_false_on_a_live_pair() {
        let body = async {
            let (offerer, answerer) = connect_loopback().await;
            assert!(!offerer.is_disconnected());
            assert!(!answerer.is_disconnected());
            offerer.close().await;
            answerer.close().await;
        };
        timeout(Duration::from_secs(30), body)
            .await
            .expect("WebRTC loopback did not complete in time");
    }

    // 一条远大于分片大小的消息在仍在到达时必须表现为有活动：
    // `next()` 在最后一个分片到达前什么都不产出，所以只看完整
    // 消息的接收方无法区分正在传输中的对端和已经掉线的对端。
    #[tokio::test]
    async fn test_webrtc_rx_progress_moves_before_a_large_message_completes() {
        let body = async {
            let (mut offerer, mut answerer) = connect_loopback().await;

            offerer.send_raw(b"warmup".to_vec()).await.unwrap();
            assert_eq!(&answerer.next().await.unwrap().unwrap()[..], b"warmup");
            let baseline = answerer.rx_progress();

            // 足够大，无法在下面循环的一次 poll 内落地，因此一个
            // 只统计完整消息的探针会让计数器自始至终纹丝不动。
            let big = vec![0x5Au8; 4 * 1024 * 1024];
            let expected = big.len();
            let sender = tokio::spawn(async move { offerer.send_raw(big).await.map(|_| offerer) });

            // `next_timeout` 返回 None 会把部分消息留在原处，因此计数器
            // 是在什么都还没交付的时候被读取的。既不用 select，也不用 sleep
            // 这种写法：断言不能依赖某次 poll 恰好走了哪个分支。
            let mut moved_early = false;
            let mut got = None;
            for _ in 0..600 {
                match answerer.next_timeout(20).await {
                    Some(msg) => {
                        got = Some(msg);
                        break;
                    }
                    None => {
                        if answerer.rx_progress() > baseline {
                            moved_early = true;
                        }
                    }
                }
            }

            assert!(
                moved_early,
                "rx_progress must advance while the message is still incomplete"
            );
            assert_eq!(
                got.expect("message never completed").unwrap().len(),
                expected
            );
            let offerer = sender.await.unwrap().expect("send");
            // `WebRTCStream` 没有 `Drop`，因此除非在这里关闭，否则两个 pc 都会留在 `SESSIONS` 中；
            // `test_cancelled_new_does_not_leak_the_pc` 会扫描那个 map 并把它们报出来。
            offerer.close().await;
            answerer.close().await;
        };
        timeout(Duration::from_secs(60), body)
            .await
            .expect("WebRTC loopback did not complete in time");
    }

    // 上限 `Stream::set_max_packet_length` 会在鉴权前被调低：超过它的消息
    // 必须在到达时就终结连接，而不是被重组。只有接收方的上限会变，
    // 因此发送方仍会把它放到链路上，与未鉴权的对端一模一样。
    #[tokio::test]
    async fn test_webrtc_max_packet_length_bounds_reassembly() {
        let body = async {
            let (mut offerer, mut answerer) = connect_loopback().await;
            answerer.set_max_packet_length(16 * 1024);

            offerer.send_raw(vec![0xCDu8; 8 * 1024]).await.unwrap();
            let got = answerer.next().await.unwrap().unwrap();
            assert_eq!(got.len(), 8 * 1024, "a message under the bound still arrives");

            offerer.send_raw(vec![0xCDu8; 200_000]).await.unwrap();
            match timeout(Duration::from_secs(10), answerer.next()).await {
                Ok(Some(Err(e))) => assert!(
                    e.to_string().contains("maximum frame size"),
                    "unexpected error: {}",
                    e
                ),
                Ok(Some(Ok(b))) => panic!("expected a refusal, got {} bytes", b.len()),
                Ok(None) => panic!("expected a refusal, got EOF"),
                Err(_) => panic!("answerer.next() hung on an oversize message"),
            }

            offerer.close().await;
            answerer.close().await;
        };
        timeout(Duration::from_secs(60), body)
            .await
            .expect("WebRTC loopback did not complete in time");
    }

    // 同一生命周期的另一半：上限较低时被拒绝的内容，在
    // 上限被抬高后就会到达，这正是服务端鉴权所做的事。
    #[tokio::test]
    async fn test_webrtc_max_packet_length_restores() {
        let body = async {
            let (mut offerer, mut answerer) = connect_loopback().await;
            answerer.set_max_packet_length(16 * 1024);

            offerer.send_raw(vec![0xCDu8; 8 * 1024]).await.unwrap();
            let got = answerer.next().await.unwrap().unwrap();
            assert_eq!(got.len(), 8 * 1024);

            answerer.set_max_packet_length(usize::MAX);
            offerer.send_raw(vec![0xCDu8; 200_000]).await.unwrap();
            let got = timeout(Duration::from_secs(10), answerer.next())
                .await
                .expect("answerer.next() hung after the bound was lifted")
                .unwrap()
                .unwrap();
            assert_eq!(got.len(), 200_000, "lifting the bound lets a large message through again");

            offerer.close().await;
            answerer.close().await;
        };
        timeout(Duration::from_secs(60), body)
            .await
            .expect("WebRTC loopback did not complete in time");
    }

    // 进程内的 offerer<->answerer 回环，检验 send/next 数据面，而分帧、
    // 空消息和 EOF 的修复都在其中。通过 host candidates 连接（离线可用；任何
    // 配置/默认的 STUN 只是在后台失败，不会阻塞 host 候选对）。
    #[tokio::test]
    async fn test_webrtc_loopback_roundtrip() {
        let connect = async {
            let (mut offerer, mut answerer) = connect_loopback().await;

            // host candidate 回环是直连，绝不经过 TURN 中继。
            assert_eq!(offerer.is_relayed().await, Some(false));

            // 小消息。
            offerer.send_raw(b"hello".to_vec()).await.unwrap();
            let got = answerer.next().await.unwrap().unwrap();
            assert_eq!(&got[..], b"hello");

            // 空消息：必须以空帧往返，不能被当成 EOF。
            offerer.send_raw(Vec::new()).await.unwrap();
            let got = answerer.next().await.unwrap().unwrap();
            assert_eq!(got.len(), 0, "empty message must not be treated as EOF");

            // 负载远超 64KB 单消息上限：必须被分片并重组。
            let big = vec![0xABu8; 200_000];
            offerer.send_raw(big.clone()).await.unwrap();
            let got = answerer.next().await.unwrap().unwrap();
            assert_eq!(
                got.len(),
                big.len(),
                "large message must survive fragmentation"
            );
            assert_eq!(&got[..], &big[..]);

            // 反方向。
            answerer.send_raw(b"world".to_vec()).await.unwrap();
            let got = offerer.next().await.unwrap().unwrap();
            assert_eq!(&got[..], b"world");

            // 对端关闭：另一侧应观察到干净的 EOF（None）或一个关闭错误，绝不能挂起。
            offerer.close().await;
            match timeout(Duration::from_secs(10), answerer.next()).await {
                Ok(None) | Ok(Some(Err(_))) => {}
                Ok(Some(Ok(b))) => panic!("expected EOF after peer close, got {} bytes", b.len()),
                Err(_) => panic!("answerer.next() hung after peer close"),
            }
            answerer.close().await;
        };
        timeout(Duration::from_secs(40), connect)
            .await
            .expect("webrtc loopback did not complete in time");
    }

    // 客户端会话运行在自己的 `#[tokio::main(flavor = "current_thread")]` 运行时上，而它在
    // io_loop 返回的瞬间就被 drop，因此在那里生成或 await 的关闭可能会在
    // `close()` 锁定 `is_closed` 之前（更糟的是之后）被杀。要把生产序列固定在
    // `Stream::WebRTC` 上：在一个完全没有运行时的线程里调用 `close_webrtc()`，然后 Drop；
    // 对端仍必须看到 EOF——由关闭线程而不是任何调用方运行时来交付。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_session_end_close_reaches_the_peer() {
        let (offerer, mut answerer) = connect_loopback().await;
        let stream = crate::Stream::WebRTC(offerer);
        tokio::task::spawn_blocking(move || {
            stream.close_webrtc();
            drop(stream);
        })
        .await
        .expect("session thread");

        match timeout(Duration::from_secs(5), answerer.next()).await {
            Ok(None) | Ok(Some(Err(_))) => {}
            Ok(Some(Ok(b))) => panic!("expected EOF after session close, got {} bytes", b.len()),
            Err(_) => panic!("peer never observed the session-end close"),
        }
        answerer.close().await;
    }

    // 起决定作用的是共享计数，而不是任何单个交接：当一个 pc 上还有第二个认领未结时，
    // 由于没人接管，drop 掉第一个必须让它为那个调用方保持打开，而 drop 掉
    // 最后一个必须关闭它——一个没人应答的 offerer 自身永远不会进入终止性 ICE 状态。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_last_abandoned_claim_closes_an_unadopted_pc() {
        let builder = WEBRTC_RT
            .as_ref()
            .expect("WebRTC I/O runtime")
            .spawn(WebRTCStream::new_inner(String::new(), false, 20000))
            .await
            .expect("setup task")
            .expect("offerer builds");
        let stream = builder
            .stream
            .as_ref()
            .expect("a fresh handoff carries its stream")
            .clone();
        let key = format!("offer:{}", stream.session_key());
        let hit = NewStreamHandoff::claim(stream);

        drop(builder);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            SESSIONS.lock().await.contains_key(&key),
            "a claim was still outstanding, so the pc had to stay open"
        );

        drop(hit);
        for _ in 0..200 {
            if !SESSIONS.lock().await.contains_key(&key) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the last abandoned claim never closed the unadopted pc");
    }

    // `new_inner` 在它自己的 `new()` 返回**之前**就缓存了 pc，因此并发的调用方可能命中
    // 该缓存并接管该流，而 builder 的交接仍未结。此时在 builder 的 Drop 上关闭
    // 就会拆掉另一个调用方正在使用的连接，因此
    // 放弃必须是 pc 的属性，而不是某个交接的属性。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_abandoned_builder_handoff_spares_an_adopted_pc() {
        // 正是 `new()` 所做的事，只是不接管：设置任务在 WEBRTC_RT 上运行，
        // 交回一个全新的、仍未被接管的交接。
        let builder = WEBRTC_RT
            .as_ref()
            .expect("WebRTC I/O runtime")
            .spawn(WebRTCStream::new_inner(String::new(), false, 20000))
            .await
            .expect("setup task")
            .expect("offerer builds");
        let stream = builder
            .stream
            .as_ref()
            .expect("a fresh handoff carries its stream")
            .clone();
        let key = format!("offer:{}", stream.session_key());
        assert!(
            SESSIONS.lock().await.contains_key(&key),
            "new_inner must cache the pc it built"
        );

        // 并发的 `new()`：一个命中缓存、拿走该流并继续使用的情形。
        let adopted = NewStreamHandoff::claim(stream)
            .into_inner()
            .expect("a cache hit hands the stream back");
        // ……现在 builder 的 `new()` 被取消了，于是运行时丢弃了它的交接。
        drop(builder);

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            SESSIONS.lock().await.contains_key(&key),
            "abandoning the builder's handoff closed a pc another caller had already adopted"
        );
        adopted.close_detached();
    }

    // 端到端：在 `new()` 第一次 poll 之后取消它（spawn 已经在进行中），
    // 它临时创建的所有会话都必须再次被关闭并驱逐。
    //
    // 在这里无法知道被取消那次尝试自己的键——它的 DTLS 指纹是在
    // 那个被放弃的任务内部生成的——因此要观察的是一个比
    // 该窗口更长寿的键，而不是一个完全没有新键的瞬间。并发的测试各自关闭它们
    // 创建的会话，所以它们的键时有时无，会从运行中的交集里掉出去；而泄漏的 pc
    // 永远不会。改成等待一个空差集反而让这个测试依赖测试套件
    // 有空闲时刻，而 `--test-threads=2` 从不给它空闲：一条通道整个等待期间
    // 都是这个测试，另一条则不断开启会话。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_cancelled_new_does_not_leak_the_pc() {
        use std::collections::HashSet;
        let before: HashSet<String> = SESSIONS.lock().await.keys().cloned().collect();
        // 零超时：恰好 poll 一次该 future（从而 spawn 出 new_inner），然后取消它——
        // 通常如此。设置任务在自己的运行时上运行，可能在那一次 poll 内就完成，
        // 然后 `new()` 交回一个活的流。`WebRTCStream` 没有 `Drop`，因此
        // 丢弃它本身就是一次泄漏，而这个测试会继续把它报告成
        // 被取消那次尝试的泄漏。把返回的东西关掉，再尝试一次真正的取消。更少的测试
        // 线程会让设置任务更可能赢，这就是为什么这个问题是在
        // `--test-threads=2` 下暴露的，而不是在默认设置下。
        let mut cancelled = false;
        for _ in 0..20 {
            match timeout(Duration::ZERO, WebRTCStream::new("", false, 20000)).await {
                Err(_) => {
                    cancelled = true;
                    break;
                }
                Ok(Ok(stream)) => stream.close().await,
                Ok(Err(_)) => {}
            }
        }
        assert!(cancelled, "new() never lost the race with its own cancellation");
        // 在采样前先让分离的设置任务跑完（并插入它的会话），否则
        // 第一次采样会在泄漏形成之前就取到，之后每一次相交都是
        // 空集。
        tokio::time::sleep(Duration::from_millis(500)).await;
        const ATTEMPTS: usize = 600; // 30s
        let mut persisted: Option<HashSet<String>> = None;
        for attempt in 1..=ATTEMPTS {
            let now: HashSet<String> = SESSIONS.lock().await.keys().cloned().collect();
            let new_keys: HashSet<String> = now.difference(&before).cloned().collect();
            persisted = Some(match persisted {
                None => new_keys,
                Some(prev) => prev.intersection(&new_keys).cloned().collect(),
            });
            let persisted_keys = persisted.as_ref().map_or(0, HashSet::len);
            if persisted_keys == 0 {
                return;
            }
            // 采用位置参数，而不是内联的 `{persisted:?}`：在 edition 2018 上，单个字面量的 `panic!`
            // 不会走 format_args，会原样打印出占位符。
            assert!(
                attempt < ATTEMPTS,
                "cancelled new() left a pc cached in SESSIONS (persisted: {:?})",
                persisted
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // 控制器会话结束时的生产形态：offerer 是在某个会话的
    // 自己的 current_thread 运行时上创建的，会话把它的分离关闭排入队列，而该运行时
    // 紧接着就被销毁（io_loop 返回会 drop 掉它）。pc 的 socket 及其
    // ICE/DTLS/SCTP 泵任务不能随那个运行时一起消亡，否则排队的关闭会
    // 在从未把 close_notify 放到链路上就完成，而对端只能等 ICE 衰减结束。已收集
    // （非 trickle）的 endpoint 把跨运行时信令保持为两次字符串交接。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_close_survives_creator_runtime_destruction() {
        // 每次交接都使用 tokio oneshot：它们的发送是同步的，而会话一侧
        // 在 block_on **内部** await 它们——current_thread 运行时只有在
        // 被 block_on 驱动时才会推进自己的任务，而 offerer 的 SCTP/ICE 泵必须保持存活直到
        // 响应方确认通道已打开。
        let (offer_tx, offer_rx) = tokio::sync::oneshot::channel::<String>();
        let (answer_tx, answer_rx) = tokio::sync::oneshot::channel::<String>();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let session = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("session runtime");
            let offerer = rt.block_on(async {
                let mut offerer = WebRTCStream::new("", false, 20000).await.unwrap();
                offer_tx
                    .send(offerer.get_local_endpoint().await.unwrap())
                    .unwrap();
                let answer = answer_rx.await.unwrap();
                offerer.set_remote_endpoint(&answer).await.unwrap();
                offerer.wait_connected(20000).await.unwrap();
                // 持续驱动该运行时直到响应方确认已打开，这样 EOF
                // 断言（而不是连接建立）才是区分点。
                go_rx.await.unwrap();
                offerer
            });
            // 会话结束：把关闭排入队列，然后销毁创建该 pc 时所用的运行时
            // ——这正是 io_loop 返回所做的事。
            drop(crate::Stream::WebRTC(offerer));
            drop(rt);
        });

        let offer = offer_rx.await.expect("offer from session thread");
        let mut answerer = WebRTCStream::new(&offer, false, 20000).await.unwrap();
        answer_tx
            .send(answerer.get_local_endpoint().await.unwrap())
            .expect("answer to session thread");
        answerer.wait_connected(20000).await.unwrap();
        go_tx.send(()).expect("release session thread");
        session.join().expect("session thread");

        match timeout(Duration::from_secs(5), answerer.next()).await {
            Ok(None) | Ok(Some(Err(_))) => {}
            Ok(Some(Ok(b))) => panic!("expected EOF after session close, got {} bytes", b.len()),
            Err(_) => panic!("peer never observed the close after the creator runtime died"),
        }
        answerer.close().await;
    }

    // 由对端发起的结束：`next()` 遇到 EOF 并触发自己的 `close_detached`，早于
    // 会话的最终关闭（这里通过 `Stream` 的 Drop）发出。两者都落在串行化的
    // 关闭线程上，因此第一个会跑完，第二个只是一个无害的空操作——这个 pc
    // 最终必须处于已关闭并被驱逐的状态，绝不会被一个关闭者取消另一个而搁浅。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_eof_close_then_drop_still_evicts() {
        let (mut offerer, answerer) = connect_loopback().await;
        let key = format!("offer:{}", offerer.session_key());
        answerer.close().await;
        match timeout(Duration::from_secs(5), offerer.next()).await {
            Ok(None) | Ok(Some(Err(_))) => {} // EOF path fired close_detached internally
            Ok(Some(Ok(b))) => panic!("expected EOF after peer close, got {} bytes", b.len()),
            Err(_) => panic!("offerer.next() hung after peer close"),
        }
        drop(crate::Stream::WebRTC(offerer)); // second close via Drop

        for _ in 0..200 {
            if !SESSIONS.lock().await.contains_key(&key) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("pc still cached after EOF close + Drop close");
    }

    // 两种分帧违规都必须终结该流。空 FRAG_MORE 这种情况是那个
    // 无法在下游被发现的：它不会往累加器里添任何东西，因此 MAX_FRAME_LENGTH
    // 上限永远不会触发，而 `next()` 本来会随对端持续写入一直空转。
    // 坏帧是在消息中途注入的，因此该分支对 `acc` 的重置也会被检验到。
    #[tokio::test]
    async fn test_webrtc_rejects_bad_fragment_framing() {
        // (原始帧, 期望的子串)
        let cases: [(&[u8], &str); 2] = [
            (&[0x2A, b'x'], "fragment header 42"),
            (&[FRAG_MORE], "carries no payload"),
        ];
        for (frame, want) in cases {
            let connect = async {
                let (offerer, mut answerer) = connect_loopback().await;
                let dc = offerer.detached_dc().await.unwrap();

                // 先在累加器里留下一条部分消息，让这次拒绝有
                // 东西可丢弃。`send_bytes` 只会发出合法的头，因此这个坏
                // 帧本身必须通过下面的原始通道写出。
                let mut lead = BytesMut::with_capacity(1 + 8);
                lead.put_u8(FRAG_MORE);
                lead.put_slice(b"leading!");
                dc.write(&lead.freeze()).await.unwrap();
                dc.write(&bytes::Bytes::copy_from_slice(frame))
                    .await
                    .unwrap();

                let err = answerer
                    .next()
                    .await
                    .expect("bad framing must surface as an error, not EOF")
                    .expect_err("bad framing must be rejected");
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
                assert!(err.to_string().contains(want), "unexpected error: {}", err);

                offerer.close().await;
                answerer.close().await;
            };
            timeout(Duration::from_secs(40), connect)
                .await
                .expect("webrtc loopback did not complete in time");
        }
    }

    #[tokio::test]
    async fn test_webrtc_concurrent_large_sends_preserve_boundaries() {
        let connect = async {
            let (offerer, mut answerer) = connect_loopback().await;
            let mut sender_a = offerer.clone();
            let mut sender_b = offerer.clone();
            let expected_a = vec![0xAA; 200_000];
            let expected_b = vec![0xBB; 200_000];
            let payload_a = expected_a.clone();
            let payload_b = expected_b.clone();
            let barrier = Arc::new(Barrier::new(3));

            let barrier_a = barrier.clone();
            let send_a = tokio::spawn(async move {
                barrier_a.wait().await;
                sender_a.send_raw(payload_a).await
            });
            let barrier_b = barrier.clone();
            let send_b = tokio::spawn(async move {
                barrier_b.wait().await;
                sender_b.send_raw(payload_b).await
            });

            barrier.wait().await;
            let receive = async {
                let first = answerer.next().await.unwrap().unwrap();
                let second = answerer.next().await.unwrap().unwrap();
                (first, second)
            };
            let (send_a, send_b, (first, second)) = tokio::join!(send_a, send_b, receive);
            send_a.unwrap().unwrap();
            send_b.unwrap().unwrap();

            let boundaries_preserved = (first.as_ref() == expected_a.as_slice()
                && second.as_ref() == expected_b.as_slice())
                || (first.as_ref() == expected_b.as_slice()
                    && second.as_ref() == expected_a.as_slice());
            assert!(boundaries_preserved, "concurrent messages were interleaved");

            offerer.close().await;
            answerer.close().await;
        };
        timeout(Duration::from_secs(40), connect)
            .await
            .expect("concurrent WebRTC sends did not complete in time");
    }
}
