//! Network tools: local / public IP, ping, DNS (flush, set, reset) and saved Wi-Fi passwords.
//! Everything shells out to tools that ship with Windows, so no new dependencies are needed.

use std::{
    fs,
    net::{IpAddr, UdpSocket},
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::util;

const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW

/// The Windows folder (never taken from the current directory or PATH).
pub fn windows_dir() -> PathBuf {
    std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

/// Full path of a tool that ships with Windows, so a look-alike .exe in the current folder or on PATH can never be started instead.
pub fn system_exe(name: &str) -> PathBuf {
    let sys = windows_dir().join("System32");
    match name.to_ascii_lowercase().trim_end_matches(".exe") {
        "powershell" => sys.join(r"WindowsPowerShell\v1.0\powershell.exe"),
        n => sys.join(format!("{n}.exe")),
    }
}

pub fn cmd(program: &str) -> Command {
    let mut c = Command::new(system_exe(program));
    c.creation_flags(NO_WINDOW);
    c
}

/* ---------------- PowerShell helpers ---------------- */
/// PowerShell's -EncodedCommand wants base64 of the UTF-16LE script (no quoting problems at all).
fn encode(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    util::base64(&bytes)
}

pub fn powershell(script: &str) -> Option<Output> {
    let full = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;{script}");
    cmd("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encode(&full)])
        .output()
        .ok()
}

/// Runs a script as administrator (shows the UAC prompt unless we are already elevated).
/// Returns true only if the script finished without errors.
fn elevated(script: &str) -> bool {
    let inner = encode(&format!("$ErrorActionPreference='Stop';try{{{script}}}catch{{exit 1}}"));
    let outer = format!(
        "try{{$p=Start-Process (Join-Path $env:SystemRoot 'System32\\WindowsPowerShell\\v1.0\\powershell.exe') -Verb RunAs -WindowStyle Hidden -Wait -PassThru \
         -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{inner}';exit $p.ExitCode}}catch{{exit 1223}}"
    );
    powershell(&outer).map(|o| o.status.success()).unwrap_or(false)
}

/* ---------------- IP addresses ---------------- */
/// The address of the adapter that owns the default route (no packet is actually sent).
pub fn local_ip() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

pub fn public_ip() -> Option<String> {
    for url in ["https://api.ipify.org", "https://icanhazip.com", "https://ifconfig.me/ip"] {
        if let Ok(o) = cmd("curl.exe").args(["-s", "-m", "6", url]).output() {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && t.parse::<IpAddr>().is_ok() {
                return Some(t);
            }
        }
    }
    // very old Windows without curl.exe
    let o = powershell("(Invoke-RestMethod https://api.ipify.org -TimeoutSec 6)")?;
    let t = String::from_utf8_lossy(&o.stdout).trim().trim_start_matches('\u{feff}').to_string();
    t.parse::<IpAddr>().is_ok().then_some(t)
}

/* ---------------- DNS ---------------- */
/// DNS servers of the connected physical adapters (Wi-Fi / Ethernet).
pub fn current_dns() -> Vec<String> {
    let script = "Get-NetAdapter -Physical | Where-Object Status -eq 'Up' | ForEach-Object { \
                  (Get-DnsClientServerAddress -InterfaceIndex $_.ifIndex -AddressFamily IPv4).ServerAddresses } | Select-Object -Unique";
    powershell(script)
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().trim_start_matches('\u{feff}'))
                .filter(|l| l.parse::<IpAddr>().is_ok())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// ipconfig /flushdns; if Windows refuses, try again as administrator.
pub fn flush_dns() -> bool {
    if cmd("ipconfig").arg("/flushdns").output().map(|o| o.status.success()).unwrap_or(false) {
        return true;
    }
    elevated("Clear-DnsClientCache")
}

/// Sets the DNS servers on every connected physical adapter. An empty list resets them to automatic (DHCP).
/// Changing DNS needs administrator rights, so this asks for them through UAC.
pub fn set_dns(servers: &[String]) -> Result<(), String> {
    if servers.iter().any(|s| s.parse::<IpAddr>().is_err()) {
        return Err("Invalid DNS address".into());
    }
    let action = if servers.is_empty() {
        "-ResetServerAddresses".to_string()
    } else {
        let list = servers.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
        format!("-ServerAddresses @({list})")
    };
    let script = format!(
        "Get-NetAdapter -Physical | Where-Object Status -eq 'Up' | ForEach-Object {{ Set-DnsClientServerAddress -InterfaceIndex $_.ifIndex {action} }}; Clear-DnsClientCache"
    );
    if elevated(&script) { Ok(()) } else { Err("Cancelled, or administrator rights are required".into()) }
}

/* ---------------- ping ---------------- */
fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && !h.starts_with('-') && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
}

/// "…time=12ms TTL=117" / "time<1ms" → milliseconds (language independent: only digits before the last "ms").
fn parse_ms(line: &str) -> Option<f64> {
    let head = &line[..line.rfind("ms")?];
    let digits: String = head.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    let v: f64 = digits.parse().ok()?;
    if head[..head.len() - digits.len()].ends_with('<') { Some(0.5) } else { Some(v) }
}

fn fmt_ms(ms: f64) -> String {
    if ms < 1.0 { "<1 ms".into() } else { format!("{ms:.0} ms") }
}

/// Four pings, one result line at a time (emit is called for each line, the last one has done = true).
pub fn ping(host: &str, emit: impl Fn(Value)) {
    if !valid_host(host) {
        emit(json!({ "line": "Invalid host name", "done": true }));
        return;
    }
    let count = 4;
    let mut times: Vec<f64> = Vec::new();
    for i in 0..count {
        let t0 = Instant::now();
        let out = cmd("ping").args(["-n", "1", "-w", "2000", host]).output();
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        match text.lines().find(|l| l.to_ascii_uppercase().contains("TTL=")) {
            Some(l) => {
                let ms = parse_ms(l).unwrap_or(wall.round());
                times.push(ms);
                emit(json!({ "line": format!("Reply from {host}: {}", fmt_ms(ms)) }));
            }
            None => emit(json!({ "line": "Request timed out or host unreachable" })),
        }
        if i + 1 < count {
            thread::sleep(Duration::from_millis(450));
        }
    }
    let got = times.len();
    let lost = (count - got) * 100 / count;
    let mut summary = format!("\nSent {count} · Received {got} · Lost {lost}%");
    if got > 0 {
        let min = times.iter().cloned().fold(f64::MAX, f64::min);
        let max = times.iter().cloned().fold(0.0, f64::max);
        let avg = times.iter().sum::<f64>() / got as f64;
        summary.push_str(&format!("\nmin {} · avg {} · max {}", fmt_ms(min), fmt_ms(avg), fmt_ms(max)));
    }
    emit(json!({ "line": summary, "done": true }));
}

/* ---------------- saved Wi-Fi passwords ---------------- */
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn tag(xml: &str, name: &str) -> Option<String> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(unescape(&xml[start..end]))
}

/// Exports every saved Wi-Fi profile to temporary XML files (language independent, always UTF-8),
/// reads name / security / password from them and deletes the files again.
/// Without admin rights Windows sometimes hides the keys: `needAdmin` tells the UI to offer an elevated retry.
/// Removes temporary Wi-Fi export folders (they hold passwords in clear text) left behind when the app was closed or crashed in the middle of an export.
pub fn cleanup_stale_wifi() {
    if let Ok(rd) = fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("wp-wifi-") && e.path().is_dir() {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
}

pub fn wifi(admin: bool) -> Value {
    let dir = std::env::temp_dir().join(format!("wp-wifi-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
    let d = dir.display().to_string();
    let mut cancelled = false;
    if admin {
        cancelled = !elevated(&format!("& (Join-Path $env:SystemRoot 'System32\\netsh.exe') wlan export profile key=clear folder=\"{d}\" | Out-Null"));
    } else {
        let _ = cmd("netsh").raw_arg(format!("wlan export profile key=clear folder=\"{d}\"")).output();
    }

    let mut items: Vec<(String, Option<String>, String)> = Vec::new();
    let mut need_admin = false;
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) != Some("xml") {
                continue;
            }
            let Ok(bytes) = fs::read(e.path()) else { continue };
            let xml = String::from_utf8_lossy(&bytes);
            let Some(name) = tag(&xml, "name") else { continue };
            let auth = tag(&xml, "authentication").unwrap_or_default();
            let pass = tag(&xml, "keyMaterial").filter(|p| !p.is_empty());
            let enterprise = xml.contains("<OneX"); // 802.1X networks have no shared password
            if pass.is_none() && !auth.eq_ignore_ascii_case("open") && !enterprise && !admin {
                need_admin = true;
            }
            items.push((name, pass, auth));
        }
    }
    let _ = fs::remove_dir_all(&dir);
    items.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    json!({
        "items": items.into_iter().map(|(name, pass, auth)| json!({ "name": name, "pass": pass, "auth": auth })).collect::<Vec<_>>(),
        "needAdmin": need_admin,
        "cancelled": cancelled,
    })
}
