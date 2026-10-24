use std::{
    collections::{hash_map::RandomState, HashMap, VecDeque},
    hash::BuildHasher,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use uuid::Uuid;

use hbb_common::{
    allow_err,
    anyhow::{self, bail},
    config::{self, option2bool, use_ws, Config, CONNECT_TIMEOUT, REG_INTERVAL, RENDEZVOUS_PORT},
    futures::future::join_all,
    log,
    protobuf::Message as _,
    rendezvous_proto::*,
    sleep,
    socket_client::{self, connect_tcp, is_ipv4, new_direct_udp_for, new_udp_for},
    tokio::{
        self, select,
        sync::{mpsc, Mutex},
        time::interval,
    },
    udp::FramedSocket,
    webrtc::WebRTCStream,
    AddrMangle, IntoTargetAddr, ResultType, Stream, TargetAddr,
};
use base::config::keys::*;

use crate::{
    check_port,
    server::{check_zombie, new as new_server, ConnectionMeta, ServerPtr},
};

type Message = RendezvousMessage;

fn connection_meta(
    control_permissions: Option<ControlPermissions>,
    controlled_context: Option<ControlledContext>,
) -> ConnectionMeta {
    ConnectionMeta {
        control_permissions,
        controlled_context,
    }
}

lazy_static::lazy_static! {
    static ref SOLVING_PK_MISMATCH: Mutex<String> = Default::default();
    static ref LAST_MSG: Mutex<(SocketAddr, Instant)> = Mutex::new((SocketAddr::new([0; 4].into(), 0), Instant::now()));
    static ref LAST_RELAY_MSG: Mutex<(SocketAddr, Instant)> = Mutex::new((SocketAddr::new([0; 4].into(), 0), Instant::now()));
    static ref WEBRTC_ICE_TXS: Mutex<HashMap<String, IceRoute>> = Default::default();
    static ref ICE_DIGEST_STATE: RandomState = Default::default();
}
/// 应答方应用期间按会话缓冲的远端 ICE 候选。深度与
/// 控制端自己的缓冲区（`Client::MAX_PENDING_WEBRTC_ICE`）相同，只是那个会淘汰最旧的
/// 条目，而这里队列满时拒绝最新的。
const MAX_PENDING_REMOTE_ICE: usize = 64;
/// 记住已入队的候选，这样控制端的重发会被跳过，而不是占用一个
/// 自己的槽位。这远多于诚实对端会收集的数量，每个只有八字节。
const ICE_DEDUP_WINDOW: usize = 256;
/// 处于 offer 与数据通道打开之间的应答方。offer 在任何密码或
/// 接受提示之前就到达，每个都会建立一个对等连接，为每个网络接口绑定一个套接字并
/// 运行 ICE 最长 `CONNECT_TIMEOUT`，其中一次伪造的 TCP 打洞只需一次 connect。超过这个数量
/// 后 offer 就被拒绝，控制端会像对待不支持 WebRTC 的对端那样继续走打洞与中继。
/// 这是防止病态设置并发的保护，超出了
/// 合法控制端在 ICE 所需那几秒内能达到的并发量；一旦通道打开，
/// 这个连接就和其他连接一样，连接层会对所有传输方式一样地在数量和时间上
/// 限制未认证的连接。
const MAX_WEBRTC_ANSWERERS: usize = 16;
static WEBRTC_ANSWERERS: AtomicUsize = AtomicUsize::new(0);

/// `MAX_WEBRTC_ANSWERERS` 槽位之一，drop 时归还。
struct AnswererSlot;

impl AnswererSlot {
    fn take() -> Option<Self> {
        WEBRTC_ANSWERERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_WEBRTC_ANSWERERS).then(|| n + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for AnswererSlot {
    fn drop(&mut self) {
        WEBRTC_ANSWERERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 进行中的打洞：每个都持有自己的套接字，最多等待 `CONNECT_TIMEOUT` 等待
/// 对端，而且它们背后的请求都不需要认证，任何知道这个 id 的人都可以让
/// hbbs 让我们开一个。对端的会话建立起来的那一刻，槽位就被归还，因此它
/// 只代表这段等待，不多于此：槽位会自行周转，在
/// `CONNECT_TIMEOUT` 加上打洞各阶段带来的几秒之内；到达上限时后来的
/// 请求会被拒绝，而不是打断更早的打洞。被拒绝只是监听本身，绝不包含
/// 回复：回复里还带着 WebRTC answer 和 v6 地址，这些会在没有 v4
/// 打洞的情况下继续发送。
///
/// 两个池子，这样一种传输方式不必为另一种的拥塞买单：一个连接在两个池中各占一个槽位，
/// 控制端首选的请求走 UDP 打洞，其 TCP 回退请求走 TCP
/// 打洞，而竞争失败的一方在整个等待期间都占着自己的槽位。
struct PunchPool {
    places: AtomicUsize,
    max: usize,
}

/// v4 和 v6 上的 UDP。带 v6 地址的请求要占两个槽位，直到对端
/// 不使用的那一个超时。
static UDP_PUNCHES: PunchPool = PunchPool::new(32);
/// TCP 打洞的监听器，以及 FetchLocalAddr 打开的局域网监听，也就是同一个监听器
/// 本身。
static TCP_PUNCHES: PunchPool = PunchPool::new(32);

impl PunchPool {
    const fn new(max: usize) -> Self {
        Self {
            places: AtomicUsize::new(0),
            max,
        }
    }

    fn take(&'static self) -> Option<PunchSlot> {
        self.places
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then(|| n + 1)
            })
            .ok()
            .map(|_| PunchSlot(self))
    }

    /// 对于要同时在两者上打洞的请求，先用 v4：到最后一个槽位时就会是 v6
    /// 打洞没有槽位可用，不管两个监听以什么顺序启动——v4 是对端
    /// 能指望的那个，v6 则是可能没有路由的那个。
    fn take_pair(&'static self, v4: bool, v6: bool) -> (Option<PunchSlot>, Option<PunchSlot>) {
        let v4 = v4.then(|| self.take()).flatten();
        let v6 = v6.then(|| self.take()).flatten();
        (v4, v6)
    }
}

/// 池中的一个槽位，drop 时归还。
pub(crate) struct PunchSlot(&'static PunchPool);

impl Drop for PunchSlot {
    fn drop(&mut self) {
        self.0.places.fetch_sub(1, Ordering::AcqRel);
    }
}
// rendezvous 的 ICE 路径无需事先打洞即可到达，且由对端决定发送多少
// 候选，所以这些位置会让别人来设定本机向
// 其日志文件写入多少。每个位置每分钟一行，带上被抑制的计数。
const ICE_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// 被拒绝的打洞是每分钟一行，带上它所代表的数量，原因相同：
/// 由对端决定多久问一次。
const PUNCH_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
static UNKNOWN_ICE_SESSION_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(ICE_LOG_INTERVAL);
static REJECTED_REMOTE_ICE_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(ICE_LOG_INTERVAL);
static FULL_ICE_QUEUE_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(ICE_LOG_INTERVAL);

struct IceRoute {
    tx: mpsc::Sender<String>,
    recent: VecDeque<u64>,
}

impl IceRoute {
    fn new(tx: mpsc::Sender<String>) -> Self {
        Self {
            tx,
            recent: VecDeque::new(),
        }
    }

    /// 让 `queue` 成为进入通道的唯一途径，这样不会有任何东西未经记录就到达通道。
    fn is_same_channel(&self, other: &mpsc::Sender<String>) -> bool {
        self.tx.same_channel(other)
    }

    /// 跳过控制端对已入队候选的重发：负责去重的 ICE agent
    /// 位于这个队列的下游，所以副本会白白占用一个自己的槽位。
    /// False 表示该候选被丢弃。
    fn queue(&mut self, candidate: String) -> bool {
        let digest = ICE_DIGEST_STATE.hash_one(candidate.as_str());
        if self.recent.contains(&digest) {
            // 只有当路径仍然存活、确实接收了它时，对丢弃的记录才算诚实。
            return !self.tx.is_closed();
        }
        // 入队后才记录，绝不在入队前记录：被拒绝的候选仍可通过重发补救。
        if self.tx.try_send(candidate).is_err() {
            return false;
        }
        if self.recent.len() >= ICE_DEDUP_WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(digest);
        true
    }
}

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);
static MANUAL_RESTARTED: AtomicBool = AtomicBool::new(false);
static SENT_REGISTER_PK: AtomicBool = AtomicBool::new(false);
pub(crate) static NEEDS_DEPLOY: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static NOTIFIED_NEEDS_DEPLOY: AtomicBool = AtomicBool::new(false);
// 设备等待部署时的 register_pk 重试间隔（毫秒）
const DEPLOY_RETRY_INTERVAL: i64 = 30_000;
lazy_static::lazy_static! {
    static ref LAST_NOT_DEPLOYED_REGISTER: Mutex<Option<Instant>> = Mutex::new(None);
}

// “等待部署”退避的唯一真实来源。服务器已经
// 告知我们这台设备不在它的数据库里；在运维人员运行
// `rustdesk --deploy --token <api_token>` 之前，没有必要比 DEPLOY_RETRY_INTERVAL 更频繁地
// 重跑注册流程。把门控放在定时器
// 循环里（而不只是放在 register_pk 内部），还能避免
// last_register_sent / fails / latency / UDP 重绑这些状态变化在
// 没有任何响应回来时让循环空转。
async fn deploy_register_throttled() -> bool {
    if !NEEDS_DEPLOY.load(Ordering::SeqCst) {
        return false;
    }
    LAST_NOT_DEPLOYED_REGISTER
        .lock()
        .await
        .map(|t| (t.elapsed().as_millis() as i64) < DEPLOY_RETRY_INTERVAL)
        .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn notify_android_needs_deploy() {
    if NOTIFIED_NEEDS_DEPLOY.load(Ordering::SeqCst) {
        return;
    }
    let event = serde_json::json!({ "name": "android_needs_deploy" }).to_string();
    if matches!(
        crate::flutter::push_global_event(crate::flutter::APP_TYPE_MAIN, event),
        Some(true)
    ) {
        NOTIFIED_NEEDS_DEPLOY.store(true, Ordering::SeqCst);
    }
}

#[cfg(target_os = "android")]
pub(crate) fn reset_needs_deploy_notification() {
    NEEDS_DEPLOY.store(false, Ordering::SeqCst);
    NOTIFIED_NEEDS_DEPLOY.store(false, Ordering::SeqCst);
}

#[derive(Clone)]
pub struct RendezvousMediator {
    addr: TargetAddr<'static>,
    host: String,
    host_prefix: String,
    keep_alive: i32,
}

impl RendezvousMediator {
    pub fn restart() {
        SHOULD_EXIT.store(true, Ordering::SeqCst);
        MANUAL_RESTARTED.store(true, Ordering::SeqCst);
        log::info!("server restart");
    }

    pub async fn start_all() {
        crate::test_nat_type();
        if config::is_outgoing_only() {
            loop {
                sleep(1.).await;
            }
        }
        crate::hbbs_http::sync::start();
        #[cfg(target_os = "windows")]
        if crate::platform::is_installed() && crate::is_server() {
            crate::updater::start_auto_update();
        }
        check_zombie();
        let server = new_server();
        if config::option2bool("stop-service", &Config::get_option("stop-service")) {
            crate::test_rendezvous_server();
        }
        let server_cloned = server.clone();
        tokio::spawn(async move {
            direct_server(server_cloned).await;
        });
        #[cfg(target_os = "android")]
        let start_lan_listening = true;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let start_lan_listening = crate::platform::is_installed();
        if start_lan_listening {
            std::thread::spawn(move || {
                allow_err!(super::lan::start_listening());
            });
        }
        scrap::codec::test_av1();
        * LAST_NOT_DEPLOYED_REGISTER.lock().await = None;
        loop {
            let timeout = Arc::new(RwLock::new(CONNECT_TIMEOUT));
            let conn_start_time = Instant::now();
            * SOLVING_PK_MISMATCH.lock().await = "".to_owned();
            if !config::option2bool("stop-service", &Config::get_option("stop-service"))
                && !crate::platform::installing_service()
            {
                let mut futs = Vec::new();
                let servers = Config::get_rendezvous_servers();
                SHOULD_EXIT.store(false, Ordering::SeqCst);
                MANUAL_RESTARTED.store(false, Ordering::SeqCst);
                for host in servers.clone() {
                    let server = server.clone();
                    let timeout = timeout.clone();
                    futs.push(tokio::spawn(async move {
                        if let Err(err) = Self::start(server, host).await {
                            let err = format!("rendezvous mediator error: {err}");
                            // 用户重启时，下面可能出现错误，等待时间过长
                            // （CONNECT_TIMEOUT 18s）会让用户以为有 bug
                            if err.contains("10054") || err.contains("11001") {
                                // 不知道这样的主机。(os error 11001)
                                // 已存在的连接被远程主机强制关闭。(os error 10054)：UDP 也会出现这种情况
                                * timeout.write().unwrap() = 3000;
                            }
                            log::error!("{err}");
                        }
                        // 这里的 SHOULD_EXIT 是为了确保其中一个退出后，其他也会退出。
                        SHOULD_EXIT.store(true, Ordering::SeqCst);
                    }));
                }
                join_all(futs).await;
            } else {
                server.write().unwrap().close_connections();
            }
            Config::reset_online();
            let timeout = *timeout.read().unwrap();
            if !MANUAL_RESTARTED.load(Ordering::SeqCst) {
                let elapsed = conn_start_time.elapsed().as_millis() as u64;
                if elapsed < timeout {
                    sleep(((timeout - elapsed) / 1000) as _).await;
                }
            } else {
                // https://github.com/rustdesk/rustdesk/issues/12233
                sleep(0.033).await;
            }
        }
    }

    fn get_host_prefix(host: &str) -> String {
        host.split(".")
            .next()
            .map(|x| {
                if x.parse::<i32>().is_ok() {
                    host.to_owned()
                } else {
                    x.to_owned()
                }
            })
            .unwrap_or(host.to_owned())
    }

    pub async fn start_udp(server: ServerPtr, host: String) -> ResultType<()> {
        let host = check_port(&host, RENDEZVOUS_PORT);
        log::info!("start udp: {host}");
        let (mut socket, mut addr) = new_udp_for(&host, CONNECT_TIMEOUT).await?;
        let mut rz = Self {
            addr: addr.clone(),
            host: host.clone(),
            host_prefix: Self::get_host_prefix(&host),
            keep_alive: crate::DEFAULT_KEEP_ALIVE,
        };

        let mut timer = crate::rustdesk_interval(interval(crate::TIMER_OUT));
        const MIN_REG_TIMEOUT: i64 = 3_000;
        const MAX_REG_TIMEOUT: i64 = 30_000;
        let mut reg_timeout = MIN_REG_TIMEOUT;
        const MAX_FAILS1: i64 = 2;
        const MAX_FAILS2: i64 = 4;
        const DNS_INTERVAL: i64 = 60_000;
        let mut fails = 0;
        let mut last_register_resp: Option<Instant> = None;
        let mut last_register_sent: Option<Instant> = None;
        let mut last_dns_check = Instant::now();
        let mut old_latency = 0;
        let mut ema_latency = 0;
        loop {
            let mut update_latency = || {
                last_register_resp = Some(Instant::now());
                fails = 0;
                reg_timeout = MIN_REG_TIMEOUT;
                let mut latency = last_register_sent
                    .map(|x| x.elapsed().as_micros() as i64)
                    .unwrap_or(0);
                last_register_sent = None;
                if latency < 0 || latency > 1_000_000 {
                    return;
                }
                if ema_latency == 0 {
                    ema_latency = latency;
                } else {
                    ema_latency = latency / 30 + (ema_latency * 29 / 30);
                    latency = ema_latency;
                }
                let mut n = latency / 5;
                if n < 3000 {
                    n = 3000;
                }
                if (latency - old_latency).abs() > n || old_latency <= 0 {
                    Config::update_latency(&host, latency);
                    log::debug!("Latency of {}: {}ms", host, latency as f64 / 1000.);
                    old_latency = latency;
                }
            };
            select! {
                n = socket.next() => {
                    match n {
                        Some(Ok((bytes, _))) => {
                            if let Ok(msg) = Message::parse_from_bytes(&bytes) {
                                rz.handle_resp(msg.union, Sink::Framed(&mut socket, &addr), &server, &mut update_latency).await?;
                            } else {
                                log::debug!("Non-protobuf message bytes received: {:?}", bytes);
                            }
                        },
                        Some(Err(e)) => bail!("Failed to receive next: {}", e),  // maybe socks5 tcp disconnected
                        None => {
                            bail!("Socket receive none. Maybe socks5 server is down.");
                        },
                    }
                },
                _ = timer.tick() => {
                    if SHOULD_EXIT.load(Ordering::SeqCst) {
                        break;
                    }
                    // 服务器已经告知我们这台设备未部署。跳过
                    // 整条 register / fails / latency / UDP 重绑流程，直到
                    // DEPLOY_RETRY_INTERVAL 过去，否则循环每隔
                    // 几秒就空转一次（日志刷屏 + 网络恢复时的重绑被误用）
                    // 直到运维人员运行 `rustdesk --deploy`。
                    if deploy_register_throttled().await {
                        continue;
                    }
                    let now = Some(Instant::now());
                    let expired = last_register_resp.map(|x| x.elapsed().as_millis() as i64 >= REG_INTERVAL).unwrap_or(true);
                    let timeout = last_register_sent.map(|x| x.elapsed().as_millis() as i64 >= reg_timeout).unwrap_or(false);
                    // 在我们给 android 加上唤醒触发器以强制连接之前，暂时为 android 禁用指数退避
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if crate::using_public_server() { // only turn on this for public server, may help DDNS self-hosting user.
                        if timeout && reg_timeout < MAX_REG_TIMEOUT {
                            reg_timeout += MIN_REG_TIMEOUT;
                        }
                    }
                    if timeout || (last_register_sent.is_none() && expired) {
                        if timeout {
                            fails += 1;
                            if fails >= MAX_FAILS2 {
                                Config::update_latency(&host, -1);
                                old_latency = 0;
                                if last_dns_check.elapsed().as_millis() as i64 > DNS_INTERVAL {
                                    // 在某些网络重连（拨号 IP 网络）的情况下，
                                    // 网络恢复后旧的 UDP 套接字不再工作
                                    if let Some((s, new_addr)) = socket_client::rebind_udp_for(&rz.host).await? {
                                        socket = s;
                                        rz.addr = new_addr.clone();
                                        addr = new_addr;
                                    }
                                    last_dns_check = Instant::now();
                                }
                            } else if fails >= MAX_FAILS1 {
                                Config::update_latency(&host, 0);
                                old_latency = 0;
                            }
                        }
                        rz.register_peer(Sink::Framed(&mut socket, &addr)).await?;
                        last_register_sent = now;
                    }
                }
            }
        }
        Ok(())
    }

    #[inline]
    async fn handle_resp(
        &mut self,
        msg: Option<rendezvous_message::Union>,
        sink: Sink<'_>,
        server: &ServerPtr,
        update_latency: &mut impl FnMut(),
    ) -> ResultType<()> {
        match msg {
            Some(rendezvous_message::Union::RegisterPeerResponse(rpr)) => {
                update_latency();
                if rpr.request_pk {
                    log::info!("request_pk received from {}", self.host);
                    self.register_pk(sink).await?;
                }
            }
            Some(rendezvous_message::Union::RegisterPkResponse(rpr)) => {
                update_latency();
                match rpr.result.enum_value() {
                    Ok(register_pk_response::Result::OK) => {
                        Config::set_key_confirmed(true);
                        Config::set_host_key_confirmed(&self.host_prefix, true);
                        * SOLVING_PK_MISMATCH.lock().await = "".to_owned();
                        NEEDS_DEPLOY.store(false, Ordering::SeqCst);
                        #[cfg(target_os = "android")]
                        reset_needs_deploy_notification();
                    }
                    Ok(register_pk_response::Result::UUID_MISMATCH) => {
                        self.handle_uuid_mismatch(sink).await?;
                    }
                    Ok(register_pk_response::Result::NOT_DEPLOYED) => {
                        if !NEEDS_DEPLOY.load(Ordering::SeqCst) {
                            log::warn!("Server requires deployment. Run `rustdesk --deploy --token <api_token>` on this device.");
                        }
                        NEEDS_DEPLOY.store(true, Ordering::SeqCst);
                        // 清除 key_confirmed 让界面反映真实情况：这台设备
                        // 当前未注册。覆盖了在线设备
                        // 在运行期间被管理员删除的情况。
                        Config::set_key_confirmed(false);
                        Config::set_host_key_confirmed(&self.host_prefix, false);
                        #[cfg(target_os = "android")]
                        notify_android_needs_deploy();
                    }
                    _ => {
                        log::error!("unknown RegisterPkResponse");
                    }
                }
                if rpr.keep_alive > 0 {
                    self.keep_alive = rpr.keep_alive * 1000;
                    log::info!("keep_alive: {}ms", self.keep_alive);
                }
            }
            Some(rendezvous_message::Union::PunchHole(ph)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_punch_hole(ph, server).await);
                });
            }
            Some(rendezvous_message::Union::RequestRelay(rr)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_request_relay(rr, server).await);
                });
            }
            Some(rendezvous_message::Union::FetchLocalAddr(fla)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_intranet(fla, server).await);
                });
            }
            Some(rendezvous_message::Union::IceCandidate(ice)) => {
                let queued = {
                    let mut txs = WEBRTC_ICE_TXS.lock().await;
                    txs.get_mut(&ice.session_key)
                        .map(|route| route.queue(ice.candidate))
                };
                match queued {
                    Some(false) => {
                        if let Some(n) = FULL_ICE_QUEUE_LOG.due() {
                            log::debug!("dropped {} ICE candidate(s): queue full or closed", n);
                        }
                    }
                    None => {
                        if let Some(n) = UNKNOWN_ICE_SESSION_LOG.due() {
                            log::debug!(
                                "dropped {} ICE candidate(s) for unknown WebRTC session key, last: {}",
                                n,
                                ice.session_key
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn start_tcp(server: ServerPtr, host: String) -> ResultType<()> {
        let host = check_port(&host, RENDEZVOUS_PORT);
        log::info!("start tcp: {}", hbb_common::websocket::check_ws(&host));
        let mut conn = connect_tcp(host.clone(), CONNECT_TIMEOUT).await?;
        let key = crate::get_key(true).await;
        crate::secure_tcp(&mut conn, &key).await?;
        let mut rz = Self {
            addr: conn.local_addr().into_target_addr()?,
            host: host.clone(),
            host_prefix: Self::get_host_prefix(&host),
            keep_alive: crate::DEFAULT_KEEP_ALIVE,
        };
        let mut timer = crate::rustdesk_interval(interval(crate::TIMER_OUT));
        let mut last_register_sent: Option<Instant> = None;
        let mut last_recv_msg = Instant::now();
        // 我们不再支持连接多个 rendezvous 服务器，所以这里可以用一个全局变量。
        Config::set_host_key_confirmed(&rz.host_prefix, false);
        loop {
            let mut update_latency = || {
                let latency = last_register_sent
                    .map(|x| x.elapsed().as_micros() as i64)
                    .unwrap_or(0);
                Config::update_latency(&host, latency);
                log::debug!("Latency of {}: {}ms", host, latency as f64 / 1000.);
            };
            select! {
                res = conn.next() => {
                    last_recv_msg = Instant::now();
                    let bytes = res.ok_or_else(|| anyhow::anyhow!("Rendezvous connection is reset by the peer"))??;
                    if bytes.is_empty() {
                        // 修复 register_pk 过于频繁的问题后，对 websocket 而言，nginx 需要把 proxy_read_timeout 设置为大于 60 秒，例如 120s
                        // https://serverfault.com/questions/1060525/why-is-my-websocket-connection-gets-closed-in-60-seconds
                        conn.send_bytes(bytes::Bytes::new()).await?;
                        continue; // heartbeat
                    }
                    let msg = Message::parse_from_bytes(&bytes)?;
                    rz.handle_resp(msg.union, Sink::Stream(&mut conn), &server, &mut update_latency).await?
                }
                _ = timer.tick() => {
                    if SHOULD_EXIT.load(Ordering::SeqCst) {
                        break;
                    }
                    // https://www.emqx.com/en/blog/mqtt-keep-alive
                    if last_recv_msg.elapsed().as_millis() as u64 > rz.keep_alive as u64 * 3 / 2 {
                        bail!("Rendezvous connection is timeout");
                    }
                    if (!Config::get_key_confirmed() ||
                        !Config::get_host_key_confirmed(&rz.host_prefix)) &&
                        last_register_sent.map(|x| x.elapsed().as_millis() as i64).unwrap_or(REG_INTERVAL) >= REG_INTERVAL {
                        rz.register_pk(Sink::Stream(&mut conn)).await?;
                        last_register_sent = Some(Instant::now());
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn start(server: ServerPtr, host: String) -> ResultType<()> {
        log::info!("start rendezvous mediator of {}", host);
        // 如果代理类型是 http 或 https，则启用 tcp 转发。
        if (cfg!(debug_assertions) && option_env!("TEST_TCP").is_some())
            || Config::is_proxy()
            || use_ws()
            || crate::is_udp_disabled()
        {
            Self::start_tcp(server, host).await
        } else {
            Self::start_udp(server, host).await
        }
    }

    async fn handle_request_relay(&self, rr: RequestRelay, server: ServerPtr) -> ResultType<()> {
        let addr = AddrMangle::decode(&rr.socket_addr);
        let last = *LAST_RELAY_MSG.lock().await;
        * LAST_RELAY_MSG.lock().await = (addr, Instant::now());
        // 跳过重复的中继请求消息
        if last.0 == addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let meta = connection_meta(
            rr.control_permissions.into_option(),
            rr.controlled_context.into_option(),
        );

        self.create_relay(
            rr.socket_addr.into(),
            rr.relay_server,
            rr.uuid,
            server,
            rr.secure,
            false,
            Default::default(),
            String::new(),
            meta,
        )
        .await
    }

    async fn create_relay(
        &self,
        socket_addr: Vec<u8>,
        relay_server: String,
        uuid: String,
        server: ServerPtr,
        secure: bool,
        initiate: bool,
        socket_addr_v6: bytes::Bytes,
        webrtc_sdp_answer: String,
        meta: ConnectionMeta,
    ) -> ResultType<()> {
        let peer_addr = AddrMangle::decode(&socket_addr);
        log::info!(
            "create_relay requested from {:?}, relay_server: {}, uuid: {}, secure: {}",
            peer_addr,
            relay_server,
            uuid,
            secure,
        );

        let mut socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
        // 携带 answer 的中继响应会同时携带本机的 ICE 候选，因此
        // 那一半只在加密通道上发出。未完成交换的服务器
        // 失去的是 answer 而不是中继：响应不带 answer 地发出，并且用一个新的
        // 套接字，因为失败的交换可能已经在当前套接字上消耗掉一条消息，而
        // 控制端会回退到其他传输方式。
        let mut webrtc_sdp_answer = webrtc_sdp_answer;
        if !webrtc_sdp_answer.is_empty() {
            let key = crate::get_key(true).await;
            if let Err(err) = crate::secure_tcp_required(&mut socket, &key).await {
                log::warn!("relaying without the WebRTC answer, it cannot be encrypted: {err}");
                webrtc_sdp_answer = String::new();
                socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
            }
        }

        let mut msg_out = Message::new();
        let mut rr = RelayResponse {
            socket_addr: socket_addr.into(),
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            webrtc_sdp_answer,
            ..Default::default()
        };
        if initiate {
            rr.uuid = uuid.clone();
            rr.relay_server = relay_server.clone();
            rr.set_id(Config::get_id());
        }
        msg_out.set_relay_response(rr);
        socket.send(&msg_out).await?;
        crate::create_relay_connection(
            server,
            relay_server,
            uuid,
            peer_addr,
            secure,
            is_ipv4(&self.addr),
            meta,
        )
        .await;
        Ok(())
    }

    async fn handle_intranet(&self, fla: FetchLocalAddr, server: ServerPtr) -> ResultType<()> {
        let addr = AddrMangle::decode(&fla.socket_addr);
        let last = *LAST_MSG.lock().await;
        * LAST_MSG.lock().await = (addr, Instant::now());
        // 跳过重复的打洞消息
        if last.0 == addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let peer_addr_v6 = hbb_common::AddrMangle::decode(&fla.socket_addr_v6);
        let relay_server = self.get_relay_server(fla.relay_server.clone());
        let relay = use_ws() || Config::is_proxy();
        let mut socket_addr_v6 = Default::default();
        let meta = connection_meta(
            fla.control_permissions.clone().into_option(),
            fla.controlled_context.clone().into_option(),
        );
        if peer_addr_v6.port() > 0 && !relay {
            socket_addr_v6 = start_ipv6(
                peer_addr_v6,
                addr,
                server.clone(),
                meta.clone(),
                UDP_PUNCHES.take(),
            )
            .await;
        }
        if is_ipv4(&self.addr) && !relay && !config::is_disable_tcp_listen() {
            if let Err(err) = self
                .handle_intranet_(
                    fla.clone(),
                    server.clone(),
                    relay_server.clone(),
                    socket_addr_v6.clone(),
                    meta.clone(),
                )
                .await
            {
                log::debug!("Failed to handle intranet: {:?}, will try relay", err);
            } else {
                return Ok(());
            }
        }
        let uuid = Uuid::new_v4().to_string();
        self.create_relay(
            fla.socket_addr.into(),
            relay_server,
            uuid,
            server,
            true,
            true,
            socket_addr_v6,
            String::new(),
            meta,
        )
        .await
    }

    async fn handle_intranet_(
        &self,
        fla: FetchLocalAddr,
        server: ServerPtr,
        relay_server: String,
        socket_addr_v6: bytes::Bytes,
        meta: ConnectionMeta,
    ) -> ResultType<()> {
        let peer_addr = AddrMangle::decode(&fla.socket_addr);
        log::debug!("Handle intranet from {:?}", peer_addr);
        // 这里打开的监听像 TCP 打洞一样等待对端，也像 TCP 打洞一样被拒绝；
        // 调用方随后走中继，就像监听因其他任何原因失败时那样。
        let Some(slot) = TCP_PUNCHES.take() else {
            hbb_common::throttled_log!(
                PUNCH_LOG_INTERVAL,
                warn,
                "declined a LAN listen: {} TCP punches already in flight",
                TCP_PUNCHES.max
            );
            bail!("no place among the punches in flight");
        };
        let mut socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
        let local_addr = socket.local_addr();
        // 我们遇到过使用代理时 local_addr 无效的情况，local_addr.ip() == "::1"
        let local_addr: SocketAddr =
            format!("{}:{}", local_addr.ip(), local_addr.port()).parse()?;
        let mut msg_out = Message::new();
        msg_out.set_local_addr(LocalAddr {
            id: Config::get_id(),
            socket_addr: AddrMangle::encode(peer_addr).into(),
            local_addr: AddrMangle::encode(local_addr).into(),
            relay_server,
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            ..Default::default()
        });
        let bytes = msg_out.write_to_bytes()?;
        socket.send_raw(bytes).await?;
        crate::accept_connection(server.clone(), socket, peer_addr, true, meta, slot).await;
        Ok(())
    }

    /// 为打洞 offer 构建 WebRTC 应答方，并返回随
    /// 打洞回复（PunchHoleSent / RelayResponse）一起发出的 SDP answer。
    ///
    /// 在打洞回复路径上就地 await，这样做只是因为这里的一切都在本地
    /// （pc + keygen + SDP；trickle 意味着 answer 不携带候选）。网络 I/O 不要放在这里——
    /// 连接建立应放在下面那个分离出去的任务里。
    async fn spawn_webrtc_answerer(
        &self,
        ph: &PunchHole,
        relay_only_ice: bool,
        server: ServerPtr,
        peer_addr: SocketAddr,
        meta: ConnectionMeta,
    ) -> ResultType<String> {
        let Some(slot) = AnswererSlot::take() else {
            hbb_common::throttled_log!(
                ICE_LOG_INTERVAL,
                warn,
                "declined a WebRTC offer: {} answerers already in flight",
                MAX_WEBRTC_ANSWERERS
            );
            return Ok(String::new());
        };
        let mut stream =
            WebRTCStream::new(&ph.webrtc_sdp_offer, relay_only_ice, CONNECT_TIMEOUT).await?;
        let answer = stream.local_endpoint().to_owned();
        let session_key = stream.session_key().to_owned();
        let return_route = ph.socket_addr.clone();

        // 重复的 PunchHole（发起方在多次打洞尝试中重发同一请求）
        // 会解析到 SESSIONS 中缓存的流。`take_local_ice_rx` 只会为每个流实例
        // 产出一次接收端，所以这里的 `None` 意味着已经为这个 offer
        // 启动过一个应答方：直接返回（相同的）缓存 answer，不要再启动
        // 第二个连接任务。否则两个 `create_tcp_connection` 任务都会分离出去并
        // 读取同一个数据通道，交错握手从而破坏会话。
        let Some(mut local_ice_rx) = stream.take_local_ice_rx() else {
            return Ok(answer);
        };

        // 有界：到达多少候选由发送方决定，而取出一个候选的代价是
        // 一次 JSON 解析和 ICE agent 的锁，所以无界的队列会让任何能到达本
        // 会话路径的人在一个长期运行的服务进程里把它无限撑大。队列满时
        // 会丢弃最新的候选，控制端会重发一次——发送方旁边的摘要正是
        // 让这次重发不必占用自己的槽位。
        let (remote_ice_tx, mut remote_ice_rx) = mpsc::channel::<String>(MAX_PENDING_REMOTE_ICE);
        let own_ice_tx = remote_ice_tx.clone();
        WEBRTC_ICE_TXS
            .lock()
            .await
            .insert(session_key.clone(), IceRoute::new(remote_ice_tx));

        let stream_for_remote_ice = stream.clone();
        tokio::spawn(async move {
            while let Some(candidate) = remote_ice_rx.recv().await {
                if let Err(err) = stream_for_remote_ice.add_remote_ice_candidate(&candidate).await
                {
                    if let Some(n) = REJECTED_REMOTE_ICE_LOG.due() {
                        log::warn!(
                            "failed to add {} remote WebRTC ICE candidate(s), last: {}",
                            n,
                            err
                        );
                    }
                }
            }
        });

        {
            let host = self.host.clone();
            let socket_addr = return_route.clone();
            let session_key_for_ice = session_key.clone();
            tokio::spawn(async move {
                // 候选像 answer 一样，走一条专用的到 rendezvous 服务器的 TCP 连接，
                // 而不是 mediator 通道：默认配置下那条通道是 UDP，
                // 而目标部署在 hbbs 前只放 websocket/TCP，那里
                // 它的 UDP 端口不可达。服务器会保持携带候选的 TCP
                // 连接处于打开状态，所以一条按需打开的连接就能服务整个
                // trickle 过程，而 TCP 的可靠性取代了旧的 400ms 重复重发
                // （控制端仍为服务器->对端的 UDP 下行保留自己的重发）。
                let mut conn = None;
                let key = crate::get_key(true).await;
                while let Some(candidate) = local_ice_rx.recv().await {
                    let mut msg = Message::new();
                    msg.set_ice_candidate(IceCandidate {
                        socket_addr: socket_addr.clone(),
                        session_key: session_key_for_ice.clone(),
                        candidate,
                        ..Default::default()
                    });
                    // 每个候选一次重连尝试：在 hbbs
                    // 重启或空闲被杀导致连接失效之后，第一次发送会在陈旧的流上失败。
                    for _ in 0..2 {
                        if conn.is_none() {
                            match connect_tcp(&*host, CONNECT_TIMEOUT).await {
                                Ok(mut s) => {
                                    // 候选就是本机的每个网络接口地址：
                                    // 只在真正加密的通道上发送，否则
                                    // 这次 WebRTC 尝试就没有候选可用。
                                    if let Err(err) = crate::secure_tcp_required(&mut s, &key).await
                                    {
                                        log::warn!(
                                            "failed to secure the WebRTC ICE candidate connection: {}",
                                            err
                                        );
                                        break;
                                    }
                                    conn = Some(s);
                                }
                                Err(err) => {
                                    log::warn!(
                                        "failed to connect for WebRTC ICE candidate: {}",
                                        err
                                    );
                                    break;
                                }
                            }
                        }
                        if let Some(s) = conn.as_mut() {
                            match s.send(&msg).await {
                                Ok(()) => break,
                                Err(err) => {
                                    log::debug!(
                                        "WebRTC ICE candidate send failed, reconnecting: {}",
                                        err
                                    );
                                    conn = None;
                                }
                            }
                        }
                    }
                }
            });
        }

        let session_key_for_cleanup = session_key.clone();
        tokio::spawn(async move {
            let result = stream.wait_connected(CONNECT_TIMEOUT).await;
            // 只清理自己的路径。键是 offer 的 DTLS 指纹，在控制端的
            // 多次打洞重试中是相同的，所以一次建立了新应答方的重试已经
            // 替换了这条记录——盲目删除会删掉活跃会话的发送方，
            // 让它完全收不到候选。
            {
                let mut txs = WEBRTC_ICE_TXS.lock().await;
                if txs
                    .get(&session_key_for_cleanup)
                    .is_some_and(|route| route.is_same_channel(&own_ice_tx))
                {
                    txs.remove(&session_key_for_cleanup);
                }
            }
            if let Err(err) = result {
                log::warn!("webrtc wait_connected failed: {}", err);
                // 现在就释放 pc，而不是等 ICE agent 超时进入
                // 终态（约 30s）；这也会及时移除 SESSIONS 条目。槽位
                // 随之归还，并在拆除完成后才回来，而不是在这个任务
                // 放弃 offer 时就回来：它所代表的是一个为
                // 未认证 offer 建立的对等连接，而一个不会死掉的对等连接照样要付出代价。
                // `pc.close()` 自身没有超时，所以如果在这里就释放槽位，一个永远
                // 不会结束的拆除会让 pc 无限制地堆积，而计数
                // 却显示为零。分离出去后，等待发生在拥有 pc 的 WEBRTC_RT 上，而不是
                // 这个任务上。
                stream.close_detached_with(slot);
                return;
            }
            // 通道已打开：从这里开始会话就是和其他连接一样的连接，
            // 连接层自身的限制对它同样适用。
            drop(slot);
            // create_tcp_connection 接管这个流的所有权；保留一个句柄，以便会话返回后关闭 pc
            // 。它会运行整个会话，正常结束时返回 Ok，
            // 建立失败时返回 Err——无论哪种情况都必须关闭 pc，否则它会永远滞留在
            // SESSIONS 中（它的状态处理函数只在 ICE 终态时触发，而正常
            // 关闭的会话可能永远到不了终态），泄漏 pc、通道和套接字 fd。
            let stream_for_cleanup = stream.clone();
            if let Err(err) = crate::server::create_tcp_connection(
                server,
                Stream::WebRTC(stream),
                peer_addr,
                true,
                meta,
            )
            .await
            {
                log::warn!("failed to create WebRTC server connection: {}", err);
            }
            stream_for_cleanup.close().await;
        });

        Ok(answer)
    }

    async fn handle_punch_hole(&self, ph: PunchHole, server: ServerPtr) -> ResultType<()> {
        let mut peer_addr = AddrMangle::decode(&ph.socket_addr);
        let last = *LAST_MSG.lock().await;
        * LAST_MSG.lock().await = (peer_addr, Instant::now());
        // 跳过重复的打洞消息
        if last.0 == peer_addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let peer_addr_v6 = hbb_common::AddrMangle::decode(&ph.socket_addr_v6);
        let local_proxy = use_ws() || Config::is_proxy();
        let relay = local_proxy || ph.force_relay;
        let mut socket_addr_v6 = Default::default();
        let meta = connection_meta(
            ph.control_permissions.clone().into_option(),
            ph.controlled_context.clone().into_option(),
        );
        // 仅凭控制端的 force_relay 并不能说明 ICE 是否必须是 Relay-only；
        // 要看它的 offer 信封。`ice_policy: "all"` 表示中继是由传输方式
        // （ws）强制造成的，所以用完整的 ICE 应答，让直连配对形成。
        let webrtc_relay_only =
            ph.force_relay && !WebRTCStream::endpoint_declares_all_ice(&ph.webrtc_sdp_offer);
        // 这里不做 enable-webrtc 检查：那是 LocalConfig，由 UI 进程写入且从不
        // 通过 IPC 同步，所以这个（server）进程会读到私有服务器默认值 "N"，
        // 从而恰恰在该传输方式所面向的自托管部署中拒绝应答。
        // 代理仍然会排除这种情况——ICE 会绕过代理并泄露真实 IP。
        let webrtc_viable = !ph.webrtc_sdp_offer.is_empty()
            && !Config::is_proxy()
            && (!webrtc_relay_only || WebRTCStream::has_turn_server());
        let webrtc_sdp_answer = if webrtc_viable {
            self.spawn_webrtc_answerer(
                &ph,
                webrtc_relay_only,
                server.clone(),
                peer_addr,
                meta.clone(),
            )
            .await
            .unwrap_or_else(|err| {
                log::warn!("failed to create WebRTC answer: {}", err);
                String::new()
            })
        } else {
            String::new()
        };
        // 这里已知 v4 各段是否走中继，并据此决定是否占用 v4 槽位
        // ：下面的中继分支会运行整个会话，跨它占着的槽位会让
        // 普通中继流量把这个池子用光。v6 打洞不与它们一起中继——
        // v4 上的对称 NAT 说明不了 v6 的情况——它的槽位在 v4 之后才占用，
        // 否则它可能占用最后一个，让对端指望的那次打洞没有槽位。
        let relay_v4 = ph.nat_type.enum_value() == Ok(NatType::SYMMETRIC)
            || Config::get_nat_type() == NatType::SYMMETRIC as i32
            || relay
            || (config::is_disable_tcp_listen() && ph.udp_port <= 0);
        let punch_udp = !relay_v4 && ph.udp_port > 0;
        let punch_tcp = !relay_v4 && ph.udp_port <= 0 && ph.webrtc_sdp_offer.is_empty();
        let punch_v6 = peer_addr_v6.port() > 0 && !relay;
        let (slot_udp, slot_v6) = UDP_PUNCHES.take_pair(punch_udp, punch_v6);
        let slot_tcp = punch_tcp.then(|| TCP_PUNCHES.take()).flatten();
        if punch_v6 {
            socket_addr_v6 = start_ipv6(
                peer_addr_v6,
                peer_addr,
                server.clone(),
                meta.clone(),
                slot_v6,
            )
            .await;
        }
        let relay_server = self.get_relay_server(ph.relay_server);
        // 为确保安全，websocket 直接走中继
        // 对称 NAT 会中继传统传输方式，但故意不中继 WebRTC：上面构建的 answer
        // 会随中继请求一起发出，而 ICE 会探测候选配对，而不是
        // 相信这个分类，所以在这个分支已经判定为 relay-only 的连接上，
        // 仍然可能形成直连的 WebRTC 配对。不要用 nat_type 来
        // 对应答方设限以让两者保持一致。
        if relay_v4 {
            let uuid = Uuid::new_v4().to_string();
            return self
                .create_relay(
                    ph.socket_addr.into(),
                    relay_server,
                    uuid,
                    server,
                    true,
                    true,
                    socket_addr_v6.clone(),
                    webrtc_sdp_answer.clone(),
                    meta,
                )
                .await;
        }
        use hbb_common::protobuf::Enum;
        let nat_type = NatType::from_i32(Config::get_nat_type()).unwrap_or(NatType::UNKNOWN_NAT);
        let msg_punch = PunchHoleSent {
            socket_addr: ph.socket_addr,
            id: Config::get_id(),
            relay_server,
            nat_type: nat_type.into(),
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            webrtc_sdp_answer,
            ..Default::default()
        };
        if ph.udp_port > 0 {
            peer_addr.set_port(ph.udp_port as u16);
            self.punch_udp_hole(peer_addr, server, msg_punch, meta, slot_udp)
                .await?;
            return Ok(());
        }
        if !ph.webrtc_sdp_offer.is_empty() {
            // 通过自己那条短生命周期的 TCP 连接返回 answer，而不是 mediator
            // 通道：那条通道默认是 UDP，而 hbbs 会对到达它的 PunchHoleSent
            // 套用 UDP 打洞语义（源地址观察）。这里不会做 TCP 打洞
            // ——控制端保留自己的请求套接字用于 trickle ICE。
            let mut msg_out = Message::new();
            msg_out.set_punch_hole_sent(msg_punch);
            let mut socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
            // answer 只在真正加密的通道上发出；否则这次
            // WebRTC 尝试会被放弃，控制端会回退到其他传输方式。
            crate::secure_tcp_required(&mut socket, &crate::get_key(true).await).await?;
            socket.send(&msg_out).await?;
            return Ok(());
        }
        log::debug!("Punch tcp hole to {:?}", peer_addr);
        let mut socket = {
            let socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
            let local_addr = socket.local_addr();
            // 这里 key 很重要，打洞时用它告诉我的网关传入的对端是安全的。
            // 这里用 await 而不是 spawn，这样在 `PunchHoleSent` 发出之前映射就已存在；
            // `local_addr` 本身是共享的而非独占的——这里每个套接字都用
            // `new_socket` 设置的 reuse 标志来绑定它。
            allow_err!(socket_client::connect_tcp_local(peer_addr, Some(local_addr), 30).await);
            socket
        };
        let mut msg_out = Message::new();
        msg_out.set_punch_hole_sent(msg_punch);
        let bytes = msg_out.write_to_bytes()?;
        socket.send_raw(bytes).await?;
        let local_addr = socket.local_addr();
        // 内部的监听器会接管这个地址，所以 mediator 的套接字要先释放。
        drop(socket);
        // 上面已经发出了回复：被拒绝只是监听本身，所以控制端的 TCP 尝试
        // 什么都遇不到，而它的 v6 尝试会继续。
        let Some(slot) = slot_tcp else {
            hbb_common::throttled_log!(
                PUNCH_LOG_INTERVAL,
                warn,
                "declined a TCP punch: {} TCP punches already in flight",
                TCP_PUNCHES.max
            );
            return Ok(());
        };
        punch_tcp_until_connected(server, peer_addr, local_addr, meta, slot).await;
        Ok(())
    }

    async fn punch_udp_hole(
        &self,
        peer_addr: SocketAddr,
        server: ServerPtr,
        msg_punch: PunchHoleSent,
        meta: ConnectionMeta,
        slot: Option<PunchSlot>,
    ) -> ResultType<()> {
        let mut msg_out = Message::new();
        msg_out.set_punch_hole_sent(msg_punch);
        let (socket, addr) = new_direct_udp_for(&self.host).await?;
        let data = msg_out.write_to_bytes()?;
        socket.send_to(&data, addr).await?;
        // 回复已经发出，同时带上了 answer 和 v6 地址：被拒绝只是监听
        // 本身，套接字会立即释放——被拒绝的请求不值得为它的
        // 重发保留一个套接字，控制端会自己重新询问。它对不再响应的映射所做的 v4 尝试
        // 会自行失败，而它其他传输方式的尝试则继续。
        let Some(slot) = slot else {
            hbb_common::throttled_log!(
                PUNCH_LOG_INTERVAL,
                warn,
                "declined a UDP punch: {} UDP punches already in flight",
                UDP_PUNCHES.max
            );
            return Ok(());
        };
        let socket_cloned = socket.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let tm = (hbb_common::time_based_rand() % 20 + 10) as f32 / 1000.;
                hbb_common::sleep(tm).await;
                socket.send_to(&data, addr).await.ok();
            }
        });
        udp_nat_listen(
            socket_cloned.clone(),
            peer_addr,
            peer_addr,
            server,
            meta,
            slot,
        )
        .await?;
        Ok(())
    }

    async fn register_pk(&mut self, socket: Sink<'_>) -> ResultType<()> {
        // 设备等待部署时对 register_pk 限流：服务器
        // 已经告知我们不在它的数据库里；发送频率高于每
        // DEPLOY_RETRY_INTERVAL 毫秒都是浪费流量，直到运维人员运行
        // `rustdesk --deploy --token <api_token>`。
        if NEEDS_DEPLOY.load(Ordering::SeqCst) {
            let mut last = LAST_NOT_DEPLOYED_REGISTER.lock().await;
            if let Some(t) = *last {
                if (t.elapsed().as_millis() as i64) < DEPLOY_RETRY_INTERVAL {
                    return Ok(());
                }
            }
            * last = Some(Instant::now());
        } else {
            * LAST_NOT_DEPLOYED_REGISTER.lock().await = None;
        }
        let mut msg_out = Message::new();
        let pk = Config::get_key_pair().1;
        let uuid = hbb_common::get_uuid();
        let id = Config::get_id();
        msg_out.set_register_pk(RegisterPk {
            id,
            uuid: uuid.into(),
            pk: pk.into(),
            no_register_device: Config::no_register_device(),
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        SENT_REGISTER_PK.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle_uuid_mismatch(&mut self, socket: Sink<'_>) -> ResultType<()> {
        {
            let mut solving = SOLVING_PK_MISMATCH.lock().await;
            if solving.is_empty() || *solving == self.host {
                log::info!("UUID_MISMATCH received from {}", self.host);
                Config::set_key_confirmed(false);
                Config::update_id();
                * solving = self.host.clone();
            } else {
                return Ok(());
            }
        }
        self.register_pk(socket).await
    }

    async fn register_peer(&mut self, socket: Sink<'_>) -> ResultType<()> {
        let solving = SOLVING_PK_MISMATCH.lock().await;
        if !(solving.is_empty() || *solving == self.host) {
            return Ok(());
        }
        drop(solving);
        if !Config::get_key_confirmed() || !Config::get_host_key_confirmed(&self.host_prefix) {
            log::info!(
                "register_pk of {} due to key not confirmed",
                self.host_prefix
            );
            return self.register_pk(socket).await;
        }
        let id = Config::get_id();
        log::trace!(
            "Register my id {:?} to rendezvous server {:?}",
            id,
            self.addr,
        );
        let mut msg_out = Message::new();
        let serial = Config::get_serial();
        msg_out.set_register_peer(RegisterPeer {
            id,
            serial,
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        Ok(())
    }

    fn get_relay_server(&self, provided_by_rendezvous_server: String) -> String {
        let mut relay_server = Config::get_option("relay-server");
        if relay_server.is_empty() {
            relay_server = provided_by_rendezvous_server;
        }
        if relay_server.is_empty() {
            relay_server = crate::increase_port(&self.host, 1);
        }
        relay_server
    }
}

fn get_direct_port() -> i32 {
    let mut port = Config::get_option("direct-access-port")
        .parse::<i32>()
        .unwrap_or(0);
    if port <= 0 {
        port = RENDEZVOUS_PORT + 2;
    }
    port
}

async fn direct_server(server: ServerPtr) {
    let mut listener = None;
    let mut port = 0;
    loop {
        let disabled = !option2bool(
            OPTION_DIRECT_SERVER,
            &Config::get_option(OPTION_DIRECT_SERVER),
        ) || option2bool("stop-service", &Config::get_option("stop-service"));
        if !disabled && listener.is_none() {
            port = get_direct_port();
            match hbb_common::tcp::listen_any(port as _).await {
                Ok(l) => {
                    listener = Some(l);
                    log::info!(
                        "Direct server listening on: {:?}",
                        listener.as_ref().map(|l| l.local_addr())
                    );
                }
                Err(err) => {
                    // 待办：传给 ui
                    log::error!(
                        "Failed to start direct server on port: {}, error: {}",
                        port,
                        err
                    );
                    loop {
                        if port != get_direct_port() {
                            break;
                        }
                        sleep(1.).await;
                    }
                }
            }
        }
        if let Some(l) = listener.as_mut() {
            if disabled || port != get_direct_port() {
                log::info!("Exit direct access listen");
                listener = None;
                continue;
            }
            if let Ok(Ok((stream, addr))) = hbb_common::timeout(1000, l.accept()).await {
                stream.set_nodelay(true).ok();
                log::info!("direct access from {}", addr);
                let local_addr = stream
                    .local_addr()
                    .unwrap_or(Config::get_any_listen_addr(true));
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(
                        crate::server::create_tcp_connection(
                            server,
                            hbb_common::Stream::from(stream, local_addr),
                            addr,
                            false,
                            ConnectionMeta::default(), // Direct connections don't have server-side user context.
                        )
                        .await
                    );
                });
            } else {
                sleep(0.1).await;
            }
        } else {
            sleep(1.).await;
        }
    }
}

enum Sink<'a> {
    Framed(&'a mut FramedSocket, &'a TargetAddr<'a>),
    Stream(&'a mut Stream),
}

impl Sink<'_> {
    async fn send(self, msg: &Message) -> ResultType<()> {
        match self {
            Sink::Framed(socket, addr) => socket.send(msg, addr.to_owned()).await,
            Sink::Stream(stream) => stream.send(msg).await,
        }
    }
}

async fn start_ipv6(
    peer_addr_v6: SocketAddr,
    peer_addr_v4: SocketAddr,
    server: ServerPtr,
    meta: ConnectionMeta,
    slot: Option<PunchSlot>,
) -> bytes::Bytes {
    // 拒绝后由 v4 路径承载连接，就像在这台机器
    // 没有公网 IPv6 地址的地方那样。
    let Some(slot) = slot else {
        hbb_common::throttled_log!(
            PUNCH_LOG_INTERVAL,
            warn,
            "declined an IPv6 punch: {} UDP punches already in flight",
            UDP_PUNCHES.max
        );
        return Default::default();
    };
    crate::test_ipv6().await;
    if let Some((socket, local_addr_v6)) = crate::get_ipv6_socket().await {
        let server = server.clone();
        tokio::spawn(async move {
            allow_err!(
                udp_nat_listen(
                    socket.clone(),
                    peer_addr_v6,
                    peer_addr_v4,
                    server,
                    meta,
                    slot
                )
                .await
            );
        });
        return local_addr_v6;
    }
    Default::default()
}

async fn udp_nat_listen(
    socket: Arc<tokio::net::UdpSocket>,
    peer_addr: SocketAddr,
    peer_addr_v4: SocketAddr,
    server: ServerPtr,
    meta: ConnectionMeta,
    slot: PunchSlot,
) -> ResultType<()> {
    let tm = Instant::now();
    let socket_cloned = socket.clone();
    let func = async {
        socket.connect(peer_addr).await?;
        let init_packet = crate::punch_udp(socket.clone(), true).await?;
        let stream = crate::kcp_stream::KcpStream::accept(
            socket,
            Duration::from_millis(CONNECT_TIMEOUT as _),
            init_packet,
        )
        .await?;
        // KCP 会话已建立：从这里开始它和其他连接一样，连接
        // 层自身的限制对它适用，所以槽位会归还给下一次打洞。
        drop(slot);
        crate::server::create_tcp_connection(server, stream.1, peer_addr_v4, true, meta).await?;
        Ok(())
    };
    func.await.map_err(|e: anyhow::Error| {
        anyhow::anyhow!(
            "Stop listening on {:?} for remote {peer_addr} with KCP, {:?} elapsed: {e}",
            socket_cloned.local_addr(),
            tm.elapsed()
        )
    })?;
    Ok(())
}

/// 重发从哪里开始，以及按什么因子变慢。控制端的 SYN 只到达一次，
/// 在我们从未被告知的时刻，落在我们同样不知情的窗口内：`Client::connect` 根据
/// 我们发出 PunchHoleSent 之后的时间，以及它为我们记录的直连失败次数，
/// 来决定拨号超时——在两个已知不对称且从未失败的 NAT 之间就是 `CONNECT_TIMEOUT`，而
/// 一旦有一个失败就短到一秒。因此重发覆盖的是我们自己的上限，也就是 `CONNECT_TIMEOUT`，
/// 它一直与下面那个 accept 愿意等待连接的时长相同，并在
/// 这段时间内退避：开始时密集，因为每个窗口都从这里开始、在短暂的窗口结束，之后稀疏，
/// 这也是 `punch_udp` 采用同样形状的原因。
const PUNCH_INTERVAL: f32 = 0.15;
const PUNCH_BACKOFF: f32 = 1.5;
const PUNCH_MAX_INTERVAL: f32 = 2.0;
/// 进行中的打洞可以超过截止时间多久运行，以及它唯一依赖的定时器。打洞
/// 在仍处于 SYN_SENT 时取消是安全的，而一旦控制端的 SYN 穿过它就不安全：
/// 此时套接字正处于握手中途，在那里丢弃会切断控制端正在建立的
/// 连接——而它的 `connect` 已经返回，所以那次尝试会
/// 直接失败而不是回退到中继。定时器无法区分这两种状态，所以
/// 不会有打洞按自己的时间表被切断，也不需要有。用 RST 应答的网关
/// 会立刻让 connect 失败，循环于是再次打洞。默默丢弃 SYN 的网关
/// 会让套接字停留在 SYN_SENT，在那里它保持映射打开，内核会重发
/// SYN，而控制端到来的任何 SYN 都会穿过它——第二次打洞没有什么可
/// 补充的。这就只剩下截止时间，允许超过它这么多，好让刚开始的穿越
/// 能够完成；反正 Windows 大约在 21s 时就放弃 SYN。
const PUNCH_GRACE: u64 = 3000;

/// 上面那次打洞在 hbbs 告知控制端拨号目标之前就离开了，因此它绝不会与控制端的 SYN
/// 同时进行：它打开了我们的 NAT，却什么都没遇到，而一个
/// 用 RST 应答它的网关会把映射一起带下去——让下面的监听器一直等着
/// 一个已不存在的洞。在控制端拨号的那个窗口内再次打洞
/// 可以重建它；而一旦控制端处于 SYN_SENT，其中一次打洞就会遇到它的 SYN 并
/// 以同时打开的方式完成：这是第二条进来的路，单次打洞从未有过。
///
/// `slot` 是这次打洞在 `TCP_PUNCHES` 中的槽位，连接一建立就立即归还，
/// 并且在会话运行之前：从那里起连接层自身的限制就适用了。
async fn punch_tcp_until_connected(
    server: ServerPtr,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    meta: ConnectionMeta,
    slot: PunchSlot,
) {
    use hbb_common::tcp::new_listener;
    // 遮蔽模块里的 `std::time::Instant`：截止时间是与 tokio 的 sleep 和
    // timeout 对齐的，所以它按 tokio 的时钟运行。
    use hbb_common::tokio::time::Instant;

    // 本身并不致命——下面的打洞仍能在没有它的情况下遇到控制端的 SYN，
    // 而这一半本来就是操作系统拒绝绑定监听器时无法覆盖的。
    let listener = match new_listener(local_addr, true).await {
        Ok(listener) => {
            log::info!("Server listening on: {local_addr}");
            Some(listener)
        }
        Err(err) => {
            log::warn!("Failed to listen on {local_addr} after punching: {err}");
            None
        }
    };
    // 同时限制两半：打洞只在 accept 还
    // 愿意通过它接受连接期间才保持映射打开。
    let until = Instant::now() + Duration::from_millis(CONNECT_TIMEOUT);
    let punch = punch_until(until, peer_addr, |ms| {
        socket_client::connect_tcp_local(peer_addr, Some(local_addr), ms)
    });
    let Some(listener) = listener else {
        if let Some(stream) = punch.await {
            drop(slot);
            serve_punched(server, stream, peer_addr, meta).await;
        }
        return;
    };
    // 循环 accept，而不是只 accept 一次：一次临时的 `accept` 错误不能耗掉控制端
    // 还需要到达的整个窗口。
    let accept = async {
        loop {
            let left = until.saturating_duration_since(Instant::now()).as_millis() as u64;
            if left == 0 {
                break;
            }
            match hbb_common::timeout(left, listener.accept()).await {
                // 不按地址过滤，`accept_connection` 从来也没这么做过：hbbs 是通过某个映射看到
                // 控制端的，而一个会汇集外部地址的 NAT 可能
                // 从另一个地址拨我们，而阻止第二个对端获得 `meta` 控制权限的
                // 是握手本身，再加上始终只会服务一个连接。
                Ok(Ok(accepted)) => return Some(accepted),
                Ok(Err(err)) => {
                    log::warn!("Failed to accept from {peer_addr}: {err}");
                    // 持续存在的错误——比如 EMFILE——否则会在这里空转到窗口结束。
                    sleep(1.).await;
                }
                Err(_) => break,
            }
        }
        log::info!("Nothing connected to the hole punched to {peer_addr}");
        None
    };
    // 只有 accept 与打洞竞争。改为与 `accept_connection` 竞争会竞争它接下来要运行的
    // 整个会话，那样中途落地的打洞就会把该会话拆掉。
    //
    // 先到者就是这次请求产生的唯一连接。若也服务落败的那一个，
    // 就会把 hbbs 为这个控制端授予的控制权限交给第二个对端，而
    // 在 `create_tcp_connection` 与连接通信之前，连接本身没有任何检查能区分两者——
    // 因此这个不变式在这里靠“不存在第二次服务”来保证。
    let punched = select! {
        // 两者同时就绪是两条连接，而不是同一条被看到两次——一次穿越带着打洞的
        // 四元组，监听器从不匹配它——保留的是打洞那一条：
        // 已知它在 hbbs 给出的地址上遇到过东西，而 accept 会接受
        // 任意地址，丢弃它会重置控制端正在建立的连接。
        biased;
        Some(stream) = punch => stream,
        Some((stream, addr)) = accept => {
            drop(slot);
            return accept_punched_connection(server, stream, addr, meta).await;
        }
        else => return,
    };
    drop(slot);
    serve_punched(server, punched, peer_addr, meta).await;
}

/// 这是 `punch_tcp_until_connected` 的重复机制，作用于任何打洞，而不只是 `connect_tcp_local`
/// 。
async fn punch_until<T, F, Fut>(
    until: tokio::time::Instant,
    peer_addr: SocketAddr,
    mut punch: F,
) -> Option<T>
where
    F: FnMut(u64) -> Fut,
    Fut: std::future::Future<Output = ResultType<T>>,
{
    use hbb_common::tokio::time::Instant;

    let mut interval = PUNCH_INTERVAL;
    let mut round = 0;
    loop {
        // 截止时间只决定是否开始另一次打洞，绝不决定已经在
        // 进行中的打洞能持续多久：那一个会一直跑到超过它 PUNCH_GRACE。
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            log::debug!("None of {round} punches to {peer_addr} was met");
            return None;
        }
        // 在截止时间处切断而不是睡过头，这样窗口结束于一次打洞，而
        // 不是结束于最长可达 PUNCH_MAX_INTERVAL 的空隙：控制端的窗口在我们的窗口之后才打开，
        // 是在 hbbs 中继的 PunchHoleSent 上打开的，所以与我们一样长的窗口在我们的尾段仍然开着。
        tokio::time::sleep(Duration::from_secs_f32(interval).min(left)).await;
        interval = (interval * PUNCH_BACKOFF).min(PUNCH_MAX_INTERVAL);
        let ms = until.saturating_duration_since(Instant::now()).as_millis() as u64 + PUNCH_GRACE;
        match punch(ms).await {
            // 控制端的 SYN 穿过了这次打洞，所以这条流就是它
            // 拨出的那条连接，而不是备用的一条：丢弃它会重置那条连接。
            Ok(stream) => return Some(stream),
            // 不会一条条记录，但计数能看出是哪个网关：RST 会立刻让
            // 一次打洞失败，一个窗口里能塞下十几次；默默丢弃则会占住那唯一一次
            // 打洞直到窗口结束。反正 `connect_tcp_local` 也不保存 errno。
            Err(_) => round += 1,
        }
    }
}

async fn serve_punched(
    server: ServerPtr,
    stream: Stream,
    peer_addr: SocketAddr,
    meta: ConnectionMeta,
) {
    log::info!("Punched tcp hole to {peer_addr}, connected on the punch itself");
    if let Err(err) =
        crate::server::create_tcp_connection(server, stream, peer_addr, true, meta).await
    {
        log::warn!("Failed to serve the connection punched to {peer_addr}: {err}");
    }
}

/// `accept_connection` 中的 accept 那一半，保留在这里是因为只有 accept 可以与打洞竞争。
async fn accept_punched_connection(
    server: ServerPtr,
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    meta: ConnectionMeta,
) {
    use crate::server::create_tcp_connection;

    stream.set_nodelay(true).ok();
    match stream.local_addr() {
        Ok(stream_addr) => {
            let stream = Stream::from(stream, stream_addr);
            if let Err(err) = create_tcp_connection(server, stream, addr, true, meta).await {
                log::warn!("Failed to serve the connection from {addr}: {err}");
            }
        }
        Err(err) => log::warn!("Failed to read the address accepted from {addr}: {err}"),
    }
}

// 当配置尚未从 root 同步过来时，register_pk 可能已经带着新生成的 pk 发出去了。
// 配置同步完成后，pk 可能发生变化。该结构体检测 pk 变化并触发
// 重新注册，方式是把 key_confirmed 设为 false。
// NOTE:
// 这只修正当前 ID 的 PK 注册。如果 root 使用的是非默认的、基于 mac 生成的 ID，
// 它本身并不能解决多 ID 问题。
pub struct CheckIfResendPk {
    pk: Option<Vec<u8>>,
}
impl CheckIfResendPk {
    pub fn new() -> Self {
        Self {
            pk: Config::get_cached_pk(),
        }
    }
}
impl Drop for CheckIfResendPk {
    fn drop(&mut self) {
        if SENT_REGISTER_PK.load(Ordering::SeqCst) && Config::get_cached_pk() != self.pk {
            Config::set_key_confirmed(false);
            log::info!("Set key_confirmed to false due to pk changed, will resend register_pk");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        connection_meta, mpsc, tokio, AnswererSlot, IceRoute, IntoTargetAddr, PunchHoleSent,
        RendezvousMediator, RendezvousMessage, MAX_WEBRTC_ANSWERERS, TCP_PUNCHES, UDP_PUNCHES,
    };
    use hbb_common::protobuf::Message as _;

    // 槽位就是上限，drop 时会归还：超出上限的打洞会被拒绝，
    // 而进行中的那些不受影响，只有在某个槽位
    // 真正归还之后，下一个才会被接纳。
    #[test]
    fn test_punch_slots_cap_and_release() {
        for pool in [&UDP_PUNCHES, &TCP_PUNCHES] {
            let held: Vec<_> = (0..pool.max)
                .map(|_| pool.take().expect("a place up to the limit"))
                .collect();
            assert!(pool.take().is_none(), "the limit is the limit");
            drop(held);
            assert!(pool.take().is_some(), "a place comes back on drop");
        }
    }

    // 被拒绝只是监听，绝不包括回复：没有槽位时 PunchHoleSent 仍会发出，
    // 并携带控制端其他传输方式所依赖的 answer 和 v6 地址，而这次
    // 调用会立即返回——监听则会一直等对端的探测并最终失败。
    #[tokio::test]
    async fn a_declined_udp_punch_still_replies() {
        let hbbs = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host = hbbs.local_addr().unwrap();
        let mediator = RendezvousMediator {
            addr: host.into_target_addr().unwrap(),
            host: host.to_string(),
            host_prefix: String::new(),
            keep_alive: 0,
        };
        let msg_punch = PunchHoleSent {
            webrtc_sdp_answer: "answer".to_owned(),
            socket_addr_v6: bytes::Bytes::from_static(b"v6"),
            ..Default::default()
        };
        mediator
            .punch_udp_hole(
                host,
                crate::server::new_for_test(),
                msg_punch,
                connection_meta(None, None),
                None,
            )
            .await
            .expect("declined is not an error, and not a listen");
        let mut buf = [0u8; 4096];
        let (n, _) = hbb_common::timeout(3000, hbbs.recv_from(&mut buf))
            .await
            .expect("the reply must reach hbbs")
            .unwrap();
        let sent = RendezvousMessage::parse_from_bytes(&buf[..n]).unwrap();
        let sent = sent.punch_hole_sent();
        assert_eq!(sent.webrtc_sdp_answer, "answer");
        assert_eq!(&sent.socket_addr_v6[..], b"v6");
    }

    fn queue(route: &mut IceRoute, candidate: &str) -> bool {
        route.queue(candidate.to_owned())
    }

    #[test]
    fn the_re_sent_copy_does_not_spend_a_queue_slot() {
        // 两个槽位，三次发送：如果没有去重，重发会占用第二个，而 "relay"，
        // 也就是那个要穿越 NAT 的，会被拒绝。
        let (tx, mut rx) = mpsc::channel::<String>(2);
        let mut route = IceRoute::new(tx);
        for _ in 0..2 {
            assert!(queue(&mut route, "host"));
        }
        assert!(queue(&mut route, "relay"));
        let mut queued = Vec::new();
        while let Ok(candidate) = rx.try_recv() {
            queued.push(candidate);
        }
        assert_eq!(queued, vec!["host".to_owned(), "relay".to_owned()]);
    }

    #[test]
    fn test_answerer_slots_cap_and_release() {
        let held: Vec<_> = (0..MAX_WEBRTC_ANSWERERS)
            .map(|_| AnswererSlot::take().unwrap())
            .collect();
        assert!(AnswererSlot::take().is_none());
        drop(held);
        assert!(AnswererSlot::take().is_some());
    }
}
