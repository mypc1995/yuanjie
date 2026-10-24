use hbb_common::ResultType;
// 保留在 hbb_common 中，因为 `config::patch()` 需要查找 shell；此处重新导出，
// 以便长期沿用的 `platform::linux::CMD_SH` 路径保持不变。
pub use hbb_common::sh::{run_cmds_trim_newline, CMD_LOGINCTL, CMD_PS, CMD_SH};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};
use users::{get_current_uid, get_user_by_uid, os::unix::UserExt};

use sctk::{
    output::OutputData,
    output::{OutputHandler, OutputState},
    reexports::client::protocol::wl_output::WlOutput,
    reexports::client::{globals, Proxy},
    reexports::client::{Connection, QueueHandle},
    registry::{ProvidesRegistryState, RegistryState},
};

lazy_static::lazy_static! {
    pub static ref DISTRO: Distro = Distro::new();
}

pub const DISPLAY_SERVER_WAYLAND: &str = "wayland";
pub const DISPLAY_SERVER_X11: &str = "x11";
pub const DISPLAY_DESKTOP_KDE: &str = "KDE";

pub const XDG_CURRENT_DESKTOP: &str = "XDG_CURRENT_DESKTOP";

pub struct Distro {
    pub name: String,
    pub version_id: String,
}

impl Distro {
    fn new() -> Self {
        let name = run_cmds("awk -F'=' '/^NAME=/ {print $2}' /etc/os-release")
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_string();
        let version_id = run_cmds("awk -F'=' '/^VERSION_ID=/ {print $2}' /etc/os-release")
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_string();
        Self { name, version_id }
    }
}

// 已废弃。暂时请改用 `base::platform::linux::is_kde_session()`。
// 或者我们需要在服务端进程中设置正确的环境变量。
#[inline]
pub fn is_kde() -> bool {
    if let Ok(env) = std::env::var(XDG_CURRENT_DESKTOP) {
        env == DISPLAY_DESKTOP_KDE
    } else {
        false
    }
}

// 此处不要使用 `base::platform::linux::is_kde()`。
// 在服务端进程中它并不正确。
pub fn is_kde_session() -> bool {
    std::process::Command::new(CMD_SH.as_str())
        .arg("-c")
        .arg("pgrep -f kded[0-9]+")
        .stdout(std::process::Stdio::piped())
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

#[inline]
pub fn is_gdm_user(username: &str) -> bool {
    username == "gdm" || username == "sddm"
    // || username == "lightgdm"
}

#[inline]
pub fn is_desktop_wayland() -> bool {
    get_display_server() == DISPLAY_SERVER_WAYLAND
}

#[inline]
pub fn is_x11_or_headless() -> bool {
    !is_desktop_wayland()
}

// -1
const INVALID_SESSION: &str = "4294967295";

pub fn get_display_server() -> String {
    // 先检查强制指定的显示服务器环境变量
    if let Ok(forced_display) = std::env::var("RUSTDESK_FORCED_DISPLAY_SERVER") {
        return forced_display;
    }

    // 检查 `loginctl` 是否能被成功调用
    if run_loginctl(None).is_err() {
        return DISPLAY_SERVER_X11.to_owned();
    }

    let mut session = get_values_of_seat0(&[0])[0].clone();
    if session.is_empty() {
        // loginctl 没有给出预期的输出。试试别的办法。
        if let Ok(sid) = std::env::var("XDG_SESSION_ID") {
            // 也可以执行 "cat /proc/self/sessionid"
            session = sid;
        }
        if session.is_empty() {
            session = run_cmds("cat /proc/self/sessionid").unwrap_or_default();
            if session == INVALID_SESSION {
                session = "".to_owned();
            }
        }
    }
    if session.is_empty() {
        std::env::var("XDG_SESSION_TYPE").unwrap_or("x11".to_owned())
    } else {
        get_display_server_of_session(&session)
    }
}

pub fn get_display_server_of_session(session: &str) -> String {
    let mut display_server = if let Ok(output) =
        run_loginctl(Some(vec!["show-session", "-p", "Type", session]))
    // 检查该会话的会话类型
    {
        String::from_utf8_lossy(&output.stdout)
            .replace("Type=", "")
            .trim_end()
            .into()
    } else {
        "".to_owned()
    };
    if display_server.is_empty() || display_server == "tty" || display_server == "unspecified" {
        if let Ok(sestype) = std::env::var("XDG_SESSION_TYPE") {
            if !sestype.is_empty() {
                return sestype.to_lowercase();
            }
        }
        display_server = "x11".to_owned();
    }
    display_server.to_lowercase()
}

#[inline]
fn line_values(indices: &[usize], line: &str) -> Vec<String> {
    indices
        .into_iter()
        .map(|idx| line.split_whitespace().nth(*idx).unwrap_or("").to_owned())
        .collect::<Vec<String>>()
}

#[inline]
pub fn get_values_of_seat0(indices: &[usize]) -> Vec<String> {
    _get_values_of_seat0(indices, true)
}

#[inline]
pub fn get_values_of_seat0_with_gdm_wayland(indices: &[usize]) -> Vec<String> {
    _get_values_of_seat0(indices, false)
}

// 忽略 "3 sessions listed."
fn ignore_loginctl_line(line: &str) -> bool {
    line.contains("sessions") || line.split(" ").count() < 4
}

fn _get_values_of_seat0(indices: &[usize], ignore_gdm_wayland: bool) -> Vec<String> {
    if let Ok(output) = run_loginctl(None) {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if ignore_loginctl_line(line) {
                continue;
            }
            if line.contains("seat0") {
                if let Some(sid) = line.split_whitespace().next() {
                    if is_active(sid) {
                        if ignore_gdm_wayland {
                            if is_gdm_user(line.split_whitespace().nth(2).unwrap_or(""))
                                && get_display_server_of_session(sid) == DISPLAY_SERVER_WAYLAND
                            {
                                continue;
                            }
                        }
                        return line_values(indices, line);
                    }
                }
            }
        }

        // 某些情况下不存在 seat0 https://github.com/rustdesk/rustdesk/issues/73
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if ignore_loginctl_line(line) {
                continue;
            }
            if let Some(sid) = line.split_whitespace().next() {
                if is_active(sid) {
                    let d = get_display_server_of_session(sid);
                    if ignore_gdm_wayland {
                        if is_gdm_user(line.split_whitespace().nth(2).unwrap_or(""))
                            && d == DISPLAY_SERVER_WAYLAND
                        {
                            continue;
                        }
                    }
                    if d == "tty" || d == "unspecified" {
                        continue;
                    }
                    return line_values(indices, line);
                }
            }
        }
    }

    line_values(indices, "")
}

pub fn is_active(sid: &str) -> bool {
    if let Ok(output) = run_loginctl(Some(vec!["show-session", "-p", "State", sid])) {
        String::from_utf8_lossy(&output.stdout).contains("active")
    } else {
        false
    }
}

pub fn is_active_and_seat0(sid: &str) -> bool {
    if let Ok(output) = run_loginctl(Some(vec!["show-session", sid])) {
        String::from_utf8_lossy(&output.stdout).contains("State=active")
            && String::from_utf8_lossy(&output.stdout).contains("Seat=seat0")
    } else {
        false
    }
}

// 同时检查 "Lock" 和 "Switch user"
pub fn is_session_locked(sid: &str) -> bool {
    if let Ok(output) = run_loginctl(Some(vec!["show-session", sid, "--property=LockedHint"])) {
        String::from_utf8_lossy(&output.stdout).contains("LockedHint=yes")
    } else {
        false
    }
}

// **注意** 此处的返回值最后一个字符是 '\n'。
// 如果想去掉末尾的 '\n'，请使用 `run_cmds_trim_newline()`。
pub fn run_cmds(cmds: &str) -> ResultType<String> {
    let output = std::process::Command::new(CMD_SH.as_str())
        .args(vec!["-c", cmds])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_loginctl(args: Option<Vec<&str>>) -> std::io::Result<std::process::Output> {
    if std::env::var("FLATPAK_ID").is_ok() {
        let mut l_args = CMD_LOGINCTL.to_string();
        if let Some(a) = args.as_ref() {
            l_args = format!("{} {}", l_args, a.join(" "));
        }
        let res = std::process::Command::new("flatpak-spawn")
            .args(vec![String::from("--host"), l_args])
            .output();
        if res.is_ok() {
            return res;
        }
    }
    let mut cmd = std::process::Command::new(CMD_LOGINCTL.as_str());
    if let Some(a) = args {
        return cmd.args(a).output();
    }
    cmd.output()
}

/// 永久：可能无效
#[cfg(target_os = "linux")]
pub fn system_message(title: &str, msg: &str, forever: bool) -> ResultType<()> {
    let cmds: HashMap<&str, Vec<&str>> = HashMap::from([
        ("notify-send", [title, msg].to_vec()),
        (
            "zenity",
            [
                "--info",
                "--timeout",
                if forever { "0" } else { "3" },
                "--title",
                title,
                "--text",
                msg,
            ]
            .to_vec(),
        ),
        ("kdialog", ["--title", title, "--msgbox", msg].to_vec()),
        (
            "xmessage",
            [
                "-center",
                "-timeout",
                if forever { "0" } else { "3" },
                title,
                msg,
            ]
            .to_vec(),
        ),
    ]);
    for (k, v) in cmds {
        if Command::new(k).args(v).spawn().is_ok() {
            return Ok(());
        }
    }
    hbb_common::bail!("failed to post system message");
}

#[derive(Debug, Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct WaylandDisplayInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub logical_size: Option<(i32, i32)>,
    pub refresh_rate: i32,
    /// 输出旋转角度（0/90/180/270），来自 `wl_output.geometry`。该 mode 保留其
    /// 未旋转的尺寸，而 `logical_size` 到达时已经是交换过的，因此没有这个字段时
    /// 旋转过的输出与缩放过的输出就无法区分。翻转变体会映射到它们的
    /// 旋转角度。设有默认值，以便来自较旧探测子进程的序列化快照仍能反序列化。
    #[serde(default)]
    pub transform: i32,
}

/// 隔离的 socket 探测回退方案，独立成文件并置于 `wayland_probe` 特性之后，因此
/// 基础的 Wayland 路径绝不会编译到它。DRM 登录屏构建会开启它。
#[cfg(feature = "wayland_probe")]
pub mod wayland_probe;
#[cfg(feature = "wayland_probe")]
pub use wayland_probe::{wayland_display_probe_child_main, WAYLAND_DISPLAY_PROBE_ARG};

// 通过 Wayland 协议获取所有已连接显示器的信息。
pub fn get_wayland_displays() -> ResultType<Vec<WaylandDisplayInfo>> {
    // 必须在连接之前读取：`connect_to_env` 会消耗掉 `WAYLAND_SOCKET`。只有探测回退方案
    // 需要它，因此仅在编译了该特性时才计算。
    #[cfg(feature = "wayland_probe")]
    let named_endpoint = wayland_probe::env_names_wayland_endpoint();
    match Connection::connect_to_env() {
        Ok(conn) => collect_wayland_displays(&conn),
        // 没有该特性时，连接错误就是最终结果，与这个回退方案出现之前完全一致。
        #[cfg(not(feature = "wayland_probe"))]
        Err(err) => Err(err.into()),
        #[cfg(feature = "wayland_probe")]
        Err(err) => wayland_probe::wayland_displays_from_runtime_dir(named_endpoint)
            .map_err(|fallback_err| anyhow::anyhow!("{err}; {fallback_err}")),
    }
}

/// 以角度表示 `wl_output::Transform`。翻转变体只上报其旋转角度：wayland
/// 把它们定义为绕垂直轴镜像后再旋转，而镜像这一半在这里
/// 被丢弃 —— 用该值校正帧的消费方会把翻转输出再镜像一次。
/// 在日志中说明一次而不是默默忽略，因为我们没有任何合成器会产生翻转的
/// 输出来测量镜像那一半；要携带该信息，必须等出现能实测的生产者。
fn transform_degrees(t: sctk::reexports::client::protocol::wl_output::Transform) -> i32 {
    use sctk::reexports::client::protocol::wl_output::Transform;
    match t {
        Transform::Normal => 0,
        Transform::_90 => 90,
        Transform::_180 => 180,
        Transform::_270 => 270,
        Transform::Flipped | Transform::Flipped90 | Transform::Flipped180
        | Transform::Flipped270 => {
            static FLIPPED_WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !FLIPPED_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!(
                    "an output reports a flipped transform ({t:?}); only its rotation is \
                     corrected, the mirror is not"
                );
            }
            match t {
                Transform::Flipped90 => 90,
                Transform::Flipped180 => 180,
                Transform::Flipped270 => 270,
                _ => 0,
            }
        }
        _ => 0,
    }
}

fn collect_wayland_displays(conn: &Connection) -> ResultType<Vec<WaylandDisplayInfo>> {
    struct WaylandEnv {
        registry_state: RegistryState,
        output_state: OutputState,
    }

    impl OutputHandler for WaylandEnv {
        fn output_state(&mut self) -> &mut OutputState {
            &mut self.output_state
        }

        fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
        fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
        fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    }

    impl ProvidesRegistryState for WaylandEnv {
        fn registry(&mut self) -> &mut RegistryState {
            &mut self.registry_state
        }

        sctk::registry_handlers![OutputState];
    }

    sctk::delegate_output!(WaylandEnv);
    sctk::delegate_registry!(WaylandEnv);

    let (globals, mut event_queue) = globals::registry_queue_init(conn)?;
    let queue_handle = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &queue_handle);

    let mut environment = WaylandEnv {
        registry_state,
        output_state,
    };

    event_queue.roundtrip(&mut environment)?;

    let outputs: Vec<_> = environment.output_state.outputs().collect();
    let mut display_infos = Vec::new();

    for output in outputs {
        if let Some(output_data) = output.data::<OutputData>() {
            output_data.with_output_info(|info| {
                if let Some(mode) = info.modes.iter().find(|m| m.current) {
                    // wlroots 合成器会把每个输出的 wl_output.geometry 都留在 (0, 0)，
                    // 只通过 xdg-output 发布真实布局，因此从那里取 `location`
                    // 会让整个桌面堆叠在原点上。Mutter 会同时填写两者，因此这在 GNOME 上仍是
                    // 无操作。
                    let (x, y) = info.logical_position.unwrap_or(info.location);
                    let (width, height) = mode.dimensions;
                    let refresh_rate = mode.refresh_rate;
                    let name = info.name.clone().unwrap_or_default();
                    let logical_size = info.logical_size;
                    let transform = transform_degrees(info.transform);
                    display_infos.push(WaylandDisplayInfo {
                        name,
                        x,
                        y,
                        width,
                        height,
                        logical_size,
                        refresh_rate,
                        transform,
                    });
                }
            });
        }
    }

    Ok(display_infos)
}

/// 把字符串用单引号包裹，进行转义以便安全地用于 shell 命令。
///
/// 此函数通过以下方式处理字符串中含单引号的边界情况：
/// 1. 结束当前的单引号片段
/// 2. 添加一个转义的单引号
/// 3. 开始新的单引号片段
///
/// 示例："it's here" -> "'it'\''s here'"
#[inline]
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace("'", "'\\''"))
}

/// 通过 getpwuid 获取当前用户的主目录（可信来源）。
///
/// 此函数使用系统的口令数据库（通过 `getpwuid`）来获取
/// 主目录，从而避免了依赖 `HOME` 所带来的安全风险 ——
/// 该环境变量可能被不可信输入操纵。
///
/// # 返回值
/// - 若找到主目录且其存在，则为 `Some(PathBuf)`
/// - 若用户查询失败或目录不存在，则为 `None`
///
/// # 安全性
/// 此函数的设计旨在抵御混淆代理（confused-deputy）攻击，即攻击者
/// 可能操纵环境变量来影响特权
/// 操作。
pub fn get_home_dir_trusted() -> Option<PathBuf> {
    let uid = get_current_uid();
    match get_user_by_uid(uid) {
        Some(user) => {
            let home = user.home_dir();
            if Path::is_dir(home) {
                Some(PathBuf::from(home))
            } else {
                log::warn!(
                    "Home directory for uid {} does not exist or is not a directory: {:?}",
                    uid,
                    home
                );
                None
            }
        }
        None => {
            log::warn!("Failed to get user info for uid {}", uid);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 get_home_dir_trusted：返回有效路径并忽略 HOME 环境变量
    #[test]
    fn test_get_home_dir_trusted() {
        let original_home = std::env::var("HOME").ok();

        // 把 HOME 设置为一个伪造/恶意路径
        std::env::set_var("HOME", "/tmp/fake_malicious_home");
        let result = get_home_dir_trusted();

        // 恢复原来的 HOME
        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        // 验证：返回的路径有效，且不是伪造的 HOME
        if let Some(path) = result {
            assert!(path.is_absolute(), "Path should be absolute: {:?}", path);
            assert!(path.is_dir(), "Path should be a directory: {:?}", path);
            assert_ne!(
                path.to_string_lossy(),
                "/tmp/fake_malicious_home",
                "Should not use HOME env var"
            );
        }
    }

    /// 用单引号测试 shell_quote（棘手的情况）
    #[test]
    fn test_shell_quote_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("don't stop"), "'don'\\''t stop'");
    }

    /// 用 shell 元字符测试 shell_quote
    #[test]
    fn test_shell_quote_metacharacters() {
        // 这些都应当被安全地加引号
        assert_eq!(shell_quote("test;rm -rf /"), "'test;rm -rf /'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(shell_quote("`id`"), "'`id`'");
        assert_eq!(shell_quote("a && b"), "'a && b'");
        assert_eq!(shell_quote("a | b"), "'a | b'");
    }
}
