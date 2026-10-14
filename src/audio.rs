//! Mute / unmute the microphone and the system sound, each with its own global shortcut.
//!
//! * Muting uses the Windows Core Audio COM interfaces (declared by hand below, so no new crate or crate feature is needed).
//!   The microphone switch affects every active recording device, the sound switch the default playback device.
//! * Shortcuts use `RegisterHotKey`, the plain documented Windows API: nothing is hooked, injected or simulated,
//!   and the key itself is not read, only the registered combination is reported.
//! * Toggle only (press = flip). Shortcuts are off until the user turns them on in the panel.

use std::{
    ffi::c_void,
    mem::{transmute, zeroed},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex,
    },
    thread,
};

use serde_json::{json, Value};
use windows_sys::core::GUID;

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const c_void, coinit: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterHotKey(hwnd: isize, id: i32, mods: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: isize, id: i32) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32, remove: u32) -> i32;
    fn PostThreadMessageW(thread: u32, msg: u32, w: usize, l: isize) -> i32;
}

#[link(name = "winmm")]
unsafe extern "system" {
    fn PlaySoundW(data: *const c_void, module: isize, flags: u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn WaitForSingleObject(handle: isize, ms: u32) -> u32;
    fn CloseHandle(handle: isize) -> i32;
}

#[repr(C)]
struct Msg {
    hwnd: isize,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

const WM_HOTKEY: u32 = 0x0312;
const WM_REBIND: u32 = 0x8001; // WM_APP + 1: "re-read the settings and register the shortcuts again"
const MOD_NOREPEAT: u32 = 0x4000; // holding the keys down must not flip the state over and over
const ID_MIC: i32 = 1;
const ID_SPK: i32 = 2;

const CLSID_ENUMERATOR: GUID = GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
const IID_ENUMERATOR: GUID = GUID::from_u128(0xA95664D2_9614_4F35_A746_DE8DB63617E6);
const IID_ENDPOINT_VOLUME: GUID = GUID::from_u128(0x5CDF2C82_841E_4546_9722_0CF74078229A);
const CLSCTX_ALL: u32 = 0x17;

// PlaySound flags: the sound is played from memory, never falls back to the default "ding"
const SND_ASYNC: u32 = 0x1;
const SND_NODEFAULT: u32 = 0x2;
const SND_MEMORY: u32 = 0x4;

// the voice clips are compiled into the exe (put the four files in <project>/assets/)
static WAV_MIC_MUTED: &[u8] = include_bytes!("../assets/mic_muted.wav");
static WAV_MIC_ON: &[u8] = include_bytes!("../assets/mic_activated.wav");
static WAV_SPK_MUTED: &[u8] = include_bytes!("../assets/sound_muted.wav");
static WAV_SPK_ON: &[u8] = include_bytes!("../assets/sound_resumed.wav");

/* ---------------- settings shared with the UI ---------------- */
#[derive(Clone)]
struct Cfg {
    enabled: bool,
    mic: Option<(u32, u32)>, // (modifiers, virtual-key)
    spk: Option<(u32, u32)>,
}

static CFG: Mutex<Cfg> = Mutex::new(Cfg { enabled: false, mic: None, spk: None });
static NOTIFY: Mutex<Option<Box<dyn Fn(&str, Value) + Send>>> = Mutex::new(None);
static TID: AtomicU32 = AtomicU32::new(0);
/// While the user is recording a new shortcut in the panel, the old ones are released so they can be pressed again.
static PAUSED: AtomicBool = AtomicBool::new(false);
static BIND_MIC: AtomicBool = AtomicBool::new(true);
static BIND_SPK: AtomicBool = AtomicBool::new(true);
/// True while *this app* is the one that muted the device, so closing the app only undoes its own mute
/// (a device the user had muted in Windows before is left alone).
static MIC_BY_US: AtomicBool = AtomicBool::new(false);
static SPK_BY_US: AtomicBool = AtomicBool::new(false);
static GUARD: AtomicBool = AtomicBool::new(false);
/// Only one toggle at a time (the "sound muted" clip takes a second, a quick second key press must not race it).
static TOGGLE_LOCK: Mutex<()> = Mutex::new(());

/* ---------------- undo our own mute, whatever happens to the app ---------------- */
// 1. normal exit  -> release()
// 2. killed / crashed -> a tiny helper process (this same exe started with --audio-guard <pid>) waits for the app to end and unmutes
// 3. power loss / the helper was killed too -> the next start finds audio-muted.json and unmutes
fn state_path() -> std::path::PathBuf {
    crate::sys::data_dir().join("audio-muted.json")
}

/// Remembers on disk what this app has muted (the file is gone when nothing is muted by us).
fn save_state() {
    let (m, s) = (MIC_BY_US.load(Ordering::Relaxed), SPK_BY_US.load(Ordering::Relaxed));
    let p = state_path();
    if m || s {
        let _ = crate::util::write_atomic(&p, json!({ "mic": m, "spk": s }).to_string());
    } else {
        let _ = std::fs::remove_file(&p);
    }
}

/// Starts the helper once per session, the first time we mute something.
fn ensure_guard() {
    if GUARD.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new(exe)
            .arg("--audio-guard")
            .arg(std::process::id().to_string())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
    }
}

/// Unmutes whatever audio-muted.json says this app had muted, then deletes the file.
pub fn recover() {
    let p = state_path();
    let Some(v) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
    with_com(|| {
        for (speaker, key) in [(false, "mic"), (true, "spk")] {
            if v[key].as_bool().unwrap_or(false) {
                for x in volumes(speaker) {
                    set_mute(&x, false);
                }
            }
        }
    });
    let _ = std::fs::remove_file(&p);
}

/// Body of the helper process: wait until the app (pid) is gone, however it ended, then undo its mute.
pub fn guard(pid: u32) {
    unsafe {
        let h = OpenProcess(0x0010_0000, 0, pid); // SYNCHRONIZE
        if h != 0 {
            WaitForSingleObject(h, 0xFFFF_FFFF);
            CloseHandle(h);
        }
    }
    recover();
}

fn key(v: &Value) -> Option<(u32, u32)> {
    let mods = (v["mods"].as_u64()? as u32) & 0xF; // 1 Alt, 2 Ctrl, 4 Shift, 8 Win
    let vk = v["vk"].as_u64()? as u32;
    (1..=254).contains(&vk).then_some((mods, vk))
}

fn parse(v: &Value) -> Cfg {
    Cfg {
        enabled: v["enabled"].as_bool().unwrap_or(false),
        mic: key(&v["mic"]),
        spk: key(&v["spk"]),
    }
}

fn emit(kind: &str, data: Value) {
    if let Ok(g) = NOTIFY.lock() {
        if let Some(f) = g.as_ref() {
            f(kind, data);
        }
    }
}

/* ---------------- Core Audio (COM) ---------------- */
struct Obj(*mut c_void);

impl Drop for Obj {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let release: extern "system" fn(*mut c_void) -> u32 = transmute(vfn(self.0, 2));
                release(self.0);
            }
        }
    }
}

/// Function number `idx` of a COM object's virtual table.
unsafe fn vfn(obj: *mut c_void, idx: usize) -> *const c_void {
    unsafe {
        let vtbl = *(obj as *const *const *const c_void);
        *vtbl.add(idx)
    }
}

/// COM is set up for the calling thread only while `f` runs; every COM object must be dropped inside `f`.
fn with_com<T>(f: impl FnOnce() -> T) -> T {
    let hr = unsafe { CoInitializeEx(null(), 0) }; // COINIT_MULTITHREADED
    let r = f();
    if hr >= 0 {
        unsafe { CoUninitialize() };
    }
    r
}

fn enumerator() -> Option<Obj> {
    let mut p: *mut c_void = null_mut();
    let hr = unsafe { CoCreateInstance(&CLSID_ENUMERATOR, null_mut(), CLSCTX_ALL, &IID_ENUMERATOR, &mut p) };
    (hr >= 0 && !p.is_null()).then(|| Obj(p))
}

/// IAudioEndpointVolume of every active microphone (`speaker` = false) or of the default playback device.
fn volumes(speaker: bool) -> Vec<Obj> {
    let Some(en) = enumerator() else { return Vec::new() };
    let mut devices: Vec<Obj> = Vec::new();
    unsafe {
        if speaker {
            // IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)
            let get_default: extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32 = transmute(vfn(en.0, 4));
            let mut d: *mut c_void = null_mut();
            if get_default(en.0, 0, 0, &mut d) >= 0 && !d.is_null() {
                devices.push(Obj(d));
            }
        } else {
            // IMMDeviceEnumerator::EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            let enum_endpoints: extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32 = transmute(vfn(en.0, 3));
            let mut coll: *mut c_void = null_mut();
            if enum_endpoints(en.0, 1, 1, &mut coll) >= 0 && !coll.is_null() {
                let coll = Obj(coll);
                let count: extern "system" fn(*mut c_void, *mut u32) -> i32 = transmute(vfn(coll.0, 3));
                let item: extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = transmute(vfn(coll.0, 4));
                let mut n = 0u32;
                if count(coll.0, &mut n) >= 0 {
                    for i in 0..n {
                        let mut d: *mut c_void = null_mut();
                        if item(coll.0, i, &mut d) >= 0 && !d.is_null() {
                            devices.push(Obj(d));
                        }
                    }
                }
            }
        }
        devices
            .iter()
            .filter_map(|d| {
                // IMMDevice::Activate(IID_IAudioEndpointVolume)
                let activate: extern "system" fn(*mut c_void, *const GUID, u32, *const c_void, *mut *mut c_void) -> i32 = transmute(vfn(d.0, 3));
                let mut v: *mut c_void = null_mut();
                (activate(d.0, &IID_ENDPOINT_VOLUME, CLSCTX_ALL, null(), &mut v) >= 0 && !v.is_null()).then(|| Obj(v))
            })
            .collect()
    }
}

fn get_mute(v: &Obj) -> Option<bool> {
    unsafe {
        let f: extern "system" fn(*mut c_void, *mut i32) -> i32 = transmute(vfn(v.0, 15)); // IAudioEndpointVolume::GetMute
        let mut m = 0i32;
        (f(v.0, &mut m) >= 0).then_some(m != 0)
    }
}

fn set_mute(v: &Obj, on: bool) -> bool {
    unsafe {
        let f: extern "system" fn(*mut c_void, i32, *const GUID) -> i32 = transmute(vfn(v.0, 14)); // IAudioEndpointVolume::SetMute
        f(v.0, on as i32, null()) >= 0
    }
}

/// Some(true) = muted, Some(false) = on, None = no such device.
fn state_of(speaker: bool) -> Option<bool> {
    let v = volumes(speaker);
    if v.is_empty() {
        return None;
    }
    Some(v.iter().all(|x| get_mute(x) == Some(true)))
}

/// Plays the voice clip for this device and state. `wait` = return only when it has finished.
fn play(speaker: bool, muted: bool, wait: bool) {
    let wav = match (speaker, muted) {
        (false, true) => WAV_MIC_MUTED,
        (false, false) => WAV_MIC_ON,
        (true, true) => WAV_SPK_MUTED,
        (true, false) => WAV_SPK_ON,
    };
    let flags = SND_MEMORY | SND_NODEFAULT | if wait { 0 } else { SND_ASYNC };
    unsafe { PlaySoundW(wav.as_ptr() as *const c_void, 0, flags) };
}

/// Flips the state. Returns the new state (true = muted), or None when there is no device / it could not be changed.
fn do_toggle(speaker: bool) -> Option<bool> {
    with_com(|| {
        let v = volumes(speaker);
        if v.is_empty() {
            return None;
        }
        let want = !v.iter().all(|x| get_mute(x) == Some(true));
        // once the speakers are muted nobody would hear the clip, so this one is played to the end first
        if speaker && want {
            play(true, true, true);
        }
        let ok = v.iter().map(|x| set_mute(x, want)).fold(false, |a, b| a || b);
        if !ok {
            return None;
        }
        (if speaker { &SPK_BY_US } else { &MIC_BY_US }).store(want, Ordering::Relaxed);
        save_state();
        if want {
            ensure_guard();
        }
        if !(speaker && want) {
            play(speaker, want, false);
        }
        Some(want)
    })
}

fn emit_state() {
    let (mic, spk) = with_com(|| (state_of(false), state_of(true)));
    emit("state", json!({ "mic": mic, "spk": spk }));
    emit("bind", json!({ "mic": BIND_MIC.load(Ordering::Relaxed), "spk": BIND_SPK.load(Ordering::Relaxed) }));
}

fn run_toggle(speaker: bool) {
    let _one_at_a_time = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let which = if speaker { "spk" } else { "mic" };
    match do_toggle(speaker) {
        Some(muted) => emit("toggled", json!({ "which": which, "muted": muted })),
        None => emit("fail", json!({ "which": which })),
    }
    emit_state();
}

/* ---------------- shortcuts ---------------- */
fn rebind() {
    unsafe {
        UnregisterHotKey(0, ID_MIC);
        UnregisterHotKey(0, ID_SPK);
    }
    let cfg = match CFG.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    if PAUSED.load(Ordering::Relaxed) {
        return;
    }
    let (mut mic_ok, mut spk_ok) = (true, true);
    if cfg.enabled {
        if let Some((mods, vk)) = cfg.mic {
            mic_ok = unsafe { RegisterHotKey(0, ID_MIC, mods | MOD_NOREPEAT, vk) } != 0;
        }
        if let Some((mods, vk)) = cfg.spk {
            spk_ok = unsafe { RegisterHotKey(0, ID_SPK, mods | MOD_NOREPEAT, vk) } != 0; // fails if another program owns the combination
        }
    }
    BIND_MIC.store(mic_ok, Ordering::Relaxed);
    BIND_SPK.store(spk_ok, Ordering::Relaxed);
    emit("bind", json!({ "mic": mic_ok, "spk": spk_ok }));
}

fn hotkey_loop() {
    let mut msg: Msg = unsafe { zeroed() };
    unsafe {
        PeekMessageW(&mut msg, 0, 0, 0, 0); // makes sure this thread has a message queue before others post to it
        TID.store(GetCurrentThreadId(), Ordering::SeqCst);
    }
    rebind();
    loop {
        let r = unsafe { GetMessageW(&mut msg, 0, 0, 0) };
        if r <= 0 {
            break;
        }
        match msg.message {
            WM_HOTKEY => match msg.wparam as i32 {
                // own thread: the shortcut loop must stay free while a clip is playing
                ID_MIC => {
                    thread::spawn(|| run_toggle(false));
                }
                ID_SPK => {
                    thread::spawn(|| run_toggle(true));
                }
                _ => {}
            },
            WM_REBIND => rebind(),
            _ => {}
        }
    }
}

fn poke() {
    let t = TID.load(Ordering::SeqCst);
    if t != 0 {
        unsafe { PostThreadMessageW(t, WM_REBIND, 0, 0) };
    }
}

/* ---------------- what main.rs uses ---------------- */
/// Starts the shortcut thread. `cfg` is the "audio" object from settings.json; `notify(kind, data)` reports back to the page.
pub fn start(cfg: &Value, notify: impl Fn(&str, Value) + Send + 'static) {
    if let Ok(mut c) = CFG.lock() {
        *c = parse(cfg);
    }
    if let Ok(mut n) = NOTIFY.lock() {
        *n = Some(Box::new(notify));
    }
    thread::spawn(recover); // the last session ended without a clean exit (power loss…): undo its mute now
    thread::spawn(hotkey_loop);
}

/// The page changed the shortcuts / switches.
pub fn configure(cfg: &Value) {
    if let Ok(mut c) = CFG.lock() {
        *c = parse(cfg);
    }
    poke();
}

/// The page is recording a new shortcut (true) or has finished (false).
pub fn capture(on: bool) {
    PAUSED.store(on, Ordering::Relaxed);
    poke();
}

/// Sends the current mute state of both devices to the page.
pub fn state() {
    thread::spawn(emit_state);
}

/// Flips "mic" or "spk" (the buttons in the panel).
pub fn toggle(which: &str) {
    let speaker = which == "spk";
    thread::spawn(move || run_toggle(speaker));
}

/// Call right before the app exits: unmutes whatever this app muted, so the microphone / sound never stays muted after it closes.
pub fn release() {
    with_com(|| {
        for speaker in [false, true] {
            let flag = if speaker { &SPK_BY_US } else { &MIC_BY_US };
            if flag.swap(false, Ordering::Relaxed) {
                for v in volumes(speaker) {
                    set_mute(&v, false);
                }
            }
        }
    });
    save_state(); // nothing left muted by us: removes the file, so the helper has nothing to do
}
