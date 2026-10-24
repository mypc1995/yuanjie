use super::*;
use hbb_common::{
    anyhow::{anyhow, Context, Result},
    compress,
};
use portable_pty::{Child, CommandBuilder, PtySize};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

// 来自 terminal_helper 模块的 Windows 专用导入
#[cfg(target_os = "windows")]
use super::terminal_helper::{
    configure_utf8_shell_command, create_named_pipe_server, encode_helper_message,
    encode_resize_message, is_helper_process_running, launch_terminal_helper_with_token,
    wait_for_pipe_connection, HelperProcessGuard, OwnedHandle, SendableHandle, WinCloseHandle,
    WinTerminateProcess, WinWaitForSingleObject, MSG_TYPE_DATA, PIPE_CONNECTION_TIMEOUT_MS,
    WIN_WAIT_OBJECT_0,
};

const MAX_OUTPUT_BUFFER_SIZE: usize = 1024 * 1024; // 1MB per terminal
const MAX_BUFFER_LINES: usize = 10000;
const MAX_SERVICES: usize = 100; // Maximum number of persistent terminal services
const SERVICE_IDLE_TIMEOUT: Duration = Duration::from_secs(3600); // 1 hour idle timeout
const CHANNEL_BUFFER_SIZE: usize = 500; // Channel buffer size. Max per-message size ~4KB (reader buffer), so worst case ~500*4KB ≈ 2MB/terminal. Increased from 100 to reduce data loss during disconnects.
const COMPRESS_THRESHOLD: usize = 512; // Compress terminal data larger than this
                                       // 重连时缓冲区回放的最大字节数默认值。
const DEFAULT_RECONNECT_BUFFER_BYTES: usize = 8 * 1024;
const MAX_SIGWINCH_PHASE_ATTEMPTS: u8 = 3; // Max attempts per SIGWINCH phase before giving up

/// 用于重连时触发 TUI 程序重绘的两阶段 SIGWINCH 机制。
///
/// 为什么要两个阶段？连续背靠背地执行一次「调整尺寸后恢复」实在太快：
/// 等 TUI 程序处理异步 SIGWINCH 信号并调用
/// `ioctl(TIOCGWINSZ)` 时，PTY 尺寸已经恢复为原始值。
/// ncurses 察觉不到尺寸变化，于是跳过了完整重绘。
///
/// 拆分成两次 `read_outputs()` 调用（相隔约 30ms）可确保程序
/// 在每次 SIGWINCH 时都能察觉到真实的尺寸变化，从而强制完整重绘。
#[derive(Debug, Clone)]
enum SigwinchPhase {
    /// 无需 SIGWINCH。
    Idle,
    /// 阶段 1：将 PTY 调整为临时尺寸（rows±1）。程序处理 SIGWINCH
    /// 并按临时尺寸重绘。
    TempResize { retries: u8 },
    /// 阶段 2：将 PTY 恢复到正确尺寸。程序处理 SIGWINCH，
    /// 察觉到尺寸变化，并按正确尺寸执行完整重绘。
    Restore { retries: u8 },
}

/// 在两阶段 SIGWINCH 序列中要执行哪一种尺寸调整。
enum SigwinchAction {
    /// 阶段 1：调整为临时尺寸（rows±1），以可见的尺寸变化触发 SIGWINCH。
    TempResize,
    /// 阶段 2：恢复到正确尺寸，触发 SIGWINCH 并强制完整重绘。
    Restore,
}

/// 用于终端流传输的会话状态机。
#[derive(Debug)]
enum SessionState {
    /// 会话已关闭，不再向客户端流式传输数据。
    Closed,
    /// 会话处于活动状态，正在向客户端流式传输数据。
    /// pending_buffer：先于实时数据发送的历史缓冲区（在重连时设置）。
    /// sigwinch：用于 TUI 程序重绘的两阶段 SIGWINCH 触发状态。
    Active {
        pending_buffer: Option<Vec<u8>>,
        sigwinch: SigwinchPhase,
    },
}

lazy_static::lazy_static! {
    // 以 service_id 为索引的持久化终端服务全局注册表
    static ref TERMINAL_SERVICES: Arc<Mutex<HashMap<String, Arc<Mutex<PersistentTerminalService>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // 清理任务句柄
    static ref CLEANUP_TASK: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // 待检查僵尸进程的终端子进程列表
    static ref TERMINAL_TASKS: Arc<Mutex<Vec<Box<dyn Child + Send + Sync>>>> = Arc::new(Mutex::new(Vec::new()));
}

/// 发送给客户端的服务元数据
#[derive(Clone, Debug)]
pub struct ServiceMetadata {
    pub service_id: String,
    pub created_at: Instant,
    pub terminal_count: usize,
    pub is_persistent: bool,
}

/// 生成新的持久化服务 ID
pub fn generate_service_id() -> String {
    format!("ts_{}", uuid::Uuid::new_v4())
}

fn get_default_shell() -> String {
    #[cfg(target_os = "windows")]
    {
        // 使用 terminal_helper 中的共享实现
        super::terminal_helper::get_default_shell()
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 先尝试 SHELL 环境变量
        if let Ok(shell) = std::env::var("SHELL") {
            if !shell.is_empty() {
                return shell;
            }
        }

        // 按优先级顺序检查常见的 shell
        let shells = ["/bin/bash", "/bin/zsh", "/bin/sh"];
        for shell in &shells {
            if std::path::Path::new(shell).exists() {
                return shell.to_string();
            }
        }

        // 最后回退到 /bin/sh，它在所有 POSIX 系统上应该都存在
        "/bin/sh".to_string()
    }
}

#[cfg(target_os = "macos")]
fn locale_value_is_utf8(value: &str) -> bool {
    let value = value.to_ascii_uppercase();
    value.contains("UTF-8") || value.contains("UTF8")
}

#[cfg(target_os = "macos")]
fn should_force_process_utf8_ctype() -> bool {
    if let Ok(value) = std::env::var("LC_ALL") {
        return !locale_value_is_utf8(&value);
    }
    if let Ok(value) = std::env::var("LC_CTYPE") {
        return !locale_value_is_utf8(&value);
    }
    if let Ok(value) = std::env::var("LANG") {
        return !locale_value_is_utf8(&value);
    }
    true
}

pub fn is_service_specified_user(service_id: &str) -> Option<bool> {
    get_service(service_id).map(|s| s.lock().unwrap().is_specified_user)
}

/// 获取或创建一个持久化终端服务
fn get_or_create_service(
    service_id: String,
    is_persistent: bool,
    is_specified_user: bool,
) -> Result<Arc<Mutex<PersistentTerminalService>>> {
    let mut services = TERMINAL_SERVICES.lock().unwrap();

    // 检查服务数量上限
    if !services.contains_key(&service_id) && services.len() >= MAX_SERVICES {
        return Err(anyhow!(
            "Maximum number of terminal services ({}) reached",
            MAX_SERVICES
        ));
    }

    let service = services
        .entry(service_id.clone())
        .or_insert_with(|| {
            log::info!(
                "Creating new terminal service: {} (persistent: {})",
                service_id,
                is_persistent
            );
            Arc::new(Mutex::new(PersistentTerminalService::new(
                service_id.clone(),
                is_persistent,
                is_specified_user,
            )))
        })
        .clone();

    // 确保清理任务正在运行
    ensure_cleanup_task();

    service.lock().unwrap().reset_status(is_persistent);

    Ok(service)
}

/// 从全局注册表中移除服务
fn remove_service(service_id: &str) {
    let mut services = TERMINAL_SERVICES.lock().unwrap();
    if let Some(service) = services.remove(service_id) {
        log::info!("Removed service: {}", service_id);
        // 关闭该服务中的所有终端
        let sessions = service.lock().unwrap().sessions.clone();
        for (_, session) in sessions.iter() {
            let mut session = session.lock().unwrap();
            session.stop();
        }
    }
}

/// 列出所有活动的终端服务
pub fn list_services() -> Vec<ServiceMetadata> {
    let services = TERMINAL_SERVICES.lock().unwrap();
    services
        .iter()
        .filter_map(|(id, service)| {
            service.lock().ok().map(|svc| ServiceMetadata {
                service_id: id.clone(),
                created_at: svc.created_at,
                terminal_count: svc.sessions.len(),
                is_persistent: svc.is_persistent,
            })
        })
        .collect()
}

/// 按 ID 获取服务
pub fn get_service(service_id: &str) -> Option<Arc<Mutex<PersistentTerminalService>>> {
    let services = TERMINAL_SERVICES.lock().unwrap();
    services.get(service_id).cloned()
}

/// 清理非活动的服务
pub fn cleanup_inactive_services() {
    let services = TERMINAL_SERVICES.lock().unwrap();
    let now = Instant::now();
    let mut to_remove = Vec::new();

    for (service_id, service) in services.iter() {
        if let Ok(svc) = service.lock() {
            // 空闲超时后移除非持久化服务
            if !svc.is_persistent && now.duration_since(svc.last_activity) > SERVICE_IDLE_TIMEOUT {
                to_remove.push(service_id.clone());
                log::info!("Cleaning up idle non-persistent service: {}", service_id);
            }
            // 在更长的超时后移除没有活动终端的持久化服务
            else if svc.is_persistent
                && svc.sessions.is_empty()
                && now.duration_since(svc.last_activity) > SERVICE_IDLE_TIMEOUT * 2
            {
                to_remove.push(service_id.clone());
                log::info!("Cleaning up empty persistent service: {}", service_id);
            }
        }
    }

    // 在迭代之外移除，以避免死锁
    drop(services);
    for id in to_remove {
        remove_service(&id);
    }
}

/// 将子进程加入僵尸进程回收器
fn add_to_reaper(child: Box<dyn Child + Send + Sync>) {
    if let Ok(mut tasks) = TERMINAL_TASKS.lock() {
        tasks.push(child);
    }
}

/// 检查并回收僵尸终端进程
fn check_zombie_terminals() {
    let mut tasks = match TERMINAL_TASKS.lock() {
        Ok(t) => t,
        Err(_) => return,
    };

    let mut i = 0;
    while i < tasks.len() {
        match tasks[i].try_wait() {
            Ok(Some(_)) => {
                // 进程已退出，将其移除
                log::info!("Process exited: {:?}", tasks[i].process_id());
                tasks.remove(i);
            }
            Ok(None) => {
                // 仍在运行
                i += 1;
            }
            Err(err) => {
                // 检查状态时出错，将其移除
                log::info!(
                    "Process exited with error: {:?}, err: {err}",
                    tasks[i].process_id()
                );
                tasks.remove(i);
            }
        }
    }
}

/// 确保清理任务正在运行
fn ensure_cleanup_task() {
    let mut task_handle = CLEANUP_TASK.lock().unwrap();
    if task_handle.is_none() {
        let handle = std::thread::spawn(|| {
            log::info!("Started cleanup task");
            let mut last_service_cleanup = Instant::now();
            loop {
                // 每 100ms 检查一次僵尸进程
                check_zombie_terminals();

                // 每 5 分钟检查一次非活动服务
                if last_service_cleanup.elapsed() > Duration::from_secs(300) {
                    cleanup_inactive_services();
                    last_service_cleanup = Instant::now();
                }

                std::thread::sleep(Duration::from_millis(100));
            }
        });
        * task_handle = Some(handle);
    }
}

#[cfg(target_os = "linux")]
pub fn get_terminal_session_count(include_zombie_tasks: bool) -> usize {
    let mut c = TERMINAL_SERVICES.lock().unwrap().len();
    if include_zombie_tasks {
        c += TERMINAL_TASKS.lock().unwrap().len();
    }
    c
}

/// 供跨模块使用的用户令牌包装类型。
///
/// # 设计说明
/// 在 Windows 上，该类型定义于 terminal_helper.rs 并在此处重新导出。
/// 在非 Windows 平台上，该类型直接定义于此处。
/// 该设计避免了循环依赖，同时保持 API 一致。
/// 两处定义必须具有完全相同的公共 API（new、as_raw 方法）。
#[cfg(not(target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserToken(pub usize);

#[cfg(not(target_os = "windows"))]
impl UserToken {
    pub fn new(handle: usize) -> Self {
        Self(handle)
    }

    pub fn as_raw(&self) -> usize {
        self.0
    }
}

#[cfg(target_os = "windows")]
pub use super::terminal_helper::UserToken;

#[derive(Clone)]
pub struct TerminalService {
    sp: GenericService,
    user_token: Option<UserToken>,
}

impl Deref for TerminalService {
    type Target = ServiceTmpl<ConnInner>;

    fn deref(&self) -> &Self::Target {
        &self.sp
    }
}

impl DerefMut for TerminalService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sp
    }
}

pub fn get_service_name(source: VideoSource, idx: usize) -> String {
    format!("{}{}", source.service_name_prefix(), idx)
}

pub fn new(
    service_id: String,
    is_persistent: bool,
    user_token: Option<UserToken>,
) -> GenericService {
    // 以初始持久化设置创建服务
    allow_err!(get_or_create_service(
        service_id.clone(),
        is_persistent,
        user_token.is_some()
    ));
    let svc = TerminalService {
        sp: GenericService::new(service_id.clone(), false),
        user_token,
    };
    GenericService::run(&svc.clone(), move |sp| run(sp, service_id.clone()));
    svc.sp
}

fn run(sp: TerminalService, service_id: String) -> ResultType<()> {
    while sp.ok() {
        let responses = TerminalServiceProxy::new(service_id.clone(), None, sp.user_token.clone())
            .read_outputs();
        for response in responses {
            let mut msg_out = Message::new();
            msg_out.set_terminal_response(response);
            sp.send(msg_out);
        }

        thread::sleep(Duration::from_millis(30)); // Read at ~33fps for responsive terminal
    }

    // 循环退出时清理非持久化服务
    if let Some(service) = get_service(&service_id) {
        let should_remove = !service.lock().unwrap().is_persistent;
        if should_remove {
            remove_service(&service_id);
        }
    }

    Ok(())
}

/// 终端会话的输出缓冲区
struct OutputBuffer {
    lines: VecDeque<Vec<u8>>,
    total_size: usize,
    last_line_incomplete: bool,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            total_size: 0,
            last_line_incomplete: false,
        }
    }

    fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        // 处理不完整的行
        let mut start = 0;
        if self.last_line_incomplete {
            if let Some(last_line) = self.lines.back_mut() {
                // 在新数据中查找第一个换行符
                if let Some(newline_pos) = data.iter().position(|&b| b == b'\n') {
                    last_line.extend_from_slice(&data[..=newline_pos]);
                    self.total_size += newline_pos + 1;
                    start = newline_pos + 1;
                    self.last_line_incomplete = false;
                } else {
                    // 仍没有换行符，全部追加
                    last_line.extend_from_slice(data);
                    self.total_size += data.len();
                    start = data.len();
                }
            }
        }

        // 处理剩余数据
        let remaining = &data[start..];
        let ends_with_newline = remaining.last() == Some(&b'\n');

        // 按行拆分
        let lines: Vec<&[u8]> = remaining.split(|&b| b == b'\n').collect();

        for (i, line) in lines.iter().enumerate() {
            if i == lines.len() - 1 && !ends_with_newline && !line.is_empty() {
                // 末尾没有换行符的最后一行
                self.last_line_incomplete = true;
            }

            if !line.is_empty() || i < lines.len() - 1 {
                let mut line_data = line.to_vec();
                if i < lines.len() - 1 || ends_with_newline {
                    line_data.push(b'\n');
                }

                self.total_size += line_data.len();
                self.lines.push_back(line_data);
            }
        }

        // 缓冲区过大时裁剪旧数据
        while self.total_size > MAX_OUTPUT_BUFFER_SIZE || self.lines.len() > MAX_BUFFER_LINES {
            if self.lines.len() == 1 && self.last_line_incomplete {
                // 只剩下一个超长且没有换行符的行：保留其较新的一半
                // 而不是直接丢弃，这样每半个缓冲区只需复制一次。
                let line = &mut self.lines[0];
                line.drain(..line.len().saturating_sub(MAX_OUTPUT_BUFFER_SIZE / 2));
                self.total_size = line.len();
                break;
            }
            if let Some(removed) = self.lines.pop_front() {
                if removed.len() > self.total_size {
                    log::error!(
                        "OutputBuffer total_size underflow avoided: total_size={}, removed_len={}, lines_len={}",
                        self.total_size,
                        removed.len(),
                        self.lines.len()
                    );
                    self.total_size = self.lines.iter().map(|line| line.len()).sum();
                } else {
                    self.total_size -= removed.len();
                }
                if self.lines.is_empty() {
                    self.last_line_incomplete = false;
                }
            } else {
                log::error!(
                    "OutputBuffer trim invariant broken: total_size={}, lines_len=0",
                    self.total_size
                );
                self.total_size = 0;
                self.last_line_incomplete = false;
                break;
            }
        }
    }

    fn get_recent(&self, max_bytes: usize) -> Vec<u8> {
        if max_bytes == 0 {
            return Vec::new();
        }
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut size = 0;

        // 从最新到最旧收集完整的块，保持时间顺序的连续性。
        // 如果最新的块本身就超过 max_bytes，则取其尾部（截断可能会截断
        // ANSI 转义序列，但终端会在后续输出时自行修正）。
        for line in self.lines.iter().rev() {
            if size + line.len() > max_bytes {
                if size == 0 && line.len() > max_bytes {
                    // 单个超大的块：取其尾部以保留最新的内容。
                    // 将偏移量向前对齐到 UTF-8 字符边界，使下游
                    // 客户端（例如 Dart）按 UTF-8 文本解码负载时不会
                    // 遇到被截断的码点。protobuf 的 bytes 字段本身允许
                    // 任意字节；这只是针对客户端解码的尽力而为的缓解措施。
                    let mut offset = line.len() - max_bytes;
                    // 最多跳过 3 个后续字节（UTF-8 最长 4 字节序列）。
                    // 避免在非 UTF-8 二进制数据上无休止地跳过。
                    let mut skipped = 0u8;
                    while skipped < 3
                        && offset < line.len()
                        && (line[offset] & 0b1100_0000) == 0b1000_0000
                    {
                        offset += 1;
                        skipped += 1;
                    }
                    // 如果跳过了所有剩余字节（退化数据），则丢弃该
                    // 块，而不是输出一个在客户端解码效果很差的切片。
                    if offset < line.len() {
                        chunks.push(&line[offset..]);
                        size = line.len() - offset;
                    }
                }
                break;
            }
            size += line.len();
            chunks.push(line);
        }

        // 反转以恢复时间顺序并拼接
        chunks.reverse();
        let mut result = Vec::with_capacity(size);
        for chunk in chunks {
            result.extend_from_slice(chunk);
        }

        result
    }
}

/// 找出 `buf` 的最大前缀，使其结尾不会落在 UTF-8 的
/// 码点中间。无效字节被视为完整字节，以便它们可以继续
/// 向下传递，并在需要时以替换字符呈现。
fn find_utf8_split_point(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }

    let start = buf.len().saturating_sub(3);
    for i in (start..buf.len()).rev() {
        let b = buf[i];
        if b & 0x80 == 0 {
            return buf.len();
        }
        if b & 0xC0 == 0x80 {
            continue;
        }

        let seq_len = if b & 0xE0 == 0xC0 {
            2
        } else if b & 0xF0 == 0xE0 {
            3
        } else if b & 0xF8 == 0xF0 {
            4
        } else {
            return buf.len();
        };

        return if buf.len() - i >= seq_len {
            buf.len()
        } else {
            i
        };
    }

    buf.len()
}

// 终端输出目前端到端遵循 UTF-8 文本模型：服务
// 将回放缓冲区保持在 UTF-8 边界上，Flutter 在写入 xterm 之前把负载字节解码为
// UTF-8 文本。该累加器只用于避免把
// 末尾的 UTF-8 码点拆分到多次 PTY 读取中。若要支持非 UTF-8 终端，
// 还需要一套单独的设计，涵盖远端编码检测、Flutter
// 解码、回放截断和输入转码。
#[derive(Default)]
struct Utf8ChunkAccumulator {
    remainder: Vec<u8>,
}

impl Utf8ChunkAccumulator {
    fn push_chunk(&mut self, mut data: Vec<u8>) -> Option<Vec<u8>> {
        if data.is_empty() {
            return None;
        }

        let had_remainder = !self.remainder.is_empty();
        if had_remainder {
            let mut combined = std::mem::take(&mut self.remainder);
            combined.extend_from_slice(&data);
            data = combined;
        }

        let split = find_utf8_split_point(&data);
        if split == data.len() {
            return Some(data);
        }

        // 只有当有证据表明前面的字节已经是 UTF-8 文本时，
        // 才暂存候选的不完整后缀。如果 split 为 0，则整个
        // 读取内容可能是一个 UTF-8 字符的开头，因此保留给下一次读取。
        if !had_remainder && split > 0 && std::str::from_utf8(&data[..split]).is_err() {
            return Some(data);
        }

        self.remainder = data.split_off(split);
        if data.is_empty() {
            None
        } else {
            Some(data)
        }
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        if self.remainder.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.remainder))
        }
    }
}

/// 尝试通过输出通道发送数据，并对丢弃日志做限速。
/// 若调用方应跳出读取循环（通道已断开），则返回 `true`。
fn try_send_output(
    output_tx: &mpsc::SyncSender<Vec<u8>>,
    data: Vec<u8>,
    terminal_id: i32,
    label: &str,
    drop_count: &mut u64,
    last_drop_warn: &mut Instant,
) -> bool {
    match output_tx.try_send(data) {
        Ok(_) => {
            if *drop_count > 0 {
                log::trace!(
                    "Terminal {}{} output channel recovered, dropped {} chunks since last report",
                    terminal_id,
                    label,
                    * drop_count
                );
                * drop_count = 0;
            }
            false
        }
        Err(mpsc::TrySendError::Full(_)) => {
            * drop_count += 1;
            if last_drop_warn.elapsed() >= Duration::from_secs(5) {
                log::trace!(
                    "Terminal {}{} output channel full, dropped {} chunks in last {:?}",
                    terminal_id,
                    label,
                    * drop_count,
                    last_drop_warn.elapsed()
                );
                * drop_count = 0;
                * last_drop_warn = Instant::now();
            }
            false
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            log::debug!(
                "Terminal {}{} output channel disconnected",
                terminal_id,
                label
            );
            true
        }
    }
}

pub struct TerminalSession {
    pub created_at: Instant,
    last_activity: Instant,
    pty_pair: Option<portable_pty::PtyPair>,
    child: Option<Box<dyn Child + std::marker::Send + Sync>>,
    // 用于向写入线程发送输入的通道
    input_tx: Option<SyncSender<Vec<u8>>>,
    // 用于从读取线程接收输出的通道
    output_rx: Option<Receiver<Vec<u8>>>,
    exiting: Arc<AtomicBool>,
    // 线程句柄
    reader_thread: Option<thread::JoinHandle<()>>,
    writer_thread: Option<thread::JoinHandle<()>>,
    output_buffer: OutputBuffer,
    title: String,
    pid: u32,
    rows: u16,
    cols: u16,
    // 跟踪是否已经发送过 closed 消息
    closed_message_sent: bool,
    // 用于处理重连的会话状态机
    state: SessionState,
    // 辅助模式：PTY 由辅助进程管理，通过消息协议通信
    #[cfg(target_os = "windows")]
    is_helper_mode: bool,
    // 辅助进程的句柄，用于会话关闭时终止进程
    #[cfg(target_os = "windows")]
    helper_process_handle: Option<SendableHandle>,
}

impl TerminalSession {
    fn new(terminal_id: i32, rows: u16, cols: u16) -> Self {
        Self {
            created_at: Instant::now(),
            last_activity: Instant::now(),
            pty_pair: None,
            child: None,
            input_tx: None,
            output_rx: None,
            exiting: Arc::new(AtomicBool::new(false)),
            reader_thread: None,
            writer_thread: None,
            output_buffer: OutputBuffer::new(),
            title: format!("Terminal {}", terminal_id),
            pid: 0,
            rows,
            cols,
            closed_message_sent: false,
            state: SessionState::Closed,
            #[cfg(target_os = "windows")]
            is_helper_mode: false,
            #[cfg(target_os = "windows")]
            helper_process_handle: None,
        }
    }

    fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    // 该辅助函数用于确保在线程 join 之后才 drop 子进程。
    // 不过在 macOS 上这并非严格必要。
    fn stop(&mut self) {
        self.state = SessionState::Closed;
        self.exiting.store(true, Ordering::SeqCst);

        // drop 输入通道以通知写入线程退出
        if let Some(input_tx) = self.input_tx.take() {
            // 发送一个末尾换行符，确保读取线程能读到一些数据，然后退出。
            // 在 Windows 和 Linux 上这是必需的。
            // 尽管下面会调用 `self.pty_pair = None;`，这里仍然可以发送一个末尾换行符。
            #[cfg(target_os = "windows")]
            let final_msg = if self.is_helper_mode {
                encode_helper_message(MSG_TYPE_DATA, b"\r\n")
            } else {
                b"\r\n".to_vec()
            };
            #[cfg(not(target_os = "windows"))]
            let final_msg = b"\r\n".to_vec();

            if let Err(e) = input_tx.send(final_msg) {
                log::warn!("Failed to send final newline to the terminal: {}", e);
            }
            drop(input_tx);
        }
        self.output_rx = None;

        // 关键：在辅助模式下，必须先终止辅助进程，再 join 线程！
        // 读取线程阻塞在 output_pipe.read() 上，只有当
        // 辅助进程退出时它才会返回 EOF。如果先尝试 join 读取线程，就会死锁。
        //
        // 辅助模式下的顺序：
        // 1. 通知退出并关闭输入通道（已完成）
        // 2. 终止辅助进程（导致输出管道 EOF）
        // 3. join 读取线程（此时已因 EOF 而解除阻塞）
        // 4. join 写入线程
        #[cfg(target_os = "windows")]
        if self.is_helper_mode {
            if let Some(helper_handle) = self.helper_process_handle.take() {
                let handle = helper_handle.as_raw();
                log::debug!("Helper mode: terminating helper process before joining threads...");

                // 给辅助进程很短的时间优雅退出（它应该能检测到管道关闭）
                // 但不要等太久 —— 我们需要解除读取线程的阻塞
                let wait_result = unsafe { WinWaitForSingleObject(handle, 100) };

                if wait_result == WIN_WAIT_OBJECT_0 {
                    log::debug!("Helper process exited gracefully");
                } else {
                    // 强制终止以解除读取线程的阻塞
                    log::debug!("Force terminating helper process to unblock reader thread");
                    unsafe {
                        let _ = WinTerminateProcess(handle, 0);
                    }
                }

                unsafe {
                    let _ = WinCloseHandle(handle);
                }
            }
        }

        // 1. Windows（非辅助模式）
        //    `pty_pair` 使用管道。https://github.com/rustdesk-org/wezterm/blob/80174f8009f41565f0fa8c66dab90d4f9211ae16/pty/src/win/conpty.rs#L16
        //     `read()` 可能会卡在 https://github.com/rustdesk-org/wezterm/blob/80174f8009f41565f0fa8c66dab90d4f9211ae16/filedescriptor/src/windows.rs#L345
        //     我们可以关闭管道来通知读取线程退出。
        //     在 https://github.com/rustdesk-org/wezterm/blob/80174f8009f41565f0fa8c66dab90d4f9211ae16/pty/src/win/psuedocon.rs#L86 之后，我的测试中读取线程读到的是 `[27, 91, 63, 57, 48, 48, 49, 108, 27, 91, 63, 49, 48, 48, 52, 108]`。
        // 2. Linux
        //    `pty_pair` 使用 `libc::openpty`。https://github.com/rustdesk-org/wezterm/blob/80174f8009f41565f0fa8c66dab90d4f9211ae16/pty/src/unix.rs#L32
        //    我们也可以先调用 drop 方法。https://github.com/rustdesk-org/wezterm/blob/80174f8009f41565f0fa8c66dab90d4f9211ae16/pty/src/unix.rs#L352
        //    drop `pty_pair` 后，读取线程会收到 [13, 10]。
        // 3. macOS
        //    目前尚未发现卡住的案例，还需要更多测试。
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            self.pty_pair = None;
        }

        // 等待线程结束
        // 在 Windows 上应先 join 读取线程，再 join 写入线程。
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }

        // 写入线程（而非子进程）退出后，读取仍能读到最后的 "\r\n"
        // （在 Linux 上，我的测试结果如此。）
        // 但出于安全考虑，我们仍会向写入线程发送 "\r\n"，并让读取线程先退出。
        if let Some(writer_thread) = self.writer_thread.take() {
            let _ = writer_thread.join();
        }

        if let Some(mut child) = self.child.take() {
            // 杀掉进程
            let _ = child.kill();
            add_to_reaper(child);
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // 确保会话 drop 时子进程被正确处理
        self.stop();
    }
}

/// 可在连接断开后继续存活的持久化终端服务
pub struct PersistentTerminalService {
    service_id: String,
    sessions: HashMap<i32, Arc<Mutex<TerminalSession>>>,
    pub created_at: Instant,
    last_activity: Instant,
    pub is_persistent: bool,
    needs_session_sync: bool,
    is_specified_user: bool,
}

impl PersistentTerminalService {
    pub fn new(service_id: String, is_persistent: bool, is_specified_user: bool) -> Self {
        Self {
            service_id,
            sessions: HashMap::new(),
            created_at: Instant::now(),
            last_activity: Instant::now(),
            is_persistent,
            needs_session_sync: false,
            is_specified_user,
        }
    }

    fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// 获取终端元数据列表
    pub fn list_terminals(&self) -> Vec<(i32, String, u32, Instant)> {
        self.sessions
            .iter()
            .map(|(id, session)| {
                let s = session.lock().unwrap();
                (*id, s.title.clone(), s.pid, s.created_at)
            })
            .collect()
    }

    /// 获取某个终端的缓冲输出
    pub fn get_terminal_buffer(&self, terminal_id: i32, max_bytes: usize) -> Option<Vec<u8>> {
        self.sessions.get(&terminal_id).map(|session| {
            let session = session.lock().unwrap();
            session.output_buffer.get_recent(max_bytes)
        })
    }

    /// 获取用于恢复的终端信息
    pub fn get_terminal_info(&self, terminal_id: i32) -> Option<(u16, u16, Vec<u8>)> {
        self.sessions.get(&terminal_id).map(|session| {
            let session = session.lock().unwrap();
            (
                session.rows,
                session.cols,
                session
                    .output_buffer
                    .get_recent(DEFAULT_RECONNECT_BUFFER_BYTES),
            )
        })
    }

    /// 检查服务是否有活动终端
    pub fn has_active_terminals(&self) -> bool {
        !self.sessions.is_empty()
    }

    fn reset_status(&mut self, is_persistent: bool) {
        self.is_persistent = is_persistent;
        self.needs_session_sync = true;
        for session in self.sessions.values() {
            let mut session = session.lock().unwrap();
            session.state = SessionState::Closed;
        }
    }
}

pub struct TerminalServiceProxy {
    service_id: String,
    is_persistent: bool,
    #[cfg(target_os = "windows")]
    user_token: Option<UserToken>,
}

pub fn set_persistent(service_id: &str, is_persistent: bool) -> Result<()> {
    if let Some(service) = get_service(service_id) {
        service.lock().unwrap().is_persistent = is_persistent;
        Ok(())
    } else {
        Err(anyhow!("Service {} not found", service_id))
    }
}

impl TerminalServiceProxy {
    pub fn new(
        service_id: String,
        is_persistent: Option<bool>,
        _user_token: Option<UserToken>,
    ) -> Self {
        // 如果服务存在，则从中获取持久化设置
        let is_persistent =
            is_persistent.unwrap_or(if let Some(service) = get_service(&service_id) {
                service.lock().unwrap().is_persistent
            } else {
                false
            });
        TerminalServiceProxy {
            service_id,
            is_persistent,
            #[cfg(target_os = "windows")]
            user_token: _user_token,
        }
    }

    pub fn get_service_id(&self) -> &str {
        &self.service_id
    }

    pub fn handle_action(&mut self, action: &TerminalAction) -> Result<Option<TerminalResponse>> {
        let service = match get_service(&self.service_id) {
            Some(s) => s,
            None => {
                let mut response = TerminalResponse::new();
                let mut error = TerminalError::new();
                error.message = format!("Terminal service {} not found", self.service_id);
                response.set_error(error);
                return Ok(Some(response));
            }
        };
        service.lock().unwrap().update_activity();
        match &action.union {
            Some(terminal_action::Union::Open(open)) => {
                self.handle_open(&mut service.lock().unwrap(), open)
            }
            Some(terminal_action::Union::Resize(resize)) => {
                let session = service
                    .lock()
                    .unwrap()
                    .sessions
                    .get(&resize.terminal_id)
                    .cloned();
                self.handle_resize(session, resize)
            }
            Some(terminal_action::Union::Data(data)) => {
                let session = service
                    .lock()
                    .unwrap()
                    .sessions
                    .get(&data.terminal_id)
                    .cloned();
                self.handle_data(session, data)
            }
            Some(terminal_action::Union::Close(close)) => {
                self.handle_close(&mut service.lock().unwrap(), close)
            }
            _ => Ok(None),
        }
    }

    fn handle_open(
        &self,
        service: &mut PersistentTerminalService,
        open: &OpenTerminal,
    ) -> Result<Option<TerminalResponse>> {
        let mut response = TerminalResponse::new();

        // 当客户端请求了一个不存在的 terminal_id，但仍有
        // 存活的持久化会话时，将 ID 最低的会话重新映射到请求的
        // terminal_id。这处理了重连时 _nextTerminalId 重置为 1，
        // 而服务端会话的 ID 不连续（例如 {2: htop}）的情况。
        //
        // 客户端请求的 terminal_id 可能与任何存活会话的 ID 都不匹配
        // （例如 _nextTerminalId 递增超出了存活会话的 ID）。该重映射只是
        // 一次性的句柄重新分配 —— 只有第一次重连会触发它，因为
        // 之后 needs_session_sync 会被清除。其余会话通过
        // `persistent_sessions` 以各自原有的服务端 ID 传回。
        if !service.sessions.contains_key(&open.terminal_id)
            && service.needs_session_sync
            && !service.sessions.is_empty()
        {
            if let Some(&lowest_id) = service.sessions.keys().min() {
                log::info!(
                    "Remapping persistent session {} -> {} for reconnection",
                    lowest_id,
                    open.terminal_id
                );
                if let Some(session_arc) = service.sessions.remove(&lowest_id) {
                    service.sessions.insert(open.terminal_id, session_arc);
                }
            }
        }

        // 检查终端是否已存在
        if let Some(session_arc) = service.sessions.get(&open.terminal_id) {
            // 重连到已存在的终端
            let mut session = session_arc.lock().unwrap();
            // 直接进入 Active 状态，并带上待回放数据以立即开始流式传输。
            // 该回放将 output_buffer 中的历史数据与重连时已经
            // 积压待处理的通道数据结合起来，使客户端无需修改 protobuf schema
            // 即可抑制过期的 xterm 查询响应。
            // 断线期间不会调用 read_outputs()；通道数据仍可能丢失，
            // 如果 output_rx 在重连排空之前就被填满。
            let mut buffer = session
                .output_buffer
                .get_recent(DEFAULT_RECONNECT_BUFFER_BYTES);
            let mut reconnect_backlog = Vec::new();
            if let Some(output_rx) = &session.output_rx {
                // 限制重连时的排空量，避免话痨式的 PTY 让 OpenTerminal
                // 在这个循环中无限期运行。剩余输出由 read_outputs() 排空。
                for _ in 0..CHANNEL_BUFFER_SIZE {
                    let Ok(data) = output_rx.try_recv() else {
                        break;
                    };
                    reconnect_backlog.push(data);
                }
            }
            let has_reconnect_backlog = !reconnect_backlog.is_empty();
            for data in reconnect_backlog {
                session.output_buffer.append(&data);
            }
            if has_reconnect_backlog {
                buffer = session
                    .output_buffer
                    .get_recent(DEFAULT_RECONNECT_BUFFER_BYTES);
            }
            let has_pending = !buffer.is_empty();
            session.state = SessionState::Active {
                pending_buffer: if has_pending { Some(buffer) } else { None },
                // 重连时始终触发两阶段 SIGWINCH 以强制 TUI 程序重绘，
                // 无论是否存在待处理的缓冲区数据。这样可避免出现
                // 缓冲区为空但 TUI 程序（top/htop）仍需完整重绘的边界情况。
                sigwinch: SigwinchPhase::TempResize {
                    retries: MAX_SIGWINCH_PHASE_ATTEMPTS,
                },
            };
            let mut opened = TerminalOpened::new();
            opened.terminal_id = open.terminal_id;
            opened.success = true;
            opened.message = if has_pending {
                "Reconnected to existing terminal with pending output".to_string()
            } else {
                "Reconnected to existing terminal".to_string()
            };
            opened.pid = session.pid;
            opened.service_id = self.service_id.clone();
            opened.replay_terminal_output = has_pending;
            if service.needs_session_sync {
                if service.sessions.len() > 1 {
                    // 无需把当前终端包含在列表中。
                    // 因为 `persistent_sessions` 用于恢复其他会话。
                    opened.persistent_sessions = service
                        .sessions
                        .keys()
                        .filter(|&id| *id != open.terminal_id)
                        .cloned()
                        .collect();
                }
                service.needs_session_sync = false;
            }
            response.set_opened(opened);

            return Ok(Some(response));
        }

        // Windows 上带有 user_token：使用辅助进程以已登录用户身份运行 shell
        // 这解决了 ConPTY + CreateProcessAsUserW 的兼容性问题，即
        // 当 ConPTY 由 SYSTEM 服务创建、而 shell 通过 CreateProcessAsUserW
        // 以用户身份运行时，vim、Claude Code 等 TUI 程序会卡死的问题。
        #[cfg(target_os = "windows")]
        if self.user_token.is_some() {
            return self.handle_open_with_helper(service, open);
        }

        // 创建新的终端会话
        log::info!(
            "Creating new terminal {} for service {}",
            open.terminal_id,
            service.service_id
        );
        let mut session =
            TerminalSession::new(open.terminal_id, open.rows as u16, open.cols as u16);

        let pty_size = PtySize {
            rows: open.rows as u16,
            cols: open.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        };

        log::debug!("Opening PTY with size: {}x{}", open.rows, open.cols);
        let pty_system = portable_pty::native_pty_system();
        let pty_pair = pty_system.openpty(pty_size).context("Failed to open PTY")?;

        // 使用该平台的默认 shell
        let shell = get_default_shell();
        log::debug!("Using shell: {}", shell);

        #[allow(unused_mut)]
        let mut cmd = CommandBuilder::new(&shell);

        #[cfg(target_os = "windows")]
        configure_utf8_shell_command(&shell, &mut cmd);

        // macOS 专用的终端配置
        // 1. 使用登录 shell（-l）加载用户的 shell 配置（~/.zprofile、~/.bash_profile）
        //    这可确保 PATH 包含 Homebrew 的路径（/opt/homebrew/bin、/usr/local/bin）
        // 2. 设置 TERM 环境变量以获得正确的终端行为
        //    这可修复控制序列相关的问题（例如 Delete/Backspace 键）
        //    macOS 的 terminfo 使用十六进制命名：xterm 条目中 '78' = 'x'
        // 注意：在 Linux 上，`TERM` 在 src/platform/linux.rs 的 try_start_server_() 中设置
        #[cfg(target_os = "macos")]
        {
            // 以登录 shell 启动以加载用户环境（PATH 等）
            cmd.arg("-l");
            log::debug!("Added -l flag for macOS login shell");

            let term = if std::path::Path::new("/usr/share/terminfo/78/xterm-256color").exists() {
                "xterm-256color"
            } else {
                "xterm"
            };
            cmd.env("TERM", term);
            log::debug!("Set TERM={} for macOS PTY", term);

            if should_force_process_utf8_ctype() {
                cmd.env_remove("LC_ALL");
                cmd.env("LC_CTYPE", "en_US.UTF-8");
                log::debug!("Set LC_CTYPE=en_US.UTF-8 for macOS PTY");
            }
        }

        // 注意：在 Windows 上带有 user_token 时，我们使用辅助模式（handle_open_with_helper）
        // 该分支在本函数更早的位置就已分发。这条代码路径只有在
        // user_token 为 None 时才会走到（例如直接以用户身份运行，而非作为 SYSTEM 服务）。

        log::debug!("Spawning shell process...");
        let child = pty_pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn command")?;

        let writer = pty_pair
            .master
            .take_writer()
            .context("Failed to get writer")?;

        let reader = pty_pair
            .master
            .try_clone_reader()
            .context("Failed to get reader")?;

        session.pid = child.process_id().unwrap_or(0) as u32;

        // 为输入/输出创建通道
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_BUFFER_SIZE);
        let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_BUFFER_SIZE);

        // 启动写入线程
        let terminal_id = open.terminal_id;
        let writer_thread = thread::spawn(move || {
            let mut writer = writer;
            while let Ok(data) = input_rx.recv() {
                if let Err(e) = writer.write_all(&data) {
                    log::error!("Terminal {} write error: {}", terminal_id, e);
                    break;
                }
                if let Err(e) = writer.flush() {
                    log::error!("Terminal {} flush error: {}", terminal_id, e);
                }
            }
            log::debug!("Terminal {} writer thread exiting", terminal_id);
        });

        let exiting = session.exiting.clone();
        // 启动读取线程
        let terminal_id = open.terminal_id;
        let reader_thread = thread::spawn(move || {
            let mut reader = reader;
            let mut buf = vec![0u8; 4096];
            let mut utf8_chunks = Utf8ChunkAccumulator::default();
            let mut drop_count: u64 = 0;
            // 初始化为 5 秒多以前，以便第一次丢弃时立即触发警告。
            let mut last_drop_warn = Instant::now() - Duration::from_secs(6);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF
                        // 在 macOS 上子进程退出时可能会走到这个分支。
                        // 但在我的测试中，Linux 和 Windows 上不会走到。
                        if let Some(data) = utf8_chunks.finish() {
                            let _ = try_send_output(
                                &output_tx,
                                data,
                                terminal_id,
                                "",
                                &mut drop_count,
                                &mut last_drop_warn,
                            );
                        }
                        break;
                    }
                    Ok(n) => {
                        if exiting.load(Ordering::SeqCst) {
                            break;
                        }
                        let Some(data) = utf8_chunks.push_chunk(buf[..n].to_vec()) else {
                            continue;
                        };
                        // 使用 try_send 以避免通道满时阻塞读取线程。
                        // 断线期间，运行循环（sp.ok()）会停止，read_outputs() 也
                        // 不再被调用，因此通道不会被排空。此时阻塞式发送会
                        // 使读取线程死锁。
                        // 注意：如果通道被填满，断线期间产生的数据可能会丢失，
                        // 因为 output_buffer 只在 read_outputs() 中更新。该缓冲区将
                        // 包含断线之前的历史数据，而不是断线之后产生的数据。
                        if try_send_output(
                            &output_tx,
                            data,
                            terminal_id,
                            "",
                            &mut drop_count,
                            &mut last_drop_warn,
                        ) {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // 在我的测试中未走到这个分支，但仍加上 `exiting` 检查以确保能够退出。
                        if exiting.load(Ordering::SeqCst) {
                            break;
                        }
                        // 对于非阻塞 I/O，短暂休眠
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        log::error!("Terminal {} read error: {}", terminal_id, e);
                        break;
                    }
                }
            }
            log::debug!("Terminal {} reader thread exiting", terminal_id);
        });

        session.pty_pair = Some(pty_pair);
        session.child = Some(child);
        session.input_tx = Some(input_tx);
        session.output_rx = Some(output_rx);
        session.reader_thread = Some(reader_thread);
        session.writer_thread = Some(writer_thread);
        session.state = SessionState::Active {
            pending_buffer: None,
            sigwinch: SigwinchPhase::Idle,
        };

        let mut opened = TerminalOpened::new();
        opened.terminal_id = open.terminal_id;
        opened.success = true;
        opened.message = "Terminal opened".to_string();
        opened.pid = session.pid;
        opened.service_id = service.service_id.clone();
        if service.needs_session_sync {
            if !service.sessions.is_empty() {
                opened.persistent_sessions = service.sessions.keys().cloned().collect();
            }
            service.needs_session_sync = false;
        }
        response.set_opened(opened);

        log::info!(
            "Terminal {} opened successfully with PID {}",
            open.terminal_id,
            session.pid
        );

        // 存储该会话
        service
            .sessions
            .insert(open.terminal_id, Arc::new(Mutex::new(session)));

        Ok(Some(response))
    }

    /// 仅 Windows：使用辅助进程模式打开终端
    /// 这解决了 ConPTY + CreateProcessAsUserW 的兼容性问题。
    /// 辅助进程以已登录用户身份运行并创建 ConPTY + shell，
    /// 并通过命名管道与本服务通信。
    #[cfg(target_os = "windows")]
    fn handle_open_with_helper(
        &self,
        service: &mut PersistentTerminalService,
        open: &OpenTerminal,
    ) -> Result<Option<TerminalResponse>> {
        let mut response = TerminalResponse::new();

        log::info!(
            "Creating new terminal {} using helper process for service: {}",
            open.terminal_id,
            service.service_id
        );

        let mut session =
            TerminalSession::new(open.terminal_id, open.rows as u16, open.cols as u16);

        // 为该终端生成唯一的管道名称
        let pipe_id = uuid::Uuid::new_v4();
        let input_pipe_name = format!(r"\\.\pipe\rustdesk_term_in_{}", pipe_id);
        let output_pipe_name = format!(r"\\.\pipe\rustdesk_term_out_{}", pipe_id);

        log::debug!(
            "Creating pipes: input={}, output={}",
            input_pipe_name,
            output_pipe_name
        );

        // 尽早获取 user_token —— 创建 DACL 和启动辅助进程都需要它
        let user_token = self
            .user_token
            .ok_or_else(|| anyhow!("user_token is required for helper mode"))?;

        // 创建管道（服务端，暂不等待连接）
        // input_pipe：服务向它写入，辅助进程从它读取
        // output_pipe：服务从它读取，辅助进程向它写入
        // 使用 OwnedHandle 实现 RAII —— 出错时句柄会自动关闭
        // 传入 user_token 以创建受限 DACL（仅 SYSTEM + 用户可访问）
        let input_pipe_handle = OwnedHandle::new(create_named_pipe_server(
            &input_pipe_name,
            false,
            user_token,
        )?);
        let output_pipe_handle = OwnedHandle::new(create_named_pipe_server(
            &output_pipe_name,
            true,
            user_token,
        )?);

        let helper_process_info = launch_terminal_helper_with_token(
            user_token,
            &input_pipe_name,
            &output_pipe_name,
            open.terminal_id,
            open.rows as u16,
            open.cols as u16,
        )?;

        // 使用 HelperProcessGuard 进行 RAII 清理 —— 出错时终止进程
        // 与仅关闭句柄的 OwnedHandle 不同，该守卫确保
        // 在管道连接失败或发生其他错误时终止辅助进程。
        let helper_process_guard =
            HelperProcessGuard::new(helper_process_info.handle, helper_process_info.pid);
        let helper_pid = helper_process_guard.pid();

        // 等待辅助进程连接到管道
        // 如果失败，HelperProcessGuard 将终止辅助进程
        let mut input_pipe = wait_for_pipe_connection(
            input_pipe_handle,
            &input_pipe_name,
            PIPE_CONNECTION_TIMEOUT_MS,
        )?;
        let mut output_pipe = wait_for_pipe_connection(
            output_pipe_handle,
            &output_pipe_name,
            PIPE_CONNECTION_TIMEOUT_MS,
        )?;

        // 检查管道连接后辅助进程是否仍在运行
        // 这可以在辅助进程启动期间崩溃时做到早期发现
        if !is_helper_process_running(helper_process_guard.as_raw()) {
            return Err(anyhow!(
                "Helper process (PID {}) exited unexpectedly after pipe connection",
                helper_pid
            ));
        }

        // 解除守卫武装并将所有权转移给会话
        // 从此以后，由会话负责终止辅助
        let helper_raw_handle = helper_process_guard.disarm();

        // 使用辅助进程 PID 进行会话跟踪
        // 注意：这是辅助进程的 PID，而不是实际 shell 的 PID。
        // 真正的 shell 在辅助进程内部运行，但其 PID 在此处并未暴露。
        // 对于进程管理（终止、状态），我们需要的是辅助进程的 PID。
        session.pid = helper_pid;

        // 为输入/输出创建通道（与直接 PTY 模式相同）
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_BUFFER_SIZE);
        let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_BUFFER_SIZE);

        // 启动写入线程：从通道读取，向输入管道写入
        let terminal_id = open.terminal_id;
        let writer_thread = thread::spawn(move || {
            while let Ok(data) = input_rx.recv() {
                if let Err(e) = input_pipe.write_all(&data) {
                    log::error!("Terminal {} pipe write error: {}", terminal_id, e);
                    break;
                }
                if let Err(e) = input_pipe.flush() {
                    log::error!("Terminal {} pipe flush error: {}", terminal_id, e);
                }
            }
            log::debug!(
                "Terminal {} writer thread (helper mode) exiting",
                terminal_id
            );
        });

        // 启动读取线程：从输出管道读取，发送到通道
        // 注意：输出管道创建时带有 FILE_FLAG_OVERLAPPED 以支持超时
        // （在 ConnectNamedPipe 期间）。不过一旦转换为 File 句柄，读取就是
        // 同步进行的。下面的 WouldBlock 处理属于防御性代码，实际上可能
        // 不会被触发，因为 File::read() 会阻塞直到有数据可用。
        let exiting = session.exiting.clone();
        let terminal_id = open.terminal_id;
        let reader_thread = thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut utf8_chunks = Utf8ChunkAccumulator::default();
            let mut drop_count: u64 = 0;
            // 初始化为 5 秒多以前，以便第一次丢弃时立即触发警告。
            let mut last_drop_warn = Instant::now() - Duration::from_secs(6);
            loop {
                match output_pipe.read(&mut buf) {
                    Ok(0) => {
                        if let Some(data) = utf8_chunks.finish() {
                            let _ = try_send_output(
                                &output_tx,
                                data,
                                terminal_id,
                                " (helper)",
                                &mut drop_count,
                                &mut last_drop_warn,
                            );
                        }
                        // EOF —— 辅助进程已退出
                        log::debug!("Terminal {} helper output EOF", terminal_id);
                        break;
                    }
                    Ok(n) => {
                        if exiting.load(Ordering::SeqCst) {
                            break;
                        }
                        let Some(data) = utf8_chunks.push_chunk(buf[..n].to_vec()) else {
                            continue;
                        };
                        // 使用 try_send 以避免阻塞读取线程（与直接 PTY 模式相同）
                        if try_send_output(
                            &output_tx,
                            data,
                            terminal_id,
                            " (helper)",
                            &mut drop_count,
                            &mut last_drop_warn,
                        ) {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // 防御性处理：使用同步的 File::read() 时基本不会出现 WouldBlock，
                        // 但仍以优雅方式处理以防万一。
                        if exiting.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        log::error!("Terminal {} pipe read error: {}", terminal_id, e);
                        break;
                    }
                }
            }
            log::debug!(
                "Terminal {} reader thread (helper mode) exiting",
                terminal_id
            );
        });

        // 在辅助模式下，我们没有 pty_pair 或 child —— 这些由辅助进程管理
        session.pty_pair = None;
        session.child = None;
        session.input_tx = Some(input_tx);
        session.output_rx = Some(output_rx);
        session.reader_thread = Some(reader_thread);
        session.writer_thread = Some(writer_thread);
        session.state = SessionState::Active {
            pending_buffer: None,
            sigwinch: SigwinchPhase::Idle,
        };
        session.is_helper_mode = true;
        session.helper_process_handle = Some(SendableHandle::new(helper_raw_handle));

        let mut opened = TerminalOpened::new();
        opened.terminal_id = open.terminal_id;
        opened.success = true;
        opened.message = "Terminal opened (helper mode)".to_string();
        opened.pid = session.pid;
        opened.service_id = service.service_id.clone();
        if service.needs_session_sync {
            if !service.sessions.is_empty() {
                opened.persistent_sessions = service.sessions.keys().cloned().collect();
            }
            service.needs_session_sync = false;
        }
        response.set_opened(opened);

        log::info!(
            "Terminal {} opened successfully using helper process (PID {})",
            open.terminal_id,
            session.pid
        );

        service
            .sessions
            .insert(open.terminal_id, Arc::new(Mutex::new(session)));

        Ok(Some(response))
    }

    fn handle_resize(
        &self,
        session: Option<Arc<Mutex<TerminalSession>>>,
        resize: &ResizeTerminal,
    ) -> Result<Option<TerminalResponse>> {
        if let Some(session_arc) = session {
            let mut session = session_arc.lock().unwrap();
            session.update_activity();
            session.rows = resize.rows as u16;
            session.cols = resize.cols as u16;

            // 注意：这里不清除 sigwinch 阶段。服务端的两阶段
            // SIGWINCH 机制在 read_outputs() 中是自包含的（临时调整尺寸 → 恢复
            // 跨两个轮询周期），因此客户端的尺寸调整纯粹是尺寸同步，
            // 不会影响它。

            // Windows：处理辅助模式与直接 PTY 模式
            #[cfg(target_os = "windows")]
            {
                if session.is_helper_mode {
                    // 辅助模式：通过消息协议发送尺寸调整命令
                    if let Some(input_tx) = &session.input_tx {
                        let msg = encode_resize_message(resize.rows as u16, resize.cols as u16);
                        if let Err(e) = input_tx.send(msg) {
                            log::error!("Failed to send resize to helper: {}", e);
                        }
                    } else {
                        log::warn!(
                            "Terminal {} is in helper mode but input_tx is None, cannot send resize",
                            resize.terminal_id
                        );
                    }
                } else {
                    // 直接 PTY 模式
                    Self::resize_pty(&session, resize)?;
                }
            }

            // 非 Windows：始终为直接 PTY 模式
            #[cfg(not(target_os = "windows"))]
            {
                Self::resize_pty(&session, resize)?;
            }
        }
        Ok(None)
    }

    /// 直接调整 PTY 尺寸（用于非辅助模式）
    fn resize_pty(session: &TerminalSession, resize: &ResizeTerminal) -> Result<()> {
        if let Some(pty_pair) = &session.pty_pair {
            pty_pair.master.resize(PtySize {
                rows: resize.rows as u16,
                cols: resize.cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        }
        Ok(())
    }

    fn handle_data(
        &self,
        session: Option<Arc<Mutex<TerminalSession>>>,
        data: &TerminalData,
    ) -> Result<Option<TerminalResponse>> {
        if let Some(session_arc) = session {
            let input = {
                let mut session = session_arc.lock().unwrap();
                session.update_activity();
                if let Some(input_tx) = session.input_tx.clone() {
                    // 在辅助模式下编码数据，在直接 PTY 模式下原样发送
                    #[cfg(target_os = "windows")]
                    let msg = if session.is_helper_mode {
                        encode_helper_message(MSG_TYPE_DATA, &data.data)
                    } else {
                        data.data.to_vec()
                    };
                    #[cfg(not(target_os = "windows"))]
                    let msg = data.data.to_vec();

                    Some((input_tx, msg))
                } else {
                    None
                }
            };

            if let Some((input_tx, msg)) = input {
                // 在会话锁之外发送；SyncSender::send 在通道满时可能阻塞。
                if let Err(e) = input_tx.send(msg) {
                    log::error!(
                        "Failed to send data to terminal {}: {}",
                        data.terminal_id,
                        e
                    );
                }
            }
        }

        Ok(None)
    }

    fn handle_close(
        &self,
        service: &mut PersistentTerminalService,
        close: &CloseTerminal,
    ) -> Result<Option<TerminalResponse>> {
        let mut response = TerminalResponse::new();

        // 始终关闭并移除该终端
        if let Some(session_arc) = service.sessions.remove(&close.terminal_id) {
            let mut session = session_arc.lock().unwrap();
            let exit_code = if let Some(mut child) = session.child.take() {
                child.kill()?;
                add_to_reaper(child);
                -1 // -1 indicates forced termination
            } else {
                0
            };

            let mut closed = TerminalClosed::new();
            closed.terminal_id = close.terminal_id;
            closed.exit_code = exit_code;
            response.set_closed(closed);
            Ok(Some(response))
        } else {
            Ok(None)
        }
    }

    /// 作为两阶段 SIGWINCH 序列的一部分，执行一次 PTY 尺寸调整。
    /// 尺寸调整成功时返回 true。
    ///
    /// 接收各字段的单独引用，以避免借用整个 TerminalSession，
    /// 否则会与 read_outputs() 中对 session.state 的可变借用冲突。
    fn do_sigwinch_resize(
        terminal_id: i32,
        rows: u16,
        cols: u16,
        pty_pair: &Option<portable_pty::PtyPair>,
        input_tx: &Option<SyncSender<Vec<u8>>>,
        _is_helper_mode: bool,
        action: &SigwinchAction,
    ) -> bool {
        // 若尺寸未初始化则跳过（重连时不应发生，
        // 但仍加以防护，避免调整到无意义的尺寸）。
        if rows == 0 || cols == 0 {
            return false;
        }

        let target_rows = match action {
            SigwinchAction::TempResize => {
                // 对于非常小的终端（≤2 行），减去 1 会导致尺寸不可用
                // （0 或 1 行），因此改为加上 1。两个方向都能触发 SIGWINCH。
                if rows > 2 {
                    rows.saturating_sub(1)
                } else {
                    rows.saturating_add(1)
                }
            }
            SigwinchAction::Restore => rows,
        };

        let phase_name = match action {
            SigwinchAction::TempResize => "temp resize",
            SigwinchAction::Restore => "restore",
        };

        #[cfg(target_os = "windows")]
        let use_helper = _is_helper_mode;
        #[cfg(not(target_os = "windows"))]
        let use_helper = false;

        if use_helper {
            #[cfg(target_os = "windows")]
            {
                let input_tx = match input_tx {
                    Some(tx) => tx,
                    None => return false,
                };
                let msg = encode_resize_message(target_rows, cols);
                if let Err(e) = input_tx.try_send(msg) {
                    log::warn!(
                        "Terminal {} SIGWINCH {} via helper failed: {}",
                        terminal_id,
                        phase_name,
                        e
                    );
                    return false;
                }
                true
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (input_tx, phase_name);
                false
            }
        } else if let Some(pty_pair) = pty_pair {
            if let Err(e) = pty_pair.master.resize(PtySize {
                rows: target_rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                log::warn!(
                    "Terminal {} SIGWINCH {} failed: {}",
                    terminal_id,
                    phase_name,
                    e
                );
                return false;
            }
            true
        } else {
            false
        }
    }

    /// 用于创建可选择压缩的 TerminalResponse 的辅助函数。
    fn create_terminal_data_response(terminal_id: i32, data: Vec<u8>) -> TerminalResponse {
        let mut response = TerminalResponse::new();
        let mut terminal_data = TerminalData::new();
        terminal_data.terminal_id = terminal_id;

        if data.len() > COMPRESS_THRESHOLD {
            let compressed = compress::compress(&data);
            if compressed.len() < data.len() {
                terminal_data.data = bytes::Bytes::from(compressed);
                terminal_data.compressed = true;
            } else {
                terminal_data.data = bytes::Bytes::from(data);
            }
        } else {
            terminal_data.data = bytes::Bytes::from(data);
        }

        response.set_data(terminal_data);
        response
    }

    pub fn read_outputs(&self) -> Vec<TerminalResponse> {
        let service = match get_service(&self.service_id) {
            Some(s) => s,
            None => {
                return vec![];
            }
        };

        // 以尽量短的服务锁时间获取会话引用
        let sessions: Vec<(i32, Arc<Mutex<TerminalSession>>)> = {
            let service = service.lock().unwrap();
            service
                .sessions
                .iter()
                .map(|(id, session)| (*id, session.clone()))
                .collect()
        };

        let mut responses = Vec::new();
        let mut closed_terminals = Vec::new();

        // 每个会话各自持锁处理
        for (terminal_id, session_arc) in sessions {
            if let Ok(mut session) = session_arc.try_lock() {
                // 检查会话是否已结束（读取线程已结束或子进程已退出）。
                // 在 Linux 上，shell 退出时 PTY 读取线程可能不会返回 EOF
                // （克隆的 master fd 会让读端保持打开），因此我们还通过 try_wait() 轮询子
                // 进程，作为备用的检测机制。
                let mut should_send_closed = false;
                if !session.closed_message_sent {
                    if let Some(thread) = &session.reader_thread {
                        if thread.is_finished() {
                            should_send_closed = true;
                        }
                    }
                    if !should_send_closed {
                        if let Some(child) = &mut session.child {
                            match child.try_wait() {
                                Ok(Some(_)) => {
                                    should_send_closed = true;
                                }
                                Ok(None) => {} // still running
                                Err(e) => {
                                    log::warn!("Terminal {} child wait error: {}", terminal_id, e);
                                }
                            }
                        }
                    }
                    if should_send_closed {
                        session.closed_message_sent = true;
                    }
                }
                // 把 closed 消息放在这里是可以的。
                // 因为 `reader_thread` 会在 `stop()` 中被 join，
                // 而 `stop()` 会在会话被 drop 之前调用。
                if should_send_closed {
                    closed_terminals.push(terminal_id);
                }

                // 无论会话状态如何，始终排空输出通道。
                // 处于 Active 时：数据发送给客户端。处于 Closed 时（在同一次
                // 连接内）：数据缓存在 output_buffer 中，以便重连时回放。
                // 注意：在真正断线时，运行循环会退出，read_outputs()
                // 不会被调用，因此断线后产生的通道数据可能会丢失。
                let mut has_activity = false;
                let mut received_data = Vec::new();
                if let Some(output_rx) = &session.output_rx {
                    // 尝试读取所有可用数据
                    while let Ok(data) = output_rx.try_recv() {
                        has_activity = true;
                        received_data.push(data);
                    }
                }

                // 更新缓冲区（始终缓存，以支持重连）
                for data in &received_data {
                    session.output_buffer.append(data);
                }

                // 如果会话不是 Active 状态，则跳过发送响应。
                // 数据已缓存在上面，下次重连时会发送。
                // 使用作用域块来限制对 session.state 的可变借用，
                // 以便之后能不可变地借用会话的其他字段。
                let (replay_buffer, sigwinch_action) = {
                    let (pending_buffer, sigwinch) = match &mut session.state {
                        SessionState::Active {
                            pending_buffer,
                            sigwinch,
                        } => (pending_buffer, sigwinch),
                        _ => continue,
                    };

                    let replay_buffer = pending_buffer.take();

                    // 两阶段 SIGWINCH：原因参见 SigwinchPhase 的文档注释。
                    // 每个阶段都是一次 PTY 尺寸调整，由轮询
                    // 间隔拉开约 30ms，确保 TUI 程序在每次信号时都能察觉到真实的尺寸变化。
                    let sigwinch_action = match sigwinch {
                        SigwinchPhase::TempResize { retries } => {
                            if *retries == 0 {
                                log::warn!(
                                    "Terminal {} SIGWINCH phase 1 (temp resize) failed after {} attempts, giving up",
                                    terminal_id, MAX_SIGWINCH_PHASE_ATTEMPTS
                                );
                                * sigwinch = SigwinchPhase::Idle;
                                None
                            } else {
                                * retries -= 1;
                                Some(SigwinchAction::TempResize)
                            }
                        }
                        SigwinchPhase::Restore { retries } => {
                            if *retries == 0 {
                                log::warn!(
                                    "Terminal {} SIGWINCH phase 2 (restore) failed after {} attempts, giving up",
                                    terminal_id, MAX_SIGWINCH_PHASE_ATTEMPTS
                                );
                                * sigwinch = SigwinchPhase::Idle;
                                None
                            } else {
                                * retries -= 1;
                                Some(SigwinchAction::Restore)
                            }
                        }
                        SigwinchPhase::Idle => None,
                    };
                    (replay_buffer, sigwinch_action)
                };

                if let Some(buffer) = replay_buffer {
                    if !buffer.is_empty() {
                        responses.push(Self::create_terminal_data_response(terminal_id, buffer));
                    }
                }

                if has_activity {
                    session.update_activity();
                }

                // 在 session.state 的可变借用作用域之外执行 SIGWINCH 尺寸调整。
                if let Some(action) = sigwinch_action {
                    #[cfg(target_os = "windows")]
                    let is_helper = session.is_helper_mode;
                    #[cfg(not(target_os = "windows"))]
                    let is_helper = false;
                    let resize_ok = Self::do_sigwinch_resize(
                        terminal_id,
                        session.rows,
                        session.cols,
                        &session.pty_pair,
                        &session.input_tx,
                        is_helper,
                        &action,
                    );
                    if let SessionState::Active { sigwinch, .. } = &mut session.state {
                        match action {
                            SigwinchAction::TempResize => {
                                if resize_ok {
                                    // 阶段 1 成功 —— 进入阶段 2（恢复）。
                                    * sigwinch = SigwinchPhase::Restore {
                                        retries: MAX_SIGWINCH_PHASE_ATTEMPTS,
                                    };
                                }
                                // 如果失败，retries 已递减；将重试阶段 1。
                            }
                            SigwinchAction::Restore => {
                                if resize_ok {
                                    // 阶段 2 成功 —— SIGWINCH 序列完成。
                                    * sigwinch = SigwinchPhase::Idle;
                                }
                                // 如果失败，retries 已递减；将重试阶段 2。
                            }
                        }
                    }
                }

                // 在历史缓冲区之后发送实时数据
                for data in received_data {
                    responses.push(Self::create_terminal_data_response(terminal_id, data));
                }
            }
        }

        // 清理已关闭的终端（需短暂持有服务锁）
        if !closed_terminals.is_empty() {
            let mut sessions = service.lock().unwrap().sessions.clone();
            for terminal_id in closed_terminals {
                let mut exit_code = 0;

                if !self.is_persistent {
                    if let Some(session_arc) = sessions.remove(&terminal_id) {
                        service.lock().unwrap().sessions.remove(&terminal_id);
                        let mut session = session_arc.lock().unwrap();
                        // 取出 child 并加入僵尸进程回收器
                        if let Some(mut child) = session.child.take() {
                            // 如果可用则尝试获取退出码
                            if let Ok(Some(status)) = child.try_wait() {
                                exit_code = status.exit_code() as i32;
                            }
                            add_to_reaper(child);
                        }
                    }
                } else {
                    // 对于持久化会话，清除 child 引用并移除该会话
                    // （前提是 closed 消息已发送，即 shell 已退出）。
                    if let Some(session_arc) = sessions.get(&terminal_id) {
                        let mut session = session_arc.lock().unwrap();
                        if let Some(mut child) = session.child.take() {
                            // 如果可用则尝试获取退出码
                            if let Ok(Some(status)) = child.try_wait() {
                                exit_code = status.exit_code() as i32;
                            }
                            add_to_reaper(child);
                        }
                        if session.closed_message_sent {
                            // shell 已退出，移除该失效会话
                            drop(session);
                            sessions.remove(&terminal_id);
                            service.lock().unwrap().sessions.remove(&terminal_id);
                        }
                    }
                }

                let mut response = TerminalResponse::new();
                let mut closed = TerminalClosed::new();
                closed.terminal_id = terminal_id;
                closed.exit_code = exit_code;
                response.set_closed(closed);
                responses.push(response);
            }
        }

        responses
    }

    /// 连接断开时的清理
    pub fn on_disconnect(&self) {
        if !self.is_persistent {
            // 移除非持久化服务
            remove_service(&self.service_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputBuffer, Utf8ChunkAccumulator, MAX_BUFFER_LINES};

    #[test]
    fn utf8_chunk_accumulator_buffers_leading_split_multibyte_output() {
        let mut chunker = Utf8ChunkAccumulator::default();

        assert!(chunker.push_chunk(vec![0xE4]).is_none());
        assert!(chunker.push_chunk(vec![0xB8]).is_none());
        assert_eq!(
            chunker.push_chunk(vec![0xAD]),
            Some("中".as_bytes().to_vec())
        );
        assert!(chunker.finish().is_none());
    }

    #[test]
    fn utf8_chunk_accumulator_flushes_incomplete_tail_on_finish() {
        let mut chunker = Utf8ChunkAccumulator::default();
        assert_eq!(chunker.push_chunk(vec![b'a', 0xE4]), Some(vec![b'a']));
        assert_eq!(chunker.finish(), Some(vec![0xE4]));
        assert!(chunker.finish().is_none());
    }

    #[test]
    fn output_buffer_trim_after_incomplete_merge_does_not_underflow() {
        let mut buffer = OutputBuffer::new();

        // 先创建一个不完整的行。
        buffer.append(b"hello");

        // 合并一个尾部包含首个换行符的大块数据。
        // 这会触发「追加到最后一行不完整内容」的分支。
        let mut large = vec![b'a'; 30_000];
        large.push(b'\n');
        buffer.append(&large);

        // 超出 MAX_BUFFER_LINES，使裁剪操作弹出第一行合并后的大行。
        for _ in 0..=MAX_BUFFER_LINES {
            buffer.append(b"x\n");
        }

        let actual_size: usize = buffer.lines.iter().map(|line| line.len()).sum();
        assert_eq!(buffer.total_size, actual_size);
    }

    #[test]
    fn output_buffer_trims_output_without_newlines() {
        let mut buffer = OutputBuffer::new();

        let chunk = vec![b'a'; 4096];
        for _ in 0..(super::MAX_OUTPUT_BUFFER_SIZE / chunk.len()) {
            buffer.append(&chunk);
        }
        buffer.append(b"TAIL");

        assert!(buffer.get_recent(usize::MAX).len() <= super::MAX_OUTPUT_BUFFER_SIZE);
        assert_eq!(buffer.get_recent(4), b"TAIL");
    }
}
