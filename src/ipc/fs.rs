#[cfg(target_os = "linux")]
use super::ipc_auth::active_uid;
use crate::ipc::{connect, Data};
use hbb_common::{config, log, ResultType};
use std::{
    ffi::CString,
    io::{Error, ErrorKind},
    os::unix::ffi::OsStrExt,
    path::Path,
};

struct FdGuard(i32);
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe {
            hbb_common::libc::close(self.0);
        }
    }
}

#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn terminal_count_candidate_uids(effective_uid: u32) -> Vec<u32> {
    if effective_uid != 0 {
        return vec![effective_uid];
    }
    let mut candidates = Vec::with_capacity(2);
    if let Some(uid) = active_uid().filter(|uid| *uid != 0) {
        candidates.push(uid);
    }
    candidates.push(0);
    candidates
}

#[inline]
fn expected_ipc_parent_mode(postfix: &str) -> u32 {
    if config::is_service_ipc_postfix(postfix) {
        0o0711
    } else {
        0o0700
    }
}

fn open_ipc_parent_dir_fd(parent_c: &CString) -> std::io::Result<i32> {
    let fd = unsafe {
        hbb_common::libc::open(
            parent_c.as_ptr(),
            hbb_common::libc::O_RDONLY
                | hbb_common::libc::O_DIRECTORY
                | hbb_common::libc::O_CLOEXEC
                | hbb_common::libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

// 通过已经打开的父目录 FD 删除一个已存在的 IPC 残留产物。
//
// 安全意图：
// - 把清理绑定到那个通过了 O_NOFOLLOW + fstat 检查的确切父 inode。
// - 避免清理过程中基于路径的 TOCTOU（例如父路径被重命名/替换的竞争）。
//
// 流程：
// 1) 用 fstatat(..., AT_SYMLINK_NOFOLLOW) 在 parent_fd 之下检查目标条目。
// 2) 根据 st_mode 判断是文件还是目录。
// 3) 相对于 parent_fd 执行 unlinkat（目录用 AT_REMOVEDIR）。
//
// 错误策略：
// - NotFound 视为无害（已被移除 / 被竞争抢先）。
// - 其他错误会显式上报。
fn remove_parent_entry_via_fd(
    parent_fd: i32,
    parent_dir: &Path,
    entry_name: &str,
) -> ResultType<()> {
    if entry_name.contains('/') {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid ipc parent entry name (contains '/'): parent={}, entry={}",
                parent_dir.display(),
                entry_name
            ),
        )
        .into());
    }
    let entry_c = CString::new(entry_name.as_bytes().to_vec()).map_err(|err| {
        Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid ipc parent entry name: parent={}, entry={}, err={}",
                parent_dir.display(),
                entry_name,
                err
            ),
        )
    })?;
    let mut stat: hbb_common::libc::stat = unsafe { std::mem::zeroed() };
    let stat_rc = unsafe {
        hbb_common::libc::fstatat(
            parent_fd,
            entry_c.as_ptr(),
            &mut stat,
            hbb_common::libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if stat_rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        return Err(Error::new(
            err.kind(),
            format!(
                "failed to stat preexisting ipc parent dir entry by fd: parent={}, entry={}, err={}",
                parent_dir.display(),
                entry_name,
                err
            ),
        )
        .into());
    }

    let is_dir = (stat.st_mode & (hbb_common::libc::S_IFMT as hbb_common::libc::mode_t))
        == hbb_common::libc::S_IFDIR;
    let unlink_flags = if is_dir {
        hbb_common::libc::AT_REMOVEDIR
    } else {
        0
    };
    let unlink_rc =
        unsafe { hbb_common::libc::unlinkat(parent_fd, entry_c.as_ptr(), unlink_flags) };
    if unlink_rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        return Err(Error::new(
            err.kind(),
            format!(
                "failed to remove preexisting ipc parent dir entry by fd: parent={}, entry={}, err={}",
                parent_dir.display(),
                entry_name,
                err
            ),
        )
        .into());
    }
    Ok(())
}

fn scrub_preexisting_ipc_parent_entries(
    parent_fd: i32,
    parent_dir: &Path,
    postfix: &str,
) -> ResultType<()> {
    let ipc_basename = format!("ipc{}", postfix);
    remove_parent_entry_via_fd(parent_fd, parent_dir, &ipc_basename)?;
    remove_parent_entry_via_fd(parent_fd, parent_dir, &format!("{}.pid", ipc_basename))?;
    Ok(())
}

/// 通过该目录上不跟随符号链接的 fd，从 IPC 父目录中删除一个条目。
///
/// 对于任何即将被 bind 的东西，优先用它而不是 `std::fs::remove_file`：`remove_file` 是
/// `unlink(2)`，对一个目录类型的抢占者会返回 EISDIR 并把它留在原地，
/// 随后的 bind 就会以 EADDRINUSE 失败。`remove_parent_entry_via_fd` 会先对该
/// 条目做 fstat，需要时选用 `AT_REMOVEDIR`。
///
/// `AT_REMOVEDIR` 就是 `rmdir(2)`，所以它能解决的目录情形是空目录；非空的
/// 抢占者仍会返回 ENOTEMPTY，仍然阻塞随后的 bind。这是有意为之，而
/// 那个“显而易见的”修法比这个 bug 更糟：递归删除就等于 root 删掉一棵
/// 由非特权进程种下的目录树。那样调用方得到的只是一个可以在
/// bind 自身失败之前记录的具名错误，而不是一次成功的 bind。
pub(crate) fn remove_ipc_entry_via_secure_parent_fd(path: &str) -> ResultType<()> {
    let entry_name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("invalid ipc path: {path}")))?
        .to_owned();
    let parent_dir = Path::new(path)
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("invalid ipc path: {path}")))?;
    let parent_c = CString::new(parent_dir.as_os_str().as_bytes().to_vec())?;
    let fd = match open_ipc_parent_dir_fd(&parent_c) {
        Ok(fd) => fd,
        Err(open_err) => {
            if open_err.kind() == ErrorKind::NotFound {
                return Ok(());
            }
            return Err(Error::new(
                open_err.kind(),
                format!(
                    "failed to open ipc parent dir for stale socket cleanup (no-follow): path={}, parent={}, err={}",
                    path,
                    parent_dir.display(),
                    open_err
                ),
            )
            .into());
        }
    };
    let _fd_guard = FdGuard(fd);
    remove_parent_entry_via_fd(fd, parent_dir, &entry_name)
}

fn remove_ipc_socket_via_secure_parent_fd(postfix: &str) -> ResultType<()> {
    remove_ipc_entry_via_secure_parent_fd(&config::Config::ipc_path(postfix))
}

// 目的：
// - 在创建/监听套接字文件之前加固 IPC 父目录。
// - 防止符号链接/路径竞争的滥用，并拒绝不安全的属主/权限模式。
//
// 做法：
// - 用 O_NOFOLLOW/O_DIRECTORY 打开父目录，并基于该 fd 操作。
// - 通过 fstat 校验 inode 类型/属主/权限模式。
// - 对受保护的服务后缀，可以选择接管属主（仅 root），然后清理陈旧的
//   rustdesk IPC 残留产物（当目录的信任边界发生变化时）。
//
// 主要步骤：
// 1) 解析父路径并安全地打开/创建目录。
// 2) 校验目录的 inode 类型和属主 uid。
// 3) 通过在已打开的 fd 上执行 fchmod 来强制期望的权限模式。
// 4) 当加固前的属主/权限模式不安全时，清理陈旧的 IPC 残留产物。
//
// 参考：
// - open(2)：O_NOFOLLOW/O_DIRECTORY/O_CLOEXEC
//   https://man7.org/linux/man-pages/man2/open.2.html
// - fstat(2)：在已打开的 fd 上校验文件类型/元数据
//   https://man7.org/linux/man-pages/man2/fstat.2.html
// - fchown(2)：以 root 运行时接管属主
//   https://man7.org/linux/man-pages/man2/chown.2.html
// - fchmod(2)：在已打开的 fd 上强制精确的权限模式
//   https://man7.org/linux/man-pages/man2/fchmod.2.html
pub(crate) fn ensure_secure_ipc_parent_dir(path: &str, postfix: &str) -> ResultType<bool> {
    let parent_dir = Path::new(path)
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("invalid ipc path: {path}")))?;
    // 通过用 O_NOFOLLOW 打开父目录来抵御常见的 TOCTOU（这样父目录
    // 本身不可能是符号链接），然后基于它的 FD 操作（fstat/fchown/fchmod）。这确保
    // 我们修改的是自己打开的那个 inode，尽管它并不能防止祖先路径
    // 组件中的符号链接。
    let parent_c = CString::new(parent_dir.as_os_str().as_bytes().to_vec())?;
    let fd = match open_ipc_parent_dir_fd(&parent_c) {
        Ok(fd) => fd,
        Err(open_err) => {
            // 如果目录还不存在，就用期望的权限模式创建它。该父
            // 目录本应是单层的 /tmp 路径，所以这里用 mkdir 就够了。
            if open_err.raw_os_error() == Some(hbb_common::libc::ENOENT) {
                let expected_mode = expected_ipc_parent_mode(postfix);
                let rc = unsafe {
                    hbb_common::libc::mkdir(
                        parent_c.as_ptr(),
                        expected_mode as hbb_common::libc::mode_t,
                    )
                };
                if rc != 0 {
                    let mkdir_err = std::io::Error::last_os_error();
                    // 处理另一个进程抢先创建了目录的竞争。
                    if mkdir_err.raw_os_error() != Some(hbb_common::libc::EEXIST) {
                        return Err(Error::new(
                            mkdir_err.kind(),
                            format!(
                                "failed to mkdir ipc parent dir: postfix={}, parent={}, err={}",
                                postfix,
                                parent_dir.display(),
                                mkdir_err
                            ),
                        )
                        .into());
                    }
                }
                match open_ipc_parent_dir_fd(&parent_c) {
                    Ok(fd) => fd,
                    Err(err) => {
                        return Err(Error::new(
                            err.kind(),
                            format!(
                                "failed to open ipc parent dir (no-follow): postfix={}, parent={}, err={}",
                                postfix,
                                parent_dir.display(),
                                err
                            ),
                        )
                        .into());
                    }
                }
            } else {
                return Err(Error::new(
                    open_err.kind(),
                    format!(
                        "failed to open ipc parent dir (no-follow): postfix={}, parent={}, err={}",
                        postfix,
                        parent_dir.display(),
                        open_err
                    ),
                )
                .into());
            }
        }
    };
    let _fd_guard = FdGuard(fd);

    let mut st: hbb_common::libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { hbb_common::libc::fstat(fd, &mut st as *mut _) } != 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(Error::new(
            os_err.kind(),
            format!(
                "failed to stat ipc parent dir: postfix={}, parent={}, err={}",
                postfix,
                parent_dir.display(),
                os_err
            ),
        )
        .into());
    }
    let mode = st.st_mode as u32;
    let is_dir = (mode & (hbb_common::libc::S_IFMT as u32)) == (hbb_common::libc::S_IFDIR as u32);
    if !is_dir {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "ipc parent is not directory: postfix={}, parent={}",
                postfix,
                parent_dir.display()
            ),
        )
        .into());
    }

    let expected_uid = unsafe { hbb_common::libc::geteuid() as u32 };
    let mut owner_uid = st.st_uid as u32;
    let mut adopted_foreign_service_parent = false;
    // 历史上，服务作用域的 IPC 可能由不同的权限上下文创建。
    // 如果以 root 运行且服务后缀受保护，就先尝试接管属主。
    if owner_uid != expected_uid && expected_uid == 0 && config::is_service_ipc_postfix(postfix) {
        let rc = unsafe {
            hbb_common::libc::fchown(
                fd,
                expected_uid as hbb_common::libc::uid_t,
                hbb_common::libc::gid_t::MAX,
            )
        };
        if rc == 0 {
            let mut st2: hbb_common::libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { hbb_common::libc::fstat(fd, &mut st2 as *mut _) } == 0 {
                owner_uid = st2.st_uid as u32;
                st = st2;
                adopted_foreign_service_parent = true;
            }
        } else {
            // 保持行为不变；捕获 errno 以便诊断 chown 失败的原因。
            let err = std::io::Error::last_os_error();
            log::warn!(
                "Failed to chown ipc parent dir, parent={}, postfix={}, expected_uid={}, rc={}, err={:?}",
                parent_dir.display(),
                postfix,
                expected_uid,
                rc,
                err
            );
        }
    }
    if owner_uid != expected_uid {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "unsafe ipc parent owner, postfix={}, expected uid {expected_uid}, got {owner_uid}: {}",
                postfix,
                parent_dir.display()
            ),
        )
        .into());
    }

    let expected_mode = expected_ipc_parent_mode(postfix);
    // 包含特殊位（setuid/setgid/sticky），以确保目录被加固到精确的
    // 期望权限模式。
    let current_mode = (st.st_mode as u32) & 0o7777;
    let repaired_parent_mode = current_mode != expected_mode;
    let had_untrusted_parent_mode = (current_mode & 0o022) != 0;
    if repaired_parent_mode {
        // 在已打开的 fd 上使用 fchmod，以避免检查和 chmod 之间的路径竞争。
        if unsafe { hbb_common::libc::fchmod(fd, expected_mode as hbb_common::libc::mode_t) } != 0 {
            let os_err = std::io::Error::last_os_error();
            return Err(Error::new(
                os_err.kind(),
                format!(
                    "failed to chmod ipc parent dir: postfix={}, parent={}, err={}",
                    postfix,
                    parent_dir.display(),
                    os_err
                ),
            )
            .into());
        }
    }
    let should_scrub =
        repaired_parent_mode || adopted_foreign_service_parent || had_untrusted_parent_mode;
    Ok(should_scrub)
}

pub(crate) fn scrub_secure_ipc_parent_dir(path: &str, postfix: &str) -> ResultType<()> {
    let parent_dir = Path::new(path)
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("invalid ipc path: {path}")))?;
    let parent_c = CString::new(parent_dir.as_os_str().as_bytes().to_vec())?;
    let fd = open_ipc_parent_dir_fd(&parent_c).map_err(|err| {
        Error::new(
            err.kind(),
            format!(
                "failed to open ipc parent dir for scrub (no-follow): postfix={}, parent={}, err={}",
                postfix,
                parent_dir.display(),
                err
            ),
        )
    })?;
    let _fd_guard = FdGuard(fd);
    scrub_preexisting_ipc_parent_entries(fd, parent_dir, postfix)
}

#[inline]
pub(crate) fn get_pid_file(postfix: &str) -> String {
    let path = config::Config::ipc_path(postfix);
    format!("{}.pid", path)
}

// 目的：
// - 把当前进程的 pid 写入 pid 文件，且不跟随攻击者控制的符号链接。
// - 确保 pid 文件是一个由所打开的 inode 路径所拥有的普通文件。
//
// 做法：
// - 使用 libc 的 open/fstat/write 系统调用（FFI），让标志位和 inode 校验都是显式的。
// - 用 O_NOFOLLOW/O_CLOEXEC 打开文件，并在写入之前用 fstat 校验 S_IFREG。
// - 把 unsafe 作用域降到最小，并立即检查系统调用的返回值。
//
// 主要步骤：
// 1) 安全地打开 pid 文件（不做截断）。
// 2) 校验所打开的 inode 是由当前 euid 拥有的普通文件。
// 3) 把 pid 文件的权限模式强制为 0600，并在校验之后用 ftruncate 截断。
// 4) 通过 fd 写入进程 id 字节。
//
// 为什么不用普通的 std::fs::write？
// - std::fs 的辅助函数无法强制这套精确的“打开时加固”流程
//   （尤其是“用 O_NOFOLLOW 打开，然后对同一个已打开的 inode 做 fstat”）。
//
// 参考：
// - open(2)：O_NOFOLLOW/O_CLOEXEC/O_NONBLOCK
//   https://man7.org/linux/man-pages/man2/open.2.html
// - fstat(2)：在已打开的 fd 上校验文件类型
//   https://man7.org/linux/man-pages/man2/fstat.2.html
// - fchmod(2)：在复用的 pid 文件上强制安全权限
//   https://man7.org/linux/man-pages/man2/fchmod.2.html
// - ftruncate(2)：校验之后截断
//   https://man7.org/linux/man-pages/man2/ftruncate.2.html
// - write(2)：通过 fd 写入字节
//   https://man7.org/linux/man-pages/man2/write.2.html
fn write_pid_file(path: &Path) -> ResultType<()> {
    let path_c = CString::new(path.as_os_str().as_bytes().to_vec()).map_err(|err| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid pid file path '{}': {}", path.display(), err),
        )
    })?;
    let flags = hbb_common::libc::O_WRONLY
        | hbb_common::libc::O_CREAT
        | hbb_common::libc::O_CLOEXEC
        | hbb_common::libc::O_NOFOLLOW
        | hbb_common::libc::O_NONBLOCK;
    let fd = unsafe { hbb_common::libc::open(path_c.as_ptr(), flags, 0o0600) };
    if fd < 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(Error::new(
            os_err.kind(),
            format!(
                "failed to open pid file with no-follow '{}': {}",
                path.display(),
                os_err
            ),
        )
        .into());
    }
    let _fd_guard = FdGuard(fd);
    let mut stat: hbb_common::libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { hbb_common::libc::fstat(fd, &mut stat) } != 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(Error::new(
            os_err.kind(),
            format!("failed to stat pid file '{}': {}", path.display(), os_err),
        )
        .into());
    }
    if (stat.st_mode & (hbb_common::libc::S_IFMT as hbb_common::libc::mode_t))
        != (hbb_common::libc::S_IFREG as hbb_common::libc::mode_t)
    {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("pid file path is not a regular file: '{}'", path.display()),
        )
        .into());
    }
    let expected_uid = unsafe { hbb_common::libc::geteuid() as u32 };
    if stat.st_uid as u32 != expected_uid {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "pid file owner mismatch: expected uid {}, got {} for '{}'",
                expected_uid,
                stat.st_uid,
                path.display()
            ),
        )
        .into());
    }
    if unsafe { hbb_common::libc::fchmod(fd, 0o600) } != 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(Error::new(
            os_err.kind(),
            format!("failed to chmod pid file '{}': {}", path.display(), os_err),
        )
        .into());
    }
    if unsafe { hbb_common::libc::ftruncate(fd, 0) } != 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(Error::new(
            os_err.kind(),
            format!(
                "failed to truncate pid file '{}': {}",
                path.display(),
                os_err
            ),
        )
        .into());
    }

    let bytes = std::process::id().to_string();
    let buf = bytes.as_bytes();
    // 即使是普通文件，`write(2)` 也允许返回短写。
    // PID 内容极小，通常一次就写完，但我们仍然循环
    // 直到所有字节都写完，这样这条路径在语义上才是正确的。
    let mut written = 0usize;
    while written < buf.len() {
        let rc = unsafe {
            hbb_common::libc::write(
                fd,
                buf[written..].as_ptr() as *const hbb_common::libc::c_void,
                buf.len() - written,
            )
        };
        if rc < 0 {
            let os_err = std::io::Error::last_os_error();
            return Err(Error::new(
                os_err.kind(),
                format!("failed to write pid file '{}': {}", path.display(), os_err),
            )
            .into());
        }
        if rc == 0 {
            return Err(Error::new(
                ErrorKind::WriteZero,
                format!(
                    "failed to write pid file '{}': write returned 0 bytes",
                    path.display()
                ),
            )
            .into());
        }
        written += rc as usize;
    }
    Ok(())
}

#[inline]
pub(crate) fn write_pid(postfix: &str) {
    let path = std::path::PathBuf::from(get_pid_file(postfix));
    if let Err(err) = write_pid_file(&path) {
        log::warn!(
            "Failed to write pid file for postfix '{}', path='{}', err={}",
            postfix,
            path.display(),
            err
        );
    }
}

// 目的：
// - 安全地读取 pid 文件，避免信任符号链接/非普通文件。
//
// 做法：
// - 使用 libc 的 open/fstat/read 系统调用（FFI）来控制标志位和 inode 校验。
// - 用 O_NOFOLLOW 打开路径，通过 fstat 校验所打开的 fd，然后读取并解析。
// - 把 unsafe 作用域降到最小，并立即检查系统调用的返回值。
//
// 主要步骤：
// 1) 以只读方式安全地打开 pid 文件。
// 2) 确保 fd 指向普通文件。
// 3) 读取字节并解析出 usize 类型的 pid。
//
// 参考：
// - open(2)：O_NOFOLLOW/O_CLOEXEC/O_NONBLOCK
//   https://man7.org/linux/man-pages/man2/open.2.html
// - fstat(2)：在已打开的 fd 上校验 S_IFREG
//   https://man7.org/linux/man-pages/man2/fstat.2.html
// - read(2)：通过 fd 读取字节
//   https://man7.org/linux/man-pages/man2/read.2.html
#[inline]
fn read_pid_file_secure(path: &Path) -> Option<usize> {
    let path_c = CString::new(path.as_os_str().as_bytes().to_vec()).ok()?;
    let flags = hbb_common::libc::O_RDONLY
        | hbb_common::libc::O_CLOEXEC
        | hbb_common::libc::O_NOFOLLOW
        | hbb_common::libc::O_NONBLOCK;
    let fd = unsafe { hbb_common::libc::open(path_c.as_ptr(), flags) };
    if fd < 0 {
        return None;
    }
    let _fd_guard = FdGuard(fd);

    let mut stat: hbb_common::libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { hbb_common::libc::fstat(fd, &mut stat) } != 0 {
        return None;
    }
    if (stat.st_mode & (hbb_common::libc::S_IFMT as hbb_common::libc::mode_t))
        != (hbb_common::libc::S_IFREG as hbb_common::libc::mode_t)
    {
        return None;
    }

    let mut buffer = [0u8; 64];
    let read_len = unsafe {
        hbb_common::libc::read(
            fd,
            buffer.as_mut_ptr() as *mut hbb_common::libc::c_void,
            buffer.len(),
        )
    };
    if read_len <= 0 {
        return None;
    }
    let content = String::from_utf8_lossy(&buffer[..read_len as usize]).to_string();
    content.trim().parse::<usize>().ok()
}

#[inline]
async fn probe_existing_listener(postfix: &str) -> bool {
    let Ok(mut stream) = connect(1000, postfix).await else {
        return false;
    };
    if postfix != crate::POSTFIX_SERVICE {
        return true;
    }
    if stream.send(&Data::SyncConfig(None)).await.is_err() {
        return false;
    }
    matches!(
        stream.next_timeout(1000).await,
        Ok(Some(Data::SyncConfig(Some(_))))
    )
}

pub(crate) async fn check_pid(postfix: &str) -> bool {
    let pid_file = std::path::PathBuf::from(get_pid_file(postfix));
    if let Some(pid) = read_pid_file_secure(&pid_file) {
        if pid > 0 {
            let mut sys = hbb_common::sysinfo::System::new();
            sys.refresh_processes();
            if let Some(p) = sys.process(pid.into()) {
                if let Some(current) = sys.process((std::process::id() as usize).into()) {
                    if current.name() == p.name() && probe_existing_listener(postfix).await {
                        return true;
                    }
                }
            }
        }
    }
    if probe_existing_listener(postfix).await {
        return true;
    }
    // 如果不删除旧的 ipc 文件，新的 ipc 创建会失败
    // 如果我们删除了某个 ipc 文件，但旧的 ipc 进程仍在运行，
    // 对 ipc 的新连接会连到新的 ipc，而旧 ipc 上原有的连接仍然存活
    if let Err(err) = remove_ipc_socket_via_secure_parent_fd(postfix) {
        log::debug!(
            "Failed to remove stale ipc socket via secure parent fd: postfix={}, err={}",
            postfix,
            err
        );
    }
    false
}

#[inline]
pub(crate) fn should_scrub_parent_entries_after_check_pid(
    should_scrub_parent_entries: bool,
    existing_listener_alive: bool,
) -> bool {
    should_scrub_parent_entries && !existing_listener_alive
}

#[cfg(test)]
mod tests {
    // 锁定的是 HELPER 的契约，而 `new_drm_listener` 在那一行只包含这些内容——而不是
    // 调用点本身。在测试里绑定真实的 `/tmp/<app>-service/ipc_drm` 会与
    // 运行中的 root 服务冲突，所以这里并没有覆盖“监听器仍然调用它”这一点。
    #[test]
    fn test_remove_ipc_entry_via_secure_parent_fd_clears_an_empty_directory_squatter() {
        let unique = format!(
            "rustdesk-ipc-entry-remove-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&base).unwrap();
        let squatter = base.join("ipc_drm");
        std::fs::create_dir(&squatter).unwrap();

        // 这是它所修复缺陷的正向对照：`remove_file` 是 `unlink(2)`，无法
        // 删除目录。这正是监听器之前无法清除它的原因，随后才会
        // 在其上 bind 失败。没有这一行，测试通过也说明不了任何问题。
        assert!(
            std::fs::remove_file(&squatter).is_err(),
            "remove_file must fail on a directory, or this test is vacuous"
        );
        assert!(squatter.is_dir());

        super::remove_ipc_entry_via_secure_parent_fd(squatter.to_string_lossy().as_ref()).unwrap();
        assert!(
            !squatter.exists(),
            "the fd-based removal picks AT_REMOVEDIR and clears it"
        );

        // 幂等：每次 bind 之前都会执行，所以路径已经消失不算错误。
        super::remove_ipc_entry_via_secure_parent_fd(squatter.to_string_lossy().as_ref()).unwrap();

        // 这是最常见的情形，也是监听器每次重启都会遇到的：上一次运行留下的陈旧套接字，
        // 也就是一个普通文件。这里覆盖它，是因为另一个文件删除测试
        // 走的是 `remove_parent_entry_via_fd` 和后缀路径，而不是这个入口。
        std::fs::write(&squatter, b"stale").unwrap();
        super::remove_ipc_entry_via_secure_parent_fd(squatter.to_string_lossy().as_ref()).unwrap();
        assert!(!squatter.exists(), "a stale regular file is cleared too");

        // 还有那个有明确文档的限制，钉住它以免文档失真：AT_REMOVEDIR 就是 rmdir(2)，所以
        // 非空的抢占者只会被报告，不会被清除。调用方记录该情况并继续；这里
        // 绝不应该开始删除自己没有创建的目录树。
        std::fs::create_dir(&squatter).unwrap();
        std::fs::write(squatter.join("planted"), b"x").unwrap();
        assert!(
            super::remove_ipc_entry_via_secure_parent_fd(squatter.to_string_lossy().as_ref())
                .is_err(),
            "a non-empty directory must be reported, not silently left as success"
        );
        assert!(squatter.join("planted").exists(), "and not deleted");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_write_pid_file_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let unique = format!(
            "rustdesk-ipc-pid-file-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&base).unwrap();

        let target = base.join("target_pid");
        std::fs::write(&target, b"origin").unwrap();
        let link = base.join("pid_link");
        symlink(&target, &link).unwrap();

        let res = super::write_pid_file(&link);
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "origin");

        std::fs::remove_file(&link).ok();
        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_ensure_secure_ipc_parent_dir_rejects_symlink_parent() {
        use std::os::unix::fs::symlink;

        let unique = format!(
            "rustdesk-ipc-secure-dir-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let real_dir = base.join("real");
        let link_dir = base.join("link");
        std::fs::create_dir_all(&real_dir).unwrap();
        symlink(&real_dir, &link_dir).unwrap();
        let ipc_path = link_dir.join("ipc_service");
        let res =
            super::ensure_secure_ipc_parent_dir(ipc_path.to_string_lossy().as_ref(), "_service");
        assert!(res.is_err());
        std::fs::remove_file(&link_dir).ok();
        std::fs::remove_dir_all(&real_dir).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_ensure_secure_ipc_parent_dir_creates_parent_with_expected_mode() {
        use std::os::unix::fs::PermissionsExt;

        let unique = format!(
            "rustdesk-ipc-secure-dir-create-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&base).unwrap();

        // 有意选择一个不存在的父目录，以走 ENOENT -> mkdir 这条分支。
        let parent_dir = base.join("parent");
        assert!(!parent_dir.exists());
        let ipc_path = parent_dir.join("ipc");

        let res = super::ensure_secure_ipc_parent_dir(ipc_path.to_string_lossy().as_ref(), "");
        // 限制性的 umask 会让 mkdir 创建出更严格的初始权限模式。这种情况下
        // ensure_secure_ipc_parent_dir 会用 fchmod 修正它，并可能请求一次清理。
        res.unwrap();

        let md = std::fs::metadata(&parent_dir).unwrap();
        assert!(md.is_dir());
        let mode = md.permissions().mode() & 0o777;
        assert_eq!(mode, 0o0700);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_scrub_preexisting_ipc_parent_entries_only_removes_target_postfix_artifacts() {
        use std::os::unix::ffi::OsStrExt;

        let unique = format!(
            "rustdesk-ipc-scrub-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&base).unwrap();

        let ipc_file = base.join("ipc_service");
        let ipc_pid_file = base.join("ipc_service.pid");
        let ipc_other_postfix_file = base.join("ipc_uinput_1");
        let keep_file = base.join("keep.txt");
        let keep_dir = base.join("keep_dir");

        std::fs::write(&ipc_file, b"socket-placeholder").unwrap();
        std::fs::write(&ipc_pid_file, b"1234").unwrap();
        std::fs::write(&ipc_other_postfix_file, b"other-postfix").unwrap();
        std::fs::write(&keep_file, b"keep").unwrap();
        std::fs::create_dir_all(&keep_dir).unwrap();

        let base_c = std::ffi::CString::new(base.as_os_str().as_bytes().to_vec()).unwrap();
        let base_fd = super::open_ipc_parent_dir_fd(&base_c).unwrap();
        let _base_guard = super::FdGuard(base_fd);
        super::scrub_preexisting_ipc_parent_entries(base_fd, &base, "_service").unwrap();

        assert!(!ipc_file.exists());
        assert!(!ipc_pid_file.exists());
        assert!(ipc_other_postfix_file.exists());
        assert!(keep_file.exists());
        assert!(keep_dir.exists());

        std::fs::remove_file(&ipc_other_postfix_file).ok();
        std::fs::remove_file(&keep_file).ok();
        std::fs::remove_dir_all(&keep_dir).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_scrub_preexisting_ipc_parent_entries_should_bind_to_opened_inode_not_path() {
        use std::os::unix::ffi::OsStrExt;

        let unique = format!(
            "rustdesk-ipc-scrub-fd-bind-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&base).unwrap();

        let trusted_parent = base.join("trusted_parent");
        let trusted_parent_moved = base.join("trusted_parent_moved");
        let attacker_parent = base.join("attacker_parent");
        std::fs::create_dir_all(&trusted_parent).unwrap();
        std::fs::create_dir_all(&attacker_parent).unwrap();

        let trusted_ipc_file = trusted_parent.join("ipc_service");
        let attacker_ipc_file = attacker_parent.join("ipc_service");
        std::fs::write(&trusted_ipc_file, b"trusted").unwrap();
        std::fs::write(&attacker_ipc_file, b"attacker").unwrap();

        let trusted_parent_c =
            std::ffi::CString::new(trusted_parent.as_os_str().as_bytes().to_vec()).unwrap();
        let trusted_parent_fd = super::open_ipc_parent_dir_fd(&trusted_parent_c).unwrap();
        let _trusted_parent_guard = super::FdGuard(trusted_parent_fd);

        // 在可信 inode 被打开之后替换路径。
        std::fs::rename(&trusted_parent, &trusted_parent_moved).unwrap();
        std::fs::rename(&attacker_parent, &trusted_parent).unwrap();

        super::scrub_preexisting_ipc_parent_entries(trusted_parent_fd, &trusted_parent, "_service")
            .unwrap();

        // 期望的安全行为：清理应针对路径替换之前打开的那个 inode。
        assert!(
            !trusted_parent_moved.join("ipc_service").exists(),
            "trusted inode artifact should be removed even after path swap"
        );
        assert!(
            trusted_parent.join("ipc_service").exists(),
            "path-swapped attacker directory should not be scrubbed"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_ensure_secure_ipc_parent_dir_keeps_service_artifacts_before_liveness_probe() {
        use std::os::unix::fs::PermissionsExt;

        let unique = format!(
            "rustdesk-ipc-secure-dir-order-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&base).unwrap();

        let parent_dir = base.join("service_parent");
        std::fs::create_dir_all(&parent_dir).unwrap();
        // 触发 "had_untrusted_service_parent_mode"。
        std::fs::set_permissions(&parent_dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        let ipc_file = parent_dir.join("ipc_service");
        let ipc_pid_file = parent_dir.join("ipc_service.pid");
        std::fs::write(&ipc_file, b"socket-placeholder").unwrap();
        std::fs::write(&ipc_pid_file, b"1234").unwrap();

        let res =
            super::ensure_secure_ipc_parent_dir(ipc_file.to_string_lossy().as_ref(), "_service");
        assert_eq!(res.unwrap(), true);

        // 父目录加固应先执行；残留产物应保留到存活探测完成之后。
        assert!(ipc_file.exists(), "ipc socket marker should be preserved");
        assert!(ipc_pid_file.exists(), "pid marker should be preserved");

        std::fs::remove_dir_all(&base).ok();
    }
}
