use super::*;
use crate::common::SimpleCallOnReturn;
#[cfg(target_os = "linux")]
use crate::platform::linux::is_x11;
#[cfg(windows)]
use crate::virtual_display_manager;
#[cfg(windows)]
use hbb_common::get_version_number;
use hbb_common::protobuf::MessageField;
use scrap::Display;
use std::sync::atomic::{AtomicBool, Ordering};

// https://github.com/rustdesk/rustdesk/discussions/6042, 避免 dbus 调用

pub const NAME: &'static str = "display";

#[cfg(windows)]
const DUMMY_DISPLAY_SIDE_MAX_SIZE: usize = 1024;

struct ChangedResolution {
    original: (i32, i32),
    changed: (i32, i32),
}

lazy_static::lazy_static! {
    static ref IS_CAPTURER_MAGNIFIER_SUPPORTED: bool = is_capturer_mag_supported();
    static ref CHANGED_RESOLUTIONS: Arc<RwLock<HashMap<String, ChangedResolution>>> = Default::default();
    static ref SYNC_DISPLAYS: Arc<Mutex<SyncDisplaysInfo>> = Default::default();
}

#[cfg(target_os = "linux")]
lazy_static::lazy_static! {
    static ref WAYLAND_UINPUT_RECT: Mutex<WaylandUinputRect> = Default::default();
    static ref WAYLAND_LAYOUT: Mutex<WaylandLayout> = Default::default();
}

#[cfg(target_os = "linux")]
const WAYLAND_LAYOUT_CHECK_INTERVAL: Duration = Duration::from_millis(1500);

#[cfg(target_os = "linux")]
#[derive(Default)]
struct WaylandUinputRect {
    rect: Option<(i32, i32, i32, i32)>,
    last_check: Option<std::time::Instant>,
}

// 按显示器的布局，用于在合成器于会话中途移动某台
// 显示器时修正注入的坐标。客户端持续发送按会话初始化时告知它的布局
// 偏移过的坐标（`baseline`）；我们把它们重新映射到当前布局（`live`）。
// https://github.com/rustdesk/rustdesk/issues/15601
#[cfg(target_os = "linux")]
#[derive(Default)]
struct WaylandLayout {
    baseline: Vec<scrap::wayland::display::DisplayRect>,
    live: Vec<scrap::wayland::display::DisplayRect>,
    // 当前的采集器是按什么布局构建的。与 `baseline` 分开保存，因为会话
    // 初始化会重置那一个，而代际检测器需要一份重置也抹不掉的记忆：
    // 否则跨越一次旋转的两次初始化就什么都不剩可比较。
    seen: Vec<scrap::wayland::display::DisplayRect>,
    // 某个采集器记录的构建布局与 `seen` 不同，并带上它构建时的
    // 代际来标记：轮询在该采集器读取快照与其记录之间观察到了当前布局，
    // 两者必有一个是过期的，因此下一次轮询无论看到什么都欠着一个边界。只有
    // 在该代际仍然有效时才是如此：记录也可能落在轮询消费某个边界
    // 与它所提升的代际递增之间（或递增之后，用的是递增之前的快照），而该
    // 采集器会自行重建，因此第二次提升会把刚建好的那些再次拆掉。
    // 由 `observe` 消费，轮询在 `edge` 之后紧接着运行它；会话初始化时的 baseline
    // 重置不会影响它。
    unseen_build: Option<u64>,
}

#[cfg(target_os = "linux")]
impl WaylandLayout {
    // 替换按会话保存的输入 baseline。第一次轮询之前，原有的 baseline 是
    // 唯一记录采集器构建时所依据布局的东西，因此它用于初始化 `seen`。
    fn reset_baseline(&mut self, baseline: Vec<scrap::wayland::display::DisplayRect>) {
        if self.seen.is_empty() {
            let previous = std::mem::take(&mut self.baseline);
            self.seen = previous;
        }
        self.baseline = baseline;
        self.live.clear();
    }

    // 这是一个边沿（当前布局与采集器构建时所依据的布局之差），不是电平：拿
    // 它与 baseline 比较会把结果在整个会话期间锁存为真。还没观察到任何东西时，
    // baseline 就是那份记录，而初始化时缺失快照会让第一次成功成为那个边沿，
    // 或者 transform=0 一直保留。
    fn edge(
        &self,
        live: &[scrap::wayland::display::DisplayRect],
        snapshot_missing: bool,
        generation: u64,
    ) -> bool {
        if self.unseen_build == Some(generation) {
            return true;
        }
        if !self.seen.is_empty() {
            return self.seen != live;
        }
        if self.baseline.is_empty() {
            return snapshot_missing;
        }
        self.baseline != live
    }

    fn observe(&mut self, live: &[scrap::wayland::display::DisplayRect]) {
        self.live = live.to_vec();
        self.seen = live.to_vec();
        self.unseen_build = None;
    }

    // 采集器构建时所依据的布局，在没有其他来源时用于初始化这份记忆。会话
    // 初始化时 wayland 查询失败会留下一个空的 baseline，而采集器自身的重试可能
    // 随后成功——于是只有采集器知道自己正在显示的布局，而
    // 没有这份记录，首次轮询前的旋转对 `edge` 就是不可见的。只在为空时才写：
    // 后来才构建的采集器不能覆盖轮询正在保存的记忆，因为在
    // 多显示器会话中，这份记忆正是其他采集器构建时所依据的布局。若某次构建
    // 与它不一致，就会被标记出来：采集器读取快照与写入这份记录
    // 是两个步骤，落在两者之间的轮询会先观察到当前布局，这
    // 否则会丢掉这份记录，让采集器停留在一个无人比较的变换上。
    fn note_capturer(&mut self, built_on: &[scrap::wayland::display::DisplayRect], built_gen: u64) {
        if built_on.is_empty() {
            return;
        }
        if self.seen.is_empty() {
            self.seen = built_on.to_vec();
        } else if self.seen != built_on {
            // 以最新的代际为准：迟到的过期记录不能掩盖新鲜的那份。
            self.unseen_build = Some(self.unseen_build.map_or(built_gen, |g| g.max(built_gen)));
        }
    }
}

// `live` 是否与 `baseline` 不同。每次鼠标移动都会读取，所以它是一个原子量：
// 常见（无漂移）的情况完全不会碰布局互斥锁。
#[cfg(target_os = "linux")]
static WAYLAND_LAYOUT_DRIFTED: AtomicBool = AtomicBool::new(false);

/// 当布局已偏离会话初始化时的 baseline 且对应的 uinput
/// 范围已应用时为真，这正是 `remap_wayland_uinput_coord` 进行重映射的条件。在
/// 它成立期间，注入的点已被移动到当前布局上，不再位于
/// DRM 光标校准当初用来确定几何的布局中，因此校准会拒绝
/// 测量。它是次级保护：阻止测量的是快照代际，
/// 而在布局移动后它就失效了；这一层覆盖的是重映射生效、而
/// 重新设定 baseline 的提升尚未完成的那段窗口。两者都由下面的轮询写入，因此布局
/// 变更最多会晚一个检查间隔才被发现；在该窗口内，测量中位图的边界是
/// 唯一的保护，而通过该保护的值会被下一对确认的
/// 值替换掉。范围应用失败会记录 false，此后也不会有任何重映射。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) fn wayland_layout_drifted() -> bool {
    WAYLAND_LAYOUT_DRIFTED.load(Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
pub(super) fn set_wayland_uinput_rect(rect: (i32, i32, i32, i32)) {
    WAYLAND_UINPUT_RECT.lock().unwrap().rect = Some(rect);
}

/// 设备当前运行的 uinput 范围所属的布局代际，以及在应用开始和
/// 设备确认时递增的计数。DRM 光标校准只在自己的代际下、
/// 并针对当前计数下注入的样本来测量：过期的范围会把点映射到
/// 别处，而两次停止共用这个误差，因此一致性无法把它排除掉。
#[cfg(all(target_os = "linux", feature = "drm"))]
static INPUT_MAP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
#[cfg(all(target_os = "linux", feature = "drm"))]
static INPUT_MAP_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 在 `input_map_epoch` 之前读取：看到一个代际就意味着看到了它的采纳所提升的计数。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) fn input_map_gen() -> u64 {
    INPUT_MAP_GEN.load(Ordering::Acquire)
}
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) fn input_map_epoch() -> u64 {
    INPUT_MAP_EPOCH.load(Ordering::Relaxed)
}
/// 设备已确认代际 `gen` 的范围（在会话初始化路径上，是在计算出该矩形之前读取的
/// 那一个，可能早于轮询尚未看到的移动）。在此之前
/// 注入的样本是按上一个范围映射的；计数先变。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn note_input_map_adopted(gen: u64) {
    INPUT_MAP_EPOCH.fetch_add(1, Ordering::Relaxed);
    INPUT_MAP_GEN.store(gen, Ordering::Release);
}
/// 设备当前运行的范围与 `gen` 的布局相符。这不是一次采纳：在它之下
/// 注入的样本依然有效。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn note_input_map_ready(gen: u64) {
    INPUT_MAP_GEN.store(gen, Ordering::Release);
}
/// 一次应用正在开始：在设备确认之前没有就绪的范围，之前
/// 的样本不计入，已记录的矩形也被遗忘，因此失败或超时的应用（设备
/// 可能稍后才接受该范围）不会留下任何就绪项，下次检查会重新应用。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn note_input_map_unknown() {
    WAYLAND_UINPUT_RECT.lock().unwrap().rect = None;
    note_input_map_adopted(u64::MAX);
}
/// 布局轮询每次检查都会向应用线程要一个任务，因此一个无法启动的线程会
/// 让每一次都失败。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) const UINPUT_APPLY_LOG_INTERVAL: Duration = Duration::from_secs(600);
/// 在唯一的那个线程上运行 `job`，该线程负责在 DRM 路径上检查、应用并记录 uinput 范围，
/// 并且要排在它之前请求的每个任务之后。两次应用不能重叠：uinput 服务保留的是
/// 最后到达它的那个请求的范围，而被记录的必须是那一个。调用方等待
/// 结果时不持有任何锁；任务借用该线程的运行时来做 IPC。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn run_uinput_apply<R: Send + 'static>(
    job: impl FnOnce(&tokio::runtime::Runtime) -> R + Send + 'static,
) -> tokio::sync::oneshot::Receiver<R> {
    type Job = Box<dyn FnOnce(&tokio::runtime::Runtime) + Send>;
    static JOBS: Mutex<Option<std::sync::mpsc::Sender<Job>>> = Mutex::new(None);
    fn start() -> Option<std::sync::mpsc::Sender<Job>> {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("uinput-apply".to_owned())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        hbb_common::throttled_log!(
                            UINPUT_APPLY_LOG_INTERVAL,
                            error,
                            "uinput apply thread: failed to build a runtime: {err}"
                        );
                        return;
                    }
                };
                for job in rx {
                    job(&rt);
                }
            });
        match spawned {
            Ok(_) => Some(tx),
            Err(err) => {
                hbb_common::throttled_log!(
                    UINPUT_APPLY_LOG_INTERVAL,
                    error,
                    "failed to start the uinput apply thread: {err}"
                );
                None
            }
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job: Job = Box::new(move |rt| {
        let _ = tx.send(job(rt));
    });
    // 从未启动或已经消失的线程，会在某个任务发现它不见时重新启动，因此一次
    // 失败不会让之后所有的应用都停下。无法发送的任务，或者在线程消失时
    // 排队的任务，会连同它的 `tx` 一起被丢弃，调用方读到的是一个错误。
    let mut jobs = JOBS.lock().unwrap();
    let job = match jobs.as_ref() {
        Some(sender) => match sender.send(job) {
            Ok(()) => return rx,
            Err(std::sync::mpsc::SendError(job)) => job,
        },
        None => job,
    };
    * jobs = start();
    if let Some(tx) = jobs.as_ref() {
        let _ = tx.send(job);
    }
    rx
}
/// 记录范围时所使用的代际：原始的 DRM 合并布局没有，因为它不是合成器
/// 布局，任何流都不是基于它构建的。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn input_map_label(gen: u64, from_compositor: bool) -> u64 {
    if from_compositor {
        gen
    } else {
        u64::MAX
    }
}

// 当前写入设备的 uinput ABS 范围，供 DRM 路径的「仅在发生变化时」才重新应用的检查使用。
// PipeWire 路径则在 refresh_wayland_uinput_rect_if_changed 中直接比较它。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn wayland_uinput_rect() -> Option<(i32, i32, i32, i32)> {
    WAYLAND_UINPUT_RECT.lock().unwrap().rect
}

#[cfg(target_os = "linux")]
pub(super) fn set_wayland_layout_baseline(baseline: Vec<scrap::wayland::display::DisplayRect>) {
    WAYLAND_LAYOUT_DRIFTED.store(false, Ordering::Relaxed);
    WAYLAND_LAYOUT.lock().unwrap().reset_baseline(baseline);
}

/// 记录采集器刚刚构建时所依据的布局，以及它在获取该布局之前读到的
/// 快照代际。参见 `WaylandLayout::note_capturer`。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(super) fn note_capturer_layout(
    displays: &[base::platform::linux::WaylandDisplayInfo],
    built_gen: u64,
) {
    if displays.is_empty() {
        return;
    }
    let rects = scrap::wayland::display::logical_rects_of_displays(displays);
    WAYLAND_LAYOUT
        .lock()
        .unwrap()
        .note_capturer(&rects, built_gen);
}

// 当注入坐标已偏离会话初始化时告知客户端的布局时，
// 把它重新映射到当前的合成器布局上；否则无锁空操作。
#[cfg(target_os = "linux")]
pub(super) fn remap_wayland_uinput_coord(x: i32, y: i32) -> (i32, i32) {
    if !WAYLAND_LAYOUT_DRIFTED.load(Ordering::Relaxed) {
        return (x, y);
    }
    let lock = WAYLAND_LAYOUT.lock().unwrap();
    scrap::wayland::display::remap_to_live_layout(x, y, &lock.baseline, &lock.live)
}

// uinput 绝对范围在会话初始化时设定。如果之后合成器布局
// 发生变化（显示器缩放/位置变化，或采集开始后出现的 portal 虚拟输出
// 出现时），注入的坐标会被过期的
// 范围重新缩放从而落到偏移的位置，https://github.com/rustdesk/rustdesk/issues/15601
#[cfg(target_os = "linux")]
fn refresh_wayland_uinput_rect_if_changed() {
    if is_x11() || !crate::input_service::wayland_use_uinput() {
        return;
    }
    {
        let mut lock = WAYLAND_UINPUT_RECT.lock().unwrap();
        if let Some(last_check) = lock.last_check {
            if last_check.elapsed() < WAYLAND_LAYOUT_CHECK_INTERVAL {
                return;
            }
        }
        lock.last_check = Some(std::time::Instant::now());
    }
    let Some((rect, live_rects)) = scrap::wayland::display::get_layout_for_uinput_live() else {
        return;
    };
    // 每次轮询都刷新按显示器的布局：显示器的原点可能移动（例如两块
    // 显示器交换位置）而整体桌面矩形不变，而鼠标
    // 路径需要当前各显示器的几何信息来修正坐标。
    let (live_changed, mut drifted) = {
        let mut layout = WAYLAND_LAYOUT.lock().unwrap();
        #[cfg(feature = "drm")]
        let snapshot_missing = scrap::wayland::display::wayland_snapshot_missing();
        #[cfg(not(feature = "drm"))]
        let snapshot_missing = false;
        #[cfg(feature = "drm")]
        let generation = scrap::wayland::display::wayland_snapshot_generation();
        #[cfg(not(feature = "drm"))]
        let generation = 0;
        let live_changed = layout.edge(&live_rects, snapshot_missing, generation);
        let drifted = !layout.baseline.is_empty()
            && !live_rects.is_empty()
            && layout.baseline != live_rects;
        layout.observe(&live_rects);
        (live_changed, drifted)
    };
    // 代际递增的唯一归属者：清缓存时若放任不管，会让每次会话初始化都拆掉
    // 其他所有存活的采集器。baseline 与清缓存一同提升（rustdesk#15601）。
    #[cfg(feature = "drm")]
    {
        // 在 DRM 暂时处于非 Available 状态时看到的边沿，会保持「欠账」状态而不被消费。
        static PROMOTION_OWED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        // 当采集器构建时没有 wayland 快照，这个闩锁就会触发：之后一次缓存
        // 重新填充会让 wayland_snapshot_missing 变成谎言，光靠 live_changed 会漏掉它。该标志
        // 无条件取：在 live_changed 的轮询中短路跳过它会让它一直置位，
        // 并在下一轮轮询时对刚重建的采集器多花一次多余的提升。
        let blind_build = super::drm_capturer::take_unrotated_snapshot_pending();
        if live_changed || blind_build {
            PROMOTION_OWED.store(true, Ordering::Release);
        }
        if PROMOTION_OWED.load(Ordering::Acquire) && super::drm_capturer::is_available_cached() {
            PROMOTION_OWED.store(false, Ordering::Release);
            scrap::wayland::display::clear_wayland_displays_cache();
            scrap::wayland::display::bump_layout_generation();
            set_wayland_layout_baseline(live_rects.clone());
            WAYLAND_LAYOUT.lock().unwrap().live = live_rects.clone();
            drifted = false;
        }
    }
    #[cfg(not(feature = "drm"))]
    let _ = live_changed;
    // 重映射修正的是各显示器原点的偏移；uinput ABS 范围修正的是
    // 整体包围盒。只有当范围与当前布局相符时才启用重映射，
    // 否则移动会被重映射到设备尚未使用的范围内。
    // 没有包围盒变化的漂移（原点互换）不需要更新范围，此时立即启用。
    // 检查、应用、它们记录的内容以及下面的标志都在 uinput 应用线程上运行，
    // 与其他路径交替执行；这个循环会等待它的结果。
    #[cfg(feature = "drm")]
    if run_uinput_apply(move |rt| {
        let mut range_ok = WAYLAND_UINPUT_RECT.lock().unwrap().rect == Some(rect);
        // 设备已经运行着与该布局相符的范围：DRM 校准可以进行测量。
        if range_ok {
            note_input_map_ready(scrap::wayland::display::wayland_snapshot_generation());
        }
        // 在登录界面时 DRM 路径掌管该矩形；这里只跳过范围/重映射的更新，
        // 上面的快照失效处理仍必须执行（欢迎界面会话没有其他触发途径）。
        if crate::platform::linux::is_login_screen_wayland_cached() {
            return;
        }
        if !range_ok {
            let (minx, maxx, miny, maxy) = rect;
            log::info!(
                "desktop layout changed, update mouse resolution: ({}, {}), ({}, {})",
                minx,
                maxx,
                miny,
                maxy
            );
            note_input_map_unknown();
            // 给 IPC 等待设定上限：`set_resolution()` 在读取响应时没有超时。
            match rt.block_on(async {
                timeout(
                    3_000,
                    crate::input_service::update_mouse_resolution(minx, maxx, miny, maxy),
                )
                .await
            }) {
                // 只在应用成功后才记录该矩形，这样瞬时的
                // 失败会在下次检查时重试。
                Ok(Ok(())) => {
                    WAYLAND_UINPUT_RECT.lock().unwrap().rect = Some(rect);
                    note_input_map_adopted(scrap::wayland::display::wayland_snapshot_generation());
                    range_ok = true;
                }
                Ok(Err(err)) => log::error!("Failed to update mouse resolution: {}", err),
                Err(err) => log::error!("Failed to update mouse resolution: {}", err),
            }
        }
        // 最后才发布该标志：读到 `true` 时一定对应着当前的 `live` 和一个
        // 相符的 uinput 范围。范围应用失败会让它保持 false，并在下次轮询时重试。
        WAYLAND_LAYOUT_DRIFTED.store(drifted && range_ok, Ordering::Relaxed);
    })
    .blocking_recv()
    .is_err()
    {
        hbb_common::throttled_log!(
            UINPUT_APPLY_LOG_INTERVAL,
            error,
            "Failed to check the uinput range: the uinput apply thread is gone"
        );
    }
    #[cfg(not(feature = "drm"))]
    {
        let mut range_ok = WAYLAND_UINPUT_RECT.lock().unwrap().rect == Some(rect);
        if !range_ok {
            let (minx, maxx, miny, maxy) = rect;
            log::info!(
                "desktop layout changed, update mouse resolution: ({}, {}), ({}, {})",
                minx,
                maxx,
                miny,
                maxy
            );
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    // 给 IPC 等待设定上限，这段代码运行在显示服务循环上，而
                    // `set_resolution()` 在读取响应时没有超时。
                    // 超时必须在 runtime 内部构造，否则会 panic
                    // 并报 "there is no reactor running"。参见 clipboard_service.rs。
                    match rt.block_on(async {
                        timeout(
                            3_000,
                            crate::input_service::update_mouse_resolution(minx, maxx, miny, maxy),
                        )
                        .await
                    }) {
                        // 只在应用成功后才记录该矩形，这样瞬时的
                        // 失败会在下次检查时重试。
                        Ok(Ok(())) => {
                            WAYLAND_UINPUT_RECT.lock().unwrap().rect = Some(rect);
                            range_ok = true;
                        }
                        Ok(Err(err)) => log::error!("Failed to update mouse resolution: {}", err),
                        Err(err) => log::error!("Failed to update mouse resolution: {}", err),
                    }
                }
                Err(err) => {
                    log::error!("Failed to build tokio runtime: {}", err);
                }
            }
        }
        // 最后才发布该标志：读到 `true` 时一定对应着当前的 `live` 和一个
        // 相符的 uinput 范围。范围应用失败会让它保持 false，并在下次轮询时重试。
        WAYLAND_LAYOUT_DRIFTED.store(drifted && range_ok, Ordering::Relaxed);
    }
}

// https://github.com/rustdesk/rustdesk/pull/8537
static TEMP_IGNORE_DISPLAYS_CHANGED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct SyncDisplaysInfo {
    displays: Vec<DisplayInfo>,
    is_synced: bool,
}

impl SyncDisplaysInfo {
    fn check_changed(&mut self, displays: &[DisplayInfo]) {
        if self.displays.as_slice() == displays {
            return;
        }

        self.displays = displays.to_vec();
        if !TEMP_IGNORE_DISPLAYS_CHANGED.load(Ordering::Relaxed) {
            self.is_synced = false;
        }
    }

    fn get_update_sync_displays(&mut self) -> Option<Vec<DisplayInfo>> {
        if self.is_synced {
            return None;
        }
        self.is_synced = true;
        Some(self.displays.clone())
    }
}

pub fn temp_ignore_displays_changed() -> SimpleCallOnReturn {
    TEMP_IGNORE_DISPLAYS_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
    SimpleCallOnReturn {
        b: true,
        f: Box::new(move || {
            // 等待一小段时间，确保 check_display_changed() 被调用
            // 是在视频服务发出它的 `SwitchDisplay` 消息之后（`try_broadcast_display_changed()`）。
            std::thread::sleep(Duration::from_millis(1000));
            TEMP_IGNORE_DISPLAYS_CHANGED.store(false, Ordering::Relaxed);
            // 触发显示器变更消息。
            SYNC_DISPLAYS.lock().unwrap().is_synced = false;
        }),
    }
}

// 这个函数非常有用，尽管它是对「显示器是否变更」的重复检查。
// 视频服务器随后会向客户端发送以下消息：
//  1. 第 {idx} 个显示器支持的分辨率
//  2. 切换分辨率的消息，这样客户端可以记录自定义分辨率。
pub(super) fn check_display_changed(
    ndisplay: usize,
    idx: usize,
    (x, y, w, h): (i32, i32, usize, usize),
) -> Option<DisplayInfo> {
    #[cfg(target_os = "linux")]
    {
        // wayland 目前不支持切换显示器
        if !is_x11() {
            return None;
        }
    }

    let lock = SYNC_DISPLAYS.lock().unwrap();
    // 当拔掉一台显示器且 lock.displays.get(idx) 为 None 时。
    //  1. 客户端版本 < 1.2.4。客户端必须重新连接。
    //  2. 客户端版本 > 1.2.4，客户端可以自行处理这种情况，因为会发送同步对端信息消息。
    // 但让用户手动重连是可以接受的，毕竟显示器已经被拔掉了。
    let d = lock.displays.get(idx)?;
    if ndisplay != lock.displays.len() {
        return Some(d.clone());
    }
    if !(d.x == x && d.y == y && d.width == w as i32 && d.height == h as i32) {
        Some(d.clone())
    } else {
        None
    }
}

#[inline]
pub fn set_last_changed_resolution(display_name: &str, original: (i32, i32), changed: (i32, i32)) {
    let mut lock = CHANGED_RESOLUTIONS.write().unwrap();
    match lock.get_mut(display_name) {
        Some(res) => res.changed = changed,
        None => {
            lock.insert(
                display_name.to_owned(),
                ChangedResolution { original, changed },
            );
        }
    }
}

#[inline]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn restore_resolutions() {
    for (name, res) in CHANGED_RESOLUTIONS.read().unwrap().iter() {
        let (w, h) = res.original;
        log::info!("Restore resolution of display '{}' to ({}, {})", name, w, h);
        if let Err(e) = crate::platform::change_resolution(name, w as _, h as _) {
            log::error!(
                "Failed to restore resolution of display '{}' to ({},{}): {}",
                name,
                w,
                h,
                e
            );
        }
    }
    // 可以清空，因为没有客户端连接时会调用 restore resolutions。
    CHANGED_RESOLUTIONS.write().unwrap().clear();
}

#[inline]
fn is_capturer_mag_supported() -> bool {
    #[cfg(windows)]
    return scrap::CapturerMag::is_supported();
    #[cfg(not(windows))]
    false
}

#[inline]
pub fn capture_cursor_embedded() -> bool {
    scrap::is_cursor_embedded()
}

#[inline]
#[cfg(windows)]
pub fn is_privacy_mode_mag_supported() -> bool {
    return *IS_CAPTURER_MAGNIFIER_SUPPORTED
        && get_version_number(&crate::VERSION) > get_version_number("1.1.9");
}

pub fn new() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME.to_owned(), true);
    GenericService::run(&svc.clone(), run);
    svc.sp
}

fn displays_to_msg(displays: Vec<DisplayInfo>) -> Message {
    let mut pi = PeerInfo {
        ..Default::default()
    };
    pi.displays = displays.clone();

    #[cfg(windows)]
    if crate::platform::is_installed() {
        let m = crate::virtual_display_manager::get_platform_additions();
        pi.platform_additions = serde_json::to_string(&m).unwrap_or_default();
    }

    // current_display 不应该在服务端使用。
    // 为了兼容旧客户端，它被设为 0。
    pi.current_display = 0;
    let mut msg_out = Message::new();
    msg_out.set_peer_info(pi);
    msg_out
}

fn check_get_displays_changed_msg() -> Option<Message> {
    #[cfg(target_os = "linux")]
    {
        if !is_x11() {
            // 在 DRM/KMS 采集路径上，PipeWire 枚举（正是它为
            // 通过 `check_update_displays` 提供 `SYNC_DISPLAYS` 的数据）被绕过了，因此在这里填充同步列表
            // ，数据取自 DRM 显示器列表。否则显示服务会广播一个空的
            // 列表，覆盖登录时的对端信息显示器，客户端就会显示 "No displays"。
            #[cfg(feature = "drm")]
            if super::drm_capturer::is_available_cached() {
                let synced = !SYNC_DISPLAYS.lock().unwrap().displays.is_empty();
                let stamped_before = scrap::wayland::display::wayland_failure_stamped();
                // 在还没有发布任何内容时，即便是未经增强的 DRM 列表也胜过下面那个空的
                // 广播；在有同步布局的情况下，被抑制的那一轮会保留它。
                if !synced || !scrap::wayland::display::wayland_lookup_suppressed() {
                    if let Some(displays) = super::drm_capturer::get_display_infos() {
                        // 第一次失败会让同步布局保留一个退避周期；只有
                        // 持续跨越一个退避周期的失败才会用 DRM 那一套替换它。
                        if !synced
                            || stamped_before
                            || !scrap::wayland::display::wayland_lookup_suppressed()
                        {
                            SYNC_DISPLAYS.lock().unwrap().check_changed(&displays);
                        }
                    }
                }
            }
            return get_displays_msg();
        }
    }
    check_update_displays(&try_get_displays().ok()?);
    get_displays_msg()
}

pub fn check_displays_changed() -> ResultType<()> {
    #[cfg(target_os = "linux")]
    {
        // 目前 wayland 需要在调用 Display::all() 之前先调用 wayland::clear()，否则会导致
        // 阻塞，甚至在这里崩溃，https://github.com/rustdesk/rustdesk/blob/0bb4d43e9ea9d9dfb9c46c8d27d1a97cd0ad6bea/libs/scrap/src/wayland/pipewire.rs#L235
        if !is_x11() {
            return Ok(());
        }
    }
    check_update_displays(&try_get_displays()?);
    Ok(())
}

fn get_displays_msg() -> Option<Message> {
    let displays = SYNC_DISPLAYS.lock().unwrap().get_update_sync_displays()?;
    Some(displays_to_msg(displays))
}

fn run(sp: EmptyExtraFieldService) -> ResultType<()> {
    while sp.ok() {
        sp.snapshot(|sps| {
            if !TEMP_IGNORE_DISPLAYS_CHANGED.load(Ordering::Relaxed) {
                if sps.has_subscribes() {
                    SYNC_DISPLAYS.lock().unwrap().is_synced = false;
                    bail!("new subscriber");
                }
            }
            Ok(())
        })?;

        if let Some(msg_out) = check_get_displays_changed_msg() {
            sp.send(msg_out);
            log::info!("Displays changed");
        }

        #[cfg(target_os = "linux")]
        if sp.has_subscribes() {
            refresh_wayland_uinput_rect_if_changed();
        }

        std::thread::sleep(Duration::from_millis(300));
    }

    Ok(())
}

#[inline]
pub(super) fn get_original_resolution(
    display_name: &str,
    w: usize,
    h: usize,
) -> MessageField<Resolution> {
    #[cfg(windows)]
    let is_rustdesk_virtual_display =
        crate::virtual_display_manager::rustdesk_idd::is_virtual_display(&display_name);
    #[cfg(not(windows))]
    let is_rustdesk_virtual_display = false;
    Some(if is_rustdesk_virtual_display {
        Resolution {
            width: 0,
            height: 0,
            ..Default::default()
        }
    } else {
        let changed_resolutions = CHANGED_RESOLUTIONS.write().unwrap();
        let (width, height) = match changed_resolutions.get(display_name) {
            Some(res) => {
                res.original
                /*
                The resolution change may not happen immediately, `changed` has been updated,
                but the actual resolution is old, it will be mistaken for a third-party change.
                if res.changed.0 != w as i32 || res.changed.1 != h as i32 {
                    // 如果分辨率被第三方进程改变，就移除 changed_resolutions 中的记录。
                    changed_resolutions.remove(display_name);
                    (w as _, h as _)
                } else {
                    res.original
                }
                */
            }
            None => (w as _, h as _),
        };
        Resolution {
            width,
            height,
            ..Default::default()
        }
    })
    .into()
}

pub(super) fn get_sync_displays() -> Vec<DisplayInfo> {
    SYNC_DISPLAYS.lock().unwrap().displays.clone()
}

pub(super) fn get_display_info(idx: usize) -> Option<DisplayInfo> {
    SYNC_DISPLAYS.lock().unwrap().displays.get(idx).cloned()
}

// 当至少有一个对外公布（已同步）的显示器不由 DRM/KMS 采集路径提供时为真，
// 即混合的 DRM + PipeWire 会话。光标服务（platform::linux::get_cursor /
// get_cursor_data）用它来判断隐藏的 DRM 硬件光标哨兵是否
// 具有权威性：在纯 DRM 会话中它是权威的（指针确实不在任何被采集的 CRTC 上），
// 但在混合会话中，这个哨兵只意味着指针移动到了一个由 PipeWire 提供的显示器上，
// 该显示器的光标必须走常规路径，而不是到处都被隐藏。
//
// 当 DRM 采集处于激活状态时，对外公布的列表是从 DRM 显示器列表枚举得到的，因此 DRM
// 列表比同步列表短，就意味着至少有一个对外公布的显示器由 PipeWire 提供。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub fn has_non_drm_backed_display() -> bool {
    match super::drm_capturer::display_count_and_any_demoted() {
        // 由 PipeWire 提供的显示器，要么不在 DRM 列表中（数量变少，例如
        // 纯 portal 的显示器），要么是 PRESENT-BUT-DEMOTED（保持在同一索引位置并被标记
        // 为离线，以保持索引空间对齐——参见 get_display_infos）。仅靠数量检查
        // 会漏掉降级这种情况（数量相同），因此被降级的显示器也被视为非 DRM 提供
        // 的显示器。正是这一点决定了隐藏光标哨兵的开关：它只在
        // 纯 DRM 会话中才具有权威性。这个标量访问器是有意为之：它在每个光标周期都会轮询，
        // 只要哨兵处于激活状态，而每个周期都克隆并做几何增强整个列表
        // （也就是 get_display_infos 所做的事）得到的是同样的两个事实。
        Some((count, any_demoted)) => {
            count < SYNC_DISPLAYS.lock().unwrap().displays.len() || any_demoted
        }
        None => false,
    }
}

// Display 转换为 DisplayInfo
// 该 DisplayInfo 会被发送给对端。
pub(super) fn check_update_displays(all: &Vec<Display>) {
    let _ = update_sync_displays(all);
}

/// 这个 seat 上是否有一个值得询问的合成器。`get_displays()` 不会缓存
/// 它的失败结果，因此在没有合成器的地方，它每次调用都会重新探测，得到的答案却
/// 无法改变任何调用方的结果。它位于 `&&` 链的最后，因此在轮询中绝不会最先执行。
#[inline]
#[cfg(target_os = "linux")]
fn wayland_has_compositor() -> bool {
    #[cfg(feature = "drm")]
    {
        !crate::platform::linux::is_login_screen_wayland_cached()
    }
    #[cfg(not(feature = "drm"))]
    {
        true
    }
}

// 在更新共享显示器缓存的同时返回转换后的输入快照。
pub(super) fn update_sync_displays(all: &Vec<Display>) -> Vec<DisplayInfo> {
    // 为了兼容：如果只有一个显示器，scale 保持 1.0，并对 `uinput` 使用物理尺寸。
    // 如果有多个显示器，就把 scale 设为 d.scale()，从而对 `uinput` 使用逻辑尺寸。
    #[cfg(target_os = "linux")]
    let use_logical_scale = !is_x11()
        && crate::is_server()
        && wayland_has_compositor()
        && scrap::wayland::display::get_displays().displays.len() > 1;
    let displays = all
        .iter()
        .map(|d| {
            let display_name = d.name();
            #[allow(unused_assignments)]
            #[allow(unused_mut)]
            let mut scale = 1.0;
            #[cfg(target_os = "macos")]
            {
                scale = d.scale();
            }
            #[cfg(target_os = "linux")]
            {
                if use_logical_scale {
                    scale = d.scale();
                }
            }
            let original_resolution = get_original_resolution(
                &display_name,
                ((d.width() as f64) / scale).round() as usize,
                (d.height() as f64 / scale).round() as usize,
            );
            DisplayInfo {
                x: d.origin().0 as _,
                y: d.origin().1 as _,
                width: d.width() as _,
                height: d.height() as _,
                name: display_name,
                online: d.is_online(),
                cursor_embedded: false,
                original_resolution,
                scale,
                ..Default::default()
            }
        })
        .collect::<Vec<DisplayInfo>>();
    SYNC_DISPLAYS.lock().unwrap().check_changed(&displays);
    displays
}

pub fn is_inited_msg() -> Option<Message> {
    #[cfg(target_os = "linux")]
    if !is_x11() {
        return super::wayland::is_inited();
    }
    None
}

// 返回刷新后的列表以及主显示器索引，这样登录时不会混用不同的显示器快照。
pub async fn update_get_sync_displays_on_login() -> ResultType<(Vec<DisplayInfo>, usize)> {
    #[cfg(target_os = "linux")]
    {
        if !is_x11() {
            let (displays, primary_display_idx) =
                super::wayland::get_displays_and_primary().await?;
            let primary_display_idx =
                normalize_primary_display_idx(primary_display_idx, displays.len());
            return Ok((displays, primary_display_idx));
        }
    }
    #[cfg(not(windows))]
    let displays = display_service::try_get_displays();
    #[cfg(windows)]
    let displays = display_service::try_get_displays_add_amyuni_headless();
    let displays = displays?;
    let primary_display_idx = get_primary_2(&displays);
    let sync_displays = update_sync_displays(&displays);
    let primary_display_idx =
        normalize_primary_display_idx(primary_display_idx, sync_displays.len());
    Ok((sync_displays, primary_display_idx))
}

#[inline]
fn normalize_primary_display_idx(primary_display_idx: usize, display_len: usize) -> usize {
    // 当列表为空或其主显示器索引已过期时，0 是协议层面的兜底值。
    if primary_display_idx < display_len {
        primary_display_idx
    } else {
        0
    }
}

#[inline]
pub fn get_primary_2(all: &Vec<Display>) -> usize {
    all.iter().position(|d| d.is_primary()).unwrap_or(0)
}

#[inline]
#[cfg(windows)]
fn no_displays(displays: &Vec<Display>) -> bool {
    let display_len = displays.len();
    if display_len == 0 {
        true
    } else if display_len == 1 {
        let display = &displays[0];
        if display.width() > DUMMY_DISPLAY_SIDE_MAX_SIZE
            || display.height() > DUMMY_DISPLAY_SIDE_MAX_SIZE
        {
            return false;
        }
        let any_real = crate::platform::resolutions(&display.name())
            .iter()
            .any(|r| {
                (r.height as usize) > DUMMY_DISPLAY_SIDE_MAX_SIZE
                    || (r.width as usize) > DUMMY_DISPLAY_SIDE_MAX_SIZE
            });
        !any_real
    } else {
        false
    }
}

#[inline]
#[cfg(not(windows))]
pub fn try_get_displays() -> ResultType<Vec<Display>> {
    Ok(Display::all()?)
}

#[inline]
#[cfg(windows)]
pub fn try_get_displays() -> ResultType<Vec<Display>> {
    try_get_displays_(false)
}

// 如果使用 amyuni idd，我们就无法完全控制这个虚拟显示器。
// 一旦添加了虚拟显示器，我们就无法自动将其移除。
// 因此在使用 amyuni idd 时，只有确有需要才会为无屏模式添加虚拟显示器。
// 例如客户端正在连接时。
#[inline]
#[cfg(windows)]
pub fn try_get_displays_add_amyuni_headless() -> ResultType<Vec<Display>> {
    try_get_displays_(true)
}

#[inline]
#[cfg(windows)]
pub fn try_get_displays_(add_amyuni_headless: bool) -> ResultType<Vec<Display>> {
    let mut displays = Display::all()?;

    // 如果未安装该平台或不支持虚拟显示器，就不要添加虚拟显示器。
    if !crate::platform::is_installed() || !virtual_display_manager::is_virtual_display_supported()
    {
        return Ok(displays);
    }

    // 在以下情况下启用无屏虚拟显示器：
    // 1. 没有使用 `amyuni` idd。
    // 2. 使用了 `amyuni` idd 且 `add_amyuni_headless` 为 true。
    if virtual_display_manager::is_amyuni_idd() && !add_amyuni_headless {
        return Ok(displays);
    }

    // 下面的代码会引发一个 bug。
    // 没有会话时无法添加虚拟显示器（例如从 RDP 退出时）。
    // 因为那时 `crate::platform::desktop_changed()` 总是返回 true。
    //
    // 这段代码只解决一种罕见情况：
    // 1. 控制端正在连接。
    // 2. windows 会话正在切换，没有检测到显示器，但它们其实存在。
    // 这时被控端会插入一个用于「无屏」的虚拟显示器。
    //
    // 无需做下面的检查。但代码保留在这里，用于标记该问题。
    // 如果有人报告这个问题，我们可以改成等待一段时间来做一个更好的检查。（会话切换中）。
    // 但在没有任何问题的情况下，我不认为加上超时检查是好事。
    //
    // 如果正在切换会话，可能检测不到显示器。
    // if displays.is_empty() && crate::platform::desktop_changed() {
    //     return Ok(displays);
    // }

    let no_displays_v = no_displays(&displays);
    if no_displays_v {
        log::debug!("no displays, create virtual display");
        if let Err(e) = virtual_display_manager::plug_in_headless() {
            log::error!("plug in headless failed {}", e);
        } else {
            displays = Display::all()?;
        }
    }
    Ok(displays)
}

#[cfg(all(test, target_os = "linux", feature = "drm"))]
mod input_map_tests {
    use super::run_uinput_apply;

    #[test]
    fn uinput_applies_run_one_at_a_time_in_the_order_asked() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let answers: Vec<_> = (0..4)
            .map(|i| {
                let log = log.clone();
                run_uinput_apply(move |_| {
                    log.lock().unwrap().push(("start", i));
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    log.lock().unwrap().push(("end", i));
                    i * 10
                })
            })
            .collect();
        let got: Vec<i32> = answers
            .into_iter()
            .map(|a| a.blocking_recv().unwrap())
            .collect();
        assert_eq!(got, vec![0, 10, 20, 30]);
        let want: Vec<_> = (0..4).flat_map(|i| [("start", i), ("end", i)]).collect();
        assert_eq!(*log.lock().unwrap(), want, "one job at a time, in order");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod wayland_layout_tests {
    use super::WaylandLayout;
    use scrap::wayland::display::DisplayRect;

    fn layout(w: i32, h: i32, transform: i32) -> Vec<DisplayRect> {
        vec![DisplayRect {
            name: "DP-1".into(),
            x: 0,
            y: 0,
            w,
            h,
            transform,
        }]
    }

    // rustdesk#15886：视频服务启动，输出发生旋转，并且在
    // 1.5 秒轮询之前就启动了重试。两处都会重置 baseline，因此它不可能是边沿检测器的记忆。
    #[test]
    fn a_rotation_between_two_session_inits_is_still_an_edge() {
        let upright = layout(1920, 1080, 0);
        let rotated = layout(1080, 1920, 1);
        let mut l = WaylandLayout::default();
        l.reset_baseline(upright.clone());
        l.observe(&upright);
        l.reset_baseline(upright.clone());
        l.reset_baseline(rotated.clone());
        assert!(l.edge(&rotated, false, 0));
    }

    // 对照：没有它，上面的断言在一个总是触发的检测器上也会通过。
    #[test]
    fn repeated_baseline_resets_without_a_rotation_are_not_an_edge() {
        let upright = layout(1920, 1080, 0);
        let mut l = WaylandLayout::default();
        l.reset_baseline(upright.clone());
        l.observe(&upright);
        l.reset_baseline(upright.clone());
        l.reset_baseline(upright.clone());
        assert!(!l.edge(&upright, false, 0));
    }

    // rustdesk#15886：`ensure_inited()` 在采集器存在之前就运行 wayland 查询，而
    // 那里的失败会保存一个空的 baseline。采集器自身的重试可能在片刻之后
    // 成功并基于布局 A 构建，而那次构建并不是盲目的，因此没有别的东西记录它。一次旋转
    // 在首次轮询之前发生时，就没有任何记忆可以作为边沿的比较对象。
    #[test]
    fn a_capturer_built_after_a_failed_init_still_owes_a_rebuild() {
        let upright = layout(1920, 1080, 0);
        let rotated = layout(1080, 1920, 1);

        let mut l = WaylandLayout::default();
        l.reset_baseline(Vec::new());
        l.note_capturer(&upright, 0);
        assert!(l.edge(&rotated, false, 0));

        // 在构建与轮询之间又发生一次 baseline 重置，情况相同。
        let mut l2 = WaylandLayout::default();
        l2.reset_baseline(Vec::new());
        l2.note_capturer(&upright, 0);
        l2.reset_baseline(rotated.clone());
        assert!(l2.edge(&rotated, false, 0));

        // 对照：两种形态下都没有旋转，也就没有边沿。
        let mut l3 = WaylandLayout::default();
        l3.reset_baseline(Vec::new());
        l3.note_capturer(&upright, 0);
        assert!(!l3.edge(&upright, false, 0));
    }

    // 一次提升会消费掉这个边沿：下一次轮询看到相同的布局，必须保持安静。
    #[test]
    fn a_promoted_layout_is_not_an_edge_again() {
        let rotated = layout(1080, 1920, 1);
        let mut l = WaylandLayout::default();
        l.reset_baseline(layout(1920, 1080, 0));
        l.observe(&rotated);
        l.reset_baseline(rotated.clone());
        assert!(!l.edge(&rotated, false, 0));
    }
}
