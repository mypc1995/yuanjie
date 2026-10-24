#[cfg(target_os = "linux")]
use super::rdp_input::client::{RdpInputKeyboard, RdpInputMouse};
use super::*;
use crate::input::*;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::whiteboard;
use base::message_proto::{
    pointer_device_event::Union::TouchEvent, touch_event::Union::ScaleUpdate,
};
#[cfg(target_os = "macos")]
use dispatch::Queue;
use enigo::{Enigo, Key, KeyboardControllable, MouseButton, MouseControllable};
use hbb_common::{get_time, protobuf::EnumOrUnknown};
use rdev::{self, EventType, Key as RdevKey, KeyCode, RawKey};
#[cfg(target_os = "macos")]
use rdev::{CGEventSourceStateID, CGEventTapLocation, VirtualInput};
#[cfg(target_os = "linux")]
use scrap::wayland::pipewire::RDP_SESSION_INFO;
#[cfg(target_os = "linux")]
use std::sync::mpsc;
use std::{
    convert::TryFrom,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{self, Duration, Instant},
};

#[cfg(windows)]
use winapi::um::winuser::WHEEL_DELTA;

const INVALID_CURSOR_POS: i32 = i32::MIN;
const INVALID_DISPLAY_IDX: i32 = -1;

#[derive(Default)]
struct StateCursor {
    hcursor: u64,
    cursor_data: Arc<Message>,
    cached_cursor_data: HashMap<u64, Arc<Message>>,
}

impl super::service::Reset for StateCursor {
    fn reset(&mut self) {
        * self = Default::default();
        CURSOR_SHAPES.lock().unwrap().clear();
        crate::platform::reset_input_cache();
        fix_key_down_timeout(true);
    }
}

struct StatePos {
    cursor_pos: (i32, i32),
}

impl Default for StatePos {
    fn default() -> Self {
        Self {
            cursor_pos: (INVALID_CURSOR_POS, INVALID_CURSOR_POS),
        }
    }
}

impl super::service::Reset for StatePos {
    fn reset(&mut self) {
        self.cursor_pos = (INVALID_CURSOR_POS, INVALID_CURSOR_POS);
    }
}

impl StatePos {
    #[inline]
    fn is_valid(&self) -> bool {
        self.cursor_pos.0 != INVALID_CURSOR_POS
    }

    #[inline]
    fn is_moved(&self, x: i32, y: i32) -> bool {
        self.is_valid() && (self.cursor_pos.0 != x || self.cursor_pos.1 != y)
    }
}

#[derive(Default)]
struct StateWindowFocus {
    display_idx: i32,
}

impl super::service::Reset for StateWindowFocus {
    fn reset(&mut self) {
        self.display_idx = INVALID_DISPLAY_IDX;
    }
}

impl StateWindowFocus {
    #[inline]
    fn is_valid(&self) -> bool {
        self.display_idx != INVALID_DISPLAY_IDX
    }

    #[inline]
    fn is_changed(&self, disp_idx: i32) -> bool {
        self.is_valid() && self.display_idx != disp_idx
    }
}

#[derive(Default, Clone, Copy)]
struct Input {
    conn: i32,
    time: i64,
    x: i32,
    y: i32,
}

const KEY_CHAR_START: u64 = 9999;

// Insert 键的 XKB 键码（evdev KEY_INSERT 编码 110，加上 XKB 偏移 8）
#[cfg(target_os = "linux")]
const XKB_KEY_INSERT: u16 = evdev::Key::KEY_INSERT.code() + 8;

#[derive(Clone, Default)]
pub struct MouseCursorSub {
    inner: ConnInner,
    cached: HashMap<u64, Arc<Message>>,
}

impl From<ConnInner> for MouseCursorSub {
    fn from(inner: ConnInner) -> Self {
        Self {
            inner,
            cached: HashMap::new(),
        }
    }
}

impl Subscriber for MouseCursorSub {
    #[inline]
    fn id(&self) -> i32 {
        self.inner.id()
    }

    #[inline]
    fn send(&mut self, msg: Arc<Message>) {
        if let Some(message::Union::CursorData(cd)) = &msg.union {
            if let Some(msg) = self.cached.get(&cd.id) {
                self.inner.send(msg.clone());
            } else {
                self.inner.send(msg.clone());
                let mut tmp = Message::new();
                // 只把 id 发出去，客户端侧也需要缓存
                tmp.set_cursor_id(cd.id);
                self.cached.insert(cd.id, Arc::new(tmp));
            }
        } else {
            self.inner.send(msg);
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
struct LockModesHandler {
    caps_lock_changed: bool,
    num_lock_changed: bool,
}

#[cfg(target_os = "macos")]
struct LockModesHandler;

impl LockModesHandler {
    #[inline]
    fn is_modifier_enabled(key_event: &KeyEvent, modifier: ControlKey) -> bool {
        key_event.modifiers.contains(&modifier.into())
    }

    #[inline]
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn new_handler(key_event: &KeyEvent, _is_numpad_key: bool) -> Self {
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            Self::new(key_event, _is_numpad_key)
        }
        #[cfg(target_os = "macos")]
        {
            Self::new(key_event)
        }
    }

    #[cfg(target_os = "linux")]
    fn sleep_to_ensure_locked(v: bool, k: enigo::Key, en: &mut Enigo) {
        if wayland_use_uinput() {
            // 最多 sleep 500 毫秒，以确保锁定状态已生效。
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                if en.get_key_state(k) == v {
                    break;
                }
            }
        } else if wayland_use_rdp_input() {
            // 我们无法调用 `en.get_key_state(k)`，因为没有对应的 api。
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn new(key_event: &KeyEvent, is_numpad_key: bool) -> Self {
        let mut en = ENIGO.lock().unwrap();
        let event_caps_enabled = Self::is_modifier_enabled(key_event, ControlKey::CapsLock);
        let local_caps_enabled = en.get_key_state(enigo::Key::CapsLock);
        let caps_lock_changed = event_caps_enabled != local_caps_enabled;
        if caps_lock_changed {
            en.key_click(enigo::Key::CapsLock);
            #[cfg(target_os = "linux")]
            Self::sleep_to_ensure_locked(event_caps_enabled, enigo::Key::CapsLock, &mut en);
        }

        let mut num_lock_changed = false;
        #[allow(unused)]
        let mut event_num_enabled = false;
        if is_numpad_key {
            let local_num_enabled = en.get_key_state(enigo::Key::NumLock);
            event_num_enabled = Self::is_modifier_enabled(key_event, ControlKey::NumLock);
            num_lock_changed = event_num_enabled != local_num_enabled;
        } else if is_legacy_mode(key_event) {
            #[cfg(target_os = "windows")]
            {
                num_lock_changed =
                    should_disable_numlock(key_event) && en.get_key_state(enigo::Key::NumLock);
            }
        }
        if num_lock_changed {
            en.key_click(enigo::Key::NumLock);
            #[cfg(target_os = "linux")]
            Self::sleep_to_ensure_locked(event_num_enabled, enigo::Key::NumLock, &mut en);
        }

        Self {
            caps_lock_changed,
            num_lock_changed,
        }
    }

    #[cfg(target_os = "macos")]
    fn new(key_event: &KeyEvent) -> Self {
        let event_caps_enabled = Self::is_modifier_enabled(key_event, ControlKey::CapsLock);
        // 不要用下面这段代码来检测 `local_caps_enabled`。
        // 因为 get_key_state 的状态不会影响本文件中 `VIRTUAL_INPUT_STATE` 的模拟。
        //
        // let local_caps_enabled = VirtualInput::get_key_state(
        //     CGEventSourceStateID::CombinedSessionState,
        //     rdev::kVK_CapsLock,
        // );
        let local_caps_enabled = unsafe {
            let _lock = VIRTUAL_INPUT_MTX.lock();
            VIRTUAL_INPUT_STATE
                .as_ref()
                .map_or(false, |input| input.capslock_down)
        };
        if event_caps_enabled && !local_caps_enabled {
            press_capslock();
        } else if !event_caps_enabled && local_caps_enabled {
            release_capslock();
        }

        Self {}
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
impl Drop for LockModesHandler {
    fn drop(&mut self) {
        // 如果是 Wayland uinput，就不要改变 led 状态。
        // 因为在 Wayland uinput 上必须有一个延迟来确保锁定状态生效，
        // 这可能会影响用户体验。
        #[cfg(target_os = "linux")]
        if wayland_use_uinput() {
            return;
        }

        let mut en = ENIGO.lock().unwrap();
        if self.caps_lock_changed {
            en.key_click(enigo::Key::CapsLock);
        }
        if self.num_lock_changed {
            en.key_click(enigo::Key::NumLock);
        }
    }
}

#[inline]
#[cfg(target_os = "windows")]
fn should_disable_numlock(evt: &KeyEvent) -> bool {
    // 在 numlock 打开时按下 home 等键就禁用 numlock，
    // 否则我们会得到数字小键盘的值（7、8、9 等）
    match (&evt.union, evt.mode.enum_value_or(KeyboardMode::Legacy)) {
        (Some(key_event::Union::ControlKey(ck)), KeyboardMode::Legacy) => {
            return NUMPAD_KEY_MAP.contains_key(&ck.value());
        }
        _ => {}
    }
    false
}

pub const NAME_CURSOR: &'static str = "mouse_cursor";
pub const NAME_POS: &'static str = "mouse_pos";
pub const NAME_WINDOW_FOCUS: &'static str = "window_focus";
#[derive(Clone)]
pub struct MouseCursorService {
    pub sp: ServiceTmpl<MouseCursorSub>,
}

impl Deref for MouseCursorService {
    type Target = ServiceTmpl<MouseCursorSub>;

    fn deref(&self) -> &Self::Target {
        &self.sp
    }
}

impl DerefMut for MouseCursorService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sp
    }
}

impl MouseCursorService {
    pub fn new(name: String, need_snapshot: bool) -> Self {
        Self {
            sp: ServiceTmpl::<MouseCursorSub>::new(name, need_snapshot),
        }
    }
}

pub fn new_cursor() -> ServiceTmpl<MouseCursorSub> {
    let svc = MouseCursorService::new(NAME_CURSOR.to_owned(), true);
    ServiceTmpl::<MouseCursorSub>::repeat::<StateCursor, _, _>(&svc.clone(), 33, run_cursor);
    svc.sp
}

pub fn new_pos() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME_POS.to_owned(), false);
    GenericService::repeat::<StatePos, _, _>(&svc.clone(), 33, run_pos);
    svc.sp
}

pub fn new_window_focus() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME_WINDOW_FOCUS.to_owned(), false);
    GenericService::repeat::<StateWindowFocus, _, _>(&svc.clone(), 33, run_window_focus);
    svc.sp
}

#[inline]
fn update_last_cursor_pos(x: i32, y: i32) {
    let mut lock = LATEST_SYS_CURSOR_POS.lock().unwrap();
    if lock.1 .0 != x || lock.1 .1 != y {
        (lock.0, lock.1) = (Some(Instant::now()), (x, y))
    }
}

fn run_pos(sp: EmptyExtraFieldService, state: &mut StatePos) -> ResultType<()> {
    let (_, (x, y)) = *LATEST_SYS_CURSOR_POS.lock().unwrap();
    if x == INVALID_CURSOR_POS || y == INVALID_CURSOR_POS {
        return Ok(());
    }

    if state.is_moved(x, y) {
        let mut msg_out = Message::new();
        msg_out.set_cursor_position(CursorPosition {
            x,
            y,
            ..Default::default()
        });
        let exclude = {
            let now = get_time();
            let lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
            if now - lock.time < 300 {
                lock.conn
            } else {
                0
            }
        };
        sp.send_without(msg_out, exclude);
    }
    state.cursor_pos = (x, y);

    sp.snapshot(|sps| {
        let mut msg_out = Message::new();
        msg_out.set_cursor_position(CursorPosition {
            x: state.cursor_pos.0,
            y: state.cursor_pos.1,
            ..Default::default()
        });
        sps.send(msg_out);
        Ok(())
    })?;
    Ok(())
}

fn run_cursor(sp: MouseCursorService, state: &mut StateCursor) -> ResultType<()> {
    if let Some(hcursor) = crate::get_cursor()? {
        if hcursor != state.hcursor {
            let msg;
            // 在 DRM 路径上，get_cursor_data() 可能返回一个 id 已经超过所请求
            // `hcursor` 的快照（它返回的是最新的硬件光标）；要把它归档到缓存中，并且
            // 在实际提供出去的那个 id 下记录 state.hcursor，这样该形状之后再次出现时
            // 能正确去重而不是被抑制。下面的一切都完全
            // 以 drm 特性为开关，因此关闭 drm 的构建与上游逐字节一致。
            #[cfg(all(target_os = "linux", feature = "drm"))]
            let mut drm_served_id = hcursor;
            if let Some(cached) = state.cached_cursor_data.get(&hcursor) {
                super::log::trace!("Cursor data cached, hcursor: {}", hcursor);
                msg = cached.clone();
            } else {
                let data = crate::get_cursor_data(hcursor)?;
                // 把形状归档在实际提供出去的那个 id 下，而不是被请求的那个。这里刻意用
                // 一个新名字，而不是遮蔽 `hcursor`：下面的插入把它读作被请求的
                // id，而在这个函数的其余各处都是这样理解的；一个受 cfg 控制的遮蔽会让两种
                // 构建对那一行的含义产生分歧。
                #[cfg(all(target_os = "linux", feature = "drm"))]
                let served_id = data.id;
                #[cfg(all(target_os = "linux", feature = "drm"))]
                {
                    drm_served_id = served_id;
                }
                #[cfg(all(target_os = "linux", feature = "drm"))]
                let cache_key = served_id;
                #[cfg(not(all(target_os = "linux", feature = "drm")))]
                let cache_key = hcursor;
                msg = cursor_shape_message(data, hbb_common::compress::compress);
                // DRM 光标 id 是由形状的像素加上几何信息推导出来的，因此一个带动画的
                // 指针每次形状变化都会生成新的 id，而这个 map 会在服务的整个生命周期内不断增长，
                // 每个条目都钉住一条压缩过的光标消息。（上游的 X11
                // id 来自一小批 XFixes 序列号，所以这个 map 实际上是有界的，
                // 这也是上限受开关控制、原版构建保持不变的原因。）
                // 超过上限后，丢弃这个 map 并重新开始：下一次请求任何被淘汰的
                // 形状时只需重新压缩它，而这个上限足以覆盖每个静态形状
                // 以及一段相当宽裕的动画窗口。
                #[cfg(all(target_os = "linux", feature = "drm"))]
                {
                    const CURSOR_CACHE_MAX: usize = 64;
                    if state.cached_cursor_data.len() >= CURSOR_CACHE_MAX {
                        state.cached_cursor_data.clear();
                        CURSOR_SHAPES.lock().unwrap().clear();
                        // 上面保留了正在发送的形状；清空之后也要保留它。
                        shared_cursor_shape(msg.clone());
                    }
                }
                limit_cursor_handles(&mut state.cached_cursor_data, &msg);
                // macOS 的 seed 标记的是一次变化，同一个绝不会出现两次：归档它只会
                // 把 map 填满。
                #[cfg(not(target_os = "macos"))]
                state.cached_cursor_data.insert(cache_key, msg.clone());
                super::log::trace!("Cursor data updated, hcursor: {}", cache_key);
            }
            #[cfg(not(all(target_os = "linux", feature = "drm")))]
            {
                state.hcursor = hcursor;
            }
            #[cfg(all(target_os = "linux", feature = "drm"))]
            {
                state.hcursor = drm_served_id;
            }
            // 为已经显示的形状新建一个 handle 或 seed，对控制端看到的东西没有任何影响；
            // 无论有多少个 handle 指向它，一个形状就是一条消息。
            if !Arc::ptr_eq(&msg, &state.cursor_data) {
                sp.send_shared(msg.clone());
                state.cursor_data = msg;
            }
        }
    }
    sp.snapshot(|sps| {
        sps.send_shared(state.cursor_data.clone());
        Ok(())
    })?;
    Ok(())
}

fn run_window_focus(sp: EmptyExtraFieldService, state: &mut StateWindowFocus) -> ResultType<()> {
    let displays = super::display_service::get_sync_displays();
    if displays.len() <= 1 {
        return Ok(());
    }
    let disp_idx = crate::get_focused_display(displays);
    if let Some(disp_idx) = disp_idx.map(|id| id as i32) {
        if state.is_changed(disp_idx) {
            let mut misc = Misc::new();
            misc.set_follow_current_display(disp_idx as i32);
            let mut msg_out = Message::new();
            msg_out.set_misc(misc);
            sp.send(msg_out);
        }
        state.display_idx = disp_idx;
    }
    Ok(())
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum KeysDown {
    RdevKey(RawKey),
    EnigoKey(u64),
}

lazy_static::lazy_static! {
    static ref ENIGO: Arc<Mutex<Enigo>> = {
        Arc::new(Mutex::new(Enigo::new()))
    };
    static ref KEYS_DOWN: Arc<Mutex<HashMap<KeysDown, Instant>>> = Default::default();
    static ref LATEST_PEER_INPUT_CURSOR: Arc<Mutex<Input>> = Default::default();
    static ref LATEST_SYS_CURSOR_POS: Arc<Mutex<(Option<Instant>, (i32, i32))>> = Arc::new(Mutex::new((None, (INVALID_CURSOR_POS, INVALID_CURSOR_POS))));
    // 跟踪当前正在使用相对鼠标移动的连接。
    // 用于在相对模式下对所有事件禁用白板/光标显示。
    static ref RELATIVE_MOUSE_CONNS: Arc<Mutex<std::collections::HashSet<i32>>> = Default::default();
}

lazy_static::lazy_static! {
    // 服务已发送的每个形状，按内容 id 记录，供再次来请求的控制端使用。
    // 在这里，一个由多个 handle 指向的形状只有一条消息，与 `cached_cursor_data` 共享，
    // 并随之被清除；各个上限参见 `limit_cursor_handles`。被丢弃的形状
    // 会在下次显示时在这里重建（在 `run_cursor` 中），在任何连接发出它的
    // `cursor_id` 之前：控制端刚被告知要显示什么，就可以来请求什么。
    static ref CURSOR_SHAPES: Mutex<CursorShapes> = Default::default();
}

/// 已发送的形状，按内容 id 记录，以及它们压缩后持有的字节。
#[derive(Default)]
struct CursorShapes {
    shapes: HashMap<u64, Arc<Message>>,
    bytes: usize,
}

impl CursorShapes {
    fn get(&self, id: &u64) -> Option<&Arc<Message>> {
        self.shapes.get(id)
    }

    fn clear(&mut self) {
        self.shapes.clear();
        self.bytes = 0;
    }

    /// 为该形状保留的消息，若之前没有则为 `msg`。
    fn keep(&mut self, cd: &CursorData, msg: &Arc<Message>) -> Arc<Message> {
        let bytes = &mut self.bytes;
        self.shapes
            .entry(cd.id)
            .or_insert_with(|| {
                * bytes += cd.colors.len();
                msg.clone()
            })
            .clone()
    }
}

/// 平台提供的形状对应的消息，按内容命名，这样按连接发送时
/// 无论平台给出多少个 handle，一个形状只发送一次；参见
/// `is_peer_naming_cursors_by_content`。之前以任何 handle 发送过的形状会被直接复用，
/// 不会重新压缩。
fn cursor_shape_message(
    mut data: CursorData,
    compress: impl FnOnce(&[u8]) -> Vec<u8>,
) -> Arc<Message> {
    data.id = crate::cursor_content_id(data.width, data.height, data.hotx, data.hoty, &data.colors);
    if let Some(msg) = cursor_data_message(data.id) {
        return msg;
    }
    data.colors = compress(&data.colors[..]).into();
    let mut msg = Message::new();
    msg.set_cursor_data(data);
    shared_cursor_shape(Arc::new(msg))
}

// 某些平台每次显示一个形状时都会生成新的 handle（Chrome、Electron），因此这些 handle
// 一旦归档就会在服务的整个生命周期内增长，每个条目都是一个 u64 和一个 Arc，哪怕每一个
// 都指向同一个形状。超过这么多 handle 或这么多压缩字节后，无论
// 有多少，handle 和形状就一起重新开始：再次显示的 handle 会被捕获并命名
// 。macOS 不归档任何 handle，所以在那里只由字节数限制形状数量。
const CURSOR_HANDLES_MAX: usize = 4096;
const CURSOR_SHAPES_BYTES_MAX: usize = 32 << 20;

fn limit_cursor_handles(handles: &mut HashMap<u64, Arc<Message>>, sending: &Arc<Message>) {
    if handles.len() >= CURSOR_HANDLES_MAX
        || CURSOR_SHAPES.lock().unwrap().bytes >= CURSOR_SHAPES_BYTES_MAX
    {
        handles.clear();
        CURSOR_SHAPES.lock().unwrap().clear();
        // 正在发送的形状被保留了；清空之后也要保留它。
        shared_cursor_shape(sending.clone());
    }
}

/// 该形状已经保留的消息（如果有），这样指向它的每个 handle 都共享它。
fn shared_cursor_shape(msg: Arc<Message>) -> Arc<Message> {
    let Some(message::Union::CursorData(cd)) = &msg.union else {
        return msg;
    };
    CURSOR_SHAPES.lock().unwrap().keep(cd, &msg)
}

/// 服务已发送的某个形状的 CursorData 消息，供 `Misc::request_cursor_data` 使用。
pub fn cursor_data_message(id: u64) -> Option<Arc<Message>> {
    CURSOR_SHAPES.lock().unwrap().get(&id).cloned()
}

#[cfg(target_os = "linux")]
lazy_static::lazy_static! {
    static ref WAYLAND_CLIPBOARD_INPUT_RECORDS: Arc<Mutex<Vec<(Instant, String)>>> =
        Default::default();
}

#[inline]
fn set_relative_mouse_active(conn: i32, active: bool) {
    let mut lock = RELATIVE_MOUSE_CONNS.lock().unwrap();
    if active {
        lock.insert(conn);
    } else {
        lock.remove(&conn);
    }
}

#[inline]
fn is_relative_mouse_active(conn: i32) -> bool {
    RELATIVE_MOUSE_CONNS.lock().unwrap().contains(&conn)
}

/// 清除某个连接的相对鼠标模式状态。
///
/// 当一个已认证的连接被丢弃时（在连接拆除过程中）必须调用它，
/// 以免在 `RELATIVE_MOUSE_CONNS`（一个 `Mutex<HashSet<i32>>`）中泄漏该连接 id。
/// 调用方负责在断开连接时调用它。
#[inline]
pub(crate) fn clear_relative_mouse_active(conn: i32) {
    set_relative_mouse_active(conn, false);
}

static EXITING: AtomicBool = AtomicBool::new(false);

const MOUSE_MOVE_PROTECTION_TIMEOUT: Duration = Duration::from_millis(1_000);
// 这里 (x,y) 的实际差值是 (1,1)。不过 5 可能更宽容一些。
const MOUSE_ACTIVE_DISTANCE: i32 = 5;

static RECORD_CURSOR_POS_RUNNING: AtomicBool = AtomicBool::new(false);

// https://github.com/rustdesk/rustdesk/issues/9729
// 在 legacy 模式下使用 macOS 时，我们需要做一些特殊处理。
#[cfg(target_os = "macos")]
static LAST_KEY_LEGACY_MODE: AtomicBool = AtomicBool::new(true);
// 我们用 enigo 来
// 1. 模拟鼠标事件
// 2. 模拟 legacy 模式的按键事件
// 3. 模拟功能键事件，比如 LockScreen
#[inline]
#[cfg(target_os = "macos")]
fn enigo_ignore_flags() -> bool {
    !LAST_KEY_LEGACY_MODE.load(Ordering::SeqCst)
}
#[inline]
#[cfg(target_os = "macos")]
fn set_last_legacy_mode(v: bool) {
    LAST_KEY_LEGACY_MODE.store(v, Ordering::SeqCst);
    ENIGO.lock().unwrap().set_ignore_flags(!v);
}

pub fn try_start_record_cursor_pos() -> Option<thread::JoinHandle<()>> {
    if RECORD_CURSOR_POS_RUNNING.load(Ordering::SeqCst) {
        return None;
    }

    RECORD_CURSOR_POS_RUNNING.store(true, Ordering::SeqCst);
    let handle = thread::spawn(|| {
        let interval = time::Duration::from_millis(33);
        loop {
            if !RECORD_CURSOR_POS_RUNNING.load(Ordering::SeqCst) {
                break;
            }

            let now = time::Instant::now();
            if let Some((x, y)) = crate::get_cursor_pos() {
                update_last_cursor_pos(x, y);
            }
            let elapsed = now.elapsed();
            if elapsed < interval {
                thread::sleep(interval - elapsed);
            }
        }
        update_last_cursor_pos(INVALID_CURSOR_POS, INVALID_CURSOR_POS);
    });
    Some(handle)
}

pub fn try_stop_record_cursor_pos() {
    let remote_count = AUTHED_CONNS
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.conn_type == AuthConnType::Remote)
        .count();
    if remote_count > 0 {
        return;
    }
    RECORD_CURSOR_POS_RUNNING.store(false, Ordering::SeqCst);
}

// mac 的按键输入必须在主线程中运行，否则在 >= osx 10.15 上会崩溃
#[cfg(target_os = "macos")]
lazy_static::lazy_static! {
    static ref QUEUE: Queue = Queue::main();
}

#[cfg(target_os = "macos")]
struct VirtualInputState {
    virtual_input: VirtualInput,
    capslock_down: bool,
}

#[cfg(target_os = "macos")]
impl VirtualInputState {
    const KEY_UP_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);

    fn new() -> Option<Self> {
        VirtualInput::new(
            CGEventSourceStateID::CombinedSessionState,
            // 注意：`CGEventTapLocation::Session` 会受到鼠标事件的影响。
            // 当我们在模拟按键事件时移动物理鼠标，按键事件会受到影响。
            // 现象类似于 https://github.com/rustdesk/rustdesk/issues/9729#issuecomment-2432306822
            // 1. 在 RustDesk 中按下 "Command" 键
            // 2. 移动物理鼠标
            // 3. 在 RustDesk 中按下 "V" 键
            // 结果被控端只是打印出 "v"，而不是粘贴。
            //
            // 把 `CGEventTapLocation::Session` 改成 `CGEventTapLocation::HID` 即可解决。
            // 但我们不认为这是一个 bug，因为它不是常见情况，
            // 我们只考虑 RustDesk 操作被控端这一种场景。
            //
            // https://developer.apple.com/documentation/coregraphics/cgeventtaplocation/
            CGEventTapLocation::Session,
        )
        .map(|virtual_input| Self {
            virtual_input,
            capslock_down: false,
        })
        .ok()
    }

    #[inline]
    fn simulate(&self, event_type: &EventType) -> ResultType<()> {
        Ok(self.virtual_input.simulate(&event_type)?)
    }
}

#[cfg(target_os = "macos")]
static mut VIRTUAL_INPUT_MTX: Mutex<()> = Mutex::new(());
#[cfg(target_os = "macos")]
static mut VIRTUAL_INPUT_STATE: Option<VirtualInputState> = None;

// 第一次调用 set_uinput() 会创建键盘和鼠标客户端。
// 这些客户端是 ipc 连接，存活时间必须短于 tokio 运行时。
// 因此这个函数绝不能在临时运行时中调用。
#[cfg(target_os = "linux")]
pub async fn setup_uinput(minx: i32, maxx: i32, miny: i32, maxy: i32) -> ResultType<()> {
    // 键盘和鼠标都打开 /dev/uinput
    // TODO: Make sure there's no race
    set_uinput_resolution(minx, maxx, miny, maxy).await?;

    let keyboard = super::uinput::client::UInputKeyboard::new().await?;
    log::info!("UInput keyboard created");
    let mouse = super::uinput::client::UInputMouse::new().await?;
    log::info!("UInput mouse created");

    let mut en = ENIGO.lock().unwrap();
    // enigo 在构造时猜测了一次 x11，而 Wayland 欢迎界面正是这样读的，于是
    // 接着把下面安装的设备都路由到一个空的 xdo，它会静默丢弃一切。
    // 能走到这里说明 `wayland_use_uinput()` 为 true，因此这是在陈述一个事实。
    en.set_is_x11(false);
    // 两者共用一把锁，因此不会出现键盘是自定义而鼠标不是的时间窗口。
    en.set_custom_keyboard(Box::new(keyboard));
    en.set_custom_mouse(Box::new(mouse));
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn setup_rdp_input() -> ResultType<(), Box<dyn std::error::Error>> {
    let mut en = ENIGO.lock()?;
    // 与 `setup_uinput` 相同：调用方以 `wayland_use_rdp_input()` 为开关。
    en.set_is_x11(false);
    let rdp_info_lock = RDP_SESSION_INFO.lock()?;
    let rdp_info = rdp_info_lock.as_ref().ok_or("RDP session is None")?;

    let keyboard = RdpInputKeyboard::new(rdp_info.conn.clone(), rdp_info.session.clone())?;
    en.set_custom_keyboard(Box::new(keyboard));
    log::info!("RdpInput keyboard created");

    if let Some(stream) = rdp_info.streams.clone().into_iter().next() {
        let resolution = rdp_info
            .resolution
            .lock()
            .unwrap()
            .unwrap_or(stream.get_size());
        let mouse = RdpInputMouse::new(
            rdp_info.conn.clone(),
            rdp_info.session.clone(),
            stream,
            resolution,
        )?;
        en.set_custom_mouse(Box::new(mouse));
        log::info!("RdpInput mouse created");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn update_mouse_resolution(minx: i32, maxx: i32, miny: i32, maxy: i32) -> ResultType<()> {
    set_uinput_resolution(minx, maxx, miny, maxy).await?;

    // 在调用方缓存之前，确认设备已经采纳了新的范围。
    // 用 spawn_blocking，因为 ENIGO 是一个 std Mutex，而 send_refresh 会阻塞在 IPC 上。
    tokio::task::spawn_blocking(move || {
        if let Some(mouse) = ENIGO.lock().unwrap().get_custom_mouse() {
            if let Some(mouse) = mouse
                .as_mut_any()
                .downcast_mut::<super::uinput::client::UInputMouse>()
            {
                return mouse.send_refresh();
            }
            bail!("failed to downcast custom mouse to UInputMouse");
        }
        // 没有自定义鼠标：无需刷新任何东西。
        Ok(())
    })
    .await?
}

#[cfg(target_os = "linux")]
async fn set_uinput_resolution(minx: i32, maxx: i32, miny: i32, maxy: i32) -> ResultType<()> {
    super::uinput::client::set_resolution(minx, maxx, miny, maxy).await
}

pub fn is_left_up(evt: &MouseEvent) -> bool {
    let buttons = evt.mask >> 3;
    let evt_type = evt.mask & MOUSE_TYPE_MASK;
    buttons == MOUSE_BUTTON_LEFT && evt_type == MOUSE_TYPE_UP
}

#[cfg(windows)]
pub fn mouse_move_relative(x: i32, y: i32) {
    crate::platform::windows::try_change_desktop();
    let mut en = ENIGO.lock().unwrap();
    en.mouse_move_relative(x, y);
}

#[cfg(windows)]
fn modifier_sleep() {
    // sleep 一段时间，目前这只是为了在 peer 中输入 rdp
    std::thread::sleep(std::time::Duration::from_nanos(1));
}

#[inline]
#[cfg(not(target_os = "macos"))]
fn is_pressed(key: &Key, en: &mut Enigo) -> bool {
    get_modifier_state(key.clone(), en)
}

// 在我的测试中 sleep 8ms 就够了，但为了稳妥我们 sleep 12ms。
// sleep 12ms 在我的测试中，字符已经是实时输出了。
#[inline]
#[cfg(target_os = "macos")]
fn key_sleep() {
    // https://www.reddit.com/r/rustdesk/comments/1kn1w5x/typing_lags_when_connecting_to_macos_clients/
    //
    // 用 `launchctl load -w /Library/LaunchAgents/abc.plist` 运行时有一个奇怪的 bug
    // `std::thread::sleep(Duration::from_millis(20));` 可能会睡 90 毫秒甚至更久。
    // 不过在终端里运行 `/Applications/RustDesk.app/Contents/MacOS/rustdesk --server` 是没问题的。
    let now = Instant::now();
    while now.elapsed() < Duration::from_millis(12) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[inline]
fn get_modifier_state(key: Key, en: &mut Enigo) -> bool {
    // https://github.com/rustdesk/rustdesk/issues/332
    // 在 Linux 上，如果按下了 RightAlt，RightAlt 的状态是 false，而 Alt 的状态是 true
    // 但在 Windows 上，两者都是 true
    let x = en.get_key_state(key.clone());
    match key {
        Key::Shift => x || en.get_key_state(Key::RightShift),
        Key::Control => x || en.get_key_state(Key::RightControl),
        Key::Alt => x || en.get_key_state(Key::RightAlt),
        Key::Meta => x || en.get_key_state(Key::RWin),
        Key::RightShift => x || en.get_key_state(Key::Shift),
        Key::RightControl => x || en.get_key_state(Key::Control),
        Key::RightAlt => x || en.get_key_state(Key::Alt),
        Key::RWin => x || en.get_key_state(Key::Meta),
        _ => x,
    }
}

#[allow(unreachable_code)]
pub fn handle_mouse(
    evt: &MouseEvent,
    conn: i32,
    username: String,
    argb: u32,
    simulate: bool,
    show_cursor: bool,
) {
    #[cfg(target_os = "macos")]
    {
        // 有 GUI 时（--server 也有托盘，它同样是 GUI），要在主 GUI 线程中运行，否则会崩溃
        let evt = evt.clone();
        QUEUE.exec_async(move || handle_mouse_(&evt, conn, username, argb, simulate, show_cursor));
        return;
    }
    #[cfg(windows)]
    crate::portable_service::client::handle_mouse(evt, conn, username, argb, simulate, show_cursor);
    #[cfg(not(windows))]
    handle_mouse_(evt, conn, username, argb, simulate, show_cursor);
}

// 待办：合并 handle_mouse 和 handle_pointer
#[allow(unreachable_code)]
pub fn handle_pointer(evt: &PointerDeviceEvent, conn: i32) {
    #[cfg(target_os = "macos")]
    {
        // 有 GUI 时，要在主 GUI 线程中运行，否则会崩溃
        let evt = evt.clone();
        QUEUE.exec_async(move || handle_pointer_(&evt, conn));
        return;
    }
    #[cfg(windows)]
    crate::portable_service::client::handle_pointer(evt, conn);
    #[cfg(not(windows))]
    handle_pointer_(evt, conn);
}

pub fn fix_key_down_timeout_loop() {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(10_000));
        fix_key_down_timeout(false);
    });
    if let Err(err) = ctrlc::set_handler(move || {
        fix_key_down_timeout_at_exit();
        std::process::exit(0); // will call atexit on posix, but not on Windows
    }) {
        log::error!("Failed to set Ctrl-C handler: {}", err);
    }
}

pub fn fix_key_down_timeout_at_exit() {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    EXITING.store(true, Ordering::SeqCst);
    fix_key_down_timeout(true);
    log::info!("fix_key_down_timeout_at_exit");
}

#[inline]
#[cfg(target_os = "linux")]
pub fn clear_remapped_keycode() {
    ENIGO.lock().unwrap().tfc_clear_remapped();
}

#[inline]
fn record_key_is_control_key(record_key: u64) -> bool {
    record_key < KEY_CHAR_START
}

#[inline]
fn record_key_is_chr(record_key: u64) -> bool {
    record_key >= KEY_CHAR_START
}

#[inline]
fn record_key_to_key(record_key: u64) -> Option<Key> {
    if record_key_is_control_key(record_key) {
        control_key_value_to_key(record_key as _)
    } else if record_key_is_chr(record_key) {
        let chr: u32 = (record_key - KEY_CHAR_START) as _;
        Some(char_value_to_key(chr))
    } else {
        None
    }
}

pub fn release_device_modifiers() {
    let mut en = ENIGO.lock().unwrap();
    for modifier in [
        Key::Shift,
        Key::Control,
        Key::Alt,
        Key::Meta,
        Key::RightShift,
        Key::RightControl,
        Key::RightAlt,
        Key::RWin,
    ] {
        if get_modifier_state(modifier, &mut en) {
            en.key_up(modifier);
        }
    }
}

#[inline]
fn release_record_key(record_key: KeysDown) {
    let func = move || match record_key {
        KeysDown::RdevKey(raw_key) => {
            simulate_(&EventType::KeyRelease(RdevKey::RawKey(raw_key)));
        }
        KeysDown::EnigoKey(key) => {
            if let Some(key) = record_key_to_key(key) {
                ENIGO.lock().unwrap().key_up(key);
                log::debug!("Fixed {:?} timeout", key);
            }
        }
    };

    #[cfg(target_os = "macos")]
    QUEUE.exec_async(func);
    #[cfg(not(target_os = "macos"))]
    func();
}

fn fix_key_down_timeout(force: bool) {
    let key_down = KEYS_DOWN.lock().unwrap();
    if key_down.is_empty() {
        return;
    }
    let cloned = (*key_down).clone();
    drop(key_down);

    for (record_key, time) in cloned.into_iter() {
        if force || time.elapsed().as_millis() >= 360_000 {
            record_pressed_key(record_key, false);
            release_record_key(record_key);
        }
    }
}

// 例如 ctrl 的当前状态是按下，但 ctrl 不在 modifier 中，我们应该把 ctrl 改成抬起，以使 modifier 状态在远端和本地之间保持同步
#[inline]
fn fix_modifier(
    modifiers: &[EnumOrUnknown<ControlKey>],
    key0: ControlKey,
    key1: Key,
    en: &mut Enigo,
) {
    if get_modifier_state(key1, en) && !modifiers.contains(&EnumOrUnknown::new(key0)) {
        #[cfg(windows)]
        if key0 == ControlKey::Control && get_modifier_state(Key::Alt, en) {
            // AltGr 的情况
            return;
        }
        en.key_up(key1);
        log::debug!("Fixed {:?}", key1);
    }
}

fn fix_modifiers(modifiers: &[EnumOrUnknown<ControlKey>], en: &mut Enigo, ck: i32) {
    if ck != ControlKey::Shift.value() {
        fix_modifier(modifiers, ControlKey::Shift, Key::Shift, en);
    }
    if ck != ControlKey::RShift.value() {
        fix_modifier(modifiers, ControlKey::Shift, Key::RightShift, en);
    }
    if ck != ControlKey::Alt.value() {
        fix_modifier(modifiers, ControlKey::Alt, Key::Alt, en);
    }
    if ck != ControlKey::RAlt.value() {
        fix_modifier(modifiers, ControlKey::Alt, Key::RightAlt, en);
    }
    if ck != ControlKey::Control.value() {
        fix_modifier(modifiers, ControlKey::Control, Key::Control, en);
    }
    if ck != ControlKey::RControl.value() {
        fix_modifier(modifiers, ControlKey::Control, Key::RightControl, en);
    }
    if ck != ControlKey::Meta.value() {
        fix_modifier(modifiers, ControlKey::Meta, Key::Meta, en);
    }
    if ck != ControlKey::RWin.value() {
        fix_modifier(modifiers, ControlKey::Meta, Key::RWin, en);
    }
}

/// 最近一次由对端注入的绝对指针位置，位于重映射之后的 uinput 布局空间中，并带有
/// 注入时间、一个序列号，以及它所对照映射的 wayland 快照代际
/// 。刻意不用 `LATEST_PEER_INPUT_CURSOR`：它的相对分支保存的是
/// `get_cursor_pos()`，一个从未经过布局重映射的 X 服务器坐标，而
/// DRM 光标校准要从这里减去一个光标平面位置，因此这两个空间必须
/// 一致。只有 uinput 的绝对路径会写入这里。
#[cfg(all(target_os = "linux", feature = "drm"))]
static LATEST_PEER_ABS_POS: std::sync::Mutex<
    Option<((i32, i32), std::time::Instant, u64, u64, u64)>,
> = std::sync::Mutex::new(None);
#[cfg(all(target_os = "linux", feature = "drm"))]
static PEER_ABS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一个对端绝对样本，按校准读取它的方式。
#[cfg(all(target_os = "linux", feature = "drm"))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PeerAbsSample {
    pub pos: (i32, i32),
    pub age_ms: u64,
    /// 每次注入的移动都不同。针对同一个样本的两次测量并不
    /// 独立，无论中间光标平面做了什么。
    pub seq: u64,
    /// 该点所对照映射的布局代际。
    pub gen: u64,
    /// 注入时的输入映射 epoch：在采纳某个范围之前产生的样本是按
    /// 旧范围映射的。参见 `display_service::input_map_epoch`。
    pub map_epoch: u64,
}

/// 对端把指针移动到一个绝对的重映射之后的位置。在 ENIGO 守卫下被调用，
/// 该守卫也保护这次移动，而采纳同样需要它来完成自己的刷新：一次以某次采纳的计数打戳的
/// 移动是在那次采纳的刷新之后注入的。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) fn note_peer_absolute_move(x: i32, y: i32) {
    let seq = PEER_ABS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let gen = scrap::wayland::display::wayland_snapshot_generation();
    let map_epoch = super::display_service::input_map_epoch();
    * LATEST_PEER_ABS_POS.lock().unwrap() =
        Some(((x, y), std::time::Instant::now(), seq, gen, map_epoch));
}

/// 指针通过一种没有重映射后绝对位置可提供的路径移动了：来自对端的相对
/// 增量，或是本进程自己发起的移动。上一个样本不再能说明
/// 指针在哪里；忘掉它，下一次绝对移动会重新提供它。
#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) fn note_pointer_moved_without_absolute_sample() {
    * LATEST_PEER_ABS_POS.lock().unwrap() = None;
}

/// 本进程自己发起的移动。这个「忘掉」发生在 `lock` 返回守卫之后，
/// 而对端移动正是在该守卫之下记录其样本的，因此不会有对端移动落在这「忘掉」与本次移动之间，
/// 让它留下的样本描述的是一个此后已经移动过的指针。
#[cfg(all(target_os = "linux", feature = "drm"))]
fn forget_sample_and_move<G>(lock: impl FnOnce() -> G, move_to: impl FnOnce(G)) {
    let guard = lock();
    note_pointer_moved_without_absolute_sample();
    move_to(guard);
}

#[cfg(all(target_os = "linux", feature = "drm"))]
pub(crate) fn last_peer_abs_sample() -> Option<PeerAbsSample> {
    let (pos, at, seq, gen, map_epoch) = (*LATEST_PEER_ABS_POS.lock().unwrap())?;
    Some(PeerAbsSample {
        pos,
        age_ms: at.elapsed().as_millis() as u64,
        seq,
        gen,
        map_epoch,
    })
}

// 更新时间，以避免向对端发送光标位置事件。
// 参见 `run_pos` --> `set_cursor_position` --> `exclude`
#[inline]
pub fn update_latest_input_cursor_time(conn: i32) {
    let mut lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
    lock.conn = conn;
    lock.time = get_time();
}

#[inline]
fn get_last_input_cursor_pos() -> (i32, i32) {
    let lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
    (lock.x, lock.y)
}

// 检查鼠标是否被被控端用户移动，以让被控端的鼠标优先级高于远端。
fn active_mouse_(_conn: i32) -> bool {
    true
    /* 这个方法有 bug（在 macOS 上不起作用，会让快速移动的鼠标事件在这里被丢弃），而且会增加延迟（这是阻塞式做法，必须用异步方式），所以我们暂时禁用它
    // 超时保护
    if LATEST_SYS_CURSOR_POS
        .lock()
        .unwrap()
        .0
        .map(|t| t.elapsed() > MOUSE_MOVE_PROTECTION_TIMEOUT)
        .unwrap_or(true)
    {
        return true;
    }

    // 上一个连接的输入可能受到保护
    if LATEST_PEER_INPUT_CURSOR.lock().unwrap().conn != conn {
        return false;
    }

    let in_active_dist = |a: i32, b: i32| -> bool { (a - b).abs() < MOUSE_ACTIVE_DISTANCE };

    // 检查输入是否在有效范围内
    match crate::get_cursor_pos() {
        Some((x, y)) => {
            let (last_in_x, last_in_y) = get_last_input_cursor_pos();
            let mut can_active = in_active_dist(last_in_x, x) && in_active_dist(last_in_y, y);
            // 如果系统现在很忙，光标可能还没被移动到上一个输入位置。
            // 虽然这不是常见情况，我们还是会在稍后再次检查它。
            if !can_active {
                // 100 微秒可能就足够系统移动光标了。
                // macOS 上的鼠标输入是异步的。1. 放入队列，在主线程中处理。2. 异步发送事件。
                // 在 macOS 上需要更多次重试。
                #[cfg(not(target_os = "macos"))]
                let retries = 10;
                #[cfg(target_os = "macos")]
                let retries = 100;
                #[cfg(not(target_os = "macos"))]
                let sleep_interval: u64 = 10;
                #[cfg(target_os = "macos")]
                let sleep_interval: u64 = 30;
                for _retry in 0..retries {
                    std::thread::sleep(std::time::Duration::from_micros(sleep_interval));
                    // 在这里 sleep 不知怎么也能抑制延迟的累积。
                    if let Some((x2, y2)) = crate::get_cursor_pos() {
                        let (last_in_x, last_in_y) = get_last_input_cursor_pos();
                        can_active = in_active_dist(last_in_x, x2) && in_active_dist(last_in_y, y2);
                        if can_active {
                            break;
                        }
                    }
                }
            }
            if !can_active {
                let mut lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
                lock.x = INVALID_CURSOR_POS / 2;
                lock.y = INVALID_CURSOR_POS / 2;
            }
            can_active
        }
        None => true,
    }
    */
}

pub fn handle_pointer_(evt: &PointerDeviceEvent, conn: i32) {
    if !active_mouse_(conn) {
        return;
    }

    if EXITING.load(Ordering::SeqCst) {
        return;
    }

    match &evt.union {
        Some(TouchEvent(evt)) => match &evt.union {
            Some(ScaleUpdate(_scale_evt)) => {
                #[cfg(target_os = "windows")]
                handle_scale(_scale_evt.scale);
            }
            _ => {}
        },
        _ => {}
    }
}

pub fn handle_mouse_(
    evt: &MouseEvent,
    conn: i32,
    _username: String,
    _argb: u32,
    simulate: bool,
    _show_cursor: bool,
) {
    if simulate {
        handle_mouse_simulation_(evt, conn);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let evt_type = evt.mask & MOUSE_TYPE_MASK;
        // 相对（增量）鼠标事件不包含绝对坐标，因此
        // 在相对模式下必须禁用白板/光标渲染，以免出现
        // 错误的光标/白板更新。我们同时检查 is_relative_mouse_active(conn)
        // （连接由于先前的事件已处于相对模式）和 evt_type（当前
        // 事件是相对的），以防标志被设置之前就出现第一个相对事件。
        if _show_cursor && !is_relative_mouse_active(conn) && evt_type != MOUSE_TYPE_MOVE_RELATIVE {
            handle_mouse_show_cursor_(evt, conn, _username, _argb);
        }
    }
}

pub fn handle_mouse_simulation_(evt: &MouseEvent, conn: i32) {
    if !active_mouse_(conn) {
        return;
    }

    if EXITING.load(Ordering::SeqCst) {
        return;
    }

    #[cfg(windows)]
    crate::platform::windows::try_change_desktop();
    let buttons = evt.mask >> 3;
    let evt_type = evt.mask & MOUSE_TYPE_MASK;
    let mut en = ENIGO.lock().unwrap();
    #[cfg(target_os = "macos")]
    en.set_ignore_flags(enigo_ignore_flags());
    #[cfg(not(target_os = "macos"))]
    let mut to_release = Vec::new();
    if evt_type == MOUSE_TYPE_DOWN {
        fix_modifiers(&evt.modifiers[..], &mut en, 0);
        #[cfg(target_os = "macos")]
        en.reset_flag();
        for ref ck in evt.modifiers.iter() {
            if let Some(key) = KEY_MAP.get(&ck.value()) {
                #[cfg(target_os = "macos")]
                en.add_flag(key);
                #[cfg(not(target_os = "macos"))]
                if key != &Key::CapsLock && key != &Key::NumLock {
                    if !get_modifier_state(key.clone(), &mut en) {
                        en.key_down(key.clone()).ok();
                        #[cfg(windows)]
                        modifier_sleep();
                        to_release.push(key);
                    }
                }
            }
        }
    }
    match evt_type {
        MOUSE_TYPE_MOVE => {
            // 切回绝对移动会隐式地禁用相对鼠标模式。
            set_relative_mouse_active(conn, false);
            // 在带 uinput 的 Wayland 上，客户端发送的坐标是按会话初始化时
            // 告知它的布局。如果合成器此后移动了某台显示器，就要把它们
            // 修正到当前布局上。https://github.com/rustdesk/rustdesk/issues/15601
            #[cfg(target_os = "linux")]
            let uinput = wayland_use_uinput();
            #[cfg(target_os = "linux")]
            let (mx, my) = if uinput {
                super::display_service::remap_wayland_uinput_coord(evt.x, evt.y)
            } else {
                (evt.x, evt.y)
            };
            #[cfg(not(target_os = "linux"))]
            let (mx, my) = (evt.x, evt.y);
            en.mouse_move_to(mx, my);
            // 只有 uinput 的那个点处在校准所测量的布局空间中。
            #[cfg(all(target_os = "linux", feature = "drm"))]
            if uinput {
                note_peer_absolute_move(mx, my);
            }
            * LATEST_PEER_INPUT_CURSOR.lock().unwrap() = Input {
                conn,
                time: get_time(),
                x: mx,
                y: my,
            };
        }
        // MOUSE_TYPE_MOVE_RELATIVE：用于游戏/3D 应用的相对鼠标移动。
        // 每个客户端独立决定是否使用相对模式。
        // 多个客户端可以混用绝对和相对移动而互不冲突，
        // 因为服务端只是把增量应用到当前的光标位置上。
        MOUSE_TYPE_MOVE_RELATIVE => {
            set_relative_mouse_active(conn, true);
            // 对增量做钳制，防止极端或恶意数值传到 OS API。
            // 这与 Flutter 客户端的 kMaxRelativeMouseDelta 常量一致。
            const MAX_RELATIVE_MOUSE_DELTA: i32 = 10000;
            let dx = evt
                .x
                .clamp(-MAX_RELATIVE_MOUSE_DELTA, MAX_RELATIVE_MOUSE_DELTA);
            let dy = evt
                .y
                .clamp(-MAX_RELATIVE_MOUSE_DELTA, MAX_RELATIVE_MOUSE_DELTA);
            // 增量一落下来，绝对样本就不再能描述指针了。要在移动
            // 之前就丢掉它，这样中间的帧就不会拿平面去对照它测量。
            #[cfg(all(target_os = "linux", feature = "drm"))]
            note_pointer_moved_without_absolute_sample();
            en.mouse_move_relative(dx, dy);
            // 获取相对移动之后的实际光标位置以便跟踪
            if let Some((x, y)) = crate::get_cursor_pos() {
                * LATEST_PEER_INPUT_CURSOR.lock().unwrap() = Input {
                    conn,
                    time: get_time(),
                    x,
                    y,
                };
            }
        }
        MOUSE_TYPE_DOWN => match buttons {
            MOUSE_BUTTON_LEFT => {
                allow_err!(en.mouse_down(MouseButton::Left));
            }
            MOUSE_BUTTON_RIGHT => {
                allow_err!(en.mouse_down(MouseButton::Right));
            }
            MOUSE_BUTTON_WHEEL => {
                allow_err!(en.mouse_down(MouseButton::Middle));
            }
            MOUSE_BUTTON_BACK => {
                allow_err!(en.mouse_down(MouseButton::Back));
            }
            MOUSE_BUTTON_FORWARD => {
                allow_err!(en.mouse_down(MouseButton::Forward));
            }
            _ => {}
        },
        MOUSE_TYPE_UP => match buttons {
            MOUSE_BUTTON_LEFT => {
                en.mouse_up(MouseButton::Left);
            }
            MOUSE_BUTTON_RIGHT => {
                en.mouse_up(MouseButton::Right);
            }
            MOUSE_BUTTON_WHEEL => {
                en.mouse_up(MouseButton::Middle);
            }
            MOUSE_BUTTON_BACK => {
                en.mouse_up(MouseButton::Back);
            }
            MOUSE_BUTTON_FORWARD => {
                en.mouse_up(MouseButton::Forward);
            }
            _ => {}
        },
        MOUSE_TYPE_WHEEL | MOUSE_TYPE_TRACKPAD => {
            #[allow(unused_mut)]
            let mut x = -evt.x;
            #[allow(unused_mut)]
            let mut y = evt.y;
            #[cfg(not(windows))]
            {
                y = -y;
            }

            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let is_track_pad = evt_type == MOUSE_TYPE_TRACKPAD;

            #[cfg(target_os = "macos")]
            {
                // TODO: support track pad on win.

                // 修复 shift + 滚动（向下/向上）
                if !is_track_pad
                    && evt
                        .modifiers
                        .contains(&EnumOrUnknown::new(ControlKey::Shift))
                {
                    x = y;
                    y = 0;
                }

                if x != 0 {
                    en.mouse_scroll_x(x, is_track_pad);
                }
                if y != 0 {
                    en.mouse_scroll_y(y, is_track_pad);
                }
            }

            #[cfg(windows)]
            if !is_track_pad {
                x *= WHEEL_DELTA as i32;
                y *= WHEEL_DELTA as i32;
            }

            #[cfg(not(target_os = "macos"))]
            {
                if y != 0 {
                    en.mouse_scroll_y(y);
                }
                if x != 0 {
                    en.mouse_scroll_x(x);
                }
            }
        }
        _ => {}
    }
    #[cfg(not(target_os = "macos"))]
    for key in to_release {
        en.key_up(key.clone());
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn handle_mouse_show_cursor_(evt: &MouseEvent, conn: i32, username: String, argb: u32) {
    let buttons = evt.mask >> 3;
    let evt_type = evt.mask & MOUSE_TYPE_MASK;
    match evt_type {
        MOUSE_TYPE_MOVE => {
            whiteboard::update_whiteboard(
                whiteboard::get_key_cursor(conn),
                whiteboard::CustomEvent::Cursor(whiteboard::Cursor {
                    x: evt.x as _,
                    y: evt.y as _,
                    argb,
                    btns: 0,
                    text: username,
                }),
            );
        }
        MOUSE_TYPE_UP => {
            if buttons == MOUSE_BUTTON_LEFT {
                // 有些客户端会有意发送不带坐标的按键事件。
                // 回退到最后已知的光标位置，避免跳到 (0, 0)。
                // TODO(protocol): (0, 0) is a valid screen coordinate. Consider using a dedicated
                // 哨兵值（例如 INVALID_CURSOR_POS）或协议层面的标志，用来区分
                // 「未提供坐标」和「坐标是 (0, 0)」。影响很小，因为
                // 这只影响白板渲染，而恰好点击 (0, 0) 是很少见的。
                let (x, y) = if evt.x == 0 && evt.y == 0 {
                    get_last_input_cursor_pos()
                } else {
                    (evt.x, evt.y)
                };
                whiteboard::update_whiteboard(
                    whiteboard::get_key_cursor(conn),
                    whiteboard::CustomEvent::Cursor(whiteboard::Cursor {
                        x: x as _,
                        y: y as _,
                        argb,
                        btns: buttons,
                        text: username,
                    }),
                );
            }
        }
        _ => {}
    }
}

#[cfg(target_os = "windows")]
fn handle_scale(scale: i32) {
    let mut en = ENIGO.lock().unwrap();
    if scale == 0 {
        en.key_up(Key::Control);
    } else {
        if en.key_down(Key::Control).is_ok() {
            en.mouse_scroll_y(scale);
        }
    }
}

pub fn is_enter(evt: &KeyEvent) -> bool {
    if let Some(key_event::Union::ControlKey(ck)) = evt.union {
        if ck.value() == ControlKey::Return.value() || ck.value() == ControlKey::NumpadEnter.value()
        {
            return true;
        }
    }
    return false;
}

pub async fn lock_screen() {
    cfg_if::cfg_if! {
    if #[cfg(target_os = "linux")] {
        // 不知怎么的，从我们的服务在 Linux 上调用 xdg_screensaver lock 不起作用
        // loginctl lock-session 也不起作用；两者在从命令行运行 rustdesk 时都正常
        std::thread::spawn(|| {
            let mut key_event = KeyEvent::new();

            key_event.set_chr('l' as _);
            key_event.modifiers.push(ControlKey::Meta.into());
            key_event.mode = KeyboardMode::Legacy.into();

            key_event.down = true;
            handle_key(&key_event);

            key_event.down = false;
            handle_key(&key_event);
        });
    } else if #[cfg(target_os = "macos")] {
        // CGSession -suspend 不是真正的锁屏，它是用户切换
        std::thread::spawn(|| {
            let mut key_event = KeyEvent::new();

            key_event.set_chr('q' as _);
            key_event.modifiers.push(ControlKey::Meta.into());
            key_event.modifiers.push(ControlKey::Control.into());
            key_event.mode = KeyboardMode::Legacy.into();

            key_event.down = true;
            handle_key(&key_event);
            key_event.down = false;
            handle_key(&key_event);
        });
    } else {
    crate::platform::lock_screen();
    }
    }
}

#[inline]
#[cfg(target_os = "linux")]
pub fn handle_key(evt: &KeyEvent) {
    handle_key_(evt);
}

#[inline]
#[cfg(target_os = "windows")]
pub fn handle_key(evt: &KeyEvent) {
    crate::portable_service::client::handle_key(evt);
}

#[inline]
#[cfg(target_os = "macos")]
pub fn handle_key(evt: &KeyEvent) {
    // 有 GUI 时，要在主 GUI 线程中运行，否则会崩溃
    let evt = evt.clone();
    QUEUE.exec_async(move || handle_key_(&evt));
    // macOS 上按键必须 sleep。
    // 如果不 sleep，按键按下/释放事件可能不会生效。
    //
    // 例如被控端的 osx `12.7.6` 或 `15.1.1`
    // 如果我们快速连续地输入字符，并且短时间内按下或释放 "Shift"，
    // 有可能在释放 "Shift" 之后，被控端仍然打印出大写字符。
    // 尽管复现起来不太容易。
    key_sleep();
}

#[cfg(target_os = "macos")]
#[inline]
fn reset_input() {
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        VIRTUAL_INPUT_STATE = VirtualInputState::new();
    }
}

#[cfg(target_os = "macos")]
pub fn reset_input_ondisconn() {
    QUEUE.exec_async(reset_input);
}

fn sim_rdev_rawkey_position(code: KeyCode, keydown: bool) {
    #[cfg(target_os = "windows")]
    let rawkey = RawKey::ScanCode(code);
    #[cfg(target_os = "linux")]
    let rawkey = RawKey::LinuxXorgKeycode(code);
    // 待办：测试 android
    // #[cfg(target_os = "android")]
    // let rawkey = RawKey::LinuxConsoleKeycode(code);
    #[cfg(target_os = "macos")]
    let rawkey = RawKey::MacVirtualKeycode(code);

    // map 模式(1)：根据对端平台发送键码。
    record_pressed_key(KeysDown::RdevKey(rawkey), keydown);

    let event_type = if keydown {
        EventType::KeyPress(RdevKey::RawKey(rawkey))
    } else {
        EventType::KeyRelease(RdevKey::RawKey(rawkey))
    };
    simulate_(&event_type);
}

#[cfg(target_os = "windows")]
fn sim_rdev_rawkey_virtual(code: u32, keydown: bool) {
    let rawkey = RawKey::WinVirtualKeycode(code);
    record_pressed_key(KeysDown::RdevKey(rawkey), keydown);
    let event_type = if keydown {
        EventType::KeyPress(RdevKey::RawKey(rawkey))
    } else {
        EventType::KeyRelease(RdevKey::RawKey(rawkey))
    };
    simulate_(&event_type);
}

#[inline]
#[cfg(target_os = "macos")]
fn simulate_(event_type: &EventType) {
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        if let Some(input) = VIRTUAL_INPUT_STATE.as_ref() {
            let _ = input.simulate(&event_type);
        } else if matches!(event_type, EventType::KeyRelease(_)) {
            hbb_common::throttled_log!(
                VirtualInputState::KEY_UP_ERROR_LOG_INTERVAL,
                error,
                "Failed to inject macOS key-up: virtual input is not initialized"
            );
        }
    }
}

#[inline]
#[cfg(target_os = "macos")]
fn press_capslock() {
    let caps_key = RdevKey::RawKey(rdev::RawKey::MacVirtualKeycode(rdev::kVK_CapsLock));
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        if let Some(input) = VIRTUAL_INPUT_STATE.as_mut() {
            if input.simulate(&EventType::KeyPress(caps_key)).is_ok() {
                input.capslock_down = true;
                key_sleep();
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[inline]
fn release_capslock() {
    let caps_key = RdevKey::RawKey(rdev::RawKey::MacVirtualKeycode(rdev::kVK_CapsLock));
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        if let Some(input) = VIRTUAL_INPUT_STATE.as_mut() {
            if input.simulate(&EventType::KeyRelease(caps_key)).is_ok() {
                input.capslock_down = false;
                key_sleep();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[inline]
fn simulate_(event_type: &EventType) {
    match rdev::simulate(&event_type) {
        Ok(()) => (),
        Err(_simulate_error) => {
            log::error!("Could not send {:?}", &event_type);
        }
    }
}

#[inline]
fn control_key_value_to_key(value: i32) -> Option<Key> {
    KEY_MAP.get(&value).and_then(|k| Some(*k))
}

#[inline]
fn char_value_to_key(value: u32) -> Key {
    Key::Layout(std::char::from_u32(value).unwrap_or('\0'))
}

fn map_keyboard_mode(evt: &KeyEvent) {
    #[cfg(windows)]
    crate::platform::windows::try_change_desktop();

    // Wayland
    #[cfg(target_os = "linux")]
    if !crate::platform::linux::is_x11() {
        wayland_send_raw_key(evt.chr() as u16, evt.down);
        return;
    }

    sim_rdev_rawkey_position(evt.chr() as _, evt.down);
}

/// 在 Wayland 上通过当前活动的后端（uinput 或 RemoteDesktop portal）发送原始键码。
/// 期望该键码是 Linux 键码（evdev 编码加上为兼容 X11 而设的 8）。
#[cfg(target_os = "linux")]
#[inline]
fn wayland_send_raw_key(code: u16, down: bool) {
    let mut en = ENIGO.lock().unwrap();
    if down {
        en.key_down(enigo::Key::Raw(code)).ok();
    } else {
        en.key_up(enigo::Key::Raw(code));
    }
}

#[cfg(target_os = "macos")]
fn add_flags_to_enigo(en: &mut Enigo, key_event: &KeyEvent) {
    // 当长按 command 键后再按下并释放
    // Tab 键时，标志中应该带有 CGEventFlagCommand。
    en.reset_flag();
    for ck in key_event.modifiers.iter() {
        if let Some(key) = KEY_MAP.get(&ck.value()) {
            en.add_flag(key);
        }
    }
}

fn get_control_key_value(key_event: &KeyEvent) -> i32 {
    if let Some(key_event::Union::ControlKey(ck)) = key_event.union {
        ck.value()
    } else {
        -1
    }
}

#[inline]
fn has_hotkey_modifiers(key_event: &KeyEvent) -> bool {
    key_event.modifiers.iter().any(|ck| {
        let v = ck.value();
        v == ControlKey::Control.value()
            || v == ControlKey::RControl.value()
            || v == ControlKey::Meta.value()
            || v == ControlKey::RWin.value()
            || {
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                {
                    v == ControlKey::Alt.value() || v == ControlKey::RAlt.value()
                }
                #[cfg(target_os = "macos")]
                {
                    false
                }
            }
    })
}

fn release_unpressed_modifiers(en: &mut Enigo, key_event: &KeyEvent) {
    let ck_value = get_control_key_value(key_event);
    fix_modifiers(&key_event.modifiers[..], en, ck_value);
}

#[cfg(target_os = "linux")]
fn is_altgr_pressed() -> bool {
    let altgr_rawkey = RawKey::LinuxXorgKeycode(ControlKey::RAlt.value() as _);
    KEYS_DOWN
        .lock()
        .unwrap()
        .get(&KeysDown::RdevKey(altgr_rawkey))
        .is_some()
}

#[cfg(not(target_os = "macos"))]
fn press_modifiers(en: &mut Enigo, key_event: &KeyEvent, to_release: &mut Vec<Key>) {
    for ref ck in key_event.modifiers.iter() {
        if let Some(key) = control_key_value_to_key(ck.value()) {
            if !is_pressed(&key, en) {
                #[cfg(target_os = "linux")]
                if key == Key::Alt && is_altgr_pressed() {
                    continue;
                }
                en.key_down(key.clone()).ok();
                to_release.push(key.clone());
                #[cfg(windows)]
                modifier_sleep();
            }
        }
    }
}

fn sync_modifiers(en: &mut Enigo, key_event: &KeyEvent, _to_release: &mut Vec<Key>) {
    #[cfg(target_os = "macos")]
    add_flags_to_enigo(en, key_event);

    if key_event.down {
        release_unpressed_modifiers(en, key_event);
        #[cfg(not(target_os = "macos"))]
        press_modifiers(en, key_event, _to_release);
    }
}

fn process_control_key(en: &mut Enigo, ck: &EnumOrUnknown<ControlKey>, down: bool) {
    if let Some(key) = control_key_value_to_key(ck.value()) {
        if down {
            en.key_down(key).ok();
        } else {
            en.key_up(key);
        }
    }
}

#[inline]
fn need_to_uppercase(en: &mut Enigo) -> bool {
    get_modifier_state(Key::Shift, en) || get_modifier_state(Key::CapsLock, en)
}

fn process_chr(en: &mut Enigo, chr: u32, down: bool, _hotkey: bool) {
    // 在带 uinput 模式的 Wayland 上：
    // - ASCII 可打印字符：通过按键事件输入（自定义键盘路径，例如 portal keysym）
    // - 非 ASCII：通过剪贴板粘贴输入
    #[cfg(target_os = "linux")]
    if !crate::platform::linux::is_x11() && wayland_use_uinput() {
        // 快捷键（按下了 Ctrl/Alt/Meta）时跳过剪贴板
        if !is_hotkey_modifier_pressed(en) {
            if let Ok(c) = char::try_from(chr) {
                if is_ascii_printable(c) {
                    if down {
                        en.key_down(Key::Layout(c)).ok();
                    } else {
                        en.key_up(Key::Layout(c));
                    }
                } else if down {
                    input_char_via_clipboard_server(en, c);
                }
            } else {
                log::warn!(
                    "Ignore invalid unicode scalar in Wayland+uinput path: {}",
                    chr
                );
            }
            return;
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if !_hotkey {
        if down {
            if let Ok(chr) = char::try_from(chr) {
                en.key_sequence(&chr.to_string());
            }
        }
        return;
    }

    let key = char_value_to_key(chr);

    if down {
        if en.key_down(key).is_ok() {
        } else {
            if let Ok(chr) = char::try_from(chr) {
                let mut s = chr.to_string();
                if need_to_uppercase(en) {
                    s = s.to_uppercase();
                }
                en.key_sequence(&s);
            };
        }
    } else {
        en.key_up(key);
    }
}

fn process_unicode(en: &mut Enigo, chr: u32) {
    // 在带 uinput 模式的 Wayland 上：
    // - ASCII 可打印字符：通过按键序列输入（自定义键盘路径）
    // - 非 ASCII：通过剪贴板粘贴输入
    #[cfg(target_os = "linux")]
    if !crate::platform::linux::is_x11() && wayland_use_uinput() {
        if let Ok(c) = char::try_from(chr) {
            if is_ascii_printable(c) {
                en.key_sequence(&c.to_string());
            } else {
                input_char_via_clipboard_server(en, c);
            }
        }
        return;
    }

    if let Ok(chr) = char::try_from(chr) {
        en.key_sequence(&chr.to_string());
    }
}

fn process_seq(en: &mut Enigo, sequence: &str) {
    // 在带 uinput 模式的 Wayland 上：
    // - 纯 ASCII 可打印序列：通过按键序列输入（自定义键盘路径）
    // - 含有任何非 ASCII 字符：通过剪贴板输入整个序列以保持顺序
    #[cfg(target_os = "linux")]
    if !crate::platform::linux::is_x11() && wayland_use_uinput() {
        if sequence.chars().all(is_ascii_printable) {
            en.key_sequence(sequence);
        } else {
            input_text_via_clipboard_server(en, sequence);
        }
        return;
    }

    en.key_sequence(&sequence);
}

/// 在 Wayland 上等待剪贴板同步的延迟（毫秒）。
/// 这是一个经验值——Wayland 没有提供回调或事件来确认
/// 剪贴板内容已被合成器接收。在系统负载很高时，
/// 这个延迟可能不够，但也没有可靠的替代机制。
#[cfg(target_os = "linux")]
const CLIPBOARD_SYNC_DELAY_MS: u64 = 50;
#[cfg(target_os = "linux")]
const WAYLAND_CLIPBOARD_INPUT_FILTER_WINDOW: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const WAYLAND_CLIPBOARD_INPUT_MAX_RECORDS: usize = 256;
#[cfg(target_os = "linux")]
pub(super) const WAYLAND_CLIPBOARD_INPUT_MAX_TEXT_CHARS: usize = 1024;

#[cfg(target_os = "linux")]
fn cleanup_wayland_clipboard_input_records(records: &mut Vec<(Instant, String)>, now: Instant) {
    records.retain(|(created_at, _)| {
        now.saturating_duration_since(*created_at) <= WAYLAND_CLIPBOARD_INPUT_FILTER_WINDOW
    });
    let len = records.len();
    if len > WAYLAND_CLIPBOARD_INPUT_MAX_RECORDS {
        records.drain(0..(len - WAYLAND_CLIPBOARD_INPUT_MAX_RECORDS));
    }
}

#[cfg(target_os = "linux")]
#[inline]
fn normalize_wayland_clipboard_input_text(text: &str) -> String {
    text.chars()
        .take(WAYLAND_CLIPBOARD_INPUT_MAX_TEXT_CHARS)
        .collect()
}

#[cfg(target_os = "linux")]
#[inline]
fn get_wayland_clipboard_input_normalized_text(text: &str) -> Option<String> {
    let normalized = normalize_wayland_clipboard_input_text(text);
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

#[cfg(target_os = "linux")]
#[inline]
fn record_wayland_clipboard_input_for_sync_filter(text: &str) -> Option<(Instant, String)> {
    if text.is_empty() || crate::platform::linux::is_x11() {
        return None;
    }
    let normalized = get_wayland_clipboard_input_normalized_text(text)?;
    let now = Instant::now();
    let mut records = WAYLAND_CLIPBOARD_INPUT_RECORDS.lock().unwrap();
    cleanup_wayland_clipboard_input_records(&mut records, now);
    records.push((now, normalized.clone()));
    Some((now, normalized))
}

#[cfg(target_os = "linux")]
#[inline]
fn rollback_wayland_clipboard_input_record(record: (Instant, String)) {
    let (created_at, normalized) = record;
    let now = Instant::now();
    let mut records = WAYLAND_CLIPBOARD_INPUT_RECORDS.lock().unwrap();
    cleanup_wayland_clipboard_input_records(&mut records, now);
    if let Some(pos) = records
        .iter()
        .rposition(|(record_created_at, record_normalized)| {
            * record_created_at == created_at && *record_normalized == normalized
        })
    {
        records.remove(pos);
    }
}

#[cfg(target_os = "linux")]
pub(super) fn is_recent_wayland_clipboard_input(text: &str) -> bool {
    if text.is_empty() || crate::platform::linux::is_x11() {
        return false;
    }
    let Some(normalized) = get_wayland_clipboard_input_normalized_text(text) else {
        return false;
    };
    let now = Instant::now();
    let mut records = WAYLAND_CLIPBOARD_INPUT_RECORDS.lock().unwrap();
    cleanup_wayland_clipboard_input_records(&mut records, now);
    records
        .iter()
        .any(|(_, record_normalized)| record_normalized == &normalized)
}

/// 内部接口：设置剪贴板内容，不加延迟。
/// 如果剪贴板设置成功则返回 true。
#[cfg(target_os = "linux")]
fn set_clipboard_content(text: &str) -> bool {
    if let Err(e) = crate::clipboard::set_text_clipboard_with_owner_sync(
        text,
        crate::clipboard::ClipboardSide::Host,
    ) {
        log::error!(
            "set_clipboard_content: failed to set clipboard with owner marker: {:?}",
            e
        );
        return false;
    }
    true
}

/// 为粘贴操作设置剪贴板内容（同步版本，用于阻塞上下文）。
///
/// 注意：粘贴后刻意不会恢复原来的剪贴板内容。
/// 恢复剪贴板可能引发竞态，导致后续的按键
/// 意外粘贴出旧的剪贴板内容，而不是预期的输入。
/// 这个取舍把输入的可靠性置于保留剪贴板状态之上。
#[cfg(target_os = "linux")]
#[inline]
pub(super) fn set_clipboard_for_paste_sync(text: &str) -> bool {
    let record = record_wayland_clipboard_input_for_sync_filter(text);
    if !set_clipboard_content(text) {
        if let Some(record) = record {
            rollback_wayland_clipboard_input_record(record);
        }
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(CLIPBOARD_SYNC_DELAY_MS));
    true
}

/// 检查一个字符是否是 ASCII 可打印字符（0x20-0x7E）。
#[cfg(target_os = "linux")]
#[inline]
pub(super) fn is_ascii_printable(c: char) -> bool {
    c as u32 >= 0x20 && c as u32 <= 0x7E
}

/// 在服务端进程中通过剪贴板 + Shift+Insert 输入单个字符。
#[cfg(target_os = "linux")]
#[inline]
fn input_char_via_clipboard_server(en: &mut Enigo, chr: char) {
    input_text_via_clipboard_server(en, &chr.to_string());
}

/// 在服务端进程中通过剪贴板 + Shift+Insert 输入文本。
/// Shift+Insert 比 Ctrl+V 更通用，在 GUI 程序和终端里都能用。
///
/// 注意：粘贴后不会恢复剪贴板内容——原因参见 `set_clipboard_for_paste_sync`。
#[cfg(target_os = "linux")]
fn input_text_via_clipboard_server(en: &mut Enigo, text: &str) {
    if text.is_empty() {
        return;
    }
    if !set_clipboard_for_paste_sync(text) {
        return;
    }

    // 直接使用 ENIGO 的 custom_keyboard，以避免创建新的 IPC 连接
    // 那会导致过多的日志以及键盘设备的反复创建/销毁
    if en.key_down(Key::Shift).is_err() {
        log::error!("input_text_via_clipboard_server: failed to press Shift, skipping paste");
        return;
    }
    if en.key_down(Key::Raw(XKB_KEY_INSERT)).is_err() {
        log::error!("input_text_via_clipboard_server: failed to press Insert, releasing Shift");
        en.key_up(Key::Shift);
        return;
    }
    en.key_up(Key::Raw(XKB_KEY_INSERT));
    en.key_up(Key::Shift);

    // 短暂延迟，让目标应用有时间处理粘贴事件。
    // 经验值——Wayland 上不存在可靠的同步机制。
    std::thread::sleep(std::time::Duration::from_millis(20));
}

#[cfg(not(target_os = "macos"))]
fn release_keys(en: &mut Enigo, to_release: &Vec<Key>) {
    for key in to_release {
        en.key_up(key.clone());
    }
}

fn record_pressed_key(record_key: KeysDown, down: bool) {
    let mut key_down = KEYS_DOWN.lock().unwrap();
    if down {
        key_down.insert(record_key, Instant::now());
    } else {
        key_down.remove(&record_key);
    }
}

fn is_function_key(ck: &EnumOrUnknown<ControlKey>) -> bool {
    let mut res = false;
    if ck.value() == ControlKey::CtrlAltDel.value() {
        // 必须新建线程，因为 send_sas 是 tokio_main，调用方不能是 tokio_main。
        #[cfg(windows)]
        std::thread::spawn(|| {
            allow_err!(send_sas());
        });
        res = true;
    } else if ck.value() == ControlKey::LockScreen.value() {
        std::thread::spawn(|| {
            lock_screen_2();
        });
        res = true;
    }
    return res;
}

/// 检查当前是否按下了某个快捷键修饰键（Ctrl/Alt/Meta）。
/// 用于检测 Ctrl+C、Alt+Tab 等快捷键组合。
///
/// 注意：这里刻意不检查 Shift。Shift+字符会产生一个不同的
/// 字符（例如 Shift+a → 'A'），这是普通的文本输入，而不是快捷键。
/// 只有当 Shift 与 Ctrl/Alt/Meta 组合时，它作为快捷键修饰键才有意义
/// （例如 Ctrl+Shift+Z），而那种情况下这个函数已经通过 Ctrl 返回 true。
#[cfg(target_os = "linux")]
#[inline]
fn is_hotkey_modifier_pressed(en: &mut Enigo) -> bool {
    get_modifier_state(Key::Control, en)
        || get_modifier_state(Key::RightControl, en)
        || get_modifier_state(Key::Alt, en)
        || get_modifier_state(Key::RightAlt, en)
        || get_modifier_state(Key::Meta, en)
        || get_modifier_state(Key::RWin, en)
}

/// 在 Legacy/Translate 模式下输入字符之前先释放 Shift 键。
/// 在这些模式下，字符已经由客户端转换过了，
/// 因此我们应当直接输入它，不让 Shift 修饰键影响结果。
///
/// 注意：如果按下了快捷键修饰键（Ctrl/Alt/Meta），则不会释放 Shift，
/// 以保留 Ctrl+Shift+Z 这样的组合。
#[cfg(target_os = "linux")]
fn release_shift_for_char_input(en: &mut Enigo) {
    // 如果按下了快捷键修饰键（Ctrl/Alt/Meta），不要释放 Shift。
    // 这样可以保留 Ctrl+Shift+Z 这样的组合。
    if is_hotkey_modifier_pressed(en) {
        return;
    }

    // 在 translate 模式下，客户端已经把按键转换成了字符
    // （例如 Shift+a → 'A'）。我们在这里释放 Shift，好让服务端直接输入该字符
    // 而不受 Shift 影响。
    //
    // 输入后刻意不会恢复 Shift——客户端会在用户物理释放 Shift 时
    // 发送明确的 Shift key_up 事件。在这里恢复它会导致
    // Shift 被短暂地重新按下，从而干扰下一个输入事件。

    let is_x11 = crate::platform::linux::is_x11();

    if get_modifier_state(Key::Shift, en) {
        if !is_x11 {
            en.key_up(Key::Shift);
        } else {
            simulate_(&EventType::KeyRelease(RdevKey::ShiftLeft));
        }
    }
    if get_modifier_state(Key::RightShift, en) {
        if !is_x11 {
            en.key_up(Key::RightShift);
        } else {
            simulate_(&EventType::KeyRelease(RdevKey::ShiftRight));
        }
    }
}

fn legacy_keyboard_mode(evt: &KeyEvent) {
    #[cfg(windows)]
    crate::platform::windows::try_change_desktop();
    let mut to_release: Vec<Key> = Vec::new();

    let mut en = ENIGO.lock().unwrap();
    sync_modifiers(&mut en, &evt, &mut to_release);

    let down = evt.down;
    match evt.union {
        Some(key_event::Union::ControlKey(ck)) => {
            if is_function_key(&ck) {
                return;
            }
            let record_key = ck.value() as u64;
            record_pressed_key(KeysDown::EnigoKey(record_key), down);
            process_control_key(&mut en, &ck, down)
        }
        Some(key_event::Union::Chr(chr)) => {
            // 在 Legacy 模式下输入字符时，需要先释放 Shift。
            // 字符已经由客户端转换过了，因此我们应当
            // 直接输入它，不让 Shift 修饰键影响结果。
            // 只应为 Ctrl+C 这样的快捷键保留 Ctrl/Alt/Meta。
            #[cfg(target_os = "linux")]
            release_shift_for_char_input(&mut en);

            let record_key = chr as u64 + KEY_CHAR_START;
            record_pressed_key(KeysDown::EnigoKey(record_key), down);
            process_chr(&mut en, chr, down, has_hotkey_modifiers(evt))
        }
        Some(key_event::Union::Unicode(chr)) => {
            // 与 Chr 相同：为 Unicode 输入释放 Shift
            #[cfg(target_os = "linux")]
            release_shift_for_char_input(&mut en);

            process_unicode(&mut en, chr)
        }
        Some(key_event::Union::Seq(ref seq)) => process_seq(&mut en, seq),
        _ => {}
    }

    #[cfg(not(target_os = "macos"))]
    release_keys(&mut en, &to_release);
}

#[cfg(target_os = "windows")]
fn translate_process_code(code: u32, down: bool) {
    crate::platform::windows::try_change_desktop();
    match code >> 16 {
        0 => sim_rdev_rawkey_position(code as _, down),
        vk_code => sim_rdev_rawkey_virtual(vk_code, down),
    };
}

fn translate_keyboard_mode(evt: &KeyEvent) {
    match &evt.union {
        Some(key_event::Union::Seq(seq)) => {
            // 在 Wayland 上：
            // - uinput 模式（--service）：把剪贴板处理留在本进程中，因为
            //   在 root 服务上下文中剪贴板并不可靠。
            // - rdp_input 模式（--server）：把序列转发给自定义键盘处理器，这样
            //   ASCII 可以用 Portal keysym，非 ASCII 可以用剪贴板。
            #[cfg(target_os = "linux")]
            if !crate::platform::linux::is_x11() {
                let mut en = ENIGO.lock().unwrap();
                if wayland_use_rdp_input() {
                    release_shift_for_char_input(&mut en);
                    en.key_sequence(seq);
                    return;
                }

                if wayland_use_uinput() {
                    // 检查这是否是快捷键（按下了 Ctrl/Alt/Meta）
                    // 对于快捷键，我们通过 Enigo 发送基于字符的按键事件，而不是
                    // 使用剪贴板。这依赖于本地键盘布局来
                    // 把字符映射到物理按键。
                    // 这假设客户端和服务端使用相同的键盘布局（常见情况）。
                    // 注意：对于非拉丁键盘（例如阿拉伯语键盘），如果字符无法通过 KEY_MAP_LAYOUT 映射到某个按键，快捷键可能无法
                    // 正常工作。
                    // 这是一个已知的限制——最常用的快捷键（Ctrl+A/C/V/Z）使用的是拉丁
                    // 字符，在大多数键盘布局上都能映射。
                    if is_hotkey_modifier_pressed(&mut en) {
                        // 对于快捷键，通过 Enigo 发送基于字符的按键事件。
                        // 这依赖于本地键盘布局映射（KEY_MAP_LAYOUT）。
                        for chr in seq.chars() {
                            if !is_ascii_printable(chr) {
                                log::warn!(
                                    "Hotkey with non-ASCII character may not work correctly on non-Latin keyboard layouts"
                                );
                            }
                            en.key_click(Key::Layout(chr));
                        }
                        return;
                    }

                    // 普通文本输入：释放 Shift 并使用剪贴板
                    release_shift_for_char_input(&mut en);
                    if seq.chars().all(is_ascii_printable) {
                        en.key_sequence(seq);
                    } else {
                        input_text_via_clipboard_server(&mut en, seq);
                    }
                    return;
                }
            }

            // Fr -> US
            // 客户端：Shift + & => 1（发送到远端）
            // 远端：Shift + 1 => "!"
            //
            // 先尝试释放 shift。
            // 远端：Shift + 1 => 1
            let mut en = ENIGO.lock().unwrap();

            #[cfg(target_os = "macos")]
            en.key_sequence(seq);
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                #[cfg(target_os = "windows")]
                let simulate_win_hot_key = is_hot_key_modifiers_down(&mut en);
                #[cfg(target_os = "linux")]
                let simulate_win_hot_key = false;
                if !simulate_win_hot_key {
                    #[cfg(target_os = "linux")]
                    release_shift_for_char_input(&mut en);
                    #[cfg(target_os = "windows")]
                    {
                        if get_modifier_state(Key::Shift, &mut en) {
                            simulate_(&EventType::KeyRelease(RdevKey::ShiftLeft));
                        }
                        if get_modifier_state(Key::RightShift, &mut en) {
                            simulate_(&EventType::KeyRelease(RdevKey::ShiftRight));
                        }
                    }
                }
                for chr in seq.chars() {
                    // rust 中的 char 是 4 字节。
                    // 但这里 char 来自键盘，我们只需要 2 字节。
                    #[cfg(target_os = "windows")]
                    if simulate_win_hot_key {
                        rdev::simulate_char(chr, true).ok();
                    } else {
                        rdev::simulate_unicode(chr as _).ok();
                    }
                    #[cfg(target_os = "linux")]
                    en.key_click(Key::Layout(chr));
                }
            }
        }
        Some(key_event::Union::Chr(..)) => {
            #[cfg(target_os = "windows")]
            translate_process_code(evt.chr(), evt.down);
            #[cfg(target_os = "linux")]
            {
                if !crate::platform::linux::is_x11() {
                    // Wayland：用 uinput 发送原始键码
                    wayland_send_raw_key(evt.chr() as u16, evt.down);
                } else {
                    sim_rdev_rawkey_position(evt.chr() as _, evt.down);
                }
            }
            #[cfg(target_os = "macos")]
            sim_rdev_rawkey_position(evt.chr() as _, evt.down);
        }
        Some(key_event::Union::Unicode(..)) => {
            // 暂不处理 unicode。
        }
        #[cfg(target_os = "windows")]
        Some(key_event::Union::Win2winHotkey(code)) => {
            simulate_win2win_hotkey(*code, evt.down);
        }
        _ => {
            log::debug!(
                "Unreachable. Unexpected key event (mode={:?}, down={:?})",
                &evt.mode,
                &evt.down
            );
        }
    }
}

#[inline]
#[cfg(target_os = "windows")]
fn is_hot_key_modifiers_down(en: &mut Enigo) -> bool {
    en.get_key_state(Key::Control)
        || en.get_key_state(Key::RightControl)
        || en.get_key_state(Key::Alt)
        || en.get_key_state(Key::RightAlt)
        || en.get_key_state(Key::Meta)
        || en.get_key_state(Key::RWin)
}

#[cfg(target_os = "windows")]
fn simulate_win2win_hotkey(code: u32, down: bool) {
    let unicode: u16 = (code & 0x0000FFFF) as u16;
    if down {
        if rdev::simulate_key_unicode(unicode, false).is_ok() {
            return;
        }
    }

    let keycode: u16 = ((code >> 16) & 0x0000FFFF) as u16;
    let scan = rdev::vk_to_scancode(keycode as _);
    allow_err!(rdev::simulate_code(None, Some(scan), down));
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn skip_led_sync_control_key(_key: &ControlKey) -> bool {
    false
}

// 当只是按下并释放单个 meta 时，不应创建 LockModesHandler。
// 因为 drop 函数可能插入 "CapsLock Click" 和 "NumLock Click"，这会破坏单独的 meta 点击。
// https://github.com/rustdesk/rustdesk/issues/3928#issuecomment-1496936687
// https://github.com/rustdesk/rustdesk/issues/3928#issuecomment-1500415822
// https://github.com/rustdesk/rustdesk/issues/3928#issuecomment-1500773473
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn skip_led_sync_control_key(key: &ControlKey) -> bool {
    matches!(
        key,
        ControlKey::Control
            | ControlKey::RControl
            | ControlKey::Meta
            | ControlKey::Shift
            | ControlKey::RShift
            | ControlKey::Alt
            | ControlKey::RAlt
            | ControlKey::Tab
            | ControlKey::Return
    )
}

#[inline]
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn is_numpad_control_key(key: &ControlKey) -> bool {
    matches!(
        key,
        ControlKey::Numpad0
            | ControlKey::Numpad1
            | ControlKey::Numpad2
            | ControlKey::Numpad3
            | ControlKey::Numpad4
            | ControlKey::Numpad5
            | ControlKey::Numpad6
            | ControlKey::Numpad7
            | ControlKey::Numpad8
            | ControlKey::Numpad9
            | ControlKey::NumpadEnter
    )
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn skip_led_sync_rdev_key(_key: &RdevKey) -> bool {
    false
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn skip_led_sync_rdev_key(key: &RdevKey) -> bool {
    matches!(
        key,
        RdevKey::ControlLeft
            | RdevKey::ControlRight
            | RdevKey::MetaLeft
            | RdevKey::MetaRight
            | RdevKey::ShiftLeft
            | RdevKey::ShiftRight
            | RdevKey::Alt
            | RdevKey::AltGr
            | RdevKey::Tab
            | RdevKey::Return
    )
}

#[inline]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_legacy_mode(evt: &KeyEvent) -> bool {
    evt.mode.enum_value_or(KeyboardMode::Legacy) == KeyboardMode::Legacy
}

pub fn handle_key_(evt: &KeyEvent) {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }

    #[cfg(target_os = "linux")]
    super::uinput::prepare_layout(evt);

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let mut _lock_mode_handler = None;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    match &evt.union {
        Some(key_event::Union::Unicode(..)) | Some(key_event::Union::Seq(..)) => {
            _lock_mode_handler = Some(LockModesHandler::new_handler(&evt, false));
        }
        Some(key_event::Union::ControlKey(ck)) => {
            let key = ck.enum_value_or(ControlKey::Unknown);
            if !skip_led_sync_control_key(&key) {
                #[cfg(target_os = "macos")]
                let is_numpad_key = false;
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                let is_numpad_key = is_numpad_control_key(&key);
                _lock_mode_handler = Some(LockModesHandler::new_handler(&evt, is_numpad_key));
            }
        }
        Some(key_event::Union::Chr(code)) => {
            if is_legacy_mode(&evt) {
                _lock_mode_handler = Some(LockModesHandler::new_handler(evt, false));
            } else {
                let key = crate::keyboard::keycode_to_rdev_key(*code);
                if !skip_led_sync_rdev_key(&key) {
                    #[cfg(target_os = "macos")]
                    let is_numpad_key = false;
                    #[cfg(any(target_os = "windows", target_os = "linux"))]
                    let is_numpad_key = crate::keyboard::is_numpad_rdev_key(&key);
                    _lock_mode_handler = Some(LockModesHandler::new_handler(evt, is_numpad_key));
                }
            }
        }
        _ => {}
    }

    match evt.mode.enum_value() {
        Ok(KeyboardMode::Map) => {
            #[cfg(target_os = "macos")]
            set_last_legacy_mode(false);
            map_keyboard_mode(evt);
        }
        Ok(KeyboardMode::Translate) => {
            #[cfg(target_os = "macos")]
            set_last_legacy_mode(false);
            translate_keyboard_mode(evt);
        }
        _ => {
            // 所有按键按下事件都从这里开始，
            // 因此我们可以在这里重置上一个 legacy 模式的标志。
            #[cfg(target_os = "macos")]
            set_last_legacy_mode(true);
            legacy_keyboard_mode(evt);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn lock_screen_2() {
    lock_screen().await;
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn send_sas() -> ResultType<()> {
    if crate::platform::is_physical_console_session().unwrap_or(true) {
        let mut stream = crate::ipc::connect(1000, crate::POSTFIX_SERVICE).await?;
        timeout(1000, stream.send(&crate::ipc::Data::SAS)).await??;
    } else {
        crate::platform::send_sas();
    };
    Ok(())
}

#[inline]
#[cfg(target_os = "linux")]
pub fn wayland_use_uinput() -> bool {
    !crate::platform::is_x11() && crate::is_server()
}

#[inline]
#[cfg(target_os = "linux")]
pub fn wayland_use_rdp_input() -> bool {
    !crate::platform::is_x11() && !crate::is_server()
}

#[cfg(target_os = "linux")]
pub struct TemporaryMouseMoveHandle {
    thread_handle: Option<std::thread::JoinHandle<()>>,
    tx: Option<mpsc::Sender<(i32, i32)>>,
}

#[cfg(target_os = "linux")]
impl TemporaryMouseMoveHandle {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<(i32, i32)>();
        let thread_handle = std::thread::spawn(move || {
            log::debug!("TemporaryMouseMoveHandle thread started");
            for (x, y) in rx {
                // 不是对端的位置：校准不能从它做减法。
                #[cfg(feature = "drm")]
                forget_sample_and_move(|| ENIGO.lock().unwrap(), |mut en| en.mouse_move_to(x, y));
                #[cfg(not(feature = "drm"))]
                ENIGO.lock().unwrap().mouse_move_to(x, y);
            }
            log::debug!("TemporaryMouseMoveHandle thread exiting");
        });
        TemporaryMouseMoveHandle {
            thread_handle: Some(thread_handle),
            tx: Some(tx),
        }
    }

    pub fn move_mouse_to(&self, x: i32, y: i32) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((x, y));
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for TemporaryMouseMoveHandle {
    fn drop(&mut self) {
        log::debug!("Dropping TemporaryMouseMoveHandle");
        // 关闭通道以通知线程退出。
        self.tx.take();
        // 等待线程结束。
        if let Some(thread_handle) = self.thread_handle.take() {
            if let Err(e) = thread_handle.join() {
                log::error!("Error joining TemporaryMouseMoveHandle thread: {:?}", e);
            }
        }
    }
}

lazy_static::lazy_static! {
    static ref MODIFIER_MAP: HashMap<i32, Key> = [
        (ControlKey::Alt, Key::Alt),
        (ControlKey::RAlt, Key::RightAlt),
        (ControlKey::Control, Key::Control),
        (ControlKey::RControl, Key::RightControl),
        (ControlKey::Shift, Key::Shift),
        (ControlKey::RShift, Key::RightShift),
        (ControlKey::Meta, Key::Meta),
        (ControlKey::RWin, Key::RWin),
    ].iter().map(|(a, b)| (a.value(), b.clone())).collect();
    static ref KEY_MAP: HashMap<i32, Key> =
    [
        (ControlKey::Alt, Key::Alt),
        (ControlKey::Backspace, Key::Backspace),
        (ControlKey::CapsLock, Key::CapsLock),
        (ControlKey::Control, Key::Control),
        (ControlKey::Delete, Key::Delete),
        (ControlKey::DownArrow, Key::DownArrow),
        (ControlKey::End, Key::End),
        (ControlKey::Escape, Key::Escape),
        (ControlKey::F1, Key::F1),
        (ControlKey::F10, Key::F10),
        (ControlKey::F11, Key::F11),
        (ControlKey::F12, Key::F12),
        (ControlKey::F2, Key::F2),
        (ControlKey::F3, Key::F3),
        (ControlKey::F4, Key::F4),
        (ControlKey::F5, Key::F5),
        (ControlKey::F6, Key::F6),
        (ControlKey::F7, Key::F7),
        (ControlKey::F8, Key::F8),
        (ControlKey::F9, Key::F9),
        (ControlKey::Home, Key::Home),
        (ControlKey::LeftArrow, Key::LeftArrow),
        (ControlKey::Meta, Key::Meta),
        (ControlKey::Option, Key::Option),
        (ControlKey::PageDown, Key::PageDown),
        (ControlKey::PageUp, Key::PageUp),
        (ControlKey::Return, Key::Return),
        (ControlKey::RightArrow, Key::RightArrow),
        (ControlKey::Shift, Key::Shift),
        (ControlKey::Space, Key::Space),
        (ControlKey::Tab, Key::Tab),
        (ControlKey::UpArrow, Key::UpArrow),
        (ControlKey::Numpad0, Key::Numpad0),
        (ControlKey::Numpad1, Key::Numpad1),
        (ControlKey::Numpad2, Key::Numpad2),
        (ControlKey::Numpad3, Key::Numpad3),
        (ControlKey::Numpad4, Key::Numpad4),
        (ControlKey::Numpad5, Key::Numpad5),
        (ControlKey::Numpad6, Key::Numpad6),
        (ControlKey::Numpad7, Key::Numpad7),
        (ControlKey::Numpad8, Key::Numpad8),
        (ControlKey::Numpad9, Key::Numpad9),
        (ControlKey::Cancel, Key::Cancel),
        (ControlKey::Clear, Key::Clear),
        (ControlKey::Menu, Key::Alt),
        (ControlKey::Pause, Key::Pause),
        (ControlKey::Kana, Key::Kana),
        (ControlKey::Hangul, Key::Hangul),
        (ControlKey::Junja, Key::Junja),
        (ControlKey::Final, Key::Final),
        (ControlKey::Hanja, Key::Hanja),
        (ControlKey::Kanji, Key::Kanji),
        (ControlKey::Convert, Key::Convert),
        (ControlKey::Select, Key::Select),
        (ControlKey::Print, Key::Print),
        (ControlKey::Execute, Key::Execute),
        (ControlKey::Snapshot, Key::Snapshot),
        (ControlKey::Insert, Key::Insert),
        (ControlKey::Help, Key::Help),
        (ControlKey::Sleep, Key::Sleep),
        (ControlKey::Separator, Key::Separator),
        (ControlKey::Scroll, Key::Scroll),
        (ControlKey::NumLock, Key::NumLock),
        (ControlKey::RWin, Key::RWin),
        (ControlKey::Apps, Key::Apps),
        (ControlKey::Multiply, Key::Multiply),
        (ControlKey::Add, Key::Add),
        (ControlKey::Subtract, Key::Subtract),
        (ControlKey::Decimal, Key::Decimal),
        (ControlKey::Divide, Key::Divide),
        (ControlKey::Equals, Key::Equals),
        (ControlKey::NumpadEnter, Key::NumpadEnter),
        (ControlKey::RAlt, Key::RightAlt),
        (ControlKey::RControl, Key::RightControl),
        (ControlKey::RShift, Key::RightShift),
    ].iter().map(|(a, b)| (a.value(), b.clone())).collect();
    static ref NUMPAD_KEY_MAP: HashMap<i32, bool> =
    [
        (ControlKey::Home, true),
        (ControlKey::UpArrow, true),
        (ControlKey::PageUp, true),
        (ControlKey::LeftArrow, true),
        (ControlKey::RightArrow, true),
        (ControlKey::End, true),
        (ControlKey::DownArrow, true),
        (ControlKey::PageDown, true),
        (ControlKey::Insert, true),
        (ControlKey::Delete, true),
    ].iter().map(|(a, b)| (a.value(), b.clone())).collect();
}

#[cfg(test)]
mod cursor_shape_tests {
    use super::*;

    fn shape(id: u64) -> Arc<Message> {
        let mut msg = Message::new();
        msg.set_cursor_data(CursorData {
            id,
            ..Default::default()
        });
        Arc::new(msg)
    }

    // 只写一个测试，因为这些形状是进程级保存的。
    #[test]
    fn a_shape_is_kept_once_and_can_be_asked_for_again() {
        let first = shared_cursor_shape(shape(u64::MAX - 1));
        let again = shared_cursor_shape(shape(u64::MAX - 1));
        assert!(Arc::ptr_eq(&first, &again), "a second handle shares it");
        let asked = cursor_data_message(u64::MAX - 1).expect("a sent shape is kept");
        assert!(Arc::ptr_eq(&asked, &first));
        assert!(cursor_data_message(u64::MAX - 2).is_none());

        let raw = |handle| CursorData {
            id: handle,
            width: 4,
            height: 4,
            colors: vec![9u8; 4 * 4 * 4].into(),
            ..Default::default()
        };
        let mut compressed = 0;
        let mut compress = |rgba: &[u8]| {
            compressed += 1;
            hbb_common::compress::compress(rgba)
        };
        let shown = cursor_shape_message(raw(1), &mut compress);
        let again = cursor_shape_message(raw(2), &mut compress);
        assert!(Arc::ptr_eq(&shown, &again), "a new handle, the same shape");
        assert_eq!(compressed, 1, "a shape sent before is not compressed again");

        let sending = shared_cursor_shape(shape(u64::MAX - 3));
        let mut few = HashMap::from([(1, shown.clone())]);
        limit_cursor_handles(&mut few, &sending);
        assert_eq!(few.len(), 1, "below the ceiling nothing goes");

        let mut handles: HashMap<u64, Arc<Message>> = (0..CURSOR_HANDLES_MAX as u64)
            .map(|handle| (handle, shown.clone()))
            .collect();
        limit_cursor_handles(&mut handles, &sending);
        assert!(
            handles.is_empty(),
            "past the ceiling the handles start over"
        );
        let Some(message::Union::CursorData(cd)) = &shown.union else {
            panic!("a cursor shape");
        };
        assert!(
            cursor_data_message(cd.id).is_none(),
            "and the shapes with them"
        );
        assert!(
            cursor_data_message(u64::MAX - 3).is_some(),
            "but the shape being sent is kept"
        );

        let big = |id| {
            let mut msg = Message::new();
            msg.set_cursor_data(CursorData {
                id,
                colors: vec![0u8; CURSOR_SHAPES_BYTES_MAX / 2 + 1].into(),
                ..Default::default()
            });
            Arc::new(msg)
        };
        shared_cursor_shape(big(u64::MAX - 4));
        let sending = shared_cursor_shape(big(u64::MAX - 5));
        let mut few = HashMap::from([(1, shown.clone())]);
        limit_cursor_handles(&mut few, &sending);
        assert!(
            few.is_empty(),
            "past the byte ceiling the handles start over"
        );
        assert!(cursor_data_message(u64::MAX - 4).is_none());
        assert!(cursor_data_message(u64::MAX - 5).is_some());

        super::super::service::Reset::reset(&mut StateCursor::default());
        assert!(
            cursor_data_message(u64::MAX - 1).is_none(),
            "a reset forgets them"
        );
    }
}

#[cfg(all(test, target_os = "linux", feature = "drm"))]
mod peer_abs_sample_tests {
    use super::*;

    #[test]
    fn a_move_without_an_absolute_sample_forgets_the_last_one() {
        note_peer_absolute_move(20, 30);
        let s = last_peer_abs_sample().expect("an absolute move leaves a sample");
        assert_eq!(s.pos, (20, 30));
        assert!(s.age_ms < 1_000);
        note_pointer_moved_without_absolute_sample();
        assert_eq!(last_peer_abs_sample(), None);
    }
}
