//! A clip's sound tracks, for the library's level editor.
//!
//! The viewer plays each track from its own file (WebView2 plays only a video's first track),
//! so every track is decoded once into a .flac in `<clips_dir>\.playback`, lined up with the
//! video's start, together with a small waveform (`.peaks`: one byte per 1/20 s).
use crate::ff;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Waveform values per second.
pub const PEAK_RATE: u32 = 20;
const WAVE_HZ: u32 = 4000;

#[derive(Debug, Serialize)]
pub struct TrackInfo {
    /// Position among the clip's audio streams (`0:a:N`).
    pub index: usize,
    /// "Game", "Discord", "Mic", "Mix", "Desktop", or "" for clips from before tracks had names.
    pub title: String,
    /// The decoded copy to play.
    pub file: String,
    /// Loudness every 1/20 s, 0-255 (square-root scaled so quiet sound still shows).
    pub peaks: Vec<u8>,
}

/// Titles of the clip's audio streams, in order ("" where it has none).
pub fn titles(ffmpeg: &str, clip: &Path) -> Vec<String> {
    let Ok(out) = ff::cmd(ffmpeg).args(["-hide_banner", "-i"]).arg(clip).stdin(Stdio::null()).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stderr);
    let mut titles: Vec<String> = Vec::new();
    let mut in_audio = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Stream #") {
            in_audio = t.contains(": Audio:");
            if in_audio {
                titles.push(String::new());
            }
            continue;
        }
        if !in_audio {
            continue;
        }
        if let Some((k, v)) = t.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            let last = titles.last_mut().unwrap();
            // ffmpeg's own default handler names aren't titles.
            let generic = v.is_empty() || v.eq_ignore_ascii_case("SoundHandler") || v.eq_ignore_ascii_case("Sound Media Handler");
            if (k == "title" || (k == "handler_name" && last.is_empty())) && !generic {
                *last = v.to_string();
            }
        }
    }
    titles
}

/// Every audio track of `clip`, decoded for playback (cached in `cache_dir`).
pub fn tracks(ffmpeg: &str, clip: &Path, cache_dir: &Path) -> Result<Vec<TrackInfo>> {
    let titles = titles(ffmpeg, clip);
    if titles.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(cache_dir)?;
    let meta = std::fs::metadata(clip)?;
    let stamp = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let stem = clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let key = format!("{stem}_{stamp}_{}", meta.len());

    // Decode the tracks side by side; each is one ffmpeg run.
    let jobs: Vec<_> = titles
        .into_iter()
        .enumerate()
        .map(|(i, title)| {
            let (ffmpeg, clip) = (ffmpeg.to_string(), clip.to_path_buf());
            let base = cache_dir.join(format!("{key}_a{i}"));
            std::thread::spawn(move || -> Result<TrackInfo> {
                let (file, peaks) = decode(&ffmpeg, &clip, i, &base)?;
                Ok(TrackInfo { index: i, title, file: file.to_string_lossy().into_owned(), peaks })
            })
        })
        .collect();
    let mut out = Vec::new();
    for j in jobs {
        out.push(j.join().map_err(|_| anyhow::anyhow!("track decoder crashed"))??);
    }
    prune(cache_dir);
    Ok(out)
}

fn decode(ffmpeg: &str, clip: &Path, i: usize, base: &Path) -> Result<(PathBuf, Vec<u8>)> {
    let flac = base.with_extension("flac");
    let peaks_file = base.with_extension("peaks");
    if flac.is_file() {
        if let Ok(p) = std::fs::read(&peaks_file) {
            return Ok((flac, p));
        }
    }
    let tmp = base.with_extension("part.flac");
    // One decode feeds both the playback copy and the waveform. `first_pts=0` pads or trims the
    // start so the track lines up with the video's timeline. Both branches of `asplit` share one
    // format, so it's fixed (stereo s16) before the split and the waveform is folded down after.
    let graph = format!(
        "[0:a:{i}]aresample=async=1:first_pts=0,aformat=sample_fmts=s16:channel_layouts=stereo,asplit[p][w];         [w]pan=mono|c0=0.5*c0+0.5*c1,aresample={WAVE_HZ}[wave]"
    );
    let mut c = ff::cmd_low(ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(clip)
        .args(["-filter_complex", &graph, "-map", "[p]", "-c:a", "flac", "-compression_level", "0", "-f", "flac"])
        .arg(&tmp)
        .args(["-map", "[wave]", "-f", "s16le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().context("Couldn't start ffmpeg")?;
    let mut stderr = child.stderr.take().unwrap();
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut stdout = child.stdout.take().unwrap();
    let per = (WAVE_HZ / PEAK_RATE) as usize;
    let mut peaks = Vec::new();
    let (mut n, mut max) = (0usize, 0u16);
    let mut buf = vec![0u8; 64 * 1024];
    let mut odd: Option<u8> = None;
    loop {
        let got = stdout.read(&mut buf)?;
        if got == 0 {
            break;
        }
        let mut bytes = &buf[..got];
        let mut sample = |s: i16| {
            max = max.max(s.unsigned_abs());
            n += 1;
            if n == per {
                peaks.push(((max as f32 / 32768.0).sqrt() * 255.0).round().min(255.0) as u8);
                n = 0;
                max = 0;
            }
        };
        if let Some(lo) = odd.take() {
            sample(i16::from_le_bytes([lo, bytes[0]]));
            bytes = &bytes[1..];
        }
        let pairs = bytes.chunks_exact(2);
        if let [b] = pairs.remainder() {
            odd = Some(*b);
        }
        for p in pairs {
            sample(i16::from_le_bytes([p[0], p[1]]));
        }
    }
    if n > 0 {
        peaks.push(((max as f32 / 32768.0).sqrt() * 255.0).round().min(255.0) as u8);
    }
    let status = child.wait()?;
    let errs = err.join().unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("Couldn't read the clip's sound: {}", ff::tail(&errs));
    }
    std::fs::rename(&tmp, &flac)?;
    let _ = std::fs::write(&peaks_file, &peaks);
    Ok((flac, peaks))
}

/// Keep the playback cache small: the newest 8 video copies and 32 sound tracks.
pub fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut videos = Vec::new();
    let mut sounds = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let Some(t) = e.metadata().ok().and_then(|m| m.modified().ok()) else { continue };
        match p.extension().and_then(|x| x.to_str()) {
            Some("mp4") => videos.push((t, p)),
            Some("flac") => sounds.push((t, p)),
            _ => {}
        }
    }
    for (list, keep) in [(&mut videos, 8), (&mut sounds, 32)] {
        list.sort();
        let extra = list.len().saturating_sub(keep);
        for (_, p) in list.drain(..extra) {
            // A file the viewer is playing can't be deleted; it goes next time.
            if std::fs::remove_file(&p).is_ok() && p.extension().is_some_and(|x| x == "flac") {
                let _ = std::fs::remove_file(p.with_extension("peaks"));
            }
        }
    }
}
