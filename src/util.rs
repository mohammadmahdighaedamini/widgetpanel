//! Platform-neutral helpers: tiny PNG encoder, base64, tray icon pixels, names.

use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

pub fn display_name(path: &str) -> String {
    let p = Path::new(path.trim_end_matches(['/', '\\']));
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

/// Writes next to the target first and then renames, so a crash or power cut never leaves a half-written file.
/// The temporary name is unique per call, so two threads saving the same file never share one; the data is flushed to disk before the rename.
pub fn write_atomic(path: &Path, data: impl AsRef<[u8]>) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", COUNTER.fetch_add(1, Ordering::Relaxed)));
    let written = fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data.as_ref())?;
        f.sync_all()
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Retry rename a few times in case Windows Defender / indexer briefly locks the destination file
    for attempt in 0..4 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(_) if attempt < 3 => thread::sleep(Duration::from_millis(15)),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    Ok(())
}

/* ---- tiny PNG encoder (uncompressed deflate, with compile-time CRC32 table) ---- */
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in kind.iter().chain(data.iter()) {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

pub fn png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let row_bytes = (w * 4) as usize;
    let raw_len = (w * h * 4 + h) as usize;
    let mut raw = Vec::with_capacity(raw_len);
    for row in rgba.chunks(row_bytes) {
        raw.push(0);
        raw.extend_from_slice(row);
    }

    let num_blocks = (raw.len() + 65534) / 65535;
    let mut z = Vec::with_capacity(2 + num_blocks * 5 + raw.len() + 4);
    z.extend_from_slice(&[0x78, 0x01]);

    let chunks = raw.chunks(65535);
    let total_chunks = chunks.len();
    for (i, ch) in chunks.enumerate() {
        z.push((i + 1 == total_chunks) as u8);
        let l = ch.len() as u16;
        z.extend_from_slice(&l.to_le_bytes());
        z.extend_from_slice(&(!l).to_le_bytes());
        z.extend_from_slice(ch);
    }

    // Adler-32 checksum (batched modulo to avoid division on every byte)
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in raw.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = Vec::with_capacity(8 + 25 + 12 + z.len() + 12);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut write_chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(kind, data).to_be_bytes());
    };
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&w.to_be_bytes());
    ihdr[4..8].copy_from_slice(&h.to_be_bytes());
    ihdr[8..13].copy_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(b"IHDR", &ihdr);
    write_chunk(b"IDAT", &z);
    write_chunk(b"IEND", &[]);
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut chunks = data.chunks_exact(3);
    for c in &mut chunks {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | (c[2] as u32);
        s.push(T[((n >> 18) & 63) as usize] as char);
        s.push(T[((n >> 12) & 63) as usize] as char);
        s.push(T[((n >> 6) & 63) as usize] as char);
        s.push(T[(n & 63) as usize] as char);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let n = (rem[0] as u32) << 16 | (*rem.get(1).unwrap_or(&0) as u32) << 8;
        s.push(T[((n >> 18) & 63) as usize] as char);
        s.push(T[((n >> 12) & 63) as usize] as char);
        s.push(if rem.len() == 2 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        s.push('=');
    }
    s
}

pub fn hex(data: &[u8]) -> String {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for &b in data {
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0x0F) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Tray icon: 32×32 RGBA pixels (assets/tray.rgba, generated from the app logo).
pub fn tray_rgba() -> Vec<u8> {
    include_bytes!("../assets/tray.rgba").to_vec()
}

/// Fast stable hash: hashes full data for small inputs (like passwords), and strides for huge buffers (like 4K screenshots).
pub fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let step = if bytes.len() > 65536 { 97 } else { 1 };
    for &b in bytes.iter().step_by(step) {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h ^ bytes.len() as u64
}

/// Nearest-neighbour downscale so the UI can show a small preview.
pub fn thumbnail(w: u32, h: u32, rgba: &[u8], max: u32) -> (u32, u32, Vec<u8>) {
    if w <= max && h <= max {
        return (w, h, rgba.to_vec());
    }
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    let mut out = vec![0u8; (tw * th * 4) as usize];
    let w_usize = w as usize;
    let tw_usize = tw as usize;
    for y in 0..th {
        let src_row = (y * h / th) as usize * w_usize;
        let dst_row = y as usize * tw_usize;
        for x in 0..tw {
            let src = (src_row + (x * w / tw) as usize) * 4;
            let dst = (dst_row + x as usize) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    (tw, th, out)
}