#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dock;
use dock::{Area, Dock, Side};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use foxes_core::aggregate::session_detail;
use foxes_core::history::History;
use foxes_core::intake::Intake;
use foxes_core::timeutil::now_ms;
use foxes_core::{analyze, Analysis, Roots, Snapshot, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Response;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    App, AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, RunEvent, Webview, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, Window, WindowEvent, Wry,
};
use tauri_plugin_autostart::ManagerExt;

const MAIN: &str = "main";
const ISLAND: &str = "island";
const TRAY: &str = "tray";

/// exe 旁边的数据目录。早期的便携版把设置存在这里（现在只兼容着读）；便携运行时用量历史在这里再放一份，跟着程序换电脑。
const DATA_DIR: &str = "Foxes-watch-data";

/// 灵动岛保持固定宽度以便居中；窗口高度随内容变化。可点范围由 island_region 指定。
/// 要和 src/island.tsx、src/styles/island.css 对上。
const ISLAND_WIDTH: f64 = 372.0;
const ISLAND_HEIGHT: f64 = 680.0;
/// 界面量出实际尺寸之前先用的范围：大约一个胶囊
const ISLAND_START: (f64, f64) = (110.0, 45.0);
/// 窗口顶边离屏幕工作区顶边的距离
const ISLAND_TOP: f64 = 4.0;

const MAIN_WIDTH: f64 = 1200.0;
const MAIN_HEIGHT: f64 = 780.0;

const REFRESH_EVERY: Duration = Duration::from_millis(1000);
/// 数据没变化时也隔这么久重算一次，让“运行中 / 已结束”、近一分钟的用量这类随时间变化的数能更新
const REPUBLISH_EVERY: Duration = Duration::from_secs(5);
/// 面板没露面、灵动岛也没展开时，数据有变化也至少隔这么久才重新汇总：胶囊上的数字晚两三秒不要紧，汇总却是最费处理器的一步
const QUIET_ANALYZE_EVERY: Duration = Duration::from_millis(2800);
/// 隔这么久重新找一遍各 agent 的数据目录：运行期间新装的 agent 不用重启就能发现
const RECHECK_EVERY: Duration = Duration::from_secs(15);
/// 灵动岛展开期间，隔这么久看一眼鼠标还在不在岛上
const ISLAND_WATCH_EVERY: Duration = Duration::from_millis(150);
/// 隔这么久检查一次灵动岛有没有被挤出置顶层
const ISLAND_RAISE_EVERY: Duration = Duration::from_secs(1);
/// 启动后这么久之内不让页面停止绘制：等它先把第一帧画出来，窗口以后露面时才有现成的画面可用
const PAGE_WARMUP: Duration = Duration::from_secs(30);
/// 有页面的两个窗口；顺序和 [`Shared::page_shown`] 对应
const PAGES: [&str; 2] = [MAIN, ISLAND];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    island: bool,
    autostart: bool,
    notify_finished: bool,
    edge_auto_hide: bool,
    units: String,
    /// dark、light，或 system（跟随系统的深浅色）
    theme: String,
    /// 少用动画：不跑脉冲和过渡，省处理器
    reduce_motion: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            island: true,
            autostart: false,
            notify_finished: true,
            edge_auto_hide: true,
            units: "kmb".into(),
            theme: "dark".into(),
            reduce_motion: false,
        }
    }
}

impl Settings {
    fn sanitize(&mut self) {
        if self.units != "wanyi" {
            self.units = "kmb".into();
        }
        if !matches!(self.theme.as_str(), "light" | "system") {
            self.theme = "dark".into();
        }
    }
}

/// 写进 settings.json 的内容
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Persisted {
    settings: Settings,
    /// 用户把灵动岛拖到的位置（物理像素）；没拖过就是空，用默认位置
    island_pos: Option<(i32, i32)>,
    dock: Option<Dock>,
    revision: u64,
}

struct TrayItems {
    island: CheckMenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
}

struct Shared {
    /// 设置文件的候选位置，靠前的优先
    config_paths: Vec<PathBuf>,
    config_path: Mutex<Option<PathBuf>>,
    persisted: Mutex<Persisted>,
    /// 设置有改动、还没写盘
    dirty: AtomicBool,
    analysis: Mutex<Option<Arc<Analysis>>>,
    /// 用量历史。监控线程每轮同步时用；退出时再写一次盘
    history: Mutex<History>,
    /// 各 agent 的数据目录，顺序：claude、codex、deepseek、antigravity
    roots: Mutex<Vec<Vec<String>>>,
    tray_items: Mutex<Option<TrayItems>>,
    tooltip: Mutex<String>,
    dragging: AtomicBool,
    /// 界面要求的可见范围（逻辑像素：宽、高）
    island_region: Mutex<(f64, f64)>,
    /// 实际生效的可见范围（物理像素，相对窗口左上角：左、上、右、下）
    island_rect: Mutex<(i32, i32, i32, i32)>,
    /// 灵动岛窗口的系统句柄；0 表示还没有
    island_hwnd: AtomicIsize,
    island_expanded: AtomicBool,
    /// 程序启动的时刻
    started: Instant,
    /// 各窗口（顺序同 [`PAGES`]）里的页面现在是否在绘制，见 [`sync_page`]
    page_shown: [AtomicBool; 2],
    /// 监控线程出错后自动重来的次数，和最近一次出错的位置、原因
    monitor_restarts: AtomicU32,
    monitor_error: Mutex<Option<String>>,
    /// 向监控线程要对话明细的通道（线程重来时换新的）
    detail_tx: Mutex<Option<mpsc::Sender<DetailRequest>>>,
}

/// 界面要某段对话的明细时发给监控线程的请求：对话标识，和回话用的通道
type DetailRequest = (String, mpsc::Sender<Option<String>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: String,
    roots: Vec<Vec<String>>,
    /// 设置文件所在的位置
    settings_path: String,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// —— 设置 ——

/// 默认写入当前用户的配置目录。旧版便携配置参与加载，用户目录不可写时也可回退。
fn config_candidates(app: &AppHandle) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(dir) = app.path().app_config_dir() {
        paths.push(dir.join("settings.json"));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        paths.push(dir.join(DATA_DIR).join("settings.json"));
    }
    paths
}

fn load_persisted(paths: &[PathBuf]) -> (Persisted, Option<PathBuf>) {
    // 老版本没有 revision，按修改时间选择；相同时间优先首选目录。
    let latest = paths.iter().rev().filter_map(|path| {
        let data: Persisted = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let modified = fs::metadata(path).ok()?.modified().ok();
        Some((data, modified, path))
    }).max_by_key(|(data, modified, _)| (data.revision, *modified));
    let Some((mut data, _, path)) = latest else { return (Persisted::default(), paths.first().cloned()) };
    data.settings.sanitize();
    (data, Some(path.clone()))
}

fn write_replacing(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text)?;
    fs::rename(&temp, path)
}

fn save_persisted(shared: &Shared) {
    // 串行化快照与替换，避免拖动位置和设置同时写入同一临时文件。
    let mut data = lock(&shared.persisted);
    data.revision = data.revision.saturating_add(1);
    let Ok(text) = serde_json::to_string_pretty(&*data) else { return };
    for path in &shared.config_paths {
        if write_replacing(path, &text).is_ok() {
            *lock(&shared.config_path) = Some(path.clone());
            return;
        }
    }
    shared.dirty.store(true, Ordering::Relaxed);
}

fn current_settings(app: &AppHandle) -> Settings {
    lock(&app.state::<Shared>().persisted).settings.clone()
}

/// 把界面或托盘菜单传来的改动合并进设置，并让改动生效。
fn apply_patch(app: &AppHandle, patch: &Value) -> Settings {
    let shared = app.state::<Shared>();
    let before = current_settings(app);

    let mut merged = serde_json::to_value(&before).unwrap_or(Value::Null);
    if let (Some(target), Some(source)) = (merged.as_object_mut(), patch.as_object()) {
        for (key, value) in source {
            if target.contains_key(key) {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    let mut next: Settings = serde_json::from_value(merged).unwrap_or_else(|_| before.clone());
    next.sanitize();

    if next.autostart != before.autostart {
        let manager = app.autolaunch();
        let _ = if next.autostart { manager.enable() } else { manager.disable() };
        next.autostart = manager.is_enabled().unwrap_or(false);
    }

    lock(&shared.persisted).settings = next.clone();
    save_persisted(&shared);

    if next.island != before.island {
        show_island(app, next.island);
    }
    if let Some(items) = lock(&shared.tray_items).as_ref() {
        let _ = items.island.set_checked(next.island);
        let _ = items.autostart.set_checked(next.autostart);
    }
    let _ = app.emit("settings", &next);
    next
}

// —— 窗口 ——

fn push_latest(app: &AppHandle, label: &str) {
    let latest = lock(&app.state::<Shared>().analysis).clone();
    if let Some(analysis) = latest {
        let _ = app.emit_to(label, "snapshot", &analysis.snapshot);
    }
}

/// 让窗口里的页面停止或恢复绘制，返回状态有没有变。
///
/// 网页组件察觉不到外层窗口已经藏起来或最小化了：不告诉它，页面照样每秒重画几十次，白占处理器和显卡。
/// 停下来之后页面的 `document.hidden` 为真，动画和逐帧回调都不再跑。只在主线程上调用。
fn set_page_shown(window: &WebviewWindow, shown: bool) -> bool {
    let Some(slot) = PAGES.iter().position(|label| *label == window.label()) else { return false };
    if window.state::<Shared>().page_shown[slot].swap(shown, Ordering::Relaxed) == shown {
        return false;
    }
    let page: &Webview = window.as_ref();
    let _ = if shown { page.show() } else { page.hide() };
    true
}

/// 让页面跟着窗口走：窗口露着就画，藏起来或最小化了就停；刚恢复绘制时补发最新的数据。
/// `may_stop` 为假时只恢复、不停：窗口事件送到的那一刻，窗口的状态可能还没变完。只在主线程上调用。
fn sync_page(app: &AppHandle, label: &str, may_stop: bool) {
    let Some(window) = app.get_webview_window(label) else { return };
    let shown = window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false);
    // 刚启动时页面也许还没画出第一帧，等它画过了再停
    let may_stop = may_stop && app.state::<Shared>().started.elapsed() >= PAGE_WARMUP;
    if (shown || may_stop) && set_page_shown(&window, shown) && shown {
        push_latest(app, label);
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN) {
        // 先让页面画起来再露出窗口，免得窗口先空着亮一下
        set_page_shown(&window, true);
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        push_latest(app, MAIN);
    }
}

/// 屏幕放不下默认大小时，把主窗口缩到能放下。
fn fit_main(app: &AppHandle, window: &WebviewWindow) {
    let Some(monitor) = app.primary_monitor().ok().flatten() else { return };
    let scale = monitor.scale_factor().max(0.5);
    let area = monitor.work_area().size;
    let room_width = area.width as f64 / scale - 24.0;
    let room_height = area.height as f64 / scale - 24.0;
    if room_width < MAIN_WIDTH || room_height < MAIN_HEIGHT {
        let size = LogicalSize::new(room_width.clamp(640.0, MAIN_WIDTH), room_height.clamp(480.0, MAIN_HEIGHT));
        let _ = window.set_size(size);
        let _ = window.center();
    }
}

fn create_island(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, ISLAND, WebviewUrl::App("island.html".into()))
        .title("Foxes Token Watch 灵动岛")
        .inner_size(ISLAND_WIDTH, ISLAND_START.1)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build()
}

/// 找到保存位置所在的显示器；副屏被拔掉时，立即用主屏布局。
fn placement_monitor(app: &AppHandle, data: &Persisted) -> Option<tauri::Monitor> {
    let monitors = app.available_monitors().ok()?;
    let saved = monitors.into_iter().find(|m| {
        if let Some(dock) = &data.dock {
            return m.name() == Some(&dock.monitor);
        }
        let Some((x, y)) = data.island_pos else { return false };
        let center = x.saturating_add((ISLAND_WIDTH * m.scale_factor() / 2.0).round() as i32);
        let p = m.position();
        center >= p.x && center < p.x + m.size().width as i32 &&
            y.saturating_add(12) >= p.y && y.saturating_add(12) < p.y + m.size().height as i32
    });
    saved.or_else(|| app.primary_monitor().ok().flatten())
}

fn change_placement(window: &WebviewWindow, dock: Option<Dock>, position: Option<(i32, i32)>) {
    let app = window.app_handle();
    let shared = app.state::<Shared>();
    let side = dock.as_ref().map(|d| d.side);
    {
        let mut data = lock(&shared.persisted);
        data.island_pos = position;
        data.dock = dock;
    }
    shared.dirty.store(true, Ordering::Relaxed);
    apply_island_region(app, window, None);
    let _ = app.emit("island-dock", side);
}

#[tauri::command]
fn get_island_dock(app: AppHandle) -> Option<Side> {
    lock(&app.state::<Shared>().persisted).dock.as_ref().map(|d| d.side)
}

#[tauri::command]
fn set_island_dock(app: AppHandle, side: Option<Side>) {
    let Some(window) = app.get_webview_window(ISLAND) else { return };
    let dock = side.and_then(|side| {
        let data = lock(&app.state::<Shared>().persisted).clone();
        let monitor = placement_monitor(&app, &data)?;
        Some(Dock { side, monitor: monitor.name().cloned().unwrap_or_default(), fraction: 0.45 })
    });
    change_placement(&window, dock, None);
}

/// 让灵动岛不出现在 Alt+Tab 列表里。窗口每次重新显示后样式会被框架重算，所以显示后都要再设一次。
#[cfg(windows)]
fn hide_from_switcher(window: &WebviewWindow) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, value: isize) -> isize;
    }
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    const WS_EX_APPWINDOW: isize = 0x0004_0000;

    let Ok(hwnd) = window.hwnd() else { return };
    let handle = hwnd.0 as *mut c_void;
    // SAFETY: handle 是这个进程里一个还活着的窗口；这两个调用只读写它的扩展样式位。
    unsafe {
        let style = GetWindowLongPtrW(handle, GWL_EXSTYLE);
        let wanted = (style | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
        if wanted != style {
            SetWindowLongPtrW(handle, GWL_EXSTYLE, wanted);
        }
    }
}

#[cfg(not(windows))]
fn hide_from_switcher(_window: &WebviewWindow) {}

/// 把灵动岛放回置顶层的最上面。带置顶标记的窗口也可能被系统或别的程序压到普通窗口下面，
/// 这时窗口仍然“可见”却被挡住。force 为假时只在确实有普通窗口压在上面时才重排，不跟菜单、任务栏这类置顶窗口抢位置。
#[cfg(windows)]
fn raise_island(hwnd: isize, force: bool) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn GetWindow(hwnd: *mut c_void, cmd: u32) -> *mut c_void;
        fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
        fn SetWindowPos(hwnd: *mut c_void, after: *mut c_void, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    }
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmGetWindowAttribute(hwnd: *mut c_void, attribute: u32, value: *mut c_void, size: u32) -> i32;
    }
    const GW_HWNDPREV: u32 = 3;
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOPMOST: isize = 0x0000_0008;
    const DWMWA_CLOAKED: u32 = 14;
    const HWND_TOPMOST: isize = -1;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_NOOWNERZORDER: u32 = 0x0200;
    const SWP_ASYNCWINDOWPOS: u32 = 0x4000;

    let island = hwnd as *mut c_void;
    if island.is_null() {
        return;
    }
    // SAFETY: 只读窗口样式、调整 Z 序；句柄失效时这些调用直接返回失败，不会访问别的内存。
    unsafe {
        if IsWindowVisible(island) == 0 {
            return;
        }
        let mut buried = force;
        let mut above = GetWindow(island, GW_HWNDPREV);
        // 遍历期间窗口可能被销毁、链表会变，限定步数
        for _ in 0..1024 {
            if buried || above.is_null() {
                break;
            }
            let mut cloaked: u32 = 0;
            let hidden = IsWindowVisible(above) == 0
                || (DwmGetWindowAttribute(above, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4) == 0 && cloaked != 0);
            buried = !hidden && GetWindowLongPtrW(above, GWL_EXSTYLE) & WS_EX_TOPMOST == 0;
            above = GetWindow(above, GW_HWNDPREV);
        }
        if buried {
            let flags = SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS;
            SetWindowPos(island, HWND_TOPMOST as *mut c_void, 0, 0, 0, 0, flags);
        }
    }
}

#[cfg(not(windows))]
fn raise_island(_hwnd: isize, _force: bool) {}

/// 把灵动岛窗口裁成界面要求的那一块：范围之外既不画也不拦鼠标。
/// scale 留空时用窗口当前的缩放比例；缩放比例刚变化时由调用方传入新值。
#[cfg(windows)]
fn apply_island_region(app: &AppHandle, window: &WebviewWindow, scale: Option<f64>) {
    use std::ffi::c_void;

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateRectRgn(left: i32, top: i32, right: i32, bottom: i32) -> *mut c_void;
        fn DeleteObject(object: *mut c_void) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn SetWindowRgn(hwnd: *mut c_void, region: *mut c_void, redraw: i32) -> i32;
    }

    let shared = app.state::<Shared>();
    let (width, height) = *lock(&shared.island_region);
    let data = lock(&shared.persisted).clone();
    let Some(monitor) = placement_monitor(app, &data) else { return };
    let area = Area::of(&monitor);
    let dragging = shared.dragging.load(Ordering::Relaxed);
    let scale = if dragging { scale.or_else(|| window.scale_factor().ok()).unwrap_or(1.0) } else { monitor.scale_factor() };
    let Ok(hwnd) = window.hwnd() else { return };

    // 窗口的物理宽度按“逻辑宽 × 缩放”算，不读窗口当前大小：缩放刚变化时窗口大小可能还没跟上
    let full_width = (ISLAND_WIDTH * scale).round() as i32;
    let region_width = ((width * scale).ceil() as i32).clamp(1, full_width.max(1));
    let region_height = ((height * scale).ceil() as i32).clamp(1, area.height.max(1));
    let size = tauri::PhysicalSize::new(full_width as u32, region_height as u32);
    if window.inner_size().ok() != Some(size) {
        let _ = window.set_size(size);
    }
    let home = (area.x + (area.width - full_width) / 2, area.y + (ISLAND_TOP * scale).round() as i32);
    let anchor = data.island_pos.filter(|(x, y)| {
        let center = x.saturating_add(full_width / 2);
        center >= monitor.position().x && center < monitor.position().x + monitor.size().width as i32 &&
            y.saturating_add(12) >= monitor.position().y && y.saturating_add(12) < monitor.position().y + monitor.size().height as i32
    }).unwrap_or(home);
    let (x, y, left) = area.layout(data.dock.as_ref(), anchor, full_width, region_width, region_height);
    let pos = PhysicalPosition::new(x, y);
    if !dragging && window.outer_position().ok() != Some(pos) {
        let _ = window.set_position(pos);
    }
    let rect = (left, 0, left + region_width, region_height);

    // SAFETY: hwnd 是这个进程里一个还活着的窗口。SetWindowRgn 成功后区域对象归系统所有，只有失败时才由我们释放。
    let applied = unsafe {
        let region = CreateRectRgn(rect.0, rect.1, rect.2, rect.3);
        if region.is_null() {
            false
        } else if SetWindowRgn(hwnd.0 as *mut c_void, region, 1) == 0 {
            DeleteObject(region);
            false
        } else {
            true
        }
    };
    if applied {
        *lock(&shared.island_rect) = rect;
        shared.island_hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
fn apply_island_region(_app: &AppHandle, _window: &WebviewWindow, _scale: Option<f64>) {}

/// 鼠标现在是否在灵动岛的可见范围内；查不到时返回 None。
#[cfg(windows)]
fn cursor_over_island(shared: &Shared) -> Option<bool> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(point: *mut Point) -> i32;
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    }

    let hwnd = shared.island_hwnd.load(Ordering::Relaxed);
    if hwnd == 0 {
        return None;
    }
    let (mut point, mut frame) = (Point::default(), Rect::default());
    // SAFETY: 两个调用都只往我们提供的结构体里写数据；句柄已经失效时调用返回 0，不会访问别的内存。
    let known = unsafe { GetCursorPos(&mut point) != 0 && GetWindowRect(hwnd as *mut c_void, &mut frame) != 0 };
    if !known {
        return None;
    }
    let (left, top, right, bottom) = *lock(&shared.island_rect);
    let (x, y) = (point.x - frame.left, point.y - frame.top);
    Some(x >= left && x < right && y >= top && y < bottom)
}

#[cfg(not(windows))]
fn cursor_over_island(_shared: &Shared) -> Option<bool> {
    None
}

fn show_island(app: &AppHandle, visible: bool) {
    let Some(window) = app.get_webview_window(ISLAND) else { return };
    if visible {
        set_page_shown(&window, true);
        let _ = window.show();
        hide_from_switcher(&window);
        raise_island(app.state::<Shared>().island_hwnd.load(Ordering::Relaxed), true);
        push_latest(app, ISLAND);
    } else {
        let _ = window.hide();
        sync_page(app, ISLAND, true);
    }
}

fn on_window_event(window: &Window, event: &WindowEvent) {
    match event {
        WindowEvent::CloseRequested { api, .. } => {
            // 关窗口 = 收到托盘；真正退出走托盘菜单或设置页
            api.prevent_close();
            if window.label() == MAIN {
                let _ = window.hide();
                sync_page(window.app_handle(), MAIN, true);
            }
        }
        // 从任务栏还原之类不经过 show_main 的露面：不等监控线程下一轮核对，马上让页面接着画
        WindowEvent::Focused(true) | WindowEvent::Resized(_) if window.label() == MAIN => {
            sync_page(window.app_handle(), MAIN, false);
        }
        // 可见范围是按物理像素设的，换到缩放比例不同的屏幕后要重算
        WindowEvent::ScaleFactorChanged { scale_factor, .. } if window.label() == ISLAND => {
            let app = window.app_handle();
            if let Some(island) = app.get_webview_window(ISLAND) {
                apply_island_region(app, &island, Some(*scale_factor));
            }
        }
        _ => {}
    }
}

// —— 托盘 ——

fn build_tray(app: &AppHandle, settings: &Settings) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开面板", true, None::<&str>)?;
    let island = CheckMenuItem::with_id(app, "island", "显示灵动岛", true, settings.island, None::<&str>)?;
    let island_reset = MenuItem::with_id(app, "island-home", "灵动岛回到默认位置", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", "开机自启", true, settings.autostart, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 Foxes Token Watch", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &island,
            &island_reset,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    // 托盘图标只有十几个像素，用专门画的简化版（design/tray.svg）
    let builder = TrayIconBuilder::with_id(TRAY)
        .icon(tauri::include_image!("icons/tray.png"))
        .tooltip("Foxes Token Watch")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "island" => {
                let next = !current_settings(app).island;
                apply_patch(app, &json!({ "island": next }));
            }
            "island-home" => set_island_dock(app.clone(), None),
            "autostart" => {
                let next = !current_settings(app).autostart;
                apply_patch(app, &json!({ "autostart": next }));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    builder.build(app)?;

    *lock(&app.state::<Shared>().tray_items) = Some(TrayItems { island, autostart });
    Ok(())
}

fn compact(value: u64) -> String {
    let number = value as f64;
    if number >= 1e9 {
        format!("{:.2}B", number / 1e9)
    } else if number >= 1e6 {
        format!("{:.1}M", number / 1e6)
    } else if number >= 1e3 {
        format!("{:.1}K", number / 1e3)
    } else {
        value.to_string()
    }
}

fn tooltip_text(snapshot: &Snapshot) -> String {
    let today = snapshot.ranges.iter().find(|range| range.id == "today").map_or(0, |range| range.totals.total);
    let state = match snapshot.live.running {
        0 => "空闲".to_string(),
        running => format!("{running} 个对话运行中"),
    };
    format!("Foxes Token Watch\n{state} · 今日 {}", compact(today))
}

// —— 监控线程 ——

fn publish(app: &AppHandle, analysis: Analysis) {
    let shared = app.state::<Shared>();
    let analysis = Arc::new(analysis);
    *lock(&shared.analysis) = Some(analysis.clone());

    // 页面没在画的窗口不推送，等它重新露面时再补发最新的一份
    for (label, shown) in PAGES.into_iter().zip(&shared.page_shown) {
        if shown.load(Ordering::Relaxed) {
            let _ = app.emit_to(label, "snapshot", &analysis.snapshot);
        }
    }
    // 窗口也可能是界面自己藏起来的，或者被最小化了：顺带核对一遍。窗口的状态要到主线程上看
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        for label in PAGES {
            sync_page(&handle, label, true);
        }
    });

    let text = tooltip_text(&analysis.snapshot);
    let changed = {
        let mut current = lock(&shared.tooltip);
        let changed = *current != text;
        if changed {
            current.clone_from(&text);
        }
        changed
    };
    if changed {
        if let Some(tray) = app.tray_by_id(TRAY) {
            let _ = tray.set_tooltip(Some(&text));
        }
    }
}

/// 旧版在应用数据目录的 `cache/gateway` 下另存过插件卡片的过程记录。现在只留用量历史这一份，那些文件不再读写：
/// 认得的删掉，目录空了也删掉，别的东西不动。
fn remove_row_cache(config: &Path) {
    let dir = config.join("cache").join("gateway");
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if [".json", ".json.zst", ".json.zst.tmp"].iter().any(|suffix| name.ends_with(suffix)) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    // remove_dir 只删得掉空目录
    let _ = fs::remove_dir(&dir);
    let _ = fs::remove_dir(config.join("cache"));
}

/// 便携运行（程序是直接拿来跑的，不是装上去的）时，用量历史在程序旁边再放一份，跟着程序换电脑。
fn carried_history() -> Option<PathBuf> {
    // 调试构建只在开发时跑
    if cfg!(debug_assertions) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // 安装版旁边有卸载程序；编译输出目录里有 cargo 的锁文件
    if dir.join("uninstall.exe").exists() || dir.join(".cargo-lock").exists() {
        return None;
    }
    let folder = dir.join(DATA_DIR);
    let file = folder.join("history.json");
    // 已经带着一份的照常读；还没有的话，旁边不让写（只读的位置）就不放
    if !file.exists() && fs::create_dir_all(&folder).is_err() {
        return None;
    }
    Some(file)
}

/// 最近一次出错的位置（只留源文件名和行号，不带目录）：监控线程重来时写进诊断信息。
static LAST_PANIC: Mutex<String> = Mutex::new(String::new());

fn note_panics() {
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(|at| {
            let file = Path::new(at.file()).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{file}:{}", at.line())
        });
        *lock(&LAST_PANIC) = place.unwrap_or_default();
    }));
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = payload.downcast_ref::<&str>().map(|text| (*text).to_owned()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_default();
    // 出错信息里可能带着正在处理的那段文字：只留前面一截说明，引用的内容不要
    let brief: String = text.split(" of `").next().unwrap_or_default().chars().take(120).collect();
    format!("{} {brief}", lock(&LAST_PANIC)).trim().to_owned()
}

fn spawn_monitor(app: AppHandle, roots: Roots) {
    let spawned = thread::Builder::new().name("foxes-monitor".into()).spawn(move || {
        let mut first = Some(roots);
        loop {
            let roots = first.take().unwrap_or_else(Roots::detect);
            // 统计里出了意料之外的错，不能让这个线程悄悄停掉（界面还开着，数字却不再变）：记下原因，从头再来
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| monitor(&app, roots)));
            let shared = app.state::<Shared>();
            let restarts = shared.monitor_restarts.fetch_add(1, Ordering::Relaxed) + 1;
            *lock(&shared.monitor_error) = Some(outcome.err().map_or_else(|| "监控循环意外结束".to_owned(), panic_text));
            // 一直出错的话越等越久，别空转
            thread::sleep(Duration::from_secs(u64::from(restarts.min(12)) * 5));
        }
    });
    if let Err(error) = spawned {
        eprintln!("failed to start the monitor thread: {error}");
    }
}

/// 监控循环：扫描、同步用量历史、汇总、推给界面，一直转下去。
fn monitor(app: &AppHandle, roots: Roots) {
    let mut store = Store::new(roots);
    let config = app.path().app_config_dir().ok();
    // 插件卡片的过程记录不另存，以前记下的请求只在用量历史里：赶在第一次扫描之前读进来交给解析器
    let primed = lock(&app.state::<Shared>().history).prime(&mut store);
    store.watch_baishitong();
    if let (true, Some(dir)) = (primed, &config) {
        remove_row_cache(dir);
    }
    let (detail_tx, detail_rx) = mpsc::channel::<DetailRequest>();
    *lock(&app.state::<Shared>().detail_tx) = Some(detail_tx);
    let mut intake = Intake::default();
    let mut published: Option<Instant> = None;
    // 数据变了、但还没轮到重新汇总
    let mut pending = false;
    let mut rechecked = Instant::now();
    loop {
        let shared = app.state::<Shared>();
        if rechecked.elapsed() >= RECHECK_EVERY {
            rechecked = Instant::now();
            // 运行期间新装（或卸载）了某个 agent：数据目录变了就换上新的，下面的刷新会马上重扫
            if store.set_roots(Roots::detect()) {
                *lock(&shared.roots) = root_lists(store.roots());
            }
            if let Some(island) = app.get_webview_window(ISLAND) {
                if !shared.dragging.load(Ordering::Relaxed) {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || apply_island_region(&handle, &island, None));
                }
            }
        }
        let changed = store.refresh();
        let now = now_ms();
        // 来源不在这台电脑上的会话（已经删掉的，别的电脑记下的）由用量历史顶上，所以要赶在汇总之前
        pending |= changed | lock(&shared.history).sync(&mut store, now);
        // 面板露着、或者灵动岛展开着的时候，有变化就马上算；都没在看的时候攒一攒再算
        let watched = shared.page_shown[0].load(Ordering::Relaxed) || shared.island_expanded.load(Ordering::Relaxed);
        let gap = if watched { Duration::ZERO } else { QUIET_ANALYZE_EVERY };
        let due = published.map_or(true, |at| {
            let waited = at.elapsed();
            (pending && waited >= gap) || waited >= REPUBLISH_EVERY
        });
        if due {
            pending = false;
            let began = Instant::now();
            let mut analysis = analyze(&store, now);
            let scan = &mut analysis.snapshot.scan;
            scan.analysis_ms = began.elapsed().as_millis() as u64;
            scan.monitor_restarts = shared.monitor_restarts.load(Ordering::Relaxed);
            scan.monitor_error = lock(&shared.monitor_error).clone();
            {
                let history = lock(&shared.history);
                scan.history_error = history.error().map(str::to_owned);
                scan.history = history.info();
            }
            let live = &mut analysis.snapshot.live;
            live.recorded = intake.update(&live.rate, &live.per_second, now);
            publish(app, analysis);
            published = Some(Instant::now());
        }
        if shared.dirty.swap(false, Ordering::Relaxed) {
            save_persisted(&shared);
        }
        // 等下一轮的这段时间里顺带应答界面要的对话明细：明细得现算，全部记录只有这个线程拿得到
        let until = Instant::now() + REFRESH_EVERY;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match detail_rx.recv_timeout(left) {
                Ok((key, reply)) => {
                    let body = session_detail(&store, &key).and_then(|detail| serde_json::to_string(&detail).ok());
                    let _ = reply.send(body);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(left);
                    break;
                }
            }
        }
    }
}

/// 灵动岛展开时盯着鼠标：鼠标已经不在岛上、界面却没收到“离开”（拖动窗口之后会这样）时提醒它收起。
/// 顺带定时检查灵动岛是否被压到了普通窗口下面。
fn spawn_island_watcher(app: AppHandle) {
    let spawned = thread::Builder::new().name("foxes-island".into()).spawn(move || {
        let mut outside: u32 = 0;
        let mut raised = Instant::now();
        loop {
            thread::sleep(ISLAND_WATCH_EVERY);
            let shared = app.state::<Shared>();
            if raised.elapsed() >= ISLAND_RAISE_EVERY {
                raised = Instant::now();
                raise_island(shared.island_hwnd.load(Ordering::Relaxed), false);
            }
            if !shared.island_expanded.load(Ordering::Relaxed) || cursor_over_island(&shared).unwrap_or(true) {
                outside = 0;
                continue;
            }
            outside += 1;
            // 连续两次都不在才算数；界面还没收起的话每隔一秒左右再提醒一次
            if outside == 2 || outside % 7 == 0 {
                let _ = app.emit_to(ISLAND, "island-leave", ());
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("failed to start the island watcher thread: {error}");
    }
}

// —— 界面调用的命令 ——

#[tauri::command]
fn get_snapshot(app: AppHandle) -> Response {
    let latest = lock(&app.state::<Shared>().analysis).clone();
    let body = latest.and_then(|analysis| serde_json::to_string(&analysis.snapshot).ok());
    Response::new(body.unwrap_or_else(|| "null".into()))
}

#[tauri::command]
fn get_sessions(app: AppHandle) -> Response {
    let latest = lock(&app.state::<Shared>().analysis).clone();
    let body = latest.and_then(|analysis| serde_json::to_string(&analysis.sessions).ok());
    Response::new(body.unwrap_or_else(|| "[]".into()))
}

/// 一段对话的逐次请求和工具调用。要现算，交给监控线程（全部记录在它手里），它在两轮刷新之间应答。
#[tauri::command]
async fn get_session_detail(app: AppHandle, key: String) -> Response {
    let (reply, answer) = mpsc::channel();
    let sent = lock(&app.state::<Shared>().detail_tx).as_ref().is_some_and(|requests| requests.send((key, reply)).is_ok());
    let body = if sent { answer.recv_timeout(Duration::from_secs(5)).ok().flatten() } else { None };
    Response::new(body.unwrap_or_else(|| "null".into()))
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    current_settings(&app)
}

#[tauri::command]
fn set_settings(app: AppHandle, patch: Value) -> Settings {
    apply_patch(&app, &patch)
}

#[tauri::command]
fn get_app_info(app: AppHandle) -> AppInfo {
    let shared = app.state::<Shared>();
    let settings_path = lock(&shared.config_path).as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    let roots = lock(&shared.roots).clone();
    AppInfo { version: app.package_info().version.to_string(), roots, settings_path }
}

#[tauri::command]
fn show_panel(app: AppHandle) {
    show_main(&app);
}

// 系统标题栏拖动会为透明工具窗口绘制移动预览。直接使用物理坐标移动窗口，
// 跨屏幕缩放时仍然保持鼠标与窗口的相对位置。
#[tauri::command]
async fn drag_island(window: WebviewWindow) -> Result<(), String> {
    if window.label() != ISLAND {
        return Err("expected island window".into());
    }
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn GetAsyncKeyState(key: i32) -> i16;
            fn GetSystemMetrics(index: i32) -> i32;
        }
        tauri::async_runtime::spawn_blocking(move || -> tauri::Result<()> {
            // SM_SWAPBUTTON 为真时主按钮是物理右键。
            let button = if unsafe { GetSystemMetrics(23) } == 0 { 1 } else { 2 };
            let origin = window.outer_position()?;
            let start = window.cursor_position()?;
            let shared = window.state::<Shared>();
            if shared.dragging.swap(true, Ordering::Relaxed) { return Ok(()) }
            let moved = (|| -> tauri::Result<_> {
                let mut last = origin;
                while unsafe { GetAsyncKeyState(button) } < 0 {
                    let cursor = window.cursor_position()?;
                    let next = PhysicalPosition::new(origin.x + (cursor.x - start.x).round() as i32,
                                                     origin.y + (cursor.y - start.y).round() as i32);
                    if next != last {
                        window.set_position(next)?;
                        last = next;
                    }
                    thread::sleep(Duration::from_millis(12));
                }
                Ok((last, window.cursor_position()?))
            })();
            shared.dragging.store(false, Ordering::Relaxed);
            let (last, cursor) = moved?;
            let app = window.app_handle();
            if let Some(monitor) = app.monitor_from_point(cursor.x, cursor.y)? {
                let area = Area::of(&monitor);
                let dock = area.snap_side(cursor.x as i32, monitor.scale_factor()).map(|side| Dock {
                    side,
                    monitor: monitor.name().cloned().unwrap_or_default(),
                    fraction: ((last.y - area.y) as f64 / area.height.max(1) as f64).clamp(0.0, 1.0),
                });
                let full = (ISLAND_WIDTH * monitor.scale_factor()).round() as i32;
                let x = if lock(&shared.persisted).dock.is_some() { cursor.x as i32 - full / 2 } else { last.x };
                let center = (x + full / 2).clamp(area.x, area.x + area.width - 1);
                let pos = (center - full / 2, last.y.clamp(area.y, area.y + (area.height - 24).max(0)));
                let position = dock.is_none().then_some(pos);
                let handle = window.clone();
                app.run_on_main_thread(move || change_placement(&handle, dock, position))?;
            }
            Ok(())
        }).await.map_err(|error| error.to_string())?.map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    window.start_dragging().map_err(|error| error.to_string())
}

/// 界面告诉后台：灵动岛现在要显示多大一块（逻辑像素），以及是否处于展开状态。
#[tauri::command]
fn island_region(app: AppHandle, width: f64, height: f64, expanded: bool) {
    let shared = app.state::<Shared>();
    let width = if width.is_finite() { width.clamp(8.0, ISLAND_WIDTH) } else { ISLAND_START.0 };
    let height = if height.is_finite() { height.clamp(24.0, ISLAND_HEIGHT) } else { ISLAND_START.1 };
    *lock(&shared.island_region) = (width, height);
    shared.island_expanded.store(expanded, Ordering::Relaxed);
    if let Some(window) = app.get_webview_window(ISLAND) {
        apply_island_region(&app, &window, None);
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

// —— 启动 ——

fn root_lists(roots: &Roots) -> Vec<Vec<String>> {
    foxes_core::model::Agent::ALL.into_iter().map(|agent| roots.of(agent))
        .map(|paths| paths.iter().map(|path| path.display().to_string()).collect())
        .collect()
}

fn setup(app: &mut App) -> tauri::Result<()> {
    let handle = app.handle().clone();
    let start_hidden = std::env::args().any(|arg| arg == "--hidden");

    let config_paths = config_candidates(&handle);
    let (mut persisted, config_path) = load_persisted(&config_paths);
    // 开机自启以系统里的实际状态为准；已开启就重新登记一次，exe 换了位置也能跟上
    let autolaunch = handle.autolaunch();
    persisted.settings.autostart = autolaunch.is_enabled().unwrap_or(false);
    if persisted.settings.autostart {
        let _ = autolaunch.enable();
    }
    let settings = persisted.settings.clone();

    let migrate_settings = config_path.as_ref() != config_paths.first();
    let roots = Roots::detect();
    let history = History::new(handle.path().app_config_dir().ok().map(|dir| dir.join("history.json"))).carry(carried_history());
    app.manage(Shared {
        config_paths,
        config_path: Mutex::new(config_path),
        persisted: Mutex::new(persisted),
        dirty: AtomicBool::new(false),
        analysis: Mutex::new(None),
        history: Mutex::new(history),
        roots: Mutex::new(root_lists(&roots)),
        tray_items: Mutex::new(None),
        tooltip: Mutex::new(String::new()),
        dragging: AtomicBool::new(false),
        island_region: Mutex::new(ISLAND_START),
        island_rect: Mutex::new((0, 0, 0, 0)),
        island_hwnd: AtomicIsize::new(0),
        island_expanded: AtomicBool::new(false),
        started: Instant::now(),
        // 页面建出来时默认在画
        page_shown: [AtomicBool::new(true), AtomicBool::new(true)],
        monitor_restarts: AtomicU32::new(0),
        monitor_error: Mutex::new(None),
        detail_tx: Mutex::new(None),
    });

    if migrate_settings {
        save_persisted(&handle.state::<Shared>());
    }
    build_tray(&handle, &settings)?;

    let island = create_island(&handle)?;
    // 显示之前先裁好，否则整个透明窗口都会挡住后面的点击
    apply_island_region(&handle, &island, None);
    hide_from_switcher(&island);
    if settings.island {
        show_island(&handle, true);
    }

    if let Some(main) = app.get_webview_window(MAIN) {
        fit_main(&handle, &main);
    }
    if !start_hidden {
        show_main(&handle);
    }

    spawn_island_watcher(handle.clone());
    spawn_monitor(handle, roots);
    Ok(())
}

/// 启动不了时弹一个系统提示框（这时候还没有自己的窗口可用）。
#[cfg(windows)]
fn alert(text: &str) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    const MB_ICONWARNING: u32 = 0x30;

    let wide = |value: &str| value.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Foxes Token Watch"));
    // SAFETY: 两个字符串都以 0 结尾，在调用期间一直有效。
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONWARNING);
    }
}

#[cfg(not(windows))]
fn alert(text: &str) {
    eprintln!("{text}");
}

/// 界面用的是系统自带的 WebView2（Windows 11 自带，Windows 10 一般随 Edge 装好）。
#[cfg(windows)]
fn webview_missing() -> bool {
    wry::webview_version().is_err()
}

#[cfg(not(windows))]
fn webview_missing() -> bool {
    false
}

/// 不让 WebView2 往它的数据目录里下载浏览器组件（版权解码器、语音识别、过滤规则等，合计几十 MB）：
/// 界面只显示程序自带的页面，这些都用不上。走环境变量是因为它对本进程建的所有窗口一起生效——
/// 两个窗口共用一个数据目录，各自指定的参数要是不一样，后建的那个建不出来。
#[cfg(windows)]
fn slim_webview() {
    const NAME: &str = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";
    const ARG: &str = "--disable-component-update";
    let current = std::env::var(NAME).unwrap_or_default();
    if !current.split_whitespace().any(|arg| arg == ARG) {
        std::env::set_var(NAME, format!("{current} {ARG}").trim_start());
    }
}

#[cfg(not(windows))]
fn slim_webview() {}

fn main() {
    // 单次统计使用和面板相同的采集内核，无须启动 WebView 或已有的托盘实例。只算本机现在还读得到的记录：
    // 由用量历史顶上的会话和百事通的对话（要等后台读取）不在内，所以会比面板上的数少。
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("--history-report")) {
        let path = match (args.next(), args.next()) {
            (Some(path), None) => PathBuf::from(path),
            _ => {
                eprintln!("Usage: Foxes-Token-Watch.exe --history-report <report.json>");
                std::process::exit(2);
            }
        };
        let mut store = Store::new(Roots::detect());
        store.refresh();
        let analysis = analyze(&store, now_ms());
        let snapshot = &analysis.snapshot;
        let all = snapshot.ranges.iter().find(|range| range.id == "all").expect("all-time range");
        let report = json!({
            "generatedAt": snapshot.generated_at,
            "from": all.from,
            "sessionsWithUsage": all.sessions,
            "mainSessionsWithoutUsage": analysis.sessions.iter().filter(|s| !s.sub_agent && s.totals.requests == 0).count(),
            "totals": all.totals,
            "byAgent": all.by_agent,
            "scan": snapshot.scan,
        });
        if let Err(error) = fs::write(&path, serde_json::to_vec_pretty(&report).expect("valid JSON")) {
            eprintln!("{}: {error}", path.display());
            std::process::exit(1);
        }
        return;
    }
    // 缺 WebView2 时说清楚原因，不要静静地打不开
    if webview_missing() {
        alert("这台电脑缺少 Microsoft Edge WebView2 运行时，Foxes Token Watch 的界面显示不出来。\n\n请到微软官网搜索“WebView2 运行时”，安装“常青版”后再打开本软件。");
        return;
    }
    // 要赶在建第一个窗口之前，这时候也还没有别的线程
    slim_webview();
    note_panics();

    let built = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_autostart::Builder::new().app_name("Foxes-watch").args(["--hidden"]).build())
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_sessions,
            get_session_detail,
            get_settings,
            set_settings,
            get_app_info,
            show_panel,
            drag_island,
            set_island_dock,
            get_island_dock,
            island_region,
            quit_app
        ])
        .on_window_event(on_window_event)
        .setup(|app| Ok(setup(app)?))
        .build(tauri::generate_context!());
    let app = match built {
        Ok(app) => app,
        Err(error) => {
            alert(&format!("Foxes Token Watch 启动失败：\n{error}"));
            return;
        }
    };

    app.run(|app, event| match event {
        // 所有窗口都藏起来也继续在托盘里跑；只有明确选择退出（带退出码）才结束
        RunEvent::ExitRequested { api, code, .. } => {
            if code.is_none() {
                api.prevent_exit();
            }
        }
        RunEvent::Exit => {
            let shared = app.state::<Shared>();
            if shared.dirty.swap(false, Ordering::Relaxed) {
                save_persisted(&shared);
            }
            // 还没到点的用量历史也写掉：换一台电脑接着用时，带过去的那份是齐的
            lock(&shared.history).flush(now_ms());
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_latest_valid_settings_and_legacy_position() {
        let dir = std::env::temp_dir().join(format!("foxes-settings-test-{}", std::process::id()));
        let paths = [dir.join("portable.json"), dir.join("user.json")];
        write_replacing(&paths[0], r#"{"settings":{"theme":"light"},"islandPos":[30,45]}"#).unwrap();
        write_replacing(&paths[1], r#"{"settings":{"edgeAutoHide":false},"revision":4}"#).unwrap();
        let (latest, source) = load_persisted(&paths);
        assert_eq!(source.as_ref(), Some(&paths[1]));
        assert!(!latest.settings.edge_auto_hide);
        write_replacing(&paths[1], "{").unwrap();
        let (legacy, source) = load_persisted(&paths);
        assert_eq!(source.as_ref(), Some(&paths[0]));
        assert_eq!(legacy.island_pos, Some((30, 45)));
        assert!(legacy.settings.edge_auto_hide);
        fs::remove_dir_all(dir).unwrap();
    }
}
