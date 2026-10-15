//! Windows implementation: Win32 APIs for paste, keys, startup, icons…

use std::{
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicIsize, Ordering},
        OnceLock,
    },
    thread,
    time::Duration,
};

use crate::util;
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, GlobalFree, ERROR_ALREADY_EXISTS, HANDLE, HWND, POINT},
    Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        BI_BITFIELDS, BI_RGB, DIB_RGB_COLORS,
    },
    Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
    System::{
        Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED},
        DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
        Ole::CF_DIB,
        Registry::{RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ},
        Threading::CreateMutexW,
    },
    UI::{
        Controls::{
            Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NODEREFERENCELINKS, OFN_PATHMUSTEXIST, OPENFILENAMEW},
            ImageList_GetIcon, HIMAGELIST, ILD_TRANSPARENT,
        },
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_ESCAPE, VK_LBUTTON, VK_V,
        },
        Shell::{
            SHBrowseForFolderW, SHGetFileInfoW, SHGetImageList, SHGetPathFromIDListW, ShellExecuteW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
            BROWSEINFOW, SHFILEINFOW, SHGFI_SYSICONINDEX, SHIL_EXTRALARGE,
        },
        WindowsAndMessaging::{
            DestroyIcon, GetCursorPos, GetForegroundWindow, GetIconInfo, PostMessageW, PrivateExtractIconsW, SetForegroundWindow, HICON,
            ICONINFO, SC_MONITORPOWER, SW_SHOWNORMAL, WM_SYSCOMMAND,
        },
    },
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_NAME: &str = "WidgetPanel";

static PREV_FG: AtomicIsize = AtomicIsize::new(0);
static SELF_HWND: AtomicIsize = AtomicIsize::new(0);
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            let base = PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into()));
            let dir = base.join("WidgetPanel");
            let _ = fs::create_dir_all(&dir);
            dir
        })
        .clone()
}

pub fn home() -> String {
    std::env::var("USERPROFILE").unwrap_or_default()
}

pub fn set_self_window(hwnd: isize) {
    SELF_HWND.store(hwnd, Ordering::Relaxed);
}

/* ---------------- startup (HKCU\...\Run) ---------------- */
fn reg_get(name: &str) -> bool {
    let (k, v) = (wide(RUN_KEY), wide(name));
    unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) == 0 }
}

fn reg_del(name: &str) {
    let (k, v) = (wide(RUN_KEY), wide(name));
    unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) };
}

pub fn startup_enabled() -> bool {
    reg_get(RUN_NAME)
}

pub fn set_startup(on: bool) {
    if !on {
        return reg_del(RUN_NAME);
    }
    let (k, v) = (wide(RUN_KEY), wide(RUN_NAME));
    let exe = std::env::current_exe().map(|p| format!("\"{}\"", p.display())).unwrap_or_default();
    let data = wide(&exe);
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32) };
}

/* ---------------- pointer ---------------- */
/// Click-through switch for the panel window: flips only WS_EX_TRANSPARENT.
pub fn click_through(on: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW, GWL_EXSTYLE, WS_EX_TRANSPARENT};
    let hwnd = SELF_HWND.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
        if ex == 0 {
            return;
        }
        let want = if on { ex | WS_EX_TRANSPARENT as i32 } else { ex & !(WS_EX_TRANSPARENT as i32) };
        if want != ex {
            SetWindowLongW(hwnd, GWL_EXSTYLE, want);
        }
    }
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}

pub fn left_down() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON as i32) < 0 }
}

/// Remember the app the user was in, so paste / pin can target it.
pub fn remember_foreground() {
    let fg = unsafe { GetForegroundWindow() } as isize;
    if fg != SELF_HWND.load(Ordering::Relaxed) {
        PREV_FG.store(fg, Ordering::Relaxed);
    }
}

pub fn clip_seq() -> u64 {
    unsafe { GetClipboardSequenceNumber() as u64 }
}

/* ---------------- keyboard ---------------- */
fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 } },
    }
}

fn send_combo(combo: &[u16]) {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(combo.len() * 2);
    inputs.extend(combo.iter().map(|&k| key(k, false)));
    inputs.extend(combo.iter().rev().map(|&k| key(k, true)));
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}

/// Give focus back to the app the user was in, then press Ctrl+V.
pub fn paste_into_previous() {
    thread::spawn(|| {
        let prev = PREV_FG.load(Ordering::Relaxed);
        if prev != 0 {
            unsafe { SetForegroundWindow(prev as HWND) };
        }
        thread::sleep(Duration::from_millis(90));
        send_combo(&[VK_CONTROL, VK_V]);
    });
}

/// Give focus back to the app the user was in (no keystrokes).
pub fn restore_foreground() {
    let prev = PREV_FG.load(Ordering::Relaxed);
    if prev != 0 {
        unsafe { SetForegroundWindow(prev as HWND) };
    }
}

/// Schedules a shutdown with the built-in shutdown.exe (replacing one that is already pending).
pub fn shutdown(secs: u32) {
    use std::{os::windows::process::CommandExt, process::Command};
    thread::spawn(move || {
        let run = |args: &[&str]| Command::new(crate::net::system_exe("shutdown")).args(args).creation_flags(0x0800_0000).status();
        let _ = run(&["/a"]);
        let _ = run(&["/s", "/t", &secs.to_string()]);
    });
}

/// Cancels a pending shutdown.
pub fn cancel_shutdown() {
    use std::{os::windows::process::CommandExt, process::Command};
    thread::spawn(|| {
        let _ = Command::new(crate::net::system_exe("shutdown")).arg("/a").creation_flags(0x0800_0000).status();
    });
}

/* ---------------- system actions ---------------- */
pub fn screen_off() {
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(600));
        unsafe { PostMessageW(SELF_HWND.load(Ordering::Relaxed) as HWND, WM_SYSCOMMAND, SC_MONITORPOWER as usize, 2) };
    });
}

pub fn screenshot() {
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(350));
        let _ = std::process::Command::new(crate::net::windows_dir().join("explorer.exe")).arg("ms-screenclip:").spawn();
    });
}

/// The small tag next to the cursor never takes the mouse.
unsafe extern "system" fn card_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_NCHITTEST};
    unsafe {
        if msg == WM_NCHITTEST {
            return -1; // HTTRANSPARENT
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// Signed distance to a rounded rectangle centred on the origin (half sizes hx, hy, corner radius rad).
fn rrect_sd(px: f32, py: f32, hx: f32, hy: f32, rad: f32) -> f32 {
    let (qx, qy) = (px.abs() - (hx - rad), py.abs() - (hy - rad));
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad
}

/* ---------------- on-screen measuring ---------------- */
static MEAS_STATE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static MEAS_CURSOR: AtomicIsize = AtomicIsize::new(0);
static MEAS_BRUSH: AtomicIsize = AtomicIsize::new(0);
static MEAS_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static MEAS_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

unsafe extern "system" fn measure_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, SetCursor, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_RBUTTONDOWN, WM_SETCURSOR,
    };
    unsafe {
        match msg {
            WM_SETCURSOR => {
                SetCursor(MEAS_CURSOR.load(Ordering::Relaxed) as *mut c_void);
                1
            }
            WM_LBUTTONDOWN => {
                MEAS_X.store((lp & 0xffff) as i16 as i32, Ordering::Relaxed);
                MEAS_Y.store(((lp >> 16) & 0xffff) as i16 as i32, Ordering::Relaxed);
                MEAS_STATE.store(1, Ordering::Relaxed);
                0
            }
            WM_LBUTTONUP => {
                let _ = MEAS_STATE.compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed);
                0
            }
            WM_RBUTTONDOWN => {
                MEAS_STATE.store(3, Ordering::Relaxed);
                0
            }
            WM_MOUSEACTIVATE => 3, // MA_NOACTIVATE
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Draws the "W × H px" tag: a small dark rounded card with centred text (smooth, anti-aliased corners).
unsafe fn render_size_card(dc: *mut c_void, bits: *mut c_void, w: i32, h: i32, k: f32, text: &str) {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{CreateFontW, DrawTextW, GdiFlush, SelectObject, SetBkMode, SetTextColor},
    };
    unsafe {
        let cover = |d: f32| (0.5 - d).clamp(0.0, 1.0);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let (rad, bw) = (6.0 * k, 1.0 * k);
        let count = (w * h) as usize;
        let mut cov = vec![0u8; count];
        {
            let px = std::slice::from_raw_parts_mut(bits as *mut u32, count);
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) as usize;
                    let (fx, fy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                    let d = rrect_sd(fx, fy, cx, cy, rad);
                    cov[i] = (cover(d) * 255.0) as u8;
                    let t = (0.5 + d + bw).clamp(0.0, 1.0);
                    let mix = |bg: f32, bd: f32| (bg + (bd - bg) * t) as u32;
                    px[i] = 0xFF00_0000 | (mix(24.0, 78.0) << 16) | (mix(24.0, 78.0) << 8) | mix(28.0, 86.0);
                }
            }
        }
        SetBkMode(dc, 1);
        let face = wide("Segoe UI");
        let font = CreateFontW(-((12.0 * k) as i32), 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 4, 0, face.as_ptr());
        let old = SelectObject(dc, font as _);
        SetTextColor(dc, 0x00F5_F5F5);
        let t = wide(text);
        let mut rc = RECT { left: 0, top: 0, right: w, bottom: h };
        DrawTextW(dc, t.as_ptr(), -1, &mut rc, 2085);
        SelectObject(dc, old);
        DeleteObject(font as _);
        GdiFlush();
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, count);
        for i in 0..count {
            let a = cov[i] as u32;
            let p = px[i];
            let (rr, gg, bb) = (((p >> 16) & 0xff) * a / 255, ((p >> 8) & 0xff) * a / 255, (p & 0xff) * a / 255);
            px[i] = (a << 24) | (rr << 16) | (gg << 8) | bb;
        }
    }
}

unsafe fn run_measure() -> Option<(i32, i32)> {
    use windows_sys::Win32::{
        Foundation::SIZE,
        Graphics::Gdi::{CreateDIBSection, CreateSolidBrush, GetDeviceCaps, SelectObject, BLENDFUNCTION},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetSystemMetrics, LoadCursorW, MoveWindow, PeekMessageW, RegisterClassW,
            SetLayeredWindowAttributes, ShowWindow, UpdateLayeredWindow, LWA_ALPHA, MSG, PM_REMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
            WS_EX_TRANSPARENT, WS_POPUP,
        },
    };
    unsafe {
        while GetAsyncKeyState(VK_LBUTTON as i32) < 0 {
            thread::sleep(Duration::from_millis(10));
        }
        let sdc = GetDC(std::ptr::null_mut());
        let k = (GetDeviceCaps(sdc, 88) as f32 / 96.0).max(1.0);
        ReleaseDC(std::ptr::null_mut(), sdc);

        let cursor = LoadCursorW(std::ptr::null_mut(), 32515 as *const u16);
        MEAS_CURSOR.store(cursor as isize, Ordering::Relaxed);
        MEAS_STATE.store(0, Ordering::Relaxed);
        if MEAS_BRUSH.load(Ordering::Relaxed) == 0 {
            MEAS_BRUSH.store(CreateSolidBrush(0x00FF_9A3C) as isize, Ordering::Relaxed);
        }
        let brush = MEAS_BRUSH.load(Ordering::Relaxed) as *mut c_void;

        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("WidgetPanelMeasure");
        let mark_class = wide("WidgetPanelMeasureMark");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(measure_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: cursor,
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let wc2 = WNDCLASSW { lpfnWndProc: Some(card_proc), hCursor: std::ptr::null_mut(), hbrBackground: brush, lpszClassName: mark_class.as_ptr(), ..wc };
        RegisterClassW(&wc2);

        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            crate::log(&format!("measure: CreateWindowExW failed (err {})", GetLastError()));
            return None;
        }
        SetLayeredWindowAttributes(hwnd, 0, 2, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOWNA);

        let ex = WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
        let mark = |alpha: u8| {
            let w = CreateWindowExW(ex, mark_class.as_ptr(), mark_class.as_ptr(), WS_POPUP, 0, 0, 1, 1, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null());
            if !w.is_null() {
                SetLayeredWindowAttributes(w, 0, alpha, LWA_ALPHA);
            }
            w
        };
        let edges = [mark(255), mark(255), mark(255), mark(255)];
        let fill = mark(55);

        let (cw, ch) = ((124.0 * k) as i32, (22.0 * k) as i32);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = cw;
        bi.bmiHeader.biHeight = -ch;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = 0;
        let mut card_bits: *mut c_void = std::ptr::null_mut();
        let card_bmp = CreateDIBSection(std::ptr::null_mut(), &bi, 0, &mut card_bits, std::ptr::null_mut(), 0);
        let card_dc = CreateCompatibleDC(std::ptr::null_mut());
        let card_old = SelectObject(card_dc, card_bmp as _);
        let card = if card_bmp.is_null() || card_bits.is_null() || card_dc.is_null() {
            std::ptr::null_mut()
        } else {
            CreateWindowExW(ex, mark_class.as_ptr(), mark_class.as_ptr(), WS_POPUP, 0, 0, cw, ch, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null())
        };

        let th = (k.round() as i32).max(1);
        let mut start = (0i32, 0i32);
        let mut dragging = false;
        let (mut frame_shown, mut card_shown) = (false, false);
        let mut last_pt = (i32::MIN, i32::MIN);
        let mut result: Option<(i32, i32)> = None;
        let mut msg: MSG = std::mem::zeroed();

        loop {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&msg);
            }
            let st = MEAS_STATE.load(Ordering::Relaxed);
            if st == 3 || GetAsyncKeyState(VK_ESCAPE as i32) < 0 {
                break;
            }
            if st == 1 && !dragging {
                dragging = true;
                start = (vx + MEAS_X.load(Ordering::Relaxed), vy + MEAS_Y.load(Ordering::Relaxed));
                last_pt = (i32::MIN, i32::MIN);
            }
            if st == 2 && !dragging {
                MEAS_STATE.store(0, Ordering::Relaxed);
            }
            if dragging {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                let (dx, dy) = ((p.x - start.0).abs(), (p.y - start.1).abs());
                if (p.x, p.y) != last_pt {
                    last_pt = (p.x, p.y);
                    let (l, t) = (start.0.min(p.x), start.1.min(p.y));
                    let (w, h) = (dx.max(th), dy.max(th));
                    MoveWindow(edges[0], l, t, w, th, 1);
                    MoveWindow(edges[1], l, t + h - th, w, th, 1);
                    MoveWindow(edges[2], l, t, th, h, 1);
                    MoveWindow(edges[3], l + w - th, t, th, h, 1);
                    MoveWindow(fill, l, t, w, h, 1);
                    if !frame_shown {
                        for e in edges {
                            ShowWindow(e, SW_SHOWNA);
                        }
                        ShowWindow(fill, SW_SHOWNA);
                        frame_shown = true;
                    }
                    if !card.is_null() {
                        render_size_card(card_dc, card_bits, cw, ch, k, &format!("{dx} × {dy} px"));
                        let off = (14.0 * k) as i32;
                        let (mut x, mut y) = (p.x + off, p.y + off);
                        if x + cw > vx + vw {
                            x = p.x - off - cw;
                        }
                        if y + ch > vy + vh {
                            y = p.y - off - ch;
                        }
                        let (pos, size, src) = (POINT { x, y }, SIZE { cx: cw, cy: ch }, POINT { x: 0, y: 0 });
                        let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
                        UpdateLayeredWindow(card, std::ptr::null_mut(), &pos, &size, card_dc, &src, 0, &blend, 2);
                        if !card_shown {
                            ShowWindow(card, SW_SHOWNA);
                            card_shown = true;
                        }
                    }
                }
                if st == 2 {
                    dragging = false;
                    MEAS_STATE.store(0, Ordering::Relaxed);
                    if dx >= 2 || dy >= 2 {
                        result = Some((dx, dy));
                        crate::set_clip(&format!("{dx}x{dy}"));
                    }
                }
            }
            thread::sleep(Duration::from_millis(8));
        }

        for e in edges {
            if !e.is_null() {
                DestroyWindow(e);
            }
        }
        if !fill.is_null() {
            DestroyWindow(fill);
        }
        if !card.is_null() {
            DestroyWindow(card);
        }
        DestroyWindow(hwnd);
        if !card_dc.is_null() {
            SelectObject(card_dc, card_old);
            DeleteDC(card_dc);
        }
        if !card_bmp.is_null() {
            DeleteObject(card_bmp as _);
        }
        result
    }
}

/* ---------------- DPAPI: saved logins ---------------- */
pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB},
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        if CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 1, &mut out) == 0 || out.pbData.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(v)
    }
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB},
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        if CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 1, &mut out) == 0 || out.pbData.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(v)
    }
}

pub fn measure(done: impl FnOnce(Option<(i32, i32)>) + Send + 'static) {
    thread::spawn(move || done(unsafe { run_measure() }));
}

pub fn pick_file() -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let filter: Vec<u16> = "Apps and shortcuts\0*.exe;*.lnk;*.url;*.bat;*.cmd\0All files\0*.*\0\0".encode_utf16().collect();
    let start = wide(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs");
    let title = wide("Add app to Widget Panel");
    unsafe {
        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = buf.as_mut_ptr();
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrInitialDir = start.as_ptr();
        ofn.lpstrTitle = title.as_ptr();
        ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NODEREFERENCELINKS;
        if GetOpenFileNameW(&mut ofn) == 0 {
            return None;
        }
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Starts / opens a file, folder or shell address. Runs on its own thread: a slow network path or a UAC prompt must never freeze the panel.
pub fn launch(path: &str) {
    let path = path.to_string();
    thread::spawn(move || {
        let (op, p) = (wide("open"), wide(&path));
        let dir = Path::new(&path).parent().filter(|d| d.is_dir()).map(|d| wide(&d.to_string_lossy()));
        let dir_ptr = dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        unsafe {
            // ShellExecute wants COM on the calling thread
            let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as _);
            ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), p.as_ptr(), std::ptr::null(), dir_ptr, SW_SHOWNORMAL);
            if hr >= 0 {
                CoUninitialize();
            }
        }
    });
}

/// Icon of any file (exe gets a crisp 64px icon, shortcuts use the shell icon) as a PNG data URI.
pub fn icon_data_uri(path: &str) -> Option<String> {
    let p = wide(path);
    let mut icon: HICON = std::ptr::null_mut();
    unsafe {
        if path.len() >= 4 && path[path.len() - 4..].eq_ignore_ascii_case(".exe") {
            let mut id = 0u32;
            PrivateExtractIconsW(p.as_ptr(), 0, 64, 64, &mut icon, &mut id, 1, 0);
        }
        if icon.is_null() {
            let mut info: SHFILEINFOW = std::mem::zeroed();
            SHGetFileInfoW(p.as_ptr(), FILE_ATTRIBUTE_NORMAL, &mut info, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX);
            const IID_IIMAGELIST: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x46eb5926_582e_4017_9fdf_e8998daa0950);
            let mut list: *mut core::ffi::c_void = std::ptr::null_mut();
            if SHGetImageList(SHIL_EXTRALARGE as i32, &IID_IIMAGELIST, &mut list) >= 0 && !list.is_null() {
                icon = ImageList_GetIcon(list as HIMAGELIST, info.iIcon, ILD_TRANSPARENT);
                // Release the COM IImageList reference to prevent memory leak
                let vtbl = *(list as *const *const *const c_void);
                let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
                release(list);
            }
        }
        if icon.is_null() {
            return None;
        }
        let out = icon_to_rgba(icon).map(|(w, h, px)| format!("data:image/png;base64,{}", util::base64(&util::png(w, h, &px))));
        DestroyIcon(icon);
        out
    }
}

unsafe fn icon_to_rgba(icon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut ii) == 0 || ii.hbmColor.is_null() {
            if !ii.hbmMask.is_null() {
                DeleteObject(ii.hbmMask);
            }
            return None;
        }
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(ii.hbmColor, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _);
        let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let ok = GetDIBits(dc, ii.hbmColor, 0, h, px.as_mut_ptr() as *mut _, &mut bi, DIB_RGB_COLORS);
        DeleteDC(dc);
        DeleteObject(ii.hbmColor);
        DeleteObject(ii.hbmMask);
        if ok == 0 {
            return None;
        }
        let has_alpha = px.chunks_exact(4).any(|c| c[3] != 0);
        for c in px.chunks_exact_mut(4) {
            c.swap(0, 2); // BGRA → RGBA
            if !has_alpha {
                c[3] = 255;
            }
        }
        Some((w, h, px))
    }
}

/* ---------------- single instance ---------------- */
pub fn single_instance() -> bool {
    let name = wide(r"Local\WidgetPanelSingleton");
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if h.is_null() {
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(h);
            return false;
        }
        let _ = h;
        true
    }
}

/* ---------------- Explorer right-click menu ---------------- */
const MENU_KEYS: [(&str, &str); 4] = [
    (r"Software\Classes\*\shell\WidgetPanel", "%1"),
    (r"Software\Classes\Directory\shell\WidgetPanel", "%1"),
    (r"Software\Classes\Drive\shell\WidgetPanel", "%1"),
    (r"Software\Classes\Directory\Background\shell\WidgetPanel", "%V"),
];

pub fn context_menu_enabled() -> bool {
    let k = wide(MENU_KEYS[0].0);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), std::ptr::null(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) == 0 }
}

pub fn set_context_menu(on: bool) {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    for (key, arg) in MENU_KEYS {
        let k = wide(key);
        unsafe {
            if !on {
                RegDeleteTreeW(HKEY_CURRENT_USER, k.as_ptr());
                continue;
            }
            let set = |sub: &[u16], name: &str, data: &str| {
                let (n, d) = (wide(name), wide(data));
                let name = if name.is_empty() { std::ptr::null() } else { n.as_ptr() };
                RegSetKeyValueW(HKEY_CURRENT_USER, sub.as_ptr(), name, REG_SZ, d.as_ptr() as *const c_void, (d.len() * 2) as u32);
            };
            set(&k, "", "Add to Widget Panel");
            set(&k, "Icon", &exe);
            set(&k, "Position", "Top");
            let cmd = wide(&format!(r"{key}\command"));
            set(&cmd, "", &format!("\"{exe}\" --add \"{arg}\""));
        }
    }
}

/* ---------------- clipboard images ---------------- */
/// Another program may hold the clipboard for a moment: try a few times before giving up.
fn open_clipboard() -> bool {
    for i in 0..6 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return true;
        }
        if i < 5 {
            thread::sleep(Duration::from_millis(15));
        }
    }
    false
}

pub fn clip_image() -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        if !open_clipboard() {
            return None;
        }
        let h = GetClipboardData(CF_DIB as u32);
        let out = if h.is_null() {
            None
        } else {
            let size = GlobalSize(h as _);
            let ptr = GlobalLock(h as _);
            if ptr.is_null() {
                None
            } else {
                let res = dib_to_rgba(ptr as *const u8, size);
                GlobalUnlock(h as _);
                res
            }
        };
        CloseClipboard();
        out
    }
}

/// Largest picture kept in the history (RGBA bytes).
const MAX_DIB_BYTES: u64 = 128 * 1024 * 1024;

/// `size` = real size of the clipboard block: nothing outside it is ever read (any program can put a broken DIB on the clipboard).
unsafe fn dib_to_rgba(dib: *const u8, size: usize) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let hdr = std::mem::size_of::<BITMAPINFOHEADER>();
        if size < hdr {
            return None;
        }
        let head = &*(dib as *const BITMAPINFOHEADER);
        let (w, h) = (head.biWidth, head.biHeight);
        if w <= 0 || h == 0 || w > 10_000 || h.abs() > 10_000 || head.biBitCount < 24 || head.biBitCount > 32 {
            return None;
        }
        if head.biCompression != BI_RGB && head.biCompression != BI_BITFIELDS {
            return None;
        }
        let (w, flip, rows) = (w as u32, h > 0, h.unsigned_abs());
        if w as u64 * rows as u64 * 4 > MAX_DIB_BYTES {
            return None;
        }
        let bpp = (head.biBitCount / 8) as usize;
        let stride = ((w as usize * bpp) + 3) & !3;
        // the three colour masks follow a plain 40-byte header; bigger headers (V4 / V5) already contain them
        let masks = if head.biCompression == BI_BITFIELDS && head.biSize == 40 { 12 } else { 0 };
        let offset = head.biSize as u64 + masks + head.biClrUsed as u64 * 4;
        if offset + stride as u64 * rows as u64 > size as u64 {
            return None;
        }
        let pixels = dib.add(offset as usize);
        let mut out = vec![0u8; (w * rows * 4) as usize];
        let mut has_alpha = false;
        for y in 0..rows as usize {
            let src_row = if flip { rows as usize - 1 - y } else { y };
            let src = pixels.add(src_row * stride);
            let dst_row = y * w as usize * 4;
            for x in 0..w as usize {
                let p = src.add(x * bpp);
                let i = dst_row + x * 4;
                out[i] = *p.add(2);
                out[i + 1] = *p.add(1);
                out[i + 2] = *p;
                out[i + 3] = if bpp == 4 { *p.add(3) } else { 255 };
                has_alpha |= out[i + 3] != 0;
            }
        }
        // a 32-bit picture whose alpha bytes are all 0 has no transparency at all (most screenshots): make it opaque
        if !has_alpha {
            for c in out.chunks_exact_mut(4) {
                c[3] = 255;
            }
        }
        Some((w, rows, out))
    }
}

/// Puts an image back on the clipboard (as a 32-bit DIB, which every app understands).
pub fn set_clip_image(w: u32, h: u32, rgba: &[u8]) -> bool {
    let header = std::mem::size_of::<BITMAPINFOHEADER>();
    if rgba.len() < w as usize * h as usize * 4 {
        return false;
    }
    let total_len = header + w as usize * h as usize * 4;
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, total_len);
        if mem.is_null() {
            return false;
        }
        let dst = GlobalLock(mem) as *mut u8;
        if dst.is_null() {
            GlobalFree(mem);
            return false;
        }
        // Write directly into the locked global memory buffer (avoids an extra Vec allocation + copy)
        let head = &mut *(dst as *mut BITMAPINFOHEADER);
        *head = BITMAPINFOHEADER {
            biSize: header as u32,
            biWidth: w as i32,
            biHeight: h as i32, // positive: rows bottom-up
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: w * h * 4,
            ..std::mem::zeroed()
        };
        let row_bytes = w as usize * 4;
        let out_pixels = std::slice::from_raw_parts_mut(dst.add(header), w as usize * h as usize * 4);
        for y in 0..h as usize {
            let src_row = &rgba[(h as usize - 1 - y) * row_bytes..][..row_bytes];
            let dst_row = &mut out_pixels[y * row_bytes..][..row_bytes];
            for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
        }
        GlobalUnlock(mem);

        if !open_clipboard() {
            GlobalFree(mem);
            return false;
        }
        EmptyClipboard();
        let ok = !SetClipboardData(CF_DIB as u32, mem as HANDLE).is_null();
        CloseClipboard();
        if !ok {
            GlobalFree(mem);
        }
        ok
    }
}

/// Folder picker (the classic browse dialog: small and dependency-free).
pub fn pick_folder() -> Option<String> {
    let title = wide("Add a folder to Widget Panel");
    unsafe {
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        bi.lpszTitle = title.as_ptr();
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let list = SHBrowseForFolderW(&mut bi);
        if list.is_null() {
            return None;
        }
        let mut buf = [0u16; 520];
        let ok = SHGetPathFromIDListW(list, buf.as_mut_ptr()) != 0;
        CoTaskMemFree(list as *const c_void);
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        ok.then(|| String::from_utf16_lossy(&buf[..len]))
    }
}

/* ---------------- housekeeping ---------------- */
/// If the exe was moved or renamed, the "start with Windows" entry and the Explorer right-click menu would still point at the old place:
/// write them again with the current path (release builds only, so a debug build never takes them over).
#[allow(dead_code)]
pub fn refresh_registered_paths() {
    if startup_enabled() {
        set_startup(true);
    }
    if context_menu_enabled() {
        set_context_menu(true);
    }
}

/// Changes whenever a monitor is added, removed or moved (the main loop then places the panel again).
pub fn display_signature() -> [i32; 5] {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};
    unsafe { [GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN), GetSystemMetrics(80)] } // 80 = SM_CMONITORS
}

/// A plain error box (the app has no console, so this is the only way to tell the user why it cannot start).
pub fn message_box(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let (t, x) = (wide(title), wide(text));
    unsafe { MessageBoxW(std::ptr::null_mut(), x.as_ptr(), t.as_ptr(), MB_OK | MB_ICONERROR) };
}
