use super::connection::{Connection, Sender};
use crate::port_forward_mux::{
    charge, close_msg, effective_window, opened_msg, run_channel, FrameSink, Inbound, RecvWindow,
    SendCredit, CHANNEL_WINDOW, INITIAL_WINDOW, MAX_CHANNELS,
};
use hbb_common::{
    bytes::Bytes,
    log,
    timeout,
    tokio::{self, net::TcpStream, sync::{mpsc, watch}},
};
use base::message_proto::*;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const CONNECT_TIMEOUT_MS: u64 = 3000;

/// 在 `opened` 之前，控制端可能只用过 `INITIAL_WINDOW`。
/// `charged` 是 `charge(len)` 的累计值，而不是原始长度之和。
fn pending_fits(charged: usize, add_len: usize) -> bool {
    charged.saturating_add(charge(add_len) as usize) <= INITIAL_WINDOW as usize
}

struct Entry {
    inbound: mpsc::UnboundedSender<Inbound>,
    credit: Arc<SendCredit>,
    window: Arc<Mutex<RecvWindow>>,
}

/// 一条多路复用隧道的被控端。主循环持有它，并
/// 把每一个 `PortForwardChannel` 帧都转发到这里；每个通道是一个任务。
pub struct PortForwardMux {
    channels: HashMap<i32, Entry>,
    tx: Sender,
    login_target: String,
    /// 由 `close_all` 为其 `clear` 无法触及的通道一次性触发：
    /// 停在目标套接字上的那个并不在入站队列上。
    teardown: watch::Sender<bool>,
}

impl PortForwardMux {
    pub fn new(tx: Sender, login_target: String) -> Self {
        Self {
            channels: HashMap::new(),
            tx,
            login_target,
            teardown: watch::channel(false).0,
        }
    }

    /// `tunnel_permitted` 只在 `open` 时查询，因此这次查找并不是
    /// 每 64 KiB 数据做一次。
    pub fn handle(&mut self, frame: PortForwardChannel, tunnel_permitted: impl FnOnce() -> bool) {
        match frame.union {
            Some(port_forward_channel::Union::Open(open)) => {
                let permitted = tunnel_permitted();
                self.on_open(open, permitted)
            }
            Some(port_forward_channel::Union::Data(d)) => {
                let len = d.data.len();
                let Some(entry) = self.channels.get(&d.channel_id) else {
                    log::debug!("port forward data for unknown channel {}", d.channel_id);
                    return;
                };
                let accepted = entry.window.lock().unwrap().accept(len);
                let delivered = accepted && entry.inbound.send(Inbound::Data(d.data)).is_ok();
                if delivered {
                    return;
                }
                // 就在这里丢弃，这样对端就无法再为
                // 这个 id 排队任何东西，而任务还在退出途中。
                let Some(entry) = self.channels.remove(&d.channel_id) else {
                    return;
                };
                if !accepted {
                    log::warn!("port forward channel {} overran its window", d.channel_id);
                    entry.inbound.send(Inbound::Violation).ok();
                }
            }
            Some(port_forward_channel::Union::Close(c)) => {
                if let Some(entry) = self.channels.remove(&c.channel_id) {
                    entry.inbound.send(Inbound::Close).ok();
                } else {
                    log::debug!("port forward close for unknown channel {}", c.channel_id);
                }
            }
            Some(port_forward_channel::Union::WindowUpdate(u)) => {
                match self.channels.get(&u.channel_id) {
                    Some(entry) => entry.credit.add(u.add),
                    None => log::debug!(
                        "port forward window update for unknown channel {}",
                        u.channel_id
                    ),
                }
            }
            Some(port_forward_channel::Union::Opened(o)) => {
                log::debug!("ignoring opened for channel {} on the controlled side", o.channel_id);
            }
            _ => {}
        }
    }

    fn on_open(&mut self, open: PortForwardOpen, permitted: bool) {
        let id = open.channel_id;
        self.channels.retain(|_, e| !e.inbound.is_closed());
        if !permitted {
            self.reply(opened_msg(id, false, "No permission of IP tunneling", 0));
            return;
        }
        if self.channels.len() >= MAX_CHANNELS {
            self.reply(opened_msg(id, false, "Too many port forward channels", 0));
            return;
        }
        if self.channels.contains_key(&id) {
            log::debug!("ignoring open for live channel {}", id);
            return;
        }
        let mut pf = PortForward {
            host: open.host,
            port: open.port,
            ..Default::default()
        };
        let (addr, is_rdp) = Connection::normalize_port_forward_target(&mut pf);
        // 批准与权限检查看到的是该次登录的目标；一条隧道
        // 只服务那一个目标，别无其他。
        if addr != self.login_target {
            log::warn!(
                "port forward channel {} asked for {} on a tunnel logged in for {}",
                id,
                addr,
                self.login_target
            );
            self.reply(opened_msg(id, false, "Port forward target not authorized", 0));
            return;
        }
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let credit = Arc::new(SendCredit::new(effective_window(open.window)));
        let window = Arc::new(Mutex::new(RecvWindow::new(INITIAL_WINDOW)));
        self.channels.insert(
            id,
            Entry {
                inbound: inbound_tx,
                credit: credit.clone(),
                window: window.clone(),
            },
        );
        tokio::spawn(run_controlled_channel(
            id,
            addr,
            is_rdp,
            credit,
            window,
            inbound_rx,
            FrameSink::Direct(self.tx.clone()),
            self.teardown.subscribe(),
        ));
    }

    fn reply(&self, msg: Message) {
        self.tx
            .send((tokio::time::Instant::now(), Arc::new(msg)))
            .ok();
    }

    #[cfg(test)]
    pub fn live_channels(&self) -> usize {
        self.channels.len()
    }

    /// 每个任务结束时都会丢弃它的目标套接字：队列中的发送方针对的是
    /// 队列上的任务，而 `teardown` 能触及停在套接字上的任务。
    pub fn close_all(&mut self) {
        self.channels.clear();
        // 不是 `send`：没有活跃通道时它什么都不存，而一个已打开的
        // 通道在隧道关闭时是看不到它的。
        self.teardown.send_replace(true);
    }
}

/// 负责通道的整个生命周期：在一个 `select!` 中连接，其中
/// 排队的命令总是优先于连接，同时缓存到达的数据，
/// 然后再转发。
async fn run_controlled_channel(
    id: i32,
    addr: String,
    is_rdp: bool,
    credit: Arc<SendCredit>,
    window: Arc<Mutex<RecvWindow>>,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    sink: FrameSink,
    teardown: watch::Receiver<bool>,
) {
    let mut pending: Vec<Bytes> = Vec::new();
    let mut pending_len = 0usize;
    let connect = timeout(CONNECT_TIMEOUT_MS, TcpStream::connect(&addr));
    tokio::pin!(connect);
    let socket = loop {
        tokio::select! {
            // 带偏置，把命令分支放在前面：一个已经排队的 `close`
            // 必须优先于同一次轮询中完成的连接，
            // 否则 `opened` 就会为一个控制端已经丢弃的通道发出去。
            biased;
            cmd = inbound.recv() => match cmd {
                Some(Inbound::Data(b)) => {
                    if !pending_fits(pending_len, b.len()) {
                        log::warn!("port forward channel {} sent more than INITIAL_WINDOW before opened", id);
                        sink.send_ordered(close_msg(id)).await.ok();
                        return;
                    }
                    pending_len += charge(b.len()) as usize;
                    pending.push(b);
                }
                Some(Inbound::Close) | None => return,
                Some(Inbound::Violation) => {
                    sink.send_ordered(close_msg(id)).await.ok();
                    return;
                }
            },
            res = &mut connect => {
                let err = match res {
                    Ok(Ok(s)) => break s,
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                };
                log::debug!("port forward channel {} connect {} failed: {}", id, addr, err);
                sink.send_ordered(opened_msg(id, false, &unreachable_message(&addr, is_rdp), 0)).await.ok();
                return;
            }
        }
    };
    // 在 `opened` 发出之前就已授予，因此对端绝不会跑在它前面。
    window.lock().unwrap().grant(CHANNEL_WINDOW - INITIAL_WINDOW);
    if sink
        .send_ordered(opened_msg(id, true, "", CHANNEL_WINDOW))
        .await
        .is_err()
    {
        return;
    }
    let (reader, writer) = socket.into_split();
    run_channel(id, reader, writer, Vec::new(), pending, credit, window, inbound, sink, teardown).await;
}

/// 与原始管道在其登录错误中使用的措辞相同，因此同一个问题无论
/// 对端走哪条路径读起来都一样。
fn unreachable_message(addr: &str, is_rdp: bool) -> String {
    format!(
        "Failed to access remote {}. Please make sure it is reachable/open.",
        if is_rdp { "RDP" } else { addr }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_forward_mux::{CHANNEL_WINDOW, MAX_CHANNELS};
    use hbb_common::{
        tokio::{
            self,
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            sync::mpsc,
            time::Instant,
        },
    };
    use base::message_proto::{message, port_forward_channel};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// 一个充当转发目标替身的回显服务器。
    async fn echo_target() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    fn open(id: i32, port: u16) -> PortForwardChannel {
        let mut ch = PortForwardChannel::new();
        ch.set_open(PortForwardOpen {
            channel_id: id,
            host: "127.0.0.1".to_owned(),
            port: port as i32,
            window: CHANNEL_WINDOW,
            ..Default::default()
        });
        ch
    }

    fn data(id: i32, bytes: &[u8]) -> PortForwardChannel {
        let mut ch = PortForwardChannel::new();
        ch.set_data(PortForwardData {
            channel_id: id,
            data: Bytes::copy_from_slice(bytes),
            ..Default::default()
        });
        ch
    }

    fn close(id: i32) -> PortForwardChannel {
        let mut ch = PortForwardChannel::new();
        ch.set_close(PortForwardClose { channel_id: id, ..Default::default() });
        ch
    }

    async fn next_frame(rx: &mut mpsc::UnboundedReceiver<(Instant, Arc<Message>)>) -> PortForwardChannel {
        let (_, m) = rx.recv().await.unwrap();
        match &m.union {
            Some(message::Union::PortForwardChannel(ch)) => ch.clone(),
            other => panic!("unexpected {:?}", other),
        }
    }

    fn opened(ch: &PortForwardChannel) -> (i32, bool) {
        match &ch.union {
            Some(port_forward_channel::Union::Opened(o)) => (o.channel_id, o.success),
            other => panic!("expected opened, got {:?}", other),
        }
    }

    fn data_of(ch: &PortForwardChannel) -> (i32, Vec<u8>) {
        match &ch.union {
            Some(port_forward_channel::Union::Data(d)) => (d.channel_id, d.data.to_vec()),
            other => panic!("expected data, got {:?}", other),
        }
    }

    #[test]
    fn open_connects_and_echoes_pipelined_data() {
        rt().block_on(async {
            let port = echo_target().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", port));
            mux.handle(open(1, port), || true);
            mux.handle(data(1, b"ping"), || true);
            assert_eq!(opened(&next_frame(&mut rx).await), (1, true));
            assert_eq!(data_of(&next_frame(&mut rx).await), (1, b"ping".to_vec()));
            mux.handle(close(1), || true);
        });
    }

    #[test]
    fn unreachable_target_fails_open_and_discards_pipelined_data() {
        rt().block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            drop(l);
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", port));
            mux.handle(open(1, port), || true);
            mux.handle(data(1, b"lost"), || true);
            assert_eq!(opened(&next_frame(&mut rx).await), (1, false));
            assert!(tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await.is_err());
        });
    }

    #[test]
    fn a_revoked_permission_refuses_new_channels_and_keeps_live_ones() {
        rt().block_on(async {
            let port = echo_target().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", port));
            mux.handle(open(1, port), || true);
            assert_eq!(opened(&next_frame(&mut rx).await), (1, true));
            // 每次 `open` 都会查询 `enable-tunnel`，因此在会话中途关闭它
            // 会阻止新建通道；已有的那个继续转发。
            mux.handle(open(2, port), || false);
            assert_eq!(opened(&next_frame(&mut rx).await), (2, false));
            mux.handle(data(1, b"still relayed"), || false);
            assert_eq!(data_of(&next_frame(&mut rx).await), (1, b"still relayed".to_vec()));
            assert_eq!(mux.live_channels(), 1);
        });
    }

    #[test]
    fn over_window_data_closes_only_that_channel() {
        rt().block_on(async {
            let port = echo_target().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", port));
            mux.handle(open(1, port), || true);
            mux.handle(open(2, port), || true);
            let mut seen = 0;
            while seen < 2 {
                opened(&next_frame(&mut rx).await);
                seen += 1;
            }
            let too_much = vec![0u8; CHANNEL_WINDOW as usize + 1];
            mux.handle(data(1, &too_much), || true);
            let ch = next_frame(&mut rx).await;
            match &ch.union {
                Some(port_forward_channel::Union::Close(c)) => assert_eq!(c.channel_id, 1),
                other => panic!("expected close, got {:?}", other),
            }
            mux.handle(data(2, b"still fine"), || true);
            assert_eq!(data_of(&next_frame(&mut rx).await), (2, b"still fine".to_vec()));
        });
    }

    #[test]
    fn an_over_window_frame_drops_the_channel_at_once() {
        rt().block_on(async {
            let port = echo_target().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", port));
            mux.handle(open(1, port), || true);
            opened(&next_frame(&mut rx).await);
            let too_much = vec![0u8; CHANNEL_WINDOW as usize + 1];
            mux.handle(data(1, &too_much), || true);
            // 在通道任务运行之前就消失了：对端继续为
            // 这个 id 发送的任何东西都无法再排队。
            assert_eq!(mux.live_channels(), 0);
            mux.handle(data(1, &too_much), || true);
            assert_eq!(mux.live_channels(), 0);
            let ch = next_frame(&mut rx).await;
            match &ch.union {
                Some(port_forward_channel::Union::Close(c)) => assert_eq!(c.channel_id, 1),
                other => panic!("expected close, got {:?}", other),
            }
        });
    }

    #[test]
    fn open_to_a_target_other_than_the_login_target_is_refused() {
        rt().block_on(async {
            let a = echo_target().await;
            let b = echo_target().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", a));
            mux.handle(open(1, a), || true);
            assert_eq!(opened(&next_frame(&mut rx).await), (1, true));
            // 批准的是目标 a；b 需要自己的登录。
            mux.handle(open(2, b), || true);
            let ch = next_frame(&mut rx).await;
            match &ch.union {
                Some(port_forward_channel::Union::Opened(o)) => {
                    assert_eq!((o.channel_id, o.success), (2, false));
                    assert!(!o.message.is_empty());
                }
                other => panic!("expected opened, got {:?}", other),
            }
            assert_eq!(mux.live_channels(), 1);
        });
    }

    /// 一个接受连接后立刻挂断的目标，因此每个通道都结束于
    /// 目标的 EOF——这正是只有下一次 `open` 才会释放该条目的情况。
    async fn drop_target() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                drop(s);
            }
        });
        port
    }

    #[test]
    fn open_frees_dead_entries_so_the_cap_counts_live_channels() {
        rt().block_on(async {
            let port = drop_target().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut mux = PortForwardMux::new(tx, format!("127.0.0.1:{}", port));
            for id in 1..=(MAX_CHANNELS as i32 * 2) {
                mux.handle(open(id, port), || true);
                assert_eq!(opened(&next_frame(&mut rx).await), (id, true));
                // 任务在目标的 EOF 处发送 `close` 并退出；该
                // 条目一直失效，直到下一次 `open` 把它丢弃。
                let ch = next_frame(&mut rx).await;
                match &ch.union {
                    Some(port_forward_channel::Union::Close(c)) => assert_eq!(c.channel_id, id),
                    other => panic!("expected close, got {:?}", other),
                }
                tokio::task::yield_now().await;
            }
        });
    }
}
