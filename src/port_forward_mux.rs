use hbb_common::{
    bytes::Bytes,
    log,
    tokio::{
        self,
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        sync::{mpsc, watch, Notify},
        time::Instant,
    },
    ResultType,
};
use base::message_proto::*;
use std::sync::{Arc, Mutex};

/// 在线路上且永久固定：控制端最多可以在一个通道上
/// 处于在途状态的数据量，也就是 `opened` 带来对端窗口之前的量。
pub const INITIAL_WINDOW: u32 = 64 * 1024;
/// 同样在线路上且永久固定：一个 `data` 帧无论多长，最少要消耗的窗口量
/// 。它限制了对端可以让我们持有的逐帧记账开销 ——
/// 否则 1 字节的帧只需花掉它 1 字节，却要花掉我们
/// 一个队列条目。
pub const MIN_FRAME_CHARGE: u32 = 64;
pub const CHANNEL_WINDOW: u32 = 256 * 1024;
pub const MAX_FRAME: usize = 64 * 1024;
/// 隧道建立后单个带帧数据包的上限：一个 `MAX_FRAME` 的数据帧，
/// 加上它的 protobuf 外壳和 16 字节 MAC 之后仍有余量。否则 codec
/// 会接受一个声明最大 1 GiB 的首部，而通道窗口
/// 只有在整个数据包到达之后才会被检查。
pub const MAX_PACKET: usize = 2 * MAX_FRAME;
pub const UPDATE_THRESHOLD: u32 = CHANNEL_WINDOW / 2;
pub const MAX_CHANNELS: usize = 256;
pub const DATA_QUEUE_FRAMES: usize = 128;
/// 无论对端提供什么，在途数据永远不能超过我们自己通告的窗口，
/// 受控端的接收端是无界的，所以信用额度是唯一
/// 限制其缓冲多少目标数据的手段，而这个数值由对端决定。
pub const MAX_SEND_CREDIT: u32 = CHANNEL_WINDOW;

pub fn effective_window(advertised: u32) -> u32 {
    advertised.max(INITIAL_WINDOW)
}

/// 一旦双方达成多路复用协议，就用于隧道的流，两侧都是如此。WebSocket
/// 和 WebRTC 的 codec 有各自的上限。
pub fn cap_packet_size(stream: &mut hbb_common::Stream) {
    if let hbb_common::Stream::Tcp(s) = stream {
        s.0.codec_mut().set_max_packet_length(MAX_PACKET);
    }
}

/// 一个该长度的 `data` 帧要消耗其通道窗口多少。
pub fn charge(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX).max(MIN_FRAME_CHARGE)
}

/// 接收端记账：对端仍有多少信用额度，以及自上次
/// `window_update` 以来我们已经排空到本地套接字的量。两者都是
/// 有界的 —— 累计计数器在长时间传输中会耗尽。
pub struct RecvWindow {
    remaining: u32,
    drained_since_update: u32,
}

impl RecvWindow {
    pub fn new(granted: u32) -> Self {
        Self {
            remaining: effective_window(granted),
            drained_since_update: 0,
        }
    }

    /// false 表示对端超出了窗口：属于协议违规。
    pub fn accept(&mut self, len: usize) -> bool {
        match self.remaining.checked_sub(charge(len)) {
            Some(left) => {
                self.remaining = left;
                true
            }
            None => false,
        }
    }

    /// 扩大窗口但不进行通告：该额度随 `opened` 一并传递。
    pub fn grant(&mut self, n: u32) {
        self.remaining = self.remaining.saturating_add(n);
    }

    /// 返回在排空了足够量之后应在 `window_update` 中通告的数量；
    /// 同样的数量会被返还为信用额度。
    pub fn drained(&mut self, n: usize) -> Option<u32> {
        self.drained_since_update = self.drained_since_update.saturating_add(charge(n));
        if self.drained_since_update < UPDATE_THRESHOLD {
            return None;
        }
        let add = self.drained_since_update;
        self.drained_since_update = 0;
        self.remaining = self.remaining.saturating_add(add);
        Some(add)
    }
}

/// 发送端信用额度。`take` 在有额度之前会一直挂起；该锁
/// 绝不会跨 await 持有。
pub struct SendCredit {
    credit: Mutex<u32>,
    notify: Notify,
}

impl SendCredit {
    pub fn new(initial: u32) -> Self {
        Self {
            credit: Mutex::new(initial.min(MAX_SEND_CREDIT)),
            notify: Notify::new(),
        }
    }

    /// `max` 必须至少为 `MIN_FRAME_CHARGE`（`MAX_FRAME` 就是），而且
    /// 调用方要支付 `charge(bytes_read)` 并退还未使用的部分 —— 因此这里会挂起
    /// 直到有整整一份最小额度可用，而不是到零为止。
    pub async fn take(&self, max: usize) -> usize {
        debug_assert!(max >= MIN_FRAME_CHARGE as usize);
        loop {
            {
                let mut credit = self.credit.lock().unwrap();
                if *credit >= MIN_FRAME_CHARGE {
                    let n = (*credit as usize).min(max);
                    * credit -= n as u32;
                    return n;
                }
            }
            self.notify.notified().await;
        }
    }

    pub fn add(&self, n: u32) {
        {
            let mut credit = self.credit.lock().unwrap();
            * credit = credit.saturating_add(n).min(MAX_SEND_CREDIT);
        }
        self.notify.notify_one();
    }

    /// 控制端以 `INITIAL_WINDOW` 的信用额度开始一个通道；当
    /// `opened` 通告对端的真实窗口时，它会以该窗口重新校准。
    pub fn raise_initial(&self, total: u32) {
        let extra = effective_window(total) - INITIAL_WINDOW;
        if extra > 0 {
            self.add(extra);
        }
    }
}

fn channel_msg(union: port_forward_channel::Union) -> Message {
    let mut ch = PortForwardChannel::new();
    ch.union = Some(union);
    let mut msg = Message::new();
    msg.set_port_forward_channel(ch);
    msg
}

pub fn open_msg(id: i32, host: &str, port: i32, window: u32) -> Message {
    channel_msg(port_forward_channel::Union::Open(PortForwardOpen {
        channel_id: id,
        host: host.to_owned(),
        port,
        window,
        ..Default::default()
    }))
}

pub fn opened_msg(id: i32, success: bool, message: &str, window: u32) -> Message {
    channel_msg(port_forward_channel::Union::Opened(PortForwardOpened {
        channel_id: id,
        success,
        message: message.to_owned(),
        window,
        ..Default::default()
    }))
}

pub fn data_msg(id: i32, data: Bytes) -> Message {
    channel_msg(port_forward_channel::Union::Data(PortForwardData {
        channel_id: id,
        data,
        ..Default::default()
    }))
}

pub fn close_msg(id: i32) -> Message {
    channel_msg(port_forward_channel::Union::Close(PortForwardClose {
        channel_id: id,
        ..Default::default()
    }))
}

pub fn window_update_msg(id: i32, add: u32) -> Message {
    channel_msg(port_forward_channel::Union::WindowUpdate(PortForwardWindowUpdate {
        channel_id: id,
        add,
        ..Default::default()
    }))
}

/// 通道的帧发往何处。控制端保留两个队列，这样
/// `window_update` 可以绕过批量数据；受控端拥有连接
/// 唯一的有序 `inner.tx`。`open` 不属于控制消息：它走
/// 有序队列，因此绝不会晚于该通道的第一个 `data` 到达。
#[derive(Clone)]
pub enum FrameSink {
    Queued {
        data: mpsc::Sender<Message>,
        control: mpsc::UnboundedSender<Message>,
    },
    Direct(mpsc::UnboundedSender<(Instant, Arc<Message>)>),
}

fn writer_gone(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, format!("{} gone", what))
}

impl FrameSink {
    pub async fn send_ordered(&self, msg: Message) -> ResultType<()> {
        match self {
            FrameSink::Queued { data, .. } => data
                .send(msg)
                .await
                .map_err(|_| writer_gone("tunnel writer").into()),
            FrameSink::Direct(tx) => tx
                .send((Instant::now(), Arc::new(msg)))
                .map_err(|_| writer_gone("connection writer").into()),
        }
    }

    pub fn send_control(&self, msg: Message) -> ResultType<()> {
        match self {
            FrameSink::Queued { control, .. } => control
                .send(msg)
                .map_err(|_| writer_gone("tunnel writer").into()),
            FrameSink::Direct(tx) => tx
                .send((Instant::now(), Arc::new(msg)))
                .map_err(|_| writer_gone("connection writer").into()),
        }
    }

    pub fn is_closed(&self) -> bool {
        match self {
            FrameSink::Queued { data, .. } => data.is_closed(),
            FrameSink::Direct(tx) => tx.is_closed(),
        }
    }
}

pub enum Inbound {
    Data(Bytes),
    Close,
    /// 解复用器在其窗口内找到了对端；通道任务
    /// 关闭并通知对端，因此该帧仍按序发出。
    Violation,
}

#[derive(Debug, PartialEq)]
pub enum RelayEnd {
    LocalEof,
    PeerClosed,
    Violation,
    Cancelled,
    TunnelGone,
}

/// 本地套接字 -> 隧道，受对端信用额度约束。`prebuf` 就是字节流的
/// 开头部分。
async fn relay_socket_to_tunnel<R: AsyncRead + Unpin>(
    id: i32,
    reader: R,
    prebuf: Vec<u8>,
    credit: Arc<SendCredit>,
    sink: FrameSink,
    mut cancel: watch::Receiver<bool>,
) -> RelayEnd {
    let mut reader = std::io::Cursor::new(prebuf).chain(reader);
    // 每个通道一个暂存缓冲区，每帧做一次精确大小的拷贝：若某帧
    // 拥有自己的读取分配，就会一直占用最多 MAX_FRAME 直到发送出去，
    // 无论其长度如何，而交互式流量大多是很小的帧。
    let mut scratch = vec![0u8; MAX_FRAME];
    loop {
        let allow = tokio::select! {
            n = credit.take(MAX_FRAME) => n,
            _ = cancel.changed() => return RelayEnd::Cancelled,
        };
        let got = tokio::select! {
            r = reader.read(&mut scratch[..allow]) => r.unwrap_or(0),
            _ = cancel.changed() => {
                credit.add(allow as u32);
                return RelayEnd::Cancelled;
            }
        };
        let spent = if got == 0 { 0 } else { charge(got) };
        if (spent as usize) < allow {
            credit.add(allow as u32 - spent);
        }
        if got == 0 {
            return RelayEnd::LocalEof;
        }
        let frame = data_msg(id, Bytes::copy_from_slice(&scratch[..got]));
        if sink.send_ordered(frame).await.is_err() {
            return RelayEnd::TunnelGone;
        }
    }
}

/// 隧道 -> 本地套接字。`initial` 在队列中的任何内容之前被写出
/// （受控端在连接期间缓冲的字节）。
async fn relay_tunnel_to_socket<W: AsyncWrite + Unpin>(
    id: i32,
    mut writer: W,
    initial: Vec<Bytes>,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    window: Arc<Mutex<RecvWindow>>,
    sink: FrameSink,
    mut cancel: watch::Receiver<bool>,
) -> RelayEnd {
    let mut pending: std::collections::VecDeque<Bytes> = initial.into();
    loop {
        let chunk = match pending.pop_front() {
            Some(c) => c,
            None => {
                let next = tokio::select! {
                    n = inbound.recv() => n,
                    _ = cancel.changed() => return RelayEnd::Cancelled,
                };
                match next {
                    Some(Inbound::Data(c)) => c,
                    Some(Inbound::Close) => return RelayEnd::PeerClosed,
                    Some(Inbound::Violation) => return RelayEnd::Violation,
                    None => return RelayEnd::TunnelGone,
                }
            }
        };
        let written = tokio::select! {
            r = writer.write_all(&chunk) => r.is_ok(),
            _ = cancel.changed() => return RelayEnd::Cancelled,
        };
        if !written {
            return RelayEnd::LocalEof;
        }
        let update = window.lock().unwrap().drained(chunk.len());
        if let Some(add) = update {
            if sink.send_control(window_update_msg(id, add)).is_err() {
                return RelayEnd::TunnelGone;
            }
        }
    }
}

/// 以两个独立任务运行两半；先结束的那一半会取消
/// 另一半。在最后的数据之后只发送一次 `close`，且仅当通道
/// 因本地原因结束时才发送 —— 对端自己的 `close` 绝不会被回送。
/// `teardown` 是隧道在通道之下关闭：它会取消两半，
/// 即使它们正挂起在套接字上，因为在那里丢弃入站发送端
/// 两半都触及不到。它是一个电平信号，因此在隧道关闭时打开的通道，
/// 即使在信号发出之后才订阅，也仍然能看到它。
pub async fn run_channel<R, W>(
    id: i32,
    reader: R,
    writer: W,
    prebuf: Vec<u8>,
    initial_out: Vec<Bytes>,
    credit: Arc<SendCredit>,
    window: Arc<Mutex<RecvWindow>>,
    inbound: mpsc::UnboundedReceiver<Inbound>,
    sink: FrameSink,
    mut teardown: watch::Receiver<bool>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut to_tunnel = tokio::spawn(relay_socket_to_tunnel(
        id, reader, prebuf, credit, sink.clone(), cancel_rx.clone(),
    ));
    let mut to_socket = tokio::spawn(relay_tunnel_to_socket(
        id, writer, initial_out, inbound, window, sink.clone(), cancel_rx,
    ));
    let (first, second) = tokio::select! {
        r = &mut to_tunnel => {
            let _ = cancel_tx.send(true);
            (r.unwrap_or(RelayEnd::Cancelled), to_socket.await.unwrap_or(RelayEnd::Cancelled))
        }
        r = &mut to_socket => {
            let _ = cancel_tx.send(true);
            (r.unwrap_or(RelayEnd::Cancelled), to_tunnel.await.unwrap_or(RelayEnd::Cancelled))
        }
        // 包一层，使 `select!` 保留的是 `bool` 而不是 `Ref`（一个读守卫，
        // 不是 `Send`），否则它会跨 join 持有。
        _ = async { teardown.wait_for(|down| *down).await.is_ok() } => {
            let _ = cancel_tx.send(true);
            (to_tunnel.await.unwrap_or(RelayEnd::Cancelled), to_socket.await.unwrap_or(RelayEnd::Cancelled))
        }
    };
    let peer_closed = first == RelayEnd::PeerClosed || second == RelayEnd::PeerClosed;
    let tunnel_gone = first == RelayEnd::TunnelGone || second == RelayEnd::TunnelGone;
    let local_reason = matches!(first, RelayEnd::LocalEof | RelayEnd::Violation)
        || matches!(second, RelayEnd::LocalEof | RelayEnd::Violation);
    if !peer_closed && !tunnel_gone && local_reason {
        if let Err(e) = sink.send_ordered(close_msg(id)).await {
            log::debug!("port forward channel {} close not sent: {}", id, e);
        }
    }
    log::debug!("port forward channel {} ended: {:?} / {:?}", id, first, second);
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use tunnel::{Claim, Tunnel};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod tunnel {
    use super::*;
    use crate::client::Interface;
    use hbb_common::{
        config::READ_TIMEOUT,
        protobuf::Message as _,
        tokio::net::TcpStream,
        Stream,
    };
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicI32, Ordering},
        time::Duration,
    };

    /// 某个拒绝原因只显示一次，之后要等它静默
    /// 这么久才会再次显示：一次页面加载中十几个拒绝只产生一个对话框，而
    /// 数小时后再次出问题的目标则会被重新报告。
    const REPORT_AGAIN_AFTER: Duration = Duration::from_secs(10);
    /// 同时记住不同的原因，使对话框数量保持有界，
    /// 无论对端如何变换它发送的文本。
    const MAX_REPORTED_OPEN_ERRORS: usize = 8;

    // 仅为内部状态；监听器看到的 API 是 `Claim`。
    enum TunnelState {
        Unset,
        Muxed(Arc<TunnelHandle>),
        Legacy,
        Failed,
    }

    pub enum Claim {
        Claimed,
        Muxed(Arc<TunnelHandle>),
        Legacy,
    }

    /// 每个监听器一个。accept 循环持有它，并在两次 accept 之间读取它；
    /// 隧道循环结束时重置它，因此下一次 accept
    /// 会重新建立。
    pub struct Tunnel {
        state: watch::Sender<TunnelState>,
        /// 永远不会发送出去。当监听器丢弃
        /// 这个 `Tunnel` 时，循环的接收端会报错，而这是唯一能结束隧道的途径。
        lifetime: watch::Sender<()>,
    }

    impl Tunnel {
        pub fn new() -> Self {
            let (state, _) = watch::channel(TunnelState::Unset);
            let (lifetime, _) = watch::channel(());
            Self { state, lifetime }
        }

        pub fn claim(&self) -> Claim {
            match &*self.state.borrow() {
                TunnelState::Unset | TunnelState::Failed => Claim::Claimed,
                TunnelState::Muxed(h) => Claim::Muxed(h.clone()),
                TunnelState::Legacy => Claim::Legacy,
            }
        }

        pub fn set_muxed(&self, mut stream: Stream, interface: impl Interface) -> Arc<TunnelHandle> {
            cap_packet_size(&mut stream);
            let (data_tx, data_rx) = mpsc::channel(DATA_QUEUE_FRAMES);
            let (control_tx, control_rx) = mpsc::unbounded_channel();
            let handle = Arc::new(TunnelHandle {
                sink: FrameSink::Queued { data: data_tx, control: control_tx },
                channels: Mutex::new(HashMap::new()),
                next_id: AtomicI32::new(1),
                reported: Default::default(),
                teardown: watch::channel(false).0,
            });
            let state = self.state.clone();
            // 先发布再派生：如果循环先退出并重置了
            // 状态，这里之后再发布就会把它固定在 Muxed 状态，而句柄已失效，
            // 监听器将永远无法重新建立。
            self.state.send_replace(TunnelState::Muxed(handle.clone()));
            tokio::spawn(tunnel_loop(
                stream,
                handle.clone(),
                data_rx,
                control_rx,
                interface,
                state,
                self.lifetime.subscribe(),
            ));
            handle
        }

        pub fn set_legacy(&self) {
            self.state.send_replace(TunnelState::Legacy);
        }

        pub fn set_failed(&self) {
            self.state.send_replace(TunnelState::Failed);
        }
    }

    struct ChannelEntry {
        inbound: mpsc::UnboundedSender<Inbound>,
        credit: Arc<SendCredit>,
        window: Arc<Mutex<RecvWindow>>,
        opened: bool,
    }

    pub struct TunnelHandle {
        sink: FrameSink,
        channels: Mutex<HashMap<i32, ChannelEntry>>,
        next_id: AtomicI32,
        reported: Mutex<HashMap<String, Instant>>,
        /// 由 `close_all` 一次性抛出，针对那些它的 `clear` 无法
        /// 触及的通道：挂起在本地套接字上的那个并不在入站队列中。
        teardown: watch::Sender<bool>,
    }

    impl TunnelHandle {
        /// `open` 从通道自己的任务内部发出，沿有序
        /// 数据队列，抢在该通道第一个 `data` 之前。在控制
        /// 队列上，那个 `data` 可能被抢先，每当隧道循环
        /// 恢复且两个队列都非空时，它就可能被那个 `data` 抢先。
        pub fn open(
            self: &Arc<Self>,
            host: &str,
            port: i32,
            socket: TcpStream,
            prebuf: Vec<u8>,
        ) -> ResultType<()> {
            if self.sink.is_closed() {
                hbb_common::bail!("port forward tunnel is gone");
            }
            let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
            let credit = Arc::new(SendCredit::new(INITIAL_WINDOW));
            let window = Arc::new(Mutex::new(RecvWindow::new(CHANNEL_WINDOW)));
            let id = {
                let mut channels = self.channels.lock().unwrap();
                if channels.len() >= MAX_CHANNELS {
                    hbb_common::bail!("too many port forward channels");
                }
                // 一旦计数器绕回一整圈，它可能落到
                // 一个仍然存在的通道上；由于存在的通道少于 MAX_CHANNELS，所以空闲的
                // id 就在几步之外。
                let mut id = self.next_id.fetch_add(1, Ordering::Relaxed);
                while channels.contains_key(&id) {
                    id = self.next_id.fetch_add(1, Ordering::Relaxed);
                }
                channels.insert(
                    id,
                    ChannelEntry {
                        inbound: inbound_tx,
                        credit: credit.clone(),
                        window: window.clone(),
                        opened: false,
                    },
                );
                id
            };
            let open = open_msg(id, host, port, CHANNEL_WINDOW);
            let (reader, writer) = socket.into_split();
            let sink = self.sink.clone();
            let teardown = self.teardown.subscribe();
            let handle = self.clone();
            tokio::spawn(async move {
                if sink.send_ordered(open).await.is_ok() {
                    run_channel(id, reader, writer, prebuf, Vec::new(), credit, window, inbound_rx, sink, teardown).await;
                }
                handle.channels.lock().unwrap().remove(&id);
            });
            Ok(())
        }

        pub(super) fn on_frame(&self, ch: PortForwardChannel) -> Option<String> {
            match ch.union {
                Some(port_forward_channel::Union::Opened(o)) => {
                    let refused = {
                        let mut channels = self.channels.lock().unwrap();
                        if o.success {
                            // 重复的 `opened` 不得再次提升信用额度。
                            if let Some(e) = channels.get_mut(&o.channel_id) {
                                if !e.opened {
                                    e.opened = true;
                                    e.credit.raise_initial(o.window);
                                }
                            }
                            None
                        } else if let Some(e) = channels.remove(&o.channel_id) {
                            log::debug!("port forward channel {} refused: {}", o.channel_id, o.message);
                            e.inbound.send(Inbound::Close).ok();
                            Some(o.message)
                        } else {
                            None
                        }
                    };
                    refused.and_then(|message| self.first_report(message))
                }
                Some(port_forward_channel::Union::Data(d)) => {
                    let mut channels = self.channels.lock().unwrap();
                    let Some(e) = channels.get(&d.channel_id) else {
                        log::debug!("port forward data for unknown channel {}", d.channel_id);
                        return None;
                    };
                    let accepted = e.window.lock().unwrap().accept(d.data.len());
                    let delivered = accepted && e.inbound.send(Inbound::Data(d.data)).is_ok();
                    if delivered {
                        return None;
                    }
                    // 在此处立即丢弃，因此对端无法为该 id 再排入任何东西，
                    // 即使任务还在退出过程中。
                    if let Some(e) = channels.remove(&d.channel_id) {
                        if !accepted {
                            log::warn!("port forward channel {} overran its window", d.channel_id);
                            e.inbound.send(Inbound::Violation).ok();
                        }
                    }
                    None
                }
                Some(port_forward_channel::Union::Close(c)) => {
                    if let Some(e) = self.channels.lock().unwrap().remove(&c.channel_id) {
                        e.inbound.send(Inbound::Close).ok();
                    }
                    None
                }
                Some(port_forward_channel::Union::WindowUpdate(u)) => {
                    if let Some(e) = self.channels.lock().unwrap().get(&u.channel_id) {
                        e.credit.add(u.add);
                    }
                    None
                }
                Some(port_forward_channel::Union::Open(o)) => {
                    log::debug!("ignoring open for channel {} on the controller", o.channel_id);
                    None
                }
                _ => None,
            }
        }

        /// 对端拒绝某个通道的原因，除非它已在
        /// `REPORT_AGAIN_AFTER` 时间内报告过。一次页面加载可能有十几个
        /// 连接因同一原因被拒绝，而用户只需要一个
        /// 对话框而不是十几个；持续不断的爆发会让它保持安静。
        fn first_report(&self, message: String) -> Option<String> {
            self.first_report_at(message, Instant::now())
        }

        fn first_report_at(&self, message: String, now: Instant) -> Option<String> {
            if message.is_empty() {
                return None;
            }
            let mut reported = self.reported.lock().unwrap();
            reported.retain(|_, last| now.duration_since(*last) < REPORT_AGAIN_AFTER);
            if let Some(last) = reported.get_mut(&message) {
                * last = now;
                return None;
            }
            if reported.len() >= MAX_REPORTED_OPEN_ERRORS {
                return None;
            }
            reported.insert(message.clone(), now);
            Some(message)
        }

        fn close_all(&self) {
            self.channels.lock().unwrap().clear();
            // 不用 `send`：没有活跃通道时它不存储任何东西，而一个
            // 在隧道关闭时打开的通道永远看不到它。
            self.teardown.send_replace(true);
        }

        #[cfg(test)]
        pub fn live_channels(&self) -> usize {
            self.channels.lock().unwrap().len()
        }
    }

    /// 唯一接触该流的任务。三个分支保持 tokio 默认的
    /// 随机公平性：如果用 `biased` 按 控制 -> 读取 -> 数据 的顺序，
    /// 一旦入站饱和就会饿死出站数据（局域网速度的
    /// 下载会让读取分支在每次轮询时都就绪），反过来则会
    /// 饿死承载对端窗口更新和 ping 的读取。
    /// 随机顺序之所以安全，只是因为没有对顺序敏感的内容被拆分
    /// 到不同队列：`open`、`data` 和 `close` 共用数据队列，而
    /// 控制队列只承载 `window_update`。
    async fn tunnel_loop(
        mut stream: Stream,
        handle: Arc<TunnelHandle>,
        mut data_rx: mpsc::Receiver<Message>,
        mut control_rx: mpsc::UnboundedReceiver<Message>,
        interface: impl Interface,
        state: watch::Sender<TunnelState>,
        mut lifetime: watch::Receiver<()>,
    ) {
        let err = loop {
            tokio::select! {
                Some(msg) = control_rx.recv() => {
                    if let Err(e) = stream.send(&msg).await {
                        break format!("send failed: {}", e);
                    }
                }
                res = stream.next_timeout(READ_TIMEOUT) => match res {
                    Some(Ok(bytes)) => {
                        let Ok(msg) = Message::parse_from_bytes(&bytes) else { continue };
                        match msg.union {
                            Some(message::Union::PortForwardChannel(ch)) => {
                                if let Some(err) = handle.on_frame(ch) {
                                    interface.on_error(&err);
                                }
                            }
                            Some(message::Union::TestDelay(t)) => {
                                interface.handle_test_delay(t, &mut stream).await;
                            }
                            Some(message::Union::Misc(misc)) => {
                                if let Some(misc::Union::CloseReason(r)) = misc.union {
                                    break format!("closed by peer: {}", r);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => break format!("read failed: {}", e),
                    None => break "timeout or reset by the peer".to_owned(),
                },
                Some(msg) = data_rx.recv() => {
                    if let Err(e) = stream.send(&msg).await {
                        break format!("send failed: {}", e);
                    }
                }
                _ = lifetime.changed() => break "window closed".to_owned(),
            }
        };
        log::info!("port forward tunnel ended: {}", err);
        handle.close_all();
        state.send_replace(TunnelState::Unset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn tiny_frames_are_charged_at_the_minimum() {
        let mut w = RecvWindow::new(INITIAL_WINDOW);
        for _ in 0..(INITIAL_WINDOW / MIN_FRAME_CHARGE) {
            assert!(w.accept(1));
        }
        // 64 KiB 的窗口能容纳 1024 个 1 字节帧，而不是 65536 个。
        assert!(!w.accept(1));
    }

    #[test]
    fn accounting_survives_a_transfer_far_larger_than_the_window() {
        // 累计计数器过去会在单个通道上约 4 GiB 处溢出，
        // 在传输过程中被读作协议违规。
        let mut w = RecvWindow::new(CHANNEL_WINDOW);
        let mut moved: u64 = 0;
        while moved < 8 * 1024 * 1024 * 1024 {
            assert!(w.accept(MAX_FRAME));
            w.drained(MAX_FRAME);
            moved += MAX_FRAME as u64;
        }
    }

    #[test]
    fn send_credit_blocks_at_zero_and_resumes_on_add() {
        rt().block_on(async {
            let credit = std::sync::Arc::new(SendCredit::new(MIN_FRAME_CHARGE + 4));
            assert_eq!(
                credit.take(MAX_FRAME).await,
                (MIN_FRAME_CHARGE + 4) as usize
            );
            let c = credit.clone();
            let waiter = tokio::spawn(async move { c.take(MAX_FRAME).await });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(!waiter.is_finished());
            // 低于一份最小额度时，领取方保持挂起：它接下来读到的任何内容
            // 都必须是付得起的。
            credit.add(MIN_FRAME_CHARGE - 1);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(!waiter.is_finished());
            credit.add(1);
            assert_eq!(waiter.await.unwrap(), MIN_FRAME_CHARGE as usize);
        });
    }

    #[test]
    fn send_credit_is_capped_whatever_the_peer_advertises() {
        rt().block_on(async {
            let credit = SendCredit::new(u32::MAX);
            assert_eq!(credit.take(usize::MAX).await, MAX_SEND_CREDIT as usize);
            // 窗口更新的洪流也无法把它抬过上限。
            for _ in 0..10 {
                credit.add(u32::MAX);
            }
            assert_eq!(credit.take(usize::MAX).await, MAX_SEND_CREDIT as usize);
        });
    }

    #[test]
    fn raise_initial_rebases_credit_from_initial_window() {
        rt().block_on(async {
            let credit = SendCredit::new(INITIAL_WINDOW);
            assert_eq!(credit.take(1000).await, 1000);
            credit.raise_initial(CHANNEL_WINDOW);
            // 现在信用额度是 CHANNEL_WINDOW - 1000，而不是 CHANNEL_WINDOW - 1000 + INITIAL_WINDOW。
            assert_eq!(
                credit.take(usize::MAX).await,
                (CHANNEL_WINDOW - 1000) as usize
            );
            credit.raise_initial(0);
            // 为零或小于 INITIAL_WINDOW 的通告没有任何作用。
            let c = std::sync::Arc::new(credit);
            let c2 = c.clone();
            let waiter = tokio::spawn(async move { c2.take(MAX_FRAME).await });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(!waiter.is_finished());
            c.add(MIN_FRAME_CHARGE);
            assert_eq!(waiter.await.unwrap(), MIN_FRAME_CHARGE as usize);
        });
    }

    use base::message_proto::{message, port_forward_channel};
    use hbb_common::tokio::{self, io::AsyncReadExt, io::AsyncWriteExt, sync::mpsc};
    use std::sync::{Arc, Mutex};

    struct Harness {
        data_rx: mpsc::Receiver<Message>,
        _control_rx: mpsc::UnboundedReceiver<Message>,
        _inbound_tx: mpsc::UnboundedSender<Inbound>,
        credit: Arc<SendCredit>,
        local: tokio::io::DuplexStream,
        _teardown: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    /// 一个“本地套接字”是双工管道一端、其
    /// “隧道”是测试直接读取的一对队列的通道。
    fn harness(id: i32, prebuf: Vec<u8>, initial_out: Vec<Bytes>) -> Harness {
        harness_on(id, prebuf, initial_out, watch::channel(false).0)
    }

    fn harness_on(id: i32, prebuf: Vec<u8>, initial_out: Vec<Bytes>, teardown: watch::Sender<bool>) -> Harness {
        let (data_tx, data_rx) = mpsc::channel(DATA_QUEUE_FRAMES);
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (local, remote) = tokio::io::duplex(1 << 20);
        let (r, w) = tokio::io::split(remote);
        let credit = Arc::new(SendCredit::new(INITIAL_WINDOW));
        let window = Arc::new(Mutex::new(RecvWindow::new(CHANNEL_WINDOW)));
        let sink = FrameSink::Queued { data: data_tx, control: control_tx };
        let teardown_rx = teardown.subscribe();
        let task = tokio::spawn(run_channel(
            id, r, w, prebuf, initial_out, credit.clone(), window, inbound_rx, sink, teardown_rx,
        ));
        Harness { data_rx, _control_rx: control_rx, _inbound_tx: inbound_tx, credit, local, _teardown: teardown, task }
    }

    // `PortForwardData.data` 是以 `bytes::Bytes` 生成的（hbb_common 构建时
    // 启用了 bytes 功能的 rust-protobuf），因此要用 `to_vec()` 转换，而不是
    // `clone()`，并且在变成 `Inbound::Data` 时无需任何包装。
    fn frame_kind(m: &Message) -> (&'static str, i32, Vec<u8>) {
        match &m.union {
            Some(message::Union::PortForwardChannel(ch)) => match &ch.union {
                Some(port_forward_channel::Union::Data(d)) => ("data", d.channel_id, d.data.to_vec()),
                Some(port_forward_channel::Union::Close(c)) => ("close", c.channel_id, vec![]),
                Some(port_forward_channel::Union::WindowUpdate(u)) => {
                    ("window_update", u.channel_id, u.add.to_le_bytes().to_vec())
                }
                Some(port_forward_channel::Union::Open(o)) => ("open", o.channel_id, vec![]),
                Some(port_forward_channel::Union::Opened(o)) => ("opened", o.channel_id, vec![]),
                None => ("none", 0, vec![]),
                // 在生成的 protobuf 代码中，`port_forward_channel::Union` 是 `#[non_exhaustive]` 的，
                // 所以这里需要一个兜底分支，尽管
                // 上面已经匹配了当前的所有变体。
                _ => ("other", 0, vec![]),
            },
            _ => ("other", 0, vec![]),
        }
    }

    #[test]
    fn send_side_stops_at_credit_and_resumes_on_add() {
        rt().block_on(async {
            let mut h = harness(3, vec![], vec![]);
            let payload = vec![1u8; INITIAL_WINDOW as usize + 5];
            h.local.write_all(&payload).await.unwrap();
            let mut got = 0usize;
            while got < INITIAL_WINDOW as usize {
                got += frame_kind(&h.data_rx.recv().await.unwrap()).2.len();
            }
            assert_eq!(got, INITIAL_WINDOW as usize);
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(50),
                h.data_rx.recv()
            )
            .await
            .is_err());
            // 一份最小额度就足以发送 5 字节的尾部。
            h.credit.add(MIN_FRAME_CHARGE);
            assert_eq!(frame_kind(&h.data_rx.recv().await.unwrap()).2.len(), 5);
        });
    }

    #[test]
    fn local_eof_sends_close_exactly_once_after_the_data() {
        rt().block_on(async {
            let mut h = harness(5, vec![], vec![]);
            h.local.write_all(b"bye").await.unwrap();
            drop(h.local);
            assert_eq!(frame_kind(&h.data_rx.recv().await.unwrap()).0, "data");
            assert_eq!(frame_kind(&h.data_rx.recv().await.unwrap()), ("close", 5, vec![]));
            h.task.await.unwrap();
            assert!(h.data_rx.try_recv().is_err());
        });
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    mod tunnel {
        use super::*;
        use crate::port_forward_mux::{tunnel::TunnelHandle, Claim, Tunnel};
        use hbb_common::{
            protobuf::Message as _,
            tcp::FramedStream,
            tokio::net::{TcpListener, TcpStream},
            Stream,
        };

        /// 一对回环 TCP 连接包装成两个 `Stream`：一个给隧道，
        /// 一个给模拟对端。
        async fn stream_pair() -> (Stream, Stream) {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let (server, _) = l.accept().await.unwrap();
            (
                Stream::Tcp(FramedStream::from(client, addr)),
                Stream::Tcp(FramedStream::from(server, addr)),
            )
        }

        async fn recv_frame(s: &mut Stream) -> PortForwardChannel {
            let bytes = s.next().await.unwrap().unwrap();
            let m = Message::parse_from_bytes(&bytes).unwrap();
            match m.union {
                Some(message::Union::PortForwardChannel(ch)) => ch,
                other => panic!("unexpected {:?}", other),
            }
        }

        async fn local_pair() -> (TcpStream, TcpStream) {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let a = TcpStream::connect(addr).await.unwrap();
            let (b, _) = l.accept().await.unwrap();
            (a, b)
        }

        #[test]
        fn a_packet_declared_over_the_cap_ends_the_tunnel_before_it_arrives() {
            rt().block_on(async {
                use std::time::{Duration, Instant};
                let (ours, mut theirs) = local_pair().await;
                let addr = ours.peer_addr().unwrap();
                let t = Tunnel::new();
                t.claim();
                let _h = t.set_muxed(Stream::Tcp(FramedStream::from(ours, addr)), NoUi::default());
                assert!(matches!(t.claim(), Claim::Muxed(_)));
                // codec 的三字节首部形式，声明比上限多一个字节
                // 且后面没有任何数据：没有上限的 codec 会等待
                // 整个数据包，而隧道保持存活。
                let head = ((MAX_PACKET as u32 + 1) << 2) | 0x2;
                theirs.write_all(&head.to_le_bytes()[..3]).await.unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while !matches!(t.claim(), Claim::Claimed) {
                    assert!(Instant::now() < deadline, "tunnel still up: the oversized header was accepted");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        }

        #[test]
        fn open_sends_open_then_pipelined_data_and_relays_replies() {
            rt().block_on(async {
                let (ours, mut peer) = stream_pair().await;
                let t = Tunnel::new();
                assert!(matches!(t.claim(), Claim::Claimed));
                let h = t.set_muxed(ours, NoUi::default());
                let (mut app, sock) = local_pair().await;
                h.open("localhost", 80, sock, b"GET / HTTP/1.0\r\n\r\n".to_vec()).unwrap();
                let open = recv_frame(&mut peer).await;
                let id = match &open.union {
                    Some(port_forward_channel::Union::Open(o)) => {
                        assert_eq!((o.host.as_str(), o.port, o.window), ("localhost", 80, CHANNEL_WINDOW));
                        o.channel_id
                    }
                    other => panic!("expected open, got {:?}", other),
                };
                let d = recv_frame(&mut peer).await;
                match &d.union {
                    Some(port_forward_channel::Union::Data(d)) => assert_eq!(d.data, b"GET / HTTP/1.0\r\n\r\n".to_vec()),
                    other => panic!("expected data, got {:?}", other),
                }
                peer.send(&opened_msg(id, true, "", CHANNEL_WINDOW)).await.unwrap();
                peer.send(&data_msg(id, Bytes::from_static(b"HTTP/1.0 200 OK\r\n"))).await.unwrap();
                let mut buf = [0u8; 17];
                app.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"HTTP/1.0 200 OK\r\n");
                peer.send(&close_msg(id)).await.unwrap();
                assert_eq!(app.read(&mut buf).await.unwrap(), 0);
            });
        }

        #[test]
        fn dropping_the_tunnel_ends_the_peer_connection() {
            rt().block_on(async {
                let (ours, mut peer) = stream_pair().await;
                let t = Tunnel::new();
                assert!(matches!(t.claim(), Claim::Claimed));
                let h = t.set_muxed(ours, NoUi::default());
                let (app, sock) = local_pair().await;
                h.open("localhost", 80, sock, Vec::new()).unwrap();
                let id = match recv_frame(&mut peer).await.union {
                    Some(port_forward_channel::Union::Open(o)) => o.channel_id,
                    other => panic!("expected open, got {:?}", other),
                };
                peer.send(&close_msg(id)).await.unwrap();
                drop(app);
                // 循环持有自己的句柄，所以丢弃我们的句柄证明不了什么；
                // 真正必须结束对端的是监听器的 `Tunnel`。
                drop(h);
                drop(t);
                let end = hbb_common::timeout(2000, peer.next()).await;
                assert!(
                    matches!(end, Ok(None) | Ok(Some(Err(_)))),
                    "peer still connected: {:?}",
                    end
                );
            });
        }

        #[test]
        fn a_refused_channel_is_reported_once_per_reason() {
            rt().block_on(async {
                let (ours, mut peer) = stream_pair().await;
                let t = Tunnel::new();
                assert!(matches!(t.claim(), Claim::Claimed));
                let ui = NoUi::default();
                let h = t.set_muxed(ours, ui.clone());
                for reason in ["unreachable", "unreachable", "no permission"] {
                    let (mut app, sock) = local_pair().await;
                    h.open("localhost", 1, sock, vec![]).unwrap();
                    let id = match recv_frame(&mut peer).await.union {
                        Some(port_forward_channel::Union::Open(o)) => o.channel_id,
                        other => panic!("expected open, got {:?}", other),
                    };
                    peer.send(&opened_msg(id, false, reason, 0)).await.unwrap();
                    let mut buf = [0u8; 1];
                    assert_eq!(app.read(&mut buf).await.unwrap(), 0);
                }
                // 一次页面加载可能有十几个连接因同一个
                // 原因被拒绝；用户是每个原因一个对话框，而不是每个套接字一个。
                assert_eq!(
                    ui.messages(),
                    vec!["unreachable".to_owned(), "no permission".to_owned()]
                );
            });
        }

        #[test]
        fn an_over_window_frame_drops_the_channel_at_once() {
            rt().block_on(async {
                let (ours, mut peer) = stream_pair().await;
                let t = Tunnel::new();
                t.claim();
                let h = t.set_muxed(ours, NoUi::default());
                let (_app, sock) = local_pair().await;
                h.open("localhost", 1, sock, vec![]).unwrap();
                let id = match recv_frame(&mut peer).await.union {
                    Some(port_forward_channel::Union::Open(o)) => o.channel_id,
                    other => panic!("expected open, got {:?}", other),
                };
                let mut ch = PortForwardChannel::new();
                ch.set_data(PortForwardData {
                    channel_id: id,
                    data: Bytes::from(vec![0u8; CHANNEL_WINDOW as usize + 1]),
                    ..Default::default()
                });
                h.on_frame(ch.clone());
                // 立即消失：持续超出窗口发送的对端
                // 再也无法为该通道排入任何东西。
                assert_eq!(h.live_channels(), 0);
                h.on_frame(ch);
                assert_eq!(h.live_channels(), 0);
            });
        }

        #[test]
        fn tunnel_death_closes_channels_and_resets_state() {
            rt().block_on(async {
                let (ours, peer) = stream_pair().await;
                let t = Tunnel::new();
                t.claim();
                let h = t.set_muxed(ours, NoUi::default());
                let (mut app_a, sock_a) = local_pair().await;
                let (mut app_b, sock_b) = local_pair().await;
                h.open("localhost", 1, sock_a, vec![]).unwrap();
                h.open("localhost", 1, sock_b, vec![]).unwrap();
                drop(peer);
                // 一条隧道就是一个故障域：其上的每个通道都会结束。
                let mut buf = [0u8; 1];
                assert_eq!(app_a.read(&mut buf).await.unwrap(), 0);
                assert_eq!(app_b.read(&mut buf).await.unwrap(), 0);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                assert!(h.open("localhost", 1, local_pair().await.1, vec![]).is_err());
                // 下一次 accept 会在同一个 `Tunnel` 上重新建立。
                assert!(matches!(t.claim(), Claim::Claimed));
                let (ours, mut peer) = stream_pair().await;
                let h = t.set_muxed(ours, NoUi::default());
                let (_app_c, sock_c) = local_pair().await;
                h.open("localhost", 1, sock_c, vec![]).unwrap();
                assert!(matches!(
                    recv_frame(&mut peer).await.union,
                    Some(port_forward_channel::Union::Open(_))
                ));
            });
        }

        /// 一个记录被要求显示哪些对话框的 `Interface`。
        /// 隧道需要它来处理 `handle_test_delay` 以及拒绝消息。
        #[derive(Clone, Default)]
        pub struct NoUi(Arc<Mutex<Vec<String>>>);

        impl NoUi {
            fn messages(&self) -> Vec<String> {
                self.0.lock().unwrap().clone()
            }
        }

        #[async_trait::async_trait]
        impl crate::client::Interface for NoUi {
            fn send(&self, _data: crate::client::Data) {}
            fn msgbox(&self, _msgtype: &str, _title: &str, text: &str, _link: &str) {
                self.0.lock().unwrap().push(text.to_owned());
            }
            fn handle_login_error(&self, _err: &str) -> bool {
                false
            }
            fn handle_peer_info(&self, _pi: PeerInfo) {}
            fn set_multiple_windows_session(&self, _sessions: Vec<WindowsSession>) {}
            async fn handle_hash(&self, _pass: &str, _hash: Hash, _peer: &mut Stream) -> bool {
                false
            }
            async fn handle_login_from_ui(
                &self,
                _os_username: String,
                _os_password: String,
                _password: String,
                _remember: bool,
                _peer: &mut Stream,
            ) {
            }
            async fn handle_test_delay(&self, t: TestDelay, peer: &mut Stream) {
                if !t.from_client {
                    crate::client::handle_test_delay(t, peer).await;
                }
            }
            fn get_lch(&self) -> Arc<std::sync::RwLock<crate::client::LoginConfigHandler>> {
                Arc::new(std::sync::RwLock::new(Default::default()))
            }
        }

        use crate::server::port_forward_mux::PortForwardMux;
        use hbb_common::tokio::time::Instant;

        /// 充当 `Connection` 的替身：一个持有该流的任务，把
        /// `inner.tx` 中的内容排入其中，并把入站帧分发给 mux。
        fn fake_controlled(mut stream: Stream, login_target: String) {
            tokio::spawn(async move {
                let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Arc<Message>)>();
                let mut mux = PortForwardMux::new(tx, login_target);
                loop {
                    tokio::select! {
                        Some((_, m)) = rx.recv() => {
                            if stream.send(&*m).await.is_err() { return; }
                        }
                        res = stream.next() => match res {
                            Some(Ok(bytes)) => {
                                let Ok(m) = Message::parse_from_bytes(&bytes) else { continue };
                                if let Some(message::Union::PortForwardChannel(ch)) = m.union {
                                    mux.handle(ch, || true);
                                }
                            }
                            _ => return,
                        },
                    }
                }
            });
        }

        async fn echo_target() -> u16 {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                loop {
                    let (mut s, _) = l.accept().await.unwrap();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 64 * 1024];
                        loop {
                            let n = s.read(&mut buf).await.unwrap_or(0);
                            if n == 0 || s.write_all(&buf[..n]).await.is_err() { return; }
                        }
                    });
                }
            });
            port
        }

        /// `Tunnel` 也会一起返回：丢弃它才是结束对端的方式，所以
        /// 想要隧道存活的测试必须一直持有它。
        async fn muxed_tunnel() -> (Tunnel, Arc<TunnelHandle>, u16) {
            let (ours, theirs) = stream_pair().await;
            let port = echo_target().await;
            fake_controlled(theirs, format!("127.0.0.1:{}", port));
            let t = Tunnel::new();
            t.claim();
            let h = t.set_muxed(ours, NoUi::default());
            (t, h, port)
        }

        #[test]
        fn many_channels_echo_concurrently() {
            rt().block_on(async {
                let (_t, h, port) = muxed_tunnel().await;
                let mut apps = Vec::new();
                for i in 0..20u8 {
                    let (app, sock) = local_pair().await;
                    h.open("127.0.0.1", port as i32, sock, vec![i]).unwrap();
                    apps.push(app);
                }
                // 二十个通道并发往返：通道 0 流式发送 4 MiB，
                // 另外十九个各自交换一个字节。
                // 用不同的任务分别读写这个大流量套接字：回显只有在
                // 本端在写入的同时持续读取时才能排空。
                let bulk = vec![0xAB; 4 << 20];
                let (mut bulk_rd, mut bulk_wr) = apps.remove(0).into_split();
                let bulk_reader = {
                    let bulk = bulk.clone();
                    tokio::spawn(async move {
                        let mut back = vec![0u8; bulk.len() + 1];
                        bulk_rd.read_exact(&mut back).await.unwrap();
                        assert_eq!(back[0], 0);
                        assert_eq!(&back[1..], &bulk[..]);
                    })
                };
                let bulk_writer = {
                    let bulk = bulk.clone();
                    tokio::spawn(async move {
                        bulk_wr.write_all(&bulk).await.unwrap();
                        // 保持写的一半处于打开状态：丢弃它会半关闭该
                        // 套接字，按设计这会结束整个通道。
                        bulk_wr
                    })
                };
                for (i, app) in apps.iter_mut().enumerate() {
                    let mut b = [0u8; 1];
                    tokio::time::timeout(std::time::Duration::from_secs(2), app.read_exact(&mut b))
                        .await
                        .expect("small channel starved")
                        .unwrap();
                    assert_eq!(b[0], (i + 1) as u8);
                }
                bulk_reader.await.unwrap();
                let _bulk_wr = bulk_writer.await.unwrap();
            });
        }

        #[test]
        fn one_byte_frames_never_trip_the_window() {
            rt().block_on(async {
                let (_t, h, port) = muxed_tunnel().await;
                let (mut app, sock) = local_pair().await;
                h.open("127.0.0.1", port as i32, sock, vec![]).unwrap();
                for i in 0..5000u32 {
                    app.write_all(&[(i % 251) as u8]).await.unwrap();
                    app.flush().await.unwrap();
                }
                let mut back = vec![0u8; 5000];
                app.read_exact(&mut back).await.unwrap();
                for (i, b) in back.iter().enumerate() {
                    assert_eq!(*b, (i as u32 % 251) as u8);
                }
            });
        }

        #[test]
        fn sequential_connections_far_beyond_max_channels_all_succeed() {
            rt().block_on(async {
                let (_t, h, port) = muxed_tunnel().await;
                for i in 0..(MAX_CHANNELS * 3) {
                    let (mut app, sock) = local_pair().await;
                    h.open("127.0.0.1", port as i32, sock, vec![i as u8]).unwrap();
                    let mut b = [0u8; 1];
                    app.read_exact(&mut b).await.unwrap();
                    assert_eq!(b[0], i as u8);
                    drop(app);
                    // 当控制端的协调任务退出时，控制端的条目随之消失，
                    // 这需要一次取消和一次 join；要等待它，而不是
                    // 只靠一次 yield，否则上限大约在第 256 轮就会触发。
                    while h.live_channels() != 0 {
                        tokio::task::yield_now().await;
                    }
                }
            });
        }
    }
}
