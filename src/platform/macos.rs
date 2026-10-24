// https://developer.apple.com/documentation/appkit/nscursor
// https://github.com/servo/core-foundation-rs
// https://github.com/rust-windowing/winit

mod update_temp;
mod verified_dmg;

#[cfg(test)]
#[path = "macos/verified_dmg_tests.rs"]
mod verified_dmg_tests;

use super::{validate_install_app_name, CursorData, ResultType};
use cocoa::{
    appkit::{NSApp, NSApplication, NSApplicationActivationPolicy::*},
    base::{id, nil, BOOL, NO, YES},
    foundation::{NSDictionary, NSPoint, NSSize, NSString},
};
use core_foundation::{
    array::{CFArrayGetCount, CFArrayGetValueAtIndex},
    dictionary::CFDictionaryRef,
    string::CFStringRef,
};
use core_graphics::{
    display::{kCGNullWindowID, kCGWindowListOptionOnScreenOnly, CGWindowListCopyWindowInfo},
    window::{kCGWindowName, kCGWindowOwnerPID},
};
use hbb_common::{
    anyhow::anyhow,
    bail, log,
    sysinfo::{Pid, Process, ProcessRefreshKind, System},
};
use base::message_proto::{DisplayInfo, Resolution};
use include_dir::{include_dir, Dir};
use objc::rc::autoreleasepool;
use objc::{class, msg_send, sel, sel_impl};
use scrap::{libc::c_void, quartz::ffi::*};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};
pub use update_temp::try_remove_temp_update_dir;
use update_temp::{copy_dmg_to_update_temp_file, get_update_temp_dir_string};
use verified_dmg::{
    attach_dmg, copy_and_verify_dmg_file, extract_dmg_inner, verified_dmg_path, verify_stored_dmg,
    VerifiedDmg,
};

// macOS 的 boolean_t 在 <mach/boolean.h> 中定义为 `int`
type BooleanT = hbb_common::libc::c_int;

static PRIVILEGES_SCRIPTS_DIR: Dir =
    include_dir!("$CARGO_MANIFEST_DIR/src/platform/privileges_scripts");
static mut LATEST_SEED: i32 = 0;
const UPDATE_CLEANUP_FAILED_AFTER_COMMIT: &str = "UPDATE_CLEANUP_FAILED_AFTER_COMMIT";
// 没有管理员权限时 `kill -9` 可能不起作用。
// 仅用于应用的更新事务，在 launchd 作业缺失或被有意停止时使用。
// 它的校验、暂存、bundle 回滚和清理要与 update.scpt 保持一致。
const PRIVILEGED_UPDATE_BODY: &str = r#"
	on run {app_name, cur_pid, source_path, user_name, restore_owner, expected_sha256, expected_bundle_id}
	    set app_bundle to "/Applications/" & app_name & ".app"
	    set app_bundle_q to quoted form of app_bundle
	    set installed_info_q to quoted form of (app_bundle & "/Contents/Info.plist")
	    set source_path_q to quoted form of source_path
	    set user_name_q to quoted form of user_name
	    set expected_sha256_q to quoted form of expected_sha256
	    set expected_bundle_id_q to quoted form of expected_bundle_id

	    set check_source to "if [ -n " & expected_sha256_q & " ]; then test -f " & source_path_q & "; else test -d " & source_path_q & "; fi;"
	    set kill_others to "pids=$(pgrep -x '" & app_name & "' | grep -vx " & cur_pid & " || true); if [ -n \"$pids\" ]; then echo \"$pids\" | xargs kill -9 || true; fi;"
	    -- Rehash the root-owned copy in a clean environment before staging bytes.
	    set prepare_verified to "verified_dir=$(/usr/bin/mktemp -d /tmp/.rustdeskupdate-verified.XXXXXX); /bin/chmod 0700 \"$verified_dir\"; verified_app=\"$verified_dir/" & app_name & ".app\"; dmg_attached=0; if [ -n " & expected_sha256_q & " ]; then verified_dmg=\"$verified_dir/update.dmg\"; /bin/cp " & source_path_q & " \"$verified_dmg\"; /usr/sbin/chown root:wheel \"$verified_dmg\"; /bin/chmod 0400 \"$verified_dmg\"; actual_sha256=$(/usr/bin/env -i /usr/bin/shasum -a 256 \"$verified_dmg\"); actual_sha256=${actual_sha256%% *}; if [ \"$actual_sha256\" != " & expected_sha256_q & " ]; then echo 'Update DMG SHA256 mismatch' >&2; exit 1; fi; dmg_mount=\"$verified_dir/mount\"; /bin/mkdir \"$dmg_mount\"; dmg_attached=1; /usr/bin/hdiutil attach -readonly -nobrowse -mountpoint \"$dmg_mount\" \"$verified_dmg\" >/dev/null; /usr/bin/ditto \"$dmg_mount/" & app_name & ".app\" \"$verified_app\"; /usr/bin/hdiutil detach \"$dmg_mount\" -force >/dev/null; dmg_attached=0; /bin/rm -f \"$verified_dmg\"; else /usr/bin/ditto " & source_path_q & " \"$verified_app\"; fi; /usr/sbin/chown -R root:wheel \"$verified_app\"; /bin/chmod -R go-w \"$verified_app\";"
	    set validate_verified_app to "candidate_bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \"$verified_app/Contents/Info.plist\"); if [ -z " & expected_bundle_id_q & " ] || [ \"$candidate_bundle_id\" != " & expected_bundle_id_q & " ]; then echo 'Update app bundle identifier mismatch' >&2; exit 1; fi; if [ -e " & app_bundle_q & " ]; then installed_bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' " & installed_info_q & "); if [ \"$installed_bundle_id\" != " & expected_bundle_id_q & " ]; then echo 'Installed app bundle identifier mismatch' >&2; exit 1; fi; fi;"
	    set prepare_swap_paths to "temp_bundle=" & app_bundle_q & ".new.$$; old_bundle=" & app_bundle_q & ".old.$$;"
	    set cleanup_swap_paths to "rm -rf \"$temp_bundle\" \"$old_bundle\";"
	    set stage_bundle to "ditto \"$verified_app\" \"$temp_bundle\";"
	    set protect_staged_bundle to "chown -R root:wheel \"$temp_bundle\"; chmod -R go-w \"$temp_bundle\"; (xattr -r -d com.apple.quarantine \"$temp_bundle\" || true);"
	    set move_current_bundle to "if [ -e " & app_bundle_q & " ]; then mv " & app_bundle_q & " \"$old_bundle\"; bundle_backed_up=1; fi;"
	    set install_staged_bundle to "mv \"$temp_bundle\" " & app_bundle_q & "; bundle_swapped=1;"
	    set restore_installed_owner to "if [ " & quoted form of restore_owner & " = '1' ]; then chown -R " & user_name_q & ":staff " & app_bundle_q & "; fi;"
	    set rollback_bundle to "if [ \"${bundle_backed_up:-0}\" -eq 1 ]; then if [ ! -e \"$old_bundle\" ]; then rollback_status=1; elif ! rm -rf " & app_bundle_q & "; then rollback_status=1; elif ! mv \"$old_bundle\" " & app_bundle_q & "; then rollback_status=1; fi; elif [ \"${bundle_swapped:-0}\" -eq 1 ]; then rm -rf " & app_bundle_q & " || rollback_status=1; fi;"
	    set cleanup_verified to "if [ \"${dmg_attached:-0}\" -eq 1 ]; then /usr/bin/hdiutil detach \"$dmg_mount\" -force >/dev/null 2>&1 || cleanup_status=1; fi; if [ -n \"${temp_bundle:-}\" ]; then rm -rf \"$temp_bundle\" || cleanup_status=1; fi; if [ -n \"${verified_dir:-}\" ]; then rm -rf \"$verified_dir\" || cleanup_status=1; fi;"
	    set rollback_update to "status=$?; trap - EXIT; set +e; cleanup_status=0; if [ \"${transaction_started:-0}\" -eq 1 ] && [ \"${transaction_committed:-0}\" -ne 1 ]; then rollback_status=0;" & rollback_bundle & "if [ \"$rollback_status\" -ne 0 ]; then status=1; fi; fi; if [ \"${rollback_status:-0}\" -eq 0 ]; then " & cleanup_verified & "fi; if [ \"$cleanup_status\" -ne 0 ] && [ \"${transaction_committed:-0}\" -ne 1 ]; then status=1; elif [ \"$cleanup_status\" -ne 0 ]; then echo 'UPDATE_CLEANUP_FAILED_AFTER_COMMIT'; fi; exit \"$status\";"
	    set commit_update to "transaction_committed=1; if ! rm -rf \"$old_bundle\"; then echo 'UPDATE_CLEANUP_FAILED_AFTER_COMMIT'; fi;"
	    set copy_files to prepare_swap_paths & cleanup_swap_paths & stage_bundle & protect_staged_bundle & "transaction_started=1;" & move_current_bundle & install_staged_bundle & restore_installed_owner & commit_update
	    set sh to "set -e; transaction_started=0; transaction_committed=0; bundle_backed_up=0; bundle_swapped=0; trap " & quoted form of rollback_update & " EXIT;" & check_source & prepare_verified & validate_verified_app & kill_others & copy_files

	    do shell script sh with prompt app_name & " wants to update itself" with administrator privileges
	end run
	        "#;

/// 用于串行化 CoreGraphics 光标操作的全局互斥锁。
/// 这避免了光标可见性（隐藏深度跟踪）
/// 与光标定位/裁剪操作之间的竞态。
static CG_CURSOR_MUTEX: Mutex<()> = Mutex::new(());

extern "C" {
    fn CGSCurrentCursorSeed() -> i32;
    fn CGEventCreate(r: *const c_void) -> *const c_void;
    fn CGEventGetLocation(e: *const c_void) -> CGPoint;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> BOOL;
    fn InputMonitoringAuthStatus(_: BOOL) -> BOOL;
    fn IsCanScreenRecording(_: BOOL) -> BOOL;
    fn CanUseNewApiForScreenCaptureCheck() -> BOOL;
    fn MacCheckAdminAuthorization() -> BOOL;
    fn MacGetModeNum(display: u32, numModes: *mut u32) -> BOOL;
    fn MacGetModes(
        display: u32,
        widths: *mut u32,
        heights: *mut u32,
        hidpis: *mut BOOL,
        max: u32,
        numModes: *mut u32,
    ) -> BOOL;
    fn majorVersion() -> u32;
    fn MacGetMode(display: u32, width: *mut u32, height: *mut u32) -> BOOL;
    fn MacSetMode(display: u32, width: u32, height: u32, tryHiDPI: bool) -> BOOL;
    fn CGWarpMouseCursorPosition(newCursorPosition: CGPoint) -> CGError;
    fn CGAssociateMouseAndMouseCursorPosition(connected: BooleanT) -> CGError;
}

pub fn major_version() -> u32 {
    unsafe { majorVersion() }
}

pub fn is_process_trusted(prompt: bool) -> bool {
    autoreleasepool(|| unsafe_is_process_trusted(prompt))
}

fn unsafe_is_process_trusted(prompt: bool) -> bool {
    unsafe {
        let value = if prompt { YES } else { NO };
        let value: id = msg_send![class!(NSNumber), numberWithBool: value];
        let options = NSDictionary::dictionaryWithObject_forKey_(
            nil,
            value,
            kAXTrustedCheckOptionPrompt as _,
        );
        AXIsProcessTrustedWithOptions(options as _) == YES
    }
}

pub fn is_can_input_monitoring(prompt: bool) -> bool {
    unsafe {
        let value = if prompt { YES } else { NO };
        InputMonitoringAuthStatus(value) == YES
    }
}

pub fn is_can_screen_recording(prompt: bool) -> bool {
    autoreleasepool(|| unsafe_is_can_screen_recording(prompt))
}

// macOS >= 10.15
// https://stackoverflow.com/questions/56597221/detecting-screen-recording-settings-on-macos-catalina/
// 只从所有权限中移除某一个应用：tccutil reset All com.carriez.rustdesk
fn unsafe_is_can_screen_recording(prompt: bool) -> bool {
    // 我们收到一些报告说即使设置了权限也不显示，所以尝试使用新的 API 来检查屏幕录制权限
    // 新 API 只在 macOS >= 10.15 上可用，但 stackoverflow 上有人说它在 >= 10.16 上可用（在 10.15 上会崩溃），
    // 也有人说它在 10.16 上有 bug，所以我们只在 11.0 上使用它。
    unsafe {
        if CanUseNewApiForScreenCaptureCheck() == YES {
            return IsCanScreenRecording(if prompt { YES } else { NO }) == YES;
        }
    }
    let mut can_record_screen: bool = false;
    unsafe {
        let our_pid: i32 = std::process::id() as _;
        let our_pid: id = msg_send![class!(NSNumber), numberWithInteger: our_pid];
        let window_list =
            CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        let n = CFArrayGetCount(window_list);
        let dock = NSString::alloc(nil).init_str("Dock");
        for i in 0..n {
            let w: id = CFArrayGetValueAtIndex(window_list, i) as _;
            let name: id = msg_send![w, valueForKey: kCGWindowName as id];
            if name.is_null() {
                continue;
            }
            let pid: id = msg_send![w, valueForKey: kCGWindowOwnerPID as id];
            let is_me: BOOL = msg_send![pid, isEqual: our_pid];
            if is_me == YES {
                continue;
            }
            let pid: i32 = msg_send![pid, intValue];
            let p: id = msg_send![
                class!(NSRunningApplication),
                runningApplicationWithProcessIdentifier: pid
            ];
            if p.is_null() {
                // 忽略我们无权访问的进程，例如管理名为 "Menubar" 和 "Backstop Menubar" 窗口的 WindowServer
                continue;
            }
            let url: id = msg_send![p, executableURL];
            let exe_name: id = msg_send![url, lastPathComponent];
            if exe_name.is_null() {
                continue;
            }
            let is_dock: BOOL = msg_send![exe_name, isEqual: dock];
            if is_dock == YES {
                // 忽略提供桌面壁纸的 Dock
                continue;
            }
            can_record_screen = true;
            break;
        }
    }
    if !can_record_screen && prompt {
        use scrap::{Capturer, Display};
        if let Ok(d) = Display::primary() {
            Capturer::new(d).ok();
        }
    }
    can_record_screen
}

pub fn install_service() -> bool {
    is_installed_daemon(false)
}

// 记得：如果改动 `is_installed_daemon()`，要检查 `update_daemon_agent()` 是否也需要改。
// 无需合并现有的重复代码，因为这两个函数中的代码太关键了。
// 新代码应该写在一个公共函数里。
pub fn is_installed_daemon(prompt: bool) -> bool {
    let daemon = format!("{}_service.plist", crate::get_full_name());
    let agent = format!("{}_server.plist", crate::get_full_name());
    let agent_plist_file = format!("/Library/LaunchAgents/{}", agent);
    if !prompt {
        // 在 macOS 13 上，有新的方式来检查它们是否在运行或已启用，https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos#Respond-to-changes-in-System-Settings
        if !std::path::Path::new(&format!("/Library/LaunchDaemons/{}", daemon)).exists() {
            return false;
        }
        if !std::path::Path::new(&agent_plist_file).exists() {
            return false;
        }
        return true;
    }

    let Some(install_script) = PRIVILEGES_SCRIPTS_DIR.get_file("install.scpt") else {
        return false;
    };
    let Some(install_script_body) = install_script.contents_utf8().map(correct_app_name) else {
        return false;
    };

    let Some(daemon_plist) = PRIVILEGES_SCRIPTS_DIR.get_file("daemon.plist") else {
        return false;
    };
    let Some(daemon_plist_body) = daemon_plist.contents_utf8().map(correct_app_name) else {
        return false;
    };

    let Some(agent_plist) = PRIVILEGES_SCRIPTS_DIR.get_file("agent.plist") else {
        return false;
    };
    let Some(agent_plist_body) = agent_plist.contents_utf8().map(correct_app_name) else {
        return false;
    };

    std::thread::spawn(move || {
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(install_script_body)
            .arg(daemon_plist_body)
            .arg(agent_plist_body)
            .arg(&get_active_username())
            .status()
        {
            Err(e) => {
                log::error!("run osascript failed: {}", e);
            }
            _ => {
                let installed = std::path::Path::new(&agent_plist_file).exists();
                log::info!("Agent file {} installed: {}", agent_plist_file, installed);
                if installed {
                    log::info!("launch server");
                    std::process::Command::new("launchctl")
                        .args(&["load", "-w", &agent_plist_file])
                        .status()
                        .ok();
                }
            }
        }
    });
    false
}

enum UpdateSource {
    AppDir(String),
    VerifiedDmg {
        path: String,
        expected_sha256: String,
    },
}

fn log_update_cleanup_warning(output: &std::process::Output) {
    if String::from_utf8_lossy(&output.stdout).contains(UPDATE_CLEANUP_FAILED_AFTER_COMMIT) {
        log::warn!("Update committed, but temporary update cleanup failed");
    }
}

impl UpdateSource {
    fn into_script_args(self) -> (String, String) {
        match self {
            Self::AppDir(path) => (path, String::new()),
            Self::VerifiedDmg {
                path,
                expected_sha256,
            } => (path, expected_sha256),
        }
    }
}

fn update_daemon_agent(agent_plist_file: String, update_source: UpdateSource) -> ResultType<()> {
    let update_script_file = "update.scpt";
    let Some(update_script) = PRIVILEGES_SCRIPTS_DIR.get_file(update_script_file) else {
        bail!("Failed to find {}", update_script_file);
    };
    let Some(update_script_body) = update_script.contents_utf8().map(correct_app_name) else {
        bail!("Failed to read {}", update_script_file);
    };

    let current_pid = std::process::id().to_string();
    let (update_source_path, expected_sha256) = update_source.into_script_args();
    let func = move || -> ResultType<()> {
        let mut binding = std::process::Command::new("osascript");
        let cmd = binding
            .arg("-e")
            .arg(update_script_body)
            .arg(&get_active_username())
            .arg(&current_pid)
            .arg(update_source_path)
            .arg(expected_sha256);
        match cmd.output() {
            Err(e) => {
                log::error!("run osascript failed: {}", e);
                bail!("run osascript failed: {}", e);
            }
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "run osascript failed with status: {}, stderr: {}",
                    output.status,
                    stderr.trim()
                );
                bail!(
                    "run osascript failed with status: {}, stderr: {}",
                    output.status,
                    stderr.trim()
                );
            }
            Ok(output) => {
                log_update_cleanup_warning(&output);
                let installed = std::path::Path::new(&agent_plist_file).exists();
                log::info!("Agent file {} installed: {}", &agent_plist_file, installed);
            }
        }
        Ok(())
    };
    func()
}

fn correct_app_name(s: &str) -> String {
    let mut s = s.to_owned();
    if let Some(bundleid) = get_bundle_id() {
        s = s.replace("com.carriez.rustdesk", &bundleid);
    }
    s = s.replace("rustdesk", &crate::get_app_name().to_lowercase());
    s = s.replace("RustDesk", &crate::get_app_name());
    s
}

fn write_plist_atomically(path: &str, body: &str) -> ResultType<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let temporary = format!("{}.tmp.{}", path, std::process::id());
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}

pub fn write_plists() -> ResultType<()> {
    let daemon_plist_path = format!(
        "/Library/LaunchDaemons/com.carriez.{}_service.plist",
        crate::get_app_name()
    );
    let agent_plist_path = format!(
        "/Library/LaunchAgents/com.carriez.{}_server.plist",
        crate::get_app_name()
    );
    let Some(daemon_plist) = PRIVILEGES_SCRIPTS_DIR.get_file("daemon.plist") else {
        bail!("daemon.plist not found in embedded resources");
    };
    let Some(daemon_plist_body) = daemon_plist.contents_utf8().map(correct_app_name) else {
        bail!("Failed to read daemon.plist");
    };
    let Some(agent_plist) = PRIVILEGES_SCRIPTS_DIR.get_file("agent.plist") else {
        bail!("agent.plist not found in embedded resources");
    };
    let Some(agent_plist_body) = agent_plist.contents_utf8().map(correct_app_name) else {
        bail!("Failed to read agent.plist");
    };
    write_plist_atomically(&daemon_plist_path, &daemon_plist_body)?;
    write_plist_atomically(&agent_plist_path, &agent_plist_body)?;
    log::info!("[write-plists] Wrote daemon and agent plists");
    Ok(())
}

pub fn uninstall_service(show_new_window: bool, sync: bool) -> bool {
    // 待办：与 win/linux 一起重构启动/停止服务
    if !is_installed_daemon(false) {
        return false;
    }

    let Some(script_file) = PRIVILEGES_SCRIPTS_DIR.get_file("uninstall.scpt") else {
        return false;
    };
    let Some(script_body) = script_file.contents_utf8().map(correct_app_name) else {
        return false;
    };

    let func = move || {
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(script_body)
            .status()
        {
            Err(e) => {
                log::error!("run osascript failed: {}", e);
            }
            _ => {
                let agent = format!("{}_server.plist", crate::get_full_name());
                let agent_plist_file = format!("/Library/LaunchAgents/{}", agent);
                let uninstalled = !std::path::Path::new(&agent_plist_file).exists();
                log::info!(
                    "Agent file {} uninstalled: {}",
                    agent_plist_file,
                    uninstalled
                );
                if uninstalled {
                    if !show_new_window {
                        let _ = crate::ipc::close_all_instances();
                        // 给 ipc 留一点时间
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    crate::ipc::set_option("stop-service", "Y");
                    std::process::Command::new("launchctl")
                        .args(&["remove", &format!("{}_server", crate::get_full_name())])
                        .status()
                        .ok();
                    if show_new_window {
                        std::process::Command::new("open")
                            .arg("-n")
                            .arg(&format!("/Applications/{}.app", crate::get_app_name()))
                            .spawn()
                            .ok();
                        // 给 open 留一点时间
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    quit_gui();
                }
            }
        }
    };
    if sync {
        func();
    } else {
        std::thread::spawn(func);
    }
    true
}

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    unsafe {
        let e = CGEventCreate(0 as _);
        let point = CGEventGetLocation(e);
        CFRelease(e);
        Some((point.x as _, point.y as _))
    }
    /*
    let mut pt: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    let screen: id = unsafe { msg_send![class!(NSScreen), currentScreenForMouseLocation] };
    let frame: NSRect = unsafe { msg_send![screen, frame] };
    pt.x -= frame.origin.x;
    pt.y -= frame.origin.y;
    Some((pt.x as _, pt.y as _))
    */
}

/// 把鼠标光标移动到指定的屏幕位置。
///
/// # 线程安全
/// 该函数影响全局光标状态，并会获取 `CG_CURSOR_MUTEX`。
/// 调用方必须确保持有该互斥锁期间不会发生嵌套调用。
///
/// # 参数
/// * `x` —— 以屏幕点为单位的 X 坐标（macOS 用的是点，不是像素）
/// * `y` —— 以屏幕点为单位的 Y 坐标
pub fn set_cursor_pos(x: i32, y: i32) -> bool {
    // 在 debug 构建中带死锁检测地获取锁。
    // 在 debug 构建中，try_lock 能及早发现重入调用；失败时我们立即返回。
    // 在 release 构建中，我们使用阻塞式 lock()，在有竞争时会等待。
    #[cfg(debug_assertions)]
    let _guard = match CG_CURSOR_MUTEX.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => {
            log::error!(
                "[BUG] set_cursor_pos: CG_CURSOR_MUTEX is already held - potential deadlock!"
            );
            debug_assert!(false, "Re-entrant call to set_cursor_pos detected");
            return false;
        }
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
    };
    #[cfg(not(debug_assertions))]
    let _guard = CG_CURSOR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        let result = CGWarpMouseCursorPosition(CGPoint {
            x: x as f64,
            y: y as f64,
        });
        if result != CGError::Success {
            log::error!(
                "CGWarpMouseCursorPosition({}, {}) returned error: {:?}",
                x,
                y,
                result
            );
        }
        result == CGError::Success
    }
}

/// 切换指针锁定（解除/建立鼠标与光标位置的关联）。
///
/// 在 macOS 上，不支持像 Windows ClipCursor 那样直接裁剪光标。
/// 我们改用 CGAssociateMouseAndMouseCursorPosition 把鼠标
/// 的移动与光标位置解除关联，从而实现"指针锁定"效果。
///
/// # 线程安全
/// 该函数影响全局光标状态，并会获取 `CG_CURSOR_MUTEX`。
/// 调用方必须确保同一时刻只有一个持有者切换指针锁定；
/// 来自不同调用点的 Some/None 嵌套切换可能引发意外行为。
///
/// # 参数
/// * `rect` —— 为 `Some(_)` 时，把鼠标与光标解除关联（启用指针锁定）。
///            为 `None` 时，重新建立鼠标与光标的关联（禁用指针锁定）。
///            在 macOS 上 rect 的坐标值会被忽略，只有 `Some`/`None` 才有意义。
///            参数签名与 Windows 保持一致，以统一 API。
pub fn clip_cursor(rect: Option<(i32, i32, i32, i32)>) -> bool {
    // 在 debug 构建中带死锁检测地获取锁。
    // 在 debug 构建中，try_lock 能及早发现重入调用；失败时我们立即返回。
    // 在 release 构建中，我们使用阻塞式 lock()，在有竞争时会等待。
    #[cfg(debug_assertions)]
    let _guard = match CG_CURSOR_MUTEX.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => {
            log::error!("[BUG] clip_cursor: CG_CURSOR_MUTEX is already held - potential deadlock!");
            debug_assert!(false, "Re-entrant call to clip_cursor detected");
            return false;
        }
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
    };
    #[cfg(not(debug_assertions))]
    let _guard = CG_CURSOR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    // CGAssociateMouseAndMouseCursorPosition 接受一个 boolean_t：
    //   1（true） = 把鼠标与光标位置关联（普通模式）
    //   0（false）= 把鼠标与光标位置解除关联（指针锁定模式）
    // 当 rect 为 Some 时，我们需要指针锁定（解除关联），所以 associate = false (0)。
    // 当 rect 为 None 时，我们需要普通模式（建立关联），所以 associate = true (1)。
    let associate: BooleanT = if rect.is_some() { 0 } else { 1 };
    unsafe {
        let result = CGAssociateMouseAndMouseCursorPosition(associate);
        if result != CGError::Success {
            log::warn!(
                "CGAssociateMouseAndMouseCursorPosition({}) returned error: {:?}",
                associate,
                result
            );
        }
        result == CGError::Success
    }
}

pub fn get_focused_display(displays: Vec<DisplayInfo>) -> Option<usize> {
    autoreleasepool(|| unsafe_get_focused_display(displays))
}

fn unsafe_get_focused_display(displays: Vec<DisplayInfo>) -> Option<usize> {
    unsafe {
        let main_screen: id = msg_send![class!(NSScreen), mainScreen];
        let screen: id = msg_send![main_screen, deviceDescription];
        let id: id =
            msg_send![screen, objectForKey: NSString::alloc(nil).init_str("NSScreenNumber")];
        let display_name: u32 = msg_send![id, unsignedIntValue];

        displays
            .iter()
            .position(|d| d.name == display_name.to_string())
    }
}

pub fn get_cursor() -> ResultType<Option<u64>> {
    autoreleasepool(|| unsafe_get_cursor())
}

// 种子标识的是一次光标变化，而不是某个光标：形状由其内容在
// 被捕获后命名——在所有平台上都是如此。由尺寸、热点和两个像素构成的指纹无法区分张开的
// 和握紧的手，也认不出带徽标的箭头，于是在它们之间切换时不会发送变化。
fn unsafe_get_cursor() -> ResultType<Option<u64>> {
    unsafe {
        let seed = CGSCurrentCursorSeed();
        if seed == LATEST_SEED {
            return Ok(None);
        }
        LATEST_SEED = seed;
        Ok(Some(seed as u32 as u64))
    }
}

pub fn reset_input_cache() {
    unsafe {
        LATEST_SEED = 0;
    }
}

pub fn get_cursor_data(hcursor: u64) -> ResultType<CursorData> {
    autoreleasepool(|| unsafe_get_cursor_data(hcursor))
}

// https://github.com/stweil/OSXvnc/blob/master/OSXvnc-server/mousecursor.c
fn unsafe_get_cursor_data(hcursor: u64) -> ResultType<CursorData> {
    unsafe {
        // 即使在 get_cursor 读取种子之后光标发生了变化，所捕获的仍是显示中的那个光标：
        // 内容标识形状，而那次变化已经推进了种子，所以下一次轮询才会捕获到它。
        let c: id = msg_send![class!(NSCursor), currentSystemCursor];
        if c == nil {
            bail!("Failed to call [NSCursor currentSystemCursor]");
        }
        let hotspot: NSPoint = msg_send![c, hotSpot];
        let img: id = msg_send![c, image];
        let size: NSSize = msg_send![img, size];
        let reps: id = msg_send![img, representations];
        if reps == nil {
            bail!("Failed to call [NSImage representations]");
        }
        let nreps: usize = msg_send![reps, count];
        if nreps == 0 {
            bail!("Get empty [NSImage representations]");
        }
        let rep: id = msg_send![reps, objectAtIndex: 0];
        /*
        let n: id = msg_send![class!(NSNumber), numberWithFloat:1.0];
        let props: id = msg_send![class!(NSDictionary), dictionaryWithObject:n forKey:NSString::alloc(nil).init_str("NSImageCompressionFactor")];
        let image_data: id = msg_send![rep, representationUsingType:2 properties:props];
        let () = msg_send![image_data, writeToFile:NSString::alloc(nil).init_str("cursor.jpg") atomically:0];
        */
        let mut colors: Vec<u8> = Vec::new();
        colors.reserve((size.height * size.width) as usize * 4);
        // TIFF 是 rgb 色彩空间，无需转换
        // let cs: id = msg_send![class!(NSColorSpace), sRGBColorSpace];
        for y in 0..(size.height as _) {
            for x in 0..(size.width as _) {
                let color: id = msg_send![rep, colorAtX:x as cocoa::foundation::NSInteger y:y as cocoa::foundation::NSInteger];
                // let color: id = msg_send![color, colorUsingColorSpace: cs];
                if color == nil {
                    continue;
                }
                let r: f64 = msg_send![color, redComponent];
                let g: f64 = msg_send![color, greenComponent];
                let b: f64 = msg_send![color, blueComponent];
                let a: f64 = msg_send![color, alphaComponent];
                colors.push((r * 255.) as _);
                colors.push((g * 255.) as _);
                colors.push((b * 255.) as _);
                colors.push((a * 255.) as _);
            }
        }
        Ok(CursorData {
            id: hcursor,
            colors: colors.into(),
            hotx: hotspot.x as _,
            hoty: hotspot.y as _,
            width: size.width as _,
            height: size.height as _,
            ..Default::default()
        })
    }
}

fn get_active_user(t: &str) -> String {
    if let Ok(output) = std::process::Command::new("ls")
        .args(vec![t, "/dev/console"])
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(n) = line.split_whitespace().nth(2) {
                return n.to_owned();
            }
        }
    }
    "".to_owned()
}

pub fn get_active_username() -> String {
    get_active_user("-l")
}

pub fn get_active_userid() -> String {
    get_active_user("-n")
}

/// 返回所有带 login-window/session 条目的 UID。快速用户切换
/// 可能同时留下多个 GUI bootstrap domain 存活，所以只更新
/// 控制台用户，可能会让另一个用户的 agent 留在旧的 bundle 上。
pub(crate) fn get_logged_in_uids() -> Vec<u32> {
    let mut uids = std::collections::BTreeSet::new();
    if let Ok(output) = std::process::Command::new("/usr/bin/who").output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some(username) = line.split_whitespace().next() else {
                continue;
            };
            let Ok(output) = std::process::Command::new("/usr/bin/id")
                .args(["-u", username])
                .output()
            else {
                continue;
            };
            let Ok(uid) = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u32>()
            else {
                continue;
            };
            let gui_domain = format!("gui/{}", uid);
            if std::process::Command::new("/bin/launchctl")
                .args(["print", &gui_domain])
                .output()
                .is_ok_and(|output| output.status.success())
            {
                uids.insert(uid);
            }
        }
    }
    if let Ok(active_uid) = get_active_userid().parse::<u32>() {
        if active_uid == 0 {
            // 当 LoginWindow 会话处于活动状态时，/dev/console 归 UID 0 所有。
            // 即使同时存在快速切换产生的 GUI domain，也要查询该服务器。
            uids.insert(0);
        } else {
            let gui_domain = format!("gui/{}", active_uid);
            if std::process::Command::new("/bin/launchctl")
                .args(["print", &gui_domain])
                .output()
                .is_ok_and(|output| output.status.success())
            {
                uids.insert(active_uid);
            }
        }
    }
    if uids.is_empty() {
        // 登录窗口没有普通的 gui/0 bootstrap domain。
        uids.insert(0);
    }
    uids.into_iter().collect()
}

pub fn get_active_user_home() -> Option<PathBuf> {
    let username = get_active_username();
    if !username.is_empty() {
        let home = PathBuf::from(format!("/Users/{}", username));
        if home.exists() {
            return Some(home);
        }
    }
    None
}

pub fn is_prelogin() -> bool {
    get_active_userid() == "0"
}

// https://stackoverflow.com/questions/11505255/osx-check-if-the-screen-is-locked
// macOS 未锁屏时找不到 "CGSSessionScreenIsLocked"。
//
// `ioreg -n Root -d1` 返回 `"CGSSessionScreenIsLocked"=Yes`
// `ioreg -n Root -d1 -a` 返回
// ```
// ...
//    <key>CGSSessionScreenIsLocked</key>
//    <true/>
// ...
// ```
pub fn is_locked() -> bool {
    match std::process::Command::new("ioreg")
        .arg("-n")
        .arg("Root")
        .arg("-d1")
        .output()
    {
        Ok(output) => {
            let output_str = String::from_utf8_lossy(&output.stdout);
            // 虽然在我的 macOS 上打印出的是 `"CGSSessionScreenIsLocked"=Yes`，
            // 我还额外检查 `"CGSSessionScreenIsLocked"=true`，以获得更好的兼容性。
            output_str.contains("\"CGSSessionScreenIsLocked\"=Yes")
                || output_str.contains("\"CGSSessionScreenIsLocked\"=true")
        }
        Err(e) => {
            log::error!("Failed to query ioreg for the lock state: {}", e);
            false
        }
    }
}

pub fn is_logon_screen() -> bool {
    is_prelogin() || is_locked()
}

pub fn is_root() -> bool {
    crate::username() == "root"
}

pub fn run_as_user(arg: Vec<&str>) -> ResultType<Option<std::process::Child>> {
    let uid = get_active_userid();
    let cmd = std::env::current_exe()?;
    let mut args = vec!["asuser", &uid, cmd.to_str().unwrap_or("")];
    args.append(&mut arg.clone());
    let task = std::process::Command::new("launchctl").args(args).spawn()?;
    Ok(Some(task))
}

pub fn lock_screen() {
    std::process::Command::new(
        "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession",
    )
    .arg("-suspend")
    .output()
    .ok();
}

/// 启动 macOS 系统服务 IPC 监听器和后台的
/// 静默自动更新线程。
pub fn start_os_service() {
    log::info!("Username: {}", crate::username());
    // 静默自动更新 —— 通过 LaunchDaemon 以 root 身份运行，无需 osascript 对话框
    crate::updater::start_auto_update_macos();
    if let Err(err) = crate::ipc::start("_service") {
        log::error!("Failed to start ipc_service: {}", err);
    }

    /* // 现在借助 launchctl asuser，鼠标/键盘在登录前也可以工作。
       // 下面这种方式可以避免多用户同时登录的问题，但又有它自己的问题。
       // 还没找到在没有 root 权限的情况下启动 --cm 的好办法（会影响文件传输）。
       // 一种方法是用 `launchctl asuser <uid> open -n -a /Applications/RustDesk.app/ --args --cm` 启动，
       // 这样 --cm 是以用户权限启动的，但我们在访达里用 open 再打开另一个 RustDesk.app 时会有问题
       // 。
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        let mut uid = "".to_owned();
        let mut server: Option<std::process::Child> = None;
        if let Err(err) = ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        }) {
            println!("Failed to set Ctrl-C handler: {}", err);
        }
        while running.load(Ordering::SeqCst) {
            let tmp = get_active_userid();
            let mut start_new = false;
            if tmp != uid && !tmp.is_empty() {
                uid = tmp;
                log::info!("active uid: {}", uid);
                if let Some(ps) = server.as_mut() {
                    hbb_common::allow_err!(ps.kill());
                }
            }
            if let Some(ps) = server.as_mut() {
                match ps.try_wait() {
                    Ok(Some(_)) => {
                        server = None;
                        start_new = true;
                    }
                    _ => {}
                }
            } else {
                start_new = true;
            }
            if start_new {
                match run_as_user("--server") {
                    Ok(Some(ps)) => server = Some(ps),
                    Err(err) => {
                        log::error!("Failed to start server: {}", err);
                    }
                    _ => { /*no happen*/ }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(super::SERVICE_INTERVAL));
        }

        if let Some(ps) = server.take().as_mut() {
            hbb_common::allow_err!(ps.kill());
        }
        log::info!("Exit");
    */
}

pub fn toggle_blank_screen(_v: bool) {
    // https://unix.stackexchange.com/questions/17115/disable-keyboard-mouse-temporarily
}

pub fn block_input(_v: bool) -> (bool, String) {
    (true, "".to_owned())
}

pub fn is_installed() -> bool {
    if let Ok(p) = std::env::current_exe() {
        return p
            .to_str()
            .unwrap_or_default()
            .starts_with(&format!("/Applications/{}.app", crate::get_app_name()));
    }
    false
}

pub fn quit_gui() {
    unsafe {
        let () = msg_send!(NSApp(), terminate: nil);
    };
}

pub fn update_me() -> ResultType<()> {
    let cmd = std::env::current_exe()?;
    // RustDesk.app/Contents/MacOS/RustDesk
    let app_dir = cmd
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|d| d.to_string_lossy().to_string());
    let Some(app_dir) = app_dir else {
        bail!("Unknown app directory of current exe file: {:?}", cmd);
    };
    update_me_from_app_dir(app_dir)
}

fn update_me_from_app_dir(app_dir: String) -> ResultType<()> {
    update_me_from_source(UpdateSource::AppDir(app_dir))
}

fn update_me_from_source(update_source: UpdateSource) -> ResultType<()> {
    let app_name = crate::get_app_name();
    validate_install_app_name(&app_name)?;
    let is_installed_daemon = is_installed_daemon(false);
    let option_stop_service = "stop-service";
    let is_service_stopped = hbb_common::config::option2bool(
        option_stop_service,
        &crate::ui_interface::get_option(option_stop_service),
    );

    // 仅在已安装的服务处于启用状态时，才把 launchd 作业纳入该事务。
    if is_installed_daemon && !is_service_stopped {
        let agent = format!("{}_server.plist", crate::get_full_name());
        let agent_plist_file = format!("/Library/LaunchAgents/{}", agent);
        update_daemon_agent(agent_plist_file, update_source)?;
    } else {
        let (update_source_path, expected_sha256) = update_source.into_script_args();
        let expected_bundle_id = get_bundle_id()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("client bundle identifier is missing"))?;
        let output = Command::new("osascript")
            .arg("-e")
            .arg(PRIVILEGED_UPDATE_BODY)
            .arg(app_name.to_string())
            .arg(std::process::id().to_string())
            .arg(update_source_path)
            .arg(get_active_username())
            .arg(if is_installed_daemon { "0" } else { "1" })
            .arg(expected_sha256)
            .arg(expected_bundle_id)
            .output();
        match output {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::error!(
                    "osascript execution failed with status: {}, stderr: {}",
                    output.status,
                    stderr.trim()
                );
                bail!(
                    "osascript execution failed with status: {}, stderr: {}",
                    output.status,
                    stderr.trim()
                );
            }
            Err(e) => {
                log::error!("run osascript failed: {}", e);
                bail!("run osascript failed: {}", e);
            }
            Ok(output) => log_update_cleanup_warning(&output),
        }
    }
    match Command::new("open")
        .arg("-n")
        .arg(&format!("/Applications/{}.app", app_name))
        .status()
    {
        Ok(status) if !status.success() => {
            log::warn!("Failed to relaunch updated app: {}", status);
        }
        Err(err) => {
            log::warn!("Failed to relaunch updated app: {}", err);
        }
        _ => {}
    }
    // 给 open 留一点时间
    std::thread::sleep(std::time::Duration::from_millis(300));
    Ok(())
}

pub fn update_from_dmg(dmg_path: &str) -> ResultType<()> {
    let update_temp_dir = get_update_temp_dir_string();
    println!("Starting update from DMG: {}", dmg_path);
    let update_result = (|| {
        let copied_dmg = copy_dmg_to_update_temp_file(dmg_path)?;
        let guard = attach_dmg(&copied_dmg.to_string_lossy())?;
        update_me_from_app_dir(format!(
            "{}/{}.app",
            guard.mount_point(),
            crate::get_app_name()
        ))
    })();
    try_remove_temp_update_dir(Some(&update_temp_dir));
    update_result?;
    println!("Update process started");
    Ok(())
}

pub fn update_to_verified_dmg(
    file: &str,
    expected_sha256: &str,
    expected_size: Option<u64>,
) -> ResultType<()> {
    let verified_dmg = copy_and_verify_dmg_file(file, expected_sha256, expected_size)?;
    update_from_verified_dmg(&verified_dmg)?;
    // 在这个进程退出之前移除私有 DMG 副本。
    drop(verified_dmg);
    quit_gui();
    Ok(())
}

fn update_from_verified_dmg(verified_dmg: &VerifiedDmg) -> ResultType<()> {
    println!("Starting update from verified DMG");
    update_me_from_source(verified_dmg_update_source(verified_dmg)?)?;
    println!("Update process started");
    Ok(())
}

fn backup_update_plist(source: &str, backup: &str) -> ResultType<()> {
    match std::fs::symlink_metadata(source) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                bail!("[root-update] plist is not a regular file: {}", source);
            }
            std::fs::copy(source, backup)?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("[root-update] required installed plist is missing: {}", source)
        }
        Err(err) => Err(err.into()),
    }
}

fn validate_update_tree(path: &Path, framework_root: Option<&Path>) -> ResultType<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        // 框架（framework）合法地使用内部符号链接（Resources、
        // Versions/Current），但绝不允许链接指向其框架之外。
        let Some(framework_root) = framework_root else {
            bail!("[root-update] symlink outside framework: {}", path.display());
        };
        let target = std::fs::read_link(path)?;
        let target = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(Path::new("/")).join(target)
        };
        let target = std::fs::canonicalize(target)?;
        let framework_root = std::fs::canonicalize(framework_root)?;
        if target.starts_with(&framework_root) {
            return Ok(());
        }
        bail!("[root-update] symlink in update bundle: {}", path.display());
    }
    if metadata.file_type().is_dir() {
        for entry in std::fs::read_dir(path)? {
            let child = entry?.path();
            let child_framework_root = if child
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".framework"))
                && std::fs::symlink_metadata(&child)?.file_type().is_dir()
            {
                Some(child.as_path())
            } else {
                framework_root
            };
            validate_update_tree(&child, child_framework_root)?;
        }
    } else if !metadata.file_type().is_file() {
        bail!(
            "[root-update] unsupported file in update bundle: {}",
            path.display()
        );
    }
    Ok(())
}

fn read_bundle_plist_value(app: &Path, key: &str) -> ResultType<String> {
    let info_plist = app.join("Contents/Info.plist");
    let plist_command = format!("Print :{key}");
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", plist_command.as_str()])
        .arg(info_plist)
        .output()?;
    if !output.status.success() {
        bail!(
            "[root-update] failed to read bundle plist value {key}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|err| anyhow!("[root-update] bundle plist value {key} is not UTF-8: {err}"))?;
    if value.trim().is_empty() {
        bail!("[root-update] bundle plist value {key} is empty");
    }
    Ok(value.trim().to_owned())
}

fn verify_update_app_identity(installed: &Path, candidate: &Path) -> ResultType<()> {
    let installed_id = read_bundle_plist_value(installed, "CFBundleIdentifier")?;
    let candidate_id = read_bundle_plist_value(candidate, "CFBundleIdentifier")?;
    if installed_id != candidate_id {
        bail!("[root-update] candidate bundle identifier mismatch");
    }
    Ok(())
}

/// 从 DMG 文件执行静默更新，不显示任何 osascript 对话框。
/// 必须由以 root 身份运行的进程调用（例如服务二进制）。
pub fn update_from_dmg_as_root(dmg_path: &str, expected_version: &str) -> ResultType<()> {
    let app_name = crate::get_app_name();
    validate_install_app_name(&app_name)?;
    let app_bundle = format!("/Applications/{}.app", app_name);
    let tmp_dir_output = std::process::Command::new("/usr/bin/mktemp")
        .args(&["-d", "/tmp/.rustdeskupdate-root-XXXXXX"])
        .output()?;
    let tmp_dir = String::from_utf8(tmp_dir_output.stdout)
        .map_err(|e| anyhow!("[root-update] mktemp output error: {}", e))?
        .trim()
        .to_string();
    if tmp_dir.is_empty() {
        bail!("[root-update] Failed to create temp directory");
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let agent_plist = format!("/Library/LaunchAgents/com.carriez.{}_server.plist", app_name);
    let daemon_plist = format!("/Library/LaunchDaemons/com.carriez.{}_service.plist", app_name);

    log::info!("[root-update] Starting silent root update from {}", dmg_path);
    // 解压前先检查会话，避免无谓的工作
    if !crate::updater::has_no_active_conns_ipc() {
        bail!("[root-update] Active session detected, deferring update.");
    }
    // 把 DMG 解压到临时目录
    extract_dmg_into_existing_dir(dmg_path, &tmp_dir)?;
    let src_app = format!("{}/{}.app", tmp_dir, app_name);
    log::info!("[root-update] DMG extracted to {}", tmp_dir);
    validate_update_tree(Path::new(&src_app), None)?;
    verify_update_app_identity(Path::new(&app_bundle), Path::new(&src_app))?;

    // 把下载到的资源与更新服务返回的版本绑定
    // ，在改动 plist 或执行暂存 bundle 中的任何东西之前
    // 。文件名正确但 bundle 错误的发布资源
    // 绝不能被允许替换已安装的应用。
    let info_plist = format!("{}/Contents/Info.plist", src_app);
    let staged_version_result = (|| -> ResultType<String> {
        let output = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleShortVersionString", &info_plist])
            .output()?;
        if !output.status.success() {
            bail!(
                "[root-update] failed to read staged bundle version: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let version = String::from_utf8(output.stdout)
            .map_err(|err| anyhow!("[root-update] staged bundle version is not UTF-8: {}", err))?;
        if version.trim().is_empty() {
            bail!("[root-update] staged bundle version is empty");
        }
        Ok(version.trim().to_owned())
    })();
    let staged_version = match staged_version_result {
        Ok(version) => version,
        Err(err) => {
            if let Err(cleanup_err) = std::fs::remove_dir_all(&tmp_dir) {
                log::warn!(
                    "[root-update] Failed to remove temp dir {}: {}",
                    tmp_dir,
                    cleanup_err
                );
            }
            return Err(err);
        }
    };
    if staged_version != expected_version {
        if let Err(err) = std::fs::remove_dir_all(&tmp_dir) {
            log::warn!(
                "[root-update] Failed to remove temp dir {}: {}",
                tmp_dir,
                err
            );
        }
        bail!(
            "[root-update] staged bundle version mismatch: expected {:?}, found {:?}",
            expected_version,
            staged_version
        );
    }

    // 残留的备份会让 `mv app app.bak` 把正在使用的 bundle 嵌套到
    // 旧目录里面，而不是创建一份事务备份。绝不
    // 覆盖或猜测早期被中断的更新所遗留的恢复状态；
    // 应先让管理员检查它。
    let app_backup = format!("{}.bak", app_bundle);
    let failed_bundle = format!("{}.failed-update", app_bundle);
    for recovery_path in [&app_backup, &failed_bundle] {
        match std::fs::symlink_metadata(recovery_path) {
            Ok(_) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                bail!(
                    "[root-update] stale application recovery path requires inspection: {}",
                    recovery_path
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return Err(err.into());
            }
        }
    }

    // 覆盖前先备份当前的 plist —— 重新加载失败时需要用它恢复
    let daemon_plist_bak = format!("{}/daemon_plist.bak", tmp_dir);
    let agent_plist_bak = format!("{}/agent_plist.bak", tmp_dir);
    // 备份是更新事务的一部分。不允许新的
    // 服务二进制覆盖任何一个正在使用的 plist，除非两个已安装的
    // 定义都已成功备份。
    backup_update_plist(&daemon_plist, &daemon_plist_bak)?;
    backup_update_plist(&agent_plist, &agent_plist_bak)?;

    // 在继续之前，确保暂存的发布包中包含服务可执行文件。
    // plist 的生成本身就在这个已经是 root 的进程中完成；
    // 不需要从 /tmp 启动刚解压出来的服务二进制。
    let new_service = format!("{}/Contents/MacOS/service", src_app);
    if !std::path::Path::new(&new_service).is_file() {
        bail!("[root-update] staged service binary is missing: {}", new_service);
    }
    // 新二进制会在 bundle 被
    // 移到最终的 root 所有的位置之后，自己写出它的 plist 定义。这样就避免了执行
    // 直接从 /tmp 来的代码，同时确保 plist 与新发布版本一致。

    // 解压后的最后一次会话检查 —— 尽量缩小竞态窗口
    if !crate::updater::has_no_active_conns_ipc() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        bail!("[root-update] Active session detected after extraction, deferring update.");
    }

    // 在获取受影响用户的快照之前，先让分离脚本的启动稳定下来。
    // 之后的最终 IPC 检查则在延迟之后、尽可能接近
    // 停止那些确切的 launchd domain 的时刻进行。
    std::thread::sleep(std::time::Duration::from_secs(3));
    if !crate::updater::has_no_active_conns_ipc() {
        bail!("[root-update] active session started before update launch");
    }
    let logged_in_uids = get_logged_in_uids();
    // UID 在嵌入以 root 运行的 shell 脚本之前先被解析为整数，
    // 因此它们无法改变脚本的命令结构。
    let uid_list = logged_in_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");

    // 写一个 shell 脚本，在本函数返回后以分离方式运行。
    // 它运行时我们无法直接替换 /Applications/RustDesk.app，
    // 所以我们派生一个脚本去等待、结束进程、复制并重启。
    let daemon_label = format!("com.carriez.{}_service", app_name);
    let agent_label = format!("com.carriez.{}_server", app_name);
    let script_path = format!("{}/rustdesk_update.sh", tmp_dir);
    let script = format!(
        r#"#!/bin/sh
rollback_done=0
bundle_swapped=0
bootstrap_agent() {{
    agent_uid="$1"
    if [ "$agent_uid" != "0" ]; then
        launchctl bootstrap gui/"$agent_uid" "{agent_plist}" 2>/dev/null || \
            launchctl bootstrap user/"$agent_uid" "{agent_plist}" 2>/dev/null || \
            launchctl load -w "{agent_plist}" 2>/dev/null
    else
        # At the login window there is no gui/0 domain.  launchctl load uses
        # the plist's LoginWindow/Aqua session policy instead.
        launchctl load -w -S LoginWindow "{agent_plist}" 2>/dev/null || \
            launchctl load -w "{agent_plist}" 2>/dev/null
    fi
}}
bootstrap_agents() {{
    for agent_uid in {uid_list}; do
        bootstrap_agent "$agent_uid" || return 1
    done
}}
loginwindow_asid() {{
    root_user_info=$(launchctl print user/0 2>/dev/null || true)
    root_login_asid=$(printf '%s\n' "$root_user_info" | \
        awk '/^[[:space:]]*asid = [0-9]+[[:space:]]*$/ {{print $3; exit}}')
    case "$root_login_asid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    printf '%s\n' "$root_login_asid"
}}
bootout_agents() {{
    # Legacy launchctl commands can report success despite operational
    # failure. Treat these as requests; stop_agents verifies the result.
    stopping_loginwindow_asid=""
    for agent_uid in {uid_list}; do
        if [ "$agent_uid" != "0" ]; then
            launchctl bootout gui/"$agent_uid"/{agent_label} 2>/dev/null || true
            launchctl bootout user/"$agent_uid"/{agent_label} 2>/dev/null || true
        else
            # LoginWindow jobs run in a login/<asid> domain even though
            # legacy root `launchctl load` is issued from the system context.
            # Remove every applicable registration before killing the process
            # so KeepAlive cannot immediately respawn it.
            launchctl unload -w -S LoginWindow "{agent_plist}" 2>/dev/null || true
            stopping_loginwindow_asid=$(loginwindow_asid || true)
            if [ -n "$stopping_loginwindow_asid" ]; then
                launchctl bootout login/"$stopping_loginwindow_asid"/{agent_label} 2>/dev/null || true
            fi
            launchctl bootout user/0/{agent_label} 2>/dev/null || true
            launchctl bootout system/{agent_label} 2>/dev/null || true
            launchctl unload -w "{agent_plist}" 2>/dev/null || true
        fi
    done
}}
find_agent_pid() {{
    agent_uid="$1"
    for candidate_pid in $(pgrep -u "$agent_uid" -x {app_name} 2>/dev/null || true); do
        process_args=$(ps -p "$candidate_pid" -o args= 2>/dev/null || true)
        if printf '%s\n' "$process_args" | grep -F "/Applications/{app_name}.app/Contents/MacOS/{app_name}" >/dev/null && \
           printf '%s\n' "$process_args" | grep -E '(^|[[:space:]])--server([[:space:]]|$)' >/dev/null; then
            printf '%s\n' "$candidate_pid"
            return 0
        fi
    done
    return 1
}}
launchd_agent_pid() {{
    agent_uid="$1"
    agent_info=$(launchctl print gui/"$agent_uid"/{agent_label} 2>/dev/null || \
        launchctl print user/"$agent_uid"/{agent_label} 2>/dev/null || true)
    agent_job_pid=$(printf '%s\n' "$agent_info" | awk '/^[[:space:]]*pid = / {{print $3; exit}}')
    if [ -n "$agent_job_pid" ] && \
       printf '%s\n' "$agent_info" | grep -E '^[[:space:]]*state = running[[:space:]]*$' >/dev/null; then
        printf '%s\n' "$agent_job_pid"
        return 0
    fi
    return 1
}}
agent_pid_for_uid() {{
    agent_uid="$1"
    if [ "$agent_uid" = "0" ]; then
        # LoginWindow agents have no ordinary gui/0 bootstrap domain. Locate
        # the root-owned --server process and validate it below instead.
        find_agent_pid "$agent_uid"
    else
        launchd_agent_pid "$agent_uid"
    fi
}}
agent_process_matches() {{
    agent_uid="$1"
    agent_pid="$2"
    process_uid=$(ps -p "$agent_pid" -o uid= 2>/dev/null | tr -d '[:space:]')
    process_args=$(ps -p "$agent_pid" -o args= 2>/dev/null || true)
    [ "$process_uid" = "$agent_uid" ] && \
        printf '%s\n' "$process_args" | grep -F "/Applications/{app_name}.app/Contents/MacOS/{app_name}" >/dev/null && \
        printf '%s\n' "$process_args" | grep -E '(^|[[:space:]])--server([[:space:]]|$)' >/dev/null
}}
capture_stopping_agent_pids() {{
    stopping_agent_pids=""
    for agent_uid in {uid_list}; do
        for candidate_pid in $(pgrep -u "$agent_uid" -x {app_name} 2>/dev/null || true); do
            if agent_process_matches "$agent_uid" "$candidate_pid"; then
                stopping_agent_pids="$stopping_agent_pids $candidate_pid"
            fi
        done
    done
}}
terminate_agent_processes() {{
    for agent_uid in {uid_list}; do
        for candidate_pid in $(pgrep -u "$agent_uid" -x {app_name} 2>/dev/null || true); do
            if agent_process_matches "$agent_uid" "$candidate_pid"; then
                kill -KILL "$candidate_pid" 2>/dev/null || true
            fi
        done
    done
}}
terminate_user_bundle_processes() {{
    for agent_uid in {uid_list}; do
        for candidate_pid in $(pgrep -u "$agent_uid" -x {app_name} 2>/dev/null || true); do
            process_args=$(ps -p "$candidate_pid" -o args= 2>/dev/null || true)
            if printf '%s\n' "$process_args" | grep -F "/Applications/{app_name}.app/" >/dev/null; then
                kill -KILL "$candidate_pid" 2>/dev/null || true
            fi
        done
    done
}}
user_bundle_processes_absent() {{
    for agent_uid in {uid_list}; do
        for candidate_pid in $(pgrep -u "$agent_uid" -x {app_name} 2>/dev/null || true); do
            process_args=$(ps -p "$candidate_pid" -o args= 2>/dev/null || true)
            if printf '%s\n' "$process_args" | grep -F "/Applications/{app_name}.app/" >/dev/null; then
                return 1
            fi
        done
    done
    return 0
}}
stop_user_bundle_processes() {{
    terminate_user_bundle_processes
    for _ in $(/usr/bin/seq 1 30); do
        if user_bundle_processes_absent; then
            sleep 2
            user_bundle_processes_absent && return 0
        fi
        terminate_user_bundle_processes
        sleep 1
    done
    return 1
}}
agent_jobs_absent() {{
    for agent_uid in {uid_list}; do
        if [ "$agent_uid" != "0" ]; then
            if launchctl print gui/"$agent_uid"/{agent_label} >/dev/null 2>&1 || \
               launchctl print user/"$agent_uid"/{agent_label} >/dev/null 2>&1; then
                return 1
            fi
        else
            if launchctl print system/{agent_label} >/dev/null 2>&1 || \
               launchctl print user/0/{agent_label} >/dev/null 2>&1; then
                return 1
            fi
            if [ -n "$stopping_loginwindow_asid" ] && \
               launchctl print login/"$stopping_loginwindow_asid"/{agent_label} >/dev/null 2>&1; then
                return 1
            fi
        fi
        find_agent_pid "$agent_uid" >/dev/null 2>&1 && return 1
    done
    return 0
}}
captured_agent_pids_gone() {{
    for stopped_pid in $stopping_agent_pids; do
        kill -0 "$stopped_pid" 2>/dev/null && return 1
    done
    return 0
}}
agents_stopped() {{
    captured_agent_pids_gone && agent_jobs_absent
}}
stop_agents() {{
    bootout_agents
    terminate_agent_processes
    for _ in $(/usr/bin/seq 1 30); do
        if agents_stopped; then
            sleep 2
            agents_stopped && return 0
        fi
        terminate_agent_processes
        sleep 1
    done
    return 1
}}
capture_agent_snapshot() {{
    agent_pids=""
    for agent_uid in {uid_list}; do
        agent_pid=$(agent_pid_for_uid "$agent_uid" || true)
        [ -n "$agent_pid" ] || return 1
        [ -S "/tmp/{app_name}-$agent_uid/ipc" ] || return 1
        kill -0 "$agent_pid" 2>/dev/null || return 1
        agent_process_matches "$agent_uid" "$agent_pid" || return 1
        agent_pids="$agent_pids $agent_uid:$agent_pid"
    done
    return 0
}}
agent_snapshot_stable() {{
    for agent_entry in $agent_pids; do
        agent_uid=$(printf '%s\n' "$agent_entry" | cut -d: -f1)
        expected_pid=$(printf '%s\n' "$agent_entry" | cut -d: -f2)
        current_pid=$(agent_pid_for_uid "$agent_uid" || true)
        [ -n "$current_pid" ] && [ "$current_pid" = "$expected_pid" ] || return 1
        [ -S "/tmp/{app_name}-$agent_uid/ipc" ] || return 1
        kill -0 "$current_pid" 2>/dev/null || return 1
        agent_process_matches "$agent_uid" "$current_pid" || return 1
    done
    return 0
}}
agent_ready() {{
    for _ in $(/usr/bin/seq 1 30); do
        if capture_agent_snapshot; then
            sleep 2
            agent_snapshot_stable && return 0
        fi
        sleep 1
    done
    return 1
}}
daemon_snapshot_stable() {{
    stable_daemon_info=$(launchctl print system/{daemon_label} 2>/dev/null || true)
    stable_daemon_pid=$(printf '%s\n' "$stable_daemon_info" | awk '/^[[:space:]]*pid = / {{print $3; exit}}')
    [ -n "$daemon_pid" ] && \
        [ "$stable_daemon_pid" = "$daemon_pid" ] && \
        printf '%s\n' "$stable_daemon_info" | grep -E '^[[:space:]]*state = running[[:space:]]*$' >/dev/null && \
        [ -S "/tmp/{app_name}-service/ipc_service" ] && \
        kill -0 "$daemon_pid" 2>/dev/null
}}
daemon_ready() {{
    daemon_pid=""
    for _ in $(/usr/bin/seq 1 30); do
        daemon_info=$(launchctl print system/{daemon_label} 2>/dev/null || true)
        daemon_pid=$(printf '%s\n' "$daemon_info" | awk '/^[[:space:]]*pid = / {{print $3; exit}}')
        if [ -n "$daemon_pid" ] && \
           printf '%s\n' "$daemon_info" | grep -E '^[[:space:]]*state = running[[:space:]]*$' >/dev/null && \
           [ -S "/tmp/{app_name}-service/ipc_service" ] && \
           kill -0 "$daemon_pid" 2>/dev/null; then
            sleep 2
            daemon_snapshot_stable && return 0
        fi
        sleep 1
    done
    return 1
}}
capture_stopping_daemon_pid() {{
    stopping_daemon_info=$(launchctl print system/{daemon_label} 2>/dev/null || true)
    stopping_daemon_pid=$(printf '%s\n' "$stopping_daemon_info" | awk '/^[[:space:]]*pid = / {{print $3; exit}}')
}}
daemon_stopped() {{
    if [ -n "$stopping_daemon_pid" ] && kill -0 "$stopping_daemon_pid" 2>/dev/null; then
        return 1
    fi
    ! launchctl print system/{daemon_label} >/dev/null 2>&1
}}
stop_daemon() {{
    capture_stopping_daemon_pid
    # Command status is advisory. daemon_stopped verifies that both the
    # captured process generation and launchd registration are gone.
    launchctl bootout system/{daemon_label} 2>/dev/null || \
        launchctl unload -w "{daemon_plist}" 2>/dev/null || true
    for _ in $(/usr/bin/seq 1 30); do
        if daemon_stopped; then
            sleep 2
            daemon_stopped && return 0
        fi
        sleep 1
    done
    return 1
}}
write_new_plists() {{
    /Applications/{app_name}.app/Contents/MacOS/service --write-plists \
        >"{tmp_dir}/write-plists.log" 2>&1 &
    write_pid=$!
    for _ in $(/usr/bin/seq 1 60); do
        if ! kill -0 "$write_pid" 2>/dev/null; then
            wait "$write_pid"
            return $?
        fi
        sleep 1
    done
    kill -TERM "$write_pid" 2>/dev/null || true
    sleep 1
    kill -KILL "$write_pid" 2>/dev/null || true
    wait "$write_pid" 2>/dev/null || true
    return 124
}}
restore_old_bundle() {{
    [ "$bundle_swapped" -eq 1 ] || return 0
    if [ ! -d "{app_bundle}.bak" ] || [ -L "{app_bundle}.bak" ]; then
        echo "[root-update] CRITICAL: valid application backup is unavailable" >> {tmp_dir}/rustdesk_root_update.log
        return 1
    fi
    if [ -e "{app_bundle}" ] || [ -L "{app_bundle}" ]; then
        if [ -e "{app_bundle}.failed-update" ] || [ -L "{app_bundle}.failed-update" ] || \
           ! mv "{app_bundle}" "{app_bundle}.failed-update"; then
            echo "[root-update] CRITICAL: could not vacate failed bundle safely" >> {tmp_dir}/rustdesk_root_update.log
            return 1
        fi
    fi
    if ! mv "{app_bundle}.bak" "{app_bundle}"; then
        echo "[root-update] CRITICAL: failed to restore application bundle" >> {tmp_dir}/rustdesk_root_update.log
        if [ ! -e "{app_bundle}" ] && [ ! -L "{app_bundle}" ]; then
            mv "{app_bundle}.failed-update" "{app_bundle}" 2>/dev/null || true
        fi
        return 1
    fi
    rm -rf "{app_bundle}.failed-update" 2>/dev/null || true
    bundle_swapped=0
    return 0
}}
rollback_transaction() {{
    # Rollback restores and verifies unattended service state. It does not
    # guarantee relaunching GUI windows that were stopped by the transaction.
    [ "$rollback_done" -eq 0 ] || return 0
    rollback_done=1
    restore_failed=0
    stop_daemon || restore_failed=1
    capture_stopping_agent_pids
    stop_agents || restore_failed=1
    restore_old_bundle || restore_failed=1
    cp "{daemon_plist_bak}" "{daemon_plist}" || restore_failed=1
    cp "{agent_plist_bak}" "{agent_plist}" || restore_failed=1
    touch /var/root/.rustdeskupdate_failed || restore_failed=1
    if ! launchctl load -w "{daemon_plist}" 2>/dev/null && \
       ! launchctl bootstrap system "{daemon_plist}" 2>/dev/null; then
        restore_failed=1
    fi
    daemon_ready || restore_failed=1
    bootstrap_agents || restore_failed=1
    agent_ready || restore_failed=1
    if [ "$restore_failed" -eq 0 ] && \
       {{ ! daemon_snapshot_stable || ! agent_snapshot_stable; }}; then
        restore_failed=1
    fi
    if [ "$restore_failed" -ne 0 ]; then
        echo "[root-update] CRITICAL: rollback restoration failed" >> {tmp_dir}/rustdesk_root_update.log
    else
        echo "[root-update] Rollback daemon and agents verified healthy" >> {tmp_dir}/rustdesk_root_update.log
    fi
}}
trap rollback_transaction EXIT
gui_uids=""
for agent_uid in {uid_list}; do
    for pid in $(pgrep -u "$agent_uid" -x {app_name} || true); do
        process_args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        if printf '%s\n' "$process_args" | grep -F "/Applications/{app_name}.app/" >/dev/null && \
           ! printf '%s\n' "$process_args" | grep -E "(^|[[:space:]])(--server|--service|--update)([[:space:]]|$)" >/dev/null; then
            gui_uids="$gui_uids $agent_uid"
            break
        fi
    done
done
if ! capture_agent_snapshot; then
    echo "[root-update] old LaunchAgent readiness check failed before shutdown" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
capture_stopping_agent_pids
if ! stop_daemon; then
    echo "[root-update] daemon did not stop before bundle swap" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
if ! stop_agents; then
    echo "[root-update] old LaunchAgent did not stop before bundle swap" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
# Agents have already been verified absent. Stop and verify any remaining GUI
# processes as well so no process keeps the old bundle mapped across the swap.
if ! stop_user_bundle_processes; then
    echo "[root-update] RustDesk GUI process did not stop before bundle swap" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
staged_bundle="{tmp_dir}/staged.app"
if [ -e "$staged_bundle" ] || [ -L "$staged_bundle" ]; then
    echo "[root-update] staged bundle path already exists, aborting" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
if ! ditto {src_app} "$staged_bundle" 2>/dev/null; then
    echo "[root-update] ditto failed, aborting update" >> {tmp_dir}/rustdesk_root_update.log
    rm -rf "$staged_bundle"
    exit 1
fi
# Validate staged bundle before atomic swap
if [ ! -d "$staged_bundle/Contents/MacOS" ] || \
   [ ! -f "$staged_bundle/Contents/MacOS/{app_name}" ] || \
   [ ! -f "$staged_bundle/Contents/MacOS/service" ] || \
   [ ! -f "$staged_bundle/Contents/Info.plist" ]; then
    echo "[root-update] staged bundle validation failed, aborting" >> {tmp_dir}/rustdesk_root_update.log
    rm -rf "$staged_bundle"
    exit 1
fi
if ! mv {app_bundle} {app_bundle}.bak; then
    echo "[root-update] backup mv failed, aborting" >> {tmp_dir}/rustdesk_root_update.log
    rm -rf "$staged_bundle"
    exit 1
fi
bundle_swapped=1
if ! mv "$staged_bundle" {app_bundle}; then
    echo "[root-update] replacement mv failed, restoring backup" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
# Install the entire bundle as root-owned.  The LaunchDaemon executes code
# from this bundle, so no nested framework, helper, or resource may remain
# user-writable.
if ! chown -R root:wheel {app_bundle} || ! chmod -R go-w {app_bundle}; then
    echo "[root-update] chown failed, restoring backup" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
xattr -r -d com.apple.quarantine {app_bundle} || true
# Keep root-executed files AND entire ancestor chain root-owned — prevent privilege escalation
if ! chown root:wheel {app_bundle} || \
   ! chmod 755 {app_bundle} || \
   ! chown root:wheel {app_bundle}/Contents || \
   ! chmod 755 {app_bundle}/Contents || \
   ! chown root:wheel {app_bundle}/Contents/MacOS || \
   ! chmod 755 {app_bundle}/Contents/MacOS || \
   ! chown root:wheel {app_bundle}/Contents/MacOS/service || \
   ! chmod 755 {app_bundle}/Contents/MacOS/service || \
   ! chown root:wheel {app_bundle}/Contents/MacOS/{app_name} || \
   ! chmod 755 {app_bundle}/Contents/MacOS/{app_name}; then
    echo "[root-update] hardening failed, restoring backup" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
# Generate launchd definitions from the new, final-location binary.  The
# subprocess is bounded and its output is retained for diagnosis; failure
# causes the existing bundle/plists to be restored by the EXIT trap.
if ! write_new_plists; then
    echo "[root-update] CRITICAL: new binary failed to write plists" >> {tmp_dir}/rustdesk_root_update.log
    cat "{tmp_dir}/write-plists.log" >> {tmp_dir}/rustdesk_root_update.log 2>/dev/null || true
    exit 1
fi
echo "[root-update] Plist definitions written by new binary" >> {tmp_dir}/rustdesk_root_update.log
# Check daemon registration and readiness BEFORE removing backup.  launchctl
# load/bootstrap only registers the job; the service can still exit immediately.
if ! launchctl load -w {daemon_plist} 2>/dev/null && \
   ! launchctl bootstrap system {daemon_plist} 2>/dev/null; then
    echo "[root-update] CRITICAL: daemon reload failed, restoring backup" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
if ! daemon_ready; then
    echo "[root-update] CRITICAL: daemon failed readiness check, restoring" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
# Bootstrap agent BEFORE removing backup — needed for rollback on failure.
# This also uses launchctl load for the login-window/no-console-user case.
if ! bootstrap_agents || ! agent_ready; then
    echo "[root-update] CRITICAL: agent bootstrap failed, rolling back" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
# Recheck daemon liveness after the agent is restored and immediately before
# deleting the only rollback bundle.
if ! daemon_snapshot_stable || ! agent_snapshot_stable; then
    echo "[root-update] CRITICAL: daemon or agent stopped before commit, restoring" >> {tmp_dir}/rustdesk_root_update.log
    exit 1
fi
# Only remove backup after BOTH daemon AND agent confirmed running
rollback_done=1
bundle_swapped=0
if ! rm -rf "{app_bundle}.bak"; then
    echo "[root-update] WARNING: committed update but could not remove backup" >> {tmp_dir}/rustdesk_root_update.log
fi
for gui_uid in $gui_uids; do
    launchctl asuser "$gui_uid" open -a "{app_bundle}" || true
done
echo "[root-update] Done!" >> {tmp_dir}/rustdesk_root_update.log
rm -rf {tmp_dir}
"#,
        app_name = app_name,
        app_bundle = app_bundle,
        src_app = src_app,
        uid_list = uid_list,
        daemon_plist = daemon_plist,
        agent_plist = agent_plist,
        tmp_dir = tmp_dir,
        daemon_label = daemon_label,
        agent_label = agent_label,
        daemon_plist_bak = daemon_plist_bak,
        agent_plist_bak = agent_plist_bak,
    );

    {
        use std::io::Write;
        if let Err(err) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&script_path)
            .and_then(|mut f| f.write_all(script.as_bytes()))
        {
            return Err(err.into());
        }
    }
    match Command::new("/bin/chmod")
        .args(&["+x", &script_path])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            bail!(
                "[root-update] failed to make update script executable: {}",
                status
            );
        }
        Err(err) => {
            return Err(err.into());
        }
    }
    // 拒绝在启动之前观察到的会话变化，但这个快照是
    // 尽力而为的：它与分离脚本中的关闭操作并不是原子的。
    if get_logged_in_uids() != logged_in_uids {
        bail!("[root-update] GUI session set changed before update launch");
    }
    if !crate::updater::has_no_active_conns_ipc() {
        bail!("[root-update] active session started before update launch");
    }
    if let Err(err) = Command::new("/bin/bash")
        .arg(&script_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    {
        return Err(err.into());
    }

    log::info!("[root-update] Update script launched.");
    Ok(())
}

fn extract_dmg_into_existing_dir(dmg_path: &str, target_dir: &str) -> ResultType<()> {
    let target_path = Path::new(target_dir);
    if !target_path.exists() {
        bail!(
            "[root-update] Temp directory does not exist: {:?}",
            target_path
        );
    }
    extract_dmg_inner(dmg_path, target_dir)
}

fn verified_dmg_update_source(verified_dmg: &VerifiedDmg) -> ResultType<UpdateSource> {
    verify_stored_dmg(verified_dmg)?;
    Ok(UpdateSource::VerifiedDmg {
        path: verified_dmg_path(verified_dmg)?.to_owned(),
        expected_sha256: verified_dmg.expected_sha256.clone(),
    })
}

pub fn get_double_click_time() -> u32 {
    // 待办：https://github.com/servo/core-foundation-rs/blob/786895643140fa0ee4f913d7b4aeb0c4626b2085/cocoa/src/appkit.rs#L2823
    500 as _
}

pub fn hide_dock() {
    unsafe {
        NSApp().setActivationPolicy_(NSApplicationActivationPolicyAccessory);
    }
}

#[inline]
#[allow(dead_code)]
fn get_server_start_time_of(p: &Process, path: &Path) -> Option<i64> {
    let cmd = p.cmd();
    if cmd.len() <= 1 {
        return None;
    }
    if &cmd[1] != "--server" {
        return None;
    }
    let Ok(cur) = std::fs::canonicalize(p.exe()) else {
        return None;
    };
    if &cur != path {
        return None;
    }
    Some(p.start_time() as _)
}

#[inline]
#[allow(dead_code)]
fn get_server_start_time(sys: &mut System, path: &Path) -> Option<(i64, Pid)> {
    sys.refresh_processes_specifics(ProcessRefreshKind::new());
    for (_, p) in sys.processes() {
        if let Some(t) = get_server_start_time_of(p, path) {
            return Some((t, p.pid() as _));
        }
    }
    None
}

pub fn handle_application_should_open_untitled_file() {
    hbb_common::log::debug!("icon clicked on finder");
    let x = std::env::args().nth(1).unwrap_or_default();
    if x == "--server" || x == "--cm" || x == "--tray" {
        std::thread::spawn(move || crate::handle_url_scheme("".to_lowercase()));
    }
}

/// 获取显示器的所有分辨率。这些分辨率：
/// 1. 按宽度和高度降序排列，并去除重复项。
/// 2. 如果数量太多（例如 >15），则过滤掉宽度小于 800（800x600）的项。
/// 3. 同时包含 HiDPI 分辨率和真实分辨率。
///
/// 我们不需要区分 HiDPI 分辨率和真实分辨率。
/// 当控制端改变分辨率时，会调用 `change_resolution_directly()`。
/// `change_resolution_directly()` 会优先尝试使用 HiDPI 分辨率。
/// teamviewer 目前就是这么做的。
///
/// 如果我们需要区分 HiDPI 分辨率和真实分辨率，可以给 `Resolution` 结构体加一个标志位。
pub fn resolutions(name: &str) -> Vec<Resolution> {
    let mut v = vec![];
    if let Ok(display) = name.parse::<u32>() {
        let mut num = 0;
        unsafe {
            if YES == MacGetModeNum(display, &mut num) {
                let (mut widths, mut heights, mut _hidpis) =
                    (vec![0; num as _], vec![0; num as _], vec![NO; num as _]);
                let mut real_num = 0;
                if YES
                    == MacGetModes(
                        display,
                        widths.as_mut_ptr(),
                        heights.as_mut_ptr(),
                        _hidpis.as_mut_ptr(),
                        num,
                        &mut real_num,
                    )
                {
                    if real_num <= num {
                        v = (0..real_num)
                            .map(|i| Resolution {
                                width: widths[i as usize] as _,
                                height: heights[i as usize] as _,
                                ..Default::default()
                            })
                            .collect::<Vec<_>>();
                        // 按 (w, h) 降序排序
                        v.sort_by(|a, b| {
                            if a.width == b.width {
                                b.height.cmp(&a.height)
                            } else {
                                b.width.cmp(&a.width)
                            }
                        });
                        // 去除重复项
                        v.dedup_by(|a, b| a.width == b.width && a.height == b.height);
                        // 如果数量太多，过滤掉宽度小于 800（800x600）的项。
                        // 我们也可以在客户端做这个过滤，但最好不改客户端，以减少影响。
                        if v.len() > 15 {
                            // 大多数宽度 > 800，所以去掉小的那些没问题。
                            v.retain(|r| r.width >= 800);
                        }
                        if v.len() > 15 {
                            // 如果长度仍然过长就忽略。
                        }
                    }
                }
            }
        }
    }
    v
}

pub fn current_resolution(name: &str) -> ResultType<Resolution> {
    let display = name.parse::<u32>().map_err(|e| anyhow!(e))?;
    unsafe {
        let (mut width, mut height) = (0, 0);
        if NO == MacGetMode(display, &mut width, &mut height) {
            bail!("MacGetMode failed");
        }
        Ok(Resolution {
            width: width as _,
            height: height as _,
            ..Default::default()
        })
    }
}

pub fn change_resolution_directly(name: &str, width: usize, height: usize) -> ResultType<()> {
    let display = name.parse::<u32>().map_err(|e| anyhow!(e))?;
    unsafe {
        if NO == MacSetMode(display, width as _, height as _, true) {
            bail!("MacSetMode failed");
        }
    }
    Ok(())
}

pub fn check_super_user_permission() -> ResultType<bool> {
    unsafe { Ok(MacCheckAdminAuthorization() == YES) }
}

pub fn elevate(args: Vec<&str>, prompt: &str) -> ResultType<bool> {
    let cmd = std::env::current_exe()?;
    match cmd.to_str() {
        Some(cmd) => {
            let mut cmd_with_args = cmd.to_string();
            for arg in args {
                cmd_with_args = format!("{} {}", cmd_with_args, arg);
            }
            let script = format!(
                r#"do shell script "{}" with prompt "{}" with administrator privileges"#,
                cmd_with_args, prompt
            );
            match std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .arg(&get_active_username())
                .status()
            {
                Err(e) => {
                    bail!("Failed to run osascript: {}", e);
                }
                Ok(status) => Ok(status.success() && status.code() == Some(0)),
            }
        }
        None => {
            bail!("Failed to get current exe str");
        }
    }
}

pub struct WakeLock(Option<keepawake::AwakeHandle>);

impl WakeLock {
    pub fn new(display: bool, idle: bool, sleep: bool) -> Self {
        WakeLock(
            keepawake::Builder::new()
                .display(display)
                .idle(idle)
                .sleep(sleep)
                .create()
                .ok(),
        )
    }

    pub fn set_display(&mut self, display: bool) -> ResultType<()> {
        self.0
            .as_mut()
            .map(|h| h.set_display(display))
            .ok_or(anyhow!("no AwakeHandle"))?
    }
}

pub(crate) fn get_bundle_id() -> Option<String> {
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        if bundle.is_null() {
            return None;
        }

        let bundle_id: id = msg_send![bundle, bundleIdentifier];
        if bundle_id.is_null() {
            return None;
        }

        let c_str: *const std::os::raw::c_char = msg_send![bundle_id, UTF8String];
        if c_str.is_null() {
            return None;
        }

        let bundle_id_str = std::ffi::CStr::from_ptr(c_str)
            .to_string_lossy()
            .to_string();
        Some(bundle_id_str)
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn update_tree_rejects_framework_root_symlink() {
        let test_dir = std::env::temp_dir().join(format!(
            "rustdesk-update-tree-framework-link-test-{}-{}",
            std::process::id(),
            hbb_common::rand::random::<u64>()
        ));
        let app_dir = test_dir.join("RustDesk.app");
        let frameworks_dir = app_dir.join("Contents/Frameworks");
        let external_framework = test_dir.join("External.framework");
        std::fs::create_dir_all(&frameworks_dir).unwrap();
        std::fs::create_dir_all(&external_framework).unwrap();
        std::os::unix::fs::symlink(
            &external_framework,
            frameworks_dir.join("External.framework"),
        )
        .unwrap();

        let result = validate_update_tree(&app_dir, None);

        std::fs::remove_dir_all(test_dir).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn update_tree_allows_internal_framework_symlink() {
        let test_dir = std::env::temp_dir().join(format!(
            "rustdesk-update-tree-internal-link-test-{}-{}",
            std::process::id(),
            hbb_common::rand::random::<u64>()
        ));
        let framework_dir = test_dir.join("RustDesk.app/Contents/Frameworks/Test.framework");
        let versions_dir = framework_dir.join("Versions");
        std::fs::create_dir_all(versions_dir.join("A")).unwrap();
        std::os::unix::fs::symlink("A", versions_dir.join("Current")).unwrap();

        let result = validate_update_tree(&test_dir.join("RustDesk.app"), None);

        std::fs::remove_dir_all(test_dir).unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn update_app_identity_requires_matching_bundles() {
        let test_dir = std::env::temp_dir().join(format!(
            "rustdesk-update-identity-test-{}-{}",
            std::process::id(),
            hbb_common::rand::random::<u64>()
        ));
        let installed = test_dir.join("installed.app");
        let candidate = test_dir.join("candidate.app");
        let info = |bundle_id| {
            format!("<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{bundle_id}</string></dict></plist>")
        };
        for app in [&installed, &candidate] {
            std::fs::create_dir_all(app.join("Contents")).unwrap();
        }
        std::fs::write(
            installed.join("Contents/Info.plist"),
            info("com.carriez.rustdesk"),
        )
        .unwrap();

        for (bundle_id, accepted) in [("com.carriez.rustdesk", true), ("example.other", false)] {
            std::fs::write(candidate.join("Contents/Info.plist"), info(bundle_id)).unwrap();
            assert_eq!(
                verify_update_app_identity(&installed, &candidate).is_ok(),
                accepted
            );
        }

        std::fs::remove_dir_all(test_dir).unwrap();
    }
}
