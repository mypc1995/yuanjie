//! 隔离的 Wayland 显示探测：当
//! 环境中没有指定任何端点时（greeter 的 `--server` 和 root 服务都不会获得
//! 任何合成器相关变量）。它置于 `wayland_probe` 特性之后，因此基础的 Wayland 路径
//! 不受影响 —— 不构建 DRM 登录屏后端的消费方绝不会编译到它，
//! 并且 `get_wayland_displays` 保持原有行为，即返回连接错误。

use super::{collect_wayland_displays, get_values_of_seat0_with_gdm_wayland, WaylandDisplayInfo};
use hbb_common::{bail, ResultType};
use sctk::reexports::client::Connection;
use std::path::{Path, PathBuf};

const RUNTIME_DIR_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// 消费方二进制在执行任何其他启动工作之前，必须分发到 `wayland_display_probe_child_main` 的参数；
/// 参见该函数，了解为什么探测要独立成一个进程。
pub const WAYLAND_DISPLAY_PROBE_ARG: &str = "--wayland-display-probe";

/// 探测子进程输出的第一行 stdout。不分发该参数的二进制绝不会打印它。
const WAYLAND_PROBE_MAGIC: &str = "wayland-display-probe-v1";

/// 握手失败时会锁存：不分发探测参数的消费方会走它正常的
/// 启动流程，于是这条路径每个枚举周期都会重新进入。
static PROBE_UNSUPPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static RUNTIME_DIR_PROBE_BUSY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 在父进程的所有退出路径上清除进行中标志，包括各错误分支。
struct ProbeBusyGuard;

impl Drop for ProbeBusyGuard {
    fn drop(&mut self) {
        RUNTIME_DIR_PROBE_BUSY.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// 隔离探测进程的入口点。消费方二进制首先会分发到
/// `WAYLAND_DISPLAY_PROBE_ARG`，在配置、日志或任何其他启动工作之前。
///
/// 之所以独立成进程，是因为 release 配置使用 panic=abort 构建：sctk 在遇到畸形的
/// 协议字节时会 panic，在进程内这种 abort 会带崩整个服务端。在这里它只会带崩
/// 这个子进程，父进程会把它报告为探测失败。seat0 查询也在此子进程内运行，因此
/// 父进程的单一截止时间也限制了 loginctl 读取。
pub fn wayland_display_probe_child_main() -> ! {
    use std::io::Write;
    // 先做握手，这样父进程就能分辨出运行的是这个入口点，而不是一个
    // 落入正常启动流程的消费方二进制。
    println!("{WAYLAND_PROBE_MAGIC}");
    let _ = std::io::stdout().flush();
    let code = match seat0_runtime_dir()
        .and_then(|dir| {
            drop_to_dir_owner(&dir)?;
            probe_runtime_dir(&dir)
        })
        .and_then(|displays| serde_json::to_string(&displays).map_err(anyhow::Error::from))
    {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(err) => {
            eprintln!("{err:#}");
            1
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

static ENDPOINT_WAS_NAMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 本进程中的环境是否曾经指定过 wayland 端点。空值不算一个名称。
///
/// 在 `connect_to_env` 之前读取，它在成功和 fd 无效两条路径上都会把 `WAYLAND_SOCKET`
/// 从环境中移除；并且要锁存，这样一个已被消耗的变量就不会让原本
/// 指向某个合成器的进程变成可以随意去寻找另一个合成器的进程。
pub(super) fn env_names_wayland_endpoint() -> bool {
    use std::sync::atomic::Ordering;
    let named = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    if named {
        ENDPOINT_WAS_NAMED.store(true, Ordering::Release);
    }
    ENDPOINT_WAS_NAMED.load(Ordering::Acquire)
}

/// 探测会解析由合成器控制的数据；root 服务不能以 root 身份去做这件事，
/// 在接触 socket 之前，先成为该运行时目录的属主 —— 如果降权失败就拒绝探测，
/// 因为保持 root 是唯一不可接受的结果。
fn drop_to_dir_owner(dir: &Path) -> ResultType<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir)?;
    let (uid, gid) = (meta.uid(), meta.gid());
    if uid == 0 {
        // root 自己的会话：没有需要跨越的边界，也没有可以降权到的对象。
        return Ok(());
    }
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setgid(gid) != 0
            || libc::setuid(uid) != 0
            || libc::setuid(0) == 0
        {
            bail!("could not drop privileges for the socket probe");
        }
    }
    Ok(())
}

/// 活动 seat0 会话（包括 greeter）的 `/run/user/<uid>`。
///
/// 由 uid 推导，而不是从 `XDG_RUNTIME_DIR` 读取：root 服务不会获得这样的
/// 变量，而 `get_home_dir_trusted` 也出于同样原因不信任环境变量。
fn seat0_runtime_dir() -> ResultType<PathBuf> {
    let uid = get_values_of_seat0_with_gdm_wayland(&[1]).remove(0);
    if uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
        bail!("no active seat0 session to take a runtime directory from");
    }
    Ok(PathBuf::from(format!("/run/user/{uid}")))
}

/// `dir` 中存在的 wayland socket，按显示号从小到大排列。
///
/// 靠扫描而不是猜测：`wl_display_add_socket_auto` 会取第一个空闲名称，最多到
/// `wayland-32`，而 greeter 正是合成器多次重启后残留文件堆积的地方。只匹配那种
/// 名称模式，因为同一个目录里还放着 pipewire 和 dbus 的 socket。
fn wayland_sockets_in(dir: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("wayland-")
                    && !name.ends_with(".lock")
                    && entry.file_type().map(|t| t.is_socket()).unwrap_or(false)
            })
            .map(|entry| entry.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("wayland-"))
            .and_then(|number| number.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    paths
}

/// 通过 seat0 运行时目录中的某个 socket 进行枚举，用于应对没有任何
/// 端点被指定的情况：greeter 的 `--server` 和 root 服务都不会获得合成器变量，因此
/// 没有任何信息能告诉枚举器正在运行的合成器在哪里。已经
/// 被指定过但连接失败的端点，绝不能悄悄改连到另一个合成器。
///
/// 放在子进程中并且有界，因为调用方在整个调用期间持有进程级锁，而
/// `connect(2)` 在积压队列已满时会一直挂起，且 sctk 的往返轮询没有截止时间；还因为
/// sctk 在遇到畸形输出事件时会 panic，而 release 配置的 panic=abort 会把它变成
/// 整个服务端的 abort。子进程独自死亡，到达截止时间时会被杀掉，而不是
/// 遗留一个线程。seat0 查询在子进程内、在同一个截止时间下运行。
pub(super) fn wayland_displays_from_runtime_dir(
    named_endpoint: bool,
) -> ResultType<Vec<WaylandDisplayInfo>> {
    use std::sync::atomic::Ordering;
    if named_endpoint {
        bail!("an explicit wayland endpoint is set and did not connect");
    }
    if PROBE_UNSUPPORTED.load(Ordering::Acquire) {
        bail!("this binary does not dispatch {WAYLAND_DISPLAY_PROBE_ARG}");
    }
    if RUNTIME_DIR_PROBE_BUSY.swap(true, Ordering::AcqRel) {
        bail!("an earlier probe has not returned");
    }
    let _busy = ProbeBusyGuard;
    let exe = std::env::current_exe()?;
    // 独立的过程组，这样截止时间可以连同 loginctl 的后代进程一起杀掉子进程，
    // 并且不会有存活的后代进程让管道在下面的读取之后仍保持打开。
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(exe)
        .arg(WAYLAND_DISPLAY_PROBE_ARG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()?;
    let probe_pgid = child.id() as libc::pid_t;
    let kill_probe_group = || unsafe {
        let _ = libc::kill(-probe_pgid, libc::SIGKILL);
    };
    let deadline = std::time::Instant::now() + RUNTIME_DIR_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => {
                kill_probe_group();
                break status;
            }
            None if std::time::Instant::now() >= deadline => {
                kill_probe_group();
                // 还要针对直接 pid，而不只是它的进程组：如果子进程离开了该进程组，它自己的
                // kill 就打不到它，而下面的 wait 随后会阻塞在一个仍存活的子进程上。针对
                // pid 的 SIGKILL 是无法捕获的，因此 wait() 无论如何都是有界的。
                let _ = child.kill();
                let _ = child.wait();
                // 未接入探测的二进制会走它正常的启动流程；而长期运行的二进制（比如
                // 服务端自身）会落到这里而不是下面握手检查处 —— 所以要
                // 在这条路径上也锁存，否则每个枚举周期都会派生出一个完整的消费方
                // 进程。判断依据是子进程已经写出的内容：真正的探测会先打印
                // 那行魔数并刷新，因此在整个截止时间之后仍未出现就意味着
                // 这不是探测。只读取已缓冲的字节 —— 阻塞式读取可能会
                // 在继承了写端的孙进程上挂住。
                match first_buffered_line(child.stdout.take()) {
                    // 根本无法检查该管道：没有证据，也不锁存。
                    None => {
                        bail!("the wayland socket probe timed out and its output was uninspectable")
                    }
                    Some(head) if head.as_deref() == Some(WAYLAND_PROBE_MAGIC) => {
                        bail!("the wayland socket probe did not answer and was killed");
                    }
                    Some(_) => {
                        PROBE_UNSUPPORTED.store(true, Ordering::Release);
                        bail!("the wayland socket probe timed out without the handshake; probe disabled");
                    }
                }
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    };
    // 以非阻塞方式排空，而不是 read_to_string：子进程已经退出，其输出已经
    // 被缓冲，但逃离了进程组的后代进程仍可能持有打开的写端，
    // 而 seek EOF 的读取会永远挂在这里。
    let stdout = drain_nonblocking(child.stdout.take()).unwrap_or_default();
    let stderr = drain_nonblocking(child.stderr.take()).unwrap_or_default();
    let mut lines = stdout.lines();
    if lines.next() != Some(WAYLAND_PROBE_MAGIC) {
        // 不是探测：该二进制运行了它正常的启动流程。要锁存，否则这条路径每个枚举周期
        // 都会派生出一个完整的消费方进程。
        PROBE_UNSUPPORTED.store(true, Ordering::Release);
        bail!("this binary does not dispatch {WAYLAND_DISPLAY_PROBE_ARG}; probe disabled");
    }
    if !status.success() {
        let detail = stderr.trim();
        if detail.is_empty() {
            // panic=abort 或收到信号会让 stderr 保持为空；此时状态码是唯一的原因线索。
            bail!("wayland socket probe failed: {status}");
        }
        bail!("wayland socket probe failed ({status}): {detail}");
    }
    let displays: Vec<WaylandDisplayInfo> =
        match serde_json::from_str(lines.next().unwrap_or_default()) {
            Ok(displays) => displays,
            Err(err) => bail!("wayland socket probe answered a malformed list: {err}"),
        };
    // 子进程已经拒绝空列表；这里也要拒绝，这样被截断的管道就不会
    // 变成被永久缓存的空枚举结果。
    if displays.is_empty() {
        bail!("wayland socket probe returned no outputs");
    }
    log::debug!(
        "wayland: {} output(s) via the probe subprocess",
        displays.len()
    );
    Ok(displays)
}

/// 管道中已经缓冲的全部内容，严格以非阻塞方式读取并设上限：逃离
/// 探测进程组的后代进程可以持有打开的写端，因此阻塞式读取（即使
/// 在子进程退出之后）也可能让枚举永远挂起。`None` 表示该管道无法被
/// 检查（句柄缺失或 fcntl 失败），不能把它当作任何证据；
/// `Some` 则是已缓冲的字节内容，无论是否到达 EOF。
fn drain_nonblocking<R: std::io::Read + std::os::fd::AsRawFd>(pipe: Option<R>) -> Option<String> {
    let mut pipe = pipe?;
    let fd = pipe.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return None;
        }
    }
    // 设置上限，这样持续写入的后代进程就不会让这次读取无限空转。
    const CAP: usize = 64 * 1024;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break, // EOF: the write end is fully closed
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() >= CAP {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            // WouldBlock：已缓冲的内容会被排空（后代进程可能仍持有写端）。
            // 其他任何错误：带着已有内容停止。
            Err(_) => break,
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// 子进程缓冲的第一行，用于决定是否因超时而锁存。`Some(None)` 表示
/// 已检查但为空的缓冲区（确实没有握手）；外层的 `None` 表示无法检查。
fn first_buffered_line(pipe: Option<std::process::ChildStdout>) -> Option<Option<String>> {
    drain_nonblocking(pipe).map(|s| s.lines().next().map(str::to_owned))
}

fn probe_runtime_dir(dir: &Path) -> ResultType<Vec<WaylandDisplayInfo>> {
    use std::os::unix::net::UnixStream;
    let mut errs = Vec::new();
    for path in wayland_sockets_in(dir) {
        match UnixStream::connect(&path)
            .map_err(anyhow::Error::from)
            .and_then(|s| Connection::from_socket(s).map_err(anyhow::Error::from))
            .and_then(|conn| collect_wayland_displays(&conn))
        {
            // 调用方会把空列表当作该进程整个生命周期的事实依据，而
            // 仍在探测自己显示器的合成器正是这条路径会连接到的对象。
            Ok(displays) if displays.is_empty() => {
                errs.push(format!("{}: no outputs yet", path.display()))
            }
            Ok(displays) => {
                // 在环境中没有指定任何 socket 时，是哪个 socket 作出了响应。
                log::debug!(
                    "wayland: {} output(s) from {}, found by scanning",
                    displays.len(),
                    path.display()
                );
                return Ok(displays);
            }
            Err(err) => errs.push(format!("{}: {err}", path.display())),
        }
    }
    bail!(
        "no usable wayland socket in {} ({})",
        dir.display(),
        if errs.is_empty() {
            "none present".to_owned()
        } else {
            errs.join("; ")
        }
    )
}
