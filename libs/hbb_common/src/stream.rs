use crate::{bail, tcp, websocket, ResultType};
#[cfg(feature = "webrtc")]
use crate::webrtc;
use sodiumoxide::crypto::secretbox::Key;
use std::net::SocketAddr;
use tokio::net::TcpStream;

// 支持 Websocket 和 tcp。
pub enum Stream {
    #[cfg(feature = "webrtc")]
    WebRTC(webrtc::WebRTCStream),
    WebSocket(websocket::WsFramedStream),
    Tcp(tcp::FramedStream),
}

impl Stream {
    #[inline]
    pub fn set_send_timeout(&mut self, ms: u64) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_send_timeout(ms),
            Stream::WebSocket(s) => s.set_send_timeout(ms),
            Stream::Tcp(s) => s.set_send_timeout(ms),
        }
    }

    #[inline]
    pub fn set_raw(&mut self) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_raw(),
            Stream::WebSocket(s) => s.set_raw(),
            Stream::Tcp(s) => s.set_raw(),
        }
    }

    /// 对端可能发送的最大消息长度。在鉴权之前会被调低，此时对端
    /// 合法发送的内容都很小，因此未鉴权的连接无法让我们持有超过
    /// 该值的消息。每种传输都会把 `n` 钳制到各自的上限，因此 `usize::MAX` 表示恢复默认值。
    ///
    /// 应在首次读取不可信数据之前设置。之后再调低并不能限制
    /// 已在进行中的消息：这对其产生什么影响取决于具体传输。
    #[inline]
    pub fn set_max_packet_length(&mut self, n: usize) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_max_packet_length(n),
            Stream::WebSocket(s) => s.set_max_packet_length(n),
            Stream::Tcp(s) => s.codec_mut().set_max_packet_length(n),
        }
    }

    #[inline]
    pub async fn send_bytes(&mut self, bytes: bytes::Bytes) -> ResultType<()> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.send_bytes(bytes).await,
            Stream::WebSocket(s) => s.send_bytes(bytes).await,
            Stream::Tcp(s) => s.send_bytes(bytes).await,
        }
    }

    #[inline]
    pub async fn send_raw(&mut self, bytes: Vec<u8>) -> ResultType<()> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.send_raw(bytes).await,
            Stream::WebSocket(s) => s.send_raw(bytes).await,
            Stream::Tcp(s) => s.send_raw(bytes).await,
        }
    }

    #[inline]
    pub fn set_key(&mut self, key: Key) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.set_key(key),
            Stream::WebSocket(s) => s.set_key(key),
            Stream::Tcp(s) => s.set_key(key),
        }
    }

    /// `t.picked` 中密钥交换版本对应的流密钥：版本 0 是 `set_key`，而
    /// 版本 1 从 transcript 中为每个方向派生一把密钥。其他任何版本都会被拒绝，
    /// 而不是按其中之一运行。`is_initiator` 是发送 sealed key 的一方。
    #[inline]
    pub fn set_negotiated_key(
        &mut self,
        key: Key,
        is_initiator: bool,
        t: &tcp::KxTranscript,
    ) -> ResultType<()> {
        match t.picked {
            0 => {
                self.set_key(key);
                Ok(())
            }
            1 => match self {
                #[cfg(feature = "webrtc")]
                Stream::WebRTC(_) => bail!("key exchange version 1 does not apply to WebRTC"),
                Stream::WebSocket(s) => s.set_key_split(key, is_initiator, t),
                Stream::Tcp(s) => s.set_key_split(key, is_initiator, t),
            },
            v => bail!("unsupported key exchange version {}", v),
        }
    }

    /// 一个不透明的令牌，每当有来自对端的字节到达时就会变化，尚未收全的消息片段
    /// 也包括在内。比较相邻的采样值即可；该值本身
    /// 没有意义。传输不报告此类信息时为 `None`。
    ///
    /// 消息只有完整后才会到达调用方，因此单靠 `next()` 无法区分
    /// 正在发送大消息的对端（剪贴板图片就是常见情形）与
    /// 在消息中途掉线的对端。把它与最后一条收到的消息一起采样即可区分二者。
    ///
    /// TCP 和 WebSocket 故意返回 `None`：本次改动保留了它们原有的关闭、错误
    /// 和 30 秒超时行为完全不变，给它们赋值反而会改变它。
    #[inline]
    pub fn rx_progress(&self) -> Option<u64> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => Some(s.rx_progress()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// 在 WebRTC 传输上，ICE 是否已停止收到对端的数据。是暂时的、
    /// 可恢复的——它只说明值得怀疑对端，绝不表示会话已结束——并且
    /// 对于本次改动未涉及的传输，它始终为 false。
    ///
    /// 由 ICE 自己的任务维护，因此与任何从会话读取循环派生出来的量不同，它
    /// 在该循环忙于发送时仍然保持准确。
    #[inline]
    pub fn webrtc_disconnected(&self) -> bool {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.is_disconnected(),
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    #[inline]
    pub fn is_secured(&self) -> bool {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.is_secured(),
            Stream::WebSocket(s) => s.is_secured(),
            Stream::Tcp(s) => s.is_secured(),
        }
    }

    /// 这是否是 WebRTC 传输。用于在安全握手中强制执行仅限 WebRTC 的 DTLS 指纹
    /// 绑定（fail closed）。
    #[inline]
    pub fn is_webrtc(&self) -> bool {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(_) => true,
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// 如果这是 WebRTC 流，则关闭底层的 WebRTC peer connection；否则不做任何事。
    /// WebRTC pc 由全局 session 缓存保持存活，其清理处理器只在
    /// 出现终止性 ICE 状态时才触发，因此必须在会话结束时显式关闭。TCP/WebSocket 流
    /// 在 drop 时释放资源，这里无需做任何处理。
    ///
    /// 刻意不做成 `async`：大多数调用方位于 `select!` 的某个分支，或位于 UI 可以
    /// 放弃的 future 中；而一个在竞态中落败的 await 关闭是无法重试的——`close()` 会
    /// 在首次 await 之前就锁定 `is_closed`，因此之后每次尝试都会提前返回，而那个本该
    /// 驱逐 session 的状态处理器永远不会执行。这里没有 await 点，因此
    /// 没有任何东西需要取消；拆除过程会在 WebRTC I/O 运行时上
    /// （见 `webrtc::WEBRTC_RT`）上跑完，该运行时拥有 pc 的 socket 和泵任务，因此即使调用方自己的运行时在返回时
    /// 已经销毁，它仍能把数据送到链路上。
    #[inline]
    pub fn close_webrtc(&self) {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.close_detached(),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    /// 已建立的 WebRTC 传输是否经由 TURN 中继（用于 UI 的
    /// 直连/中继标志）。非 WebRTC 传输为 `None`；对于非 relay 策略的 pc，
    /// 在 ICE 选出候选对之前也是 `None`；Relay 策略的 pc 会直接返回 `Some(true)`——见
    /// `WebRTCStream::is_relayed`。
    #[inline]
    pub async fn webrtc_relayed(&self) -> Option<bool> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.is_relayed().await,
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// 已建立的 WebRTC 传输是否通过 IPv6 到达对端（用于在 UI 中
    /// 标注传输方式）。非 WebRTC 传输以及 ICE 选出候选对之前都是 `None`——
    /// 其他每种传输都已经在竞速时的标签里携带了该地址族。
    #[inline]
    pub async fn webrtc_remote_ipv6(&self) -> Option<bool> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.is_remote_ipv6().await,
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// WebRTC 流的 DTLS 证书指纹（`local`=true 表示本端自己的
    /// 证书，为 false 表示对端的），用于把通道绑定到已签名的对端身份。
    /// 非 WebRTC 传输返回 None，它们通过 secretbox 密钥交换完成鉴权。
    #[inline]
    #[cfg_attr(not(feature = "webrtc"), allow(unused_variables))]
    pub async fn dtls_fingerprint(&self, local: bool) -> Option<String> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => {
                if local {
                    s.local_dtls_fingerprint().await.ok()
                } else {
                    s.remote_dtls_fingerprint().await.ok()
                }
            }
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    #[inline]
    pub async fn next_timeout(
        &mut self,
        timeout: u64,
    ) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        match self {
            #[cfg(feature = "webrtc")]
            Stream::WebRTC(s) => s.next_timeout(timeout).await,
            Stream::WebSocket(s) => s.next_timeout(timeout).await,
            Stream::Tcp(s) => s.next_timeout(timeout).await,
        }
    }

    /// 发送消息
    #[inline]
    pub async fn send(&mut self, msg: &impl protobuf::Message) -> ResultType<()> {
        match self {
            #[cfg(feature = "webrtc")]
            Self::WebRTC(s) => s.send(msg).await,
            Self::WebSocket(ws) => ws.send(msg).await,
            Self::Tcp(tcp) => tcp.send(msg).await,
        }
    }

    /// 接收消息
    #[inline]
    pub async fn next(&mut self) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        match self {
            #[cfg(feature = "webrtc")]
            Self::WebRTC(s) => s.next().await,
            Self::WebSocket(ws) => ws.next().await,
            Self::Tcp(tcp) => tcp.next().await,
        }
    }

    #[inline]
    pub fn local_addr(&self) -> SocketAddr {
        match self {
            #[cfg(feature = "webrtc")]
            Self::WebRTC(s) => s.local_addr(),
            Self::WebSocket(ws) => ws.local_addr(),
            Self::Tcp(tcp) => tcp.local_addr(),
        }
    }

    #[inline]
    pub fn from(stream: TcpStream, stream_addr: SocketAddr) -> Self {
        Self::Tcp(tcp::FramedStream::from(stream, stream_addr))
    }
}

/// 拥有该流就等于拥有该传输，WebRTC 也不例外。
///
/// peer connection 的存活期长于它的句柄——session 缓存持有一个克隆，而
/// 驱逐它的处理器只在终止性 ICE 状态时才触发——因此必须显式关闭它。如此一来
/// 在每个退出路径上关闭，就成了每个 `return`、`break` 和 `?` 都得记住的义务，而
/// 长生命周期的那一侧却从不这样做：`server::connection` 结束它约 15 处退出靠的是 drop 掉该流。
/// 没有任何提示，因为漏掉的关闭会静默泄漏，而且只在 WebRTC 下发生。
///
/// `Stream` 不是 `Clone`，因此 drop 掉它确实就是该传输的终点，不存在
/// 第二个所有者来搞突然袭击。显式调用 `close_webrtc()` 依然有效——它们关闭得更早，
/// 比作用域结束更早——但现在它们只是一种优化，而不再是正确性所依赖的手段。
impl Drop for Stream {
    fn drop(&mut self) {
        self.close_webrtc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_set_negotiated_key_refuses_unknown_versions() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut stream = Stream::from(TcpStream::connect(addr).await.unwrap(), addr);
        let t = |picked| tcp::KxTranscript {
            initiator_pk: &[1u8; 32],
            responder_pk: &[2u8; 32],
            advertised: 1,
            picked,
        };
        let mut set = |picked| stream.set_negotiated_key(Key([0u8; 32]), true, &t(picked));
        assert!(set(2).is_err());
        assert!(set(1).is_ok());
    }
}
