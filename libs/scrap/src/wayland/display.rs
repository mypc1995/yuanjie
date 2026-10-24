use hbb_common::regex::Regex;
use lazy_static::lazy_static;
use std::sync::Mutex;
use std::{
    process::{Command, Output, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::warn;

use base::platform::linux::{get_wayland_displays, WaylandDisplayInfo};

lazy_static! {
    static ref DISPLAYS: Mutex<Option<Arc<Displays>>> = Mutex::new(None);
}

static MISSING_LOGICAL_SIZE_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const COMMAND_TIMEOUT: Duration = Duration::from_millis(1000);

// 仅用于 drm 构建：在那里，未命名端点的失败会 fork 出探测子进程，而轮询器
// 每隔几百毫秒才转一次。其他任何失败都只是一次廉价的进程内错误。
#[cfg(feature = "drm")]
const FAILED_LOOKUP_BACKOFF: Duration = Duration::from_secs(5);

#[cfg(feature = "drm")]
static LAST_FAILED_LOOKUP: Mutex<Option<Instant>> = Mutex::new(None);

#[cfg(feature = "drm")]
static LOOKUP_FAILURE_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub struct Displays {
    pub primary: usize,
    pub displays: Vec<WaylandDisplayInfo>,
}

// 我们需要这个辅助函数带超时地运行命令，因为有些命令可能会挂起。
// 已知 `kscreen-doctor -o` 在以下情况会挂起：
// 1. 在 Archlinux 上同时安装了 GNOME 和 KDE Plasma。
// 2. 在 GNOME 会话中运行该命令。
fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
    label: &str,
) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if start.elapsed() >= timeout {
            warn!("{} command timed out after {:?}", label, timeout);
            if let Err(e) = child.kill() {
                warn!("Failed to kill child process for '{}': {}", label, e);
            }
            if let Err(e) = child.wait() {
                warn!("Failed to wait for child process for '{}': {}", label, e);
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    match child.wait_with_output() {
        Ok(output) => {
            if !output.status.success() {
                warn!("{} command failed with status: {}", label, output.status);
                return None;
            }
            Some(output)
        }
        Err(_) => None,
    }
}

// xrandr 方式有一些限制：
// 1. 只有在 XWayland 运行时才有效。
// 2. 发行版默认可能没有安装 xrandr。
// 3. xrandr 的输出中可能不包含 “primary”。例如 openSUSE Leap 15.6 KDE Plasma。
fn try_xrandr_primary() -> Option<String> {
    let output = Command::new("xrandr").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("primary") && line.contains("connected") {
            if let Some(name) = line.split_whitespace().next() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn try_kscreen_primary() -> Option<String> {
    if !base::platform::linux::is_kde_session() {
        return None;
    }

    let output = run_with_timeout(
        "kscreen-doctor",
        &["-o"],
        COMMAND_TIMEOUT,
        "kscreen-doctor -o",
    )?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);

    // 去掉 ANSI 颜色代码
    let re_ansi = Regex::new(r"\x1b\[[0-9;]*m").ok()?;
    let clean_text = re_ansi.replace_all(&text, "");

    // 把文本拆成若干块，每块以 “Output:” 开头。
    // 拆分结果的第一个元素会是空的，所以跳过它。
    for block in clean_text.split("Output:").skip(1) {
        // 检查这一块描述的是否是主显示器。
        if block.contains("priority 1") {
            // 显示器名称是块中位于 ID 之后的第二段文本。
            // 例如 “ 1 eDP-1 enabled...” -> “eDP-1”
            if let Some(name) = block.split_whitespace().nth(1) {
                return Some(name.to_string());
            }
        }
    }

    None
}

fn try_gdbus_primary() -> Option<String> {
    let output = run_with_timeout(
        "gdbus",
        &[
            "call",
            "--session",
            "--dest",
            "org.gnome.Mutter.DisplayConfig",
            "--object-path",
            "/org/gnome/Mutter/DisplayConfig",
            "--method",
            "org.gnome.Mutter.DisplayConfig.GetCurrentState",
        ],
        COMMAND_TIMEOUT,
        "gdbus DisplayConfig.GetCurrentState",
    )?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);

    // 匹配 primary=true 的逻辑显示器条目
    // 模式：(x, y, scale, transform, true, [('connector-name', ...), ...], ...)
    // 用正则找出第 5 个字段为 true 的条目，然后提取连接器名称
    // 匹配到的文本示例：“(0, 0, 1.5, 0, true, [('HDMI-1', 'MHH', 'Monitor', '0x00000000')], ...)”
    let re = Regex::new(r"\([^()]*,\s*true,\s*\[\('([^']+)'").ok()?;

    if let Some(captures) = re.captures(&text) {
        return captures.get(1).map(|m| m.as_str().to_string());
    }

    None
}

fn get_primary_monitor() -> Option<String> {
    try_xrandr_primary()
        .or_else(try_kscreen_primary)
        .or_else(try_gdbus_primary)
}

#[cfg(feature = "drm")]
fn lookup_allowed(failed_at: Option<Instant>, now: Instant) -> bool {
    failed_at.map_or(true, |at| {
        now.saturating_duration_since(at) >= FAILED_LOOKUP_BACKOFF
    })
}

#[cfg(feature = "drm")]
fn backed_off() -> bool {
    let failed_at = *LAST_FAILED_LOOKUP.lock().unwrap();
    !lookup_allowed(failed_at, Instant::now())
}

// 与探测模块的门控逻辑一致（含锁存）：连接会消耗 WAYLAND_SOCKET，因此
// 一旦被命名为端点，就必须在进程的生命周期内保持命名状态。
#[cfg(feature = "drm")]
fn endpoint_named() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WAS_NAMED: AtomicBool = AtomicBool::new(false);
    let named = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    if named {
        WAS_NAMED.store(true, Ordering::Release);
    }
    WAS_NAMED.load(Ordering::Acquire)
}

// 负责枚举并保持失败时间戳为最新。它本身不抑制任何东西：一次性调用方
// （会话初始化、pipewire）必须始终拿到一次全新的读取，否则一次瞬时失败就会被锁存。
fn enumerate_displays() -> hbb_common::ResultType<Vec<WaylandDisplayInfo>> {
    // 在连接之前读取，因为连接会消耗 WAYLAND_SOCKET。
    #[cfg(feature = "drm")]
    let named = endpoint_named();
    let probed = get_wayland_displays();
    // 只有那种会导致 fork 的失败才会打戳；已命名的端点失败时只是廉价的进程内错误。
    #[cfg(feature = "drm")]
    {
        * LAST_FAILED_LOOKUP.lock().unwrap() = (probed.is_err() && !named).then(Instant::now);
        if let Err(err) = &probed {
            if !LOOKUP_FAILURE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!("Failed to get wayland displays: {}", err);
            }
        } else {
            LOOKUP_FAILURE_WARNED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
    probed
}

// 当现在的查找既无法命中缓存也无法探测时为真。轮询器遇到它会跳过本轮，
// 并保持上一次发布的状态；一次性调用方不能参考它。
#[cfg(feature = "drm")]
pub fn wayland_lookup_suppressed() -> bool {
    DISPLAYS.lock().unwrap().is_none() && backed_off()
}

// 是否存在任何失败时间戳（无论是否已过期）：轮询器用它来区分首次失败
// 和已经在退避期间持续存在的失败。
#[cfg(feature = "drm")]
pub fn wayland_failure_stamped() -> bool {
    LAST_FAILED_LOOKUP.lock().unwrap().is_some()
}

pub fn get_displays() -> Arc<Displays> {
    let mut lock = DISPLAYS.lock().unwrap();
    match lock.as_ref() {
        Some(displays) => displays.clone(),
        None => match enumerate_displays() {
            Ok(displays) => {
                let mut primary_index = None;
                if let Some(name) = get_primary_monitor() {
                    for (i, display) in displays.iter().enumerate() {
                        if display.name == name {
                            primary_index = Some(i);
                            break;
                        }
                    }
                };
                if primary_index.is_none() {
                    for (i, display) in displays.iter().enumerate() {
                        if display.x == 0 && display.y == 0 {
                            primary_index = Some(i);
                            break;
                        }
                    }
                }
                let displays = Arc::new(Displays {
                    primary: primary_index.unwrap_or(0),
                    displays,
                });
                * lock = Some(displays.clone());
                displays
            }
            Err(_err) => {
                #[cfg(not(feature = "drm"))]
                warn!("Failed to get wayland displays: {}", _err);
                Arc::new(Displays {
                    primary: 0,
                    displays: Vec::new(),
                })
            }
        },
    }
}

#[inline]
pub fn clear_wayland_displays_cache() {
    let _ = DISPLAYS.lock().unwrap().take();
    // 失败时间戳是刻意保留下来的：它描述的是会话席位而不是缓存，而
    // 捕获器重建循环大约每秒清空一次。
}

// 只由 display_service 中的布局漂移触发点（唯一的所有者）递增，绝不会因为缓存
// 清空而递增：会话初始化和热插拔 worker 也会清空缓存，而在那里递增会拆掉
// 多显示器会话中所有其他仍在运行的捕获器。捕获器在构建时记录该值，并
// 把之后的递增视为“布局在我脚下变了，需要重建”——这也是 rotation
// 唯一的触发条件，因为它既不改变 CRTC 模式，也不改变帧缓冲大小（rustdesk#15886）。
#[cfg(feature = "drm")]
static SNAPSHOT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 是否尚未缓存任何快照：这是会话构建时枚举失败的标志
/// （`Err` 是刻意不缓存的），与一开始健康的会话相区别。
#[cfg(feature = "drm")]
pub fn wayland_snapshot_missing() -> bool {
    DISPLAYS.lock().unwrap().is_none()
}

#[cfg(feature = "drm")]
pub fn bump_layout_generation() {
    SNAPSHOT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[cfg(feature = "drm")]
pub fn wayland_snapshot_generation() -> u64 {
    SNAPSHOT_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

// 返回 (min_x, max_x, min_y, max_y)
pub fn get_desktop_rect_for_uinput() -> Option<(i32, i32, i32, i32)> {
    let wayland_displays = get_displays();
    desktop_rect_of(&wayland_displays.displays)
}

// 桌面矩形以及各显示器的逻辑矩形，通过一次往返从合成器实时读取
// （drm 构建在失败退避期间可能跳过一轮）。它会跳过显示器缓存
// 和主显示器检测，开销足够低，可以轮询。rustdesk/rustdesk#15601
pub fn get_layout_for_uinput_live() -> Option<((i32, i32, i32, i32), Vec<DisplayRect>)> {
    #[cfg(feature = "drm")]
    if backed_off() {
        return None;
    }
    match enumerate_displays() {
        Ok(displays) => {
            desktop_rect_of(&displays).map(|rect| (rect, logical_rects_of(&displays)))
        }
        Err(_err) => {
            #[cfg(not(feature = "drm"))]
            warn!("Failed to get wayland displays: {}", _err);
            None
        }
    }
}

fn desktop_rect_of(displays: &[WaylandDisplayInfo]) -> Option<(i32, i32, i32, i32)> {
    if displays.is_empty() {
        return None;
    }

    // 为了兼容，`uinput` 使用物理尺寸。
    // 否则，`uinput` 使用逻辑尺寸。
    if displays.len() == 1 {
        let d = &displays[0];
        let (w, h) = oriented_physical(d);
        return Some((d.x, d.x + w, d.y, d.y + h));
    }

    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for d in displays.iter() {
        min_x = min_x.min(d.x);
        min_y = min_y.min(d.y);
        let size = if let Some(logical_size) = d.logical_size {
            logical_size
        } else if d.transform == 90 || d.transform == 270 {
            oriented_physical(d)
        } else {
            // 当 `logical_size` 为 None 时，我们无法获得正确的桌面矩形。
            // 这可能发生在 Wayland 合成器不提供逻辑尺寸信息时，
            // 或者显示信息不完整时。我们会退回使用物理尺寸，它能提供
            // 可用的尺寸，但具体是否正确取决于合成器的行为。
            // 只警告一次；会话活动期间实时路径会轮询这里。
            if !MISSING_LOGICAL_SIZE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!(
                    "Display at ({}, {}) is missing logical_size; falling back to physical size ({}, {}).",
                    d.x, d.y, d.width, d.height
                );
            }
            (d.width, d.height)
        };
        max_x = max_x.max(d.x + size.0);
        max_y = max_y.max(d.y + size.1);
    }
    Some((min_x, max_x, min_y, max_y))
}

/// 某个显示器在客户端所用桌面坐标系中的逻辑矩形：
/// 逻辑原点加上逻辑尺寸；当合成器未报告逻辑尺寸时退回物理尺寸
/// （与 `desktop_rect_of` 保持一致）。
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayRect {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    // 带上它是为了让漂移比较能识别 0<->180 和 90<->270 的翻转，这两种翻转的矩形
    // 本身是相同的；真正的重映射靠名称和包含关系来匹配，绝不靠它。
    pub transform: i32,
}

/// 按投递方向计算的物理尺寸：90/270 的输出扫描输出为 WxH，但被抓取、
/// 对外声明并按 HxW 指向。
fn oriented_physical(d: &WaylandDisplayInfo) -> (i32, i32) {
    if d.transform == 90 || d.transform == 270 {
        (d.height, d.width)
    } else {
        (d.width, d.height)
    }
}

/// 显示器列表中各显示器的逻辑矩形，供已经持有该列表的调用方使用。
pub fn logical_rects_of_displays(displays: &[WaylandDisplayInfo]) -> Vec<DisplayRect> {
    logical_rects_of(displays)
}

fn logical_rects_of(displays: &[WaylandDisplayInfo]) -> Vec<DisplayRect> {
    // 与 `desktop_rect_of` 一致：单个显示器使用其物理尺寸（它的缩放
    // 向客户端报告为 1.0），多个显示器使用逻辑尺寸。这样做可以让
    // 单个显示器在重映射时不做任何事（它的原点不会偏移），并让矩形
    // 与客户端坐标所处的坐标系保持一致。
    let single = displays.len() == 1;
    displays
        .iter()
        .map(|d| {
            let (w, h) = if single {
                oriented_physical(d)
            } else {
                d.logical_size.unwrap_or_else(|| oriented_physical(d))
            };
            DisplayRect {
                name: d.name.clone(),
                x: d.x,
                y: d.y,
                w,
                h,
                transform: d.transform,
            }
        })
        .collect()
}

// 来自缓存的初始化快照的各显示器逻辑矩形。客户端注入的
// 坐标在这一布局下是 `local + origin`，所以它是映射的基准。
pub fn get_display_rects_for_uinput() -> Vec<DisplayRect> {
    logical_rects_of(&get_displays().displays)
}

/// 把注入的坐标从客户端仍然认定的布局
/// （`baseline`，会话初始化时捕获）重映射到当前的合成器布局（`live`）。
///
/// 单显示器客户端发送的是整桌面坐标：`local + baseline_origin[d]`
/// 其中 `d` 是它正在跟随的那个显示器。如果该显示器的原点或逻辑尺寸
/// 此后发生了变化（例如另一台显示器被重新缩放，导致这台发生偏移），那么
/// 这个坐标就会落偏。我们先找出该点所在的基准显示器，再把
/// 该点映射到同一显示器的实时矩形中，匹配依据是连接器名称（或者在
/// 合成器不报告名称时，在显示器数量未变的前提下按索引匹配）。
///
/// 当该点落在所有基准显示器之外，或者
/// 匹配到的显示器已经消失时，原样返回输入值，这样匹配失败只会让光标保持原状，
/// 而不会错得更远。https://github.com/rustdesk/rustdesk/issues/15601
pub fn remap_to_live_layout(
    x: i32,
    y: i32,
    baseline: &[DisplayRect],
    live: &[DisplayRect],
) -> (i32, i32) {
    let Some((bi, b)) = baseline
        .iter()
        .enumerate()
        .find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
    else {
        return (x, y);
    };
    let matched = if b.name.is_empty() {
        // 合成器不提供名称：按索引匹配，但仅在数量未变时有效。一个
        // 仅仅是从实时布局中消失的已命名显示器，必须落到下面的
        // “不变”分支，而不是被按索引匹配到如今占据该索引的设备。
        if baseline.len() == live.len() {
            live.get(bi)
        } else {
            None
        }
    } else {
        live.iter().find(|r| r.name == b.name)
    };
    let Some(l) = matched else {
        return (x, y);
    };
    // 把该点映射到实时矩形中，并保留它在显示器内的相对位置，这样
    // 所跟随显示器自身的缩放变化也能被修正，而不只是偏移。
    // 按 (extent - 1) 缩放，使两个端点都精确落位：客户端会把它的
    // 坐标限制在 `[origin, origin + w - 1]`，而把这一段映射到实时跨度的
    // `[0, w' - 1]`，能保证远端边缘（热区角落）在两个方向上依然可达，并且
    // 在尺寸未变时保持为精确的平移。
    let nx = map_axis(x, b.x, b.w, l.x, l.w);
    let ny = map_axis(y, b.y, b.h, l.y, l.h);
    (nx, ny)
}

fn map_axis(v: i32, base_origin: i32, base_extent: i32, live_origin: i32, live_extent: i32) -> i32 {
    if base_extent <= 1 || live_extent <= 1 {
        return live_origin;
    }
    live_origin + ((v - base_origin) as i64 * (live_extent - 1) as i64 / (base_extent - 1) as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        logical_size: Option<(i32, i32)>,
    ) -> WaylandDisplayInfo {
        WaylandDisplayInfo {
            name: "".to_owned(),
            x,
            y,
            width,
            height,
            logical_size,
            refresh_rate: 60,
            transform: 0,
        }
    }

    #[test]
    fn test_desktop_rect_single_display_uses_physical_size() {
        let displays = [display(0, 0, 2880, 1800, Some((1859, 1162)))];
        assert_eq!(desktop_rect_of(&displays), Some((0, 2880, 0, 1800)));
    }

    #[test]
    fn test_desktop_rect_multi_display_uses_logical_size() {
        // 笔记本屏幕 155%，位于两块纵向排列的 100% 外接屏之下。
        let displays = [
            display(0, 718, 2880, 1800, Some((1859, 1162))),
            display(1859, 0, 1920, 1080, Some((1920, 1080))),
            display(1859, 1080, 1920, 1080, Some((1920, 1080))),
        ];
        assert_eq!(desktop_rect_of(&displays), Some((0, 3779, 0, 2160)));
    }

    #[test]
    fn a_single_rotated_display_swaps_the_uinput_rect() {
        // rustdesk#15889 的评审发现 1：单显示器分支提供的是未旋转的
        // 模式，所以指针无法到达竖屏约 44% 的区域。
        let mut d = display(0, 0, 1920, 1080, None);
        d.transform = 90;
        assert_eq!(desktop_rect_of(&[d.clone()]), Some((0, 1080, 0, 1920)));
        let rects = logical_rects_of(&[d]);
        assert_eq!((rects[0].w, rects[0].h), (1080, 1920));
    }

    fn rect(name: &str, x: i32, y: i32, w: i32, h: i32) -> DisplayRect {
        DisplayRect {
            name: name.to_owned(),
            x,
            y,
            w,
            h,
            transform: 0,
        }
    }

    // 报告的故障：连接到第二台显示器，重新缩放主显示器。
    // 基准：两块宽 2560 的显示器并排放置，均为 100%。
    // 实时：主显示器（DP-1）被缩放到 125% -> 逻辑宽度 2048，于是第二台
    // 显示器（DP-2）从 x=2560 左移到 x=2048。跟随 DP-2 的客户端仍然
    // 发送按 DP-2 旧原点（2560）偏移的坐标。
    #[test]
    fn test_remap_primary_rescale_shifts_second_display() {
        let baseline = [
            rect("DP-1", 0, 0, 2560, 1440),
            rect("DP-2", 2560, 0, 2560, 1440),
        ];
        let live = [
            rect("DP-1", 0, 0, 2048, 1440),
            rect("DP-2", 2048, 0, 2560, 1440),
        ];
        // DP-2 的左上角：客户端发送 (2560, 0)，应当落在实时 DP-2 的原点。
        assert_eq!(remap_to_live_layout(2560, 0, &baseline, &live), (2048, 0));
        // DP-2 的中部保持其相对位置比例。
        assert_eq!(
            remap_to_live_layout(3840, 720, &baseline, &live),
            (3328, 720)
        );
    }

    // 所跟随显示器被放大后，其远端边缘依然可达，因此热区
    // 角落依然可用。基准 DP-1 宽 2048，实时 DP-1 宽 2560；
    // 客户端的最后一列（2047）必须映射到实时的最后一列（2559），而不是 2558。
    #[test]
    fn test_remap_enlarged_display_reaches_far_edge() {
        let baseline = [rect("DP-1", 0, 0, 2048, 1440), rect("DP-2", 2048, 0, 1920, 1080)];
        let live = [rect("DP-1", 0, 0, 2560, 1440), rect("DP-2", 2560, 0, 1920, 1080)];
        assert_eq!(remap_to_live_layout(2047, 0, &baseline, &live), (2559, 0));
        assert_eq!(remap_to_live_layout(0, 0, &baseline, &live), (0, 0));
    }

    // 单个显示器在基准和实时状态下都使用物理尺寸（缩放报告为
    // 1.0），因此它不会漂移，即使发生重新缩放，重映射也不做任何事。
    #[test]
    fn test_logical_rects_single_display_uses_physical() {
        let displays = [display(0, 0, 2560, 1440, Some((2048, 1152)))];
        assert_eq!(
            logical_rects_of(&displays),
            vec![rect("", 0, 0, 2560, 1440)]
        );
    }
}
