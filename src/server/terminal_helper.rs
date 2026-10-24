//! 终端辅助进程
//!
//! 本模块实现一个以已登录用户身份运行的辅助进程，用于创建
//! ConPTY + Shell。这是必要的，因为当 ConPTY 由其他用户（SYSTEM 服务）创建时，
//! ConPTY 与 CreateProcessAsUserW 存在兼容性问题。
//!
//! 架构：
//! ```
//! SYSTEM 服务（terminal_service.rs）
//!     |
//!     +-- CreateProcessAsUserW --> 终端辅助进程（本模块，以用户身份运行）
//!     |                                |
//!     |                                +-- CreateProcessW + ConPTY --> Shell
//!     |                                |
//!     +-- 命名管道 <----------------+
//! ```
//!
//! 本模块还包含 terminal_service.rs 使用的 Windows 专用工具函数：
//! - 命名管道的创建与连接
//! - 用户令牌与 SID 处理
//! - 辅助进程启动

use hbb_common::{
    anyhow::{anyhow, Context, Result},
    log,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::{
    ffi::{c_void, OsStr},
    fs::File,
    io::{Read, Write},
    os::windows::{ffi::OsStrExt, io::FromRawHandle, raw::HANDLE as RawHandle},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            CloseHandle, LocalFree, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
            INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        },
        Security::{
            Authorization::{
                SetEntriesInAclW, EXPLICIT_ACCESS_W, SET_ACCESS, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
                TRUSTEE_W,
            },
            CreateWellKnownSid, GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor,
            SetSecurityDescriptorDacl, TokenUser, WinLocalSystemSid, ACE_FLAGS, ACL,
            PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_USER,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ALL_ACCESS, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
            FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Threading::{
                CreateEventW, CreateProcessAsUserW, WaitForSingleObject, CREATE_NO_WINDOW,
                CREATE_UNICODE_ENVIRONMENT, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
                STARTUPINFOW,
            },
            IO::{GetOverlappedResult, OVERLAPPED},
        },
    },
};

// 重新导出 terminal_service.rs 所需的类型
pub use windows::Win32::{
    Foundation::{
        CloseHandle as WinCloseHandle, HANDLE as WinHANDLE, WAIT_OBJECT_0 as WIN_WAIT_OBJECT_0,
    },
    System::Threading::{
        GetExitCodeProcess as WinGetExitCodeProcess, TerminateProcess as WinTerminateProcess,
        WaitForSingleObject as WinWaitForSingleObject,
    },
};

/// 供跨模块使用的用户令牌包装类型。
///
/// 使用 newtype 模式以保证类型安全。内部值为 `usize`，以匹配
/// 平台的指针大小（x86 上为 32 位，x64 上为 64 位）。
/// Windows HANDLE 定义为 `*mut c_void`，其大小与 `usize` 相同。
///
/// # 设计说明
/// 该类型在 Windows 下于此处（terminal_helper.rs）定义，而在
/// 非 Windows 平台则于 terminal_service.rs 中定义。这样可避免循环
/// 依赖，同时保持各平台 API 的一致性。
/// 两处定义必须具有完全相同的公共 API（new、as_raw 方法）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserToken(pub usize);

impl UserToken {
    /// 从原始句柄值创建一个新的 UserToken。
    pub fn new(handle: usize) -> Self {
        Self(handle)
    }

    /// 获取原始句柄值。
    pub fn as_raw(&self) -> usize {
        self.0
    }
}

// Windows 管道访问模式常量（windows crate 未导出）
const PIPE_ACCESS_INBOUND: u32 = 0x00000001;
const PIPE_ACCESS_OUTBOUND: u32 = 0x00000002;

// 命名管道配置常量
const PIPE_BUFFER_SIZE: u32 = 65536; // 64KB for better throughput with large terminal output
const PIPE_DEFAULT_TIMEOUT_MS: u32 = 5000;
/// 等待辅助进程连接到管道的超时时间
pub const PIPE_CONNECTION_TIMEOUT_MS: u32 = 10000;

/// 辅助协议的消息类型常量。
/// 用于区分终端数据与控制命令。
/// 注意：使用非零值以便调试（0x00 可能表示未初始化的内存）。
pub const MSG_TYPE_DATA: u8 = 0x01;
pub const MSG_TYPE_RESIZE: u8 = 0x02;

/// 消息头大小：1 字节类型 + 4 字节长度
pub const MSG_HEADER_SIZE: usize = 5;

/// 最大负载大小，用于防止恶意消息造成拒绝服务。
/// 对于任何合法的终端数据，16MB 应该都绰绰有余。
const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// 以毫秒计的超时时间，用于在强制终止前等待辅助进程优雅退出。
/// 使用 500ms，为辅助进程留出足够的清理时间，尤其是在系统高负载时。
pub const HELPER_GRACEFUL_EXIT_TIMEOUT_MS: u64 = 500;

/// 已启动的辅助进程的信息。
/// 同时包含进程句柄和 PID，用于跟踪与状态检查。
#[derive(Debug)]
pub struct HelperProcessInfo {
    /// 用于终止和等待的进程句柄
    pub handle: HANDLE,
    /// 用于日志记录和状态显示的进程 ID
    pub pid: u32,
}

/// 实现了 Send 的 Windows HANDLE 包装类型。
/// 这是安全的，因为 Windows HANDLE 在各线程间都是有效的。
/// 注意：我们只实现 Send，不实现 Sync。该句柄由
/// TerminalSession 中的 Mutex 保护，因此并发访问由那里控制。
///
/// # 所有权与清理
/// 该类型有意不实现 Drop。该句柄由
/// `TerminalSession` 持有，并在 `TerminalSession::close_internal()` 中显式关闭，
/// 位置在优雅关闭逻辑之后（等待辅助进程退出，必要时强制终止）。
/// 在这里实现 Drop 会干扰该清理流程。
#[derive(Debug)]
pub struct SendableHandle(HANDLE);

impl SendableHandle {
    /// 从原始 HANDLE 创建一个新的 SendableHandle。
    pub fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// 获取原始 HANDLE 值。
    pub fn as_raw(&self) -> HANDLE {
        self.0
    }
}

unsafe impl Send for SendableHandle {}

/// Windows HANDLE 的 RAII 包装类型，在 drop 时自动关闭句柄。
/// 这确保了即使发生错误或代码路径分叉，资源也能被正确清理。
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// 从原始 HANDLE 创建一个新的 OwnedHandle。
    /// 该 OwnedHandle 被 drop 时，句柄将被关闭。
    pub fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// 消耗该 OwnedHandle 并返回原始 HANDLE，而不关闭它。
    /// 当将所有权转移给其他资源（例如 File）时使用此方法。
    pub fn into_raw(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self); // Prevent Drop from closing the handle
        handle
    }

    /// 获取原始 HANDLE 值。
    pub fn as_raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 辅助进程的 RAII 守卫，在 drop 时终止该进程。
/// 这可以避免在管道连接失败或发生其他错误时辅助进程泄漏。
///
/// 与 OwnedHandle（仅关闭句柄）不同，该守卫：
/// 1. 使用 TerminateProcess 终止进程
/// 2. 随后关闭句柄
///
/// 使用 `disarm()` 可阻止终止操作 —— 当辅助进程成功移交给
/// 终端会话以进行正确的生命周期管理时。
pub struct HelperProcessGuard {
    handle: HANDLE,
    pid: u32,
    armed: bool,
}

impl HelperProcessGuard {
    /// 为辅助进程创建一个新的守卫。
    pub fn new(handle: HANDLE, pid: u32) -> Self {
        Self {
            handle,
            pid,
            armed: true,
        }
    }

    /// 获取原始进程 HANDLE。
    pub fn as_raw(&self) -> HANDLE {
        self.handle
    }

    /// 获取进程 ID。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 解除守卫武装并返回原始 HANDLE。
    /// 调用后，守卫在 drop 时将不会终止该进程。
    /// 在成功将辅助进程移交给会话管理时使用此方法。
    pub fn disarm(self) -> HANDLE {
        let handle = self.handle;
        std::mem::forget(self); // Prevent Drop from running
        handle
    }
}

impl Drop for HelperProcessGuard {
    fn drop(&mut self) {
        if self.armed && self.handle != INVALID_HANDLE_VALUE && !self.handle.is_invalid() {
            log::warn!(
                "HelperProcessGuard: terminating leaked helper process (PID {})",
                self.pid
            );
            unsafe {
                // 先终止进程
                let _ = WinTerminateProcess(self.handle, 1);
                // 然后关闭句柄
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// 为辅助协议编码一条消息。
/// 格式：[type: u8][length: u32 LE][payload: bytes]
pub fn encode_helper_message(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(MSG_HEADER_SIZE + payload.len());
    msg.push(msg_type);
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(payload);
    msg
}

/// 为辅助协议编码一条尺寸调整消息。
/// 负载：rows (u16 LE) + cols (u16 LE)
pub fn encode_resize_message(rows: u16, cols: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&rows.to_le_bytes());
    payload.extend_from_slice(&cols.to_le_bytes());
    encode_helper_message(MSG_TYPE_RESIZE, &payload)
}

/// 获取 Windows 的默认 shell。
pub fn get_default_shell() -> String {
    // 先尝试 PowerShell Core（仅使用绝对路径）
    let pwsh_paths = [
        "pwsh.exe",
        r"C:\Program Files\PowerShell\7\pwsh.exe",
        r"C:\Program Files\PowerShell\6\pwsh.exe",
    ];

    for path in &pwsh_paths {
        if std::path::Path::new(path).exists() {
            log::debug!("Found PowerShell Core: {}", path);
            return path.to_string();
        }
    }

    // 尝试 Windows PowerShell
    let powershell_path = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    if std::path::Path::new(powershell_path).exists() {
        return powershell_path.to_string();
    }

    // 回退到 cmd.exe
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
}

fn utf8_shell_args(shell: &str) -> Vec<String> {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
        .to_ascii_lowercase();

    if name == "cmd.exe" || name == "cmd" {
        return vec!["/K".to_string(), "chcp 65001 >NUL".to_string()];
    }

    if name == "pwsh.exe" || name == "pwsh" || name == "powershell.exe" {
        return vec![
            "-NoLogo".to_string(),
            "-NoExit".to_string(),
            "-Command".to_string(),
            "chcp.com 65001 > $null; [Console]::InputEncoding = [System.Text.Encoding]::UTF8; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8".to_string(),
        ];
    }

    Vec::new()
}

pub fn configure_utf8_shell_command(shell: &str, cmd: &mut CommandBuilder) {
    for arg in utf8_shell_args(shell) {
        cmd.arg(arg);
    }
}

/// 从令牌获取用户的 SID。
/// 返回包含 SID 字节的 Vec<u8>。
pub fn get_user_sid_from_token(user_token: UserToken) -> Result<Vec<u8>> {
    let token_handle = HANDLE(user_token.as_raw() as _);

    // 首次调用以获取所需的缓冲区大小
    let mut return_length = 0u32;
    let _ = unsafe { GetTokenInformation(token_handle, TokenUser, None, 0, &mut return_length) };

    if return_length == 0 {
        return Err(anyhow!(
            "Failed to get token information size: {}",
            std::io::Error::last_os_error()
        ));
    }

    // 分配缓冲区并获取令牌信息
    let mut buffer = vec![0u8; return_length as usize];
    unsafe {
        GetTokenInformation(
            token_handle,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut c_void),
            return_length,
            &mut return_length,
        )
        .map_err(|e| anyhow!("Failed to get token information: {}", e))?;
    }

    // 从 TOKEN_USER 结构中提取 SID
    let token_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let sid_ptr = token_user.User.Sid;

    // 获取 SID 长度并复制到自有缓冲区
    let sid_length = unsafe { GetLengthSid(sid_ptr) };

    if sid_length == 0 {
        return Err(anyhow!("Invalid SID length"));
    }

    let mut sid_buffer = vec![0u8; sid_length as usize];
    unsafe {
        ptr::copy_nonoverlapping(
            sid_ptr.0 as *const u8,
            sid_buffer.as_mut_ptr(),
            sid_length as usize,
        );
    }

    Ok(sid_buffer)
}

/// 创建一个仅允许 SYSTEM 和特定用户访问的受限 DACL。
/// 返回指向 ACL 的指针，该 ACL 必须通过 LocalFree 释放。
///
/// # 安全性
///
/// 此函数调用是安全的，但内部包含依赖以下保证的 unsafe 代码：
/// 指针生命周期保证：
///
/// - `user_sid` 切片必须包含有效的 SID 二进制数据。
/// - 内部会将指向 `system_sid_buffer`（栈分配）和 `user_sid` 的原始指针
///   存入 `TRUSTEE_W.ptstrName` 字段。这些指针仅在
///   `SetEntriesInAclW` 调用期间使用，该调用发生在任一缓冲区离开作用域之前。
/// - 返回的 ACL 指针由 Windows 分配，必须通过 `LocalFree` 释放。
pub fn create_restricted_dacl(user_sid: &[u8]) -> Result<*mut c_void> {
    // 创建 SYSTEM SID（知名 SID：S-1-5-18）
    // 安全性：该缓冲区必须比引用它的 TRUSTEE_W 结构活得更久
    let mut system_sid_buffer = vec![0u8; 64]; // Max SID size
    let mut system_sid_size = system_sid_buffer.len() as u32;
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None, // No domain SID
            Some(PSID(system_sid_buffer.as_mut_ptr() as *mut c_void)),
            &mut system_sid_size,
        )
        .map_err(|e| anyhow!("Failed to create SYSTEM SID: {}", e))?;
    }

    // 为 SYSTEM 和用户构建 EXPLICIT_ACCESS 条目
    // 安全性：下面的 ptstrName 指针引用了 system_sid_buffer 和 user_sid。
    // 在 SetEntriesInAclW 返回之前，这些缓冲区必须保持有效。
    let mut explicit_access: [EXPLICIT_ACCESS_W; 2] = unsafe { std::mem::zeroed() };

    // 条目 0：SYSTEM - 完全访问
    explicit_access[0].grfAccessPermissions = FILE_ALL_ACCESS.0;
    explicit_access[0].grfAccessMode = SET_ACCESS;
    explicit_access[0].grfInheritance = ACE_FLAGS(0); // No inheritance for pipes
    explicit_access[0].Trustee = TRUSTEE_W {
        pMultipleTrustee: ptr::null_mut(),
        MultipleTrusteeOperation: Default::default(),
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: PWSTR::from_raw(system_sid_buffer.as_ptr() as *mut u16),
    };

    // 条目 1：用户 - 完全访问
    explicit_access[1].grfAccessPermissions = FILE_ALL_ACCESS.0;
    explicit_access[1].grfAccessMode = SET_ACCESS;
    explicit_access[1].grfInheritance = ACE_FLAGS(0); // No inheritance for pipes
                                                      // 安全性：当 TrusteeForm 为 TRUSTEE_IS_SID 时，ptstrName 会被解释为 PSID
                                                      // 指针，而非字符串指针。Windows API 会根据 TrusteeForm 将该字段用于不同
                                                      // 用途。user_sid 中的 SID 二进制数据在
                                                      // 本次函数调用期间（直到 SetEntriesInAclW 返回）有效。
    explicit_access[1].Trustee = TRUSTEE_W {
        pMultipleTrustee: ptr::null_mut(),
        MultipleTrusteeOperation: Default::default(),
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: PWSTR::from_raw(user_sid.as_ptr() as *mut u16),
    };

    // 根据显式访问条目创建 ACL
    // 该调用返回后，不再需要 system_sid_buffer 和 user_sid
    let mut new_acl: *mut ACL = ptr::null_mut();
    let result = unsafe {
        SetEntriesInAclW(
            Some(&explicit_access),
            None, // No existing ACL
            &mut new_acl,
        )
    };

    if result.0 != 0 {
        return Err(anyhow!(
            "SetEntriesInAclW failed with error code: {}",
            result.0
        ));
    }

    if new_acl.is_null() {
        return Err(anyhow!("SetEntriesInAclW returned null ACL"));
    }

    Ok(new_acl as *mut c_void)
}

/// 创建一个带受限 DACL 的命名管道。
/// 只有 SYSTEM 和指定的用户可以访问该管道。
///
/// # 参数
/// * `pipe_name` - 要创建的管道名称
/// * `for_input` - 若服务向该管道写入（辅助进程读取）则为 true，否则为 false
/// * `user_token` - 创建受限 DACL 所需的用户令牌
///
/// # 安全性
///
/// 受限 DACL 将管道访问权限限制为：
/// - SYSTEM 账户（服务）
/// - 提供令牌的那个特定用户（辅助进程）
///
/// 此函数需要有效的 user_token，若 DACL 创建失败则该函数会失败，
/// 而不会回退到安全性更低的 NULL DACL。
pub fn create_named_pipe_server(
    pipe_name: &str,
    for_input: bool,
    user_token: UserToken,
) -> Result<HANDLE> {
    // 在 x64 上 SECURITY_DESCRIPTOR 的最小长度为 40 字节。
    const SD_BUFFER_SIZE: usize = 64;
    const _: () = assert!(
        SD_BUFFER_SIZE >= 40,
        "SD_BUFFER_SIZE must be at least 40 bytes for SECURITY_DESCRIPTOR"
    );

    let mut sd_buffer = [0u8; SD_BUFFER_SIZE];
    let sd_ptr = PSECURITY_DESCRIPTOR(sd_buffer.as_mut_ptr() as *mut c_void);

    // 初始化安全描述符
    unsafe {
        InitializeSecurityDescriptor(sd_ptr, 1)
            .map_err(|e| anyhow!("Failed to initialize security descriptor: {}", e))?;
    }

    // 创建受限 DACL —— 如果失败则直接报错（不回退到 NULL DACL）
    let user_sid = get_user_sid_from_token(user_token)
        .context("Failed to get user SID from token for pipe DACL")?;
    let acl_ptr =
        create_restricted_dacl(&user_sid).context("Failed to create restricted DACL for pipe")?;

    log::debug!("Created restricted DACL for pipe: {}", pipe_name);

    // 在安全描述符上设置 DACL
    unsafe {
        SetSecurityDescriptorDacl(sd_ptr, true, Some(acl_ptr as *const _ as *const _), false)
            .map_err(|e| {
                // 出错时清理 ACL（忽略结果 —— 清理是尽力而为，原始错误优先）
                let _ = LocalFree(Some(HLOCAL(acl_ptr)));
                anyhow!("Failed to set restricted DACL: {}", e)
            })?;
    }

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd_buffer.as_mut_ptr() as *mut c_void,
        bInheritHandle: false.into(),
    };

    let wide_name: Vec<u16> = OsStr::new(pipe_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let access_mode = if for_input {
        FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED.0)
    } else {
        FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED.0)
    };

    log::debug!(
        "Creating named pipe: {} (for_input={}, restricted_dacl=true)",
        pipe_name,
        for_input
    );

    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR::from_raw(wide_name.as_ptr()),
            access_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1, // max instances
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            PIPE_DEFAULT_TIMEOUT_MS,
            Some(&sa),
        )
    };

    // 管道创建后清理 ACL（安全描述符已应用）
    // 忽略结果：管道已创建，LocalFree 失败不构成关键问题
    unsafe {
        let _ = LocalFree(Some(HLOCAL(acl_ptr)));
    }

    if handle == INVALID_HANDLE_VALUE {
        return Err(anyhow!(
            "Failed to create named pipe {}: {}",
            pipe_name,
            std::io::Error::last_os_error()
        ));
    }

    log::debug!("Named pipe created: {}", pipe_name);
    Ok(handle)
}

/// 等待客户端连接到命名管道，带超时。
///
/// # 所有权
/// 此函数通过 OwnedHandle **接管** `pipe_handle` 的所有权：
/// - 成功时：句柄被取出并包装进 `File`。
/// - 失败时：OwnedHandle drop 时句柄会自动关闭。
pub fn wait_for_pipe_connection(
    pipe_handle: OwnedHandle,
    pipe_name: &str,
    timeout_ms: u32,
) -> Result<File> {
    log::debug!("Waiting for pipe connection: {}", pipe_name);

    // 为重叠 I/O 创建事件（同样用 OwnedHandle 包装以实现 RAII）
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|e| anyhow!("Failed to create event for pipe connection: {}", e))?;
    let event_handle = OwnedHandle::new(event);

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event_handle.as_raw();

    let result = unsafe { ConnectNamedPipe(pipe_handle.as_raw(), Some(&mut overlapped)) };
    if result.is_err() {
        let err = std::io::Error::last_os_error();
        let err_code = err.raw_os_error().unwrap_or(0);

        // ERROR_PIPE_CONNECTED 表示客户端已连接，这没问题
        if err_code == ERROR_PIPE_CONNECTED.0 as i32 {
            log::debug!("Pipe already connected: {}", pipe_name);
            return Ok(unsafe { File::from_raw_handle(pipe_handle.into_raw().0 as RawHandle) });
        }

        // ERROR_IO_PENDING 表示需要等待
        if err_code == ERROR_IO_PENDING.0 as i32 {
            log::debug!("Pipe connection pending, waiting with timeout...");
            let wait_result = unsafe { WaitForSingleObject(event_handle.as_raw(), timeout_ms) };

            if wait_result != WAIT_OBJECT_0 {
                log::error!("Timeout waiting for pipe connection: {}", pipe_name);
                return Err(anyhow!(
                    "Timeout waiting for pipe connection: {}",
                    pipe_name
                ));
            }

            // 检查连接是否成功
            let mut bytes_transferred = 0u32;
            let overlapped_result = unsafe {
                GetOverlappedResult(
                    pipe_handle.as_raw(),
                    &overlapped,
                    &mut bytes_transferred,
                    false,
                )
            };
            if overlapped_result.is_err() {
                let err = std::io::Error::last_os_error();
                log::error!("Failed to complete pipe connection {}: {}", pipe_name, err);
                return Err(anyhow!(
                    "Failed to complete pipe connection {}: {}",
                    pipe_name,
                    err
                ));
            }

            log::debug!("Pipe connected: {}", pipe_name);
        } else {
            log::error!("Failed to connect named pipe {}: {}", pipe_name, err);
            return Err(anyhow!(
                "Failed to connect named pipe {}: {}",
                pipe_name,
                err
            ));
        }
    } else {
        log::debug!("Pipe connected immediately: {}", pipe_name);
    }

    // 成功：将管道所有权转移给 File，event_handle 被 drop
    Ok(unsafe { File::from_raw_handle(pipe_handle.into_raw().0 as RawHandle) })
}

/// 使用提供的令牌，以已登录用户身份启动终端辅助进程。
/// 辅助进程创建 ConPTY 和 shell，并通过命名管道通信。
/// 这里直接使用用户令牌调用 CreateProcessAsUserW，能生效是因为
/// 辅助进程本身不需要 ConPTY —— 它会在内部创建 ConPTY。
///
/// 返回包含进程句柄和 PID 的 HelperProcessInfo。

/// 用于清理环境块的 RAII 守卫。
/// 确保即使发生错误也会调用 DestroyEnvironmentBlock。
struct EnvironmentBlockGuard {
    ptr: *mut c_void,
}

impl Drop for EnvironmentBlockGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                // 忽略结果：清理期间 DestroyEnvironmentBlock 失败不构成关键问题
                let _ = DestroyEnvironmentBlock(self.ptr);
            }
        }
    }
}

pub fn launch_terminal_helper_with_token(
    user_token: UserToken,
    input_pipe_name: &str,
    output_pipe_name: &str,
    terminal_id: i32,
    rows: u16,
    cols: u16,
) -> Result<HelperProcessInfo> {
    let exe_path =
        std::env::current_exe().map_err(|e| anyhow!("Failed to get current exe path: {}", e))?;

    // 构建命令行参数（不含 exe 路径，以避免转义问题）
    // lpApplicationName 将单独包含 exe 路径
    let cmd_args = format!(
        "--terminal-helper {} {} {} {} {}",
        input_pipe_name, output_pipe_name, rows, cols, terminal_id
    );

    log::debug!("Launching terminal helper for terminal {}", terminal_id);

    // 将 exe 路径转换为宽字符串以供 lpApplicationName 使用
    let exe_path_wide: Vec<u16> = OsStr::new(exe_path.as_os_str())
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // 按照 Windows 约定，命令行必须将 exe 名称作为第一个参数
    let cmd_line = format!("\"{}\" {}", exe_path.display(), cmd_args);
    let mut cmd_wide: Vec<u16> = OsStr::new(&cmd_line)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // 为该用户创建环境块，并带有 RAII 清理
    let mut environment: *mut c_void = ptr::null_mut();
    let env_ok = unsafe {
        CreateEnvironmentBlock(
            &mut environment,
            Some(HANDLE(user_token.as_raw() as _)),
            true,
        )
    }
    .is_ok();

    // 使用 RAII 守卫确保即使在错误路径上也能清理
    let _env_guard = if env_ok && !environment.is_null() {
        Some(EnvironmentBlockGuard { ptr: environment })
    } else {
        if !env_ok {
            log::warn!("Failed to create environment block, using default");
        }
        None
    };

    let creation_flags = CREATE_NO_WINDOW
        | if env_ok {
            CREATE_UNICODE_ENVIRONMENT
        } else {
            PROCESS_CREATION_FLAGS(0)
        };

    // 使用 lpApplicationName 将 exe 路径与命令行分开传递
    // 这避免了 exe 路径中特殊字符可能带来的问题
    let result = unsafe {
        CreateProcessAsUserW(
            Some(HANDLE(user_token.as_raw() as _)),
            PCWSTR::from_raw(exe_path_wide.as_ptr()), // lpApplicationName: exe path
            Some(PWSTR::from_raw(cmd_wide.as_mut_ptr())), // lpCommandLine: full command
            None,
            None,
            false, // Don't inherit handles
            creation_flags,
            if env_ok { Some(environment) } else { None },
            PCWSTR::null(), // Use default current directory
            &si,
            &mut pi,
        )
    };

    // 环境块清理由 _env_guard 的 Drop 处理

    if let Err(e) = result {
        log::error!("CreateProcessAsUserW failed: {}", e);
        return Err(anyhow!("Failed to launch terminal helper: {}", e));
    }

    // 关闭线程句柄 —— 跟踪只需要进程句柄
    // 忽略结果：进程已启动，此处 CloseHandle 失败不构成关键问题
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }

    log::info!("Terminal helper launched with PID {}", pi.dwProcessId);
    // 返回进程信息用于跟踪
    Ok(HelperProcessInfo {
        handle: pi.hProcess,
        pid: pi.dwProcessId,
    })
}

/// 检查辅助进程是否仍在运行。
/// 进程仍在运行时返回 true，已退出时返回 false。
pub fn is_helper_process_running(handle: HANDLE) -> bool {
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    // WAIT_TIMEOUT (258) 表示进程仍在运行
    // WAIT_OBJECT_0 (0) 表示进程已退出
    wait_result != WAIT_OBJECT_0
}

/// 运行终端辅助进程
/// 参数：--terminal-helper <input_pipe_name> <output_pipe_name> <rows> <cols> <terminal_id>
pub fn run_terminal_helper(args: &[String]) -> Result<()> {
    if args.len() < 5 {
        return Err(anyhow!(
            "Usage: --terminal-helper <input_pipe> <output_pipe> <rows> <cols> <terminal_id>"
        ));
    }

    let input_pipe_name = &args[0];
    let output_pipe_name = &args[1];
    let rows: u16 = args[2]
        .parse()
        .map_err(|e| anyhow!("Failed to parse rows '{}': {}", args[2], e))?;
    let cols: u16 = args[3]
        .parse()
        .map_err(|e| anyhow!("Failed to parse cols '{}': {}", args[3], e))?;
    let terminal_id: i32 = args[4]
        .parse()
        .map_err(|e| anyhow!("Failed to parse terminal_id '{}': {}", args[4], e))?;

    log::debug!(
        "Terminal helper starting: terminal_id={}, size={}x{}",
        terminal_id,
        cols,
        rows
    );

    // 打开命名管道（由服务创建）
    let mut input_pipe = open_pipe(input_pipe_name, true)?;
    let mut output_pipe = open_pipe(output_pipe_name, false)?;

    // 创建 ConPTY 和 shell
    let pty_size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pty_system = portable_pty::native_pty_system();
    let pty_pair = pty_system.openpty(pty_size).context("Failed to open PTY")?;

    let shell = get_default_shell();
    log::debug!("Using shell: {}", shell);

    let mut cmd = CommandBuilder::new(&shell);
    configure_utf8_shell_command(&shell, &mut cmd);
    let mut child = pty_pair
        .slave
        .spawn_command(cmd)
        .context("Failed to spawn shell")?;

    // 启动后显式 drop 从端以释放资源
    drop(pty_pair.slave);

    let pid = child.process_id().unwrap_or(0);
    log::debug!("Shell started with PID: {}", pid);

    let mut pty_writer = pty_pair
        .master
        .take_writer()
        .context("Failed to get PTY writer")?;

    let mut pty_reader = pty_pair
        .master
        .try_clone_reader()
        .context("Failed to get PTY reader")?;

    // 将 pty_pair.master 包装进 Arc<Mutex>，以便与输入线程共享（用于调整尺寸）。
    let pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pty_pair.master));

    let exiting = Arc::new(AtomicBool::new(false));

    // 线程：从输入管道读取、解析消息，将数据写入 PTY 或处理控制命令
    let exiting_clone = exiting.clone();
    let pty_master_clone = pty_master.clone();
    let input_thread = thread::spawn(move || {
        let mut input_pipe = input_pipe;
        let mut header_buf = [0u8; MSG_HEADER_SIZE];
        let mut payload_buf = vec![0u8; 4096];

        loop {
            if exiting_clone.load(Ordering::SeqCst) {
                break;
            }

            // 读取消息头
            match read_exact_or_eof(&mut input_pipe, &mut header_buf) {
                Ok(false) => {
                    log::debug!("Input pipe EOF");
                    break;
                }
                Ok(true) => {}
                Err(e) => {
                    log::error!("Input pipe header read error: {}", e);
                    break;
                }
            }

            let msg_type = header_buf[0];
            let payload_len =
                u32::from_le_bytes([header_buf[1], header_buf[2], header_buf[3], header_buf[4]])
                    as usize;

            // 校验负载长度以防止拒绝服务
            if payload_len > MAX_PAYLOAD_SIZE {
                log::error!(
                    "Payload too large: {} bytes (max {})",
                    payload_len,
                    MAX_PAYLOAD_SIZE
                );
                break;
            }

            // 确保负载缓冲区足够大
            if payload_buf.len() < payload_len {
                payload_buf.resize(payload_len, 0);
            }

            // 读取负载
            if payload_len > 0 {
                match read_exact_or_eof(&mut input_pipe, &mut payload_buf[..payload_len]) {
                    Ok(false) => {
                        log::debug!("Input pipe EOF during payload read");
                        break;
                    }
                    Ok(true) => {}
                    Err(e) => {
                        log::error!("Input pipe payload read error: {}", e);
                        break;
                    }
                }
            }

            match msg_type {
                MSG_TYPE_DATA => {
                    // 将终端数据写入 PTY
                    if let Err(e) = pty_writer.write_all(&payload_buf[..payload_len]) {
                        log::error!("PTY write error: {}", e);
                        break;
                    }
                    if let Err(e) = pty_writer.flush() {
                        log::error!("PTY flush error: {}", e);
                        break;
                    }
                }
                MSG_TYPE_RESIZE => {
                    if payload_len >= 4 {
                        let rows = u16::from_le_bytes([payload_buf[0], payload_buf[1]]);
                        let cols = u16::from_le_bytes([payload_buf[2], payload_buf[3]]);
                        log::debug!("Resize: {}x{}", cols, rows);
                        if let Ok(master) = pty_master_clone.lock() {
                            let _ = master.resize(PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                    }
                }
                _ => {
                    // 未知类型可能表示数据损坏 —— 停止处理以避免解析错误
                    log::error!("Unknown message type: {}, terminating", msg_type);
                    break;
                }
            }
        }
        log::debug!("Input thread exiting");
    });

    // 线程：从 PTY 读取、写入输出管道
    let exiting_clone = exiting.clone();
    let output_thread = thread::spawn(move || {
        let mut output_pipe = output_pipe;
        let mut buf = vec![0u8; 4096];
        loop {
            if exiting_clone.load(Ordering::SeqCst) {
                break;
            }
            match pty_reader.read(&mut buf) {
                Ok(0) => {
                    log::debug!("PTY EOF");
                    break;
                }
                Ok(n) => {
                    if let Err(e) = output_pipe.write_all(&buf[..n]) {
                        log::error!("Output pipe write error: {}", e);
                        break;
                    }
                    if let Err(e) = output_pipe.flush() {
                        log::error!("Output pipe flush error: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        log::error!("PTY read error: {}", e);
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        log::debug!("Output thread exiting");
    });

    // 等待子进程退出
    let exit_status = child.wait();
    log::info!("Shell exited: {:?}", exit_status);

    exiting.store(true, Ordering::SeqCst);

    // 等待线程结束
    let _ = input_thread.join();
    let _ = output_thread.join();

    // pty_master 将在此处被 drop，释放 PTY 资源
    drop(pty_master);

    log::info!("Terminal helper exiting");
    Ok(())
}

/// 从 reader 中精确读取 `buf.len()` 个字节。
/// 成功时返回 Ok(true)，遇到 EOF 返回 Ok(false)，出错时返回 Err。
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut pos = 0;
    while pos < buf.len() {
        match reader.read(&mut buf[pos..]) {
            Ok(0) => return Ok(false), // EOF
            Ok(n) => pos += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// 以客户端身份打开命名管道。
/// `for_read`：读取（输入管道）为 true，写入（输出管道）为 false。
fn open_pipe(pipe_name: &str, for_read: bool) -> Result<File> {
    let wide_name: Vec<u16> = OsStr::new(pipe_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let access = if for_read {
        FILE_GENERIC_READ.0
    } else {
        FILE_GENERIC_WRITE.0
    };

    let handle = unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide_name.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };

    match handle {
        Ok(h) => Ok(unsafe { File::from_raw_handle(h.0 as _) }),
        Err(e) => Err(anyhow!(
            "Failed to open {} pipe '{}': {}",
            if for_read { "input" } else { "output" },
            pipe_name,
            e
        )),
    }
}
