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
    pub scale_d3d11: bool,
    pub vp9: bool,
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
        scale_d3d11: filters.contains(" scale_d3d11 "),
        vp9: encoders.contains("libvpx-vp9"),
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
    let dest = dir.join("ffmpeg.exe");
    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&src, &dest)?;
    let _ = std::fs::remove_dir_all(&unpack);
    Ok(dest)
}
