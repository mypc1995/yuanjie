// 以非特权方式消费 root `--service` 的 DRM/KMS 采集流：该服务负责
// 特权的导出（打开并抓取 scanout 的 dma-buf fd），EGL detile / RGBA 转换在这里进行。

use crate::ipc::{connect_drm, Data, DrmDisplayInfo};
use hbb_common::{anyhow::anyhow, bail, log, tokio, ResultType};
use base::message_proto::DisplayInfo;
use scrap::drm_render::RenderConverter;
use scrap::drmtap_dl::drmtap_dmabuf_desc;
use scrap::{Frame, Pixfmt, PixelBuffer, TraitCapturer};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT_MS: u64 = 3000;
const DRM_CONNECT_TIMEOUT_MS: u64 = 1000;
/// 该服务在唤醒休眠显示器的过程中可能会暂缓给出列表：约 3.6 秒（DRM_WAKE_*）。
const DISPLAY_LIST_TIMEOUT_MS: u64 = HANDSHAKE_TIMEOUT_MS + 4000;
/// 涵盖了连接超时，加上 `recv_msg_timeout2` 两次应用 DISPLAY_LIST_TIMEOUT_MS
/// （先是一个字节，然后是消息体）。打开渲染节点和发送 DrmStart 仍有可能超过它。
const HANDSHAKE_WAIT_MS: u64 = DRM_CONNECT_TIMEOUT_MS + DISPLAY_LIST_TIMEOUT_MS * 2 + 500;
/// 只有读取头部时会重新检查 `stop`，因此消息体的读取要在这里设上限，而不是依赖
    /// `next_raw_into` 自身的上限。
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 自上次以来经过这么长时间后，上一帧的重复帧仍会被发出，
/// 因为编码失败的帧会再次到达编码器，即使画面没有任何变化。
const REPEAT_EVERY: Duration = Duration::from_secs(1);
/// 这么久没有新帧时，屏幕就被视为静止，生产者被要求的频率最多为
/// STILL_FPS，即它原本的 33 毫秒节拍：更快地抓取静止画面只会给 frame() 送来重复帧，
/// 然后被丢弃。有三个这样的节拍，因此以 STILL_FPS 节奏输出运动画面的生产者永远不会被视为静止。
const STILL_AFTER: Duration = Duration::from_millis(100);
const STILL_FPS: u8 = 30;

struct FrameSlot {
    // 行跨度是 `pixels.len() / height`，可能有填充；格式和该帧的平面旋转
    // 是生产者为这一帧读取的（`DmabufDesc::plane_rotation`），逐帧不同。
    latest: Option<(usize, usize, Pixfmt, Option<u32>, Vec<u8>)>,
    // 两个槽位：可以同时有两个缓冲区处于空闲——接收路径取走一个，并以两次
    // 独立的获取来发布，因此编码器可以在中间归还它的借用。
    free: [Option<Vec<u8>>; 2],
    ended: Option<String>,
}

impl FrameSlot {
    fn publish(
        &mut self,
        w: usize,
        h: usize,
        fmt: Pixfmt,
        plane_rotation: Option<u32>,
        buf: Vec<u8>,
    ) {
        if let Some((.., old)) = self.latest.take() {
            self.recycle(old);
        }
        self.latest = Some((w, h, fmt, plane_rotation, buf));
    }

    fn recycle(&mut self, buf: Vec<u8>) {
        if let Some(slot) = self.free.iter_mut().find(|s| s.is_none()) {
            * slot = Some(buf);
        }
    }

    fn take_free(&mut self) -> Option<Vec<u8>> {
        self.free.iter_mut().find_map(|s| s.take())
    }
}

/// 在 new() 存入真实值之前的 `Shared.cursor_transform`：这么早到的光标会被
/// 暂缓，并在会话变换就位后重放，因为生产者不会在形状变化之前重发
/// 它。
const TRANSFORM_PENDING: i32 = i32::MIN;

struct Shared {
    slot: Mutex<FrameSlot>,
    cv: Condvar,
    // 输出真实的变换，在 new() 于握手之后存入之前一直是 TRANSFORM_PENDING。只有
    // 光标需要跨线程使用它：它与帧被旋转的角度不同，在
    // 硬件旋转 180 的输出上，主平面正立扫描输出，但合成器
    // 仍会预先旋转精灵图（在 i915 + mutter 上实测：帧是正立到达的，而
    // 精灵图是上下颠倒的）。帧自身的角度不共享：`frame()` 逐帧计算它，
    // 依据输出变换和帧上标记的平面旋转。接收线程
    // 会暂缓任何与该写入竞争的到来的光标。
    cursor_transform: std::sync::atomic::AtomicI32,
    // 光标校准用的几何信息，与变换取自同一份快照，并在
    // 它之前存入：看到了变换的接收线程也会看到它。`None` 表示这条流
    // 从不测量。每个形状到达时读取一次，绝不在帧路径上读取。
    cal_context: Mutex<Option<CalContext>>,
    // 编码器取帧的速率，随每次 ack 传递，这样生产者不会抓得更快。
    // 在第一次 frame() 调用之前为 0，这让生产者保持其原本的 33 毫秒节拍。
    demand_fps: std::sync::atomic::AtomicU8,
    pace: Mutex<EncoderPace>,
}

/// 编码器自身的每帧耗时：从一次返回了帧的 frame() 调用到下一次调用，
/// 减去该调用等待它所花的时间，并做平滑。frame() 内部的工作（一次旋转）计入；
/// 等待不计入，因此一个迟到的生产者无法把自己的需求说低。返回了空帧的调用
/// （被丢弃的重复帧、超时）让编码器无事可做，因此它之后的时间不作为样本。
/// 这些样本还保存着 video_service 用来控制节奏的休眠时长，因此更短的 spf 会重新开始。
#[derive(Default)]
struct EncoderPace {
    last_call: Option<Instant>,
    last_wait: Duration,
    busy: Option<Duration>,
    spf: Duration,
}

impl EncoderPace {
    /// 在 `now` 时刻、带 `spf` 的一次 frame() 调用中，向生产者请求的速率，上一帧
    /// 是在 `returned_at` 返回的。
    fn demand(&mut self, now: Instant, returned_at: Instant, spf: Duration) -> u8 {
        if spf < self.spf {
            // 跨越该变化的样本也保存着旧的休眠时长。
            self.busy = None;
            self.last_call = None;
        }
        self.spf = spf;
        if let Some(prev) = self.last_call.filter(|&prev| returned_at > prev) {
            let sample = now
                .saturating_duration_since(prev)
                .saturating_sub(self.last_wait)
                .min(Duration::from_secs(1));
            self.busy = Some(self.busy.map_or(sample, |avg| (avg * 7 + sample) / 8));
        }
        self.last_call = Some(now);
        self.last_wait = Duration::ZERO;
        let demand = demand_fps_of(spf, self.busy);
        if now.saturating_duration_since(returned_at) >= STILL_AFTER {
            demand.min(STILL_FPS)
        } else {
            demand
        }
    }
}

/// 在等待的每一种出口上，记录一次 frame() 调用等待一帧花了多久。
struct WaitStamp<'a>(&'a Mutex<EncoderPace>, Instant);

impl Drop for WaitStamp<'_> {
    fn drop(&mut self) {
        if let Ok(mut pace) = self.0.lock() {
            pace.last_wait = self.1.elapsed();
        }
    }
}

/// 对于要求 `spf` 且每帧忙碌 `busy` 的编码器，应当以什么速率抓取：它
/// 要求的速率，或当要求更高时它实际达到的速率。
fn demand_fps_of(spf: Duration, busy: Option<Duration>) -> u8 {
    let max = f64::from(super::video_qos::MAX_FPS);
    let asked = if spf.is_zero() {
        max
    } else {
        // f32 会让 1/60 读成 59.99999：需要四舍五入，否则 60 会被当成 59 来请求。
        (1.0 / spf.as_secs_f64()).round().clamp(1.0, max)
    };
    let achieved = match busy {
        Some(o) if o > spf => (1.0 / o.as_secs_f64()).ceil().clamp(1.0, max),
        _ => max,
    };
    asked.min(achieved) as u8
}

pub struct IpcDrmCapturer {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    display: i32,
    connector: Option<String>,
    // 编码器尺寸的依据：CapturerInfo{width,height} 只在构建时读取一次。
    // 输出被旋转时，这些就是旋转后的尺寸，与交付的帧一致。
    session_size: Option<(usize, usize)>,
    // 来自 wayland 快照的输出变换（角度）；每一帧都会被它转回，
    // 除非平面旋转了 scanout（`frame_transform`）。每个会话固定；一次旋转
    // 会重建采集器。
    transform: i32,
    // 本次会话构建时所依据的 wayland 快照代际：之后一旦失效就意味着
    // 布局（旋转也包括在内）可能已经改变，而 frame() 会请求重建。
    snapshot_gen: u64,
    cur: Vec<u8>,
    cur_w: usize,
    cur_h: usize,
    cur_fmt: Pixfmt,
    got_frame: bool,
    // 输出被旋转时，每一帧会先被转到这里，以便与 `cur` 比较。
    spare: Vec<u8>,
    // frame() 上次返回一帧的时间；参见 REPEAT_EVERY。
    returned_at: Instant,
}

/// 列表索引并不等同于身份标识：`drm_enumerate_all_displays` 会把各张卡的列表拼接起来。
fn connector_key(d: &DrmDisplayInfo) -> String {
    format!("{}:{}", d.device, d.name)
}

/// 交付帧所具有的尺寸所对应的变换：平面在硬件中旋转 90/270 时
/// 交出的就已经是转置后的 scanout（此时 `frame_transform` 什么都不转），无论
/// 输出变换怎么说；否则输出变换就在软件中被撤销。
fn delivered_transform(d: &DrmDisplayInfo, wl_transform: i32) -> i32 {
    if d.plane_transposed {
        90
    } else {
        wl_transform
    }
}

/// 撤销 `transform` 度的输出旋转之后的帧尺寸。
fn rotated_dims(transform: i32, w: usize, h: usize) -> (usize, usize) {
    if transform == 90 || transform == 270 {
        (h, w)
    } else {
        (w, h)
    }
}

/// 旋转后的光标位图上的热点：与 `unrotate_bgra` 作用于
/// 像素时相同的那个点映射，作用于那个必须始终指明点击点的坐标。
fn unrotate_hotspot(transform: i32, w: i32, h: i32, hotx: i32, hoty: i32) -> (i32, i32) {
    match transform {
        90 => (h - 1 - hoty, hotx),
        180 => (w - 1 - hotx, h - 1 - hoty),
        270 => (hoty, w - 1 - hotx),
        _ => (hotx, hoty),
    }
}

/// 把一个 4 字节像素的帧转正并紧凑地写入 `dst`，撤销 `transform` 度；
/// 允许 `src` 的行有填充（stride = len/h）。方向由测试固定为实测的锚点
/// rustdesk#15886；libyuv 逐像素处理，因此通道顺序无关紧要。
fn unrotate_bgra(src: &[u8], w: usize, h: usize, transform: i32, dst: &mut Vec<u8>) {
    const PX: usize = 4;
    let stride = if h > 0 { src.len() / h } else { 0 };
    let (dw, dh) = rotated_dims(transform, w, h);
    dst.resize(
        dw.checked_mul(dh).and_then(|p| p.checked_mul(PX)).unwrap_or(0),
        0,
    );
    if dst.is_empty() || stride < w * PX {
        log::error!("unrotate: rejected geometry {w}x{h} stride {stride}; frame left blank");
        return;
    }
    let mode = match transform {
        90 => scrap::RotationMode::kRotate90,
        180 => scrap::RotationMode::kRotate180,
        270 => scrap::RotationMode::kRotate270,
        _ => scrap::RotationMode::kRotate0,
    };
    unsafe {
        scrap::ARGBRotate(
            src.as_ptr(),
            stride as i32,
            dst.as_mut_ptr(),
            (dw * PX) as i32,
            w as i32,
            h as i32,
            mode,
        );
    }
}

/// 某条 wire 条目的变换与增强后的原点，取自同一份 wayland 快照，因此两者
/// 反映的是同一个输出分配；两次 `get_displays()` 读取可能跨越一次缓存
/// 失效。`None` 原点表示没有可用来增强的内容（调用方保留 DRM 原点）。
fn transform_and_origin(
    drm: &[DrmDisplayInfo],
    wire_idx: usize,
    wl: &scrap::wayland::display::Displays,
) -> (i32, Option<(i32, i32)>) {
    if wl.displays.is_empty() || (wl.displays.len() == 1 && drm.len() > 1) {
        if wl.displays.is_empty() && !drm.is_empty() {
            // 之后一次成功的枚举会重新填充缓存，并把这个状态对
            // wayland_snapshot_missing 隐藏起来，因此布局轮询需要这份持久记录来知道某个
            // 采集器是在没有快照的情况下构建的，欠一次重建。
            UNROTATED_SNAPSHOT_PENDING.store(true, Ordering::Release);
            log::warn!(
                "drm: no wayland snapshot at capturer build for display {:?}; assuming unrotated",
                drm.get(wire_idx).map(|d| d.name.as_str()).unwrap_or("?")
            );
        }
        return (0, None);
    }
    let assignment = assign_wayland_outputs(drm, &wl.displays);
    // 变换只来自身份匹配（名称，或唯一的分辨率），并且要走
    // 与公布方用来确定交换的同一个渐进 taken 过程：按布局顺序
    // 的回退用来猜原点没问题，但把旋转钉在猜测上会让公布的
    // 尺寸与实际交付的尺寸对不上。
    let transform = identity_matches(drm, &wl.displays)
        .get(wire_idx)
        .copied()
        .flatten()
        .map(|j| wl.displays[j].transform)
        .unwrap_or(0);
    let origin = augment_with_wayland_geometry_from(drm, wl, &assignment)
        .get(wire_idx)
        .map(|di| (di.x, di.y));
    (transform, origin)
}

/// 没有旋转也没有做镜像的平面所对应的 DRM `rotation` 值。
const PLANE_ROTATE_0: u32 = 0x1;

/// 帧需要被转回的角度。
///
/// 合成器要么在硬件中旋转输出（设置主平面的 `rotation`），
/// （在 i915 + mutter 上 180 度时实测：scanout 是正立的，而平面报告 rotate-180），要么
/// 在软件中旋转（virtio-gpu 和 vmwgfx 没有这样的属性，
/// KWin 让 amdgpu 平面保持 rotate-0；实测：180 度出来是上下颠倒的，而 90/270 度
/// 在原生模式下是横躺的）。`wl_output` 分不出这两种情况；`plane_rotation` 可以。
/// 做了旋转的平面无论输出变换怎么说都会扫描输出逻辑桌面：
/// 合成器根据 CRTC 变换对它编程，而该变换还把连接器的面板
/// 方向（`wl_output` 从不携带）也算在内，因此两者不会相减。rotate-0 的平面
/// （没有该属性的平面会作为 rotate-0 到达，参见 `DrmReader::plane_rotation`）扫描
/// 输出合成器绘制的内容，帧则按整个变换旋转。`None`（
/// 0.5.8 之前的 libdrmtap，或者库读不到任何东西）沿用 0.5.8 之前的规则：90/270
/// 会旋转，180 不动。光标不走这条路径：参见 `Shared::cursor_transform`。
fn frame_transform(wl_transform: i32, plane_rotation: Option<u32>) -> i32 {
    match plane_rotation {
        Some(mask) if mask != PLANE_ROTATE_0 => 0,
        Some(_) => wl_transform,
        None => {
            if wl_transform == 90 || wl_transform == 270 {
                wl_transform
            } else {
                0
            }
        }
    }
}

/// 会获取 DRM_STATE：绝不要在持有下面某个按显示器的 map 时调用它。
fn display_info_of(display: i32) -> Option<DrmDisplayInfo> {
    match &*DRM_STATE.lock().unwrap() {
        ProbeState::Available(_, list) => list.get(display.max(0) as usize).cloned(),
        _ => None,
    }
}

/// 交付的一帧会重置连续计数相关的判定（`zero_frame_streak`、`demotes`、`since`），并且
    /// 不做别的。
#[derive(Clone, Copy)]
struct DisplayHealth {
    zero_frame_streak: u32,
    since: Instant,
    demotes: u32,
    last_build: Option<Instant>,
    rapid_builds: u32,
    /// 这个显示器的 dma-buf 转换失败了。最常见的原因是多 GPU：我们的渲染节点
    /// 并不是导出 scanout 的那块 GPU。对应于整个进程运行期间的显示器。
    prefer_cpu: bool,
    /// 这个显示器的 PipeWire 回退因几何原因被拒绝（流是转置的），因此
    /// `mark_demoted_displays` 中的单显示器豁免不能继续把它公布为在线。
    fallback_rejected: bool,
}

impl DisplayHealth {
    fn new() -> Self {
        Self {
            zero_frame_streak: 0,
            since: Instant::now(),
            demotes: 0,
            last_build: None,
            rapid_builds: 0,
            prefer_cpu: false,
            fallback_rejected: false,
        }
    }

    fn demoted(&self) -> bool {
        self.zero_frame_streak >= DRM_GRAB_MAX_FAILURES
            && self.since.elapsed() < demote_cooldown(self.demotes)
    }
}

static DRM_DISPLAY_HEALTH: Mutex<BTreeMap<String, DisplayHealth>> = Mutex::new(BTreeMap::new());
const DRM_GRAB_MAX_FAILURES: u32 = 4;
const DEMOTE_COOLDOWN: Duration = Duration::from_secs(30);
const DEMOTE_BACKOFF_MAX_SHIFT: u32 = 4;
const RAPID_REBUILD_WINDOW: Duration = Duration::from_secs(3);
const RAPID_REBUILD_MAX: u32 = 6;

/// 每次降级都翻倍，最多到 `DEMOTE_BACKOFF_MAX_SHIFT`；交付一帧会把降级
/// 计数清零（参见 `frame()`），而不是按时间衰减。
fn demote_cooldown(demotes: u32) -> Duration {
    DEMOTE_COOLDOWN * (1u32 << demotes.saturating_sub(1).min(DEMOTE_BACKOFF_MAX_SHIFT))
}

#[derive(Debug, PartialEq, Eq)]
enum RefreshOutcome {
    Publish,
    Unavailable,
    Restamp,
    /// 证据针对的是生产者而不是硬件：判定最多给到 `Unknown`。
    GiveUp,
}

/// `failures` 统计连续失败次数且包含本次，因此第一次失败时它是 1。
fn refresh_outcome(probe: Option<usize>, failures: u32) -> RefreshOutcome {
    match probe {
        Some(0) => RefreshOutcome::Unavailable,
        Some(_) => RefreshOutcome::Publish,
        None if failures >= DRM_REFRESH_MAX_FAILURES => RefreshOutcome::GiveUp,
        None => RefreshOutcome::Restamp,
    }
}

fn drm_prefer_cpu(key: &Option<String>) -> bool {
    key.as_ref().is_some_and(|k| {
        DRM_DISPLAY_HEALTH
            .lock()
            .unwrap()
            .get(k)
            .is_some_and(|h| h.prefer_cpu)
    })
}

fn drm_set_prefer_cpu(key: &Option<String>) {
    if let Some(k) = key {
        DRM_DISPLAY_HEALTH
            .lock()
            .unwrap()
            .entry(k.clone())
            .or_insert_with(DisplayHealth::new)
            .prefer_cpu = true;
    }
}

fn render_node_count() -> usize {
    std::fs::read_dir("/dev/dri").map_or(0, |entries| {
        entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("renderD"))
                    .and_then(|minor| minor.parse::<u32>().ok())
                    .is_some()
            })
            .count()
    })
}

static UINPUT_REFRESH_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 某个采集器在没有 wayland 快照的情况下构建，且以未旋转方式运行；布局轮询消费
/// 它，一旦有了当前快照就递增代际。
static UNROTATED_SNAPSHOT_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn take_unrotated_snapshot_pending() -> bool {
    UNROTATED_SNAPSHOT_PENDING.swap(false, std::sync::atomic::Ordering::AcqRel)
}
static UINPUT_REFRESH_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl IpcDrmCapturer {
    /// 该服务是针对它自己的枚举来解析索引的，因此接收线程会重新解析
    /// `expected`（按连接器身份），并返回几何信息必须读取的索引。
    pub fn new(
        display: i32,
        expected: Option<DrmDisplayInfo>,
    ) -> ResultType<(IpcDrmCapturer, Vec<DrmDisplayInfo>, usize, Option<(i32, i32)>)> {
        let shared = Arc::new(Shared {
            slot: Mutex::new(FrameSlot {
                latest: None,
                free: [None, None],
                ended: None,
            }),
            cv: Condvar::new(),
            cursor_transform: std::sync::atomic::AtomicI32::new(TRANSFORM_PENDING),
            cal_context: Mutex::new(None),
            demand_fps: std::sync::atomic::AtomicU8::new(0),
            pace: Mutex::new(EncoderPace::default()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<ResultType<(Vec<DrmDisplayInfo>, usize)>>();
        {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("drm-recv".into())
                .spawn(move || recv_thread(display, expected, shared, stop, tx))
                .map_err(|err| anyhow!("could not spawn the drm receive thread: {err}"))?;
        }
        let (displays, wire_idx) = match rx.recv_timeout(Duration::from_millis(HANDSHAKE_WAIT_MS)) {
            Ok(res) => res?,
            Err(_) => {
                // 稍后才完成的握手会导致流无归属：这里不会运行 Drop。
                stop.store(true, Ordering::SeqCst);
                bail!("drm capture handshake timed out");
            }
        };
        // 整个会话使用一份快照：变换、原点和公布的交换必须都
        // 反映同一个输出分配。代际是在快照之前读取的，因此与构建
        // 竞争的清缓存会导致重建一次，而不是用过期几何运行一个会话。
        let snapshot_gen = scrap::wayland::display::wayland_snapshot_generation();
        let wl = scrap::wayland::display::get_displays();
        let (wl_transform, origin) = transform_and_origin(&displays, wire_idx, &wl);
        // 与变换取自同一份快照，存储在下面对变换的 Release 写入之前。
        let cal = calibration_context(&displays, wire_idx, &wl, snapshot_gen);
        log::info!(
            "drm: cursor calibration for display {display} ({}): {cal:?}",
            displays.get(wire_idx).map(|d| d.name.as_str()).unwrap_or("?")
        );
        * shared.cal_context.lock().unwrap() = cal.ok();
        // 这个采集器现在显示的就是那个布局。如果会话初始化时自己的 wayland 查询失败了，它
        // 保存的是一个空的 baseline，因此这份记录是这条流构建依据的唯一记录。
        super::display_service::note_capturer_layout(&wl.displays, snapshot_gen);
        shared
            .cursor_transform
            .store(wl_transform, std::sync::atomic::Ordering::Release);
        Ok((
            IpcDrmCapturer {
                shared,
                stop,
                display,
                connector: displays.get(wire_idx).map(connector_key),
                session_size: displays.get(wire_idx).map(|d| {
                    rotated_dims(
                        delivered_transform(d, wl_transform),
                        d.width as usize,
                        d.height as usize,
                    )
                }),
                transform: wl_transform,
                snapshot_gen,
                cur: Vec::new(),
                cur_w: 0,
                cur_h: 0,
                cur_fmt: Pixfmt::BGRA,
                got_frame: false,
                spare: Vec::new(),
                returned_at: Instant::now(),
            },
            displays,
            wire_idx,
            origin,
        ))
    }

    /// 没有身份标识时直接跳过，而不是记在 "" 下面，get_capturer_info 会把它读回来
    /// 当成同一个键：一个无法识别的显示器会连带降级下一个。
    fn note_session_without_frame(&self) {
        let Some(key) = self.connector.clone() else {
            log::debug!(
                "drm: display {} produced no frame but has no connector identity; \
                 not counting it against any display",
                self.display
            );
            return;
        };
        let mut map = DRM_DISPLAY_HEALTH.lock().unwrap();
        let h = map.entry(key).or_insert_with(DisplayHealth::new);
        h.zero_frame_streak += 1;
        h.since = Instant::now();
        if h.zero_frame_streak == DRM_GRAB_MAX_FAILURES {
            h.demotes += 1;
            log::warn!(
                "drm: display {} produced no frame in {} sessions; using PipeWire for it, \
                 retrying DRM in {:?} (demotion {})",
                self.display,
                h.zero_frame_streak,
                demote_cooldown(h.demotes),
                h.demotes
            );
        }
    }
}

impl Drop for IpcDrmCapturer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl TraitCapturer for IpcDrmCapturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        // video_service 把它的 spf 作为超时时间传进来：它以及两次调用之间的忙碌程度决定了
        // 向生产者请求的速率。
        let demand =
            self.shared
                .pace
                .lock()
                .unwrap()
                .demand(Instant::now(), self.returned_at, timeout);
        self.shared.demand_fps.store(demand, Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        let repeat = {
            let waited = WaitStamp(&self.shared.pace, Instant::now());
            let mut slot = self.shared.slot.lock().unwrap();
            loop {
                if slot.latest.is_some() || slot.ended.is_some() {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let (guard, _timed_out) =
                    self.shared.cv.wait_timeout(slot, deadline - now).unwrap();
                slot = guard;
            }
            if let Some((w, h, fmt, plane_rotation, buf)) = slot.latest.take() {
                drop(slot);
                drop(waited);
                // 布局变化会递增代际，除此之外在这里看不出来（mode
                // 和 framebuffer 保持尺寸不变）。为新的变换重建；这不计入
                // 健康度：布局移动了，显示器并没有故障。
                if scrap::wayland::display::wayland_snapshot_generation() != self.snapshot_gen {
                    self.shared.slot.lock().unwrap().recycle(buf);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("drm: display {} layout changed; rebuilding", self.display),
                    ));
                }
                // 这一帧仍需旋转的角度：输出变换，除非
                // 平面自己旋转了 scanout（逐帧判断，因为只有生产者能
                // 看到平面）。帧以 scanout 方向到达，而会话是
                // 按旋转后的尺寸设定的，因此这个守卫比较的是旋转后的尺寸：在硬件中旋转了 90 度的
                // 平面交出的是竖向的 scanout 而 `t` 为 0；没有旋转的平面交出的是
                // 横向模式而 `t` 为 90；两者最终都落在会话尺寸上。
                // convert_to_yuv 只会拒绝更大的源（更小的源会留下过期的边缘）；
                // 第一帧：CRTC mode 与 scanout fb 的对比。
                let t = frame_transform(self.transform, plane_rotation);
                let (fw, fh) = rotated_dims(t, w, h);
                if self.session_size.is_some_and(|(sw, sh)| (fw, fh) != (sw, sh)) {
                    self.shared.slot.lock().unwrap().recycle(buf);
                    if !self.got_frame {
                        self.note_session_without_frame();
                    }
                    let (sw, sh) = self.session_size.unwrap_or_default();
                    let what = if self.got_frame {
                        "changed geometry mid-session"
                    } else {
                        "never matched its advertised geometry"
                    };
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "drm: display {} {what} ({sw}x{sh} -> {fw}x{fh}); rebuilding",
                            self.display
                        ),
                    ));
                }
                // 生产者按定时器抓取，因此静止的画面每个
                // 周期都以相同的字节到达。`cur` 仍然保存着上一帧；相同字节换一种格式就是
                // 另一幅图像。
                let repeat = if t == 0 {
                    let repeat = fmt == self.cur_fmt && buf == self.cur;
                    let previous = std::mem::replace(&mut self.cur, buf);
                    self.shared.slot.lock().unwrap().recycle(previous);
                    repeat
                } else if !matches!(fmt, Pixfmt::BGRA | Pixfmt::RGBA) {
                    // 以今天的生产者来说不可达（转换路径输出 4 字节像素
                    // 而 CPU 路径硬编码为 BGRA）；保留它是为了让将来非 4 字节的生产者
                    // 直接让会话失败，而不是把图像拉斜。
                    self.shared.slot.lock().unwrap().recycle(buf);
                    if !self.got_frame {
                        self.note_session_without_frame();
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "drm: display {} delivered {fmt:?} on a rotated output; rebuilding",
                            self.display
                        ),
                    ));
                } else {
                    unrotate_bgra(&buf, w, h, t, &mut self.spare);
                    let repeat = fmt == self.cur_fmt && self.spare == self.cur;
                    std::mem::swap(&mut self.cur, &mut self.spare);
                    self.shared.slot.lock().unwrap().recycle(buf);
                    repeat
                };
                self.cur_w = fw;
                self.cur_h = fh;
                self.cur_fmt = fmt;
                if !self.got_frame {
                    // 只清除连续计数：`rapid_builds` 是给那种先交付了第一帧
                    // 随后又失败的显示器用的，而 `prefer_cpu` 是在 recv 线程上写入的。
                    self.got_frame = true;
                    if let Some(key) = &self.connector {
                        if let Some(h) = DRM_DISPLAY_HEALTH.lock().unwrap().get_mut(key) {
                            h.zero_frame_streak = 0;
                            h.demotes = 0;
                            h.since = Instant::now();
                            h.fallback_rejected = false;
                        }
                    }
                }
                repeat
            } else {
                let err = slot
                    .ended
                    .clone()
                    .unwrap_or_else(|| "drm stream ended".to_owned());
                if !self.got_frame {
                    self.note_session_without_frame();
                }
                return Err(io::Error::new(io::ErrorKind::Other, err));
            }
        };
        if repeat && self.returned_at.elapsed() < REPEAT_EVERY {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.returned_at = Instant::now();
        Ok(Frame::PixelBuffer(PixelBuffer::new(
            &self.cur,
            self.cur_fmt,
            self.cur_w,
            self.cur_h,
        )))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn recv_thread(
    display: i32,
    expected: Option<DrmDisplayInfo>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<ResultType<(Vec<DrmDisplayInfo>, usize)>>,
) {
    let cursor_epoch = next_cursor_epoch();
    let mut conn = match connect_drm(DRM_CONNECT_TIMEOUT_MS).await {
        Ok(c) => c,
        Err(err) => {
            let _ = tx.send(Err(err));
            return;
        }
    };
    if let Err(err) = conn.send_msg(&Data::DrmHello { wake: true }, None).await {
        let _ = tx.send(Err(err));
        return;
    }
    let displays = match conn.recv_msg_timeout2(DISPLAY_LIST_TIMEOUT_MS).await {
        Some(Ok((Data::DrmDisplayList(v), _fd))) => v,
        Some(Ok((other, _fd))) => {
            let _ = tx.send(Err(anyhow!("expected DrmDisplayList, got {:?}", other)));
            return;
        }
        Some(Err(err)) => {
            let _ = tx.send(Err(err));
            return;
        }
        None => {
            let _ = tx.send(Err(anyhow!("timed out waiting for DrmDisplayList")));
            return;
        }
    };
    // 我们的显示器在本次连接列表中的索引；`display` 是客户端列表中的索引。在一台
    // T2 上实测：被唤醒的 2880x1800 面板排到了 Touch Bar 前面，颠倒了索引 0。
    let wire_idx = match &expected {
        Some(e) => {
            match displays
                .iter()
                .position(|d| d.device == e.device && d.name == e.name)
            {
                Some(i) => i,
                None => {
                    let _ = tx.send(Err(anyhow!(
                        "display {display} ({}) is no longer in the service's list; \
                         the video service will rebuild against the fresh topology",
                        e.name
                    )));
                    return;
                }
            }
        }
        None => {
            let _ = tx.send(Err(anyhow!(
                "display {display} is not in the advertised list; not guessing a monitor for it"
            )));
            return;
        }
    };
    // （device, crtc_id）能挺过拓扑变化；列表索引不能。
    let bound_to = displays
        .get(wire_idx)
        .map(|d| (d.device.clone(), d.crtc_id));
    let our_key = displays.get(wire_idx).map(connector_key);
    let render_node = displays
        .get(wire_idx)
        .or_else(|| displays.first())
        .map(|d| d.render_node.clone())
        .unwrap_or_default();
    // 在有多个渲染节点的主机上，未命名的导出方会悄无声息地失败：在 Jetson 上
    // （scanout 是 nvidia-drm，第一个渲染节点是 tegra），导入错误的设备反而成功了，并且破坏了
    // 像素，因此没有可供 prefer_cpu 参考的转换错误。
    let ambiguous_gpu = render_node.is_empty() && render_node_count() > 1;
    let force_cpu = drm_prefer_cpu(&our_key) || ambiguous_gpu;
    let mut converter = if force_cpu {
        None
    } else {
        RenderConverter::open_render(Some(render_node.as_str()))
    };
    let need_cpu = converter.is_none();
    if need_cpu {
        log::info!(
            "drm: requesting the CPU-converted frame path for display {display} ({})",
            if ambiguous_gpu {
                "the service did not name the exporting GPU and this host has several render nodes; \
                 auto-selecting one can import the scanout on the wrong device and silently corrupt it"
            } else if force_cpu {
                "a prior consumer convert failed, e.g. multi-GPU render-node mismatch"
            } else {
                "no render-node convert context: libdrmtap did not load here, or \
                 drmtap_open_render found no usable /dev/dri/renderD*"
            }
        );
    }
    if let Err(err) = conn
        .send_msg(
            &Data::DrmStart {
                display: wire_idx as i32,
                need_cpu,
            },
            None,
        )
        .await
    {
        let _ = tx.send(Err(err));
        return;
    }
    let _ = tx.send(Ok((displays, wire_idx)));

    // 在 new() 存入会话变换之前到达的光标会被保留以备重放。只有
    // 最新的那个有意义；200 毫秒的 recv 超时保证即使链路空闲也会重试。
    let mut pending_cursor: Option<(u64, u32, u32, i32, i32, bool, Vec<u8>)> = None;
    // 正在测量的形状，或者 None：隐藏的、内核测量的，或这条流没有上下文。
    let mut cal: Option<CursorCal> = None;
    let end_reason = loop {
        if stop.load(Ordering::SeqCst) {
            break "stopped".to_owned();
        }
        if pending_cursor.is_some() {
            let t = shared.cursor_transform.load(std::sync::atomic::Ordering::Acquire);
            if t != TRANSFORM_PENDING {
                if let Some((id, width, height, hotx, hoty, hot_measured, raw)) = pending_cursor.take() {
                    let ctx = *shared.cal_context.lock().unwrap();
                    cal = on_cursor_shape(
                        display, cursor_epoch, ctx, id, width, height, hotx, hoty, hot_measured, raw, t,
                    );
                }
            }
        }
        let (msg, recv_fd) = match conn.recv_msg_timeout2(200).await {
            None => continue, // timeout: re-check stop at the loop top
            Some(Ok(pair)) => pair,
            Some(Err(err)) => break format!("recv: {err}"),
        };
        match msg {
            Data::DrmFrameDmabuf(desc) => {
                if cal.is_some() {
                    on_cursor_plane(
                        &mut cal,
                        desc.cursor_pos,
                        super::input_service::last_peer_abs_sample,
                        super::display_service::wayland_layout_drifted(),
                        scrap::wayland::display::wayland_snapshot_generation(),
                        super::display_service::input_map_gen(),
                        super::display_service::input_map_epoch(),
                        display,
                        cursor_epoch,
                    );
                }
                let conv = match converter.as_mut() {
                    Some(c) => c,
                    None => break "no DRM render node; cannot convert dma-buf frame".to_owned(),
                };
                // 在本进程内有效；-1 表示对 `fb_id` 的一次性导入缓存命中。
                let received_fd: RawFd = if desc.has_fd {
                    match recv_fd.as_ref() {
                        Some(f) => f.as_raw_fd(),
                        None => {
                            break "dma-buf frame set has_fd but carried no SCM_RIGHTS fd".to_owned()
                        }
                    }
                } else {
                    -1
                };
                let mut ddesc = drmtap_dmabuf_desc {
                    dma_buf_fd: -1,
                    width: desc.width,
                    height: desc.height,
                    format: desc.format,
                    modifier: desc.modifier,
                    fb_id: desc.fb_id,
                    // RAW：`drm_render::convert` 会拒绝超范围的数量，而不是
                    // 做截断，因此 C 读到的数量就是经过校验的那个。
                    num_planes: desc.num_planes,
                    offsets: desc.offsets,
                    pitches: desc.pitches,
                    hdr_eotf: desc.hdr_eotf,
                    hdr_max_nits: desc.hdr_max_nits,
                };
                match conv.convert(&mut ddesc, received_fd) {
                    Ok((data, w, h, fmt)) => {
                        // 从渲染上下文借用而来，只在下一次转换之前有效。
                        // 复制到一个可复用的缓冲区中，并且在槽位锁之外进行，这样
                        // 几兆字节的 memcpy 永远不会让编码器占着槽位不放。
                        let mut buf = shared.slot.lock().unwrap().take_free().unwrap_or_default();
                        buf.clear();
                        buf.extend_from_slice(data);
                        let mut slot = shared.slot.lock().unwrap();
                        slot.publish(w as usize, h as usize, fmt, desc.plane_rotation, buf);
                        shared.cv.notify_one();
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) => {
                        drm_set_prefer_cpu(&our_key);
                        break format!("convert: {err}");
                    }
                }
                // `recv_fd` 在本次迭代结束时关闭，在 convert 导入它之后。
                // 发出 ack，让生产者释放一个发送额度并转发下一帧；这样把
                // 套接字限制在几帧在途，而不是堆积过期的积压。
                if let Err(err) = conn.send_frame_ack(shared.demand_fps.load(Ordering::Relaxed)).await {
                    break format!("frame ack: {err}");
                }
            }
            Data::DrmFrame {
                width,
                height,
                plane_rotation,
                cursor_pos,
            } => {
                if cal.is_some() {
                    on_cursor_plane(
                        &mut cal,
                        cursor_pos,
                        super::input_service::last_peer_abs_sample,
                        super::display_service::wayland_layout_drifted(),
                        scrap::wayland::display::wayland_snapshot_generation(),
                        super::display_service::input_map_gen(),
                        super::display_service::input_map_epoch(),
                        display,
                        cursor_epoch,
                    );
                }
                // `frame()` 把它交给 PixelBuffer::new，后者按如下方式推导跨度
                // `data.len() / height`：height==0 会导致除以零。
                if width == 0 || height == 0 {
                    break format!("cpu frame: degenerate geometry {width}x{height}");
                }
                let need = (width as usize)
                    .saturating_mul(height as usize)
                    .saturating_mul(4);
                let mut buf = shared.slot.lock().unwrap().take_free().unwrap_or_default();
                match tokio::time::timeout(BODY_READ_TIMEOUT, conn.next_raw_into(&mut buf)).await {
                    Err(_) => break "cpu frame body read timed out".to_owned(),
                    Ok(Ok(())) => {
                        if buf.len() < need {
                            break format!(
                                "cpu frame: body {} bytes < {need} for {width}x{height}",
                                buf.len()
                            );
                        }
                        let mut slot = shared.slot.lock().unwrap();
                        slot.publish(
                            width as usize,
                            height as usize,
                            Pixfmt::BGRA,
                            plane_rotation,
                            buf,
                        );
                        shared.cv.notify_one();
                    }
                    Ok(Err(err)) => break format!("frame body: {err}"),
                }
                // 这一帧 CPU 帧也要发 ack（流控；参见上面的 dma-buf 分支）。
                if let Err(err) = conn.send_frame_ack(shared.demand_fps.load(Ordering::Relaxed)).await {
                    break format!("frame ack: {err}");
                }
            }
            Data::DrmCursor {
                id,
                width,
                height,
                hotx,
                hoty,
                hot_measured,
            } => {
                // get_cursor_data() 把 `colors` 直接交给客户端，客户端会渲染
                // width*height*4 个 RGBA 字节：响应体过短会让它越界读取缓冲区。隐藏光标
                // 哨兵以 1x1、4 字节响应体到达，因此 `need` 是 4，而这项
                // 检查是有效的。
                let need = (width as usize)
                    .saturating_mul(height as usize)
                    .saturating_mul(4);
                let mut raw = Vec::new();
                match tokio::time::timeout(BODY_READ_TIMEOUT, conn.next_raw_into(&mut raw)).await {
                    Err(_) => break "cursor body read timed out".to_owned(),
                    Ok(Ok(())) => {
                        if raw.len() < need {
                            break format!(
                                "cursor body {} bytes < {need} for {width}x{height}",
                                raw.len()
                            );
                        }
                        let t = shared.cursor_transform.load(std::sync::atomic::Ordering::Acquire);
                        if t == TRANSFORM_PENDING {
                            cal = None;
                            pending_cursor = Some((id, width, height, hotx, hoty, hot_measured, raw));
                        } else {
                            pending_cursor = None;
                            let ctx = *shared.cal_context.lock().unwrap();
                            cal = on_cursor_shape(
                                display,
                                cursor_epoch,
                                ctx,
                                id,
                                width,
                                height,
                                hotx,
                                hoty,
                                hot_measured,
                                raw,
                                t,
                            );
                        }
                    }
                    Ok(Err(err)) => break format!("cursor body: {err}"),
                }
            }
            Data::DrmDisplaysChanged(list) => {
                // 是 `display`（客户端的索引）而不是 `wire_idx`，这是有意为之。`bound_to` 是一个
                // 身份标识 `(device, crtc_id)`，不是位置，所以这里问的是「那个槽位是否仍然
                // 是我的显示器」；而下面的交换会把这份列表作为 DRM_STATE 安装，
                // 也就是 display_service 重新公布、输入也据此映射的客户端空间列表。
                // 探测 `wire_idx` 在这个守卫存在的那个场景下恰好保持沉默：一条
                // wire_idx 与 display 不同的流继续运行，而客户端的索引最终会指向
                // 另一台显示器。这个检查在交换之前进行，依据的是这条流
                // 启动时的拓扑。
                let now_at_our_index = list
                    .get(display.max(0) as usize)
                    .map(|d| (d.device.clone(), d.crtc_id));
                if bound_to.is_some() && now_at_our_index != bound_to {
                    swap_available_displays(list);
                    scrap::wayland::display::clear_wayland_displays_cache();
                    break match (&bound_to, &now_at_our_index) {
                        (Some((_, was)), Some((_, now))) => format!(
                            "hotplug renumbered display {display}: it was crtc {was}, now crtc {now}"
                        ),
                        _ => format!("hotplug removed display {display} from the list"),
                    };
                }
                swap_available_displays(list);
                scrap::wayland::display::clear_wayland_displays_cache();
                UINPUT_REFRESH_GEN.fetch_add(1, Ordering::AcqRel);
                if !UINPUT_REFRESH_BUSY.swap(true, Ordering::AcqRel) {
                    // 在创建线程之前取得并移进去：`Builder::spawn` 可能在
                    // 交换之后以 EAGAIN 失败，因此建在闭包内部的守卫根本不会存在，那个标志
                    // 会在整个进程生命周期里一直置位。
                    let mut busy = UinputRefreshGuard(true);
                    let spawned = std::thread::Builder::new()
                        .name("drm-uinput-refresh".into())
                        .spawn(move || {
                        let rt = match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(rt) => rt,
                            Err(err) => {
                                log::warn!(
                                    "drm: uinput refresh worker could not build a runtime: {err}"
                                );
                                return; // the guard hands the slot back
                            }
                        };
                        let mut served = 0u64;
                        loop {
                            let g = UINPUT_REFRESH_GEN.load(Ordering::Acquire);
                            if g != served {
                                served = g;
                                rt.block_on(super::wayland::update_uinput_resolution());
                                continue;
                            }
                            busy.release();
                            if UINPUT_REFRESH_GEN.load(Ordering::Acquire) == served {
                                break;
                            }
                            if !busy.retake() {
                                break; // another handler already started a fresh worker
                            }
                        }
                    });
                    if let Err(err) = spawned {
                        log::error!("drm: could not spawn the uinput refresh worker: {err}");
                    }
                }
            }
            _ => {} // ignore any unexpected control message
        }
    };
    log::info!("drm capture stream ended: {end_reason}");
    // 在本线程上释放渲染上下文：它的 EGL 状态和缓存的导入是线程本地的，
    // 跨线程关闭会把它们搁置在那里。绝不要放在 `Drop` 里，因为那是在编码器线程上运行的。
    drop(converter);
    remove_drm_cursor(display, cursor_epoch);
    let mut slot = shared.slot.lock().unwrap();
    slot.ended = Some(format!("drm stream ended ({end_reason})"));
    shared.cv.notify_one();
}

// 以显示器索引为键：光标存在于指针所在的 CRTC 上，而其他每一条
// 流都报告一个隐藏哨兵，若用一个全局变量就会把它冲掉。
#[derive(Clone)]
pub struct DrmCursorData {
    pub id: u64,
    pub width: i32,
    pub height: i32,
    pub hotx: i32,
    pub hoty: i32,
    pub colors: Vec<u8>,
}


// 内核只在 DRIVER_CURSOR_HOTSPOT 驱动上（虚拟机）暴露光标热点；在物理机上
// 线路上传的是 `infer_hotspot` 的猜测，对于某些形状，仅凭位图无法算对，
// 因为主题把点击点放在了 alpha 没有标出的位置。但是
// `plane_origin = pointer_tip - hotspot`，本进程注入的是尖端本身，而平面
// 位置会随每一帧移动：一旦两者都静止下来，它们的差就是热点。

type Pos = (i32, i32);
type Hotspot = (i32, i32);

/// 平面必须连续这么多帧保持同一个位置，且不计入最初报告该位置的
/// 那一帧，然后才进行测量……
const CURSOR_CAL_STABLE_TICKS: u32 = 3;
/// ……而一个保持静止超过这个时长的位置就不再测量：将要被减去
/// 的那个对端点也就陈旧了那么多。
const CURSOR_CAL_WINDOW_TICKS: u32 = 33;
/// 相距这么近的两次测量就是一致的；与实际绘制内容相差这么小的修正就是抖动。
/// 对端坐标是按逻辑像素量化的，所以在有缩放比例时，测量值会抖动
/// ceil(scale) 个物理像素。
const CURSOR_CAL_TOLERANCE: i32 = 2;
/// 上一次对端绝对移动至少要这么旧（合成器已经消费掉了它）……
const CURSOR_CAL_MIN_INPUT_AGE_MS: u64 = 150;
/// ……且最多这么旧：本地用户可能在此期间移动过指针。
const CURSOR_CAL_MAX_INPUT_AGE_MS: u64 = 10_000;

/// 一条流的校准几何信息，在采集器构建时一次性确定，取自
/// 帧变换所来源的那份 wayland 快照。会话期间不可变；布局变化
/// 会递增快照代际，从而停止测量，并重建采集器。
#[derive(Clone, Copy, Debug, PartialEq)]
struct CalContext {
    /// 该输出在逻辑布局中的矩形，对端的绝对坐标就存在于这个逻辑布局中。
    rect: (i32, i32, i32, i32),
    /// 光标平面位置所采用的 scanout 尺寸。
    physical_size: (i32, i32),
    /// 此对象构建时所依据的快照代际。
    built_gen: u64,
}

/// 在一份快照上一次做完所有资格检查。部分快照不可信（
/// 与 `transform_and_origin` 在信任某个匹配之前遵循同样的规则），输出必须是
/// 身份匹配，绝不能是按布局顺序的猜测；有任何旋转都要拒绝，因为平面处于
/// 未旋转的 scanout 空间，而注入的点是在已定向的布局中（180 也包括在内，因此读取的是
/// UNFOLDED 变换），并且几何信息必须可用。
fn calibration_context(
    drm: &[DrmDisplayInfo],
    wire_idx: usize,
    wl: &scrap::wayland::display::Displays,
    gen: u64,
) -> Result<CalContext, &'static str> {
    if wl.displays.is_empty() || (wl.displays.len() == 1 && drm.len() > 1) {
        return Err("partial wayland snapshot");
    }
    let info = drm.get(wire_idx).ok_or("wire index out of range")?;
    let j = identity_matches(drm, &wl.displays)
        .get(wire_idx)
        .copied()
        .flatten()
        .ok_or("no identity match")?;
    if wl.displays[j].transform != 0 {
        return Err("rotated output");
    }
    let rects = scrap::wayland::display::logical_rects_of_displays(&wl.displays);
    let r = rects.get(j).ok_or("no logical rect")?;
    if r.w <= 0 || r.h <= 0 || info.width == 0 || info.height == 0 {
        return Err("degenerate geometry");
    }
    Ok(CalContext {
        rect: (r.x, r.y, r.w, r.h),
        physical_size: (info.width as i32, info.height as i32),
        built_gen: gen,
    })
}

/// 纯几何判断。注入的点位于逻辑布局中（`logical_rects_of`：单独的输出
/// 按其定向后的物理尺寸，多个输出则按各自的逻辑尺寸），而平面原点
/// 以这个 CRTC 的 scanout 像素表示，因此要求该点落在该输出的矩形内（否则
/// 说明指针在另一台显示器上，该测量由那条流来做），缩放到 scanout 空间，并且
/// 只有之后才减去平面。落在位图之外的答案是竞态、边缘裁剪或者
/// 本地用户移动了指针，而不是热点。
fn measure_hotspot(
    ctx: &CalContext,
    injected: Pos,
    plane: Pos,
    cursor_size: (i32, i32),
) -> Option<Hotspot> {
    let (ax, ay, aw, ah) = ctx.rect;
    let (px, py) = ctx.physical_size;
    let (ix, iy) = injected;
    if ix < ax || ix >= ax + aw || iy < ay || iy >= ay + ah {
        return None;
    }
    let tipx = ((ix - ax) as f64 * px as f64 / aw as f64).round() as i32;
    let tipy = ((iy - ay) as f64 * py as f64 / ah as f64).round() as i32;
    let h = (tipx - plane.0, tipy - plane.1);
    if h.0 < 0 || h.1 < 0 || h.0 >= cursor_size.0 || h.1 >= cursor_size.1 {
        return None;
    }
    Some(h)
}

/// 一条流对当前显示的形状所了解的全部信息。
struct CursorCal {
    /// wire id：缓存的键，以及修正后的 id 重新混合的来源。
    id: u64,
    width: u32,
    height: u32,
    /// 保留下来，这样一次被确认的测量就能在新的热点下重新交付同样的像素。
    raw: Vec<u8>,
    ctx: CalContext,
    /// 生产者报告的内容。
    wire_hot: Hotspot,
    /// 提供给客户端的内容：缓存中已确认的值，否则用线路上的值。
    current_hot: Hotspot,
    /// 一次未经确认的测量，连同它所对照的对端样本以及
    /// 该样本的输入映射 epoch。
    candidate: Option<(Hotspot, u64, u64)>,
    plane: Option<Pos>,
    stable_ticks: u32,
    measured_this_position: bool,
}

/// 只测量生产者猜测出来的热点；内核给出的真值永远不会被覆盖。生产者
/// 若早于 provenance 字段存在，就作为已测量到达（`legacy_hot_measured`），因此它
/// 绝不会被两者中的任何一个测量。
fn calibratable(id: u64, hot_measured: bool) -> bool {
    id != scrap::drm_reader::HIDDEN_CURSOR_ID && !hot_measured
}

/// 一帧报告了平面的位置。
fn update_plane_stability(c: &mut CursorCal, p: Pos) {
    if c.plane != Some(p) {
        c.plane = Some(p);
        c.stable_ticks = 0;
        c.measured_this_position = false;
        return;
    }
    c.stable_ticks = c.stable_ticks.saturating_add(1);
}

/// 每个稳定的平面位置最多产生一次测量，而且只在那个窗口内。
fn settled_unmeasured(c: &CursorCal) -> bool {
    !c.measured_this_position
        && (CURSOR_CAL_STABLE_TICKS..=CURSOR_CAL_WINDOW_TICKS).contains(&c.stable_ticks)
}

/// 测量可以据以相减的对端样本，或者没有：布局自上下文构建以来
/// 不能移动过（重映射标志是次级保护，覆盖的是
/// 提升尚未完成的那段窗口），设备必须运行该布局的 uinput 范围，而且样本
/// 必须是在它采纳该范围之后注入的（过期的范围会把点映射到别处，
/// 而两次停止共用这个误差），样本必须是对着同一个布局映射的，
/// 并且它必须足够老以已经被消费，又足够新以仍然能描述
/// 指针。代际和该标志都由显示服务的布局轮询推进，
/// 因此会话中途移动的显示器最多会晚一个检查间隔才被发现；窗口内的测量
/// 以位图为界，并且只在其代际下缓存。
fn usable_sample(
    ctx: &CalContext,
    s: Option<super::input_service::PeerAbsSample>,
    drifted: bool,
    current_gen: u64,
    map_gen: u64,
    map_epoch: u64,
) -> Option<(Pos, u64, u64)> {
    if drifted || current_gen != ctx.built_gen || map_gen != ctx.built_gen {
        return None;
    }
    let s = s?;
    if s.gen != ctx.built_gen || s.map_epoch != map_epoch {
        return None;
    }
    if !(CURSOR_CAL_MIN_INPUT_AGE_MS..=CURSOR_CAL_MAX_INPUT_AGE_MS).contains(&s.age_ms) {
        return None;
    }
    Some((s.pos, s.seq, s.map_epoch))
}

fn near(a: Hotspot, b: Hotspot) -> bool {
    (a.0 - b.0).abs() <= CURSOR_CAL_TOLERANCE && (a.1 - b.1).abs() <= CURSOR_CAL_TOLERANCE
}

/// 确认策略。只有当第二次测量与候选值一致时才返回 `Some`，
/// 而且此时返回的就是那个候选值，绝不是第二次的原始值。针对同一个
/// 对端样本的两次测量并不独立：平面动了而对端没动，这正是本地
/// 用户把手放在鼠标上的样子，所以它们永远不会互相确认。在不同
/// 输入映射下测得的两次也不会：设备在中间采纳了新的范围，而候选值是
/// 对着旧范围测得的。
fn observe_measurement(c: &mut CursorCal, m: Hotspot, seq: u64, map_epoch: u64) -> Option<Hotspot> {
    match c.candidate {
        Some((_, kseq, _)) if kseq == seq => None,
        Some((k, _, kepoch)) if kepoch == map_epoch && near(k, m) => {
            c.candidate = None;
            Some(k)
        }
        _ => {
            c.candidate = Some((m, seq, map_epoch));
            None
        }
    }
}

/// 两个决策都基于那一个被采纳的值。先比较提供出去的值：与之接近时
/// 什么可见变化都不会发生，提供出去的值仍留在缓存中。这也包括那些
/// 同样接近线路值的值——相差 3 或 4 像素的修正就会造成这种情况——而在那种情况下
/// 先比较线路值会让提供出去的热点和 id 在每次确认时来回抖动。除非
/// 接近线路值，否则说明生产者的猜测是对的：此时提供线路上的值，并丢弃
/// 之前为该形状缓存的修正值。否则就缓存并提供那个被采纳的值。
fn apply_confirmed_hotspot(c: &mut CursorCal, accepted: Hotspot, display: i32, cursor_epoch: u64) {
    if near(accepted, c.current_hot) {
        if c.current_hot == c.wire_hot {
            // 提供线路上的值，而且它是对的：我们这边不会为该形状保留任何缓存。
            forget_cursor_cal(c.id, c.ctx.built_gen);
        } else if cached_cursor_cal(c.id, c.ctx.built_gen) != Some(c.current_hot) {
            store_cursor_cal(c.id, c.current_hot, c.ctx.built_gen);
        }
        return;
    }
    let target = if near(accepted, c.wire_hot) {
        forget_cursor_cal(c.id, c.ctx.built_gen);
        c.wire_hot
    } else {
        store_cursor_cal(c.id, accepted, c.ctx.built_gen);
        accepted
    };
    debug_assert_ne!(target, c.current_hot, "not near the served value, so a change");
    c.current_hot = target;
    let id = if target == c.wire_hot {
        c.id
    } else {
        remix_cursor_id(c.id, target.0, target.1)
    };
    log::info!(
        "drm: cursor hotspot for display {display}: confirmed {target:?} (wire {:?}); serving it",
        c.wire_hot
    );
    deliver_drm_cursor(
        display,
        cursor_epoch,
        id,
        c.width,
        c.height,
        target.0,
        target.1,
        false,
        c.raw.clone(),
        0,
    );
}

/// 按 wire 光标 id 保存的已确认热点，这样合成器复用的形状（箭头、光束、箭头）
/// 从第一帧起就能被正确提供，每个都带着它被确认时的布局代际：
/// 对着轮询尚未看到移动的布局测出的值，只提供给该代际的流，
/// 绝不会提供给提升之后重建的那条流。其边界通过清除以下内容来限定：
/// 写入者代际或更早代际上已确认的值；id 会因主题或尺寸变化而不断变动，
/// 重新测量的代价很低。只在一处读取，且位于上下文闸门之后。
static CURSOR_CAL_CACHE: Mutex<BTreeMap<u64, (Hotspot, u64)>> = Mutex::new(BTreeMap::new());
const CURSOR_CAL_CACHE_CAP: usize = 64;

/// 一个条目只会被它自己那一代或更新一代的条目替换或删除：仍在旧布局上
/// 运行的接收线程无法撤销重建后的流所确认的内容。
fn store_cursor_cal(wire_id: u64, h: Hotspot, gen: u64) {
    let mut cache = CURSOR_CAL_CACHE.lock().unwrap();
    if cache.get(&wire_id).is_some_and(|(_, g)| *g > gen) {
        return;
    }
    if cache.len() >= CURSOR_CAL_CACHE_CAP && !cache.contains_key(&wire_id) {
        cache.retain(|_, (_, g)| *g > gen);
        if cache.len() >= CURSOR_CAL_CACHE_CAP {
            return;
        }
    }
    cache.insert(wire_id, (h, gen));
}
fn forget_cursor_cal(wire_id: u64, gen: u64) {
    let mut cache = CURSOR_CAL_CACHE.lock().unwrap();
    if cache.get(&wire_id).is_some_and(|(_, g)| *g <= gen) {
        cache.remove(&wire_id);
    }
}
/// 该形状在这一代下已确认的修正值（如果有）。另一代的流
/// 得不到修正值，会一直提供线路上的热点，直到有一对测量确认了一个。
fn cached_cursor_cal(wire_id: u64, gen: u64) -> Option<Hotspot> {
    CURSOR_CAL_CACHE
        .lock()
        .unwrap()
        .get(&wire_id)
        .filter(|(_, g)| *g == gen)
        .map(|(h, _)| *h)
}

/// 在读取方的折叠之上，把热点混入 wire id，于是 `run_cursor` 把
/// 修正后的形状当作一次变化，并按它自己的键缓存；对端看到的 id 是在 input_service 中
/// 计算出的内容哈希，它本身就涵盖了热点。发生冲突的可能性和
/// 任意两个 64 位 id 之间冲突的可能性一样低，一旦发生就会在下一次变化之前把一个缓存的位图错发给另一个。
fn remix_cursor_id(wire_id: u64, hotx: i32, hoty: i32) -> u64 {
    let mut id = wire_id ^ 0x9e37_79b9_7f4a_7c15;
    for v in [hotx as u32 as u64, hoty as u32 as u64] {
        id ^= v;
        id = id.wrapping_mul(1099511628211);
    }
    id
}

/// 一个形状到达。按生产者发送的样子发布它，或者按缓存为它保存的已确认修正值发布，
/// 并且在存在上下文且它是猜测时才开始测量它。缓存
/// 只在这里被查询，别处没有，因此没有上下文时，在正向输出上测得的修正值
/// 永远到不了旋转的输出上。
#[allow(clippy::too_many_arguments)]
fn on_cursor_shape(
    display: i32,
    cursor_epoch: u64,
    ctx: Option<CalContext>,
    id: u64,
    width: u32,
    height: u32,
    hotx: i32,
    hoty: i32,
    hot_measured: bool,
    raw: Vec<u8>,
    t: i32,
) -> Option<CursorCal> {
    let cal = ctx.filter(|_| calibratable(id, hot_measured)).map(|ctx| {
        debug_assert_eq!(t, 0, "a calibration context implies an upright output");
        let wire_hot = (hotx, hoty);
        CursorCal {
            id,
            width,
            height,
            raw: raw.clone(),
            ctx,
            wire_hot,
            current_hot: cached_cursor_cal(id, ctx.built_gen).unwrap_or(wire_hot),
            candidate: None,
            plane: None,
            stable_ticks: 0,
            measured_this_position: false,
        }
    });
    let (pid, hx, hy) = match &cal {
        Some(c) if c.current_hot != c.wire_hot => (
            remix_cursor_id(id, c.current_hot.0, c.current_hot.1),
            c.current_hot.0,
            c.current_hot.1,
        ),
        _ => (id, hotx, hoty),
    };
    deliver_drm_cursor(display, cursor_epoch, pid, width, height, hx, hy, hot_measured, raw, t);
    cal
}

/// 一帧报告了平面的位置。只有在平面稳定之后才读取样本，因此
/// 大多数帧只花一次比较，而且不涉及加锁。
#[allow(clippy::too_many_arguments)]
fn on_cursor_plane(
    cal: &mut Option<CursorCal>,
    pos: Option<Pos>,
    sample: impl FnOnce() -> Option<super::input_service::PeerAbsSample>,
    drifted: bool,
    current_gen: u64,
    map_gen: u64,
    map_epoch: u64,
    display: i32,
    cursor_epoch: u64,
) {
    let (Some(c), Some(p)) = (cal.as_mut(), pos) else {
        return;
    };
    update_plane_stability(c, p);
    if !settled_unmeasured(c) {
        return;
    }
    let Some((injected, seq, epoch)) =
        usable_sample(&c.ctx, sample(), drifted, current_gen, map_gen, map_epoch)
    else {
        return;
    };
    c.measured_this_position = true;
    let Some(m) = measure_hotspot(&c.ctx, injected, p, (c.width as i32, c.height as i32)) else {
        return;
    };
    let Some(accepted) = observe_measurement(c, m, seq, epoch) else {
        log::debug!("drm: cursor hotspot for display {display}: measured {m:?} (sample {seq}), waiting for a second one");
        return;
    };
    apply_confirmed_hotspot(c, accepted, display, cursor_epoch);
}

static DRM_CURSOR: Mutex<BTreeMap<i32, (u64, DrmCursorData)>> = Mutex::new(BTreeMap::new());
// 按流单调递增的标签：重建后的流会复用显示器索引，因此被拆除的流只会
// 在 epoch 仍然匹配时才删除自己的条目。
static DRM_CURSOR_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_cursor_epoch() -> u64 {
    DRM_CURSOR_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// 比较并设置：仍在排空的上一条流（较旧的 epoch）不能覆盖替代流
// （较新的 epoch）已经发布的条目。只接受 epoch 至少
// 等于已存储那个的写入。
fn set_drm_cursor(display: i32, epoch: u64, c: DrmCursorData) {
    let mut map = DRM_CURSOR.lock().unwrap();
    match map.get(&display) {
        Some((stored, _)) if *stored > epoch => {}
        _ => {
            map.insert(display, (epoch, c));
        }
    }
}

fn remove_drm_cursor(display: i32, epoch: u64) {
    let mut map = DRM_CURSOR.lock().unwrap();
    if map.get(&display).map(|(e, _)| *e) == Some(epoch) {
        map.remove(&display);
    }
}

/// 把 wire 上的光标反旋转到会话方向并发布。合成器
/// 会预先旋转它编程到光标平面中的位图，因此在未旋转的视频上，
/// 光标本身会保持旋转状态、热点被转置（评审意见 11，见
/// rustdesk#15889）。wire id 只对平面像素和几何做哈希，因此在新的变换下
/// 重建的流会重发同一个 id，而客户端按 id 缓存光标会继续保留
/// 旧的方向：把变换折叠进去（用生产者自己的 FNV 步骤），这样 id 和方向
/// 永远不会互相矛盾。隐藏哨兵必须原样保留。
#[allow(clippy::too_many_arguments)]
fn deliver_drm_cursor(
    display: i32,
    cursor_epoch: u64,
    id: u64,
    width: u32,
    height: u32,
    hotx: i32,
    hoty: i32,
    hot_measured: bool,
    raw: Vec<u8>,
    t: i32,
) {
    // 每一个非零角度，180 也包括在内。在 i915 + mutter 上输出为 180 度时实测：
    // 帧是正立到达的而精灵图是上下颠倒的，说明合成器按完整的变换预先旋转了
    // 精灵图，而平面扫描输出时已经转过。wl_output 无法
    // 判断这两种情况中发生的是哪一种，所以在每个角度上都把精灵图当作已预先旋转。
    let (width, height, hotx, hoty, colors) = if t != 0 {
        let mut turned = Vec::new();
        unrotate_bgra(&raw, width as usize, height as usize, t, &mut turned);
        let (dw, dh) = rotated_dims(t, width as usize, height as usize);
        // 驱动测得的热点是 scanout 精灵图上的一个点，像像素一样映射。
        // 猜测得到的热点是在旋转后的精灵图上猜的——箭头的框的左上角——而
        // 转过之后的箭头尖端是另一个角，所以要在正立的
        // 精灵图上再猜一次。旧流程在每个角度上都是偏的；区别只在于误差的大小，这正是
        // 为什么当时只报告了其中一种情况。
        let (hx, hy) = if hot_measured {
            unrotate_hotspot(t, width as i32, height as i32, hotx, hoty)
        } else {
            scrap::drm_reader::infer_hotspot(&turned, dw, dh)
        };
        (dw as i32, dh as i32, hx, hy, turned)
    } else {
        (width as i32, height as i32, hotx, hoty, raw)
    };
    note_received_provenance(display, id, hot_measured);
    let id = fold_cursor_id(id, t);
    set_drm_cursor(
        display,
        cursor_epoch,
        DrmCursorData {
            id,
            width,
            height,
            hotx,
            hoty,
            colors,
        },
    );
}

/// 按显示器分别保存的最近一次 provenance。不是全局的一个：多条流同时运行，它们的
/// provenance 可能不同但都是稳定的——一个输出发布 HOTSPOT_X/Y，而另一个
/// 不发布，在多 GPU 主机上很正常——而单个槽位会把这读成无尽的
/// 来回交替，从而每条光标消息都打一条日志。
static LAST_RECEIVED_PROVENANCE: Mutex<BTreeMap<i32, bool>> = Mutex::new(BTreeMap::new());

/// 该显示器的 provenance 是否变化，并记录新值。它是独立函数，因此
/// 按显示器的行为可以在不读日志的情况下断言。
fn provenance_is_news_for(display: i32, hot_measured: bool) -> bool {
    LAST_RECEIVED_PROVENANCE
        .lock()
        .unwrap()
        .insert(display, hot_measured)
        != Some(hot_measured)
}

/// 每个显示器说一次，之后只在发生变化时再说，内容是生产者告诉我们的热点信息。
///
/// 它与生产者自己的 provenance 日志相对应，存在的目的正是为了覆盖那行日志
/// 覆盖不到的情况：一条完全没有 `hot_measured` 字段的消息，此时发生的事
/// 由该字段的 serde 默认值决定，在运行中的机器上完全看不出来。
///
/// 隐藏光标哨兵会被跳过。它带有 `hot_measured: false` 发送，因为没有
/// 可描述的热点，记录它会让每次隐藏/显示看起来都像一次 provenance 变化。
fn note_received_provenance(display: i32, id: u64, hot_measured: bool) {
    if id == scrap::drm_reader::HIDDEN_CURSOR_ID {
        return;
    }
    if !provenance_is_news_for(display, hot_measured) {
        return;
    }
    if hot_measured {
        log::info!(
            "drm: cursor hotspots arrive MEASURED; mapping the point the producer sent. \
             (a producer that predates the field reads as measured too, on purpose: that \
              preserves what the old protocol meant)"
        );
    } else {
        log::info!(
            "drm: cursor hotspots arrive as a GUESS; re-inferring from the upright bitmap"
        );
    }
}

fn fold_cursor_id(id: u64, t: i32) -> u64 {
    if id == scrap::drm_reader::HIDDEN_CURSOR_ID {
        id
    } else {
        (id ^ t as u32 as u64).wrapping_mul(1099511628211)
    }
}

fn with_drm_cursor<T>(f: impl Fn(&DrmCursorData) -> T) -> Option<T> {
    let map = DRM_CURSOR.lock().unwrap();
    map.values()
        .map(|(_, c)| c)
        .find(|c| c.id != scrap::drm_reader::HIDDEN_CURSOR_ID)
        .or_else(|| map.values().map(|(_, c)| c).next())
        .map(f)
}

pub fn drm_cursor_id() -> Option<u64> {
    with_drm_cursor(|c| c.id)
}

/// DRM 硬件光标的快照，或者 None。像素是预乘的 ARGB，并原样传递
/// 过去，就像 XFixes 路径那样，这样客户端从任一后端看到的都是同一种光标格式。
pub fn drm_cursor() -> Option<DrmCursorData> {
    with_drm_cursor(|c| c.clone())
}

enum ProbeState {
    Unknown,
    Unavailable(Instant),
    Available(Instant, Vec<DrmDisplayInfo>),
}

static DRM_STATE: Mutex<ProbeState> = Mutex::new(ProbeState::Unknown);
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const POSITIVE_TTL: Duration = Duration::from_secs(15);

/// 在一个一次性线程上运行：在运行时内部调用嵌套的 `#[tokio::main]` 会 panic。
fn query_displays(wake: bool) -> ResultType<Vec<DrmDisplayInfo>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("drm-query".into())
        .spawn(move || {
            let _ = tx.send(query_displays_async(wake));
        })
        .map_err(|err| anyhow!("could not spawn the drm display query thread: {err}"))?;
    rx.recv_timeout(Duration::from_millis(HANDSHAKE_WAIT_MS))
        .map_err(|_| anyhow!("drm display query timed out"))?
}

#[tokio::main(flavor = "current_thread")]
async fn query_displays_async(wake: bool) -> ResultType<Vec<DrmDisplayInfo>> {
    query_displays_inner(wake).await
}

async fn query_displays_inner(wake: bool) -> ResultType<Vec<DrmDisplayInfo>> {
    let mut conn = connect_drm(DRM_CONNECT_TIMEOUT_MS).await?;
    conn.send_msg(&Data::DrmHello { wake }, None).await?;
    match conn.recv_msg_timeout2(DISPLAY_LIST_TIMEOUT_MS).await {
        Some(Ok((Data::DrmDisplayList(v), _fd))) => Ok(v),
        Some(Ok((other, _fd))) => Err(anyhow!("expected DrmDisplayList, got {:?}", other)),
        Some(Err(err)) => Err(err),
        None => Err(anyhow!("timed out waiting for DrmDisplayList")),
    }
}

static DRM_PROBE_FAILURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const DRM_PROBE_MAX_FAILURES: u32 = 5;
static DRM_REFRESH_FAILURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const DRM_REFRESH_MAX_FAILURES: u32 = 3;
// 单飞（single-flight），这样 is_available() 就绝不会在持有 DRM_STATE 时调用 query_displays()（约 4 秒的 IPC）。
static DRM_PROBE_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 每次发布都会推进它，因此一个缓慢的未加锁探测能察觉到期间落下了更新的判定。
static DRM_STATE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 对 DRM_STATE 的每一次判定变更都走这里，这样代际才保持真实；而 TTL
    /// 在 `refresh_available_async` 中的重新打戳是唯一的直接写入。
#[inline]
fn publish_probe_state(st: &mut ProbeState, next: ProbeState) {
    * st = next;
    DRM_STATE_GEN.fetch_add(1, Ordering::Release);
}

/// 在每一种出口上释放 DRM_PROBE_IN_FLIGHT；一次泄漏的释放会让之后所有探测都卡死。
struct ProbeInFlightGuard;
impl Drop for ProbeInFlightGuard {
    fn drop(&mut self) {
        DRM_PROBE_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// `UINPUT_REFRESH_BUSY` 的所有权，在每一种出口上释放。它会被交还并重新获取，
/// 而且是在循环中途，因此无条件在 drop 时释放会清掉一个由替代 worker 持有的标志。
struct UinputRefreshGuard(bool);
impl UinputRefreshGuard {
    fn release(&mut self) {
        if self.0 {
            self.0 = false;
            UINPUT_REFRESH_BUSY.store(false, Ordering::Release);
        }
    }
    fn retake(&mut self) -> bool {
        self.0 = !UINPUT_REFRESH_BUSY.swap(true, Ordering::AcqRel);
        self.0
    }
}
impl Drop for UinputRefreshGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// 绝不探测也不阻塞。用于 `wayland::clear()`、`is_inited()` 以及显示器
/// 枚举这类热路径，因为几秒的 IPC 会触发 "deadline has elapsed"。
pub(crate) fn is_available_cached() -> bool {
    matches!(&*DRM_STATE.lock().unwrap(), ProbeState::Available(..))
}

/// 对 DRM 采集可用性的三态评估。
/// `Unsettled` 表示有探测正在进行，或者失败次数尚未达到禁用阈值。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    Available,
    Unavailable,
    Unsettled,
}

/// 可能会阻塞数秒：绝不要在异步执行器上使用（没有判定的登录会把它放在
/// 阻塞线程池上运行）。这种阻塞形式服务于采集侧的调用方，通过
/// `is_available` 调用，在那里等待一个稳定结论是可以接受的。
fn availability() -> Availability {
    let (verdict, stale_no) = {
        let st = DRM_STATE.lock().unwrap();
        // 在线程外的探测重新验证期间，保持那个已经确定的「否」，避免出现一个瞬时的
        // 在负缓存过期时变成 Unsettled 的结果。
        let stale_no =
            matches!(&*st, ProbeState::Unavailable(since) if since.elapsed() >= NEGATIVE_TTL);
        let verdict = match &*st {
            ProbeState::Available(since, _) => {
                Some((Availability::Available, since.elapsed() >= POSITIVE_TTL))
            }
            ProbeState::Unavailable(_) => Some((Availability::Unavailable, false)),
            ProbeState::Unknown => None, // fall through and probe with the lock released
        };
        (verdict, stale_no)
    };
    if let Some((answer, stale)) = verdict {
        if stale {
            refresh_available_async();
        }
        if stale_no {
            refresh_unavailable_async();
        }
        return answer;
    }
    if DRM_PROBE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        // 别人正在探测：他们的结果还没出来，而「还没」不等于「否」。
        return match &*DRM_STATE.lock().unwrap() {
            ProbeState::Available(..) => Availability::Available,
            ProbeState::Unavailable(_) => Availability::Unavailable,
            ProbeState::Unknown => Availability::Unsettled,
        };
    }
    let _in_flight = ProbeInFlightGuard;
    probe_and_publish()
}

/// 非阻塞的登录路径评估。
/// Unknown 会在线程外启动一次探测；调用方在接纳会话之前要求 `Available`。
pub(crate) fn availability_cached() -> Availability {
    let (verdict, stale_no) = {
        let st = DRM_STATE.lock().unwrap();
        let stale_no =
            matches!(&*st, ProbeState::Unavailable(since) if since.elapsed() >= NEGATIVE_TTL);
        let verdict = match &*st {
            ProbeState::Available(since, _) => {
                Some((Availability::Available, since.elapsed() >= POSITIVE_TTL))
            }
            ProbeState::Unavailable(_) => Some((Availability::Unavailable, false)),
            ProbeState::Unknown => None,
        };
        (verdict, stale_no)
    };
    if let Some((answer, stale)) = verdict {
        if stale {
            refresh_available_async();
        }
        if stale_no {
            refresh_unavailable_async();
        }
        return answer;
    }
    if !DRM_PROBE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        let in_flight = ProbeInFlightGuard;
        let spawned = std::thread::Builder::new()
            .name("drm-avail-probe".into())
            .spawn(move || {
                let _in_flight = in_flight;
                probe_and_publish();
            });
        // 出错时守卫已被移入被丢弃的闭包，因此标志已经释放了。
        if let Err(err) = spawned {
            log::warn!("drm: could not spawn the availability probe thread: {err}");
        }
    }
    Availability::Unsettled
}

/// 同步探测并发布结果。调用方必须持有 DRM_PROBE_IN_FLIGHT。
fn probe_and_publish() -> Availability {
    let t = Instant::now();
    let result = query_displays(true);
    let mut st = DRM_STATE.lock().unwrap();
    let answer = match result {
        Ok(list) if !list.is_empty() => {
            log::debug!(
                "drm: availability probe -> available ({} displays) in {:?}",
                list.len(),
                t.elapsed()
            );
            DRM_PROBE_FAILURES.store(0, Ordering::Relaxed);
            publish_probe_state(&mut st, ProbeState::Available(Instant::now(), list));
            Availability::Available
        }
        Ok(_) => {
            log::info!("drm: availability probe -> no displays in {:?}", t.elapsed());
            publish_probe_state(&mut st, ProbeState::Unavailable(Instant::now()));
            Availability::Unavailable
        }
        Err(err) => {
            let n = DRM_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= DRM_PROBE_MAX_FAILURES {
                log::info!("drm: availability probe failed {n}x ({err}); disabling DRM");
                publish_probe_state(&mut st, ProbeState::Unavailable(Instant::now()));
                Availability::Unavailable
            } else {
                log::info!(
                    "drm: availability probe failed ({err}), attempt {n}/{DRM_PROBE_MAX_FAILURES}; will retry"
                );
                // 在 DRM_STATE 中刻意仍是 Unknown：这是一个重试窗口，不是判定。
                Availability::Unsettled
            }
        }
    };
    drop(st);
    answer
}

/// 给采集路径调用方使用的布尔形式，在那里未定的探测和确定的「否」
/// 走同一条路（进入非 DRM 回退）。
pub(crate) fn is_available() -> bool {
    availability() == Availability::Available
}

/// `refresh_available_async` 的反面镜像：重新验证一个过期的 Unavailable，全程不会
/// 回答 Unknown。重新探测失败或为空时，会用一个新的时间戳重新确认那个「否」；
/// 只有非空的显示器列表才会翻转判定。
fn refresh_unavailable_async() {
    if DRM_PROBE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let in_flight = ProbeInFlightGuard;
    let sampled_gen = {
        let st = DRM_STATE.lock().unwrap();
        match &*st {
            ProbeState::Unavailable(since) if since.elapsed() >= NEGATIVE_TTL => {}
            _ => return,
        }
        DRM_STATE_GEN.load(Ordering::Acquire)
    };
    let spawned = std::thread::Builder::new()
        .name("drm-unavail-refresh".into())
        .spawn(move || {
            let _in_flight = in_flight;
            let result = query_displays(true);
            let mut st = DRM_STATE.lock().unwrap();
            if DRM_STATE_GEN.load(Ordering::Acquire) != sampled_gen {
                return;
            }
            match result {
                Ok(list) if !list.is_empty() => {
                    log::info!(
                        "drm: availability re-probe -> available ({} displays)",
                        list.len()
                    );
                    DRM_PROBE_FAILURES.store(0, Ordering::Relaxed);
                    publish_probe_state(&mut st, ProbeState::Available(Instant::now(), list));
                    drop(st);
                    scrap::wayland::display::clear_wayland_displays_cache();
                }
                _ => {
                    // 重新打戳：重新探测失败或为空，就是对「否」的一次新确认。
                    publish_probe_state(&mut st, ProbeState::Unavailable(Instant::now()));
                }
            }
        });
    // 出错时没有什么要释放：守卫已移入闭包，无论如何都会随它一起被丢弃。
    if let Err(err) = spawned {
        log::warn!("drm: could not spawn the unavailability re-probe thread: {err}");
    }
}

fn refresh_available_async() {
    if DRM_PROBE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let in_flight = ProbeInFlightGuard;
    let sampled_gen = {
        let st = DRM_STATE.lock().unwrap();
        if !matches!(&*st, ProbeState::Available(..)) {
            return;
        }
        DRM_STATE_GEN.load(Ordering::Acquire)
    };
    let spawned = std::thread::Builder::new()
        .name("drm-avail-refresh".into())
        .spawn(move || {
            let _in_flight = in_flight;
            let result = query_displays(true);
            let mut st = DRM_STATE.lock().unwrap();
            if DRM_STATE_GEN.load(Ordering::Acquire) != sampled_gen {
                return;
            }
            let failures = match &result {
                Ok(_) => {
                    DRM_REFRESH_FAILURES.store(0, Ordering::Relaxed);
                    0
                }
                Err(_) => DRM_REFRESH_FAILURES.fetch_add(1, Ordering::Relaxed) + 1,
            };
            match refresh_outcome(result.as_ref().ok().map(|l| l.len()), failures) {
                RefreshOutcome::Publish => {
                    let fresh = result.unwrap_or_default();
                    let changed = match &*st {
                        ProbeState::Available(_, old) => *old != fresh,
                        _ => true,
                    };
                    publish_probe_state(&mut st, ProbeState::Available(Instant::now(), fresh));
                    if changed {
                        drop(st);
                        scrap::wayland::display::clear_wayland_displays_cache();
                    }
                }
                RefreshOutcome::Unavailable => {
                    log::info!("drm: refresh -> 0 displays, marking DRM unavailable");
                    publish_probe_state(&mut st, ProbeState::Unavailable(Instant::now()));
                }
                // 只有 TTL 时间戳变了，因此这里不经过 publish_probe_state。
                RefreshOutcome::Restamp => {
                    if let ProbeState::Available(since, _) = &mut *st {
                        * since = Instant::now();
                    }
                }
                RefreshOutcome::GiveUp => {
                    log::info!(
                        "drm: availability refresh failed {failures}x ({:?}); the producer looks \
                         gone, dropping the cached verdict so the next enumeration re-probes",
                        result.as_ref().err()
                    );
                    DRM_REFRESH_FAILURES.store(0, Ordering::Relaxed);
                    publish_probe_state(&mut st, ProbeState::Unknown);
                }
            }
        });
    // 没有什么要释放：守卫已移入闭包并随之丢弃。清除该标志
    // 会变成两个探测同时运行，因为另一个刷新可能已经持有它。
    if let Err(err) = spawned {
        log::warn!(
            "drm: could not spawn the availability refresh thread: {err}; the cached verdict \
             stays stale until the next probe"
        );
    }
}

pub(super) fn warm_availability() {
    // 闸门放在循环内部，因为 `get_display_server()` 只要 loginctl
    // 还无法指出 seat0 会话，就会回答 "x11"。`is_x11_for_drm()` 就是那个形式，只是去掉了欢迎界面
    // 这一盲区，在那里普通的 `is_x11()` 永远是 true。
    for _ in 0..10 {
        if crate::platform::linux::is_x11_for_drm() {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        let sampled_gen = {
            let st = DRM_STATE.lock().unwrap();
            if matches!(&*st, ProbeState::Available(..)) {
                return;
            }
            DRM_STATE_GEN.load(Ordering::Acquire)
        };
        match query_displays(false) {
            Ok(list) if !list.is_empty() => {
                let mut st = DRM_STATE.lock().unwrap();
                // 在本次查询运行期间发布的登录探测会保留它那份更新的列表。
                if DRM_STATE_GEN.load(Ordering::Acquire) == sampled_gen {
                    log::info!("drm: consumer cache warmed ({} displays) at startup", list.len());
                    publish_probe_state(&mut st, ProbeState::Available(Instant::now(), list));
                }
                return;
            }
            Ok(_) => {
                log::info!("drm: no display is lit at startup; the first login probes again");
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(300)),
        }
    }
    log::info!("drm: consumer cache warm found no producer at startup (will probe lazily)");
}

/// 还没有任何判定时，登录会立即探测而不是回答「还没」；这可能会唤醒显示器。
/// 登录发现另一次探测正在运行时不会等待它。
pub(super) async fn settle_unknown_availability() {
    let unknown = matches!(&*DRM_STATE.lock().unwrap(), ProbeState::Unknown);
    if unknown {
        if let Err(err) = tokio::task::spawn_blocking(availability).await {
            log::warn!("drm: login availability settlement task failed: {err}");
        }
    }
}

/// 该服务会一直持有自己的答案，直到拓扑稳定。它只替换 `Available` 判定。
pub(super) async fn refresh_displays_for_login() {
    let sampled_gen = {
        let st = DRM_STATE.lock().unwrap();
        if !matches!(&*st, ProbeState::Available(..)) {
            return;
        }
        DRM_STATE_GEN.load(Ordering::Acquire)
    };
    let t = Instant::now();
    match query_displays_inner(true).await {
        Ok(list) if !list.is_empty() => {
            let changed = {
                let mut st = DRM_STATE.lock().unwrap();
                if DRM_STATE_GEN.load(Ordering::Acquire) != sampled_gen {
                    log::debug!(
                        "drm: login display refresh superseded while probing; keeping the newer list"
                    );
                    return;
                }
                match &*st {
                    ProbeState::Available(_, old) => {
                        let changed = *old != list;
                        log::debug!(
                            "drm: login display refresh -> {} display(s) in {:?}{}",
                            list.len(),
                            t.elapsed(),
                            if changed { " (list changed)" } else { "" }
                        );
                        publish_probe_state(&mut st, ProbeState::Available(Instant::now(), list));
                        changed
                    }
                    _ => return,
                }
            };
            if changed {
                scrap::wayland::display::clear_wayland_displays_cache();
            }
        }
        Ok(_) => log::debug!(
            "drm: login display refresh found no displays in {:?}; keeping the cached list",
            t.elapsed()
        ),
        Err(err) => log::debug!(
            "drm: login display refresh failed in {:?} ({err}); keeping the cached list",
            t.elapsed()
        ),
    }
}

/// 与 get_display_infos 对应：只有多显示器的主机才会公布被降级的显示器。
pub(super) fn display_count_and_any_demoted() -> Option<(usize, bool)> {
    // 在 DRM_STATE 下对身份键做快照，然后在释放 DRM_STATE 的情况下查询健康状态——
    // 顺序与 get_display_infos 相同：绝不要在获取按显示器的 map 时持有 DRM_STATE。
    let (len, keys): (usize, Vec<String>) = match &*DRM_STATE.lock().unwrap() {
        ProbeState::Available(_, list) => (
            list.len(),
            if list.len() > 1 {
                list.iter().map(connector_key).collect()
            } else {
                Vec::new()
            },
        ),
        _ => return None,
    };
    let any_demoted = if len > 1 {
        let health = DRM_DISPLAY_HEALTH.lock().unwrap();
        keys.iter()
            .any(|k| health.get(k).is_some_and(|h| h.demoted()))
    } else {
        false
    };
    Some((len, any_demoted))
}

// 一条多显示器的 portal 流无法替代某个被降级的连接器。保留它的索引，但把它标记为
// 离线；单个连接器仍可通过整个桌面的回退使用——除非那个
// 回退本身因几何原因被拒绝，那样的话把唯一的显示器公布为在线
// 会让视频服务对着一条什么都提供不了的流陷入重启循环。
fn mark_demoted_displays(list: &[DrmDisplayInfo], infos: &mut [DisplayInfo]) {
    let health = DRM_DISPLAY_HEALTH.lock().unwrap();
    if list.len() <= 1 {
        if let (Some(display), Some(info)) = (list.first(), infos.first_mut()) {
            if health
                .get(&connector_key(display))
                .is_some_and(|health| health.demoted() && health.fallback_rejected)
            {
                info.online = false;
            }
        }
        return;
    }
    for (display, info) in list.iter().zip(infos.iter_mut()) {
        if health
            .get(&connector_key(display))
            .is_some_and(|health| health.demoted())
        {
            info.online = false;
        }
    }
}

/// 这个显示器的 PipeWire 回退因几何原因被拒绝；记录下来是为了让单显示器的
/// 豁免不再公布什么都提供不了的显示器。交付一帧会清除它，
/// 降级冷却的重新武装也会清除它。
pub(super) fn mark_fallback_rejected(display_idx: usize) {
    let Some(expected) = display_info_of(display_idx as i32) else {
        return;
    };
    DRM_DISPLAY_HEALTH
        .lock()
        .unwrap()
        .entry(connector_key(&expected))
        .or_insert_with(DisplayHealth::new)
        .fallback_rejected = true;
}

fn primary_index_from_assignment(assignment: &[Option<usize>], primary: usize) -> usize {
    assignment
        .iter()
        .position(|assigned| *assigned == Some(primary))
        .unwrap_or(0)
}

/// 在获取 Wayland 锁和健康状态锁之前释放 DRM_STATE。
pub(super) fn get_display_infos_and_primary() -> Option<(Vec<DisplayInfo>, usize)> {
    let list = match &*DRM_STATE.lock().unwrap() {
        ProbeState::Available(_, list) => list.clone(),
        _ => return None,
    };
    let wl = scrap::wayland::display::get_displays();
    let assignment = assign_wayland_outputs(&list, &wl.displays);
    let mut infos = augment_with_wayland_geometry_from(&list, &wl, &assignment);
    mark_demoted_displays(&list, &mut infos);
    // 主显示器和几何信息必须使用同一份连接器分配快照。
    let primary = primary_index_from_assignment(&assignment, wl.primary);
    Some((infos, primary))
}

pub(super) fn get_display_infos() -> Option<Vec<DisplayInfo>> {
    let list = match &*DRM_STATE.lock().unwrap() {
        ProbeState::Available(_, list) => list.clone(),
        _ => return None,
    };
    let mut infos = augment_with_wayland_geometry(&list);
    mark_demoted_displays(&list, &mut infos);
    Some(infos)
}

/// DRM 把每台显示器都按物理尺寸、原点 (0,0) 上报，多显示器客户端就会叠在一起。
///
/// 有意在登录界面也这样问：欢迎界面会运行一个合成器，而 hbb_common 中的套接字回退
/// 让枚举器无需环境变量就能接触到它。在那个回退
/// 无法给出答案的地方，列表会返回空，一切都不做增强，这正是旧的
/// is-login-screen 闸门无条件产生的结果。
fn augment_with_wayland_geometry(drm: &[DrmDisplayInfo]) -> Vec<DisplayInfo> {
    let wl = scrap::wayland::display::get_displays();
    let assignment = assign_wayland_outputs(drm, &wl.displays);
    augment_with_wayland_geometry_from(drm, &wl, &assignment)
}

fn augment_with_wayland_geometry_from(
    drm: &[DrmDisplayInfo],
    wl: &scrap::wayland::display::Displays,
    matched: &[Option<usize>],
) -> Vec<DisplayInfo> {
    let mut infos: Vec<DisplayInfo> = drm.iter().map(display_info_from_drm).collect();
    // 单个显示器仍然会被增强：在多 GPU 主机上，这个服务唯一能
    // 打开的连接器在合成器布局中可能位于非零原点，而单独的 DRM 只上报 (0,0)。
    if drm.is_empty() {
        return infos;
    }
    if wl.displays.is_empty() {
        return infos;
    }
    // 一个连接器对应一个输出就是只看原点的情况：这个单独的输出仍可能位于
    // 本端看不到的非零原点，但它保持 scale 为 1 的约定——单个
    // 显示器是按物理尺寸公布的（参见 `logical_rects_of`），所以它的逻辑尺寸
    // 不能被采纳。连接器比那个输出多，就说明快照不一致，而
    // `assign_wayland_outputs` 中按布局顺序的回退会把那个原点安在猜测上。
    let origin_only = wl.displays.len() == 1;
    if origin_only && drm.len() > 1 {
        return infos;
    }
    let identity = identity_matches(drm, &wl.displays);
    for (i, info) in infos.iter_mut().enumerate() {
        let Some(w) = matched[i].map(|j| &wl.displays[j]) else {
            continue;
        };
        info.x = w.x;
        info.y = w.y;
        // 在看原点之前先算旋转后的尺寸：单独的旋转输出仍然交付旋转过的
        // 帧，因此必须按旋转后的尺寸公布；只有逻辑缩放的采纳才限定于多输出。
        // original_resolution 也在同一步随之改变，否则客户端会把转置后的
        // 当前尺寸与未转置的原始尺寸对比，误判为第三方的分辨率变化。
        // 只做身份匹配，与采集器变换遵循同样的规则：按
        // 布局顺序猜测交换，会公布采集器并不会交付的尺寸。
        let is_identity = identity[i].is_some() && identity[i] == matched[i];
        // 平面转置的显示器已经由 `display_info_from_drm` 交换过了。
        if is_identity && !drm[i].plane_transposed && (w.transform == 90 || w.transform == 270) {
            std::mem::swap(&mut info.width, &mut info.height);
            info.original_resolution = super::display_service::get_original_resolution(
                &drm[i].name,
                info.width as usize,
                info.height as usize,
            );
        }
        if origin_only {
            continue;
        }
        if let Some((lw, lh)) = w.logical_size {
            if lw > 0 && lh > 0 {
                // 交换后的宽度除以逻辑宽度，旋转时它到达时就已经交换过了：
                // 未旋转的分子会让一个旋转的 1:1 显示器公布出 16/9 的缩放。
                info.scale = info.width as f64 / lw as f64;
                info.original_resolution = super::display_service::get_original_resolution(
                    &drm[i].name,
                    lw as usize,
                    lh as usize,
                );
            }
        }
    }
    infos
}

/// 每个输出最多对应一个连接器；未匹配的连接器会取走同尺寸的下一个空闲输出，
/// 否则取布局顺序中的下一个空闲输出，因为不增强它们会让它们全都留在
/// DRM 的 (0,0) 上。
/// 分配中基于身份的那一半（名称，或唯一分辨率），使用与完整分配相同的渐进 `taken`
/// 过程。旋转在双方都以此为依据：按布局顺序交换或旋转
/// 猜测会让公布的尺寸与交付的帧对不上。
/// 身份分配分两轮全局进行：先预留每一个完全匹配名称的项，然后
/// 在剩下的未匹配项上按分辨率配对，而且只在被迫如此时才做——恰好一个
/// 空闲输出，且该分辨率下恰好一个未匹配的连接器。为较早的连接器做的分辨率猜测
/// 绝不能从较晚的连接器那里抢走一个完全匹配名称的机会。
fn identity_matches(
    drm: &[DrmDisplayInfo],
    wl: &[base::platform::linux::WaylandDisplayInfo],
) -> Vec<Option<usize>> {
    let mut taken = vec![false; wl.len()];
    let mut matched: Vec<Option<usize>> = vec![None; drm.len()];
    for (i, d) in drm.iter().enumerate() {
        let dn = normalize_connector(&d.name);
        if let Some((j, _)) = wl
            .iter()
            .enumerate()
            .find(|(j, w)| !taken[*j] && normalize_connector(&w.name) == dn)
        {
            matched[i] = Some(j);
            taken[j] = true;
        }
    }
    for (i, d) in drm.iter().enumerate() {
        if matched[i].is_some() {
            continue;
        }
        let free_same: Vec<usize> = wl
            .iter()
            .enumerate()
            .filter(|(j, w)| !taken[*j] && w.width == d.width as i32 && w.height == d.height as i32)
            .map(|(j, _)| j)
            .collect();
        let unmatched_same = drm
            .iter()
            .enumerate()
            .filter(|(k, o)| matched[*k].is_none() && o.width == d.width && o.height == d.height)
            .count();
        if free_same.len() == 1 && unmatched_same == 1 {
            matched[i] = Some(free_same[0]);
            taken[free_same[0]] = true;
        }
    }
    matched
}

fn assign_wayland_outputs(
    drm: &[DrmDisplayInfo],
    wl: &[base::platform::linux::WaylandDisplayInfo],
) -> Vec<Option<usize>> {
    let mut matched = identity_matches(drm, wl);
    let mut taken = vec![false; wl.len()];
    for m in matched.iter().flatten() {
        taken[*m] = true;
    }
    for (i, d) in drm.iter().enumerate() {
        if matched[i].is_some() {
            continue;
        }
        let free_same_size = wl
            .iter()
            .enumerate()
            .position(|(j, w)| !taken[j] && w.width == d.width as i32 && w.height == d.height as i32);
        let Some(j) = free_same_size.or_else(|| taken.iter().position(|t| !t)) else {
            continue; // more connectors than outputs; leave the rest unaugmented
        };
        log::warn!(
            "drm: connector {} matched no compositor output by name or by a unique resolution; \
             falling back to layout order and taking {} at ({}, {})",
            d.name,
            wl[j].name,
            wl[j].x,
            wl[j].y
        );
        matched[i] = Some(j);
        taken[j] = true;
    }
    matched
}


/// DRM 会插入一个单字母的类型区分符，而合成器会把它丢掉（"HDMI-A-1" -> "HDMI-1"）。
/// 只折叠*字母*：单个*数字*是 MST 端口索引，所以 "DP-1-2" 不是 "DP-2"。
fn normalize_connector(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() == 3 && parts[1].len() == 1 && parts[1].chars().all(|c| c.is_ascii_alphabetic()) {
        format!("{}-{}", parts[0], parts[2])
    } else {
        name.to_string()
    }
}

fn swap_available_displays(list: Vec<DrmDisplayInfo>) {
    let mut st = DRM_STATE.lock().unwrap();
    if matches!(&*st, ProbeState::Available(..)) {
        if list.is_empty() {
            log::info!("drm: hotplug refresh -> 0 displays, marking DRM unavailable");
            publish_probe_state(&mut st, ProbeState::Unavailable(Instant::now()));
        } else {
            log::info!("drm: hotplug refresh -> {} display(s)", list.len());
            publish_probe_state(&mut st, ProbeState::Available(Instant::now(), list));
        }
    }
}

fn display_info_from_drm(d: &DrmDisplayInfo) -> DisplayInfo {
    let (width, height) = rotated_dims(
        delivered_transform(d, 0),
        d.width as usize,
        d.height as usize,
    );
    let original_resolution =
        super::display_service::get_original_resolution(&d.name, width, height);
    DisplayInfo {
        x: d.x,
        y: d.y,
        width: width as i32,
        height: height as i32,
        name: d.name.clone(),
        online: d.active,
        cursor_embedded: false,
        original_resolution,
        scale: 1.0,
        ..Default::default()
    }
}

/// 有意不把握手列表发布到 DRM_STATE：它是在一次可能
/// 长达数秒的停顿之前读取的，而且当 `wire_idx != display_idx` 时它的顺序也不同。
pub(super) fn get_capturer_info(
    display_idx: usize,
) -> ResultType<super::video_service::CapturerInfo> {
    let expected = display_info_of(display_idx as i32);
    let key = expected.as_ref().map(connector_key);
    {
        let mut map = DRM_DISPLAY_HEALTH.lock().unwrap();
        if let Some(h) = key.as_ref().and_then(|k| map.get_mut(k)) {
            if h.zero_frame_streak >= DRM_GRAB_MAX_FAILURES {
                if h.demoted() {
                    bail!(
                        "drm capture for display {display_idx} repeatedly produced no frame; using PipeWire"
                    );
                }
                h.zero_frame_streak = 0;
                h.since = Instant::now();
                // 冷却会为这个显示器重新武装 DRM，因此回退判定也会重新开始。
                h.fallback_rejected = false;
            }
        }
    }
    // 首先构建：瞬时的 `_drm` 中断绝不能计入下面的抖动阈值。
    let (capturer, displays, wire_idx, origin) = IpcDrmCapturer::new(display_idx as i32, expected)?;
    // 初次构建计为 0，因此降级会在一个窗口内第 (RAPID_REBUILD_MAX + 1) 次触发。
    if let Some(key) = key.clone() {
        let now = Instant::now();
        let mut map = DRM_DISPLAY_HEALTH.lock().unwrap();
        let h = map.entry(key).or_insert_with(DisplayHealth::new);
        h.rapid_builds = match h.last_build {
            Some(last) if now.duration_since(last) < RAPID_REBUILD_WINDOW => h.rapid_builds + 1,
            _ => 0,
        };
        h.last_build = Some(now);
        if h.rapid_builds >= RAPID_REBUILD_MAX {
            log::warn!(
                "drm: display {display_idx} rebuilt {} times within {RAPID_REBUILD_WINDOW:?}; flapping, falling back to PipeWire",
                h.rapid_builds
            );
            h.zero_frame_streak = DRM_GRAB_MAX_FAILURES;
            h.since = now;
            h.demotes += 1;
            bail!("drm capture for display {display_idx} is flapping; using PipeWire");
        }
    }
    let ndisplay = displays.len();
    // 取自该流所绑定的条目；`display_idx` 是客户端列表中的位置。
    let d = displays
        .get(wire_idx)
        .ok_or_else(|| anyhow!("drm display index {wire_idx} out of range ({ndisplay})"))?
        .clone();
    // 原点和变换取自 new() 解析出的那一份快照，因此两者反映的是同一个
    // 输出分配；尺寸保持物理值，并旋转到帧的方向。
    let origin = origin.unwrap_or((d.x, d.y));
    let (cap_w, cap_h) = rotated_dims(
        delivered_transform(&d, capturer.transform),
        d.width as usize,
        d.height as usize,
    );
    Ok(super::video_service::CapturerInfo {
        origin,
        width: cap_w,
        height: cap_h,
        ndisplay,
        current: display_idx,
        privacy_mode_id: 0,
        _capturer_privacy_mode_id: 0,
        capturer: Box::new(capturer),
    })
}

#[cfg(test)]
mod drm_capturer_tests {
    use super::*;

    fn capturer_with(session: Option<(usize, usize)>) -> IpcDrmCapturer {
        capturer_named(session, None)
    }

    // DRM_DISPLAY_HEALTH 是进程级的，而测试并行运行：请给每个测试传入它自己的键。
    fn capturer_named(session: Option<(usize, usize)>, key: Option<&str>) -> IpcDrmCapturer {
        let connector = key.map(|k| k.to_owned());
        IpcDrmCapturer {
            shared: Arc::new(Shared {
                slot: Mutex::new(FrameSlot {
                    latest: None,
                    free: [None, None],
                    ended: None,
                }),
                cv: Condvar::new(),
                cursor_transform: std::sync::atomic::AtomicI32::new(0),
                cal_context: Mutex::new(None),
                demand_fps: std::sync::atomic::AtomicU8::new(0),
                pace: Mutex::new(EncoderPace::default()),
            }),
            stop: Arc::new(AtomicBool::new(false)),
            display: 0,
            connector,
            session_size: session,
            transform: 0,
            snapshot_gen: scrap::wayland::display::wayland_snapshot_generation(),
            cur: Vec::new(),
            cur_w: 0,
            cur_h: 0,
            cur_fmt: Pixfmt::BGRA,
            got_frame: false,
            spare: Vec::new(),
            returned_at: Instant::now(),
        }
    }

    /// 每个标签字节对应一个 BGRA 像素，因此旋转的结果读起来就是一张标签矩阵。
    fn px_frame(labels: &[&[u8]], pad_bytes: usize) -> (Vec<u8>, usize, usize) {
        let h = labels.len();
        let w = labels[0].len();
        let mut buf = Vec::new();
        for row in labels {
            for &l in *row {
                buf.extend_from_slice(&[l, l, l, 255]);
            }
            buf.extend(std::iter::repeat(0u8).take(pad_bytes));
        }
        (buf, w, h)
    }

    fn labels_of(buf: &[u8], w: usize, h: usize) -> Vec<Vec<u8>> {
        (0..h)
            .map(|y| (0..w).map(|x| buf[(y * w + x) * 4]).collect())
            .collect()
    }

    #[test]
    fn the_cursor_id_names_the_orientation_too() {
        // 同一个 wire 光标在两种变换下必须发布为两个 id，否则客户端按 id 的
        // 缓存会在会话中途旋转之后仍提供之前的方向。
        let wire = 0xDEAD_BEEF_u64;
        assert_ne!(fold_cursor_id(wire, 0), fold_cursor_id(wire, 90));
        assert_ne!(fold_cursor_id(wire, 90), fold_cursor_id(wire, 270));
        // 对 (id, transform) 是确定性的，因此未变化的光标仍会被去重。
        assert_eq!(fold_cursor_id(wire, 90), fold_cursor_id(wire, 90));
        // 隐藏哨兵在消费端是按值比较的，因此它必须原样通过（不折叠）。
        let hidden = scrap::drm_reader::HIDDEN_CURSOR_ID;
        assert_eq!(fold_cursor_id(hidden, 90), hidden);
    }

    #[test]
    fn unrotate_hotspot_follows_the_pixel_mapping() {
        // 宽 3 高 2，热点在 (2,0)（右上角）：旋转 90 度后（左列变成顶
        // 行），该像素在 2x3 的结果中位于 (1,2)；270 度则把它送到 (0,0)。
        assert_eq!(unrotate_hotspot(90, 3, 2, 2, 0), (1, 2));
        assert_eq!(unrotate_hotspot(270, 3, 2, 2, 0), (0, 0));
        assert_eq!(unrotate_hotspot(180, 3, 2, 2, 0), (0, 1));
        assert_eq!(unrotate_hotspot(0, 3, 2, 2, 0), (2, 0));
    }

    #[test]
    fn unrotate_90_maps_the_left_column_to_the_top_row() {
        // 来自 rustdesk#15886 的实测锚点：mutter 的 transform=1 会把面板栏带到下
        // 方（scanout 的左边缘），而正立意味着那条边变成顶行。
        let (src, w, h) = px_frame(&[&[1, 2, 3], &[4, 5, 6]], 0);
        let mut dst = Vec::new();
        unrotate_bgra(&src, w, h, 90, &mut dst);
        // src 左列自上而下 = [1, 4]；顺时针旋转会把它放到顶行，变成 [4, 1]。
        assert_eq!(labels_of(&dst, h, w), vec![vec![4, 1], vec![5, 2], vec![6, 3]]);
    }

    /// 一个正立的箭头，尖端在 (0,0)：一个 4x6 的位图，所有满足 x <= y/2 的像素都是不透明的，因此
    /// 不透明区域是左边 3 列。它的左上角就是尖端——这正是
    /// 物理机上的猜测在精灵图正立时才有效，而且仅此而已。
    fn arrow() -> (Vec<u8>, usize, usize) {
        let (w, h) = (4usize, 6usize);
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                if x <= y / 2 {
                    px[(y * w + x) * 4 + 3] = 255;
                }
            }
        }
        (px, w, h)
    }

    /// 在实际重要的宽高比下的形状：一个 20x9 的不透明区域，这正是
    /// 已安装的 Adwaita `vertical-text` 在 24 px 下测得的尺寸。2.22:1，只是刚刚超过该规则的
    /// 两倍阈值。像真实情况那样偏心放在一个 24x24 的缓冲区里，区域为 (2,6)-(21,14)，
    /// 中心 (11,10)——与主题文件给出的数字相同，对照它声明的热点
    /// (12,11).
    fn ibeam_theme_aspect() -> (Vec<u8>, usize, usize, (i32, i32)) {
        let (w, h) = (24usize, 24usize);
        let mut px = vec![0u8; w * h * 4];
        for y in 6..15 {
            for x in 2..22 {
                px[(y * w + x) * 4 + 3] = 255;
            }
        }
        (px, w, h, (11, 10))
    }

    /// 在旋转了 `t` 的输出上合成器放进光标平面的内容：正立的精灵图
    /// 按 t 正向旋转后的结果，这正是把它转回 (360 - t) 度所得到的。
    fn as_scanned_out(upright: &[u8], w: usize, h: usize, t: i32) -> (Vec<u8>, usize, usize) {
        let mut out = Vec::new();
        unrotate_bgra(upright, w, h, (360 - t) % 360, &mut out);
        let (dw, dh) = rotated_dims(t, w, h);
        (out, dw, dh)
    }

    /// 升级窗口，完整地从 WIRE 端到端验证，而不是从结构体字面量开始。
    ///
    /// 旧的 root 服务可能正在向刚启动的 `--server` 推流，而它的 `DrmCursor`
    /// 完全没有携带 `hot_measured`。这里取这样的生产者放到
    /// 套接字上的确切字节，用消费端接收循环相同的方式反序列化，并驱动真实的
    /// 交付路径，然后把发布出去的内容读回来。用两个活着的进程来搭建它是
    /// 不可能的，因为这个包的构建方式如此——来自某次构建的 `--server` 无法在
    /// 另一个的目录树里运行——所以这就是那个测试的诚实形式，它会在 CI 中运行，
    /// 而不是在某个人的桌面上跑一次了。
    ///
    /// 变换必须非零：为 0 时交付路径绝不会查询 `hot_measured`，因此在 0 处的
    /// 测试无法区分这两个分支。
    #[test]
    fn a_legacy_cursor_message_keeps_the_hotspot_the_old_protocol_meant() {
        use crate::ipc::Data;

        let (up, w, h, _box_centre) = ibeam_theme_aspect();
        let t = 90;
        let (scan, sw, sh) = as_scanned_out(&up, w, h, t);
        // 旧生产者测得并作为扫描输出精灵图上一个点发来的热点。
        let (hotx, hoty) = (3, 5);

        // 正是旧生产者写出的东西：`Data` 是相邻标签式的，而且没有
        // `hot_measured` 键，因为那个构建没有这个字段。
        let legacy = format!(
            r#"{{"t":"DrmCursor","c":{{"id":11,"width":{sw},"height":{sh},"hotx":{hotx},"hoty":{hoty}}}}}"#
        );
        let (id, mw, mh, mx, my, measured) =
            match serde_json::from_str::<Data>(&legacy).expect("a legacy DrmCursor must parse") {
                Data::DrmCursor { id, width, height, hotx, hoty, hot_measured } => {
                    (id, width, height, hotx, hoty, hot_measured)
                }
                other => panic!("expected DrmCursor, got {other:?}"),
            };
        assert!(measured, "no provenance on the wire must mean what the old protocol meant");

        // 两个分支在这里必须不同，否则下面的断言什么也证明不了。
        let mapped = unrotate_hotspot(t, mw as i32, mh as i32, mx, my);
        let mut turned = Vec::new();
        unrotate_bgra(&scan, sw, sh, t, &mut turned);
        let (dw, dh) = rotated_dims(t, sw, sh);
        let reinferred = scrap::drm_reader::infer_hotspot(&turned, dw, dh);
        assert_ne!(
            mapped, reinferred,
            "fixture cannot discriminate: mapping and re-inferring give the same point"
        );

        let display = 9_400;
        deliver_drm_cursor(display, 1, id, mw, mh, mx, my, measured, scan, t);
        let published = {
            let map = DRM_CURSOR.lock().unwrap();
            let (_, c) = map.get(&display).expect("the delivery path published nothing");
            (c.hotx, c.hoty)
        };
        assert_eq!(
            published, mapped,
            "a legacy message must keep the hotspot the producer sent, transformed as a point; \
             re-inferring it would move the cursor for the length of an upgrade"
        );

        // 对照：一个请求重新推断的新生产者会明确说明这一点，而这
        // 不能被默认值吞掉。
        let modern = format!(
            r#"{{"t":"DrmCursor","c":{{"id":12,"width":{sw},"height":{sh},"hotx":{hotx},"hoty":{hoty},"hot_measured":false}}}}"#
        );
        let Data::DrmCursor { hot_measured, .. } =
            serde_json::from_str::<Data>(&modern).expect("a modern DrmCursor must parse")
        else {
            panic!("expected DrmCursor")
        };
        assert!(!hot_measured);
        let (scan2, _, _) = as_scanned_out(&up, w, h, t);
        deliver_drm_cursor(display + 1, 1, 12, sw as u32, sh as u32, hotx, hoty, false, scan2, t);
        let published2 = {
            let map = DRM_CURSOR.lock().unwrap();
            let (_, c) = map.get(&(display + 1)).expect("nothing published for the control");
            (c.hotx, c.hoty)
        };
        assert_eq!(
            published2, reinferred,
            "an explicit false must still re-infer, or the legacy default has eaten the new \
             producer's request"
        );
    }

    #[test]
    fn the_cursor_path_publishes_an_upright_sprite_at_every_angle() {
        let (up, w, h) = arrow();
        for (i, t) in [0, 90, 180, 270].into_iter().enumerate() {
            // 每个角度都有自己的显示器 id：DRM_CURSOR 是进程级的，并且以显示器为键。
            let display = 9_100 + i as i32;
            let (scan, sw, sh) = as_scanned_out(&up, w, h, t);
            deliver_drm_cursor(display, 1, 7, sw as u32, sh as u32, 0, 0, false, scan, t);
            let map = DRM_CURSOR.lock().unwrap();
            let (_, c) = map.get(&display).expect("the cursor path published nothing");
            assert_eq!(
                (c.width as usize, c.height as usize),
                (w, h),
                "the published sprite is upright-sized at {t}"
            );
            assert_eq!(c.colors, up, "the published sprite is upright at {t}");
            assert_eq!((c.hotx, c.hoty), (0, 0), "the tip is the hotspot at {t}");
        }
    }

    /// 合成器根据 CRTC 变换对平面编程，而该变换是逻辑变换
    /// 与连接器的面板方向复合而成的（mutter: meta_output_logical_to_crtc_transform）；
    /// `wl_output` 只携带逻辑部分。一个在逻辑 0 处倒装的面板，其
    /// 平面处于 rotate-180，帧缓冲是正立的：相减会把它翻转，而
    /// master 交付的是正立的。因此平面的答案不与 `wl_output` 对照测量。
    #[test]
    fn a_plane_that_rotated_holds_the_logical_desktop_whatever_wl_output_says() {
        assert_eq!(frame_transform(0, Some(0x4)), 0);
        assert_eq!(frame_transform(90, Some(0x4)), 0);
        assert_eq!(frame_transform(180, Some(0x2)), 0);
        assert_eq!(frame_transform(90, Some(0x8)), 0);
    }

    #[test]
    fn a_180_frame_is_turned_only_when_the_plane_did_not_turn_it() {
        use scrap::TraitPixelBuffer;
        let (src, w, h) = px_frame(&[&[1, 2, 3], &[4, 5, 6]], 0);
        let mut c = capturer_with(Some((w, h)));
        c.transform = 180;
        // 软件旋转（virtio-gpu）：scanout 是上下颠倒的，回来时是正立的。
        put_frame_with(&c, w, h, Some(0x1), &src);
        match c.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(pb)) => {
                assert_eq!((pb.width(), pb.height()), (w, h));
                assert_eq!(labels_of(pb.data(), w, h), vec![vec![6, 5, 4], vec![3, 2, 1]]);
            }
            Ok(_) => panic!("expected a pixel-buffer frame"),
            Err(err) => panic!("expected a delivered frame, got {err}"),
        }
        // 又是同一个 scanout：一旦转过，它会重复上面那一帧，因此被丢弃。
        put_frame_with(&c, w, h, Some(0x1), &src);
        assert!(matches!(
            c.frame(Duration::from_millis(50)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
        ));
        // 硬件旋转（i915 + mutter）：scanout 已经是正立的，保持不动。
        put_frame_with(&c, w, h, Some(0x4), &src);
        match c.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(pb)) => {
                assert_eq!(labels_of(pb.data(), w, h), vec![vec![1, 2, 3], vec![4, 5, 6]]);
            }
            Ok(_) => panic!("expected a pixel-buffer frame"),
            Err(err) => panic!("expected a delivered frame, got {err}"),
        }
        // 无法表达的生产者（0.5.8 之前的库）沿用旧规则：180 不动。
        // 一幅新的图像：`src` 保持不动就是上面那一帧，会被当作重复帧丢弃。
        let (other, _, _) = px_frame(&[&[7, 8, 9], &[10, 11, 12]], 0);
        put_frame_with(&c, w, h, None, &other);
        match c.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(pb)) => {
                assert_eq!(labels_of(pb.data(), w, h), vec![vec![7, 8, 9], vec![10, 11, 12]]);
            }
            Ok(_) => panic!("expected a pixel-buffer frame"),
            Err(err) => panic!("expected a delivered frame, got {err}"),
        }
    }

    #[test]
    fn a_frame_equal_to_the_last_one_goes_out_once_a_second() {
        let (still, w, h) = px_frame(&[&[1, 2, 3], &[4, 5, 6]], 0);
        let (moved, _, _) = px_frame(&[&[1, 2, 3], &[4, 5, 7]], 0);
        let mut c = capturer_with(Some((w, h)));
        put_frame_with(&c, w, h, None, &still);
        assert!(c.frame(Duration::from_millis(50)).is_ok());
        put_frame_with(&c, w, h, None, &still);
        assert!(matches!(
            c.frame(Duration::from_millis(50)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
        ));
        // 一秒钟后它会再次发出，以防它的编码失败，第二次重新开始。
        std::thread::sleep(REPEAT_EVERY);
        put_frame_with(&c, w, h, None, &still);
        assert!(c.frame(Duration::from_millis(50)).is_ok());
        put_frame_with(&c, w, h, None, &still);
        assert!(matches!(
            c.frame(Duration::from_millis(50)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
        ));
        put_frame_with(&c, w, h, None, &moved);
        assert!(c.frame(Duration::from_millis(50)).is_ok());
    }

    #[test]
    fn a_portrait_scanout_from_a_plane_that_rotated_90_is_left_alone() {
        use scrap::TraitPixelBuffer;
        // 一个 2x3 的竖向 scanout：平面做了那 90 度旋转，因此像素已经是正立的。
        let (portrait, pw, ph) = px_frame(&[&[1, 2], &[3, 4], &[5, 6]], 0);
        let mut c = capturer_with(Some((pw, ph)));
        c.transform = 90;
        put_frame_with(&c, pw, ph, Some(0x2), &portrait);
        match c.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(pb)) => {
                assert_eq!((pb.width(), pb.height()), (pw, ph));
                assert_eq!(labels_of(pb.data(), pw, ph), vec![vec![1, 2], vec![3, 4], vec![5, 6]]);
            }
            Ok(_) => panic!("expected a pixel-buffer frame"),
            Err(err) => panic!("expected a delivered frame, got {err}"),
        }
        // 同一个输出，但平面什么都没做：3x2 的横向模式到达后被
        // 以与 `unrotate_bgra` 旋转 90 度帧完全相同的方式转过。
        let (landscape, lw, lh) = px_frame(&[&[1, 2, 3], &[4, 5, 6]], 0);
        let mut expected = Vec::new();
        unrotate_bgra(&landscape, lw, lh, 90, &mut expected);
        put_frame_with(&c, lw, lh, Some(0x1), &landscape);
        match c.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(pb)) => {
                assert_eq!((pb.width(), pb.height()), (pw, ph));
                assert_eq!(pb.data(), &expected[..]);
            }
            Ok(_) => panic!("expected a pixel-buffer frame"),
            Err(err) => panic!("expected a delivered frame, got {err}"),
        }
    }

    #[test]
    fn unrotate_reads_padded_strides_and_writes_tight() {
        // 行跨度由 len/h 推导得出，因此有填充的源不能把结果拉斜。
        let (src, w, h) = px_frame(&[&[1, 2, 3], &[4, 5, 6]], 8);
        let mut dst = Vec::new();
        unrotate_bgra(&src, w, h, 90, &mut dst);
        assert_eq!(dst.len(), w * h * 4);
        assert_eq!(labels_of(&dst, h, w), vec![vec![4, 1], vec![5, 2], vec![6, 3]]);
        let mut plain = Vec::new();
        unrotate_bgra(&src, w, h, 0, &mut plain);
        assert_eq!(labels_of(&plain, w, h), vec![vec![1, 2, 3], vec![4, 5, 6]]);
    }

    #[test]
    fn a_rotated_session_delivers_rotated_frames_and_guards_in_rotated_dims() {
        use scrap::TraitPixelBuffer;
        let mut c = capturer_with(Some((32, 64))); // rotated session of a 64x32 scanout
        c.transform = 90;
        put_frame(&c, 64, 32);
        match c.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(pb)) => {
                assert_eq!((pb.width(), pb.height()), (32, 64));
            }
            Ok(_) => panic!("expected a pixel-buffer frame"),
            Err(err) => panic!("expected a delivered frame, got {err}"),
        }
        // scanout 变化同样会结束会话，并以旋转后的尺寸上报。
        put_frame(&c, 32, 64);
        let err = match c.frame(Duration::from_millis(50)) {
            Err(e) => e,
            Ok(_) => panic!("a scanout change must end a rotated session too"),
        };
        assert!(err.to_string().contains("(32x64 -> 64x32)"), "{err}");
    }

    fn zero_frame_streak_of(c: &IpcDrmCapturer) -> u32 {
        let key = c.connector.clone().expect("this check needs an identity");
        DRM_DISPLAY_HEALTH
            .lock()
            .unwrap()
            .get(&key)
            .map(|h| h.zero_frame_streak)
            .unwrap_or(0)
    }

    fn put_frame(c: &IpcDrmCapturer, w: usize, h: usize) {
        put_frame_rot(c, w, h, None);
    }

    fn put_frame_rot(c: &IpcDrmCapturer, w: usize, h: usize, plane_rotation: Option<u32>) {
        // 每次调用的第一个像素都不同：frame() 会丢弃与上一帧相同的帧。
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut buf = c.shared.slot.lock().unwrap().take_free().unwrap_or_default();
        buf.clear();
        buf.resize(w * h * 4, 0);
        buf[..4].copy_from_slice(&SEQ.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let mut slot = c.shared.slot.lock().unwrap();
        slot.publish(w, h, Pixfmt::BGRA, plane_rotation, buf);
        c.shared.cv.notify_one();
    }

    fn put_frame_with(
        c: &IpcDrmCapturer,
        w: usize,
        h: usize,
        plane_rotation: Option<u32>,
        pixels: &[u8],
    ) {
        let mut buf = c.shared.slot.lock().unwrap().take_free().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(pixels);
        assert_eq!(buf.len(), w * h * 4);
        let mut slot = c.shared.slot.lock().unwrap();
        slot.publish(w, h, Pixfmt::BGRA, plane_rotation, buf);
        c.shared.cv.notify_one();
    }

    #[test]
    fn a_smaller_frame_ends_the_session_instead_of_being_encoded() {
        let mut c = capturer_named(Some((1920, 1080)), Some("test:mid-session-shrink"));
        put_frame(&c, 1920, 1080);
        assert!(matches!(c.frame(Duration::from_millis(50)), Ok(_)));
        put_frame(&c, 1280, 720);
        let err = match c.frame(Duration::from_millis(50)) {
            Err(e) => e,
            Ok(_) => panic!("a mid-session shrink must be a hard error, not a delivered frame"),
        };
        assert!(err.to_string().contains("changed geometry mid-session"));
        assert!(
            c.got_frame,
            "the rebuild must not look like a display that never produced a frame"
        );
        assert_eq!(
            zero_frame_streak_of(&c),
            0,
            "a session that streamed must not be counted as one that produced nothing"
        );
    }

    // ---- 光标热点校准 ----
    // 缓存和 DRM_CURSOR 都是进程级的，而 libtest 并发运行测试：下面每个测试
    // 都有自己的显示器 id（9_700..9_799）和自己的形状 id，而那些会读回
    // 缓存的测试持有这把锁。
    static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const CAL_GEN: u64 = 7;
    fn ctx1() -> CalContext {
        CalContext {
            rect: (0, 0, 1920, 1080),
            physical_size: (1920, 1080),
            built_gen: CAL_GEN,
        }
    }
    fn peer(pos: Pos, seq: u64) -> crate::server::input_service::PeerAbsSample {
        peer_under(pos, seq, CAL_GEN, 0)
    }
    /// 在布局代际 `gen` 和输入映射计数 `map_epoch` 下注入的样本。
    fn peer_under(
        pos: Pos,
        seq: u64,
        gen: u64,
        map_epoch: u64,
    ) -> crate::server::input_service::PeerAbsSample {
        crate::server::input_service::PeerAbsSample {
            pos,
            age_ms: 400,
            seq,
            gen,
            map_epoch,
        }
    }
    /// 一个猜测的形状到达带有上下文的流上，接收循环就是这样登记它的。
    fn arrive(display: i32, epoch: u64, shape: u64, wire: Hotspot) -> Option<CursorCal> {
        on_cursor_shape(
            display,
            epoch,
            Some(ctx1()),
            shape,
            64,
            64,
            wire.0,
            wire.1,
            false,
            vec![0; 64 * 64 * 4],
            0,
        )
    }
    /// 一个稳定的平面位置对应一个样本：第一帧报告这次移动，
    /// 随后的稳定帧让它确定下来，最后一帧进行测量。
    fn settle(
        cal: &mut Option<CursorCal>,
        plane: Pos,
        sample: Option<crate::server::input_service::PeerAbsSample>,
        display: i32,
        epoch: u64,
    ) {
        settle_under(cal, plane, sample, display, epoch, (CAL_GEN, CAL_GEN, 0));
    }
    /// `settle` 在（布局代际、设备运行的 uinput 范围所属的代际、
    /// 输入映射计数）下稳定下来，按接收循环读取它们的方式。
    fn settle_under(
        cal: &mut Option<CursorCal>,
        plane: Pos,
        sample: Option<crate::server::input_service::PeerAbsSample>,
        display: i32,
        epoch: u64,
        (gen, map_gen, map_epoch): (u64, u64, u64),
    ) {
        for _ in 0..=CURSOR_CAL_STABLE_TICKS {
            on_cursor_plane(cal, Some(plane), || sample, false, gen, map_gen, map_epoch, display, epoch);
        }
    }
    fn served(display: i32) -> (u64, i32, i32) {
        let map = DRM_CURSOR.lock().unwrap();
        let (_, c) = map.get(&display).expect("a shape was published for this display");
        (c.id, c.hotx, c.hoty)
    }
    fn plain(shape: u64) -> u64 {
        fold_cursor_id(shape, 0)
    }
    fn corrected(shape: u64, h: Hotspot) -> u64 {
        fold_cursor_id(remix_cursor_id(shape, h.0, h.1), 0)
    }

    // 两个稳定位置对应两个样本：第一次测量是一个候选值，不改变
    // 任何可见内容；第二次测量相距 1 像素，确认了它，之后的一切都使用
    // 保留下来的候选值，而不是第二个样本。捕获到的变异：发布第一次
    // 测量、用新值确认、以及去掉 seq 检查。
    #[test]
    fn a_confirming_measurement_publishes_the_retained_candidate_not_the_new_sample() {
        let _serial = CACHE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (d, e, shape) = (9_700, next_cursor_epoch(), 0x7a00);
        forget_cursor_cal(shape, CAL_GEN);
        let mut cal = arrive(d, e, shape, (4, 4));
        assert_eq!(served(d), (plain(shape), 4, 4));
        // (500,300) - (488,288) = (12,12)：候选值。
        settle(&mut cal, (488, 288), Some(peer((500, 300), 1)), d, e);
        assert_eq!(cal.as_ref().unwrap().candidate, Some(((12, 12), 1, 0)));
        assert_eq!(cal.as_ref().unwrap().current_hot, (4, 4));
        assert_eq!(served(d), (plain(shape), 4, 4), "unconfirmed: nothing visible changes");
        assert_eq!(cached_cursor_cal(shape, CAL_GEN), None);
        // (600,400) - (587,388) = (13,12)：确认了 (12,12)。
        settle(&mut cal, (587, 388), Some(peer((600, 400), 2)), d, e);
        let c = cal.as_ref().unwrap();
        assert_eq!(c.candidate, None);
        assert_eq!(c.current_hot, (12, 12));
        assert_eq!(cached_cursor_cal(shape, CAL_GEN), Some((12, 12)));
        assert_eq!(served(d), (corrected(shape, (12, 12)), 12, 12));
        forget_cursor_cal(shape, CAL_GEN);
    }

    #[test]
    fn a_calibration_context_exists_only_for_an_identity_matched_upright_output() {
        let one = |t: i32| {
            let mut o = wl_display("HDMI-1", 0, 0, 1920, 1080);
            o.transform = t;
            scrap::wayland::display::Displays {
                primary: 0,
                displays: vec![o],
            }
        };
        let drm = [drm_display("HDMI-A-1", 1920, 1080)];
        assert_eq!(
            calibration_context(&drm, 0, &one(0), 7),
            Ok(CalContext {
                rect: (0, 0, 1920, 1080),
                physical_size: (1920, 1080),
                built_gen: 7
            })
        );
        for t in [90, 180, 270] {
            assert!(calibration_context(&drm, 0, &one(t), 7).is_err(), "transform {t}");
        }
        // 做了旋转的平面会把 180 折叠成 0 用于帧；这个折叠绝不能传到
        // 校准那里。
        assert_eq!(frame_transform(transform_and_origin(&drm, 0, &one(180)).0, Some(0x4)), 0);
        // 部分快照：两个连接器，一个输出，其中一个甚至名称匹配。
        let two = [
            drm_display("HDMI-A-1", 1920, 1080),
            drm_display("DP-1", 2560, 1440),
        ];
        assert!(calibration_context(&two, 0, &one(0), 7).is_err());
        // 没有身份匹配：帧路径会猜一个原点，而校准绝不能猜。
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![
                wl_display("DP-1", 0, 0, 2560, 1440),
                wl_display("DP-2", 2560, 0, 3840, 2160),
            ],
        };
        assert!(calibration_context(&drm, 0, &wl, 7).is_err());
        assert_eq!(transform_and_origin(&drm, 0, &wl).0, 0);
        // 退化的几何。
        let mut a = wl_display("HDMI-1", 0, 0, 1920, 1080);
        a.logical_size = Some((0, 0));
        let mut b = wl_display("DP-1", 1920, 0, 2560, 1440);
        b.logical_size = Some((0, 0));
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![a, b],
        };
        assert!(calibration_context(&two, 0, &wl, 7).is_err());
        // wire_idx = 1：旋转的索引 0 拒绝，正立的索引 1 给出它自己的矩形。
        let mut r = wl_display("HDMI-1", 0, 0, 1920, 1080);
        r.transform = 90;
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![r, wl_display("DP-1", 1920, 0, 2560, 1440)],
        };
        assert!(calibration_context(&two, 0, &wl, 7).is_err());
        assert_eq!(
            calibration_context(&two, 1, &wl, 7).map(|c| (c.rect, c.physical_size)),
            Ok(((1920, 0, 2560, 1440), (2560, 1440)))
        );
        // 链式且带缩放：T2 面板，2880x1800 扫描输出一个 1986x1241 的逻辑矩形。
        let panel = [
            drm_display("eDP-1", 2880, 1800),
            drm_display("DP-6", 1920, 1080),
        ];
        let mut p = wl_display("eDP-1", 0, 0, 2880, 1800);
        p.logical_size = Some((1986, 1241));
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![p, wl_display("DP-6", 1986, 0, 1920, 1080)],
        };
        let ctx = calibration_context(&panel, 0, &wl, 7).unwrap();
        assert_eq!(ctx.rect, (0, 0, 1986, 1241));
        assert_eq!(measure_hotspot(&ctx, (1643, 577), (2357, 814), (128, 128)), Some((26, 23)));
    }

    #[test]
    fn the_measurement_is_the_injected_tip_in_scanout_space_minus_the_plane() {
        let ctx = ctx1();
        assert_eq!(measure_hotspot(&ctx, (500, 300), (488, 288), (32, 32)), Some((12, 12)));
        // 发现空间不匹配的那次 T2 读数：2880x1800 被公布为 1986x1241。
        let scaled = CalContext {
            rect: (0, 0, 1986, 1241),
            physical_size: (2880, 1800),
            built_gen: CAL_GEN,
        };
        assert_eq!(measure_hotspot(&scaled, (1643, 577), (2357, 814), (128, 128)), Some((26, 23)));
        // 指针在另一台显示器上：由那条流来测量，不是这条。
        let right = CalContext {
            rect: (1920, 0, 1920, 1080),
            ..ctx
        };
        assert_eq!(measure_hotspot(&right, (500, 300), (488, 288), (32, 32)), None);
        // 落在位图之外：离得很远，而且平面在尖端之前。
        assert_eq!(measure_hotspot(&ctx, (900, 300), (488, 288), (32, 32)), None);
        assert_eq!(measure_hotspot(&ctx, (480, 300), (488, 288), (32, 32)), None);
    }

    fn drm_display(name: &str, w: u32, h: u32) -> DrmDisplayInfo {
        DrmDisplayInfo {
            name: name.to_owned(),
            crtc_id: 1,
            x: 0,
            y: 0,
            width: w,
            height: h,
            active: true,
            render_node: String::new(),
            device: String::new(),
            plane_transposed: false,
        }
    }

    fn wl_display(
        name: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> base::platform::linux::WaylandDisplayInfo {
        base::platform::linux::WaylandDisplayInfo {
            name: name.to_owned(),
            x,
            y,
            width: w,
            height: h,
            logical_size: Some((w, h)),
            refresh_rate: 60,
            transform: 0,
        }
    }

    fn plane_transposed_display(name: &str, w: u32, h: u32) -> DrmDisplayInfo {
        DrmDisplayInfo {
            plane_transposed: true,
            ..drm_display(name, w, h)
        }
    }

    #[test]
    fn a_plane_transposed_display_advertises_its_scanout() {
        // gamescope 下的 Steam Deck：800x1280 的面板模式，1280x800 的帧缓冲，平面为 rotate-270，
        // 并且没有与该连接器匹配的 wayland 输出。
        let drm = [plane_transposed_display("eDP-1", 800, 1280)];
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![],
        };
        let assignment = assign_wayland_outputs(&drm, &wl.displays);
        let infos = augment_with_wayland_geometry_from(&drm, &wl, &assignment);
        assert_eq!((infos[0].width, infos[0].height), (1280, 800));
    }

    #[test]
    fn a_rotated_output_over_a_rotated_plane_is_swapped_once() {
        // mutter 旋转输出并据此对平面编程：四分之一圈，和之前一样。
        let drm = [plane_transposed_display("HDMI-A-1", 1920, 1080)];
        let mut out = wl_display("HDMI-1", 0, 0, 1920, 1080);
        out.transform = 90;
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![out],
        };
        let assignment = assign_wayland_outputs(&drm, &wl.displays);
        let infos = augment_with_wayland_geometry_from(&drm, &wl, &assignment);
        assert_eq!((infos[0].width, infos[0].height), (1080, 1920));
    }

    #[test]
    fn a_lone_rotated_output_advertises_delivered_dimensions() {
        // 对只看原点那种情况的修正：一个连接器，一个旋转的输出。采集器会
        // 交付旋转过的帧，因此即使在看原点这种情况下，公布的尺寸也必须交换，
        // 而逻辑缩放仍然不被采纳（保持 1.0）。
        let drm = [drm_display("HDMI-A-1", 1920, 1080)];
        let mut out = wl_display("HDMI-1", 0, 0, 1920, 1080);
        out.transform = 90;
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![out],
        };
        let assignment = assign_wayland_outputs(&drm, &wl.displays);
        let infos = augment_with_wayland_geometry_from(&drm, &wl, &assignment);
        assert_eq!((infos[0].width, infos[0].height), (1080, 1920));
        assert_eq!(infos[0].scale, 1.0);
    }

    #[test]
    fn transform_and_origin_come_from_the_same_snapshot() {
        // 两者都取自同一份 Displays 快照：旋转输出的变换和它的原点
        // 必须属于同一次分配，而「多连接器单输出」守卫会把
        // 两者都清零，而不是把猜测的原点与真实的变换混在一起。
        let drm = [
            drm_display("HDMI-A-1", 1920, 1080),
            drm_display("DP-1", 2560, 1440),
        ];
        let mut rotated = wl_display("DP-1", 1920, 0, 2560, 1440);
        rotated.transform = 270;
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![rotated, wl_display("HDMI-1", 0, 0, 1920, 1080)],
        };
        let (t, origin) = transform_and_origin(&drm, 1, &wl);
        assert_eq!(t, 270);
        assert_eq!(origin, Some((1920, 0)));
        let lone = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![wl_display("HDMI-1", 0, 0, 1920, 1080)],
        };
        assert_eq!(transform_and_origin(&drm, 1, &lone), (0, None));
    }

    #[test]
    fn one_connector_assignment_drives_geometry_and_primary() {
        let drm = [
            drm_display("HDMI-A-1", 1920, 1080),
            drm_display("DP-1", 2560, 1440),
        ];
        let wl = scrap::wayland::display::Displays {
            primary: 0,
            displays: vec![
                wl_display("DP-1", 1920, 0, 2560, 1440),
                wl_display("HDMI-1", 0, 0, 1920, 1080),
            ],
        };

        let assignment = assign_wayland_outputs(&drm, &wl.displays);
        let infos = augment_with_wayland_geometry_from(&drm, &wl, &assignment);
        assert_eq!((infos[0].x, infos[1].x), (0, 1920));
        assert_eq!(primary_index_from_assignment(&assignment, wl.primary), 1);
    }

    #[test]
    fn a_resolution_guess_never_steals_an_exact_name_match() {
        // 评审中的场景：较早的一个名称无法匹配的连接器，与较晚连接器的
        // 精确名称匹配共享同一个分辨率。名称优先在全局范围内预留。
        let drm = vec![
            drm_display("DSI-1", 1920, 1080),
            drm_display("HDMI-A-1", 1920, 1080),
        ];
        let wl = vec![
            wl_display("HDMI-1", 0, 0, 1920, 1080),
            wl_display("Unknown-9", 1920, 0, 2560, 1440),
        ];
        let m = identity_matches(&drm, &wl);
        assert_eq!(m[1], Some(0), "the exact name match must win globally");
        assert_eq!(m[0], None, "the leftover pairing is not forced, so no identity");
        // 唯一空闲分辨率上有两个未匹配的连接器：在 DRM 那边同样是歧义的，
        // 所以旋转不能钉在其中任何一个上。
        let drm2 = vec![
            drm_display("DSI-1", 1920, 1080),
            drm_display("DSI-2", 1920, 1080),
        ];
        let wl2 = vec![wl_display("HDMI-1", 0, 0, 1920, 1080)];
        let m2 = identity_matches(&drm2, &wl2);
        assert!(m2[0].is_none() && m2[1].is_none());
    }

    // M10 的情况：型号和分辨率都相同，名称无法归一化成合成器的那种。
    #[test]
    fn identical_monitors_that_match_no_name_take_layout_order() {
        let drm = [drm_display("DP-1", 1920, 1080), drm_display("DP-2", 1920, 1080)];
        let wl = [
            wl_display("Unknown-1", 0, 0, 1920, 1080),
            wl_display("Unknown-2", 1920, 0, 1920, 1080),
        ];
        assert_eq!(assign_wayland_outputs(&drm, &wl), vec![Some(0), Some(1)]);
    }

    #[test]
    fn health_reports_demoted_only_while_the_cooldown_runs() {
        let mut h = DisplayHealth::new();
        assert!(!h.demoted(), "a fresh display is not demoted");
        h.zero_frame_streak = DRM_GRAB_MAX_FAILURES - 1;
        assert!(!h.demoted(), "one session short of the threshold is not demoted");
        h.zero_frame_streak = DRM_GRAB_MAX_FAILURES;
        h.demotes = 1;
        assert!(h.demoted(), "at the threshold, inside the cooldown");
        h.since = Instant::now() - demote_cooldown(h.demotes) - Duration::from_secs(1);
        assert!(!h.demoted(), "past the cooldown the display must be retried");
        h.demotes = 4;
        assert!(h.demoted(), "the backoff must still be holding it at demotion 4");
    }

    // ---------------------------------------------------------------------------------------
    // rustdesk#16242 第二轮：推断模型背后的那次测量。
    //
    // 合成夹具无法回答的问题：当驱动没有发布热点、且
    // 在猜测之前必须先把精灵图转正时，每条流程落点距离主题真实的点击
    // 点有多远？这里按形状、按角度驱动两条流程，对照
    // 主题作者写进光标文件里的热点。它读取已安装的 Adwaita
    // 主题，因此它只打印而不做断言，默认被忽略：
    //
    //   cargo test --features drm --lib drm_capturer_tests::adwaita -- --ignored --nocapture
    // ---------------------------------------------------------------------------------------

    /// 极简的 Xcursor 读取器。该格式是 16 字节的头部、一个由 12 字节条目构成的目录，
    /// 以及每个（标称尺寸、动画帧）一个数据块；图像块是 36 字节的头部，
    /// 后面跟着 width*height 个小端 ARGB 字，因此每个像素的第 3 字节就是
    /// `infer_hotspot` 读取的 alpha。每个标称尺寸只取第一帧。
    fn xcursor_images(path: &std::path::Path) -> Vec<(u32, usize, usize, i32, i32, Vec<u8>)> {
        let b = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        let u32at = |o: usize| -> u32 {
            if o + 4 > b.len() {
                0
            } else {
                u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
            }
        };
        if b.len() < 16 || &b[0..4] != b"Xcur" {
            return Vec::new();
        }
        let (hdr, ntoc) = (u32at(4) as usize, u32at(12) as usize);
        let mut out: Vec<(u32, usize, usize, i32, i32, Vec<u8>)> = Vec::new();
        for i in 0..ntoc {
            let e = hdr + i * 12;
            if u32at(e) != 0xfffd_0002 {
                continue;
            }
            let nominal = u32at(e + 4);
            if out.iter().any(|(n, ..)| *n == nominal) {
                continue;
            }
            let p = u32at(e + 8) as usize;
            let (w, h) = (u32at(p + 16) as usize, u32at(p + 20) as usize);
            let (xhot, yhot) = (u32at(p + 24) as i32, u32at(p + 28) as i32);
            let px = p + 36;
            if w == 0 || h == 0 || px.saturating_add(w * h * 4) > b.len() {
                continue;
            }
            out.push((nominal, w, h, xhot, yhot, b[px..px + w * h * 4].to_vec()));
        }
        out
    }

    fn dist(a: (i32, i32), b: (i32, i32)) -> f64 {
        let (dx, dy) = ((a.0 - b.0) as f64, (a.1 - b.1) as f64);
        (dx * dx + dy * dy).sqrt()
    }

    #[test]
    #[ignore]
    fn adwaita_hotspot_survey() {
        let dir = std::env::var("SURVEY_DIR")
            .unwrap_or_else(|_| "/usr/share/icons/Adwaita/cursors".to_owned());
        let dir = std::path::Path::new(&dir);
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("no installed Adwaita cursors to measure")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let want: u32 = std::env::var("SURVEY_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        let mut display = 20_000;
        println!("SURVEY size={want}");
        println!("shape,size,w,h,truth_x,truth_y,box,t,sent_x,sent_y,base_x,base_y,base_err,head_x,head_y,head_err,master_x,master_y,master_err");
        for name in &names {
            let imgs = xcursor_images(&dir.join(name));
            let Some((n, w, h, tx, ty, up)) = imgs.into_iter().find(|(n, ..)| *n == want) else {
                println!("# {name}: no {want} px image");
                continue;
            };
            // 不透明区域，用来在之后区分各个形状。
            let (mut minx, mut miny, mut maxx, mut maxy) = (w as i32, h as i32, -1i32, -1i32);
            for (i, px) in up.chunks_exact(4).take(w * h).enumerate() {
                if px[3] >= 128 {
                    let (x, y) = ((i % w) as i32, (i / w) as i32);
                    minx = minx.min(x);
                    maxx = maxx.max(x);
                    miny = miny.min(y);
                    maxy = maxy.max(y);
                }
            }
            let bx = format!("{}x{}+{}+{}", maxx - minx + 1, maxy - miny + 1, minx, miny);
            for t in [0, 90, 180, 270] {
                let (scan, sw, sh) = as_scanned_out(&up, w, h, t);
                // 生产者放到线路上的内容：在平面持有该精灵图的状态下猜出的值，
                // 其中 hot_measured 为 false。
                let sent = scrap::drm_reader::infer_hotspot(&scan, sw, sh);
                // 本 PR 之前的流程：把那个点当作像素来反向映射。
                let base = unrotate_hotspot(t, sw as i32, sh as i32, sent.0, sent.1);
                // 本 PR 中的流程，从真实的交付路径读回。
                display += 1;
                deliver_drm_cursor(display, 1, 1, sw as u32, sh as u32, sent.0, sent.1, false, scan, t);
                let head = {
                    let map = DRM_CURSOR.lock().unwrap();
                    let (_, c) = map.get(&display).expect("nothing published");
                    (c.hotx, c.hoty)
                };
                let m = master_delivered(&up, w, h, t);
                println!(
                    "{name},{n},{w},{h},{tx},{ty},{bx},{t},{},{},{},{},{:.2},{},{},{:.2},{},{},{:.2}",
                    sent.0, sent.1, base.0, base.1, dist(base, (tx, ty)),
                    head.0, head.1, dist(head, (tx, ty)),
                    m.0, m.1, dist(m, (tx, ty))
                );
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // rustdesk#16242 第二轮：对照主题声明的热点测出的猜测。
    //
    // 上面每个测试都构建自己的精灵图，因此它们全都是拿猜测和猜测作比较。
    // 这样可以固定一种映射，但无法说明猜测到底好不好。这个测试读取
    // 光标主题中的预期值——主题作者写进
    // 文件里的点击点，而这正是合成器在有该属性时会编程进 HOTSPOT_X/Y 的内容
    // 。
    // -----------------------------------------------------------------------------------------

    include!("drm_cursor_theme.rs");

    /// 把夹具里打包的 alpha 掩码展开成交付路径所使用的精灵图。只设置
    /// alpha：猜测不读其他任何内容。
    fn theme_sprite(mask_hex: &str, w: usize, h: usize) -> Vec<u8> {
        let bytes: Vec<u8> = (0..mask_hex.len() / 2)
            .map(|i| u8::from_str_radix(&mask_hex[i * 2..i * 2 + 2], 16).expect("fixture hex"))
            .collect();
        let mut px = vec![0u8; w * h * 4];
        for i in 0..w * h {
            if bytes[i / 8] & (0x80 >> (i % 8)) != 0 {
                px[i * 4 + 3] = 255;
            }
        }
        px
    }

    /// master 发布的那个猜测，已冻结。只有一个高瘦的区域在那边居中：对称的宽高
    /// 比测试是后来在本 PR 中加入的，所以改为冻结那个的话，就等于让本 PR 拿自己
    /// 之前的修订版作比较，而不是拿已合并的版本作比较。
    fn master_guess(rgba: &[u8], w: usize, h: usize) -> (i32, i32) {
        let (mut minx, mut miny, mut maxx, mut maxy) = (w as i32, h as i32, -1i32, -1i32);
        for (i, px) in rgba.chunks_exact(4).take(w * h).enumerate() {
            if px[3] >= 128 {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
            }
        }
        if maxx < minx || maxy < miny {
            return (0, 0);
        }
        let (bw, bh) = (maxx - minx + 1, maxy - miny + 1);
        if bh > bw * 2 {
            ((minx + maxx) / 2, (miny + maxy) / 2)
        } else {
            (minx, miny)
        }
    }

    /// 对于旋转了 `t` 的输出上的正立精灵图，master 交付的内容：它在
    /// 平面持有该精灵图的状态下猜测，然后把那个点当作像素来映射，而且只
    /// 在 90 和 270 度时这样做——0 和 180 原样通过。
    fn master_delivered(up: &[u8], w: usize, h: usize, t: i32) -> (i32, i32) {
        let (scan, sw, sh) = as_scanned_out(up, w, h, t);
        let (gx, gy) = master_guess(&scan, sw, sh);
        if t == 90 || t == 270 {
            unrotate_hotspot(t, sw as i32, sh as i32, gx, gy)
        } else {
            (gx, gy)
        }
    }

    fn away(a: (i32, i32), b: (i32, i32)) -> f64 {
        (((a.0 - b.0) as f64).powi(2) + ((a.1 - b.1) as f64).powi(2)).sqrt()
    }

    /// 交付的热点距离主题声明的点击点有多远，按形状，
    /// 与 MASTER 对照测量，而不是与本 PR 之前的修订版对照。
    ///
    /// master 对光标处理 0、90 和 270（在那边 180 原样通过），因此这个
    /// 比较只在这三个角度上进行，也就是 master 实际覆盖的那一组。
    ///
    /// 有两个形状比 master 偏得更远，它们被点名指出，而不是被平均掉。
    /// `help` 是任何位图规则都无法逾越的诚实边界：主题把它的点击点放在问号的
    /// 那个圆点上，而这一点无法从 alpha 还原。`alias` 是 0.51 像素。其他所有
    /// 都与 master 持平或更好，而且整个语料库上的平均值从 12.08 像素降到 4.20。
    #[test]
    fn no_shape_lands_further_from_the_theme_than_master_except_the_two_no_bitmap_can_reach() {
        const NOT_INFERABLE: [&str; 2] = ["help", "alias"];
        let (mut sum_master, mut sum_now) = (0f64, 0f64);
        let mut regressed = Vec::new();
        for &(name, w, h, hx, hy, mask) in THEME_SHAPES {
            let (w, h) = (w as usize, h as usize);
            let up = theme_sprite(mask, w, h);
            let truth = (hx, hy);
            let angles = [0, 90, 270];
            let mut e_master = 0f64;
            let mut e_now = 0f64;
            for t in angles {
                e_master += away(master_delivered(&up, w, h, t), truth);
                // 正立的推断按构造在每个角度上答案都相同。
                e_now += away(scrap::drm_reader::infer_hotspot(&up, w, h), truth);
            }
            let (e_master, e_now) = (e_master / angles.len() as f64, e_now / angles.len() as f64);
            sum_master += e_master;
            sum_now += e_now;
            if e_now > e_master + 0.005 && !NOT_INFERABLE.contains(&name) {
                regressed.push((name, e_master, e_now));
            }
        }
        let n = THEME_SHAPES.len() as f64;
        assert!(
            regressed.is_empty(),
            "further from the theme hotspot than master, averaged over 0/90/270: {regressed:?}"
        );
        assert!(
            sum_now / n <= 4.5 && sum_master / n >= 11.0,
            "mean error over {n} shapes: master {:.2}, this pr {:.2} (measured 12.08 and 4.20)",
            sum_master / n,
            sum_now / n
        );
    }
}
