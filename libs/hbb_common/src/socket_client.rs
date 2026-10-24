#[cfg(feature = "webrtc")]
use crate::webrtc::{self, is_webrtc_endpoint};
use crate::{
    config::{Config, NetworkType},
    tcp::FramedStream,
    udp::FramedSocket,
    websocket::{self, check_ws, is_ws_endpoint},
    ResultType, Stream,
};
use anyhow::Context;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio_socks::{IntoTargetAddr, TargetAddr};

#[inline]
pub fn check_port<T: std::string::ToString>(host: T, port: i32) -> String {
    let host = host.to_string();
    if crate::is_ipv6_str(&host) {
        if host.starts_with('[') {
            return host;
        }
        return format!("[{host}]:{port}");
    }
    if !host.contains(':') {
        return format!("{host}:{port}");
    }
    host
}

#[inline]
pub fn increase_port<T: std::string::ToString>(host: T, offset: i32) -> String {
    let host = host.to_string();
    if crate::is_ipv6_str(&host) {
        if host.starts_with('[') {
            let tmp: Vec<&str> = host.split("]:").collect();
            if tmp.len() == 2 {
                let port: i32 = tmp[1].parse().unwrap_or(0);
                if port > 0 {
                    return format!("{}]:{}", tmp[0], port + offset);
                }
            }
        }
    } else if host.contains(':') {
        let tmp: Vec<&str> = host.split(':').collect();
        if tmp.len() == 2 {
            let port: i32 = tmp[1].parse().unwrap_or(0);
            if port > 0 {
                return format!("{}:{}", tmp[0], port + offset);
            }
        }
    }
    host
}

pub fn split_host_port<T: std::string::ToString>(host: T) -> Option<(String, i32)> {
    let host = host.to_string();
    if crate::is_ipv6_str(&host) {
        if host.starts_with('[') {
            let tmp: Vec<&str> = host.split("]:").collect();
            if tmp.len() == 2 {
                let port: i32 = tmp[1].parse().unwrap_or(0);
                if port > 0 {
                    return Some((format!("{}]", tmp[0]), port));
                }
            }
        }
    } else if host.contains(':') {
        let tmp: Vec<&str> = host.split(':').collect();
        if tmp.len() == 2 {
            let port: i32 = tmp[1].parse().unwrap_or(0);
            if port > 0 {
                return Some((tmp[0].to_string(), port));
            }
        }
    }
    None
}

pub fn test_if_valid_server(host: &str, test_with_proxy: bool) -> String {
    let host = check_port(host, 0);
    use std::net::ToSocketAddrs;

    if test_with_proxy && NetworkType::ProxySocks == Config::get_network_type() {
        test_if_valid_server_for_proxy_(&host)
    } else {
        match host.to_socket_addrs() {
            Err(err) => err.to_string(),
            Ok(_) => "".to_owned(),
        }
    }
}

#[inline]
pub fn test_if_valid_server_for_proxy_(host: &str) -> String {
    // `&host.into_target_addr()` 在 `tokio-socs` 中有定义，是测试中常见的写法，
    // 它对 `socks` 和 `http` 代理都适用。
    match &host.into_target_addr() {
        Err(err) => err.to_string(),
        Ok(_) => "".to_owned(),
    }
}

pub trait IsResolvedSocketAddr {
    fn resolve(&self) -> Option<&SocketAddr>;
}

impl IsResolvedSocketAddr for SocketAddr {
    fn resolve(&self) -> Option<&SocketAddr> {
        Some(self)
    }
}

impl IsResolvedSocketAddr for String {
    fn resolve(&self) -> Option<&SocketAddr> {
        None
    }
}

impl IsResolvedSocketAddr for &str {
    fn resolve(&self) -> Option<&SocketAddr> {
        None
    }
}

// 此函数检查目标是否为 websocket 端点，并据此连接。
#[inline]
pub async fn connect_tcp<
    't,
    T: IntoTargetAddr<'t> + ToSocketAddrs + IsResolvedSocketAddr + std::fmt::Display,
>(
    target: T,
    ms_timeout: u64,
) -> ResultType<crate::Stream> {
    #[cfg(feature = "webrtc")]
    if is_webrtc_endpoint(&target.to_string()) {
        return Ok(Stream::WebRTC(
            webrtc::WebRTCStream::new(&target.to_string(), false, ms_timeout).await?,
        ));
    }
    let target_str = check_ws(&target.to_string());
    if is_ws_endpoint(&target_str) {
        return Ok(Stream::WebSocket(
            websocket::WsFramedStream::new(target_str, None, ms_timeout).await?,
        ));
    }
    connect_tcp_local(target, None, ms_timeout).await
}

// 此函数直接连接目标，不检查是否为 websocket 端点。
pub async fn connect_tcp_local<
    't,
    T: IntoTargetAddr<'t> + ToSocketAddrs + IsResolvedSocketAddr + std::fmt::Display,
>(
    target: T,
    local: Option<SocketAddr>,
    ms_timeout: u64,
) -> ResultType<Stream> {
    if let Some(conf) = Config::get_socks() {
        return Ok(Stream::Tcp(
            FramedStream::connect(target, local, &conf, ms_timeout).await?,
        ));
    }

    if let Some(target_addr) = target.resolve() {
        if let Some(local_addr) = local {
            if local_addr.is_ipv6() && target_addr.is_ipv4() {
                let resolved_target = query_nip_io(target_addr).await?;
                return Ok(Stream::Tcp(
                    FramedStream::new(resolved_target, Some(local_addr), ms_timeout).await?,
                ));
            }
        }
    }

    Ok(Stream::Tcp(
        FramedStream::new(target, local, ms_timeout).await?,
    ))
}

#[inline]
pub fn is_ipv4(target: &TargetAddr<'_>) -> bool {
    match target {
        TargetAddr::Ip(addr) => addr.is_ipv4(),
        _ => true,
    }
}

#[inline]
pub async fn query_nip_io(addr: &SocketAddr) -> ResultType<SocketAddr> {
    tokio::net::lookup_host(format!("{}.nip.io:{}", addr.ip(), addr.port()))
        .await?
        .find(|x| x.is_ipv6())
        .context("Failed to get ipv6 from nip.io")
}

#[inline]
pub fn ipv4_to_ipv6(addr: String, ipv4: bool) -> String {
    if !ipv4 && crate::is_ipv4_str(&addr) {
        if let Some(ip) = addr.split(':').next() {
            return addr.replace(ip, &format!("{ip}.nip.io"));
        }
    }
    addr
}

/// 解析器为 `target` 选出的首选地址：按 getaddrinfo 的 RFC 6724 顺序排在最前，因此在纯 v6 主机上 NAT64 合成的
/// IPv6 会排在前面。是首选但未经证明可达：这正是 `test_target`
/// 回退时采用的地址，也是控制器的 NAT 测试为省去该验证而直接接受的值。
async fn resolve_target(target: &str) -> ResultType<SocketAddr> {
    tokio::net::lookup_host(target)
        .await?
        .next()
        .context(format!("Failed to look up host for {target}"))
}

/// TCP 连接到达的 `target` 地址，即按顺序尝试解析器的候选地址直到
/// 有一个响应为止：服务器拒绝的 AAAA 会被跳过而改用其 A 记录，这一点单靠顺序是做不到的。
/// 而 rendezvous 注册和打洞响应都依赖于它选出的地址。
async fn test_target(target: &str) -> ResultType<SocketAddr> {
    if let Ok(Ok(s)) = super::timeout(1000, tokio::net::TcpStream::connect(target)).await {
        if let Ok(addr) = s.peer_addr() {
            return Ok(addr);
        }
    }
    resolve_target(target).await
}

#[inline]
pub async fn new_direct_udp_for(target: &str) -> ResultType<(Arc<UdpSocket>, SocketAddr)> {
    let peer_addr = test_target(target).await?;
    let local_addr = Config::get_any_listen_addr(peer_addr.is_ipv4());
    let socket = UdpSocket::bind(local_addr).await?;
    Ok((Arc::new(socket), peer_addr))
}

/// 在解析器的首选地址上调用 `new_direct_udp_for`，不做握手验证，
/// 最多为这次查询付出第二次代价：对控制器的 NAT 测试来说，每次连接一次，而
/// 验证成本是一次往返，紧接着它还要向同一主机建立连接。
#[inline]
pub async fn new_direct_udp_for_unverified(
    target: &str,
) -> ResultType<(Arc<UdpSocket>, SocketAddr)> {
    let peer_addr = super::timeout(1000, resolve_target(target)).await??;
    let local_addr = Config::get_any_listen_addr(peer_addr.is_ipv4());
    let socket = UdpSocket::bind(local_addr).await?;
    Ok((Arc::new(socket), peer_addr))
}

#[inline]
pub async fn new_udp_for(
    target: &str,
    ms_timeout: u64,
) -> ResultType<(FramedSocket, TargetAddr<'static>)> {
    let (ipv4, target) = if NetworkType::Direct == Config::get_network_type() {
        let addr = test_target(target).await?;
        (addr.is_ipv4(), addr.into_target_addr()?)
    } else {
        (true, target.into_target_addr()?)
    };
    Ok((
        new_udp(Config::get_any_listen_addr(ipv4), ms_timeout).await?,
        target.to_owned(),
    ))
}

async fn new_udp<T: ToSocketAddrs>(local: T, ms_timeout: u64) -> ResultType<FramedSocket> {
    match Config::get_socks() {
        None => Ok(FramedSocket::new(local).await?),
        Some(conf) => {
            let socket = FramedSocket::new_proxy(
                conf.proxy.as_str(),
                local,
                conf.username.as_str(),
                conf.password.as_str(),
                ms_timeout,
            )
            .await?;
            Ok(socket)
        }
    }
}

pub async fn rebind_udp_for(
    target: &str,
) -> ResultType<Option<(FramedSocket, TargetAddr<'static>)>> {
    if Config::get_network_type() != NetworkType::Direct {
        return Ok(None);
    }
    let addr = test_target(target).await?;
    let v4 = addr.is_ipv4();
    Ok(Some((
        FramedSocket::new(Config::get_any_listen_addr(v4)).await?,
        addr.into_target_addr()?.to_owned(),
    )))
}

#[cfg(test)]
mod tests {
    use std::net::ToSocketAddrs;

    use super::*;

    #[test]
    fn test_nat64() {
        test_nat64_async();
    }

    #[tokio::main(flavor = "current_thread")]
    async fn test_nat64_async() {
        assert_eq!(ipv4_to_ipv6("1.1.1.1".to_owned(), true), "1.1.1.1");
        assert_eq!(ipv4_to_ipv6("1.1.1.1".to_owned(), false), "1.1.1.1.nip.io");
        assert_eq!(
            ipv4_to_ipv6("1.1.1.1:8080".to_owned(), false),
            "1.1.1.1.nip.io:8080"
        );
        assert_eq!(
            ipv4_to_ipv6("rustdesk.com".to_owned(), false),
            "rustdesk.com"
        );
        if ("rustdesk.com:80")
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap()
            .is_ipv6()
        {
            assert!(query_nip_io(&"1.1.1.1:80".parse().unwrap())
                .await
                .unwrap()
                .is_ipv6());
            return;
        }
        assert!(query_nip_io(&"1.1.1.1:80".parse().unwrap()).await.is_err());
    }

    #[test]
    fn test_test_if_valid_server() {
        assert!(!test_if_valid_server("a", false).is_empty());
        // 在 Linux 上，"1" 会被解析为 "0.0.0.1"
        assert!(test_if_valid_server("1.1.1.1", false).is_empty());
        assert!(test_if_valid_server("1.1.1.1:1", false).is_empty());
        assert!(test_if_valid_server("microsoft.com", false).is_empty());
        assert!(test_if_valid_server("microsoft.com:1", false).is_empty());

        // 使用代理
        // `:0` 表示调用了 `let host = check_port(host, 0);`。
        assert!(test_if_valid_server_for_proxy_("a:0").is_empty());
        assert!(test_if_valid_server_for_proxy_("1.1.1.1:0").is_empty());
        assert!(test_if_valid_server_for_proxy_("1.1.1.1:1").is_empty());
        assert!(test_if_valid_server_for_proxy_("abc.com:0").is_empty());
        assert!(test_if_valid_server_for_proxy_("abcd.com:1").is_empty());
    }

    #[test]
    fn test_check_port() {
        assert_eq!(check_port("[1:2]:12", 32), "[1:2]:12");
        assert_eq!(check_port("1:2", 32), "[1:2]:32");
        assert_eq!(check_port("z1:2", 32), "z1:2");
        assert_eq!(check_port("1.1.1.1", 32), "1.1.1.1:32");
        assert_eq!(check_port("1.1.1.1:32", 32), "1.1.1.1:32");
        assert_eq!(check_port("test.com:32", 0), "test.com:32");
        assert_eq!(increase_port("[1:2]:12", 1), "[1:2]:13");
        assert_eq!(increase_port("1.2.2.4:12", 1), "1.2.2.4:13");
        assert_eq!(increase_port("1.2.2.4", 1), "1.2.2.4");
        assert_eq!(increase_port("test.com", 1), "test.com");
        assert_eq!(increase_port("test.com:13", 4), "test.com:17");
        assert_eq!(increase_port("1:13", 4), "1:13");
        assert_eq!(increase_port("22:1:13", 4), "22:1:13");
        assert_eq!(increase_port("z1:2", 1), "z1:3");
    }
}
