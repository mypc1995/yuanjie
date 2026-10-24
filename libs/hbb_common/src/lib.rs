pub mod compress;
pub mod protos;
pub use bytes;
use config::Config;
pub use futures;
pub use protobuf;
pub use protos::rendezvous as rendezvous_proto;
use serde_derive::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, BufRead},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    path::Path,
    time::{self, SystemTime, UNIX_EPOCH},
};
pub use tokio;
pub use tokio_util;
pub mod proxy;
pub mod socket_client;
pub mod tcp;
pub mod udp;
pub use env_logger;
pub use log;
pub mod bytes_codec;
pub use anyhow::{self, bail};
pub use futures_util;
pub mod config;
pub mod mem;
pub use lazy_static;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use mac_address;
pub use rand;
pub use regex;
pub use sodiumoxide;
pub use tokio_socks;
pub use tokio_socks::IntoTargetAddr;
pub use tokio_socks::TargetAddr;
pub mod password_security;
pub use chrono;
pub use directories_next;
pub use libc;
#[cfg(target_os = "linux")]
pub mod sh;
pub use base64;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use dlopen;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use machine_uid;
pub use serde_derive;
pub use serde_json;
pub use sha2;
pub use sysinfo;
pub use thiserror;
pub use toml;
pub use uuid;
pub mod fingerprint;
pub use flexi_logger;
pub mod log_throttle;
pub mod rx_probe;
pub mod stream;
pub mod websocket;
#[cfg(feature = "webrtc")]
pub mod webrtc;
#[cfg(any(target_os = "android", target_os = "ios"))]
pub use rustls_platform_verifier;
pub use stream::Stream;
pub use whoami;
pub mod tls;
pub mod verifier;
pub use async_recursion;
#[cfg(target_os = "linux")]
pub use users;
pub use libloading;
#[cfg(target_os = "linux")]
pub use x11;

pub type SessionID = uuid::Uuid;

#[inline]
pub async fn sleep(sec: f32) {
    tokio::time::sleep(time::Duration::from_secs_f32(sec)).await;
}

#[macro_export]
macro_rules! allow_err {
    ($e:expr) => {
        if let Err(err) = $e {
            log::debug!(
                "{:?}, {}:{}:{}:{}",
                err,
                module_path!(),
                file!(),
                line!(),
                column!()
            );
        } else {
        }
    };

    ($e:expr, $($arg:tt)*) => {
        if let Err(err) = $e {
            log::debug!(
                "{:?}, {}, {}:{}:{}:{}",
                err,
                format_args!($($arg)*),
                module_path!(),
                file!(),
                line!(),
                column!()
            );
        } else {
        }
    };
}

#[inline]
pub fn timeout<T: std::future::Future>(ms: u64, future: T) -> tokio::time::Timeout<T> {
    tokio::time::timeout(std::time::Duration::from_millis(ms), future)
}

pub type ResultType<F, E = anyhow::Error> = anyhow::Result<F, E>;

/// 某些路由器和防火墙会扫描数据包，如果它们
/// 发现属于自己地址池的 IP 地址并用其做 NAT 映射/转换，因此这里我们对 ip 地址做了变形处理

pub struct AddrMangle();

#[inline]
pub fn try_into_v4(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) if !addr.ip().is_loopback() => {
            if let Some(v4) = v6.ip().to_ipv4() {
                SocketAddr::new(IpAddr::V4(v4), addr.port())
            } else {
                addr
            }
        }
        _ => addr,
    }
}

impl AddrMangle {
    pub fn encode(addr: SocketAddr) -> Vec<u8> {
        // 对 [:1]:<port> 不适用
        let addr = try_into_v4(addr);
        match addr {
            SocketAddr::V4(addr_v4) => {
                let tm = (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(std::time::Duration::ZERO)
                    .as_micros() as u32) as u128;
                let ip = u32::from_le_bytes(addr_v4.ip().octets()) as u128;
                let port = addr.port() as u128;
                let v = ((ip + tm) << 49) | (tm << 17) | (port + (tm & 0xFFFF));
                let bytes = v.to_le_bytes();
                let mut n_padding = 0;
                for i in bytes.iter().rev() {
                    if i == &0u8 {
                        n_padding += 1;
                    } else {
                        break;
                    }
                }
                bytes[..(16 - n_padding)].to_vec()
            }
            SocketAddr::V6(addr_v6) => {
                let mut x = addr_v6.ip().octets().to_vec();
                let port: [u8; 2] = addr_v6.port().to_le_bytes();
                x.push(port[0]);
                x.push(port[1]);
                x
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> SocketAddr {
        use std::convert::TryInto;

        if bytes.len() > 16 {
            if bytes.len() != 18 {
                return Config::get_any_listen_addr(false);
            }
            let tmp: [u8; 2] = bytes[16..].try_into().unwrap_or_default();
            let port = u16::from_le_bytes(tmp);
            let tmp: [u8; 16] = bytes[..16].try_into().unwrap_or_default();
            let ip = std::net::Ipv6Addr::from(tmp);
            return SocketAddr::new(IpAddr::V6(ip), port);
        }
        let mut padded = [0u8; 16];
        padded[..bytes.len()].copy_from_slice(bytes);
        let number = u128::from_le_bytes(padded);
        let tm = (number >> 17) & (u32::max_value() as u128);
        let ip = (((number >> 49) - tm) as u32).to_le_bytes();
        let port = (number & 0xFFFFFF) - (tm & 0xFFFF);
        SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]),
            port as u16,
        ))
    }
}

pub fn get_version_from_url(url: &str) -> String {
    let n = url.chars().count();
    let a = url.chars().rev().position(|x| x == '-');
    if let Some(a) = a {
        let b = url.chars().rev().position(|x| x == '.');
        if let Some(b) = b {
            if a > b {
                if url
                    .chars()
                    .skip(n - b)
                    .collect::<String>()
                    .parse::<i32>()
                    .is_ok()
                {
                    return url.chars().skip(n - a).collect();
                } else {
                    return url.chars().skip(n - a).take(a - b - 1).collect();
                }
            } else {
                return url.chars().skip(n - a).collect();
            }
        }
    }
    "".to_owned()
}

pub fn gen_version() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    use std::io::prelude::*;
    let mut file = File::create("./src/version.rs").unwrap();
    for line in read_lines("Cargo.toml").unwrap().flatten() {
        let ab: Vec<&str> = line.split('=').map(|x| x.trim()).collect();
        if ab.len() == 2 && ab[0] == "version" {
            file.write_all(format!("pub const VERSION: &str = {};\n", ab[1]).as_bytes())
                .ok();
            break;
        }
    }
    // 生成构建日期
    let build_date = format!("{}", chrono::Local::now().format("%Y-%m-%d %H:%M"));
    file.write_all(
        format!("#[allow(dead_code)]\npub const BUILD_DATE: &str = \"{build_date}\";\n").as_bytes(),
    )
    .ok();
    file.sync_all().ok();
}

fn read_lines<P>(filename: P) -> io::Result<io::Lines<io::BufReader<File>>>
where
    P: AsRef<Path>,
{
    let file = File::open(filename)?;
    Ok(io::BufReader::new(file).lines())
}

pub fn is_valid_custom_id(id: &str) -> bool {
    regex::Regex::new(r"^[a-zA-Z][\w-]{5,15}$")
        .unwrap()
        .is_match(id)
}

// 支持 1.1.10-1，- 之后的数字是补丁版本号。
pub fn get_version_number(v: &str) -> i64 {
    let mut versions = v.split('-');

    let mut n = 0;

    // 第一部分是版本号。
    // 1.1.10 -> 1001100, 1.2.3 -> 1001030，把最后一个数字乘以 10
    // 以给补丁版本留出空间。
    if let Some(v) = versions.next() {
        let mut last = 0;
        for x in v.split('.') {
            last = x.parse::<i64>().unwrap_or(0);
            n = n * 1000 + last;
        }
        n -= last;
        n += last * 10;
    }

    if let Some(v) = versions.next() {
        n += v.parse::<i64>().unwrap_or(0);
    }

    // 忽略其余部分

    n
}

pub fn get_modified_time(path: &std::path::Path) -> SystemTime {
    std::fs::metadata(path)
        .map(|m| m.modified().unwrap_or(UNIX_EPOCH))
        .unwrap_or(UNIX_EPOCH)
}

pub fn get_created_time(path: &std::path::Path) -> SystemTime {
    std::fs::metadata(path)
        .map(|m| m.created().unwrap_or(UNIX_EPOCH))
        .unwrap_or(UNIX_EPOCH)
}

pub fn get_exe_time() -> SystemTime {
    std::env::current_exe().map_or(UNIX_EPOCH, |path| {
        let m = get_modified_time(&path);
        let c = get_created_time(&path);
        if m > c {
            m
        } else {
            c
        }
    })
}

/// machine_uid::get() 可能失败的已知情况：
/// - Windows 关机："The media is write protected. (os error 19)"
/// - macOS（难以复现，在登录界面可复现）："No matching IOPlatformUUID in `ioreg -rd1 -c IOPlatformExpertDevice` command"
pub fn get_uuid() -> Vec<u8> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static CACHED_MACHINE_UID: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        // 节流仅适用于下面回退的 machine_uid::get() 日志，而不适用于 Once::call_once 的重试日志。
        static LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

        // 这里只有 macOS 需要重试逻辑，因为：
        // - macOS：测试中以 50ms 间隔读取时只出现过一次失败，所以重试是有帮助的
        // - Windows：关机期间的失败是持续性的，重试没有意义
        #[cfg(target_os = "macos")]
        {
            static INIT: std::sync::Once = std::sync::Once::new();
            INIT.call_once(|| {
                // 与上游的处理方式保持同步：
                // https://github.com/rustdesk/rustdesk/blob/85db6779828349b23ca3eba91cc7cd36c5337797/src/common.rs#L822
                let username = whoami::username().trim_end_matches('\0').to_owned();
                let max_retries = if username == "root" { 16 } else { 8 };
                for i in 0..max_retries {
                    match machine_uid::get() {
                        Ok(id) => {
                            let _ = CACHED_MACHINE_UID.set(id.into());
                            return;
                        }
                        Err(e) => {
                            log::error!("Failed to get machine uid in macOS retry #{i}: {e}");
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            });
        }

        if let Some(uid) = CACHED_MACHINE_UID.get() {
            return uid.clone();
        }

        match machine_uid::get() {
            Ok(id) => {
                let uid: Vec<u8> = id.into();
                let _ = CACHED_MACHINE_UID.set(uid.clone());
                return uid;
            }
            Err(e) => {
                if LOG_COUNT
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                        (count < 30).then_some(count + 1)
                    })
                    .is_ok()
                {
                    log::error!("Failed to get machine uid: {e}");
                }
            }
        }
    }
    Config::get_key_pair().1
}

#[inline]
pub fn get_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as _
}

#[inline]
pub fn is_ipv4_str(id: &str) -> bool {
    if let Ok(reg) = regex::Regex::new(
        r"^(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)(:\d+)?$",
    ) {
        reg.is_match(id)
    } else {
        false
    }
}

#[inline]
pub fn is_ipv6_str(id: &str) -> bool {
    if let Ok(reg) = regex::Regex::new(
        r"^((([a-fA-F0-9]{1,4}:{1,2})+[a-fA-F0-9]{1,4})|(\[([a-fA-F0-9]{1,4}:{1,2})+[a-fA-F0-9]{1,4}\]:\d+))$",
    ) {
        reg.is_match(id)
    } else {
        false
    }
}

#[inline]
pub fn is_ip_str(id: &str) -> bool {
    is_ipv4_str(id) || is_ipv6_str(id)
}

#[inline]
pub fn is_domain_port_str(id: &str) -> bool {
    // 针对 RFC1123 主机名修改过的正则表达式。主机名的原始版本见 https://stackoverflow.com/a/106223。
    // 根据 [TLD List](https://data.iana.org/TLD/tlds-alpha-by-domain.txt) 版本 2023011700，
    // TLD 中不含数字，长度为 2~63。
    if let Ok(reg) = regex::Regex::new(
        r"(?i)^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z-]{0,61}[a-z]:\d{1,5}$",
    ) {
        reg.is_match(id)
    } else {
        false
    }
}

pub fn init_log(_is_async: bool, _name: &str) -> Option<flexi_logger::LoggerHandle> {
    static INIT: std::sync::Once = std::sync::Once::new();
    #[allow(unused_mut)]
    let mut logger_holder: Option<flexi_logger::LoggerHandle> = None;
    INIT.call_once(|| {
        // webrtc-rs 在 warn 级别上报竞态设计的正常结果：正在取消 transport
        // 失败的一方会逐行关闭每个已收集的 candidate，而 trickle 意味着
        // agent 总是先检查再持有候选对。两者在连接
        // 成功旁边都像是故障。agent_gather 保持 warn 级别——它连不上的 STUN 服务器
        // 正是解释那个从未连上的会话的上游信号。
        #[cfg(debug_assertions)]
        {
            use env_logger::*;
            init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "info,reqwest=warn,hyper_util=warn,rustls=warn,webrtc-sctp=warn,webrtc=warn,webrtc_ice::agent::agent_internal=error,webrtc::peer_connection=error,smithay_client_toolkit=warn"));
        }
        #[cfg(not(debug_assertions))]
        {
            // https://docs.rs/flexi_logger/latest/flexi_logger/error_info/index.html#write
            // 虽然异步日志器更高效，但它也带来了更多问题，暂时将其禁用
            let mut path = config::Config::log_path();
            #[cfg(target_os = "android")]
            if !config::Config::get_home().exists() {
                return;
            }
            if !_name.is_empty() {
                path.push(_name);
            }
            use flexi_logger::*;
            if let Ok(x) = Logger::try_with_env_or_str("debug,reqwest=warn,hyper_util=warn,rustls=warn,webrtc-sctp=warn,webrtc=warn,webrtc_ice::agent::agent_internal=error,webrtc::peer_connection=error,smithay_client_toolkit=warn") {
                logger_holder = x
                    .log_to_file(FileSpec::default().directory(path))
                    .write_mode(if _is_async {
                        WriteMode::Async
                    } else {
                        WriteMode::Direct
                    })
                    .format(opt_format)
                    .rotate(
                        // 同时限制大小和时长：仅按天轮转会让某一天的文件不断增长
                        // 且不受限制，因此任何能制造热点日志的对端——比如发送
                        // 畸形数据包、在重试循环中不断报错的 socket——都能决定
                        // 这会占用多少磁盘。在这里设限可以一次性覆盖所有调用点。
                        Criterion::AgeOrSize(Age::Day, 16 * 1024 * 1024),
                        Naming::Timestamps,
                        // 对于任何保持在
                        // 大小标准以内的机器——也就是本软件发布到的每个平台上的每个普通安装——90 个文件约等于 90 天。
                        // 把它调高以应对洪泛情况带来的收益很小
                        // （文件数量永远跑不赢洪泛；只有日志点上的速率限制
                        // 能起作用），代价是让所有人的常驻保留时间和磁盘占用成倍增加
                        // 。
                        Cleanup::KeepLogFiles(90),
                    )
                    .start()
                    .ok();
            }
        }
    });
    logger_holder
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct VersionCheckRequest {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub os_version: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub device_id: Vec<u8>,
    #[serde(default)]
    pub typ: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct VersionCheckResponse {
    #[serde(default)]
    pub url: String,
}

pub const VER_TYPE_RUSTDESK_CLIENT: &str = "rustdesk-client";
pub const VER_TYPE_RUSTDESK_SERVER: &str = "rustdesk-server";

pub fn version_check_request(typ: String) -> (VersionCheckRequest, String) {
    const URL: &str = "https://api.rustdesk.com/version/latest";

    use sysinfo::System;
    let system = System::new();
    let os = system.distribution_id();
    let os_version = system.os_version().unwrap_or_default();
    let arch = std::env::consts::ARCH.to_string();
    #[allow(deprecated)]
    let device_id = fingerprint::get_fingerprint(None, None);
    (
        VersionCheckRequest {
            os,
            os_version,
            arch,
            device_id,
            typ,
        },
        URL.to_string(),
    )
}

pub fn time_based_rand() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let mut x = nanos as u64;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;

    (x % 32768) as u32
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_mangle() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 16, 32), 21116));
        assert_eq!(addr, AddrMangle::decode(&AddrMangle::encode(addr)));

        let addr = "[2001:db8::1]:8080".parse::<SocketAddr>().unwrap();
        assert_eq!(addr, AddrMangle::decode(&AddrMangle::encode(addr)));

        let addr = "[2001:db8:ff::1111]:80".parse::<SocketAddr>().unwrap();
        assert_eq!(addr, AddrMangle::decode(&AddrMangle::encode(addr)));
    }

    #[test]
    fn test_allow_err() {
        allow_err!(Err("test err") as Result<(), &str>);
        allow_err!(
            Err("test err with msg") as Result<(), &str>,
            "prompt {}",
            "failed"
        );
    }

    #[test]
    fn test_ipv6() {
        assert!(is_ipv6_str("1:2:3"));
        assert!(is_ipv6_str("[ab:2:3]:12"));
        assert!(is_ipv6_str("[ABEF:2a:3]:12"));
        assert!(!is_ipv6_str("[ABEG:2a:3]:12"));
        assert!(!is_ipv6_str("1[ab:2:3]:12"));
        assert!(!is_ipv6_str("1.1.1.1"));
        assert!(is_ip_str("1.1.1.1"));
        assert!(!is_ipv6_str("1:2:"));
        assert!(is_ipv6_str("1:2::0"));
        assert!(is_ipv6_str("[1:2::0]:1"));
        assert!(!is_ipv6_str("[1:2::0]:"));
        assert!(!is_ipv6_str("1:2::0]:1"));
    }

    #[test]
    fn test_ipv4() {
        assert!(is_ipv4_str("1.2.3.4"));
        assert!(is_ipv4_str("1.2.3.4:90"));
        assert!(is_ipv4_str("192.168.0.1"));
        assert!(is_ipv4_str("0.0.0.0"));
        assert!(is_ipv4_str("255.255.255.255"));
        assert!(!is_ipv4_str("256.0.0.0"));
        assert!(!is_ipv4_str("256.256.256.256"));
        assert!(!is_ipv4_str("1:2:"));
        assert!(!is_ipv4_str("192.168.0.256"));
        assert!(!is_ipv4_str("192.168.0.1/24"));
        assert!(!is_ipv4_str("192.168.0."));
        assert!(!is_ipv4_str("192.168..1"));
    }

    #[test]
    fn test_hostname_port() {
        assert!(!is_domain_port_str("a:12"));
        assert!(!is_domain_port_str("a.b.c:12"));
        assert!(is_domain_port_str("test.com:12"));
        assert!(is_domain_port_str("test-UPPER.com:12"));
        assert!(is_domain_port_str("some-other.domain.com:12"));
        assert!(!is_domain_port_str("under_score:12"));
        assert!(!is_domain_port_str("a@bc:12"));
        assert!(!is_domain_port_str("1.1.1.1:12"));
        assert!(!is_domain_port_str("1.2.3:12"));
        assert!(!is_domain_port_str("1.2.3.45:12"));
        assert!(!is_domain_port_str("a.b.c:123456"));
        assert!(!is_domain_port_str("---:12"));
        assert!(!is_domain_port_str(".:12"));
        // 待办：我们是否也应该检查这些边界情况？
        // 端口超出范围
        assert!(is_domain_port_str("test.com:0"));
        assert!(is_domain_port_str("test.com:98989"));
    }

    #[test]
    fn test_mangle2() {
        let addr = "[::ffff:127.0.0.1]:8080".parse().unwrap();
        let addr_v4 = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(AddrMangle::decode(&AddrMangle::encode(addr)), addr_v4);
        assert_eq!(
            AddrMangle::decode(&AddrMangle::encode("[::127.0.0.1]:8080".parse().unwrap())),
            addr_v4
        );
        assert_eq!(AddrMangle::decode(&AddrMangle::encode(addr_v4)), addr_v4);
        let addr_v6 = "[ef::fe]:8080".parse().unwrap();
        assert_eq!(AddrMangle::decode(&AddrMangle::encode(addr_v6)), addr_v6);
        let addr_v6 = "[::1]:8080".parse().unwrap();
        assert_eq!(AddrMangle::decode(&AddrMangle::encode(addr_v6)), addr_v6);
    }

    #[test]
    fn test_get_version_number() {
        assert_eq!(get_version_number("1.1.10"), 1001100);
        assert_eq!(get_version_number("1.1.10-1"), 1001101);
        assert_eq!(get_version_number("1.1.11-1"), 1001111);
        assert_eq!(get_version_number("1.2.3"), 1002030);
    }
}
