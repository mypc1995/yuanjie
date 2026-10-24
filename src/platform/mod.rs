#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub mod win_device;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod delegate;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "linux")]
pub mod gtk_sudo;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use base::message_proto::CursorData;
#[cfg(all(
    not(all(target_os = "windows", not(target_pointer_width = "64"))),
    not(any(target_os = "android", target_os = "ios"))
))]
use hbb_common::sysinfo::System;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::{sysinfo::Pid, ResultType};
use std::sync::{Arc, Mutex};

// 限制名称，因为安装程序会把它们插值进特权脚本和路径中。
#[cfg(any(windows, target_os = "macos"))]
fn validate_install_app_name(app_name: &str) -> ResultType<()> {
    let mut bytes = app_name.bytes();
    let Some(first) = bytes.next() else {
        hbb_common::bail!("Application name must not be empty.");
    };
    // 在 Windows 上保留前导连字符的有效性，以兼容已有的自定义客户端。macOS
    // 会拒绝它们，因为 pgrep 会把前导连字符当作选项。
    let valid_first =
        first.is_ascii_alphanumeric() || first == b'_' || (cfg!(windows) && first == b'-');
    if !valid_first
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        let valid_prefix = if cfg!(windows) {
            "a letter, number, underscore, or hyphen"
        } else {
            "a letter, number, or underscore"
        };
        hbb_common::bail!(
            "Application name must start with {valid_prefix} and contain only letters, numbers, hyphens, and underscores."
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "android", target_os = "ios")))]
pub const SERVICE_INTERVAL: u64 = 300;

lazy_static::lazy_static! {
    static ref INSTALLING_SERVICE: Arc<Mutex<bool>>= Default::default();
}

pub fn installing_service() -> bool {
    INSTALLING_SERVICE.lock().unwrap().clone()
}

pub fn is_xfce() -> bool {
    #[cfg(target_os = "linux")]
    {
        return std::env::var_os("XDG_CURRENT_DESKTOP") == Some(std::ffi::OsString::from("XFCE"));
    }
    #[cfg(not(target_os = "linux"))]
    {
        return false;
    }
}

pub fn breakdown_callback() {
    #[cfg(target_os = "linux")]
    crate::input_service::clear_remapped_keycode();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    crate::input_service::release_device_modifiers();
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn change_resolution(name: &str, width: usize, height: usize) -> ResultType<()> {
    let cur_resolution = current_resolution(name)?;
    // 对于 macOS
    // 待办：确保下面的比较能正常工作。
    // 对于 Linux
    // 直接运行 "xrandr"，可能不会考虑 dpi。
    // 对于 Windows
    // dmPelsWidth 和 dmPelsHeight 与 width 和 height 相同
    // 因为该进程运行在 dpi 感知模式下。
    if cur_resolution.width as usize == width && cur_resolution.height as usize == height {
        return Ok(());
    }
    hbb_common::log::warn!("Change resolution of '{}' to ({},{})", name, width, height);
    change_resolution_directly(name, width, height)
}

// Android
#[cfg(target_os = "android")]
pub fn get_active_username() -> String {
    // TODO
    "android".into()
}

#[cfg(target_os = "android")]
pub const PA_SAMPLE_RATE: u32 = 48000;

#[cfg(target_os = "android")]
#[derive(Default)]
pub struct WakeLock(Option<android_wakelock::WakeLock>);

#[cfg(target_os = "android")]
impl WakeLock {
    pub fn new(tag: &str) -> Self {
        let tag = format!("{}:{tag}", crate::get_app_name());
        match android_wakelock::partial(tag) {
            Ok(lock) => Self(Some(lock)),
            Err(e) => {
                hbb_common::log::error!("Failed to get wakelock: {e:?}");
                Self::default()
            }
        }
    }
}

#[cfg(not(target_os = "ios"))]
pub fn get_wakelock(_display: bool) -> WakeLock {
    hbb_common::log::info!("new wakelock, require display on: {_display}");
    #[cfg(target_os = "android")]
    return crate::platform::WakeLock::new("server");
    // display：保持屏幕常亮
    // idle：保持 CPU 不休眠
    // sleep：阻止系统进入睡眠，即使手动触发
    #[cfg(not(target_os = "android"))]
    return crate::platform::WakeLock::new(_display, true, false);
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) struct InstallingService; // please use new

#[cfg(any(target_os = "windows", target_os = "linux"))]
impl InstallingService {
    pub fn new() -> Self {
        * INSTALLING_SERVICE.lock().unwrap() = true;
        Self
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
impl Drop for InstallingService {
    fn drop(&mut self) {
        * INSTALLING_SERVICE.lock().unwrap() = false;
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[inline]
pub fn is_prelogin() -> bool {
    false
}

// 注意：该方法在 Windows 上效率不高，它会获取所有进程。
// 它只应在性能不敏感时被调用。
// 如果想自己获取命令行，就会引入大量新代码。
#[allow(dead_code)]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn get_pids_of_process_with_args<S1: AsRef<str>, S2: AsRef<str>>(
    name: S1,
    args: &[S2],
) -> Vec<Pid> {
    // 当进程是 32 位而系统是 64 位 Windows 时，该函数无法工作，
    // 这种情况下 `process.cmd()` 总是返回 []。
    // 所以我们改用 `windows::get_pids_with_args_by_wmic()`。
    #[cfg(all(target_os = "windows", not(target_pointer_width = "64")))]
    {
        return windows::get_pids_with_args_by_wmic(name, args);
    }
    #[cfg(not(all(target_os = "windows", not(target_pointer_width = "64"))))]
    {
        let name = name.as_ref().to_lowercase();
        let system = System::new_all();
        system
            .processes()
            .iter()
            .filter(|(_, process)| {
                process.name().to_lowercase() == name
                    && process.cmd().len() == args.len() + 1
                    && args.iter().enumerate().all(|(i, arg)| {
                        process.cmd()[i + 1].to_lowercase() == arg.as_ref().to_lowercase()
                    })
            })
            .map(|(&pid, _)| pid)
            .collect()
    }
}

// 注意：该方法在 Windows 上效率不高，它会获取所有进程。
// 它只应在性能不敏感时被调用。
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_pids_of_process_with_first_arg<S1: AsRef<str>, S2: AsRef<str>>(
    name: S1,
    arg: S2,
) -> Vec<Pid> {
    // 当进程是 32 位而系统是 64 位 Windows 时，该函数无法工作，
    // 这种情况下 `process.cmd()` 总是返回 []。
    // 所以我们改用 `windows::get_pids_with_first_arg_by_wmic()`。
    #[cfg(all(target_os = "windows", not(target_pointer_width = "64")))]
    {
        return windows::get_pids_with_first_arg_by_wmic(name, arg);
    }
    #[cfg(not(all(target_os = "windows", not(target_pointer_width = "64"))))]
    {
        let name = name.as_ref().to_lowercase();
        let system = System::new_all();
        system
            .processes()
            .iter()
            .filter(|(_, process)| {
                process.name().to_lowercase() == name
                    && process.cmd().len() >= 2
                    && process.cmd()[1].to_lowercase() == arg.as_ref().to_lowercase()
            })
            .map(|(&pid, _)| pid)
            .collect()
    }
}

#[cfg(all(test, any(windows, target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn install_app_names_enforce_ascii_command_safety() {
        for app_name in [
            "RustDesk",
            "RustDesk-2",
            "A",
            "A-0",
            "1RustDesk",
            "_RustDesk",
            "Rust_Desk",
        ] {
            assert!(validate_install_app_name(app_name).is_ok(), "{app_name}");
        }
        for app_name in ["", "Rust.Desk", "Rust Desk", "RüstDesk", "RustDesk;touch"] {
            assert!(validate_install_app_name(app_name).is_err(), "{app_name}");
        }

        #[cfg(windows)]
        assert!(validate_install_app_name("-RustDesk").is_ok());
        #[cfg(target_os = "macos")]
        assert!(validate_install_app_name("-RustDesk").is_err());
    }
}
