use anyhow::{bail, Result};
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// An ffmpeg command that never flashes a console window.
pub fn cmd(ffmpeg: &str) -> Command {
    let mut c = Command::new(ffmpeg);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// `cmd_low` when `low` is set, else a normal-priority `cmd`.
pub fn cmd_prio(ffmpeg: &str, low: bool) -> Command {
    if low { cmd_low(ffmpeg) } else { cmd(ffmpeg) }
}

/// Like `cmd`, but at idle CPU priority so a background export never competes with a game.
pub fn cmd_low(ffmpeg: &str) -> Command {
    const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
    let mut c = Command::new(ffmpeg);
    c.creation_flags(CREATE_NO_WINDOW | IDLE_PRIORITY_CLASS);
    c
}

pub fn run(mut c: Command) -> Result<()> {
    let out = c.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("ffmpeg failed: {}", tail(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

/// Last few non-empty lines of ffmpeg's stderr, for error messages.
pub fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" | ")
}

#[derive(Serialize, Default)]
pub struct FfInfo {
    pub ok: bool,
    pub version: String,
    pub encoders: Vec<String>,
    pub ddagrab: bool,
    pub gfxcapture: bool,
    /// Can record below native resolution: gfxcapture scales on the GPU (scale_d3d11 is broken).
    pub gpu_scale: bool,
    pub vp9: bool,
}

/// Whether this ffmpeg has the `gfxcapture` filter (window capture). Cached per path,
/// since the engine asks every tick.
pub fn has_gfxcapture(ffmpeg: &str) -> bool {
    use std::sync::Mutex;
    static CACHE: Cache = Mutex::new(None);
    cached(&CACHE, ffmpeg, || {
        let out = cmd(ffmpeg).args(["-hide_banner", "-filters"]).stdin(Stdio::null()).output();
        let Ok(out) = out else { return true }; // ffmpeg missing: other errors will say so
        String::from_utf8_lossy(&out.stdout).contains(" gfxcapture ")
    })
}

/// Whether `-thread_queue_size` can still be given to an input. Recent FFmpeg master builds
/// dropped the input side and refuse to start when it's there. Cached per path.
pub fn input_queue_size_ok(ffmpeg: &str) -> bool {
    use std::sync::Mutex;
    static CACHE: Cache = Mutex::new(None);
    cached(&CACHE, ffmpeg, || {
        let out = cmd(ffmpeg)
            .args(["-hide_banner", "-thread_queue_size", "8", "-f", "f32le", "-i", "NUL", "-f", "null", "-"])
            .stdin(Stdio::null())
            .output();
        let Ok(out) = out else { return true };
        !String::from_utf8_lossy(&out.stderr).contains("cannot be applied to input")
    })
}

type Cache = std::sync::Mutex<Option<(String, Option<std::time::SystemTime>, bool)>>;

/// Run `probe` once per ffmpeg path. The file's modified time is part of the key, so
/// replacing ffmpeg.exe in place is noticed.
fn cached(cache: &Cache, ffmpeg: &str, probe: impl FnOnce() -> bool) -> bool {
    let stamp = std::fs::metadata(ffmpeg).and_then(|m| m.modified()).ok();
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, t, v)) = c.as_ref() {
        if p == ffmpeg && *t == stamp {
            return *v;
        }
    }
    let v = probe();
    *c = Some((ffmpeg.to_string(), stamp, v));
    v
}

pub fn info(ffmpeg: &str) -> FfInfo {
    let grab = |args: &[&str]| -> Option<String> {
        let out = cmd(ffmpeg).args(args).stdin(Stdio::null()).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(ver) = grab(&["-hide_banner", "-version"]) else {
        return FfInfo::default();
    };
    let encoders = grab(&["-hide_banner", "-encoders"]).unwrap_or_default();
    let filters = grab(&["-hide_banner", "-filters"]).unwrap_or_default();
    FfInfo {
        ok: true,
        version: ver.lines().next().unwrap_or("").to_string(),
        encoders: ["nvenc", "amf", "qsv"]
            .iter()
            .filter(|e| encoders.contains(&format!("h264_{e}")))
            .map(|e| e.to_string())
            .collect(),
        ddagrab: filters.contains(" ddagrab "),
        gfxcapture: filters.contains(" gfxcapture "),
        gpu_scale: filters.contains(" gfxcapture "),
        vp9: encoders.contains("libvpx-vp9"),
    }
}

/// The video codec of a clip ("h264", "hevc", "vp9", ...), or "" if it can't be read.
pub fn probe_codec(ffmpeg: &str, file: &std::path::Path) -> String {
    let Ok(out) = cmd(ffmpeg).args(["-hide_banner", "-i"]).arg(file).stdin(Stdio::null()).output() else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines()
        .find_map(|l| l.split_once("Video: ").map(|(_, r)| r))
        .and_then(|r| r.split([' ', ',', '(']).next())
        .unwrap_or("")
        .to_lowercase()
}

/// Remove what earlier FFmpeg downloads left in `dir`: a half-finished download, and copies the
/// settings no longer use (a download made while the old ffmpeg.exe was running is saved beside
/// it as ffmpeg-<n>.exe). Only when `current` (the ffmpeg in the settings) is one of them; a copy
/// still running can't be deleted and goes next time.
pub fn clean_up(dir: &std::path::Path, current: &str) {
    let _ = std::fs::remove_file(dir.join("ffmpeg.zip"));
    let _ = std::fs::remove_dir_all(dir.join("unpack"));
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).ok();
    let Some(cur) = canon(std::path::Path::new(current)) else { return };
    if cur.parent() != canon(dir).as_deref() {
        return; // the settings point at an FFmpeg of your own: leave the downloaded ones alone
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name.starts_with("ffmpeg") && name.ends_with(".exe") && canon(&e.path()).as_ref() != Some(&cur) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

const FFMPEG_URL: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip";

/// Download a full FFmpeg build into `dir` (using the curl.exe and tar.exe that ship with Windows 10+)
/// and return the path to ffmpeg.exe. `progress` gets the bytes downloaded so far.
pub fn download(dir: &std::path::Path, progress: impl Fn(u64)) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let zip = dir.join("ffmpeg.zip");
    let _ = std::fs::remove_file(&zip);

    let mut child = cmd("curl.exe")
        .args(["-L", "--fail", "--silent", "--show-error", "-o"])
        .arg(&zip)
        .arg(FFMPEG_URL)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("couldn't start curl: {e}"))?;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        progress(std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0));
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut s) = child.stderr.take() {
            use std::io::Read;
            let _ = s.read_to_string(&mut err);
        }
        let _ = std::fs::remove_file(&zip);
        bail!("download failed: {}", tail(&err));
    }

    let unpack = dir.join("unpack");
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack)?;
    let out = cmd("tar.exe")
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&unpack)
        .stdin(Stdio::null())
        .output()?;
    let _ = std::fs::remove_file(&zip);
    if !out.status.success() {
        bail!("couldn't unpack FFmpeg: {}", tail(&String::from_utf8_lossy(&out.stderr)));
    }

    // The zip holds one top-level folder with bin\ffmpeg.exe inside.
    let src = std::fs::read_dir(&unpack)?
        .flatten()
        .map(|e| e.path().join("bin").join("ffmpeg.exe"))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("ffmpeg.exe wasn't in the download"))?;
    let mut dest = dir.join("ffmpeg.exe");
    let _ = std::fs::remove_file(&dest);
    if dest.exists() {
        // The old copy is still running (the recorder holds it): put the new one beside it.
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        dest = dir.join(format!("ffmpeg-{secs}.exe"));
    }
    std::fs::rename(&src, &dest)?;
    let _ = std::fs::remove_dir_all(&unpack);
    Ok(dest)
}
