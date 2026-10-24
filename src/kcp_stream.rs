use hbb_common::{
    anyhow,
    bytes::{Bytes, BytesMut},
    bytes_codec::BytesCodec,
    config, log,
    tcp::{DynTcpStream, FramedStream},
    tokio::{self, net::UdpSocket, sync::mpsc, sync::oneshot},
    tokio_util, ResultType, Stream,
};
use kcp_sys::{
    endpoint::{ConnId, KcpEndpoint},
    packet_def::{KcpPacket, KcpPacketHeader},
    stream,
};
use std::{net::SocketAddr, sync::Arc};

pub struct KcpStream {
    endpoint: KcpEndpoint,
    conn_id: ConnId,
    stop_sender: Option<oneshot::Sender<()>>,
}

const KCP_IO_ERR_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
static KCP_SEND_ERR_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(KCP_IO_ERR_LOG_INTERVAL);
static KCP_RECV_ERR_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(KCP_IO_ERR_LOG_INTERVAL);

impl KcpStream {
    // 选择使用 KCP 内置的拥塞窗口 (nc=0)，而不是一直沿用的纯 turbo 配置
    // (nc=1)；至于为什么这不是默认设置，请参见 `get_kcp_cc_enabled`。
    // 仅作用于发送端，因此不需要线路协商，任一端使用哪种配置都可以。
    // 需要来自 `rustdesk-patches` 分支的 kcp-sys，它把配置工厂接入
    // 连接建立流程（在更早的版本中该工厂只被保存而从未被查询）。
    fn apply_kcp_config(endpoint: &mut KcpEndpoint) {
        if crate::get_kcp_cc_enabled() {
            endpoint.set_kcp_config_factory(Box::new(|conv| {
                let mut config = kcp_sys::ffi_safe::KcpConfig::new_turbo(conv);
                config.nc = Some(0);
                config
            }));
        }
    }

    /// 距离上次从对端收到有效包过了多久，或 `None`（当
    /// 连接消失后）。由 KCP 端点自己的任务回答，而不是由会话的读
    /// 循环回答，因此当该循环忙于发送大消息时它仍然有意义；而且
    /// 端点会足够频繁地对空闲对端发 ping，所以这里的静默意味着对端出问题了，而不是它比较安静。
    pub fn peer_silent_for(&self) -> Option<std::time::Duration> {
        self.endpoint.peer_silent_for(&self.conn_id)
    }

    fn create_framed(stream: stream::KcpStream, local_addr: Option<SocketAddr>) -> Stream {
        Stream::Tcp(FramedStream(
            tokio_util::codec::Framed::new(DynTcpStream(Box::new(stream)), BytesCodec::new()),
            local_addr.unwrap_or(config::Config::get_any_listen_addr(true)),
            None,
            0,
        ))
    }

    pub async fn accept(
        udp_socket: Arc<UdpSocket>,
        timeout: std::time::Duration,
        init_packet: Option<BytesMut>,
    ) -> ResultType<(Self, Stream)> {
        let mut endpoint = KcpEndpoint::new();
        Self::apply_kcp_config(&mut endpoint);
        endpoint.run().await;

        let (input, output) = (
            endpoint.input_sender(),
            endpoint
                .output_receiver()
                .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?,
        );
        let (stop_sender, stop_receiver) = oneshot::channel();
        if let Some(packet) = init_packet {
            if packet.len() >= std::mem::size_of::<KcpPacketHeader>() {
                input.send(packet.into()).await?;
            }
        }
        Self::kcp_io(udp_socket.clone(), input, output, stop_receiver).await;

        let conn_id = tokio::time::timeout(timeout, endpoint.accept()).await??;
        if let Some(stream) = stream::KcpStream::new(&endpoint, conn_id) {
            Ok((
                Self {
                    endpoint,
                    conn_id,
                    stop_sender: Some(stop_sender),
                },
                Self::create_framed(stream, udp_socket.local_addr().ok()),
            ))
        } else {
            Err(anyhow::anyhow!("Failed to create KcpStream"))
        }
    }

    pub async fn connect(
        udp_socket: Arc<UdpSocket>,
        timeout: std::time::Duration,
    ) -> ResultType<(Self, Stream)> {
        let mut endpoint = KcpEndpoint::new();
        Self::apply_kcp_config(&mut endpoint);
        endpoint.run().await;

        let (input, output) = (
            endpoint.input_sender(),
            endpoint
                .output_receiver()
                .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?,
        );
        let (stop_sender, stop_receiver) = oneshot::channel();
        Self::kcp_io(udp_socket.clone(), input, output, stop_receiver).await;

        let conn_id = endpoint.connect(timeout, 0, 0, Bytes::new()).await?;
        if let Some(stream) = stream::KcpStream::new(&endpoint, conn_id) {
            Ok((
                Self {
                    endpoint,
                    conn_id,
                    stop_sender: Some(stop_sender),
                },
                Self::create_framed(stream, udp_socket.local_addr().ok()),
            ))
        } else {
            Err(anyhow::anyhow!("Failed to create KcpStream"))
        }
    }

    async fn kcp_io(
        udp_socket: Arc<UdpSocket>,
        input: mpsc::Sender<KcpPacket>,
        mut output: mpsc::Receiver<KcpPacket>,
        mut stop_receiver: oneshot::Receiver<()>,
    ) {
        let udp = udp_socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0; 1500];
            // 已连接的 UDP 套接字上的套接字错误是 ICMP 不可达 —— 属于提示性的，而且
            // 在打洞形成期间很常见 —— 所以要把它们当作丢包处理，让 KCP 的 pong 超时去淘汰
            // 真正死掉的链路。每个方向一个节流器：该错误只会被报告一次
            // 然后被清除，因此发送成功/接收失败会交替出现，共享计数器永远不会触发。
            loop {
                tokio::select! {
                    _ = &mut stop_receiver => {
                        log::debug!("KCP io loop received stop signal");
                        break;
                    }
                    Some(data) = output.recv() => {
                        if let Err(e) = udp.send(&data.inner()).await {
                            if let Some(n) = KCP_SEND_ERR_LOG.due() {
                                log::debug!("KCP send error x{n} (treated as loss), last: {e}");
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                    result = udp.recv_from(&mut buf) => {
                        match result {
                            Ok((size, _)) => {
                                if size < std::mem::size_of::<KcpPacketHeader>() {
                                    continue;
                                }
                                input
                                    .send(BytesMut::from(&buf[..size]).into())
                                    .await.ok();
                            }
                            Err(e) => {
                                if let Some(n) = KCP_RECV_ERR_LOG.due() {
                                    log::debug!("KCP recv error x{n} (treated as loss), last: {e}");
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        }
                    }
                    else => {
                        log::debug!("KCP endpoint input closed");
                        break;
                    }
                }
            }
        });
    }
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn connected_pair() -> (Arc<UdpSocket>, Arc<UdpSocket>) {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        a.connect(b.local_addr().unwrap()).await.unwrap();
        b.connect(a.local_addr().unwrap()).await.unwrap();
        (Arc::new(a), Arc::new(b))
    }

    async fn establish() -> ((KcpStream, Stream), (KcpStream, Stream)) {
        let (a, b) = connected_pair().await;
        let (accept_res, connect_res) = tokio::join!(
            KcpStream::accept(b, Duration::from_secs(5), None),
            KcpStream::connect(a, Duration::from_secs(5))
        );
        (
            connect_res.expect("connect over loopback"),
            accept_res.expect("accept over loopback"),
        )
    }

    // 在真实回环套接字上的完整客户端路径：通过 kcp_io
    // 泵完成握手，双向传输带帧的数据，然后优雅关闭。端点守卫在整个
    // 流断开期间保持存活，这样 FIN 才能发出去，而且对端带帧的
    // 流必须结束（kcp reader 返回 BrokenPipe），而不是一直挂起。
    #[tokio::test]
    async fn test_kcp_stream_loopback_roundtrip_and_close() {
        let ((_guard_a, mut stream_a), (_guard_b, mut stream_b)) = establish().await;

        stream_a
            .send_bytes(Bytes::from_static(b"ping"))
            .await
            .unwrap();
        let got = stream_b.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(&got[..], b"ping");

        stream_b
            .send_bytes(Bytes::from_static(b"pong"))
            .await
            .unwrap();
        let got = stream_a.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(&got[..], b"pong");

        drop(stream_a);
        match stream_b.next_timeout(10_000).await {
            None | Some(Err(_)) => {}
            Some(Ok(data)) => panic!("unexpected data after close: {:?}", data),
        }
    }

    // 排队许多帧后立即关闭的写入者，不应让
    // 读取者损失其中任何一帧：每一帧都应完整、按序地在流结束之前到达。
    // 这是通过真实的 BytesCodec 组帧对 kcp-sys “关闭时排空尾部”语义的客户端侧固定测试，
    // 而 rustdesk 会话使用的正是这种真实的 BytesCodec 组帧。
    #[tokio::test]
    async fn test_kcp_stream_close_delivers_all_frames() {
        let ((_guard_a, mut tx), (_guard_b, mut rx)) = establish().await;

        const N: usize = 50;
        let payload = vec![7u8; 32 * 1024];
        for _ in 0..N {
            tx.send_bytes(Bytes::from(payload.clone())).await.unwrap();
        }
        drop(tx);

        let mut got = 0usize;
        loop {
            match rx.next_timeout(10_000).await {
                Some(Ok(data)) => {
                    assert_eq!(data.len(), payload.len(), "frame boundary broken");
                    assert!(data.iter().all(|&b| b == 7), "frame content corrupted");
                    got += 1;
                }
                // BrokenPipe（kcp reader 结束）或超时的 None 都会结束该流。
                None | Some(Err(_)) => break,
            }
        }
        assert_eq!(got, N, "graceful close lost frames");
    }
}
