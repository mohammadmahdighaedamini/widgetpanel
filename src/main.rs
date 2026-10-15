#![windows_subsystem = "windows"]

//! Widget Panel — a liquid side panel that lives on the right edge of the screen.

use std::{
    fs,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod audio;
mod net;
mod sys;
mod util;
mod vault;

use serde_json::{json, Value};
use tao::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::WindowBuilder,
};
use wry::{WebContext, WebViewBuilder};

const UI: &str = include_str!("ui.html");
const WIN_W: f64 = 480.0;
const WIN_H: f64 = 680.0;
/// Size (logical px) of the clickable handle on the screen edge, centred vertically.
const HANDLE_W: f64 = 12.0;
const HANDLE_H: f64 = 60.0;
/// Extra logical px around the handle while the cursor is on it (prevents hot/not-hot flicker at the border).
const HYST: f64 = 8.0;

static OPEN: AtomicBool = AtomicBool::new(false);
/// Current click-through state of the window (so we only touch the window style when it really changes).
static PASS: AtomicBool = AtomicBool::new(false);
/// When the panel was last closed (ms since epoch): the handle stays quiet for a moment afterwards.
static CLOSED_AT: AtomicU64 = AtomicU64::new(0);
static PICKING: AtomicBool = AtomicBool::new(false);
/// Where the panel currently lives, so the hover watcher follows moves between edges / monitors.
static PANEL_X: AtomicI32 = AtomicI32::new(0);
static PANEL_Y: AtomicI32 = AtomicI32::new(0);
static PANEL_H: AtomicI32 = AtomicI32::new(0);
static EDGE_X: AtomicI32 = AtomicI32::new(0);
static ON_LEFT: AtomicBool = AtomicBool::new(false);
/// Screen (y, height) the panel sits on, and how far the panel's centre was dragged from the screen centre.
static SCR_Y: AtomicI32 = AtomicI32::new(0);
static SCR_H: AtomicI32 = AtomicI32::new(0);
static HANDLE_OFF: AtomicI32 = AtomicI32::new(0);
/// Recent clipboard images, newest last. Thumbnails go to the UI; the pixels stay here in an Arc to avoid deep clones.
type ImageEntry = (u64, u32, u32, Arc<Vec<u8>>);
static IMAGES: Mutex<Vec<ImageEntry>> = Mutex::new(Vec::new());
const MAX_IMAGES: usize = 25;
const MAX_IMAGE_BYTES: usize = 150 * 1024 * 1024;
/// Pinned images are also kept on disk, so they come back after a restart.
static PINNED: Mutex<Vec<ImageEntry>> = Mutex::new(Vec::new());
static LAST_IMAGE: AtomicU64 = AtomicU64::new(0);
/// Last time the page said "I am alive" (ms since epoch). If it goes quiet, the web view is restarted.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
const MAX_PINNED: usize = 12;
/// Set when the page has finished loading (it sends "ready"): background threads wait for it so nothing they send is lost.
static READY: AtomicBool = AtomicBool::new(false);
/// Scale factor (x1000) of the monitor the panel is on.
static SCALE_MILLI: AtomicU32 = AtomicU32::new(1000);

/// Locks a mutex even if a thread panicked while holding it (the data is still fine for us).
fn lk<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs a background loop on its own thread and starts it again if it ever panics,
/// so one unexpected error can never silently kill (for example) the edge handle or the clipboard history.
fn keep_alive<F: Fn() + Send + 'static>(name: &'static str, f: F) {
    thread::spawn(move || loop {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&f)) {
            Ok(()) => return,
            Err(_) => {
                log(&format!("{name} crashed and was restarted"));
                thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

fn scale() -> f64 {
    SCALE_MILLI.load(Ordering::Relaxed) as f64 / 1000.0
}

fn wait_ready() {
    for _ in 0..300 {
        if READY.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

enum Ev {
    Script(String),
    Ipc(String),
    Open,
    /// open the panel from the tray, optionally straight into a widget
    Show(&'static str),
    Menu(String),
    /// the handle is being dragged: new vertical centre of the panel (screen px)
    MoveY(i32),
    /// drag finished: tell the page so it can save the position
    MoveEnd,
    /// cursor is over the handle: catch the click ourselves instead of letting it fall through
    Grab(bool),
    /// a monitor was added / removed / changed: place the panel again
    Rescan,
    /// the page stopped answering (WebView2 crashed / GPU reset): build the web view again
    Respawn,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/* ---------------- network panel ---------------- */
/// Sends a result to the Network panel: app.net(kind, data)
fn net_emit(proxy: &EventLoopProxy<Ev>, kind: &str, data: Value) {
    let _ = proxy.send_event(Ev::Script(format!("app.net({},{})", json!(kind), data)));
}

/* ---------------- passwords ---------------- */
/// Sends a result to the Passwords panel: app.vault(kind, data)
fn vault_emit(proxy: &EventLoopProxy<Ev>, kind: &str, data: Value) {
    let _ = proxy.send_event(Ev::Script(format!("app.vault({},{})", json!(kind), data)));
}

/// True when the clipboard holds a password we copied ourselves (it must not enter the history).
fn is_secret(text: &str) -> bool {
    vault::is_secret(text)
}

/* ---------------- clipboard ---------------- */
pub fn set_clip(text: &str) {
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_text(text.to_owned());
    }
}

pub fn get_clip() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

fn spawn_clip_watch(proxy: EventLoopProxy<Ev>) {
    keep_alive("clipboard watcher", move || {
        wait_ready();
        let mut seq = 0;
        loop {
            let s = sys::clip_seq();
            if s != seq {
                seq = s;
                match get_clip() {
                    Some(t) if !t.trim().is_empty() => {
                        if is_secret(&t) {
                            // a password we copied: keep it out of the history
                        } else if t.len() < 100_000 && proxy.send_event(Ev::Script(format!("app.clip({})", json!(t)))).is_err() {
                            return;
                        }
                    }
                    _ => {
                        if let Some(script) = take_clip_image() {
                            if proxy.send_event(Ev::Script(script)).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(400));
        }
    });
}

/// Copies a new clipboard image into the history and returns the call that shows its thumbnail.
fn take_clip_image() -> Option<String> {
    let (w, h, rgba) = sys::clip_image()?;
    let id = util::hash(&rgba) ^ ((w as u64) << 32) ^ h as u64;
    if LAST_IMAGE.swap(id, Ordering::Relaxed) == id {
        return None;
    }
    let call = image_call(id, w, h, &rgba, false);
    let mut images = lk(&IMAGES);
    images.retain(|(other, ..)| *other != id);
    images.push((id, w, h, Arc::new(rgba))); // moves the buffer, no extra copy of the pixels
    // same count as the Windows clipboard history, with a memory guard for huge screenshots
    while images.len() > MAX_IMAGES || (images.len() > 1 && images.iter().map(|(_, _, _, px)| px.len()).sum::<usize>() > MAX_IMAGE_BYTES) {
        images.remove(0);
    }
    Some(call)
}

/* ---------------- pinned clipboard images ---------------- */
fn pins_dir() -> std::path::PathBuf {
    let dir = sys::data_dir().join("pinned");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn image_call(id: u64, w: u32, h: u32, rgba: &[u8], pinned: bool) -> String {
    let (tw, th, thumb) = util::thumbnail(w, h, rgba, 240);
    let uri = format!("data:image/png;base64,{}", util::base64(&util::png(tw, th, &thumb)));
    format!("app.clipImage({})", json!({ "id": id.to_string(), "w": w, "h": h, "thumb": uri, "pinned": pinned }))
}

fn pin_image(id: u64, on: bool) -> Result<(), &'static str> {
    const FAIL: &str = "Could not pin that image";
    let file = pins_dir().join(format!("{id}.bin"));
    if !on {
        let _ = fs::remove_file(file);
        lk(&PINNED).retain(|(i, ..)| *i != id);
        return Ok(());
    }
    if lk(&PINNED).iter().filter(|(i, ..)| *i != id).count() >= MAX_PINNED {
        return Err("You can pin up to 12 images");
    }
    let Some((_, w, h, px)) = lk(&IMAGES).iter().find(|(i, ..)| *i == id).cloned() else { return Err(FAIL) };
    let (w, h, px) = util::thumbnail(w, h, &px, 1600); // keep the saved copy reasonable
    let mut body = Vec::with_capacity(px.len() + 8);
    body.extend_from_slice(&w.to_le_bytes());
    body.extend_from_slice(&h.to_le_bytes());
    body.extend_from_slice(&px);
    util::write_atomic(&file, &body).map_err(|_| FAIL)?;
    let mut v = lk(&PINNED);
    v.retain(|(i, ..)| *i != id); // pinning the same picture twice must not list it twice
    v.push((id, w, h, Arc::new(px)));
    Ok(())
}

/// Shows pinned images again when the app starts.
fn restore_pins(proxy: EventLoopProxy<Ev>) {
    thread::spawn(move || {
        wait_ready();
        let Ok(entries) = fs::read_dir(pins_dir()) else { return };
        // newest first; anything beyond the limit (older versions had none) is removed so it cannot pile up unseen
        let mut files: Vec<_> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "bin"))
            .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH), e))
            .collect();
        files.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, extra) in files.drain(files.len().min(MAX_PINNED)..) {
            let _ = fs::remove_file(extra.path());
        }
        for (_, entry) in files {
            let Some(id) = entry.path().file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()) else { continue };
            let Ok(mut body) = fs::read(entry.path()) else { continue };
            if body.len() < 8 {
                continue;
            }
            let w = u32::from_le_bytes(body[0..4].try_into().unwrap());
            let h = u32::from_le_bytes(body[4..8].try_into().unwrap());
            if (body.len() - 8) as u64 != w as u64 * h as u64 * 4 {
                continue;
            }
            body.drain(..8); // cut the 8-byte header off in place: the pixels are not copied again
            let _ = proxy.send_event(Ev::Script(image_call(id, w, h, &body, true)));
            lk(&PINNED).push((id, w, h, Arc::new(body)));
        }
    });
}

/* ---------------- edge handle: click to open, drag to move ---------------- */
fn spawn_edge_watch(proxy: EventLoopProxy<Ev>) {
    keep_alive("edge watcher", move || {
        let mut hot = false;
        let mut was_down = sys::left_down();
        let mut press: Option<(i32, i32)> = None;
        let mut dragging = false;
        let mut far = true;
        let mut last_center = i32::MIN;
        let mut flipped = std::time::Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(std::time::Instant::now);
        loop {
            thread::sleep(Duration::from_millis(if far && press.is_none() { 40 } else { 12 }));
            let scale = scale(); // follows the monitor the panel is on
            sys::click_through(PASS.load(Ordering::Relaxed));
            let down = sys::left_down();
            if OPEN.load(Ordering::Relaxed) || PICKING.load(Ordering::Relaxed) {
                if OPEN.load(Ordering::Relaxed) && !PICKING.load(Ordering::Relaxed) && down && !was_down {
                    if let Some((px, py)) = sys::cursor_pos() {
                        let (wx, wy, wh) = (PANEL_X.load(Ordering::Relaxed), PANEL_Y.load(Ordering::Relaxed), PANEL_H.load(Ordering::Relaxed));
                        let ww = (WIN_W * scale) as i32;
                        if px < wx || px >= wx + ww || py < wy || py >= wy + wh {
                            let _ = proxy.send_event(Ev::Script("app.close()".into()));
                        }
                    }
                }
                hot = false;
                press = None;
                dragging = false;
                was_down = down;
                continue;
            }
            let Some((px, py)) = sys::cursor_pos() else { continue };
            let (win_y, h, edge) = (PANEL_Y.load(Ordering::Relaxed), PANEL_H.load(Ordering::Relaxed), EDGE_X.load(Ordering::Relaxed));
            let left = ON_LEFT.load(Ordering::Relaxed);
            let dist = if left { px - edge } else { edge - 1 - px };
            let mid = win_y + h / 2;
            far = dist > (160.0 * scale) as i32 || dist < 0;
            let slack = if hot { (HYST * scale) as i32 } else { 0 };
            let quiet = now_ms().saturating_sub(CLOSED_AT.load(Ordering::Relaxed)) < 450;
            let over = !quiet
                && dist >= 0
                && dist < (HANDLE_W * scale) as i32 + slack
                && (py - mid).abs() <= (HANDLE_H * 0.5 * scale) as i32 + slack;

            match press {
                None => {
                    let settled = flipped.elapsed() >= Duration::from_millis(if over { 60 } else { 140 });
                    if over != hot && settled {
                        hot = over;
                        flipped = std::time::Instant::now();
                        if hot {
                            sys::remember_foreground();
                        }
                        let _ = proxy.send_event(Ev::Grab(hot));
                        if proxy.send_event(Ev::Script(format!("app.handle({hot})"))).is_err() {
                            return;
                        }
                    }
                    if over && down && !was_down {
                        press = Some((py - mid, py));
                        dragging = false;
                        last_center = i32::MIN;
                    }
                }
                Some((grab, start)) => {
                    if down {
                        if !dragging && (py - start).abs() > (6.0 * scale) as i32 {
                            dragging = true;
                        }
                        if dragging && py - grab != last_center {
                            last_center = py - grab;
                            let _ = proxy.send_event(Ev::MoveY(last_center));
                        }
                    } else {
                        if dragging {
                            let _ = proxy.send_event(Ev::MoveEnd);
                            sys::restore_foreground();
                        } else {
                            sys::remember_foreground();
                            OPEN.store(true, Ordering::Relaxed);
                            let _ = proxy.send_event(Ev::Script("app.handle(false)".into()));
                            let _ = proxy.send_event(Ev::Open);
                            hot = false;
                        }
                        press = None;
                        dragging = false;
                    }
                }
            }
            was_down = down;
        }
    });
}

fn log(msg: &str) {
    use std::io::Write;
    let path = sys::data_dir().join("log.txt");
    if fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    let msg: String = msg.chars().take(500).collect();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
}

fn add_app(proxy: EventLoopProxy<Ev>, folder: bool) {
    thread::spawn(move || {
        if let Some(path) = if folder { sys::pick_folder() } else { sys::pick_file() } {
            let item = json!({ "path": path, "name": util::display_name(&path), "icon": sys::icon_data_uri(&path) });
            let _ = proxy.send_event(Ev::Script(format!("app.appAdded({item})")));
            let _ = proxy.send_event(Ev::Show("settings"));
        }
    });
}

/* ---------------- "Add to Widget Panel" from the file manager ---------------- */
fn inbox() -> std::path::PathBuf {
    sys::data_dir().join("inbox.txt")
}

/// `--add "D:\"` reaches us as `D:"` (the backslash escapes the closing quote): strip the quote and give a bare drive its root back.
fn clean_path_arg(raw: &str) -> String {
    let p = raw.trim().trim_end_matches('"');
    if p.len() == 2 && p.ends_with(':') { format!("{p}\\") } else { p.to_string() }
}

fn queue_path(path: &str) {
    use std::io::Write;
    if path.is_empty() {
        return;
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(inbox()) {
        let _ = writeln!(f, "{path}");
    }
}

fn spawn_inbox_watch(proxy: EventLoopProxy<Ev>) {
    let file = inbox();
    keep_alive("inbox watcher", move || loop {
        wait_ready();
        // move the list away first: a path added a moment later goes into a fresh inbox.txt instead of being deleted unread
        let work = file.with_extension("work");
        if fs::rename(&file, &work).is_ok() {
            thread::sleep(Duration::from_millis(50));
            let text = fs::read_to_string(&work).unwrap_or_default();
            let _ = fs::remove_file(&work);
            for path in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let item = json!({ "path": path, "name": util::display_name(path), "icon": sys::icon_data_uri(path) });
                if proxy.send_event(Ev::Script(format!("app.appAdded({item})"))).is_err() {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(700));
    });
}

/* ---------------- where the panel sits ---------------- */
struct Screen {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    scale: f64,
    name: String,
}

fn collect_screens(monitors: impl Iterator<Item = tao::monitor::MonitorHandle>) -> Vec<Screen> {
    monitors
        .enumerate()
        .map(|(i, m)| {
            let (p, s) = (m.position(), m.size());
            Screen { x: p.x, y: p.y, w: s.width as i32, h: s.height as i32, scale: m.scale_factor(), name: m.name().unwrap_or_else(|| format!("Monitor {}", i + 1)) }
        })
        .collect()
}

/// Puts the panel on monitor `idx` (each monitor has its own scale factor, so size and handle follow it).
fn place(window: &tao::window::Window, screens: &[Screen], idx: usize, left: bool) -> (i32, i32) {
    let Some(s) = screens.get(idx).or_else(|| screens.first()) else { return (0, 0) };
    let scale = s.scale;
    SCALE_MILLI.store((scale * 1000.0).round() as u32, Ordering::Relaxed);
    let w = (WIN_W * scale) as i32;
    let h = ((WIN_H * scale) as i32).min(s.h);
    let x = if left { s.x } else { s.x + s.w - w };
    let y = (s.y + (s.h - h) / 2 + HANDLE_OFF.load(Ordering::Relaxed)).clamp(s.y, s.y + s.h - h);
    window.set_inner_size(PhysicalSize::new(w, h));
    window.set_outer_position(PhysicalPosition::new(x, y));
    SCR_Y.store(s.y, Ordering::Relaxed);
    SCR_H.store(s.h, Ordering::Relaxed);
    PANEL_X.store(x, Ordering::Relaxed);
    PANEL_Y.store(y, Ordering::Relaxed);
    PANEL_H.store(h, Ordering::Relaxed);
    EDGE_X.store(if left { s.x } else { s.x + s.w }, Ordering::Relaxed);
    ON_LEFT.store(left, Ordering::Relaxed);
    (w, h)
}

/// The app has no console: log the reason, tell the user, and stop.
fn fatal(msg: &str) -> ! {
    log(msg);
    sys::message_box("Widget Panel", msg);
    std::process::exit(1);
}

/* ---------------- click-through while closed ---------------- */
fn set_passthrough(window: &tao::window::Window, on: bool) {
    let _ = window;
    if PASS.swap(on, Ordering::Relaxed) != on {
        sys::click_through(on);
    }
}

/* ---------------- tray ---------------- */
mod tray {
    use super::{util, Ev};
    use tao::event_loop::EventLoopProxy;
    use tray_icon::{
        menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    pub struct Tray {
        _icon: TrayIcon,
        startup: CheckMenuItem,
    }

    impl Tray {
        pub fn new(proxy: &EventLoopProxy<Ev>, startup_on: bool) -> Option<Tray> {
            let startup = CheckMenuItem::with_id("startup", "Start with Windows", true, startup_on, None);
            let menu = Menu::new();
            let _ = menu.append_items(&[
                &MenuItem::with_id("open", "Open Widget Panel", true, None),
                &MenuItem::with_id("settings", "Widgets && settings…", true, None),
                &MenuItem::with_id("addapp", "Add app…", true, None),
                &PredefinedMenuItem::separator(),
                &startup,
                &PredefinedMenuItem::separator(),
                &MenuItem::with_id("quit", "Quit Widget Panel", true, None),
            ]);
            let icon = TrayIconBuilder::new()
                .with_tooltip("Widget Panel")
                .with_icon(Icon::from_rgba(util::tray_rgba(), 32, 32).ok()?)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .ok()?;
            let mp = proxy.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let _ = mp.send_event(Ev::Menu(e.id.0));
            }));
            let tp = proxy.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    let _ = tp.send_event(Ev::Show(""));
                }
            }));
            Some(Tray { _icon: icon, startup })
        }

        pub fn set_startup(&self, on: bool) {
            self.startup.set_checked(on);
        }
    }
}

/// Reads a JSON file. A file that exists but cannot be parsed is not lost: it is kept next to the original as `<name>.bad`.
fn read_json(path: &std::path::Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Some(v),
        Err(_) => {
            let _ = fs::rename(path, path.with_extension("bad"));
            log(&format!("{} was damaged and was moved to .bad", path.display()));
            None
        }
    }
}

/* ---------------- web view ---------------- */
fn init_script(settings: &Value, notes: &Value, screens: &[Screen]) -> String {
    // the setInterval is a heartbeat: when it stops arriving, the page is dead and the web view is rebuilt
    format!(
        "window.onerror=(m,s,l)=>window.ipc.postMessage(JSON.stringify({{t:\"log\",text:m+\" @\"+l}}));\
         setInterval(()=>{{try{{window.ipc.postMessage('{{\"t\":\"hb\"}}')}}catch(e){{}}}},3000);\
         window.__init = {};",
        json!({ "settings": settings, "notes": notes, "home": sys::home(),
                "contextMenu": sys::context_menu_enabled(),
                "screens": screens.iter().map(|s| json!({ "name": s.name, "w": s.w, "h": s.h })).collect::<Vec<_>>() })
    )
}

fn build_webview(window: &tao::window::Window, ctx: &mut WebContext, proxy: &EventLoopProxy<Ev>, init: &str) -> wry::Result<wry::WebView> {
    let ipc_proxy = proxy.clone();
    let drop_proxy = proxy.clone();
    WebViewBuilder::new_with_web_context(ctx)
        .with_drag_drop_handler(move |e| {
            match e {
                wry::DragDropEvent::Enter { .. } => {
                    let _ = drop_proxy.send_event(Ev::Script("app.dropHint(true)".into()));
                }
                wry::DragDropEvent::Leave => {
                    let _ = drop_proxy.send_event(Ev::Script("app.dropHint(false)".into()));
                }
                wry::DragDropEvent::Drop { paths, .. } => {
                    let _ = drop_proxy.send_event(Ev::Script("app.dropHint(false)".into()));
                    // reading icons can be slow (many files, network drives): never on the UI thread
                    let proxy = drop_proxy.clone();
                    thread::spawn(move || {
                        for path in paths {
                            let p = path.display().to_string();
                            let item = json!({ "path": p, "name": util::display_name(&p), "icon": sys::icon_data_uri(&p) });
                            let _ = proxy.send_event(Ev::Script(format!("app.appAdded({item})")));
                        }
                    });
                }
                _ => {}
            }
            true
        })
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_initialization_script(init)
        .with_html(UI)
        .with_ipc_handler(move |req| {
            let body = req.body();
            if body.starts_with(r#"{"t":"hb""#) {
                HEARTBEAT.store(now_ms(), Ordering::Relaxed);
                return;
            }
            let _ = ipc_proxy.send_event(Ev::Ipc(body.clone()));
        })
        .build(window)
}

fn main() {
    // the app has no console: a crash would be silent, so write the reason to log.txt first
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let adding = args.iter().position(|a| a == "--add").and_then(|i| args.get(i + 1));
    if let Some(path) = adding {
        queue_path(&clean_path_arg(path));
    }
    if let Some(v) = args.iter().position(|a| a == "--context-menu").and_then(|i| args.get(i + 1)) {
        sys::set_context_menu(v == "on");
        return;
    }
    if let Some(pid) = args.iter().position(|a| a == "--audio-guard").and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u32>().ok()) {
        audio::guard(pid);
        return;
    }
    if !sys::single_instance() {
        return;
    }
    if adding.is_none() {
        let _ = fs::remove_file(inbox()); // only a stale list from an earlier crash: never the path this very start was launched to add
    }

    let dir = sys::data_dir();
    let settings_path = dir.join("settings.json");
    thread::spawn(net::cleanup_stale_wifi); // password exports left in %TEMP% by an interrupted Wi-Fi read
    #[cfg(not(debug_assertions))]
    sys::refresh_registered_paths(); // the exe may have been moved since "start with Windows" / the right-click menu were set up

    let mut settings: Value = read_json(&settings_path).unwrap_or_else(|| json!({}));
    settings["startup"] = json!(sys::startup_enabled());
    let notes_json = dir.join("notes.json");
    let notes: Value = read_json(&notes_json).unwrap_or_else(|| json!([]));

    let event_loop = EventLoopBuilder::<Ev>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let mut all_screens = collect_screens(event_loop.available_monitors());
    if all_screens.is_empty() {
        fatal("No monitor was found.");
    }
    let on_left = settings["edge"] == "left";
    let mut screen_idx = settings["monitor"].as_u64().unwrap_or(0) as usize;
    if screen_idx >= all_screens.len() {
        screen_idx = 0;
    }
    let home = &all_screens[screen_idx];
    let home_scale = home.scale;
    SCALE_MILLI.store((home_scale * 1000.0).round() as u32, Ordering::Relaxed);
    let (w, h) = ((WIN_W * home_scale) as i32, ((WIN_H * home_scale) as i32).min(home.h));
    let x = if on_left { home.x } else { home.x + home.w - w };
    let y = home.y + (home.h - h) / 2;

    let builder = WindowBuilder::new()
        .with_title("Widget Panel")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_inner_size(PhysicalSize::new(w, h))
        .with_position(PhysicalPosition::new(x, y));
    let builder = {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder.with_skip_taskbar(true).with_undecorated_shadow(false)
    };
    let window = builder.build(&event_loop).unwrap_or_else(|e| fatal(&format!("The window could not be created: {e}")));
    window.set_ignore_cursor_events(true).ok();
    PASS.store(true, Ordering::Relaxed);
    {
        use tao::platform::windows::WindowExtWindows;
        sys::set_self_window(window.hwnd() as isize);
    }

    let mut ctx = WebContext::new(Some(dir.join("webview")));
    let init = init_script(&settings, &notes, &all_screens);
    HEARTBEAT.store(now_ms(), Ordering::Relaxed);
    let mut webview = build_webview(&window, &mut ctx, &proxy, &init).unwrap_or_else(|e| {
        fatal(&format!(
            "The web view could not be started: {e}\n\nWidget Panel needs the Microsoft Edge WebView2 Runtime.\nInstall it from https://developer.microsoft.com/microsoft-edge/webview2 and start the app again."
        ))
    });
    spawn_clip_watch(proxy.clone());
    spawn_inbox_watch(proxy.clone());
    restore_pins(proxy.clone());
    HANDLE_OFF.store((settings["handleY"].as_f64().unwrap_or(0.0) * home_scale) as i32, Ordering::Relaxed);
    place(&window, &all_screens, screen_idx, on_left);
    spawn_edge_watch(proxy.clone());
    {
        // monitors plugged in / out or rearranged while the app runs
        let p = proxy.clone();
        keep_alive("display watcher", move || {
            let mut last = sys::display_signature();
            loop {
                thread::sleep(Duration::from_secs(2));
                let now = sys::display_signature();
                if now != last {
                    last = now;
                    if p.send_event(Ev::Rescan).is_err() {
                        return;
                    }
                }
            }
        });
    }
    let (mut cur_idx, mut cur_left) = (screen_idx, on_left);
    audio::start(&settings["audio"], {
        let p = proxy.clone();
        move |kind, data| {
            let _ = p.send_event(Ev::Script(format!("app.audio({},{})", json!(kind), data)));
        }
    });
    let mut tray: Option<tray::Tray> = None;
    {
        // watchdog: the page sends a heartbeat every 3 s. If it goes quiet for 20 s the web view is dead
        // (WebView2 process crashed, GPU reset after sleep / lock …) and gets rebuilt.
        let p = proxy.clone();
        keep_alive("watchdog", move || {
            let mut t = now_ms();
            loop {
                thread::sleep(Duration::from_secs(3));
                let slept = now_ms().saturating_sub(t);
                t = now_ms();
                if slept > 10_000 {
                    // the PC was asleep: give the page a moment to wake up before judging it
                    HEARTBEAT.store(now_ms(), Ordering::Relaxed);
                    continue;
                }
                if now_ms().saturating_sub(HEARTBEAT.load(Ordering::Relaxed)) > 20_000 && p.send_event(Ev::Respawn).is_err() {
                    return;
                }
            }
        });
    }

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        // a panic while handling one event must not take the whole panel down
        let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match event {
            Event::NewEvents(StartCause::Init) => {
                tray = tray::Tray::new(&proxy, sys::startup_enabled());
                if !OPEN.load(Ordering::Relaxed) {
                    set_passthrough(&window, true);
                }
            }
            Event::LoopDestroyed => {
                audio::release(); // always unmute on any normal exit path
            }
            Event::UserEvent(Ev::Respawn) => {
                log("the page stopped answering: the web view is being rebuilt");
                READY.store(false, Ordering::Relaxed);
                HEARTBEAT.store(now_ms(), Ordering::Relaxed);
                OPEN.store(false, Ordering::Relaxed);
                set_passthrough(&window, true);
                let mut st = read_json(&settings_path).unwrap_or_else(|| json!({}));
                st["startup"] = json!(sys::startup_enabled());
                let nt = read_json(&notes_json).unwrap_or_else(|| json!([]));
                match build_webview(&window, &mut ctx, &proxy, &init_script(&st, &nt, &all_screens)) {
                    Ok(fresh) => {
                        webview = fresh; // the old one is dropped here
                        lk(&PINNED).clear();
                        restore_pins(proxy.clone());
                    }
                    Err(e) => log(&format!("the web view could not be rebuilt: {e}")),
                }
            }
            Event::UserEvent(Ev::Script(js)) => {
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(Ev::MoveY(center)) => {
                let (sy, sh, h, x) = (SCR_Y.load(Ordering::Relaxed), SCR_H.load(Ordering::Relaxed), PANEL_H.load(Ordering::Relaxed), PANEL_X.load(Ordering::Relaxed));
                let y = (center - h / 2).clamp(sy, (sy + sh - h).max(sy));
                window.set_outer_position(PhysicalPosition::new(x, y));
                PANEL_Y.store(y, Ordering::Relaxed);
                HANDLE_OFF.store(y + h / 2 - (sy + sh / 2), Ordering::Relaxed);
            }
            Event::UserEvent(Ev::Grab(on)) => {
                if !OPEN.load(Ordering::Relaxed) {
                    set_passthrough(&window, !on);
                }
            }
            Event::UserEvent(Ev::Rescan) => {
                let fresh = collect_screens(window.available_monitors());
                if !fresh.is_empty() {
                    all_screens = fresh;
                    if cur_idx >= all_screens.len() {
                        cur_idx = 0;
                    }
                    place(&window, &all_screens, cur_idx, cur_left);
                }
            }
            Event::WindowEvent { event: WindowEvent::ScaleFactorChanged { .. }, .. } => {
                let _ = proxy.send_event(Ev::Rescan);
            }
            Event::UserEvent(Ev::MoveEnd) => {
                let off = HANDLE_OFF.load(Ordering::Relaxed) as f64 / scale();
                let _ = webview.evaluate_script(&format!("app.handleMoved({off:.0})"));
            }
            Event::UserEvent(Ev::Open) => {
                set_passthrough(&window, false);
                let _ = webview.evaluate_script("app.open()");
            }
            Event::UserEvent(Ev::Show(panel)) => {
                sys::remember_foreground();
                OPEN.store(true, Ordering::Relaxed);
                set_passthrough(&window, false);
                let _ = webview.evaluate_script(&format!("app.show('{panel}')"));
            }
            Event::UserEvent(Ev::Menu(id)) => match id.as_str() {
                "open" => {
                    let _ = proxy.send_event(Ev::Show(""));
                }
                "settings" => {
                    let _ = proxy.send_event(Ev::Show("settings"));
                }
                "addapp" => add_app(proxy.clone(), false),
                "startup" => {
                    let on = !sys::startup_enabled();
                    sys::set_startup(on);
                    if let Some(t) = &tray {
                        t.set_startup(on);
                    }
                    let _ = webview.evaluate_script(&format!("app.syncStartup({on})"));
                }
                "quit" => {
                    *flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::UserEvent(Ev::Ipc(body)) => {
                let Ok(m) = serde_json::from_str::<Value>(&body) else { return };
                let s = |k: &str| m[k].as_str().unwrap_or_default().to_string();
                match m["t"].as_str().unwrap_or_default() {
                    "edge" => {
                        if !OPEN.swap(true, Ordering::Relaxed) {
                            sys::remember_foreground();
                            set_passthrough(&window, false);
                            let _ = webview.evaluate_script("app.open()");
                        }
                    }
                    "ready" => READY.store(true, Ordering::Relaxed),
                    "pass" => {
                        CLOSED_AT.store(now_ms(), Ordering::Relaxed);
                        set_passthrough(&window, true);
                        OPEN.store(false, Ordering::Relaxed);
                    }
                    "copy" => set_clip(&s("text")),
                    "paste" => {
                        set_clip(&s("text"));
                        sys::paste_into_previous();
                    }
                    "note" => {
                        let _ = util::write_atomic(&notes_json, m["notes"].to_string());
                    }
                    "settings" => {
                        let st = &m["settings"];
                        let want = st["startup"].as_bool().unwrap_or(false);
                        if want != sys::startup_enabled() {
                            sys::set_startup(want);
                        }
                        if let Some(t) = &tray {
                            t.set_startup(want);
                        }
                        let _ = util::write_atomic(&settings_path, st.to_string());
                    }
                    "screenshot" => sys::screenshot(),
                    "measure" => {
                        if !PICKING.swap(true, Ordering::Relaxed) {
                            let p = proxy.clone();
                            sys::measure(move |size| {
                                if let Some((w, h)) = size {
                                    OPEN.store(true, Ordering::Relaxed);
                                    let _ = p.send_event(Ev::Open);
                                    let _ = p.send_event(Ev::Script(format!("app.toast({})", json!(format!("Copied {w}x{h} px")))));
                                }
                                PICKING.store(false, Ordering::Relaxed);
                            });
                        }
                    }
                    "vaultList" => {
                        vault_emit(&proxy, "list", vault::public());
                        let p = proxy.clone();
                        thread::spawn(move || {
                            if vault::fetch_missing_logos() {
                                vault_emit(&p, "list", vault::public());
                            }
                        });
                    }
                    "vaultSave" => {
                        let entry = m["entry"].clone();
                        let pass = m["pass"].as_str().filter(|p| !p.is_empty()).map(String::from);
                        let p = proxy.clone();
                        thread::spawn(move || {
                            match vault::save(&entry, pass.as_deref()) {
                                Ok(logo_job) => {
                                    vault_emit(&p, "saved", json!({ "ok": true, "list": vault::public() }));
                                    if let Some((id, site)) = logo_job {
                                        if let Some(logo) = vault::favicon(&site) {
                                            vault::set_logo(&id, logo);
                                            vault_emit(&p, "list", vault::public());
                                        }
                                    }
                                }
                                Err(e) => vault_emit(&p, "saved", json!({ "ok": false, "msg": e })),
                            }
                        });
                    }
                    "vaultDelete" => {
                        vault::delete(&s("id"));
                        vault_emit(&proxy, "list", vault::public());
                    }
                    "vaultReveal" => {
                        let id = s("id");
                        let pw = vault::reveal(&id);
                        vault_emit(&proxy, "reveal", json!({ "id": id, "pw": pw }));
                    }
                    "vaultCopy" => {
                        let ok = vault::copy_password(&s("id"));
                        let _ = webview.evaluate_script(if ok { "app.toast('Password copied')" } else { "app.toast('Could not read that password')" });
                    }
                    "log" => log(&s("text")),
                    "addApp" => add_app(proxy.clone(), m["folder"].as_bool().unwrap_or(false)),
                    "launch" | "open" => sys::launch(&s(if m["t"] == "open" { "target" } else { "path" })),
                    "screenoff" => sys::screen_off(),
                    "shutdown" => sys::shutdown(m["secs"].as_u64().unwrap_or(0).min(315_360_000) as u32),
                    "shutdownCancel" => sys::cancel_shutdown(),
                    "refreshIcon" => {
                        let (id, path, p2) = (s("id"), s("path"), proxy.clone());
                        thread::spawn(move || {
                            if let Some(icon) = sys::icon_data_uri(&path) {
                                let _ = p2.send_event(Ev::Script(format!("app.iconFor({},{})", json!(id), json!(icon))));
                            }
                        });
                    }
                    "contextMenu" => sys::set_context_menu(m["on"].as_bool().unwrap_or(false)),
                    "placement" => {
                        let left = m["edge"] == "left";
                        let idx = m["monitor"].as_u64().unwrap_or(0) as usize;
                        let fresh = collect_screens(window.available_monitors());
                        if !fresh.is_empty() {
                            all_screens = fresh;
                        }
                        (cur_idx, cur_left) = (if idx < all_screens.len() { idx } else { 0 }, left);
                        place(&window, &all_screens, cur_idx, cur_left);
                        let _ = webview.evaluate_script(&format!("app.placed({})", json!({ "edge": if left { "left" } else { "right" } })));
                    }
                    "pinImage" => {
                        let id: u64 = s("id").parse().unwrap_or(0);
                        let on = m["on"].as_bool().unwrap_or(false);
                        let p = proxy.clone();
                        thread::spawn(move || {
                            let msg = match pin_image(id, on) {
                                Err(e) => e,
                                Ok(()) if on => "Image pinned",
                                Ok(()) => "Image unpinned",
                            };
                            let _ = p.send_event(Ev::Script(format!("app.toast({})", json!(msg))));
                        });
                    }
                    "clipImage" => {
                        let id: u64 = s("id").parse().unwrap_or(0);
                        let found = lk(&IMAGES)
                            .iter()
                            .find(|(i, ..)| *i == id)
                            .cloned()
                            .or_else(|| lk(&PINNED).iter().find(|(i, ..)| *i == id).cloned());
                        let ok = found.map(|(_, w, h, px)| sys::set_clip_image(w, h, &px)).unwrap_or(false);
                        LAST_IMAGE.store(id, Ordering::Relaxed);
                        let _ = webview.evaluate_script(if ok { "app.toast('Image copied')" } else { "app.toast('Could not copy that image')" });
                    }
                    "netInfo" => {
                        if s("only").is_empty() {
                            let p = proxy.clone();
                            thread::spawn(move || net_emit(&p, "local", json!(net::local_ip())));
                            let p = proxy.clone();
                            thread::spawn(move || net_emit(&p, "public", json!(net::public_ip())));
                        }
                        let p = proxy.clone();
                        thread::spawn(move || net_emit(&p, "dns", json!(net::current_dns())));
                    }
                    "netPing" => {
                        let (host, p) = (s("host"), proxy.clone());
                        thread::spawn(move || net::ping(host.trim(), |v| net_emit(&p, "ping", v)));
                    }
                    "netFlush" => {
                        let p = proxy.clone();
                        thread::spawn(move || {
                            let ok = net::flush_dns();
                            let msg = if ok { "DNS cache flushed" } else { "Could not flush the DNS cache" };
                            net_emit(&p, "dnsDone", json!({ "ok": ok, "msg": msg }));
                        });
                    }
                    "netDns" => {
                        let servers: Vec<String> =
                            m["servers"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                        let p = proxy.clone();
                        thread::spawn(move || {
                            let (ok, msg) = match net::set_dns(&servers) {
                                Ok(()) if servers.is_empty() => (true, "DNS reset to automatic".to_string()),
                                Ok(()) => (true, format!("DNS set to {}", servers.join(", "))),
                                Err(e) => (false, e),
                            };
                            net_emit(&p, "dnsDone", json!({ "ok": ok, "msg": msg }));
                        });
                    }
                    "audioState" => audio::state(),
                    "audioToggle" => audio::toggle(&s("which")),
                    "audioCfg" => audio::configure(&m["cfg"]),
                    "audioCapture" => audio::capture(m["on"].as_bool().unwrap_or(false)),
                    "netWifi" => {
                        let (admin, p) = (m["admin"].as_bool().unwrap_or(false), proxy.clone());
                        thread::spawn(move || net_emit(&p, "wifi", net::wifi(admin)));
                    }
                    "quit" => {
                        *flow = ControlFlow::Exit;
                    }
                    _ => {}
                }
            }
            Event::WindowEvent { event: WindowEvent::Focused(false), .. } => {
                let _ = webview.evaluate_script("app.blur()");
            }
            _ => {}
        }));
        if guarded.is_err() {
            log("a panic in the UI loop was caught and ignored");
        }
    });
}