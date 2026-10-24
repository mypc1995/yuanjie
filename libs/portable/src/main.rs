#![windows_subsystem = "windows"]

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use bin_reader::{normalize_path, BinaryReader};

pub mod bin_reader;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
const APP_METADATA: &[u8] = include_bytes!("../app_metadata.toml");
#[cfg(not(windows))]
const APP_METADATA: &[u8] = &[];
const APP_METADATA_CONFIG: &str = "meta.toml";
const META_LINE_PREFIX_TIMESTAMP: &str = "timestamp = ";
const META_LINE_PREFIX_FILE: &str = "file = ";
const APP_PREFIX: &str = "rustdesk";
#[cfg(windows)]
const VERIFIED_UPDATE_FILE_PREFIX: &str = "rustdesk-verified-";
const APPNAME_RUNTIME_ENV_KEY: &str = "RUSTDESK_APPNAME";
#[cfg(windows)]
const SET_FOREGROUND_WINDOW_ENV_KEY: &str = "SET_FOREGROUND_WINDOW";

// 解压目录跟随负载所要求的可执行文件，因此一个
// 定制客户端会得到自己的目录，而不是与 RustDesk 共用。在没有注入包时回退到
// APP_PREFIX，这样保持原版构建不变。
fn app_dir_name(exe: &str) -> String {
    Path::new(&exe.replace('\\', "/"))
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.trim().to_lowercase())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| APP_PREFIX.to_owned())
}

fn is_timestamp_matches(dir: &Path, ts: &mut u64) -> bool {
    let Ok(app_metadata) = std::str::from_utf8(APP_METADATA) else {
        return true;
    };
    for line in app_metadata.lines() {
        if line.starts_with(META_LINE_PREFIX_TIMESTAMP) {
            if let Ok(stored_ts) = line.replace(META_LINE_PREFIX_TIMESTAMP, "").parse::<u64>() {
                * ts = stored_ts;
                break;
            }
        }
    }
    if *ts == 0 {
        return true;
    }

    if let Ok(content) = std::fs::read_to_string(dir.join(APP_METADATA_CONFIG)) {
        for line in content.lines() {
            if line.starts_with(META_LINE_PREFIX_TIMESTAMP) {
                if let Ok(stored_ts) = line.replace(META_LINE_PREFIX_TIMESTAMP, "").parse::<u64>() {
                    return *ts == stored_ts;
                }
            }
        }
    }
    false
}

fn write_meta(dir: &Path, ts: u64, package_paths: &[String]) {
    let meta_file = dir.join(APP_METADATA_CONFIG);
    let mut content = format!("{}{}\n", META_LINE_PREFIX_TIMESTAMP, ts);
    for path in package_paths {
        content.push_str(&format!("{}{}\n", META_LINE_PREFIX_FILE, path));
    }
    // 这里忽略掉也没问题
    let _ = std::fs::write(meta_file, content);
}

fn previous_package_files(dir: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(dir.join(APP_METADATA_CONFIG)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| line.strip_prefix(META_LINE_PREFIX_FILE))
        .map(|path| path.trim().to_owned())
        .collect()
}

// meta.toml 是用户可写目录中的纯文本，而它现在还驱动着删除操作，
// 因此路径是用普通组件重新拼装的，而不是按写下来的样子直接拼接。一个
// 前缀、根或父组件否则就会逃出解压目录：
// Path::join 在遇到绝对路径时会整个替换掉基础路径。
fn resolve_within(dir: &Path, relative: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut path = dir.to_path_buf();
    let mut any = false;
    for component in Path::new(&relative.replace('\\', "/")).components() {
        match component {
            Component::Normal(part) => {
                // 像 "C:x" 这样的驱动器相对名称会被解析为 Normal，而只有
                // Windows 主机才会把 "C:/..." 归类为 Prefix，所以这个冒号
                // 被直接拒绝，而不是依赖主机的解析器。
                if part.to_string_lossy().contains(':') {
                    return None;
                }
                path.push(part);
                any = true;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if any {
        Some(path)
    } else {
        None
    }
}

// 客户若移除某个品牌素材，得到的包里就没有它，而该文件
// 否则会滞留在已有的解压目录中并继续被使用。而这种清理
// 覆盖不到这种情况：它以打包器的构建时间戳为键，而现在这个时间戳
// 对同一版本的所有客户都是一样的。
fn remove_dropped_package_files_with<F>(
    dir: &Path,
    current: &[String],
    mut remove_file: F,
) -> Vec<String>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    let keep: std::collections::HashSet<String> =
        current.iter().map(|p| normalize_path(p)).collect();
    let mut failed = Vec::new();
    for previous in previous_package_files(dir) {
        if keep.contains(&normalize_path(&previous)) {
            continue;
        }
        let Some(path) = resolve_within(dir, &previous) else {
            continue;
        };
        if path.is_file() {
            println!("removing dropped {}", previous);
            if let Err(error) = remove_file(&path) {
                eprintln!("failed to remove dropped {}: {}", previous, error);
                failed.push(previous);
            }
        }
    }
    failed
}

fn remove_dropped_package_files(dir: &Path, current: &[String]) -> Vec<String> {
    remove_dropped_package_files_with(dir, current, |path| std::fs::remove_file(path))
}

fn setup(
    reader: BinaryReader,
    dir: Option<PathBuf>,
    clear: bool,
    _args: &Vec<String>,
    _ui: &mut bool,
) -> Option<PathBuf> {
    let dir = if let Some(dir) = dir {
        dir
    } else {
        // 主目录
        if let Some(dir) = dirs::data_local_dir() {
            dir.join(app_dir_name(&reader.exe))
        } else {
            eprintln!("not found data local dir");
            return None;
        }
    };

    let mut ts = 0;
    if clear || !is_timestamp_matches(&dir, &mut ts) {
        #[cfg(windows)]
        if _args.is_empty() {
            * _ui = true;
            ui::setup();
        }
        std::fs::remove_dir_all(&dir).ok();
    }
    let mut metadata_paths = reader.package_paths.clone();
    metadata_paths.extend(remove_dropped_package_files(&dir, &reader.package_paths));
    for file in reader.files.iter() {
        file.write_to_file(&dir);
    }
    write_meta(&dir, ts, &metadata_paths);
    #[cfg(windows)]
    win::copy_runtime_broker(&dir);
    #[cfg(linux)]
    reader.configure_permission(&dir);
    Some(dir.join(&reader.exe))
}

fn use_null_stdio() -> bool {
    #[cfg(windows)]
    {
        // 在 Windows 7 上从 CMD 运行时，spawn 配合 Stdio::inherit() 会返回「invalid handle」错误。
        // 由于使用 Stdio::null() 没有引起任何问题，而判断程序是从 CMD 启动还是双击启动需要在启动过程中调用更多 API，因此在 Windows 7 上双击启动时我们同样使用 Stdio::null()。
        let is_windows_7 = is_windows_7();
        println!("is windows7: {}", is_windows_7);
        return is_windows_7;
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn is_windows_7() -> bool {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    unsafe {
        let mut version_info = OSVERSIONINFOW::default();
        version_info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;

        if RtlGetVersion(&mut version_info).is_ok() {
            // Windows 7 的版本号是 6.1
            println!(
                "Windows version: {}.{}",
                version_info.dwMajorVersion, version_info.dwMinorVersion
            );
            return version_info.dwMajorVersion == 6 && version_info.dwMinorVersion == 1;
        }
    }
    false
}

fn execute(path: PathBuf, args: Vec<String>, _ui: bool) {
    println!("executing {}", path.display());
    // 设置环境
    let exe = std::env::current_exe().unwrap_or_default();
    let exe_name = exe.file_name().unwrap_or_default();
    // 运行可执行程序
    let mut cmd = Command::new(path);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
        if _ui {
            cmd.env(SET_FOREGROUND_WINDOW_ENV_KEY, "1");
        }
    }

    cmd.env(APPNAME_RUNTIME_ENV_KEY, exe_name);
    if use_null_stdio() {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    } else {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }
    let _child = cmd.spawn();

    #[cfg(windows)]
    if _ui {
        match _child {
            Ok(child) => unsafe {
                winapi::um::winuser::AllowSetForegroundWindow(child.id() as u32);
            },
            Err(e) => {
                eprintln!("{:?}", e);
            }
        }
    }
}

fn main() -> Result<(), String> {
    let mut args = Vec::new();
    let mut arg_exe = Default::default();
    let mut i = 0;
    for arg in std::env::args() {
        if i == 0 {
            arg_exe = arg.clone();
        } else {
            args.push(arg);
        }
        i += 1;
    }
    let click_setup = args.is_empty() && arg_exe.to_lowercase().ends_with("install.exe");
    #[cfg(windows)]
    let quick_support = args.is_empty() && win::is_quick_support_exe(&arg_exe);
    #[cfg(not(windows))]
    let quick_support = false;

    #[cfg(windows)]
    if args.first().is_some_and(|arg| arg == "--update") {
        if let Err(err) = win::schedule_verified_update_cleanup() {
            eprintln!("Failed to schedule verified update file cleanup: {}", err);
        }
    }

    let mut ui = false;
    let reader = BinaryReader::new()?;
    if let Some(exe) = setup(
        reader,
        None,
        click_setup || args.contains(&"--silent-install".to_owned()),
        &args,
        &mut ui,
    ) {
        if click_setup {
            args = vec!["--install".to_owned()];
        } else if quick_support {
            args = vec!["--quick_support".to_owned()];
        }
        execute(exe, args, ui);
    }
    Ok(())
}

#[cfg(windows)]
mod win {
    use std::{fs, os::windows::process::CommandExt, path::Path, process::Command};

    // 用于隐私模式（放大镜实现）。
    pub const RUNTIME_BROKER_EXE: &'static str = "C:\\Windows\\System32\\RuntimeBroker.exe";
    pub const WIN_TOPMOST_INJECTED_PROCESS_EXE: &'static str = "RuntimeBroker_rustdesk.exe";

    pub(super) fn schedule_verified_update_cleanup() -> std::io::Result<()> {
        use windows::{
            core::{HSTRING, PCWSTR},
            Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT},
        };

        let current_exe = std::env::current_exe()?;
        let is_verified_update = current_exe
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(super::VERIFIED_UPDATE_FILE_PREFIX) && name.ends_with(".exe")
            });
        if !is_verified_update {
            return Ok(());
        }

        unsafe {
            MoveFileExW(
                &HSTRING::from(current_exe.as_path()),
                PCWSTR::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            )
        }
        .map_err(std::io::Error::other)
    }

    pub(super) fn copy_runtime_broker(dir: &Path) {
        let src = RUNTIME_BROKER_EXE;
        let tgt = WIN_TOPMOST_INJECTED_PROCESS_EXE;
        let target_file = dir.join(tgt);
        if target_file.exists() {
            if let (Ok(src_file), Ok(tgt_file)) = (fs::read(src), fs::read(&target_file)) {
                let src_md5 = format!("{:x}", md5::compute(&src_file));
                let tgt_md5 = format!("{:x}", md5::compute(&tgt_file));
                if src_md5 == tgt_md5 {
                    return;
                }
            }
        }
        let _allow_err = Command::new("taskkill")
            .args(&["/F", "/IM", "RuntimeBroker_rustdesk.exe"])
            .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW)
            .output();
        let _allow_err = std::fs::copy(src, &format!("{}\\{}", dir.to_string_lossy(), tgt));
    }

    /// 检查该可执行程序是否是 Quick Support 版本。
    /// 注意：此函数必须与 `src/core_main.rs` 保持同步。
    #[inline]
    pub(super) fn is_quick_support_exe(exe: &str) -> bool {
        let exe = exe.to_lowercase();
        exe.contains("-qs-") || exe.contains("-qs.exe") || exe.contains("_qs.exe")
    }
}

#[cfg(test)]
mod meta_tests {
    use super::*;

    #[test]
    fn resolve_within_rejects_paths_that_escape() {
        let base = Path::new("/base");
        assert_eq!(
            resolve_within(base, "./data/logo.png"),
            Some(base.join("data").join("logo.png"))
        );
        assert_eq!(
            resolve_within(base, ".\\data\\logo.png"),
            Some(base.join("data").join("logo.png"))
        );
        // meta.toml 是用户可写的，所以这些绝不能进入 remove_file。
        assert_eq!(resolve_within(base, "../../etc/passwd"), None);
        assert_eq!(resolve_within(base, "/etc/passwd"), None);
        assert_eq!(resolve_within(base, "C:\\Windows\\System32\\x.dll"), None);
        assert_eq!(resolve_within(base, "."), None);
        assert_eq!(resolve_within(base, ""), None);
    }
}
