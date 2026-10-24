#[path = "ipc/auth.rs"]
mod ipc_auth;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "ipc/fs.rs"]
mod ipc_fs;
// DRM/KMS 采集生产者、`_drm` 通道及其 SCM_RIGHTS 组帧都放在它们自己的
// 模块中，以与本文件其他部分相同的方式声明，这样这个可选特性在这里只增加
// 一个边界清晰、自成体系的接口，而不是在共享 IPC 中间塞进约 1800 行。
#[cfg(all(target_os = "linux", feature = "drm"))]
#[path = "ipc/drm.rs"]
mod ipc_drm;
// 重新导出，使得调用方已经在使用的路径（`crate::ipc::start_drm`、`crate::ipc::connect_drm`、
// `crate::ipc::DrmDisplayInfo`）继续可用，同时也让 `Data` 变体能指明这两种
// 载荷类型。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub use ipc_drm::{start_drm, DmabufDesc, DrmDisplayInfo};
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) use ipc_drm::DrmConn;
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) use ipc_drm::connect_drm;

use crate::{
    common::{is_server, CheckTestNatType},
    privacy_mode,
    privacy_mode::PrivacyModeState,
    rendezvous_mediator::RendezvousMediator,
    ui_interface::{get_local_option, set_local_option},
};
use bytes::Bytes;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use clipboard::ClipboardFile;
#[cfg(target_os = "linux")]
use hbb_common::anyhow;
use hbb_common::{
    allow_err, bail, bytes,
    bytes_codec::BytesCodec,
    config::{self, Config, Config2},
    futures::StreamExt as _,
    futures_util::sink::SinkExt,
    log, password_security as password, timeout,
    tokio::{
        self,
        io::{AsyncRead, AsyncWrite},
    },
    tokio_util::codec::Framed,
    ResultType,
};
use base::config::keys::{self, OPTION_ALLOW_WEBSOCKET};
#[cfg(windows)]
pub(crate) use ipc_auth::authorize_windows_portable_service_ipc_connection;
#[cfg(windows)]
pub(crate) use ipc_auth::ensure_peer_executable_matches_current_by_pid_opt;
#[cfg(windows)]
pub(crate) use ipc_auth::log_rejected_windows_ipc_connection;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ipc_auth::{active_uid, authorize_service_scoped_ipc_connection};
#[cfg(target_os = "macos")]
use ipc_auth::authorize_user_server_process;
#[cfg(windows)]
use ipc_auth::{
    authorize_windows_main_ipc_connection, portable_service_listener_security_attributes,
    should_allow_everyone_create_on_windows,
};
#[cfg(target_os = "linux")]
pub(crate) use ipc_auth::{
    ensure_peer_executable_matches_current_by_fd, is_allowed_service_peer_uid,
    log_rejected_uinput_connection, peer_uid_from_fd,
};
#[cfg(target_os = "linux")]
use ipc_fs::terminal_count_candidate_uids;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ipc_fs::{
    check_pid, ensure_secure_ipc_parent_dir, scrub_secure_ipc_parent_dir,
    should_scrub_parent_entries_after_check_pid, write_pid,
};
// 与使用它的模块一同做条件门控，这样不含 `drm` 的构建就不会带上未使用的 import。
#[cfg(all(target_os = "linux", feature = "drm"))]
use ipc_fs::remove_ipc_entry_via_secure_parent_fd;
use parity_tokio_ipc::{
    Connection as Conn, ConnectionClient as ConnClient, Endpoint, Incoming, SecurityAttributes,
};
use serde_derive::{Deserialize, Serialize};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::cell::Cell;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

// 这里的 IPC 动作。
pub const IPC_ACTION_CLOSE: &str = "close";
#[cfg(target_os = "windows")]
const PORTABLE_SERVICE_IPC_HANDSHAKE_TIMEOUT_MS: u64 = 3_000;
#[cfg(target_os = "windows")]
pub(crate) const IPC_TOKEN_LEN: usize = 64;
#[cfg(target_os = "windows")]
const IPC_TOKEN_RANDOM_BYTES: usize = IPC_TOKEN_LEN / 2;
#[cfg(target_os = "windows")]
const _: () = assert!(IPC_TOKEN_LEN % 2 == 0);
pub static EXIT_RECV_CLOSE: AtomicBool = AtomicBool::new(true);

#[cfg(any(target_os = "linux", target_os = "macos"))]
thread_local! {
    static USE_USER_MAIN_IPC: Cell<bool> = Cell::new(false);
}

#[must_use = "bind this guard to a local variable to keep the IPC scope active"]
/// 线程局部守卫，用于在 Linux/macOS 上把 root 的主 IPC 路由到当前活动用户。
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct UserMainIpcScope {
    previous: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl UserMainIpcScope {
    pub(crate) fn new() -> Self {
        let previous = USE_USER_MAIN_IPC.with(|use_user_main| {
            let previous = use_user_main.get();
            use_user_main.set(true);
            previous
        });
        Self { previous }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for UserMainIpcScope {
    fn drop(&mut self) {
        USE_USER_MAIN_IPC.with(|use_user_main| use_user_main.set(self.previous));
    }
}

#[inline]
pub async fn connect_service(ms_timeout: u64) -> ResultType<ConnectionTmpl<ConnClient>> {
    connect(ms_timeout, crate::POSTFIX_SERVICE).await
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum FS {
    ReadEmptyDirs {
        dir: String,
        include_hidden: bool,
    },
    ReadDir {
        dir: String,
        include_hidden: bool,
    },
    RemoveDir {
        path: String,
        id: i32,
        recursive: bool,
    },
    RemoveFile {
        path: String,
        id: i32,
        file_num: i32,
    },
    CreateDir {
        path: String,
        id: i32,
    },
    NewWrite {
        path: String,
        id: i32,
        file_num: i32,
        files: Vec<(String, u64)>,
        overwrite_detection: bool,
        total_size: u64,
        conn_id: i32,
    },
    CancelWrite {
        id: i32,
    },
    WriteBlock {
        id: i32,
        file_num: i32,
        data: Bytes,
        compressed: bool,
    },
    WriteDone {
        id: i32,
        file_num: i32,
    },
    WriteError {
        id: i32,
        file_num: i32,
        err: String,
    },
    WriteOffset {
        id: i32,
        file_num: i32,
        offset_blk: u32,
    },
    CheckDigest {
        id: i32,
        file_num: i32,
        file_size: u64,
        last_modified: u64,
        is_upload: bool,
        is_resume: bool,
    },
    SendConfirm(Vec<u8>),
    Rename {
        id: i32,
        path: String,
        new_name: String,
    },
    // CM 侧的文件读取操作（仅 Windows）
    // 这些让连接管理器可以读取文件并流式传回 Connection
    ReadFile {
        path: String,
        id: i32,
        file_num: i32,
        include_hidden: bool,
        conn_id: i32,
        overwrite_detection: bool,
    },
    CancelRead {
        id: i32,
        conn_id: i32,
    },
    SendConfirmForRead {
        id: i32,
        file_num: i32,
        skip: bool,
        offset_blk: u32,
        conn_id: i32,
    },
    ReadAllFiles {
        path: String,
        id: i32,
        include_hidden: bool,
        conn_id: i32,
    },
}

#[cfg(target_os = "windows")]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t")]
pub struct ClipboardNonFile {
    pub compress: bool,
    pub content: bytes::Bytes,
    pub content_len: usize,
    pub next_raw: bool,
    pub width: i32,
    pub height: i32,
    // message.proto：ClipboardFormat
    pub format: i32,
    pub special_name: String,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataKeyboard {
    Sequence(String),
    KeyDown(enigo::Key),
    KeyUp(enigo::Key),
    KeyClick(enigo::Key),
    GetKeyState(enigo::Key),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataKeyboardResponse {
    GetKeyState(bool),
    #[cfg(target_os = "linux")]
    GetKeyStateError(String),
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataMouse {
    MoveTo(i32, i32),
    MoveRelative(i32, i32),
    Down(enigo::MouseButton),
    Up(enigo::MouseButton),
    Click(enigo::MouseButton),
    ScrollX(i32),
    ScrollY(i32),
    Refresh,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataControl {
    Resolution {
        minx: i32,
        maxx: i32,
        miny: i32,
        maxy: i32,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataPortableService {
    Ping,
    Pong,
    AuthToken(String),
    AuthResult(bool),
    ConnCount(Option<usize>),
    Mouse((Vec<u8>, i32, String, u32, bool, bool)),
    Pointer((Vec<u8>, i32)),
    Key(Vec<u8>),
    RequestStart,
    WillClose,
    CmShowElevation(bool),
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum SwitchSidesUuidAction {
    Check,
    Consume,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum Data {
    Login {
        id: i32,
        is_file_transfer: bool,
        is_view_camera: bool,
        is_terminal: bool,
        peer_id: String,
        name: String,
        avatar: String,
        authorized: bool,
        port_forward: String,
        keyboard: bool,
        clipboard: bool,
        audio: bool,
        file: bool,
        file_transfer_enabled: bool,
        restart: bool,
        recording: bool,
        block_input: bool,
        privacy_mode: bool,
        from_switch: bool,
    },
    ChatMessage {
        text: String,
    },
    SwitchPermission {
        name: String,
        enabled: bool,
    },
    SystemInfo(Option<String>),
    ClickTime(i64),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    MouseMoveTime(i64),
    Authorize,
    Close,
    #[cfg(windows)]
    SAS,
    UserSid(Option<u32>),
    OnlineStatus(Option<(i64, bool)>),
    Config((String, Option<String>)),
    Options(Option<HashMap<String, String>>),
    NatType(Option<i32>),
    ConfirmedKey(Option<(Vec<u8>, Vec<u8>)>),
    RawMessage(Vec<u8>),
    Socks(Option<config::Socks5Server>),
    FS(FS),
    Test,
    SyncConfig(Option<Box<(Config, Config2)>>),
    #[cfg(target_os = "windows")]
    ClipboardFile(ClipboardFile),
    ClipboardFileEnabled(bool),
    #[cfg(target_os = "windows")]
    ClipboardNonFile(Option<(String, Vec<ClipboardNonFile>)>),
    PrivacyModeState((i32, PrivacyModeState, String)),
    TestRendezvousServer,
    Deployed,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Keyboard(DataKeyboard),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    KeyboardResponse(DataKeyboardResponse),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Mouse(DataMouse),
    Control(DataControl),
    Theme(String),
    Language(String),
    Empty,
    Disconnected,
    DataPortableService(DataPortableService),
    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    SwitchSidesRequest(String),
    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    SwitchSidesUuid(String, String, SwitchSidesUuidAction, Option<bool>),
    #[cfg(feature = "flutter")]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    SwitchSidesBack,
    UrlLink(String),
    VoiceCallIncoming,
    StartVoiceCall,
    VoiceCallResponse(bool),
    CloseVoiceCall(String),
    #[cfg(windows)]
    SyncWinCpuUsage(Option<f64>),
    FileTransferLog((String, String)),
    #[cfg(windows)]
    ControlledSessionCount(usize),
    CmErr(String),
    // CM 侧的文件读取响应（仅 Windows）
    // 当 CM 处理文件读取时，这些消息从 CM 发回 Connection
    /// 对 ReadFile 的响应：包含初始文件列表或错误
    ReadJobInitResult {
        id: i32,
        file_num: i32,
        include_hidden: bool,
        conn_id: i32,
        /// FileDirectory 的序列化 protobuf 字节，或错误字符串
        result: Result<Vec<u8>, String>,
    },
    /// CM 读取的文件数据块。
    ///
    /// 实际数据在这条消息之后通过 `send_raw()` 单独发送，以避免
    /// 大块二进制数据的 JSON 编码开销。这与 `WriteBlock` 的模式一致。
    ///
    /// **协议：**
    /// - 发送方：先 `send(FileBlockFromCM{...})`，再 `send_raw(data)`
    /// - 接收方：`next()` 返回 `FileBlockFromCM`，然后 `next_raw()` 返回数据字节
    ///
    /// **关于空数据（例如空文件）的说明：**
    /// 支持空数据。IPC 连接使用 `raw=false`（默认）的 `BytesCodec`，
    /// 它会给每个帧加上长度头。因此 `send_raw(Bytes::new())` 发送的是
    /// 一个 1 字节的帧（length=0），而 `next_raw()` 会正确返回空的 `BytesMut`。
    /// 验证请参见 `libs/hbb_common/src/bytes_codec.rs` 中的测试 `test_codec2`。
    FileBlockFromCM {
        id: i32,
        file_num: i32,
        /// 数据通过 `send_raw()` 单独发送，以避免 JSON 编码开销。
        /// 序列化时会跳过该字段；发送方必须在发送后调用 `send_raw()`。
        /// 接收方必须调用 `next_raw()` 并手动填充该字段。
        #[serde(skip)]
        data: bytes::Bytes,
        compressed: bool,
        conn_id: i32,
    },
    /// 文件读取成功完成
    FileReadDone {
        id: i32,
        file_num: i32,
        conn_id: i32,
    },
    /// 文件读取失败
    FileReadError {
        id: i32,
        file_num: i32,
        err: String,
        conn_id: i32,
    },
    /// 来自 CM 的摘要信息，用于检测覆盖
    FileDigestFromCM {
        id: i32,
        file_num: i32,
        last_modified: u64,
        file_size: u64,
        is_resume: bool,
        conn_id: i32,
    },
    /// 对 ReadAllFiles 的响应：递归目录列表
    AllFilesResult {
        id: i32,
        conn_id: i32,
        path: String,
        /// FileDirectory 的序列化 protobuf 字节，或错误字符串
        result: Result<Vec<u8>, String>,
    },
    CheckHwcodec,
    #[cfg(feature = "flutter")]
    VideoConnCount(Option<usize>),
    // 虽然这个 key 并非必需，但用它可以避免把 key 写死。
    WaylandScreencastRestoreToken((String, String)),
    HwCodecConfig(Option<String>),
    RemoveTrustedDevices(Vec<Bytes>),
    ClearTrustedDevices,
    #[cfg(all(target_os = "windows", feature = "flutter"))]
    PrinterData(Vec<u8>),
    InstallOption(Option<(String, String)>),
    #[cfg(all(
        feature = "flutter",
        not(any(target_os = "android", target_os = "ios"))
    ))]
    ControllingSessionCount(usize),
    #[cfg(target_os = "linux")]
    TerminalSessionCount(usize),
    #[cfg(target_os = "windows")]
    PortForwardSessionCount(Option<usize>),
    SocksWs(Option<Box<(Option<config::Socks5Server>, String)>>),
    #[cfg(target_os = "macos")]
    HasNoActiveConns(Option<bool>),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Whiteboard((String, crate::whiteboard::CustomEvent)),
    ControlPermissionsRemoteModify(Option<bool>),
    #[cfg(target_os = "windows")]
    FileTransferEnabledState(Option<bool>),
    /// CM -> 服务端：连接管理器的窗口消失了，这与
    /// 操作员断开某个对端不是同一事件。仅限 Linux，而且是有意为之：在那里注销会话
    /// 会关闭所有窗口，而这个关闭事件到达 CM 时与
    /// 用户手动点击关闭无法区分 —— 实测在 KDE 上，CM 收不到任何信号，而 logind 仍报告
    /// 会话处于活动状态。所以这种有歧义的情况会结束会话，但不带不重试的原因，且
    /// 允许对端重新连接（注销后会回到登录界面），而
    /// 显式点击断开按钮则仍然发送 `Close` 并彻底踢掉对端。
    #[cfg(target_os = "linux")]
    CmWindowClosed,
    // --- 通过 `_drm` 服务作用域通道进行 DRM/KMS 采集（可选的 `drm` 特性）---
    // 以下所有内容都是 `cfg(all(linux, drm))`，因此关闭 drm 时的 IPC 线路与上游
    // 逐字节一致。`_drm` 协议：客户端发送 `DrmHello`，服务端发送 `DrmDisplayList`，然后
    // 客户端回复 `DrmStart{display}`，接着服务端流式发送 `DrmFrame` + send_raw(BGRA) 以及
    // `DrmCursor` + send_raw(RGBA)。帧/光标首部后面总是紧跟着恰好
    // 一个 `send_raw()` 载荷（与 `FileBlockFromCM` 相同的先首部后原始数据的配对方式）。这保持了
    // 首部的可扩展性。下方零拷贝的 `DrmFrameDmabuf(DmabufDesc)` 同级消息只携带一小段
    // JSON 元数据描述符；scanout 的 dma-buf fd 通过 SCM_RIGHTS 辅助消息传递，
    // 在同一个 `DrmConn` 发送上（见 `DrmConn::send_msg`），因此它没有尾随的 `send_raw()` 数据体。
    /// 客户端 -> 服务端，在 `_drm` 上最先发送：服务端是否可以在响应之前唤醒处于休眠状态的显示器
    /// 。每次 `--server` 启动时的缓存预热说明答案是不能：没有对端在等它。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    DrmHello { wake: bool },
    /// 客户端 -> 服务端：开始流式发送所选显示器。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    // `need_cpu` 由无法打开 render-node 转换上下文的非特权消费者设置
    // （例如 drmtap_open_render 失败，如没有 /dev/dri/renderD* 访问权限）。此时服务端会为该连接流式发送
    // CPU 转换的 `DrmFrame` 路径，而不是消费者无法
    // 解 tiled 的 dma-buf fd，这样没有 render node 的座位仍能采集，而不会丢失流。
    DrmStart { display: i32, need_cpu: bool },
    /// 服务端 -> 客户端：枚举出的 DRM 显示器（只发送一次，在帧之前）。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    DrmDisplayList(Vec<DrmDisplayInfo>),
    /// 服务端 -> 客户端：流传输过程中连接器拓扑发生变化（显示器热插拔/拔出/模式设置，
    /// 由服务端的 udev DRM-uevent 监听器观测到）。它携带重新枚举出的列表，使
    /// 消费者可以在热路径之外切换其粘性的“可用”缓存，而无需重新探测
    /// `_drm`（重新探测会触发枚举重启循环）。它与帧在同一
    /// 流上交错发送；不携带 `send_raw()` 数据体，也不携带 fd。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    DrmDisplaysChanged(Vec<DrmDisplayInfo>),
    /// 服务端 -> 客户端：一个帧首部；打包的 BGRA 像素随后通过 `send_raw()` 发送。
    /// CPU 回退路径（没有 render node，或 dma-buf 不可传输）：像素通过线路传输。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    DrmFrame {
        width: u32,
        height: u32,
        /// 参见 `DmabufDesc::plane_rotation`；比该字段更旧的生产者不会带上它。
        #[serde(default)]
        plane_rotation: Option<u32>,
        /// 参见 `DmabufDesc::cursor_pos`：紧接在该帧之后读取的光标平面位置，
        /// 或在光标被隐藏、读取失败或生产者早于该字段时为 `None`。
        #[serde(default)]
        cursor_pos: Option<(i32, i32)>,
    },
    /// 服务端 -> 客户端：一个零拷贝的 dma-buf 帧描述符。scanout fd 不是字段；当
    /// `desc.has_fd` 为真时，它通过同一个 `DrmConn::send_msg` 上的 SCM_RIGHTS 辅助消息传递，并且
    /// 没有尾随的 `send_raw()` 数据体。非特权的 `--server` 导入该 fd 并自行
    /// 完成 EGL detile/转换（见 `DmabufDesc`）。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    DrmFrameDmabuf(DmabufDesc),
    /// 服务端 -> 客户端：一个硬件光标首部；RGBA 像素随后通过 `send_raw()` 发送。
    #[cfg(all(target_os = "linux", feature = "drm"))]
    DrmCursor {
        id: u64,
        width: u32,
        height: u32,
        hotx: i32,
        hoty: i32,
        /// 热点是来自驱动，还是根据位图推断出来的。
        ///
        /// 缺失意味着生产者早于该字段，此时默认值必须为 TRUE，而不是
        /// `bool::default()`。这些消息是两个进程之间通过 unix socket 传输的 JSON，
        /// 而这两个进程是分别升级的：旧的 root 服务可能正在向刚
        /// 启动的 `--server` 流式发送，而它的 `{"hotx": 12, "hoty": 11}` 不带来源信息。若读作
        /// `false`，新的消费者就会丢弃该热点，并从
        /// 未旋转的位图重新推断一个，从而在升级期间让旋转显示器上的光标发生偏移。
        /// 默认为 true 意味着“没有可用的来源信息，因此按旧协议的方式处理”
        /// —— 使用生产者发送的那个点。这并不是声称该值是实测得到的；
        /// 想要重新推断的新生产者会显式写上 `hot_measured: false`，而这
        /// 会被序列化出来，所以新对新之间不受影响。
        #[serde(default = "legacy_hot_measured")]
        hot_measured: bool,
    },
}

/// 当 `hot_measured` 根本不在线路上的默认值；参见该字段的文档。
#[cfg(all(target_os = "linux", feature = "drm"))]
fn legacy_hot_measured() -> bool {
    true
}

#[tokio::main(flavor = "current_thread")]
pub async fn start(postfix: &str) -> ResultType<()> {
    let mut incoming = new_listener(postfix).await?;
    loop {
        if let Some(result) = incoming.next().await {
            match result {
                Ok(stream) => {
                    let mut stream = Connection::new(stream);
                    let postfix = postfix.to_owned();
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if config::is_service_ipc_postfix(&postfix) {
                        if !authorize_service_scoped_ipc_connection(&stream, &postfix) {
                            continue;
                        }
                    }
                    #[cfg(windows)]
                    if postfix.is_empty() {
                        // Windows 主 IPC（`postfix == ""`）在此处授权。
                        // 其他安全敏感通道使用专门的授权路径：
                        // - `_portable_service`：便携服务监听器 + 握手策略
                        // - 服务作用域的 postfix：服务专属的监听器/授权
                        if !authorize_windows_main_ipc_connection(&stream, &postfix) {
                            continue;
                        }
                    }
                    tokio::spawn(async move {
                        loop {
                            match stream.next().await {
                                Err(err) => {
                                    log::trace!("ipc '{}' connection closed: {}", postfix, err);
                                    break;
                                }
                                Ok(Some(data)) => {
                                    // 在 Linux/macOS 上，受保护的 `_service` 通道只用于
                                    // 在 root 服务与活动用户进程之间同步配置。
                                    //
                                    // NOTE: `is_service_ipc_postfix()` also includes `_uinput_*`, but those
                                    // 通道由专门的 uinput 监听器/协议处理，位于
                                    // `src/server/uinput.rs`，因此它们不共享这个 Data 枚举
                                    // 的白名单。这里的 SyncConfig 白名单有意只限于
                                    // `_service` 通道。
                                    //
                                    // 保留这个显式分支，以避免 `_service` 与
                                    // uinput IPC 路径之间的策略漂移，同时仍尽量减少这里暴露的消息面。
                                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                                    if postfix == crate::POSTFIX_SERVICE {
                                        if matches!(&data, Data::SyncConfig(_)) {
                                            handle(data, &mut stream).await;
                                        } else {
                                            log::warn!(
                                                "Rejected non-sync data on protected _service IPC channel: postfix={}, data_kind={:?}, peer_uid={:?}",
                                                postfix,
                                                std::mem::discriminant(&data),
                                                stream.peer_uid()
                                            );
                                            // 关闭该连接，避免在反复收到无效流量时
                                            // 仍保持受保护通道存活。
                                            break;
                                        }
                                        continue;
                                    }
                                    handle(data, &mut stream).await;
                                }
                                Ok(None) => {
                                    // `Ok(None)` 表示收到了一帧完整数据，但未能
                                    // 反序列化为 `Data`。对端关闭/重置则由
                                    // `ConnectionTmpl::next()` 以 `Err` 返回。保持历史上的
                                    // 忽略行为，但受保护的 `_service` 通道除外。
                                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                                    {
                                        if postfix == crate::POSTFIX_SERVICE {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
                Err(err) => {
                    log::error!("Couldn't get client: {:?}", err);
                }
            }
        }
    }
}

pub async fn new_listener(postfix: &str) -> ResultType<Incoming> {
    let path = Config::ipc_path(postfix);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let should_scrub_parent_entries = ensure_secure_ipc_parent_dir(&path, postfix)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let existing_listener_alive = check_pid(postfix).await;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if should_scrub_parent_entries_after_check_pid(
        should_scrub_parent_entries,
        existing_listener_alive,
    ) {
        scrub_secure_ipc_parent_dir(&path, postfix)?;
    }
    let mut endpoint = Endpoint::new(path.clone());
    let security_attrs = {
        #[cfg(windows)]
        {
            if postfix == "_portable_service" {
                portable_service_listener_security_attributes()
            } else if should_allow_everyone_create_on_windows(postfix) {
                SecurityAttributes::allow_everyone_create()
            } else {
                Ok(SecurityAttributes::empty())
            }
        }
        #[cfg(not(windows))]
        {
            SecurityAttributes::allow_everyone_create()
        }
    };
    match security_attrs {
        Ok(attr) => endpoint.set_security_attributes(attr),
        Err(err) => {
            log::error!("Failed to set ipc{} security: {}", postfix, err);
            #[cfg(windows)]
            if postfix == "_portable_service" {
                // 当 SDDL 构造失败时，`_portable_service` 按失败关闭处理。
                // 该端点对安全至关重要，不能以默认 ACL 启动。
                return Err(err.into());
            }
        }
    };
    match endpoint.incoming() {
        Ok(incoming) => {
            if postfix == crate::POSTFIX_SERVICE {
                log::info!("Started protected ipc service server: postfix={}", postfix);
            } else {
                log::info!("Started ipc{} server at path: {}", postfix, &path);
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                // NOTE: On Linux/macOS, some IPC sockets are intentionally world-connectable
                // (0666)，这样活动（非 root）用户进程可以连接。授权是
                // 在这些通道上于 accept 时强制执行的，而受保护的 `_service`
                // 通道还进一步受显式消息白名单限制（仅 SyncConfig
                // ）。
                let socket_mode = if config::is_service_ipc_postfix(postfix) {
                    0o0666
                } else {
                    0o0600
                };
                if let Err(err) =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(socket_mode))
                {
                    log::error!(
                        "Failed to set permissions on ipc{} socket at path {}: {}",
                        postfix,
                        &path,
                        err
                    );
                    std::fs::remove_file(&path).ok();
                    return Err(err.into());
                }
                write_pid(postfix);
            }
            Ok(incoming)
        }
        Err(err) => {
            log::error!(
                "Failed to start ipc{} server at path {}: {}",
                postfix,
                path,
                err
            );
            Err(err.into())
        }
    }
}

pub struct CheckIfRestart {
    stop_service: String,
    rendezvous_servers: Vec<String>,
    audio_input: String,
    voice_call_input: String,
    ws: String,
    disable_udp: String,
    allow_insecure_tls_fallback: String,
    api_server: String,
}

impl CheckIfRestart {
    pub fn new() -> CheckIfRestart {
        CheckIfRestart {
            stop_service: Config::get_option("stop-service"),
            rendezvous_servers: Config::get_rendezvous_servers(),
            audio_input: Config::get_option("audio-input"),
            voice_call_input: Config::get_option("voice-call-input"),
            ws: Config::get_option(OPTION_ALLOW_WEBSOCKET),
            disable_udp: Config::get_option(keys::OPTION_DISABLE_UDP),
            allow_insecure_tls_fallback: Config::get_option(
                keys::OPTION_ALLOW_INSECURE_TLS_FALLBACK,
            ),
            api_server: Config::get_option("api-server"),
        }
    }
}
impl Drop for CheckIfRestart {
    fn drop(&mut self) {
        // 如果使用了 https 代理，就需要重启 rendezvous mediator。
        // 无需检查是否使用了 https 代理，因为该选项不会经常变化
        // 而且即使没有使用 https 代理，重启 mediator 也是安全的。
        let allow_insecure_tls_fallback_changed = self.allow_insecure_tls_fallback
            != Config::get_option(keys::OPTION_ALLOW_INSECURE_TLS_FALLBACK);
        if allow_insecure_tls_fallback_changed
            || self.stop_service != Config::get_option("stop-service")
            || self.rendezvous_servers != Config::get_rendezvous_servers()
            || self.ws != Config::get_option(OPTION_ALLOW_WEBSOCKET)
            || self.disable_udp != Config::get_option(keys::OPTION_DISABLE_UDP)
            || self.api_server != Config::get_option("api-server")
        {
            if allow_insecure_tls_fallback_changed {
                hbb_common::tls::reset_tls_cache();
            }
            RendezvousMediator::restart();
        }
        if self.audio_input != Config::get_option("audio-input") {
            crate::audio_service::restart();
        }
        if self.voice_call_input != Config::get_option("voice-call-input") {
            crate::audio_service::set_voice_call_input_device(
                Some(Config::get_option("voice-call-input")),
                true,
            )
        }
    }
}

async fn handle(data: Data, stream: &mut Connection) {
    match data {
        Data::SystemInfo(_) => {
            let info = format!(
                "log_path: {}, config: {}, username: {}",
                Config::log_path().to_str().unwrap_or(""),
                Config::file().to_str().unwrap_or(""),
                crate::username(),
            );
            allow_err!(stream.send(&Data::SystemInfo(Some(info))).await);
        }
        Data::ClickTime(_) => {
            let t = crate::server::CLICK_TIME.load(Ordering::SeqCst);
            allow_err!(stream.send(&Data::ClickTime(t)).await);
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::MouseMoveTime(_) => {
            let t = crate::server::MOUSE_MOVE_TIME.load(Ordering::SeqCst);
            allow_err!(stream.send(&Data::MouseMoveTime(t)).await);
        }
        Data::Close => {
            log::info!("Receive close message");
            if EXIT_RECV_CLOSE.load(Ordering::SeqCst) {
                #[cfg(not(target_os = "android"))]
                crate::server::input_service::fix_key_down_timeout_at_exit();
                if is_server() {
                    let _ = privacy_mode::turn_off_privacy(0, Some(PrivacyModeState::OffByPeer));
                }
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if crate::is_main() {
                    // 下面这部分是为了支持在安装 rustdesk 以及从界面安装服务期间主窗口能被重新打开
                    // 这样才能创建新的 ipc 服务器（域套接字）。
                    std::fs::remove_file(&Config::ipc_path("")).ok();
                    #[cfg(target_os = "linux")]
                    {
                        hbb_common::sleep((crate::platform::SERVICE_INTERVAL * 2) as f32 / 1000.0)
                            .await;
                        // https://github.com/rustdesk/rustdesk/discussions/9254
                        crate::run_me::<&str>(vec!["--no-server"]).ok();
                    }
                    #[cfg(target_os = "macos")]
                    {
                        // 我们的 launchagent 间隔是 1 秒
                        hbb_common::sleep(1.5).await;
                        std::process::Command::new("open")
                            .arg("-n")
                            .arg(&format!("/Applications/{}.app", crate::get_app_name()))
                            .spawn()
                            .ok();
                    }
                    // 让上面的保持打开一小段时间
                    hbb_common::sleep(0.3).await;
                    // 以防下面的退出失败
                    crate::platform::quit_gui();
                }
                std::process::exit(-1); // to make sure --server luauchagent process can restart because SuccessfulExit used
            }
        }
        Data::OnlineStatus(_) => {
            let x = config::get_online_state();
            let confirmed = Config::get_key_confirmed();
            allow_err!(stream.send(&Data::OnlineStatus(Some((x, confirmed)))).await);
        }
        Data::ConfirmedKey(None) => {
            let out = if Config::get_key_confirmed() {
                Some(Config::get_key_pair())
            } else {
                None
            };
            allow_err!(stream.send(&Data::ConfirmedKey(out)).await);
        }
        Data::Socks(s) => match s {
            None => {
                allow_err!(stream.send(&Data::Socks(Config::get_socks())).await);
            }
            Some(data) => {
                let _nat = CheckTestNatType::new();
                if data.proxy.is_empty() {
                    Config::set_socks(None);
                } else {
                    Config::set_socks(Some(data));
                }
                RendezvousMediator::restart();
                log::info!("socks updated");
            }
        },
        Data::SocksWs(s) => match s {
            None => {
                allow_err!(
                    stream
                        .send(&Data::SocksWs(Some(Box::new((
                            Config::get_socks(),
                            Config::get_option(OPTION_ALLOW_WEBSOCKET)
                        )))))
                        .await
                );
            }
            _ => {}
        },
        #[cfg(feature = "flutter")]
        Data::VideoConnCount(None) => {
            let n = crate::server::AUTHED_CONNS
                .lock()
                .unwrap()
                .iter()
                .filter(|x| x.conn_type == crate::server::AuthConnType::Remote)
                .count();
            allow_err!(stream.send(&Data::VideoConnCount(Some(n))).await);
        }
        Data::Config((name, value)) => match value {
            None => {
                let value;
                if name == "id" {
                    value = Some(Config::get_id());
                } else if name == "temporary-password" {
                    value = Some(password::temporary_password());
                } else if name == "permanent-password-storage-and-salt" {
                    let (storage, salt) = Config::get_local_permanent_password_storage_and_salt();
                    value = Some(storage + "\n" + &salt);
                } else if name == "permanent-password-set" {
                    value = Some(if Config::has_permanent_password() {
                        "Y".to_owned()
                    } else {
                        "N".to_owned()
                    });
                } else if name == "permanent-password-is-preset" {
                    value = Some(if Config::is_using_preset_password() {
                        "Y".to_owned()
                    } else {
                        "N".to_owned()
                    });
                } else if name == "salt" {
                    value = Some(Config::get_salt());
                } else if name == "rendezvous_server" {
                    value = Some(format!(
                        "{},{}",
                        Config::get_rendezvous_server(),
                        Config::get_rendezvous_servers().join(",")
                    ));
                } else if name == "rendezvous_servers" {
                    value = Some(Config::get_rendezvous_servers().join(","));
                } else if name == "fingerprint" {
                    value = if Config::get_key_confirmed() {
                        Some(crate::common::pk_to_fingerprint(Config::get_key_pair().1))
                    } else {
                        None
                    };
                } else if name == "hide_cm" {
                    value = if crate::hbbs_http::sync::is_pro() || crate::common::is_custom_client()
                    {
                        Some(hbb_common::password_security::hide_cm().to_string())
                    } else {
                        None
                    };
                } else if name == "voice-call-input" {
                    value = crate::audio_service::get_voice_call_input_device();
                } else if name == "unlock-pin" {
                    value = Some(Config::get_unlock_pin());
                } else if name == "trusted-devices" {
                    value = Some(Config::get_trusted_devices_json());
                } else {
                    value = None;
                }
                allow_err!(stream.send(&Data::Config((name, value))).await);
            }
            Some(value) => {
                let mut updated = true;
                if name == "id" {
                    // 空的 id 会清除本地 id 并取消密钥确认（参见 #15626）。
                    if value.is_empty() {
                        log::warn!("Ignoring empty id write over IPC");
                        updated = false;
                    } else {
                        Config::set_key_confirmed(false);
                        Config::set_id(&value);
                    }
                } else if name == "temporary-password" {
                    password::update_temporary_password();
                } else if name == "permanent-password" {
                    if Config::is_disable_change_permanent_password() {
                        log::warn!("Changing permanent password is disabled");
                        updated = false;
                    } else {
                        updated = Config::set_permanent_password(&value);
                    }
                    // 对永久密码写入显式返回 ACK/NACK。这让界面/FFI 能够
                    // 区分“守护进程已接受”和“IPC 发送成功”，而无需
                    // 回读任何密钥。
                    let ack = if updated { "Y" } else { "N" }.to_owned();
                    allow_err!(stream.send(&Data::Config((name.clone(), Some(ack)))).await);
                } else if name == "salt" {
                    Config::set_salt(&value);
                } else if name == "voice-call-input" {
                    crate::audio_service::set_voice_call_input_device(Some(value), true);
                } else if name == "unlock-pin" {
                    Config::set_unlock_pin(&value);
                } else {
                    return;
                }
                if updated {
                    log::info!("{} updated", name);
                }
            }
        },
        Data::Options(value) => match value {
            None => {
                let v = Config::get_options();
                allow_err!(stream.send(&Data::Options(Some(v))).await);
            }
            Some(value) => {
                let _chk = CheckIfRestart::new();
                let _nat = CheckTestNatType::new();
                if let Some(v) = value.get("privacy-mode-impl-key") {
                    crate::privacy_mode::switch(v);
                }
                Config::set_options(value);
                allow_err!(stream.send(&Data::Options(None)).await);
            }
        },
        Data::NatType(_) => {
            let t = Config::get_nat_type();
            allow_err!(stream.send(&Data::NatType(Some(t))).await);
        }
        Data::SyncConfig(Some(configs)) => {
            let (config, config2) = *configs;
            let _chk = CheckIfRestart::new();
            Config::set(config);
            Config2::set(config2);
            allow_err!(stream.send(&Data::SyncConfig(None)).await);
        }
        Data::SyncConfig(None) => {
            allow_err!(
                stream
                    .send(&Data::SyncConfig(Some(
                        (Config::get(), Config2::get()).into()
                    )))
                    .await
            );
        }
        #[cfg(windows)]
        Data::SyncWinCpuUsage(None) => {
            allow_err!(
                stream
                    .send(&Data::SyncWinCpuUsage(
                        base::platform::windows::cpu_uage_one_minute()
                    ))
                    .await
            );
        }
        Data::TestRendezvousServer => {
            crate::test_rendezvous_server();
        }
        Data::Deployed => {
            crate::rendezvous_mediator::NEEDS_DEPLOY.store(false, Ordering::SeqCst);
            crate::rendezvous_mediator::RendezvousMediator::restart();
        }
        #[cfg(feature = "flutter")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::SwitchSidesRequest(id) => {
            let uuid = uuid::Uuid::new_v4();
            crate::server::insert_switch_sides_uuid(id, uuid.clone());
            crate::hbbs_http::sync::register_switch_grant(uuid.to_string());
            allow_err!(
                stream
                    .send(&Data::SwitchSidesRequest(uuid.to_string()))
                    .await
            );
        }
        #[cfg(feature = "flutter")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::SwitchSidesUuid(uuid, id, action, None) => {
            let allowed = uuid
                .parse::<uuid::Uuid>()
                .map(|uuid| match action {
                    SwitchSidesUuidAction::Check => {
                        crate::server::has_pending_switch_sides_uuid(&id, &uuid)
                    }
                    SwitchSidesUuidAction::Consume => {
                        crate::server::claim_pending_switch_sides_uuid(&id, &uuid)
                    }
                })
                .unwrap_or(false);
            allow_err!(
                stream
                    .send(&Data::SwitchSidesUuid(uuid, id, action, Some(allowed)))
                    .await
            );
        }
        #[cfg(windows)]
        Data::ControlledSessionCount(_) => {
            allow_err!(
                stream
                    .send(&Data::ControlledSessionCount(
                        crate::Connection::alive_conns().len()
                    ))
                    .await
            );
        }
        #[cfg(target_os = "macos")]
        Data::HasNoActiveConns(None) => {
            allow_err!(
                stream
                    .send(&Data::HasNoActiveConns(Some(
                        crate::updater::has_no_active_conns()
                    )))
                    .await
            );
        }
        #[cfg(all(
            feature = "flutter",
            not(any(target_os = "android", target_os = "ios"))
        ))]
        Data::ControllingSessionCount(count) => {
            crate::updater::update_controlling_session_count(count);
        }
        #[cfg(target_os = "linux")]
        Data::TerminalSessionCount(_) => {
            let count = crate::terminal_service::get_terminal_session_count(true);
            allow_err!(stream.send(&Data::TerminalSessionCount(count)).await);
        }
        #[cfg(feature = "hwcodec")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::CheckHwcodec => {
            scrap::hwcodec::start_check_process();
        }
        #[cfg(feature = "hwcodec")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::HwCodecConfig(c) => {
            match c {
                None => {
                    let v = match scrap::hwcodec::HwCodecConfig::get_set_value() {
                        Some(v) => Some(serde_json::to_string(&v).unwrap_or_default()),
                        None => None,
                    };
                    allow_err!(stream.send(&Data::HwCodecConfig(v)).await);
                }
                Some(v) => {
                    // --server 和 portable
                    scrap::hwcodec::HwCodecConfig::set(v);
                }
            }
        }
        Data::WaylandScreencastRestoreToken((key, value)) => {
            let v = if value == "get" {
                let opt = get_local_option(key.clone());
                #[cfg(not(target_os = "linux"))]
                {
                    Some(opt)
                }
                #[cfg(target_os = "linux")]
                {
                    let v = if opt.is_empty() {
                        if scrap::wayland::pipewire::is_rdp_session_hold() {
                            "fake token".to_string()
                        } else {
                            "".to_owned()
                        }
                    } else {
                        opt
                    };
                    Some(v)
                }
            } else if value == "clear" {
                set_local_option(key.clone(), "".to_owned());
                // 在 portal 请求等待用户期间会一直占用该会话槽位，而这个 ipc
                // 监听器运行在单个线程上：要在该线程之外等待槽位，然后关闭该
                // 请求带回来的内容。
                #[cfg(target_os = "linux")]
                allow_err!(
                    hbb_common::tokio::task::spawn_blocking(
                        scrap::wayland::pipewire::close_session
                    )
                    .await
                );
                Some("".to_owned())
            } else {
                None
            };
            if let Some(v) = v {
                allow_err!(
                    stream
                        .send(&Data::WaylandScreencastRestoreToken((key, v)))
                        .await
                );
            }
        }
        Data::RemoveTrustedDevices(v) => {
            Config::remove_trusted_devices(&v);
        }
        Data::ClearTrustedDevices => {
            Config::clear_trusted_devices();
        }
        Data::InstallOption(opt) => match opt {
            Some((_k, _v)) => {
                #[cfg(target_os = "windows")]
                if let Err(e) = crate::platform::windows::update_install_option(&_k, &_v) {
                    log::error!(
                        "Failed to update install option \"{}\" to \"{}\", error: {}",
                        &_k,
                        &_v,
                        e
                    );
                }
            }
            None => {
                // `None` 通常用于获取值。
                // 这个分支留空，以便统一和后续使用。
            }
        },
        #[cfg(target_os = "windows")]
        Data::PortForwardSessionCount(c) => match c {
            None => {
                let count = crate::server::AUTHED_CONNS
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c.conn_type == crate::server::AuthConnType::PortForward)
                    .count();
                allow_err!(
                    stream
                        .send(&Data::PortForwardSessionCount(Some(count)))
                        .await
                );
            }
            _ => {
                // 端口转发会话数只是用于获取的值。
            }
        },
        Data::ControlPermissionsRemoteModify(_) => {
            use hbb_common::rendezvous_proto::control_permissions::Permission;
            let state =
                crate::server::get_control_permission_state(Permission::remote_modify, true);
            allow_err!(
                stream
                    .send(&Data::ControlPermissionsRemoteModify(state))
                    .await
            );
        }
        #[cfg(target_os = "windows")]
        Data::FileTransferEnabledState(_) => {
            use hbb_common::rendezvous_proto::control_permissions::Permission;
            let state = crate::server::get_control_permission_state(Permission::file, false);
            let enabled = state.unwrap_or_else(|| {
                crate::server::Connection::is_permission_enabled_locally(
                    keys::OPTION_ENABLE_FILE_TRANSFER,
                )
            });
            allow_err!(
                stream
                    .send(&Data::FileTransferEnabledState(Some(enabled)))
                    .await
            );
        }
        _ => {}
    };
}

#[cfg(target_os = "windows")]
pub(crate) fn generate_one_time_ipc_token() -> ResultType<String> {
    use hbb_common::rand::{rngs::OsRng, RngCore as _};
    use std::fmt::Write as _;

    let mut random_bytes = [0u8; IPC_TOKEN_RANDOM_BYTES];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut random_bytes).map_err(|err| {
        hbb_common::anyhow::anyhow!(
            "failed to generate portable service ipc token from OsRng: {}",
            err
        )
    })?;

    let mut token = String::with_capacity(IPC_TOKEN_LEN);
    for byte in random_bytes {
        let _ = write!(token, "{:02x}", byte);
    }
    Ok(token)
}

#[cfg(target_os = "windows")]
pub(crate) fn constant_time_ipc_token_eq(expected: &str, candidate: &str) -> bool {
    if expected.len() != IPC_TOKEN_LEN || candidate.len() != IPC_TOKEN_LEN {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(candidate.as_bytes().iter())
        .fold(0u8, |diff, (left, right)| diff | (*left ^ *right))
        == 0
}

#[cfg(target_os = "windows")]
pub(crate) async fn portable_service_ipc_handshake_as_client<T>(
    stream: &mut ConnectionTmpl<T>,
    token: &str,
) -> ResultType<()>
where
    T: AsyncRead + AsyncWrite + std::marker::Unpin,
{
    stream
        .send(&Data::DataPortableService(DataPortableService::AuthToken(
            token.to_owned(),
        )))
        .await?;
    match stream
        .next_timeout(PORTABLE_SERVICE_IPC_HANDSHAKE_TIMEOUT_MS)
        .await?
    {
        Some(Data::DataPortableService(DataPortableService::AuthResult(true))) => Ok(()),
        Some(Data::DataPortableService(DataPortableService::AuthResult(false))) => {
            bail!("portable service ipc handshake was rejected by server")
        }
        Some(_) | None => bail!("portable service ipc handshake returned an unexpected response"),
    }
}

#[cfg(target_os = "windows")]
pub(crate) async fn portable_service_ipc_handshake_as_server<T, F>(
    stream: &mut ConnectionTmpl<T>,
    mut validate_token: F,
) -> ResultType<()>
where
    T: AsyncRead + AsyncWrite + std::marker::Unpin,
    // 令牌校验器必须使用 `constant_time_ipc_token_eq` 或等效的
    // 定长比较；该握手属于权限边界的一部分。
    F: FnMut(&str) -> bool,
{
    let authorized = match stream
        .next_timeout(PORTABLE_SERVICE_IPC_HANDSHAKE_TIMEOUT_MS)
        .await?
    {
        Some(Data::DataPortableService(DataPortableService::AuthToken(token))) => {
            validate_token(&token)
        }
        Some(_) | None => false,
    };
    stream
        .send(&Data::DataPortableService(DataPortableService::AuthResult(
            authorized,
        )))
        .await?;
    if !authorized {
        bail!("portable service ipc handshake failed")
    }
    Ok(())
}

#[inline]
async fn connect_with_path(ms_timeout: u64, path: &str) -> ResultType<ConnectionTmpl<ConnClient>> {
    let client = timeout(ms_timeout, Endpoint::connect(path)).await??;
    Ok(ConnectionTmpl::new(client))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[inline]
fn select_server_uid_for_user_main_ipc(
    server_uids: &[u32],
    active_uid: Option<u32>,
    prefer_root: bool,
) -> ResultType<u32> {
    let mut server_uids = server_uids.to_vec();
    server_uids.sort_unstable();
    server_uids.dedup();

    match server_uids.as_slice() {
        [] => {
            if let Some(uid) = active_uid {
                // 如果没找到 `--server` 进程，但能识别出活动用户，
                // 仍然尝试活动用户，因为主进程也可能监听 "" IPC。
                return Ok(uid);
            } else {
                bail!("No --server process found for user main IPC")
            }
        }
        [uid] => return Ok(*uid),
        _ => {}
    }

    if prefer_root && server_uids.contains(&0) {
        return Ok(0);
    }
    if let Some(active_uid) = active_uid.filter(|uid| server_uids.contains(uid)) {
        return Ok(active_uid);
    }
    bail!("Multiple --server processes found for user main IPC");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn running_server_uids_for_current_exe() -> ResultType<Vec<u32>> {
    let current_exe = std::env::current_exe()?;
    let current_exe_path = std::fs::canonicalize(&current_exe)?;
    let current_pid = hbb_common::sysinfo::Pid::from_u32(std::process::id());
    let mut sys = hbb_common::sysinfo::System::new();
    sys.refresh_processes();
    let mut server_uids = Vec::new();
    for process in sys.processes().values() {
        if process.pid() == current_pid {
            continue;
        }
        if process.cmd().get(1).map_or(true, |arg| arg != "--server") {
            continue;
        }
        let Ok(process_path) = std::fs::canonicalize(process.exe()) else {
            continue;
        };
        if process_path != current_exe_path {
            continue;
        }
        let Some(uid) = process.user_id().map(|uid| **uid as u32) else {
            // root 的 CLI 管理命令需要一个稳定匹配的 `--server` 目标。
            // 如果该 key 进程在枚举期间发生竞态，让命令失败比
            // 静默跳过它更清晰；`--server` 不应频繁退出。
            bail!("Failed to read --server process uid");
        };
        server_uids.push(uid);
    }
    Ok(server_uids)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn user_main_ipc_server_uid() -> ResultType<u32> {
    let server_uids = running_server_uids_for_current_exe()?;
    #[cfg(target_os = "linux")]
    let prefer_root = crate::platform::linux::is_login_screen_wayland();
    #[cfg(target_os = "macos")]
    let prefer_root = false;
    select_server_uid_for_user_main_ipc(&server_uids, active_uid(), prefer_root)
}

pub async fn connect(ms_timeout: u64, postfix: &str) -> ResultType<ConnectionTmpl<ConnClient>> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let use_user_main_ipc = USE_USER_MAIN_IPC.with(|use_user_main| use_user_main.get());
        let is_root_main_ipc =
            unsafe { hbb_common::libc::geteuid() == 0 } && postfix.is_empty() && use_user_main_ipc;
        if is_root_main_ipc {
            let uid = user_main_ipc_server_uid()?;
            let path = Config::ipc_path_for_uid(uid, postfix);
            return connect_with_path(ms_timeout, &path).await;
        }
        let path = Config::ipc_path(postfix);
        return connect_with_path(ms_timeout, &path).await;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let path = Config::ipc_path(postfix);
        connect_with_path(ms_timeout, &path).await
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn connect_for_uid(
    ms_timeout: u64,
    uid: u32,
    postfix: &str,
) -> ResultType<ConnectionTmpl<ConnClient>> {
    let path = Config::ipc_path_for_uid(uid, postfix);
    let conn = connect_with_path(ms_timeout, &path).await?;
    #[cfg(target_os = "macos")]
    if postfix.is_empty()
        && !authorize_user_server_process(conn.peer_uid(), conn.peer_pid(), uid)
    {
        bail!("Rejected user IPC peer for uid {}", uid);
    }
    Ok(conn)
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
pub async fn start_pa() {
    use crate::audio_service::AUDIO_DATA_SIZE_U8;

    match new_listener("_pa").await {
        Ok(mut incoming) => {
            loop {
                if let Some(result) = incoming.next().await {
                    match result {
                        Ok(stream) => {
                            let mut stream = Connection::new(stream);
                            let mut device: String = "".to_owned();
                            if let Some(Ok(Some(Data::Config((_, Some(x)))))) =
                                stream.next_timeout2(1000).await
                            {
                                device = x;
                            }
                            if !device.is_empty() {
                                device = crate::platform::linux::get_pa_source_name(&device);
                            }
                            if device.is_empty() {
                                device = crate::platform::linux::get_pa_monitor();
                            }
                            if device.is_empty() {
                                continue;
                            }
                            let spec = pulse::sample::Spec {
                                format: pulse::sample::Format::F32le,
                                channels: 2,
                                rate: crate::platform::PA_SAMPLE_RATE,
                            };
                            log::info!("pa monitor: {:?}", device);
                            // systemctl --user status pulseaudio.service
                            let mut buf: Vec<u8> = vec![0; AUDIO_DATA_SIZE_U8];
                            match psimple::Simple::new(
                                None,                             // Use the default server
                                &crate::get_app_name(),           // Our application’s name
                                pulse::stream::Direction::Record, // We want a record stream
                                Some(&device),                    // Use the default device
                                "record",                         // Description of our stream
                                &spec,                            // Our sample format
                                None,                             // Use default channel map
                                None, // Use default buffering attributes
                            ) {
                                Ok(s) => loop {
                                    // 失效的 pulse 句柄会让每次读取都立刻失败，所以忽略
                                    // 该错误会让这个循环没有任何节流，从而烧掉一个核心。
                                    if let Err(err) = s.read(&mut buf) {
                                        log::error!("Failed to read audio data:{}", err);
                                        break;
                                    }
                                    {
                                        let out =
                                            if buf.iter().filter(|x| **x != 0).next().is_none() {
                                                vec![]
                                            } else {
                                                buf.clone()
                                            };
                                        if let Err(err) = stream.send_raw(out.into()).await {
                                            log::error!("Failed to send audio data:{}", err);
                                            break;
                                        }
                                    }
                                },
                                Err(err) => {
                                    log::error!("Could not create simple pulse: {}", err);
                                }
                            }
                        }
                        Err(err) => {
                            log::error!("Couldn't get pa client: {:?}", err);
                        }
                    }
                }
            }
        }
        Err(err) => {
            log::error!("Failed to start pa ipc server: {}", err);
        }
    }
}

pub struct ConnectionTmpl<T> {
    inner: Framed<T, BytesCodec>,
}

pub type Connection = ConnectionTmpl<Conn>;

impl<T> ConnectionTmpl<T>
where
    T: AsyncRead + AsyncWrite + std::marker::Unpin,
{
    pub fn new(conn: T) -> Self {
        Self {
            inner: Framed::new(conn, BytesCodec::new()),
        }
    }

    pub async fn send(&mut self, data: &Data) -> ResultType<()> {
        let v = serde_json::to_vec(data)?;
        self.inner.send(bytes::Bytes::from(v)).await?;
        Ok(())
    }

    async fn send_config(&mut self, name: &str, value: String) -> ResultType<()> {
        self.send(&Data::Config((name.to_owned(), Some(value))))
            .await
    }

    pub async fn next_timeout(&mut self, ms_timeout: u64) -> ResultType<Option<Data>> {
        Ok(timeout(ms_timeout, self.next()).await??)
    }

    pub async fn next_timeout2(&mut self, ms_timeout: u64) -> Option<ResultType<Option<Data>>> {
        if let Ok(x) = timeout(ms_timeout, self.next()).await {
            Some(x)
        } else {
            None
        }
    }

    pub async fn next(&mut self) -> ResultType<Option<Data>> {
        match self.inner.next().await {
            Some(res) => {
                let bytes = res?;
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    if let Ok(data) = serde_json::from_str::<Data>(s) {
                        return Ok(Some(data));
                    }
                }
                return Ok(None);
            }
            _ => {
                bail!("reset by the peer");
            }
        }
    }

    pub async fn send_raw(&mut self, data: Bytes) -> ResultType<()> {
        self.inner.send(data).await?;
        Ok(())
    }

    pub async fn next_raw(&mut self) -> ResultType<bytes::BytesMut> {
        match self.inner.next().await {
            Some(Ok(res)) => Ok(res),
            _ => {
                bail!("reset by the peer");
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_config(name: &str) -> ResultType<Option<String>> {
    get_config_async(name, 1_000).await
}

async fn get_config_async(name: &str, ms_timeout: u64) -> ResultType<Option<String>> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::Config((name.to_owned(), None))).await?;
    if let Some(Data::Config((name2, value))) = c.next_timeout(ms_timeout).await? {
        if name == name2 {
            return Ok(value);
        }
    }
    return Ok(None);
}

pub async fn set_config_async(name: &str, value: String) -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send_config(name, value).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_data(data: &Data) -> ResultType<()> {
    set_data_async(data).await
}

async fn set_data_async(data: &Data) -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(data).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_config(name: &str, value: String) -> ResultType<()> {
    set_config_async(name, value).await
}

pub fn update_temporary_password() -> ResultType<()> {
    set_config("temporary-password", "".to_owned())
}

fn apply_permanent_password_storage_and_salt_payload(payload: Option<&str>) -> ResultType<()> {
    let Some(payload) = payload else {
        return Ok(());
    };
    let Some((storage, salt)) = payload.split_once('\n') else {
        bail!("Invalid permanent-password-storage-and-salt payload");
    };

    Config::set_permanent_password_storage_for_sync(storage, salt)?;
    Ok(())
}

pub fn sync_permanent_password_storage_from_daemon() -> ResultType<()> {
    let v = get_config("permanent-password-storage-and-salt")?;
    apply_permanent_password_storage_and_salt_payload(v.as_deref())
}

async fn sync_permanent_password_storage_from_daemon_async() -> ResultType<()> {
    let ms_timeout = 1_000;
    let v = get_config_async("permanent-password-storage-and-salt", ms_timeout).await?;
    apply_permanent_password_storage_and_salt_payload(v.as_deref())
}

pub fn is_permanent_password_set() -> bool {
    match get_config("permanent-password-set") {
        Ok(Some(v)) => {
            let v = v.trim();
            return v == "Y";
        }
        Ok(None) => {
            // 无响应/无值（超时）。
        }
        Err(_) => {
            // 连接错误。
        }
    }
    log::warn!("Failed to query permanent password state from daemon");
    false
}

pub fn is_permanent_password_preset() -> bool {
    if let Ok(Some(v)) = get_config("permanent-password-is-preset") {
        let v = v.trim();
        return v == "Y";
    }
    false
}

pub fn get_fingerprint() -> String {
    get_config("fingerprint")
        .unwrap_or_default()
        .unwrap_or_default()
}

pub fn set_permanent_password(v: String) -> ResultType<()> {
    if Config::is_disable_change_permanent_password() {
        bail!("Changing permanent password is disabled");
    }
    if set_permanent_password_with_ack(v)? {
        Ok(())
    } else {
        bail!("Changing permanent password was rejected by daemon");
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_permanent_password_with_ack(v: String) -> ResultType<bool> {
    set_permanent_password_with_ack_async(v).await
}

async fn set_permanent_password_with_ack_async(v: String) -> ResultType<bool> {
    // 守护进程会在进程内应用配置，因此预期很快返回 ACK/NACK。
    let ms_timeout = 1_000;
    let mut c = connect(ms_timeout, "").await?;
    c.send_config("permanent-password", v).await?;
    if let Some(Data::Config((name2, Some(v)))) = c.next_timeout(ms_timeout).await? {
        if name2 == "permanent-password" {
            let v = v.trim();
            let ok = v == "Y";
            if ok {
                // 确保把哈希后的永久密码存储写入用户配置文件。
                // 该同步不得影响守护进程 ACK 的结果。
                if let Err(err) = sync_permanent_password_storage_from_daemon_async().await {
                    log::warn!("Failed to sync permanent password storage from daemon: {err}");
                }
            }
            return Ok(ok);
        }
    }
    Ok(false)
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn set_unlock_pin(v: String, translate: bool) -> ResultType<()> {
    let v = v.trim().to_owned();
    let min_len = 4;
    let max_len = crate::ui_interface::max_encrypt_len();
    let len = v.chars().count();
    if !v.is_empty() {
        if len < min_len {
            let err = if translate {
                crate::lang::translate(
                    "Requires at least {".to_string() + &format!("{min_len}") + "} characters",
                )
            } else {
                // 有时翻译后的内容无法在命令行正常显示
                format!("Requires at least {} characters", min_len)
            };
            bail!(err);
        }
        if len > max_len {
            bail!("No more than {max_len} characters");
        }
    }
    Config::set_unlock_pin(&v);
    set_config("unlock-pin", v)
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_unlock_pin() -> String {
    if let Ok(Some(v)) = get_config("unlock-pin") {
        Config::set_unlock_pin(&v);
        v
    } else {
        Config::get_unlock_pin()
    }
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_trusted_devices() -> String {
    if let Ok(Some(v)) = get_config("trusted-devices") {
        v
    } else {
        Config::get_trusted_devices_json()
    }
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn remove_trusted_devices(hwids: Vec<Bytes>) {
    Config::remove_trusted_devices(&hwids);
    allow_err!(set_data(&Data::RemoveTrustedDevices(hwids)));
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn clear_trusted_devices() {
    Config::clear_trusted_devices();
    allow_err!(set_data(&Data::ClearTrustedDevices));
}

pub fn get_id() -> String {
    // 空的 id 可能来自一个接管了主 IPC 的进程，其
    // 配置作用域尚无 id（例如某个用户界面进程变成了服务端，
    // 而已安装的服务正在重启）。要把它当作没有回答，
    // 否则下面就会采纳这个空 id 并清除本地的 id。
    if let Ok(Some(v)) = get_config("id") {
        if !v.is_empty() {
            // 同时更新 salt，这样下次重新安装不会导致首次自动登录失败
            if let Ok(Some(v2)) = get_config("salt") {
                Config::set_salt(&v2);
            }
            if v != Config::get_id() {
                Config::set_key_confirmed(false);
                Config::set_id(&v);
            }
            return v;
        }
    }
    Config::get_id()
}

pub async fn get_rendezvous_server(ms_timeout: u64) -> (String, Vec<String>) {
    if let Ok(Some(v)) = get_config_async("rendezvous_server", ms_timeout).await {
        let mut urls = v.split(",");
        let a = urls.next().unwrap_or_default().to_owned();
        let b: Vec<String> = urls.map(|x| x.to_owned()).collect();
        (a, b)
    } else {
        (
            Config::get_rendezvous_server(),
            Config::get_rendezvous_servers(),
        )
    }
}

async fn get_options_(ms_timeout: u64) -> ResultType<HashMap<String, String>> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::Options(None)).await?;
    if let Some(Data::Options(Some(value))) = c.next_timeout(ms_timeout).await? {
        Config::set_options(value.clone());
        Ok(value)
    } else {
        Ok(Config::get_options())
    }
}

pub async fn get_options_async() -> HashMap<String, String> {
    get_options_(1000).await.unwrap_or(Config::get_options())
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_options() -> HashMap<String, String> {
    get_options_async().await
}

pub async fn get_option_async(key: &str) -> String {
    if let Some(v) = get_options_async().await.get(key) {
        v.clone()
    } else {
        "".to_owned()
    }
}

pub fn set_option(key: &str, value: &str) {
    let mut options = get_options();
    if value.is_empty() {
        options.remove(key);
    } else {
        options.insert(key.to_owned(), value.to_owned());
    }
    set_options(options).ok();
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_options(value: HashMap<String, String>) -> ResultType<()> {
    let _nat = CheckTestNatType::new();
    if let Ok(mut c) = connect(1000, "").await {
        c.send(&Data::Options(Some(value.clone()))).await?;
        // 不要把下面的内容放到 connect 之前，因为我们需要检查 should_exit
        c.next_timeout(1000).await.ok();
    }
    Config::set_options(value);
    Ok(())
}

#[inline]
async fn get_nat_type_(ms_timeout: u64) -> ResultType<i32> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::NatType(None)).await?;
    if let Some(Data::NatType(Some(value))) = c.next_timeout(ms_timeout).await? {
        Config::set_nat_type(value);
        Ok(value)
    } else {
        Ok(Config::get_nat_type())
    }
}

pub async fn get_nat_type(ms_timeout: u64) -> i32 {
    get_nat_type_(ms_timeout)
        .await
        .unwrap_or(Config::get_nat_type())
}

pub async fn get_rendezvous_servers(ms_timeout: u64) -> Vec<String> {
    if let Ok(Some(v)) = get_config_async("rendezvous_servers", ms_timeout).await {
        return v.split(',').map(|x| x.to_owned()).collect();
    }
    return Config::get_rendezvous_servers();
}

#[inline]
async fn get_socks_(ms_timeout: u64) -> ResultType<Option<config::Socks5Server>> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::Socks(None)).await?;
    if let Some(Data::Socks(value)) = c.next_timeout(ms_timeout).await? {
        Config::set_socks(value.clone());
        Ok(value)
    } else {
        Ok(Config::get_socks())
    }
}

pub async fn get_socks_async(ms_timeout: u64) -> Option<config::Socks5Server> {
    get_socks_(ms_timeout).await.unwrap_or(Config::get_socks())
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_socks() -> Option<config::Socks5Server> {
    get_socks_async(1_000).await
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_socks(value: config::Socks5Server) -> ResultType<()> {
    let _nat = CheckTestNatType::new();
    Config::set_socks(if value.proxy.is_empty() {
        None
    } else {
        Some(value.clone())
    });
    connect(1_000, "")
        .await?
        .send(&Data::Socks(Some(value)))
        .await?;
    Ok(())
}

async fn get_socks_ws_(ms_timeout: u64) -> ResultType<(Option<config::Socks5Server>, String)> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::SocksWs(None)).await?;
    if let Some(Data::SocksWs(Some(value))) = c.next_timeout(ms_timeout).await? {
        Config::set_socks(value.0.clone());
        Config::set_option(OPTION_ALLOW_WEBSOCKET.to_string(), value.1.clone());
        Ok(*value)
    } else {
        Ok((
            Config::get_socks(),
            Config::get_option(OPTION_ALLOW_WEBSOCKET),
        ))
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_socks_ws() -> (Option<config::Socks5Server>, String) {
    get_socks_ws_(1_000).await.unwrap_or((
        Config::get_socks(),
        Config::get_option(OPTION_ALLOW_WEBSOCKET),
    ))
}

pub fn get_proxy_status() -> bool {
    Config::get_socks().is_some()
}
#[tokio::main(flavor = "current_thread")]
pub async fn test_rendezvous_server() -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(&Data::TestRendezvousServer).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn notify_deployed() -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(&Data::Deployed).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn send_url_scheme(url: String) -> ResultType<()> {
    connect(1_000, "_url")
        .await?
        .send(&Data::UrlLink(url))
        .await?;
    Ok(())
}

// 向 ipc 发出 `close` 事件。
pub fn close_all_instances() -> ResultType<bool> {
    match crate::ipc::send_url_scheme(IPC_ACTION_CLOSE.to_owned()) {
        Ok(_) => Ok(true),
        Err(err) => Err(err),
    }
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
pub async fn connect_to_user_session(usid: Option<u32>) -> ResultType<()> {
    let mut stream = crate::ipc::connect_service(1000).await?;
    timeout(1000, stream.send(&crate::ipc::Data::UserSid(usid))).await??;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn notify_server_to_check_hwcodec() -> ResultType<()> {
    connect(1_000, "").await?.send(&&Data::CheckHwcodec).await?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub async fn get_port_forward_session_count(ms_timeout: u64) -> ResultType<usize> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::PortForwardSessionCount(None)).await?;
    if let Some(Data::PortForwardSessionCount(Some(count))) = c.next_timeout(ms_timeout).await? {
        return Ok(count);
    }
    bail!("Failed to get port forward session count");
}

#[cfg(feature = "hwcodec")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tokio::main(flavor = "current_thread")]
pub async fn get_hwcodec_config_from_server() -> ResultType<()> {
    if !scrap::codec::enable_hwcodec_option() || scrap::hwcodec::HwCodecConfig::already_set() {
        return Ok(());
    }
    let mut c = connect(50, "").await?;
    c.send(&Data::HwCodecConfig(None)).await?;
    if let Some(Data::HwCodecConfig(v)) = c.next_timeout(50).await? {
        match v {
            Some(v) => {
                scrap::hwcodec::HwCodecConfig::set(v);
                return Ok(());
            }
            None => {
                bail!("hwcodec config is none");
            }
        }
    }
    bail!("failed to get hwcodec config");
}

#[cfg(feature = "hwcodec")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn client_get_hwcodec_config_thread(wait_sec: u64) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if !crate::platform::is_installed()
        || !scrap::codec::enable_hwcodec_option()
        || scrap::hwcodec::HwCodecConfig::already_set()
    {
        return;
    }
    ONCE.call_once(move || {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let mut intervals: Vec<u64> = vec![wait_sec, 3, 3, 6, 9];
            for i in intervals.drain(..) {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(i));
                }
                if get_hwcodec_config_from_server().is_ok() {
                    break;
                }
            }
        });
    });
}

#[cfg(feature = "hwcodec")]
#[tokio::main(flavor = "current_thread")]
pub async fn hwcodec_process() {
    let s = scrap::hwcodec::check_available_hwcodec();
    for _ in 0..5 {
        match crate::ipc::connect(1000, "").await {
            Ok(mut conn) => {
                match conn
                    .send(&crate::ipc::Data::HwCodecConfig(Some(s.clone())))
                    .await
                {
                    Ok(()) => {
                        log::info!("send ok");
                        break;
                    }
                    Err(e) => {
                        log::error!("send failed: {e:?}");
                    }
                }
            }
            Err(e) => {
                log::error!("connect failed: {e:?}");
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_wayland_screencast_restore_token(key: String) -> ResultType<String> {
    let v = handle_wayland_screencast_restore_token(key, "get".to_owned()).await?;
    Ok(v.unwrap_or_default())
}

#[tokio::main(flavor = "current_thread")]
pub async fn clear_wayland_screencast_restore_token(key: String) -> ResultType<bool> {
    if let Some(v) = handle_wayland_screencast_restore_token(key, "clear".to_owned()).await? {
        return Ok(v.is_empty());
    }
    return Ok(false);
}

#[cfg(all(
    feature = "flutter",
    not(any(target_os = "android", target_os = "ios"))
))]
#[tokio::main(flavor = "current_thread")]
pub async fn update_controlling_session_count(count: usize) -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(&Data::ControllingSessionCount(count)).await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
pub async fn get_terminal_session_count() -> ResultType<usize> {
    let timeout_ms = 1_000;
    let effective_uid = unsafe { hbb_common::libc::geteuid() as u32 };
    let candidate_uids = terminal_count_candidate_uids(effective_uid);
    let mut last_err: Option<anyhow::Error> = None;
    for candidate_uid in candidate_uids {
        let socket_path = Config::ipc_path_for_uid(candidate_uid, "");
        let connect_result = timeout(timeout_ms, Endpoint::connect(&socket_path))
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "Timeout connecting to terminal ipc at {}: {}",
                    socket_path,
                    err
                )
            });
        let connection = match connect_result {
            Ok(Ok(connection)) => connection,
            Ok(Err(err)) => {
                last_err = Some(anyhow::anyhow!(
                    "Failed to connect to terminal ipc at {}: {}",
                    socket_path,
                    err
                ));
                continue;
            }
            Err(err) => {
                last_err = Some(err);
                continue;
            }
        };
        let mut ipc_conn = ConnectionTmpl::new(connection);
        if let Err(err) = ipc_conn.send(&Data::TerminalSessionCount(0)).await {
            last_err = Some(anyhow::anyhow!(
                "Failed to request terminal session count via ipc at {}: {}",
                socket_path,
                err
            ));
            continue;
        }
        match ipc_conn.next_timeout(timeout_ms).await {
            Ok(Some(Data::TerminalSessionCount(session_count))) => {
                return Ok(session_count);
            }
            Ok(None) => {
                last_err = Some(anyhow::anyhow!(
                    "Invalid response when requesting terminal session count via ipc at {}",
                    socket_path
                ));
            }
            Ok(other) => {
                last_err = Some(anyhow::anyhow!(
                    "Unexpected response when requesting terminal session count via ipc at {}: {:?}",
                    socket_path,
                    other.map(|v| std::mem::discriminant(&v))
                ));
            }
            Err(err) => {
                last_err = Some(anyhow::anyhow!(
                    "Failed to read terminal session count via ipc at {}: {}",
                    socket_path,
                    err
                ));
            }
        }
    }
    if let Some(err) = last_err {
        Err(err.into())
    } else {
        Ok(0)
    }
}

async fn handle_wayland_screencast_restore_token(
    key: String,
    value: String,
) -> ResultType<Option<String>> {
    let ms_timeout = 1_000;
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::WaylandScreencastRestoreToken((key, value)))
        .await?;
    if let Some(Data::WaylandScreencastRestoreToken((_key, v))) = c.next_timeout(ms_timeout).await?
    {
        return Ok(Some(v));
    }
    return Ok(None);
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_install_option(k: String, v: String) -> ResultType<()> {
    if let Ok(mut c) = connect(1000, "").await {
        c.send(&&Data::InstallOption(Some((k, v)))).await?;
        // 不要把下面的内容放到 connect 之前，因为我们需要检查 should_exit
        c.next_timeout(1000).await.ok();
    }
    Ok(())
}

#[cfg(test)]
mod test {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_select_server_uid_uses_active_uid_when_no_server_found() {
        assert_eq!(
            select_server_uid_for_user_main_ipc(&[], Some(501), false).unwrap(),
            501
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_select_server_uid_uses_single_server_uid() {
        assert_eq!(
            select_server_uid_for_user_main_ipc(&[501], None, false).unwrap(),
            501
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_select_server_uid_prefers_active_uid_with_multiple_servers() {
        assert_eq!(
            select_server_uid_for_user_main_ipc(&[0, 501], Some(501), false).unwrap(),
            501
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_select_server_uid_prefers_root_on_wayland_login_screen() {
        assert_eq!(
            select_server_uid_for_user_main_ipc(&[0, 501], Some(501), true).unwrap(),
            0
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_select_server_uid_fails_when_multiple_servers_are_ambiguous() {
        assert!(select_server_uid_for_user_main_ipc(&[501, 502], None, false).is_err());
    }
}
