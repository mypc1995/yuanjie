#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::ipc::Connection;
#[cfg(not(any(target_os = "ios")))]
use crate::ipc::{self, Data};
#[cfg(target_os = "windows")]
use crate::{clipboard::ClipboardSide, ipc::ClipboardNonFile};
#[cfg(target_os = "windows")]
use base::config::keys::*;
#[cfg(not(any(target_os = "ios")))]
use base::fs::serialize_transfer_job;
use base::{
    config::keys::{OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW, OPTION_FILE_TRANSFER_MAX_FILES},
    fs::{self, get_string, is_write_need_confirmation, new_send_confirm, DigestCheckResult},
    message_proto::*,
};
#[cfg(target_os = "windows")]
use clipboard::ContextSend;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::tokio::sync::mpsc::unbounded_channel;
#[cfg(target_os = "windows")]
use hbb_common::tokio::sync::Mutex as TokioMutex;
use hbb_common::{
    allow_err, bail,
    config::{option2bool, Config},
    log,
    protobuf::Message as _,
    tokio::{
        self,
        sync::mpsc::{self, UnboundedSender},
        task::spawn_blocking,
    },
    ResultType,
};
use serde_derive::Serialize;
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
use std::iter::FromIterator;
#[cfg(not(any(target_os = "ios")))]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::sync::Arc;
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicI64, Ordering},
        RwLock,
    },
};

/// 每次传输请求允许的最大文件数（默认值）。
/// 单位：文件数（不是字节数）。
#[cfg(not(any(target_os = "ios")))]
const DEFAULT_MAX_VALIDATED_FILES: usize = 10_000;

/// 单次文件传输请求中允许的最大文件数。
///
/// 该限制用于避免处理
/// 大目录时产生过多 I/O 和内存占用。它适用于：
/// - CM 侧的读取任务（Windows 上服务器到客户端的文件传输）
/// - `AllFiles` 递归目录列举操作
/// - Connection 侧的读取任务（非 Windows 平台）
///
/// 单位：文件数（不是字节数）。
/// 默认：10,000 个文件。
/// 配置方式：`OPTION_FILE_TRANSFER_MAX_FILES`（"file-transfer-max-files"）
#[cfg(not(any(target_os = "ios")))]
static MAX_VALIDATED_FILES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// 获取每次传输请求允许的最大文件数。
///
/// 首次调用时从配置（`OPTION_FILE_TRANSFER_MAX_FILES`）
/// 中初始化该值。语义如下：
/// - 如果该选项被设为 `0`，则使用 `DEFAULT_MAX_VALIDATED_FILES`（10,000）作为安全上限。
/// - 如果该选项未设置、为负数或不是整数，
///   则使用 `usize::MAX` 表示“不限制”，以保持与更早那些
///   未强制任何文件数限制的版本的向后兼容。
///   （注意：负数对 `usize` 来说是非法的，会导致解析失败。）
///
/// 单位：文件数。
#[cfg(not(any(target_os = "ios")))]
#[inline]
pub fn get_max_validated_files() -> usize {
    // 如果 `OPTION_FILE_TRANSFER_MAX_FILES` 未设置、为负数或不是整数，就用
    // `usize::MAX` 表示“不限制”，以保持与那些
    // 没有文件传输限制的版本的向后兼容。
    const NO_LIMIT_FILE_COUNT: usize = usize::MAX;
    * MAX_VALIDATED_FILES.get_or_init(|| {
        let c = crate::get_builtin_option(OPTION_FILE_TRANSFER_MAX_FILES)
            .trim()
            .parse::<usize>()
            .unwrap_or(NO_LIMIT_FILE_COUNT);
        if c == 0 {
            DEFAULT_MAX_VALIDATED_FILES
        } else {
            c
        }
    })
}

/// 检查文件数是否超过允许的上限。
///
/// 该检查在以下位置执行：
/// - `start_read_job()`，用于 CM 侧的读取任务
/// - `read_all_files()`，用于递归目录列举
/// - `Connection::on_message()`，用于 Connection 侧的读取任务
///
/// # 参数
/// * `file_count` - 传输请求中的文件数
///
/// # 返回值
/// * 在限制内时返回 `Ok(())`
/// * 超出限制时返回带错误信息的 `Err(String)`
#[cfg(not(any(target_os = "ios")))]
pub fn check_file_count_limit(file_count: usize) -> Result<(), String> {
    let max_files = get_max_validated_files();
    if file_count > max_files {
        let msg = format!(
            "file transfer rejected: too many files ({} files exceeds limit of {}). \
             Adjust '{}' option to increase limit.",
            file_count, max_files, OPTION_FILE_TRANSFER_MAX_FILES
        );
        log::warn!("{}", msg);
        Err(msg)
    } else {
        Ok(())
    }
}

#[derive(Serialize, Clone)]
pub struct Client {
    pub id: i32,
    pub authorized: bool,
    pub disconnected: bool,
    pub is_file_transfer: bool,
    pub is_view_camera: bool,
    pub is_terminal: bool,
    pub port_forward: String,
    pub name: String,
    pub avatar: String,
    pub peer_id: String,
    pub keyboard: bool,
    pub clipboard: bool,
    pub audio: bool,
    pub file: bool,
    pub restart: bool,
    pub recording: bool,
    pub block_input: bool,
    pub privacy_mode: bool,
    pub from_switch: bool,
    pub in_voice_call: bool,
    pub incoming_voice_call: bool,
    #[serde(skip)]
    #[cfg(not(any(target_os = "ios")))]
    tx: UnboundedSender<Data>,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct IpcTaskRunner<T: InvokeUiCM> {
    stream: Connection,
    cm: ConnectionManager<T>,
    tx: mpsc::UnboundedSender<Data>,
    rx: mpsc::UnboundedReceiver<Data>,
    close: bool,
    running: bool,
    conn_id: i32,
    #[cfg(target_os = "windows")]
    file_transfer_enabled: bool,
    #[cfg(target_os = "windows")]
    file_transfer_enabled_peer: bool,
    /// 用于 CM 侧文件读取（服务器到客户端传输）的读取任务
    read_jobs: Vec<fs::TransferJob>,
}

lazy_static::lazy_static! {
    static ref CLIENTS: RwLock<HashMap<i32, Client>> = Default::default();
}

static CLICK_TIME: AtomicI64 = AtomicI64::new(0);

#[derive(Clone)]
pub struct ConnectionManager<T: InvokeUiCM> {
    pub ui_handler: T,
}

pub trait InvokeUiCM: Send + Clone + 'static + Sized {
    fn add_connection(&self, client: &Client);

    fn remove_connection(&self, id: i32, close: bool);

    fn new_message(&self, id: i32, text: String);

    fn change_theme(&self, dark: String);

    fn change_language(&self);

    fn show_elevation(&self, show: bool);

    fn update_voice_call_state(&self, client: &Client);

    fn file_transfer_log(&self, action: &str, log: &str);
}

impl<T: InvokeUiCM> Deref for ConnectionManager<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.ui_handler
    }
}

impl<T: InvokeUiCM> DerefMut for ConnectionManager<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ui_handler
    }
}

impl<T: InvokeUiCM> ConnectionManager<T> {
    fn add_connection(
        &self,
        id: i32,
        is_file_transfer: bool,
        is_view_camera: bool,
        is_terminal: bool,
        port_forward: String,
        peer_id: String,
        name: String,
        avatar: String,
        authorized: bool,
        keyboard: bool,
        clipboard: bool,
        audio: bool,
        file: bool,
        restart: bool,
        recording: bool,
        block_input: bool,
        privacy_mode: bool,
        from_switch: bool,
        #[cfg(not(any(target_os = "ios")))] tx: mpsc::UnboundedSender<Data>,
    ) {
        let client = Client {
            id,
            authorized,
            disconnected: false,
            is_file_transfer,
            is_view_camera,
            is_terminal,
            port_forward,
            name: name.clone(),
            avatar,
            peer_id: peer_id.clone(),
            keyboard,
            clipboard,
            audio,
            file,
            restart,
            recording,
            block_input,
            privacy_mode,
            from_switch,
            #[cfg(not(any(target_os = "ios")))]
            tx,
            in_voice_call: false,
            incoming_voice_call: false,
        };
        CLIENTS
            .write()
            .unwrap()
            .retain(|_, c| !(c.disconnected && c.peer_id == client.peer_id));
        CLIENTS.write().unwrap().insert(id, client.clone());
        self.ui_handler.add_connection(&client);
    }

    #[inline]
    #[cfg(target_os = "windows")]
    fn is_authorized(&self, id: i32) -> bool {
        CLIENTS
            .read()
            .unwrap()
            .get(&id)
            .map(|c| c.authorized)
            .unwrap_or(false)
    }

    fn remove_connection(&self, id: i32, close: bool) {
        if close {
            CLIENTS.write().unwrap().remove(&id);
        } else {
            CLIENTS
                .write()
                .unwrap()
                .get_mut(&id)
                .map(|c| c.disconnected = true);
        }

        #[cfg(target_os = "windows")]
        {
            crate::clipboard::try_empty_clipboard_files(ClipboardSide::Host, id);
        }

        #[cfg(any(target_os = "android"))]
        if CLIENTS
            .read()
            .unwrap()
            .iter()
            .filter(|(_k, v)| !v.is_file_transfer && !v.is_terminal)
            .next()
            .is_none()
        {
            if let Err(e) =
                scrap::android::call_main_service_set_by_name("stop_capture", None, None)
            {
                log::debug!("stop_capture err:{}", e);
            }
        }

        self.ui_handler.remove_connection(id, close);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn show_elevation(&self, show: bool) {
        self.ui_handler.show_elevation(show);
    }

    #[cfg(not(target_os = "ios"))]
    fn voice_call_started(&self, id: i32) {
        if let Some(client) = CLIENTS.write().unwrap().get_mut(&id) {
            client.incoming_voice_call = false;
            client.in_voice_call = true;
            self.ui_handler.update_voice_call_state(client);
        }
    }

    #[cfg(not(target_os = "ios"))]
    fn voice_call_incoming(&self, id: i32) {
        if let Some(client) = CLIENTS.write().unwrap().get_mut(&id) {
            client.incoming_voice_call = true;
            client.in_voice_call = false;
            self.ui_handler.update_voice_call_state(client);
        }
    }

    #[cfg(not(target_os = "ios"))]
    fn voice_call_closed(&self, id: i32, _reason: &str) {
        if let Some(client) = CLIENTS.write().unwrap().get_mut(&id) {
            client.incoming_voice_call = false;
            client.in_voice_call = false;
            self.ui_handler.update_voice_call_state(client);
        }
    }
}

#[inline]
#[cfg(not(any(target_os = "ios")))]
pub fn check_click_time(id: i32) {
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        allow_err!(client.tx.send(Data::ClickTime(0)));
    };
}

#[inline]
pub fn get_click_time() -> i64 {
    CLICK_TIME.load(Ordering::SeqCst)
}

#[inline]
#[cfg(not(any(target_os = "ios")))]
pub fn authorize(id: i32) {
    if let Some(client) = CLIENTS.write().unwrap().get_mut(&id) {
        client.authorized = true;
        allow_err!(client.tx.send(Data::Authorize));
    };
}

#[inline]
#[cfg(not(any(target_os = "ios")))]
pub fn close(id: i32) {
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        allow_err!(client.tx.send(Data::Close));
    };
}

/// 与 `close` 类似，但表示是 CM 的窗口被关闭，而不是有人断开了这个对端。
/// 参见 `ipc::Data::CmWindowClosed`。
#[cfg(target_os = "linux")]
pub fn close_window(id: i32) {
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        allow_err!(client.tx.send(Data::CmWindowClosed));
    };
}

#[inline]
pub fn remove(id: i32) {
    CLIENTS.write().unwrap().remove(&id);
}

// 服务端模式向对端发送聊天消息
#[inline]
#[cfg(not(any(target_os = "ios")))]
pub fn send_chat(id: i32, text: String) {
    let clients = CLIENTS.read().unwrap();
    if let Some(client) = clients.get(&id) {
        allow_err!(client.tx.send(Data::ChatMessage { text }));
    }
}

#[inline]
#[cfg(not(any(target_os = "ios")))]
pub fn switch_permission(id: i32, name: String, enabled: bool) {
    #[cfg(target_os = "android")]
    let is_keyboard_permission = name == "keyboard";
    #[cfg(not(target_os = "android"))]
    let is_keyboard_permission = false;
    if !option2bool(
        OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW,
        &crate::get_builtin_option(OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW),
    ) && !is_keyboard_permission
    {
        log::info!(
            "blocked cm switch_permission by policy, conn_id={}, permission={}, enabled={}",
            id,
            name,
            enabled
        );
        return;
    }
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        allow_err!(client.tx.send(Data::SwitchPermission { name, enabled }));
    };
}

#[inline]
#[cfg(target_os = "android")]
pub fn switch_permission_all(name: String, enabled: bool) {
    if name != "keyboard"
        && !option2bool(
            OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW,
            &crate::get_builtin_option(OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW),
        )
    {
        log::info!(
            "blocked cm switch_permission_all by policy, permission={}, enabled={}",
            name,
            enabled
        );
        return;
    }
    for (_, client) in CLIENTS.read().unwrap().iter() {
        allow_err!(client.tx.send(Data::SwitchPermission {
            name: name.clone(),
            enabled
        }));
    }
}

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
#[inline]
pub fn get_clients_state() -> String {
    let clients = CLIENTS.read().unwrap();
    let res = Vec::from_iter(clients.values().cloned());
    serde_json::to_string(&res).unwrap_or("".into())
}

#[inline]
pub fn get_clients_length() -> usize {
    let clients = CLIENTS.read().unwrap();
    clients.len()
}

#[inline]
#[cfg(target_os = "android")]
pub fn has_active_clients() -> bool {
    let clients = CLIENTS.read().unwrap();
    clients.values().any(|c| !c.disconnected)
}

#[inline]
#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn switch_back(id: i32) {
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        allow_err!(client.tx.send(Data::SwitchSidesBack));
    };
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl<T: InvokeUiCM> IpcTaskRunner<T> {
    async fn run(&mut self) {
        use hbb_common::config::LocalConfig;
        use hbb_common::tokio::time::{self, Duration, Instant};

        const MILLI5: Duration = Duration::from_millis(5);
        const SEC30: Duration = Duration::from_secs(30);

        // 临时使用，没有真实的连接 id
        let mut write_jobs: Vec<fs::TransferJob> = Vec::new();
        // 用于处理 read_jobs 的文件定时器
        let mut file_timer =
            crate::rustdesk_interval(time::interval_at(Instant::now() + SEC30, SEC30));

        #[cfg(target_os = "windows")]
        let is_authorized = self.cm.is_authorized(self.conn_id);

        #[cfg(target_os = "windows")]
        let rx_clip_holder;
        let mut rx_clip;
        let _tx_clip;
        #[cfg(target_os = "windows")]
        if self.conn_id > 0 && is_authorized {
            log::debug!("Clipboard is enabled from client peer: type 1");
            let conn_id = self.conn_id;
            rx_clip_holder = (
                clipboard::get_rx_cliprdr_server(conn_id),
                Some(crate::SimpleCallOnReturn {
                    b: true,
                    f: Box::new(move || {
                        clipboard::remove_channel_by_conn_id(conn_id);
                    }),
                }),
            );
            rx_clip = rx_clip_holder.0.lock().await;
        } else {
            log::debug!("Clipboard is enabled from client peer, actually useless: type 2");
            let rx_clip2;
            (_tx_clip, rx_clip2) = unbounded_channel::<clipboard::ClipboardFile>();
            rx_clip_holder = (Arc::new(TokioMutex::new(rx_clip2)), None);
            rx_clip = rx_clip_holder.0.lock().await;
        }
        #[cfg(not(target_os = "windows"))]
        {
            (_tx_clip, rx_clip) = unbounded_channel::<i32>();
        }

        #[cfg(target_os = "windows")]
        {
            if ContextSend::is_enabled() {
                log::debug!("Clipboard is enabled");
                allow_err!(
                    self.stream
                        .send(&Data::ClipboardFile(clipboard::ClipboardFile::MonitorReady))
                        .await
                );
            }
        }
        let (tx_log, mut rx_log) = mpsc::unbounded_channel::<String>();

        self.running = false;
        loop {
            tokio::select! {
                res = self.stream.next() => {
                    match res {
                        Err(err) => {
                            log::info!("cm ipc connection closed: {}", err);
                            break;
                        }
                        Ok(Some(data)) => {
                            match data {
                                Data::Login{id, is_file_transfer, is_view_camera, is_terminal, port_forward, peer_id, name, avatar, authorized, keyboard, clipboard, audio, file, file_transfer_enabled: _file_transfer_enabled, restart, recording, block_input, privacy_mode, from_switch} => {
                                    log::debug!("conn_id: {}", id);
                                    self.cm.add_connection(id, is_file_transfer, is_view_camera, is_terminal, port_forward, peer_id, name, avatar, authorized, keyboard, clipboard, audio, file, restart, recording, block_input, privacy_mode, from_switch, self.tx.clone());
                                    self.conn_id = id;
                                    #[cfg(target_os = "windows")]
                                    {
                                        self.file_transfer_enabled = _file_transfer_enabled;
                                    }
                                    self.running = true;
                                    break;
                                }
                                Data::Close => {
                                    log::info!("cm ipc connection closed from connection request");
                                    break;
                                }
                                Data::Disconnected => {
                                    self.close = false;
                                    log::info!("cm ipc connection disconnect");
                                    break;
                                }
                                Data::PrivacyModeState((_id, _, _)) => {
                                    #[cfg(windows)]
                                    cm_inner_send(_id, data);
                                }
                                Data::ClickTime(ms) => {
                                    CLICK_TIME.store(ms, Ordering::SeqCst);
                                }
                                Data::ChatMessage { text } => {
                                    self.cm.new_message(self.conn_id, text);
                                }
                                Data::SwitchPermission { name, enabled } => {
                                    // 把这个分支限定在隐私模式回滚上。
                                    // 其他 CM 权限开关由 UI 自行乐观更新。
                                    // 后端目前只在
                                    // 关闭隐私模式失败、需要恢复 UI 状态时，才把 SwitchPermission 发回给 CM。
                                    if name == "privacy_mode" {
                                        let client = {
                                            let mut clients = CLIENTS.write().unwrap();
                                            clients.get_mut(&self.conn_id).map(|c| {
                                                c.privacy_mode = enabled;
                                                c.clone()
                                            })
                                        };
                                        if let Some(client) = client {
                                            // 这复用了 add_connection()，而 cm.tis 只会选择性地更新
                                            // 这条回退路径上的已有行（authorized/privacy_mode）。
                                            self.cm.ui_handler.add_connection(&client);
                                        }
                                    }
                                }
                                Data::FS(mut fs) => {
                                    if let ipc::FS::WriteBlock { id, file_num, data: _, compressed } = fs {
                                        if let Ok(bytes) = self.stream.next_raw().await {
                                            fs = ipc::FS::WriteBlock{id, file_num, data:bytes.into(), compressed};
                                            handle_fs(fs, &mut write_jobs, &mut self.read_jobs, &self.tx, Some(&tx_log), self.conn_id).await;
                                        }
                                    } else {
                                        handle_fs(fs, &mut write_jobs, &mut self.read_jobs, &self.tx, Some(&tx_log), self.conn_id).await;
                                    }
                                    // 存在读取任务时立即启用快速定时器。
                                    // 这样新任务无需等待慢速的 30s 定时器就开始处理。
                                    // 任务耗尽时在 tick 处理函数中停用（恢复为 30s）。
                                    if !self.read_jobs.is_empty() {
                                        file_timer = crate::rustdesk_interval(time::interval(MILLI5));
                                    }
                                    let log = fs::serialize_transfer_jobs(&write_jobs);
                                    self.cm.ui_handler.file_transfer_log("transfer", &log);
                                }
                                Data::FileTransferLog((action, log)) => {
                                    self.cm.ui_handler.file_transfer_log(&action, &log);
                                }
                                #[cfg(target_os = "windows")]
                                Data::ClipboardFile(_clip) => {
                                    let is_stopping_allowed = _clip.is_beginning_message();
                                    let is_clipboard_enabled = ContextSend::is_enabled();
                                    let file_transfer_enabled = self.file_transfer_enabled;
                                    let stop = !is_stopping_allowed && !(is_clipboard_enabled && file_transfer_enabled);
                                    log::debug!(
                                        "Process clipboard message from client peer, stop: {}, is_stopping_allowed: {}, is_clipboard_enabled: {}, file_transfer_enabled: {}",
                                        stop, is_stopping_allowed, is_clipboard_enabled, file_transfer_enabled);
                                    if stop {
                                        ContextSend::set_is_stopped();
                                    } else {
                                        if !is_authorized {
                                            log::debug!("Clipboard message from client peer, but not authorized");
                                            continue;
                                        }
                                        let conn_id = self.conn_id;
                                        let _ = ContextSend::proc(|context| -> ResultType<()> {
                                            context.server_clip_file(conn_id, _clip)
                                                .map_err(|e| e.into())
                                        });
                                    }
                                }
                                Data::ClipboardFileEnabled(_enabled) => {
                                    #[cfg(target_os = "windows")]
                                    {
                                        self.file_transfer_enabled_peer = _enabled;
                                    }
                                }
                                Data::Theme(dark) => {
                                    self.cm.change_theme(dark);
                                }
                                Data::Language(lang) => {
                                    LocalConfig::set_option("lang".to_owned(), lang);
                                    self.cm.change_language();
                                }
                                Data::DataPortableService(ipc::DataPortableService::CmShowElevation(show)) => {
                                    self.cm.show_elevation(show);
                                }
                                Data::StartVoiceCall => {
                                    self.cm.voice_call_started(self.conn_id);
                                }
                                Data::VoiceCallIncoming => {
                                    self.cm.voice_call_incoming(self.conn_id);
                                }
                                Data::CloseVoiceCall(reason) => {
                                    self.cm.voice_call_closed(self.conn_id, reason.as_str());
                                }
                                #[cfg(target_os = "windows")]
                                Data::ClipboardNonFile(_) => {
                                    match crate::clipboard::check_clipboard_cm() {
                                        Ok(multi_clipoards) => {
                                            let mut raw_contents = bytes::BytesMut::new();
                                            let mut main_data = vec![];
                                            for c in multi_clipoards.clipboards.into_iter() {
                                                let content_len = c.content.len();
                                                let (content, next_raw) = {
                                                    // TODO: find out a better threshold
                                                    if content_len > 1024 * 3 {
                                                        raw_contents.extend(c.content);
                                                        (bytes::Bytes::new(), true)
                                                    } else {
                                                        (c.content, false)
                                                    }
                                                };
                                                main_data.push(ClipboardNonFile {
                                                    compress: c.compress,
                                                    content,
                                                    content_len,
                                                    next_raw,
                                                    width: c.width,
                                                    height: c.height,
                                                    format: c.format.value(),
                                                    special_name: c.special_name,
                                                });
                                            }
                                            allow_err!(self.stream.send(&Data::ClipboardNonFile(Some(("".to_owned(), main_data)))).await);
                                            if !raw_contents.is_empty() {
                                                allow_err!(self.stream.send_raw(raw_contents.into()).await);
                                            }
                                        }
                                        Err(e) => {
                                            log::debug!("Failed to get clipboard content. {}", e);
                                            allow_err!(self.stream.send(&Data::ClipboardNonFile(Some((format!("{}", e), vec![])))).await);
                                        }
                                    }
                                }
                                _ => {

                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some(data) = self.rx.recv() => {
                    // 对 FileBlockFromCM，数据通过 send_raw 单独发送（data 字段带有 #[serde(skip)]）。
                    // 这样可以避免对大块二进制数据做 JSON 编码的开销。
                    // 这与 start_ipc 中的 WriteBlock 模式一致（参见 rx_to_cm 处理函数）。
                    //
                    // 注意：空数据（空文件的情形）已被正确处理。raw=false 的 BytesCodec
                    // （IPC 连接的默认值）会加上长度前缀，所以 send_raw(Bytes::new())
                    // 发出的是 1 字节的帧，next_raw() 能正确地将其作为空数据接收。
                    if let Data::FileBlockFromCM { id, file_num, ref data, compressed, conn_id } = data {
                        // 先发送元数据（data 字段被 serde 跳过），再发送原始数据字节
                        if let Err(e) = self.stream.send(&Data::FileBlockFromCM {
                            id,
                            file_num,
                            data: bytes::Bytes::new(), // placeholder, skipped by serde
                            compressed,
                            conn_id,
                        }).await {
                            log::error!("error sending FileBlockFromCM metadata: {}", e);
                            break;
                        }
                        if let Err(e) = self.stream.send_raw(data.clone()).await {
                            log::error!("error sending FileBlockFromCM data: {}", e);
                            break;
                        }
                        continue;
                    }
                    if let Err(e) = self.stream.send(&data).await {
                        log::error!("error encountered in IPC task, quitting: {}", e);
                        break;
                    }
                    match &data {
                        Data::SwitchPermission{name: _name, enabled: _enabled} => {
                            #[cfg(target_os = "windows")]
                            if _name == "file" {
                                self.file_transfer_enabled = *_enabled;
                            }
                        }
                        Data::Authorize => {
                            self.running = true;
                            break;
                        }
                        _ => {
                        }
                    }
                },
                clip_file = rx_clip.recv() => match clip_file {
                    Some(_clip) => {
                        #[cfg(target_os = "windows")]
                        {
                            let is_stopping_allowed = _clip.is_stopping_allowed();
                            let is_clipboard_enabled = ContextSend::is_enabled();
                            let file_transfer_enabled = self.file_transfer_enabled;
                            let file_transfer_enabled_peer = self.file_transfer_enabled_peer;
                            let stop = is_stopping_allowed && !(is_clipboard_enabled && file_transfer_enabled && file_transfer_enabled_peer);
                            log::debug!(
                                "Process clipboard message from clip, stop: {}, is_stopping_allowed: {}, is_clipboard_enabled: {}, file_transfer_enabled: {}, file_transfer_enabled_peer: {}",
                                stop, is_stopping_allowed, is_clipboard_enabled, file_transfer_enabled, file_transfer_enabled_peer);
                            if stop {
                                ContextSend::set_is_stopped();
                            } else {
                                if _clip.is_beginning_message() && crate::get_builtin_option(OPTION_ONE_WAY_FILE_TRANSFER) == "Y" {
                                    // 如果启用了单向文件传输，就不要向客户端发送剪贴板文件
                                    // 不要调用 `ContextSend::set_is_stopped()`，因为它会停止双向文件复制粘贴。
                                } else {
                                    allow_err!(self.tx.send(Data::ClipboardFile(_clip)));
                                }
                            }
                        }
                    }
                    None => {
                        //
                    }
                },
                Some(job_log) = rx_log.recv() => {
                    self.cm.ui_handler.file_transfer_log("transfer", &job_log);
                }
                _ = file_timer.tick() => {
                    if !self.read_jobs.is_empty() {
                        let conn_id = self.conn_id;
                        if let Err(e) = handle_read_jobs_tick(&mut self.read_jobs, &self.tx, conn_id).await {
                            log::error!("Error processing read jobs: {}", e);
                        }
                        let log = fs::serialize_transfer_jobs(&self.read_jobs);
                        self.cm.ui_handler.file_transfer_log("transfer", &log);
                    } else {
                        file_timer = crate::rustdesk_interval(time::interval_at(Instant::now() + SEC30, SEC30));
                    }
                }
            }
        }
    }

    async fn ipc_task(stream: Connection, cm: ConnectionManager<T>) {
        log::debug!("ipc task begin");
        let (tx, rx) = mpsc::unbounded_channel::<Data>();
        let mut task_runner = Self {
            stream,
            cm,
            tx,
            rx,
            close: true,
            running: true,
            conn_id: 0,
            #[cfg(target_os = "windows")]
            file_transfer_enabled: false,
            #[cfg(target_os = "windows")]
            file_transfer_enabled_peer: false,
            read_jobs: Vec::new(),
        };

        while task_runner.running {
            task_runner.run().await;
        }
        if task_runner.conn_id > 0 {
            task_runner
                .cm
                .remove_connection(task_runner.conn_id, task_runner.close);
        }
        log::debug!("ipc task end");
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tokio::main(flavor = "current_thread")]
pub async fn start_ipc<T: InvokeUiCM>(cm: ConnectionManager<T>) {
    #[cfg(target_os = "windows")]
    {
        let enabled = crate::Connection::is_permission_enabled_locally(OPTION_ENABLE_FILE_TRANSFER);
        let mut lock = crate::ui_interface::IS_FILE_TRANSFER_ENABLED
            .lock()
            .unwrap();
        ContextSend::enable(enabled);
        * lock = Some(enabled);
    }
    match ipc::new_listener("_cm").await {
        Ok(mut incoming) => {
            while let Some(result) = incoming.next().await {
                match result {
                    Ok(stream) => {
                        log::debug!("Got new connection");
                        tokio::spawn(IpcTaskRunner::<T>::ipc_task(
                            Connection::new(stream),
                            cm.clone(),
                        ));
                    }
                    Err(err) => {
                        log::error!("Couldn't get cm client: {:?}", err);
                    }
                }
            }
        }
        Err(err) => {
            log::error!("Failed to start cm ipc server: {}", err);
        }
    }
    quit_cm();
}

#[cfg(target_os = "android")]
#[tokio::main(flavor = "current_thread")]
pub async fn start_listen<T: InvokeUiCM>(
    cm: ConnectionManager<T>,
    mut rx: mpsc::UnboundedReceiver<Data>,
    tx: mpsc::UnboundedSender<Data>,
) {
    let mut current_id = 0;
    let mut write_jobs: Vec<fs::TransferJob> = Vec::new();
    loop {
        match rx.recv().await {
            Some(Data::Login {
                id,
                is_file_transfer,
                is_view_camera,
                is_terminal,
                port_forward,
                peer_id,
                name,
                avatar,
                authorized,
                keyboard,
                clipboard,
                audio,
                file,
                restart,
                recording,
                block_input,
                privacy_mode,
                from_switch,
                ..
            }) => {
                current_id = id;
                cm.add_connection(
                    id,
                    is_file_transfer,
                    is_view_camera,
                    is_terminal,
                    port_forward,
                    peer_id,
                    name,
                    avatar,
                    authorized,
                    keyboard,
                    clipboard,
                    audio,
                    file,
                    restart,
                    recording,
                    block_input,
                    privacy_mode,
                    from_switch,
                    tx.clone(),
                );
            }
            Some(Data::ChatMessage { text }) => {
                cm.new_message(current_id, text);
            }
            Some(Data::FS(fs)) => {
                // Android 不需要 CM 侧的文件读取（没有 need_validate_file_read_access）
                let mut read_jobs_placeholder: Vec<fs::TransferJob> = Vec::new();
                handle_fs(
                    fs,
                    &mut write_jobs,
                    &mut read_jobs_placeholder,
                    &tx,
                    None,
                    current_id,
                )
                .await;
            }
            Some(Data::Close) => {
                break;
            }
            Some(Data::StartVoiceCall) => {
                cm.voice_call_started(current_id);
            }
            Some(Data::VoiceCallIncoming) => {
                cm.voice_call_incoming(current_id);
            }
            Some(Data::CloseVoiceCall(reason)) => {
                cm.voice_call_closed(current_id, reason.as_str());
            }
            None => {
                break;
            }
            _ => {}
        }
    }
    cm.remove_connection(current_id, true);
}

#[cfg(not(any(target_os = "ios")))]
async fn handle_fs(
    fs: ipc::FS,
    write_jobs: &mut Vec<fs::TransferJob>,
    read_jobs: &mut Vec<fs::TransferJob>,
    tx: &UnboundedSender<Data>,
    tx_log: Option<&UnboundedSender<String>>,
    _conn_id: i32,
) {
    // Android 只支持 scoped storage，因此对端提供的每个路径都必须留在
    // 应用工作区之内。这是文件系统边界，即使
    // `Connection` 更早也会拒绝工作区之外的请求，这里仍要保持强制。
    #[cfg(target_os = "android")]
    {
        // 这条消息所处理的对端提供路径的（路径、任务 id、文件序号、是否允许为空）
        // 。
        let checked: Option<(&str, i32, i32, bool)> = match &fs {
            ipc::FS::ReadEmptyDirs { dir, .. } => Some((dir.as_str(), -1, -1, false)),
            ipc::FS::ReadDir { dir, .. } => Some((dir.as_str(), -1, -1, true)),
            ipc::FS::RemoveDir { path, id, .. } | ipc::FS::CreateDir { path, id } => {
                Some((path.as_str(), *id, 0, false))
            }
            ipc::FS::Rename { path, id, .. } => Some((path.as_str(), *id, 0, false)),
            ipc::FS::RemoveFile { path, id, file_num } => {
                Some((path.as_str(), *id, *file_num, false))
            }
            ipc::FS::ReadAllFiles { path, id, .. } => Some((path.as_str(), *id, -1, false)),
            ipc::FS::NewWrite {
                path, id, file_num, ..
            }
            | ipc::FS::ReadFile {
                path, id, file_num, ..
            } => Some((path.as_str(), *id, *file_num, false)),
            _ => None,
        };
        if let Some((path, id, file_num, allow_empty)) = checked {
            if !crate::common::is_peer_path_allowed(path, allow_empty) {
                log::warn!("Reject file operation outside the app workspace: {}", path);
                if id >= 0 {
                    send_raw(fs::new_error(id, "Permission denied", file_num), tx);
                }
                return;
            }
        }
        if let ipc::FS::Rename { path, new_name, id } = &fs {
            let destination = std::path::Path::new(path)
                .parent()
                .map(|parent| parent.join(new_name));
            let allowed = destination
                .as_deref()
                .and_then(std::path::Path::to_str)
                .map_or(false, |path| {
                    crate::common::is_peer_path_allowed(path, false)
                });
            if !allowed {
                log::warn!(
                    "Reject rename destination outside the app workspace: {:?}",
                    destination
                );
                send_raw(fs::new_error(*id, "Permission denied", 0), tx);
                return;
            }
        }
    }
    match fs {
        ipc::FS::ReadEmptyDirs {
            dir,
            include_hidden,
        } => {
            read_empty_dirs(&dir, include_hidden, tx).await;
        }
        ipc::FS::ReadDir {
            dir,
            include_hidden,
        } => {
            read_dir(&dir, include_hidden, tx).await;
        }
        ipc::FS::RemoveDir {
            path,
            id,
            recursive,
        } => {
            remove_dir(path, id, recursive, tx).await;
        }
        ipc::FS::RemoveFile { path, id, file_num } => {
            remove_file(path, id, file_num, tx).await;
        }
        ipc::FS::CreateDir { path, id } => {
            create_dir(path, id, tx).await;
        }
        ipc::FS::NewWrite {
            path,
            id,
            file_num,
            mut files,
            overwrite_detection,
            total_size,
            conn_id,
        } => {
            // 把文件转换为 FileEntry
            let file_entries: Vec<FileEntry> = files
                .drain(..)
                .map(|f| FileEntry {
                    name: f.0,
                    modified_time: f.1,
                    ..Default::default()
                })
                .collect();

            // cm 没有 show_hidden 上下文
            // 占位的 remote、show_hidden、is_remote
            let mut job = fs::TransferJob::new_write(
                id,
                fs::JobType::Generic,
                "".to_string(),
                fs::DataSource::FilePath(PathBuf::from(&path)),
                file_num,
                false,
                false,
                overwrite_detection,
            );
            if let Err(e) = job.set_files(file_entries) {
                log::warn!("Reject unsafe transfer file list for {}: {}", path, e);
                send_raw(fs::new_error(id, e, file_num), tx);
                return;
            }
            job.total_size = total_size;
            job.conn_id = conn_id;
            write_jobs.push(job);
        }
        ipc::FS::CancelWrite { id } => {
            if let Some(job) = fs::remove_job(id, write_jobs) {
                job.remove_download_file();
                if let Some(tx) = tx_log {
                    if let Err(e) = tx.send(serialize_transfer_job(&job, false, true, "")) {
                        log::error!("error sending transfer job log via IPC: {}", e);
                    }
                }
            }
        }
        ipc::FS::WriteDone { id, file_num } => {
            if let Some(job) = fs::remove_job(id, write_jobs) {
                job.modify_time();
                send_raw(fs::new_done(id, file_num), tx);
                tx_log.map(|tx| tx.send(serialize_transfer_job(&job, true, false, "")));
            }
        }
        ipc::FS::WriteError { id, file_num, err } => {
            if let Some(job) = fs::remove_job(id, write_jobs) {
                tx_log.map(|tx| tx.send(serialize_transfer_job(&job, false, false, &err)));
                send_raw(fs::new_error(job.id(), err, file_num), tx);
            }
        }
        ipc::FS::WriteBlock {
            id,
            file_num,
            data,
            compressed,
        } => {
            if let Some(job) = fs::get_job(id, write_jobs) {
                if let Err(err) = job
                    .write(FileTransferBlock {
                        id,
                        file_num,
                        data,
                        compressed,
                        ..Default::default()
                    })
                    .await
                {
                    send_raw(fs::new_error(id, err, file_num), &tx);
                }
            }
        }
        ipc::FS::CheckDigest {
            id,
            file_num,
            file_size,
            last_modified,
            is_upload,
            is_resume,
        } => {
            if let Some(job) = fs::get_job(id, write_jobs) {
                let mut req = FileTransferSendConfirmRequest {
                    id,
                    file_num,
                    union: Some(file_transfer_send_confirm_request::Union::OffsetBlk(0)),
                    ..Default::default()
                };
                let digest = FileTransferDigest {
                    id,
                    file_num,
                    last_modified,
                    file_size,
                    ..Default::default()
                };
                if let Some(file) = job.files().get(file_num as usize) {
                    if let fs::DataSource::FilePath(p) = &job.data_source {
                        let path = get_string(&fs::TransferJob::join(p, &file.name));
                        match is_write_need_confirmation(is_resume, &path, &digest) {
                            Ok(digest_result) => {
                                job.set_digest(file_size, last_modified);
                                match digest_result {
                                    DigestCheckResult::IsSame => {
                                        req.set_skip(true);
                                        let msg_out = new_send_confirm(req);
                                        send_raw(msg_out, &tx);
                                    }
                                    DigestCheckResult::NeedConfirm(mut digest) => {
                                        // 上传到服务器，但服务器已有相同文件，于是请求
                                        digest.is_upload = is_upload;
                                        let mut msg_out = Message::new();
                                        let mut fr = FileResponse::new();
                                        fr.set_digest(digest);
                                        msg_out.set_file_response(fr);
                                        send_raw(msg_out, &tx);
                                    }
                                    DigestCheckResult::NoSuchFile => {
                                        let msg_out = new_send_confirm(req);
                                        send_raw(msg_out, &tx);
                                    }
                                }
                            }
                            Err(err) => {
                                send_raw(fs::new_error(id, err, file_num), &tx);
                            }
                        }
                    }
                }
            }
        }
        ipc::FS::SendConfirm(bytes) => {
            if let Ok(r) = FileTransferSendConfirmRequest::parse_from_bytes(&bytes) {
                if let Some(job) = fs::get_job(r.id, write_jobs) {
                    job.confirm(&r).await;
                }
            }
        }
        ipc::FS::Rename { id, path, new_name } => {
            rename_file(path, new_name, id, tx).await;
        }
        ipc::FS::ReadFile {
            path,
            id,
            file_num,
            include_hidden,
            conn_id,
            overwrite_detection,
        } => {
            start_read_job(
                path,
                file_num,
                include_hidden,
                id,
                conn_id,
                overwrite_detection,
                read_jobs,
                tx,
            )
            .await;
        }
        // 取消进行中的读取任务（服务器到客户端的文件传输）。
        // 注意：这只会取消 `read_jobs` 中的任务。它不会取消 `ReadAllFiles`
        // 操作，那些是一次性的目录扫描，完成得很快，而且没有
        // 持久化的任务跟踪。
        ipc::FS::CancelRead { id, conn_id: _ } => {
            if let Some(job) = fs::remove_job(id, read_jobs) {
                if let Some(tx) = tx_log {
                    if let Err(e) = tx.send(serialize_transfer_job(&job, false, true, "")) {
                        log::error!("error sending transfer job log via IPC: {}", e);
                    }
                }
            }
        }
        ipc::FS::SendConfirmForRead {
            id,
            file_num: _,
            skip,
            offset_blk,
            conn_id: _,
        } => {
            if let Some(job) = fs::get_job(id, read_jobs) {
                let req = FileTransferSendConfirmRequest {
                    id,
                    file_num: job.file_num(),
                    union: if skip {
                        Some(file_transfer_send_confirm_request::Union::Skip(true))
                    } else {
                        Some(file_transfer_send_confirm_request::Union::OffsetBlk(
                            offset_blk,
                        ))
                    },
                    ..Default::default()
                };
                job.confirm(&req).await;
            }
        }
        // 递归列出目录中的所有文件。
        // 这是一次性操作，无法通过 CancelRead 取消。
        // 该操作通常完成得很快，因为它只读取目录元数据，
        // 而不读取文件内容。文件数受 `check_file_count_limit()` 限制。
        ipc::FS::ReadAllFiles {
            path,
            id,
            include_hidden,
            conn_id,
        } => {
            read_all_files(path, include_hidden, id, conn_id, tx).await;
        }
        _ => {}
    }
}

/// 在 CM 中启动一个用于服务器到客户端文件传输的读取任务（仅 Windows）。
///
/// 这会通过 `new_read()` 创建一个 `TransferJob`，校验它，并把
/// 初始文件列表通过 IPC 发回给 Connection。
///
/// NOTE: This is the CM-side equivalent of `create_and_start_read_job()` in
/// `src/server/connection.rs`。在非 Windows 平台上，Connection 直接处理
/// 读取任务。两者都用 `TransferJob::new_read()`，逻辑相似。
/// 修改任务创建或校验时，要确保两条路径保持同步。
#[cfg(not(any(target_os = "ios")))]
async fn start_read_job(
    path: String,
    file_num: i32,
    include_hidden: bool,
    id: i32,
    conn_id: i32,
    overwrite_detection: bool,
    read_jobs: &mut Vec<fs::TransferJob>,
    tx: &UnboundedSender<Data>,
) {
    let path_clone = path.clone();
    let result = spawn_blocking(move || -> ResultType<fs::TransferJob> {
        let data_source = fs::DataSource::FilePath(PathBuf::from(&path));
        fs::TransferJob::new_read(
            id,
            fs::JobType::Generic,
            "".to_string(),
            data_source,
            file_num,
            include_hidden,
            true,
            overwrite_detection,
        )
    })
    .await;

    match result {
        Ok(Ok(mut job)) => {
            // 可选：对 CM 侧的任务强制文件数上限，以避免
            // 过多 I/O。这是作用在任务的文件列表上的，
            // 该列表由 `new_read` 产生，与 AllFiles 使用同一个辅助函数的方式类似。
            if let Err(msg) = check_file_count_limit(job.files().len()) {
                if let Err(e) = tx.send(Data::ReadJobInitResult {
                    id,
                    file_num,
                    include_hidden,
                    conn_id,
                    result: Err(msg),
                }) {
                    log::error!("error sending ReadJobInitResult via IPC: {}", e);
                }
                return;
            }

            // 根据任务的文件列表构建 FileDirectory 并序列化
            let files = job.files().to_owned();
            let mut dir = FileDirectory::new();
            dir.id = id;
            dir.path = path_clone.clone();
            dir.entries = files.clone().into();

            let dir_bytes = match dir.write_to_bytes() {
                Ok(bytes) => bytes,
                Err(e) => {
                    if let Err(e) = tx.send(Data::ReadJobInitResult {
                        id,
                        file_num,
                        include_hidden,
                        conn_id,
                        result: Err(format!("serialize failed: {}", e)),
                    }) {
                        log::error!("error sending ReadJobInitResult via IPC: {}", e);
                    }
                    return;
                }
            };

            if let Err(e) = tx.send(Data::ReadJobInitResult {
                id,
                file_num,
                include_hidden,
                conn_id,
                result: Ok(dir_bytes),
            }) {
                log::error!("error sending ReadJobInitResult via IPC: {}", e);
            }

            // 附带连接 id，以便 CM 能正确地把读取块路由回去
            job.conn_id = conn_id;
            read_jobs.push(job);
        }
        Ok(Err(e)) => {
            if let Err(e) = tx.send(Data::ReadJobInitResult {
                id,
                file_num,
                include_hidden,
                conn_id,
                result: Err(format!("validation failed: {}", e)),
            }) {
                log::error!("error sending ReadJobInitResult via IPC: {}", e);
            }
        }
        Err(e) => {
            if let Err(e) = tx.send(Data::ReadJobInitResult {
                id,
                file_num,
                include_hidden,
                conn_id,
                result: Err(format!("validation task failed: {}", e)),
            }) {
                log::error!("error sending ReadJobInitResult via IPC: {}", e);
            }
        }
    }
}

/// 定期处理读取任务，读取文件块并通过 IPC 发送。
///
/// NOTE: This is the CM-side equivalent of `handle_read_jobs()` in
/// `libs/base/src/fs.rs`。这里的逻辑与该实现一致，
/// 只是通过 IPC 通信而不是直接用网络流。
/// 修改任务处理逻辑时，要确保两个实现保持同步。
#[cfg(not(any(target_os = "ios")))]
async fn handle_read_jobs_tick(
    jobs: &mut Vec<fs::TransferJob>,
    tx: &UnboundedSender<Data>,
    conn_id: i32,
) -> ResultType<()> {
    let mut finished = Vec::new();

    for job in jobs.iter_mut() {
        if job.is_last_job {
            continue;
        }

        // 必要时初始化数据流（打开文件，发送摘要用于覆盖检测）
        if let Err(err) = init_read_job_for_cm(job, tx, conn_id).await {
            if let Err(e) = tx.send(Data::FileReadError {
                id: job.id,
                file_num: job.file_num(),
                err: format!("{}", err),
                conn_id,
            }) {
                log::error!("error sending FileReadError via IPC: {}", e);
            }
            finished.push(job.id);
            continue;
        }

        // 从文件中读取一个块
        match job.read().await {
            Err(err) => {
                if let Err(e) = tx.send(Data::FileReadError {
                    id: job.id,
                    file_num: job.file_num(),
                    err: format!("{}", err),
                    conn_id,
                }) {
                    log::error!("error sending FileReadError via IPC: {}", e);
                }
                // 把任务标记为已完成，以防止无限重试。
                // Connection 侧在收到 FileReadError 后已经
                // 移除了 cm_read_job_ids，继续下去没有意义。
                finished.push(job.id);
            }
            Ok(Some(block)) => {
                if let Err(e) = tx.send(Data::FileBlockFromCM {
                    id: block.id,
                    file_num: block.file_num,
                    data: block.data,
                    compressed: block.compressed,
                    conn_id,
                }) {
                    log::error!("error sending FileBlockFromCM via IPC: {}", e);
                }
            }
            Ok(None) => {
                if job.job_completed() {
                    finished.push(job.id);
                    match job.job_error() {
                        Some(err) => {
                            if let Err(e) = tx.send(Data::FileReadError {
                                id: job.id,
                                file_num: job.file_num(),
                                err,
                                conn_id,
                            }) {
                                log::error!("error sending FileReadError via IPC: {}", e);
                            }
                        }
                        None => {
                            if let Err(e) = tx.send(Data::FileReadDone {
                                id: job.id,
                                file_num: job.file_num(),
                                conn_id,
                            }) {
                                log::error!("error sending FileReadDone via IPC: {}", e);
                            }
                        }
                    }
                }
                // 否则：等待对端确认
            }
        }
        // 跳出以便逐个处理任务。
        break;
    }

    for id in finished {
        let _ = fs::remove_job(id, jobs);
    }

    Ok(())
}

/// 初始化读取任务的数据流，并处理用于覆盖检测的摘要发送。
///
/// NOTE: This is the CM-side equivalent of `TransferJob::init_data_stream()` in
/// `libs/base/src/fs.rs`。它调用 `init_data_stream_for_cm()` 并通过 IPC 发送
/// 摘要，而不是直接用网络流。
/// 修改初始化或摘要逻辑时，要确保两条路径保持同步。
#[cfg(not(any(target_os = "ios")))]
async fn init_read_job_for_cm(
    job: &mut fs::TransferJob,
    tx: &UnboundedSender<Data>,
    conn_id: i32,
) -> ResultType<()> {
    // 初始化数据流，并在需要覆盖检测时获取摘要信息
    match job.init_data_stream_for_cm().await? {
        Some((last_modified, file_size)) => {
            // 通过 IPC 发送摘要用于覆盖检测
            if let Err(e) = tx.send(Data::FileDigestFromCM {
                id: job.id,
                file_num: job.file_num(),
                last_modified,
                file_size,
                is_resume: job.is_resume,
                conn_id,
            }) {
                log::error!("error sending FileDigestFromCM via IPC: {}", e);
            }
        }
        None => {
            // 任务已完成或已初始化，无需处理
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "ios")))]
async fn read_all_files(
    path: String,
    include_hidden: bool,
    id: i32,
    conn_id: i32,
    tx: &UnboundedSender<Data>,
) {
    let path_clone = path.clone();
    let result = spawn_blocking(move || fs::get_recursive_files(&path, include_hidden)).await;

    let result = match result {
        Ok(Ok(files)) => {
            // 检查文件数上限，以避免过多 I/O 和资源占用
            if let Err(msg) = check_file_count_limit(files.len()) {
                Err(msg)
            } else {
                // 把 FileDirectory 序列化为 protobuf 字节
                let mut fd = FileDirectory::new();
                fd.id = id;
                fd.path = path_clone.clone();
                fd.entries = files.into();
                match fd.write_to_bytes() {
                    Ok(bytes) => Ok(bytes),
                    Err(e) => Err(format!("serialize failed: {}", e)),
                }
            }
        }
        Ok(Err(e)) => Err(format!("{}", e)),
        Err(e) => Err(format!("task failed: {}", e)),
    };

    if let Err(e) = tx.send(Data::AllFilesResult {
        id,
        conn_id,
        path: path_clone,
        result,
    }) {
        log::error!("error sending AllFilesResult via IPC: {}", e);
    }
}

#[cfg(not(any(target_os = "ios")))]
async fn read_empty_dirs(dir: &str, include_hidden: bool, tx: &UnboundedSender<Data>) {
    let path = dir.to_owned();
    let path_clone = dir.to_owned();

    if let Ok(Ok(fds)) =
        spawn_blocking(move || fs::get_empty_dirs_recursive(&path, include_hidden)).await
    {
        let mut msg_out = Message::new();
        let mut file_response = FileResponse::new();
        file_response.set_empty_dirs(ReadEmptyDirsResponse {
            path: path_clone,
            empty_dirs: fds,
            ..Default::default()
        });
        msg_out.set_file_response(file_response);
        send_raw(msg_out, tx);
    }
}

#[cfg(not(any(target_os = "ios")))]
async fn read_dir(dir: &str, include_hidden: bool, tx: &UnboundedSender<Data>) {
    let path = {
        if dir.is_empty() {
            Config::get_home()
        } else {
            fs::get_path(dir)
        }
    };
    let result = spawn_blocking(move || fs::read_dir(&path, include_hidden)).await;
    let msg_out = match result {
        Ok(Ok(fd)) => {
            let mut msg_out = Message::new();
            let mut file_response = FileResponse::new();
            file_response.set_dir(fd);
            msg_out.set_file_response(file_response);
            msg_out
        }
        Ok(Err(err)) => fs::new_error(0, err, -1),
        Err(err) => fs::new_error(0, err, -1),
    };
    send_raw(msg_out, tx);
}

#[cfg(not(any(target_os = "ios")))]
async fn handle_result<F: std::fmt::Display, S: std::fmt::Display>(
    res: std::result::Result<std::result::Result<(), F>, S>,
    id: i32,
    file_num: i32,
    tx: &UnboundedSender<Data>,
) {
    match res {
        Err(err) => {
            send_raw(fs::new_error(id, err, file_num), tx);
        }
        Ok(Err(err)) => {
            send_raw(fs::new_error(id, err, file_num), tx);
        }
        Ok(Ok(())) => {
            send_raw(fs::new_done(id, file_num), tx);
        }
    }
}

#[cfg(not(any(target_os = "ios")))]
async fn remove_file(path: String, id: i32, file_num: i32, tx: &UnboundedSender<Data>) {
    handle_result(
        spawn_blocking(move || fs::remove_file(&path)).await,
        id,
        file_num,
        tx,
    )
    .await;
}

#[cfg(not(any(target_os = "ios")))]
async fn create_dir(path: String, id: i32, tx: &UnboundedSender<Data>) {
    handle_result(
        spawn_blocking(move || fs::create_dir(&path)).await,
        id,
        0,
        tx,
    )
    .await;
}

#[cfg(not(any(target_os = "ios")))]
async fn rename_file(path: String, new_name: String, id: i32, tx: &UnboundedSender<Data>) {
    handle_result(
        spawn_blocking(move || fs::rename_file(&path, &new_name)).await,
        id,
        0,
        tx,
    )
    .await;
}

#[cfg(not(any(target_os = "ios")))]
async fn remove_dir(path: String, id: i32, recursive: bool, tx: &UnboundedSender<Data>) {
    let path = fs::get_path(&path);
    handle_result(
        spawn_blocking(move || {
            if recursive {
                fs::remove_all_empty_dir(&path)
            } else {
                std::fs::remove_dir(&path).map_err(|err| err.into())
            }
        })
        .await,
        id,
        0,
        tx,
    )
    .await;
}

#[cfg(not(any(target_os = "ios")))]
fn send_raw(msg: Message, tx: &UnboundedSender<Data>) {
    match msg.write_to_bytes() {
        Ok(bytes) => {
            allow_err!(tx.send(Data::RawMessage(bytes)));
        }
        err => allow_err!(err),
    }
}

#[cfg(windows)]
fn cm_inner_send(id: i32, data: Data) {
    let lock = CLIENTS.read().unwrap();
    if id != 0 {
        if let Some(s) = lock.get(&id) {
            allow_err!(s.tx.send(data));
        }
    } else {
        for s in lock.values() {
            allow_err!(s.tx.send(data.clone()));
        }
    }
}

pub fn can_elevate() -> bool {
    #[cfg(windows)]
    return !crate::platform::is_installed();
    #[cfg(not(windows))]
    return false;
}

pub fn elevate_portable(_id: i32) {
    #[cfg(windows)]
    {
        let lock = CLIENTS.read().unwrap();
        if let Some(s) = lock.get(&_id) {
            allow_err!(s.tx.send(ipc::Data::DataPortableService(
                ipc::DataPortableService::RequestStart
            )));
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
#[inline]
pub fn handle_incoming_voice_call(id: i32, accept: bool) {
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        // iOS 上尚未处理。
        #[cfg(not(any(target_os = "ios")))]
        allow_err!(client.tx.send(Data::VoiceCallResponse(accept)));
    };
}

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
#[inline]
pub fn close_voice_call(id: i32) {
    if let Some(client) = CLIENTS.read().unwrap().get(&id) {
        // iOS 上尚未处理。
        #[cfg(not(any(target_os = "ios")))]
        allow_err!(client.tx.send(Data::CloseVoiceCall("".to_owned())));
    };
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn quit_cm() {
    // 以防 std::process::exit 不生效
    log::info!("quit cm");
    CLIENTS.write().unwrap().clear();
    // `quit_gui()` 在 Windows 和 macOS 上会结束进程，但在 Linux 上它调用的是
    // `gtk_main_quit()`，在 Flutter 连接管理器中不起作用：
    // `flutter/linux/main.cc` 运行的是 `g_application_run()`（GtkApplication），所以
    // `gtk_main()` 从未被调用。改为直接退出，否则这个
    // 进程会在不再服务 `_cm` ipc 端点的情况下继续运行，导致
    // 服务器无法复用它，会再启动一个连接管理器。
    //
    // NOTE: a client merely disconnecting does not come here, the Flutter side
    // 随后关闭窗口，所以这是一个兜底手段，而不是对
    // #15698 中那些残留进程的解释。
    #[cfg(all(target_os = "linux", feature = "flutter"))]
    std::process::exit(0);
    #[cfg(not(all(target_os = "linux", feature = "flutter")))]
    crate::platform::quit_gui();
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ipc::Data;
    use base::message_proto::{FileDirectory, Message};
    use hbb_common::tokio::{runtime::Runtime, sync::mpsc::unbounded_channel};
    use std::fs;

    #[test]
    #[cfg(not(any(target_os = "ios")))]
    fn read_all_files_success() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = unbounded_channel();
            let dir = std::env::temp_dir().join("rustdesk_read_all_test");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("test.txt"), b"hello").unwrap();

            let path_str = dir.to_string_lossy().to_string();
            super::read_all_files(path_str.clone(), false, 1, 2, &tx).await;

            match rx.recv().await.unwrap() {
                Data::AllFilesResult { result, .. } => {
                    let bytes = result.unwrap();
                    let fd = FileDirectory::parse_from_bytes(&bytes).unwrap();
                    assert!(!fd.entries.is_empty());
                }
                _ => panic!("unexpected data"),
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    #[cfg(not(any(target_os = "ios")))]
    fn read_dir_reports_success_and_error() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = unbounded_channel();
            let dir = std::env::temp_dir().join("rustdesk_read_dir_test");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();

            super::read_dir(&dir.to_string_lossy(), false, &tx).await;

            match rx.recv().await.unwrap() {
                Data::RawMessage(bytes) => {
                    let mut msg = Message::new();
                    msg.merge_from_bytes(&bytes).unwrap();
                    assert!(msg
                        .file_response()
                        .dir()
                        .path
                        .contains("rustdesk_read_dir_test"));
                }
                _ => panic!("unexpected data"),
            }
            let _ = fs::remove_dir_all(&dir);

            super::read_dir(&dir.to_string_lossy(), false, &tx).await;

            match rx.recv().await.unwrap() {
                Data::RawMessage(bytes) => {
                    let mut msg = Message::new();
                    msg.merge_from_bytes(&bytes).unwrap();
                    assert_eq!(msg.file_response().error().id, 0);
                    assert!(!msg.file_response().error().error.is_empty());
                }
                _ => panic!("unexpected data"),
            }
        });
    }
}
