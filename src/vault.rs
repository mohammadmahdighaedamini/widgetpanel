//! Saved logins. Passwords are encrypted with Windows DPAPI (only your Windows account can decrypt them)
//! and kept in vault.json next to the settings. The panel only receives names, usernames and logos;
//! a password travels to the page only when you press "Show", and "copy" puts it on the clipboard (it stays there until you copy something else).

use std::{
    fs, io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};

use serde_json::{json, Value};

use crate::{net, sys, util};

/// Hash (never the text itself) of the password we last put on the clipboard, so the clipboard history does not record it. 0 = none.
static SECRET: AtomicU64 = AtomicU64::new(0);

/// True when the clipboard text is the password we copied ourselves.
pub fn is_secret(text: &str) -> bool {
    let h = SECRET.load(Ordering::Relaxed);
    h != 0 && h == util::hash(text.as_bytes())
}

fn path() -> PathBuf {
    sys::data_dir().join("vault.json")
}

/// Only one read-modify-write at a time (save / delete / logo download run on different threads).
/// Never hold it during a network download.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn parse_list(text: &str) -> Option<Vec<Value>> {
    serde_json::from_str::<Value>(text).ok().and_then(|v| v.as_array().cloned())
}

/// The saved logins. `None` = the file could not be used right now (nothing may be written then, or good data would be overwritten).
/// A damaged vault.json is never thrown away: it is kept as vault.bad and the last good copy (vault.bak) is restored when there is one.
fn load() -> Option<Vec<Value>> {
    let p = path();
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => String::new(), // not text at all: damaged
        Err(_) => return None, // locked for a moment (antivirus, indexer…): try again later
    };
    if let Some(list) = parse_list(&text) {
        return Some(list);
    }
    let bak = p.with_extension("bak");
    let _ = fs::rename(&p, p.with_extension("bad"));
    crate::log("vault.json was damaged and was moved to vault.bad");
    match fs::read_to_string(&bak).ok().and_then(|t| parse_list(&t)) {
        Some(list) => {
            let _ = fs::copy(&bak, &p);
            Some(list)
        }
        None => Some(Vec::new()),
    }
}

/// Writes the list (the previous good file is kept as vault.bak). Returns false when it could not be saved.
fn store(list: &mut Vec<Value>) -> bool {
    list.sort_by_key(|v| v["title"].as_str().unwrap_or("").to_lowercase());
    let p = path();
    if p.exists() {
        let _ = fs::copy(&p, p.with_extension("bak"));
    }
    util::write_atomic(&p, Value::Array(list.clone()).to_string()).is_ok()
}

/// What the page gets: everything except the encrypted password.
pub fn public() -> Value {
    Value::Array(
        load()
            .unwrap_or_default()
            .into_iter()
            .map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    o.remove("pw");
                }
                v
            })
            .collect(),
    )
}

pub fn reveal(id: &str) -> Option<String> {
    let list = load()?;
    let v = list.iter().find(|v| v["id"] == id)?;
    let bytes = util::unhex(v["pw"].as_str()?)?;
    String::from_utf8(sys::unprotect(&bytes)?).ok()
}

/// Adds or updates an entry (`pass` = None keeps the stored password).
/// Returns Some((id, site)) when the site logo still has to be downloaded.
pub fn save(entry: &Value, pass: Option<&str>) -> Result<Option<(String, String)>, String> {
    let id = entry["id"].as_str().ok_or("Bad entry")?.to_string();
    let _guard = lock();
    let mut list = load().ok_or("The saved logins file is busy, try again")?;
    let old = list.iter().position(|v| v["id"] == id.as_str());
    let pw = match pass {
        Some(p) => util::hex(&sys::protect(p.as_bytes()).ok_or("Windows could not encrypt the password")?),
        None => old.and_then(|i| list[i]["pw"].as_str().map(String::from)).ok_or("Enter a password")?,
    };
    let want_site = entry["logo"] == "site";
    let site = entry["site"].as_str().unwrap_or("").trim().to_string();
    let logo = if want_site {
        // keep the downloaded icon only while the website is unchanged
        old.filter(|&i| list[i]["site"].as_str().unwrap_or("").trim() == site)
            .and_then(|i| list[i]["logo"].as_str().filter(|l| l.starts_with("data:")).map(String::from))
            .unwrap_or_else(|| "🌐".into())
    } else {
        entry["logo"].as_str().unwrap_or("🔑").chars().take(8).collect()
    };
    let e = json!({
        "id": id,
        "title": entry["title"].as_str().unwrap_or("").trim(),
        "site": site,
        "user": entry["user"].as_str().unwrap_or("").trim(),
        "logo": logo,
        "pw": pw,
    });
    match old {
        Some(i) => list[i] = e,
        None => list.push(e),
    }
    if !store(&mut list) {
        return Err("Could not write the saved logins file".into());
    }
    Ok(if want_site && !site.is_empty() { Some((id, site)) } else { None })
}

pub fn delete(id: &str) {
    let _guard = lock();
    let Some(mut list) = load() else { return };
    list.retain(|v| v["id"] != id);
    store(&mut list);
}

pub fn set_logo(id: &str, logo: String) {
    let _guard = lock();
    let Some(mut list) = load() else { return };
    if let Some(v) = list.iter_mut().find(|v| v["id"] == id) {
        v["logo"] = json!(logo);
        store(&mut list);
    }
}

fn domain(site: &str) -> Option<String> {
    let s = site.trim().trim_start_matches("https://").trim_start_matches("http://");
    let host = s.split(['/', '?', '#']).next()?.trim_start_matches("www.").to_lowercase();
    (!host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')).then_some(host)
}

fn image_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if b.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if b.starts_with(b"GIF8") {
        Some("image/gif")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// Downloads the site's icon once (curl.exe ships with Windows) and keeps it inside the entry, so it works offline afterwards.
/// Tries Google's favicon service first, then DuckDuckGo's, then the site's own /favicon.ico.
pub fn favicon(site: &str) -> Option<String> {
    let d = domain(site)?;
    let urls = [
        format!("https://www.google.com/s2/favicons?domain={d}&sz=64"),
        format!("https://icons.duckduckgo.com/ip3/{d}.ico"),
        format!("https://{d}/favicon.ico"),
    ];
    for url in urls {
        // https only, also after a redirect
        let Ok(o) = net::cmd("curl.exe").args(["-s", "-f", "-L", "-m", "8", "--proto", "=https", "--proto-redir", "=https", &url]).output() else { continue };
        if !o.status.success() || o.stdout.is_empty() || o.stdout.len() > 120_000 {
            continue;
        }
        if let Some(mime) = image_mime(&o.stdout) {
            return Some(format!("data:{mime};base64,{}", util::base64(&o.stdout)));
        }
    }
    None
}

/// Set while the background icon retry runs, so opening the panel repeatedly never starts a second one.
static FETCHING: AtomicBool = AtomicBool::new(false);

/// Logins that have a website but still show the placeholder (an earlier download failed, e.g. offline): (id, site).
fn missing_logos() -> Vec<(String, String)> {
    load()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            let site = v["site"].as_str()?.trim();
            let logo = v["logo"].as_str().unwrap_or("");
            (!site.is_empty() && !logo.starts_with("data:") && domain(site).is_some())
                .then(|| (v["id"].as_str().unwrap_or("").to_string(), site.to_string()))
        })
        .collect()
}

/// Tries the missing icons again (blocking, run it on a thread). Returns true when at least one icon was added.
pub fn fetch_missing_logos() -> bool {
    if FETCHING.swap(true, Ordering::SeqCst) {
        return false;
    }
    let mut changed = false;
    for (id, site) in missing_logos() {
        if let Some(logo) = favicon(&site) {
            set_logo(&id, logo);
            changed = true;
        }
    }
    FETCHING.store(false, Ordering::SeqCst);
    changed
}

/// Puts the password on the clipboard. It stays there until something else is copied,
/// but it is kept out of the panel's own clipboard history.
pub fn copy_password(id: &str) -> bool {
    let Some(pw) = reveal(id) else { return false };
    SECRET.store(util::hash(pw.as_bytes()), Ordering::Relaxed);
    crate::set_clip(&pw);
    true
}
