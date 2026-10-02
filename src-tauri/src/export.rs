//! Trim + export. Paths:
//!  * original, same codec (.mp4/.mkv/.mov) -> stream copy (instant, original quality, cuts snap to keyframes)
//!  * original, other codec                 -> GPU re-encode at a quality-matched bitrate, audio copied
//!  * size / bitrate                        -> GPU encode at a computed bitrate (fast); size mode re-tries once if over
//!  * exact size                            -> CPU two-pass (x264 or x265), slowest, lands closest to the target size
//!  * .gif                                  -> palette-based CPU encode, no audio, capped frame rate and size
//!  * .png                                  -> one raw frame at `start`, straight from the clip (lossless, no scaling)
//!  * .webm                                 -> always re-encoded with VP9 + Opus (.webm can't hold H.264/AAC)
//!
//! Exports run at normal priority on all cores; `low_impact` drops to idle priority with a few threads.
use crate::config::{Config, Encoder};
use crate::engine::sanitize_name;
use crate::{ff, library};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    Mp4,
    Mkv,
    Mov,
    Webm,
    Gif,
    Png,
}

impl ExportFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ExportFormat::Mp4 => "mp4",
            ExportFormat::Mkv => "mkv",
            ExportFormat::Mov => "mov",
            ExportFormat::Webm => "webm",
            ExportFormat::Gif => "gif",
            ExportFormat::Png => "png",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExportCodec {
    /// Whatever the clip already is (stream copy when nothing else changes).
    #[default]
    Keep,
    H264,
    Hevc,
}

/// HEVC needs roughly this share of H.264's bitrate for the same picture quality.
pub const HEVC_RATIO: f64 = 0.65;

#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub path: String,
    pub start: f64,
    pub end: f64,
    /// "original", "size" or "bitrate".
    pub mode: String,
    #[serde(default)]
    pub format: ExportFormat,
    #[serde(default)]
    pub codec: ExportCodec,
    /// Used when mode == "size".
    #[serde(default)]
    pub target_mb: f64,
    /// Video bitrate, used when mode == "bitrate".
    #[serde(default)]
    pub target_kbps: u32,
    /// -1 = auto, 0 = source, otherwise max output height.
    pub height: i32,
    pub precise: bool,
    /// .gif only: frames per second.
    #[serde(default)]
    pub fps: u32,
    /// File name without extension; empty = automatic.
    #[serde(default)]
    pub name: String,
    /// The clip's own total bitrate (video + audio), so we never export "bigger than native".
    #[serde(default)]
    pub src_kbps: f64,
    /// Idle priority and fewer threads, so a running game isn't affected.
    #[serde(default)]
    pub low_impact: bool,
}

fn unique(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = p.extension().unwrap_or_default().to_string_lossy().into_owned();
    let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
    (2..)
        .map(|i| dir.join(format!("{stem}_{i}.{ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

fn auto_height(video_kbps: u32) -> u32 {
    match video_kbps {
        0..=999 => 480,
        1000..=2499 => 720,
        2500..=5999 => 1080,
        _ => 0,
    }
}

fn audio_tier(total_kbps: f64) -> u32 {
    if total_kbps < 600.0 { 48 } else if total_kbps < 2000.0 { 96 } else { 128 }
}

pub fn clean_stem(name: &str, ext: &str) -> String {
    let n = name.trim();
    let n = n
        .strip_suffix(&format!(".{ext}"))
        .or_else(|| n.strip_suffix(&format!(".{}", ext.to_uppercase())))
        .unwrap_or(n);
    let s = sanitize_name(n);
    if s == "Unknown" && n.trim().is_empty() { String::new() } else { s.chars().take(120).collect() }
}

/// A screenshot: the single frame at `req.start`, decoded straight from the clip and saved as .png.
fn export_frame(cfg: &Config, req: &ExportRequest, input: &Path) -> Result<PathBuf> {
    let game = sanitize_name(&library::game_of(input));
    let src_stem = input.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let custom = clean_stem(&req.name, "png");
    let stem = if custom.is_empty() { format!("{src_stem}_frame") } else { custom };
    let out = unique(cfg.exports_dir().join(&game).join("Screenshots").join(format!("{stem}.png")));
    std::fs::create_dir_all(out.parent().unwrap())?;
    let ss = format!("{:.3}", req.start.max(0.0));
    let mut c = ff::cmd(&cfg.ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &ss, "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-frames:v", "1", "-an", "-c:v", "png"])
        .arg(&out);
    ff::run(c).context("Couldn't grab that frame")?;
    if !out.is_file() {
        bail!("Couldn't grab that frame. Try a spot a little earlier in the clip.");
    }
    Ok(out)
}

/// `progress` gets the fraction done (0..1).
pub fn export(cfg: &Config, req: &ExportRequest, progress: impl Fn(f64)) -> Result<PathBuf> {
    let input = PathBuf::from(&req.path);
    if !input.is_file() {
        bail!("Clip not found: {}", req.path);
    }
    if req.format == ExportFormat::Png {
        return export_frame(cfg, req, &input);
    }
    let dur = req.end - req.start;
    if dur < 0.2 {
        bail!("The selection is too short to export");
    }

    let fmt = req.format;
    let ext = fmt.ext();
    let webm = fmt == ExportFormat::Webm;
    let gif = fmt == ExportFormat::Gif;

    let game = sanitize_name(&library::game_of(&input));
    let src_stem = input.file_stem().unwrap_or_default().to_string_lossy().into_owned();

    // ---- what are we asked to produce?
    let size_mode = !gif && req.mode == "size" && req.target_mb > 0.0;
    let rate_mode = !gif && req.mode == "bitrate" && req.target_kbps > 0;
    let known_src = req.src_kbps > 0.0;
    let src_codec = ff::probe_codec(&cfg.ffmpeg, &input);
    let hevc = match req.codec {
        ExportCodec::Hevc => true,
        ExportCodec::H264 => false,
        // "same as clip" can't re-encode on its own; in size/bitrate mode it means H.264
        ExportCodec::Keep => false,
    };
    // Original quality but a different codec: a quality-matched GPU re-encode (audio is copied).
    let recode = !size_mode && !rate_mode && !webm && req.codec != ExportCodec::Keep && (if hevc { "hevc" } else { "h264" }) != src_codec;

    if size_mode && known_src {
        let native_mb = req.src_kbps * 1000.0 / 8.0 * dur / 1e6;
        if req.target_mb >= native_mb * 0.98 {
            bail!("That's as big as the clip already is. Pick a smaller size, or export the original.");
        }
    }
    if rate_mode && known_src && (req.target_kbps as f64) > req.src_kbps - 120.0 {
        bail!("That's a higher bitrate than the clip has. Pick a lower one, or export the original.");
    }

    let tag = if gif {
        "gif".into()
    } else if size_mode {
        let mb = req.target_mb;
        if mb.fract() == 0.0 { format!("{mb:.0}MB") } else { format!("{mb:.1}MB") }
    } else if rate_mode {
        let m = req.target_kbps as f64 / 1000.0;
        if m.fract() == 0.0 { format!("{m:.0}Mbps") } else { format!("{m:.1}Mbps") }
    } else if recode {
        (if hevc { "hevc" } else { "h264" }).into()
    } else {
        "trim".into()
    };
    let custom = clean_stem(&req.name, ext);
    let stem = if custom.is_empty() { format!("{src_stem}_{tag}") } else { custom };
    let out = unique(cfg.exports_dir().join(&game).join(format!("{stem}.{ext}")));
    std::fs::create_dir_all(out.parent().unwrap())?;

    let ss = format!("{:.3}", req.start.max(0.0));
    let t = format!("{:.3}", dur);
    let faststart = matches!(fmt, ExportFormat::Mp4 | ExportFormat::Mov);

    // ---- .gif: one palette for the whole clip, no audio
    if gif {
        let fps = req.fps.clamp(5, 30);
        let h = if req.height > 0 { req.height as u32 } else { 480 };
        let vf = format!(
            "fps={fps},scale=-2:'min({h},ih)':flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle"
        );
        let mut c = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
        c.args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &ss, "-i"])
            .arg(&input)
            .args(["-t", &t, "-map", "0:v:0", "-an", "-vf", &vf, "-loop", "0", "-progress", "pipe:1", "-nostats"])
            .arg(&out);
        run_progress(c, dur, 0.0, 1.0, &progress)?;
        return Ok(out);
    }

    // ---- no re-encode
    if !size_mode && !rate_mode && !webm && !recode {
        let mut c = ff::cmd(&cfg.ffmpeg);
        c.args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &ss, "-i"])
            .arg(&input)
            .args(["-t", &t, "-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"]);
        if faststart {
            c.args(["-movflags", "+faststart"]);
        }
        c.args(["-progress", "pipe:1", "-nostats"]).arg(&out);
        run_progress(c, dur, 0.0, 1.0, &progress)?;
        return Ok(out);
    }

    // ---- bitrate budget (MB = 1,000,000 bytes; 4% muxing headroom)
    let (mut video_kbps, audio_kbps) = if size_mode {
        let total_kbps = req.target_mb * 8000.0 * 0.96 / dur;
        let a = audio_tier(total_kbps);
        (((total_kbps - a as f64).max(150.0)) as u32, a)
    } else if rate_mode {
        (req.target_kbps, audio_tier(req.target_kbps as f64 + 128.0))
    } else if recode {
        // Same look in the other codec: scale the clip's own video bitrate.
        let ratio = if hevc { HEVC_RATIO } else { 1.0 / HEVC_RATIO };
        let v = if known_src { (req.src_kbps - 128.0).max(500.0) * ratio } else { 8000.0 };
        (v.max(500.0) as u32, 128)
    } else {
        // WebM "original": keep roughly the clip's own quality.
        let total = if known_src { req.src_kbps * 0.85 } else { 8000.0 };
        ((total - 128.0).max(500.0) as u32, 128)
    };
    let height = match req.height {
        h if h > 0 => h as u32,
        0 => 0,
        _ => if size_mode { auto_height(video_kbps) } else { 0 },
    };
    let vf = (height > 0).then(|| format!("scale=-2:'min({height},ih)'"));
    let ab = format!("{audio_kbps}k");
    let (acodec, vcodec_cpu) = if webm {
        ("libopus", "libvpx-vp9")
    } else if hevc {
        ("aac", "libx265")
    } else {
        ("aac", "libx264")
    };

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = if req.low_impact { (cores / 2).max(2) } else { cores }.to_string();

    let base = |c: &mut Command| {
        c.args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &ss]);
        c.arg("-i").arg(&input).args(["-t", &t]);
        if let Some(vf) = &vf {
            c.args(["-vf", vf]);
        }
        c.args(["-progress", "pipe:1", "-nostats"]);
    };
    let vp9_flags = ["-deadline", "good", "-cpu-used", "4", "-row-mt", "1", "-tile-columns", "2", "-pix_fmt", "yuv420p"];

    // ---- .webm: VP9 + Opus on the CPU
    if webm {
        let vb = format!("{video_kbps}k");
        if req.precise {
            let log = std::env::temp_dir().join(format!("clipr_2pass_{}", std::process::id()));
            let logs = log.to_string_lossy().into_owned();
            let mut p1 = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
            base(&mut p1);
            p1.args(["-map", "0:v:0", "-c:v", vcodec_cpu, "-b:v", &vb, "-threads", &threads])
                .args(vp9_flags)
                .args(["-pass", "1", "-passlogfile", &logs, "-an", "-f", "null", "NUL"]);
            let r1 = run_progress(p1, dur, 0.0, 0.5, &progress);
            let r = r1.and_then(|_| {
                let mut p2 = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
                base(&mut p2);
                p2.args(["-map", "0:v:0", "-map", "0:a?", "-c:v", vcodec_cpu, "-b:v", &vb, "-threads", &threads])
                    .args(vp9_flags)
                    .args(["-pass", "2", "-passlogfile", &logs, "-c:a", acodec, "-b:a", &ab])
                    .arg(&out);
                run_progress(p2, dur, 0.5, 0.5, &progress)
            });
            cleanup_passlogs(&log);
            r?;
        } else {
            let mut c = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
            base(&mut c);
            c.args(["-map", "0:v:0", "-map", "0:a?", "-c:v", vcodec_cpu, "-b:v", &vb, "-threads", &threads])
                .args(vp9_flags)
                .args(["-c:a", acodec, "-b:a", &ab])
                .arg(&out);
            run_progress(c, dur, 0.0, 1.0, &progress)?;
        }
        return Ok(out);
    }

    let mut mov_flags: Vec<&str> = if faststart { vec!["-movflags", "+faststart"] } else { vec![] };
    if hevc && faststart {
        // Players and editors only recognise HEVC in .mp4/.mov with this tag.
        mov_flags.extend(["-tag:v", "hvc1"]);
    }

    // ---- exact size: CPU two-pass
    if req.precise {
        let name = format!("clipr_2pass_{}", std::process::id());
        let log = std::env::temp_dir().join(&name);
        let logs = log.to_string_lossy().into_owned();
        let vb = format!("{video_kbps}k");
        let tmp = std::env::temp_dir();
        // x265 takes its pass settings through -x265-params (relative stats file: a drive letter's colon would break the list).
        let pass_args = |n: u8| -> Vec<String> {
            if hevc {
                vec!["-x265-params".into(), format!("pass={n}:stats={name}.x265:log-level=error")]
            } else {
                vec!["-pass".into(), n.to_string(), "-passlogfile".into(), logs.clone()]
            }
        };

        let mut p1 = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
        p1.current_dir(&tmp);
        base(&mut p1);
        p1.args(["-map", "0:v:0", "-c:v", vcodec_cpu, "-preset", "medium", "-threads", &threads, "-b:v", &vb])
            .args(pass_args(1))
            .args(["-an", "-f", "null", "NUL"]);
        let r1 = run_progress(p1, dur, 0.0, 0.5, &progress);

        let r = r1.and_then(|_| {
            let mut p2 = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
            p2.current_dir(&tmp);
            base(&mut p2);
            p2.args(["-map", "0:v:0", "-map", "0:a?", "-c:v", vcodec_cpu, "-preset", "medium", "-threads", &threads, "-b:v", &vb])
                .args(pass_args(2))
                .args(["-pix_fmt", "yuv420p", "-c:a", acodec, "-b:a", &ab])
                .args(&mov_flags)
                .arg(&out);
            run_progress(p2, dur, 0.5, 0.5, &progress)
        });
        cleanup_passlogs(&log);
        r?;
        return Ok(out);
    }

    // ---- GPU single pass; in size mode, if it overshoots, redo once with a scaled-down bitrate.
    let family = if hevc { "hevc" } else { "h264" };
    let enc = match cfg.encoder {
        Encoder::Nvenc => format!("{family}_nvenc"),
        Encoder::Amf => format!("{family}_amf"),
        Encoder::Qsv => format!("{family}_qsv"),
    };
    let target_bytes = req.target_mb * 1_000_000.0;
    for attempt in 0..2 {
        let vb = format!("{video_kbps}k");
        let mut c = ff::cmd_prio(&cfg.ffmpeg, req.low_impact);
        base(&mut c);
        c.args(["-map", "0:v:0", "-map", "0:a?", "-c:v", &enc]);
        match cfg.encoder {
            // p5 instead of p6: nearly the same quality for a fraction of the GPU time.
            Encoder::Nvenc => c.args(["-preset", "p5", "-rc", "cbr", "-multipass", "qres"]),
            Encoder::Amf => c.args(["-quality", "balanced", "-rc", "cbr"]),
            Encoder::Qsv => c.args(["-preset", "medium"]),
        };
        c.args(["-b:v", &vb, "-maxrate", &vb, "-bufsize", &vb, "-pix_fmt", "nv12"]);
        if recode {
            c.args(["-c:a", "copy"]);
        } else {
            c.args(["-c:a", acodec, "-b:a", &ab]);
        }
        c.args(&mov_flags).arg(&out);
        run_progress(c, dur, 0.0, 1.0, &progress)?;

        if !size_mode {
            break;
        }
        let size = std::fs::metadata(&out).map(|m| m.len() as f64).unwrap_or(0.0);
        if size <= target_bytes || attempt == 1 {
            break;
        }
        video_kbps = ((video_kbps as f64) * (target_bytes / size) * 0.95) as u32;
    }
    Ok(out)
}

fn cleanup_passlogs(prefix: &Path) {
    let (Some(dir), Some(name)) = (prefix.parent(), prefix.file_name()) else { return };
    let name = name.to_string_lossy().into_owned();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn run_progress(mut c: Command, dur: f64, base: f64, span: f64, progress: &impl Fn(f64)) -> Result<()> {
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Couldn't start ffmpeg")?;
    let mut err = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let stdout = BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines().map_while(|l| l.ok()) {
        let v = line
            .strip_prefix("out_time_us=")
            .or_else(|| line.strip_prefix("out_time_ms=")); // also microseconds, despite the name
        if let Some(us) = v.and_then(|v| v.trim().parse::<f64>().ok()) {
            progress(base + span * (us / 1e6 / dur).clamp(0.0, 1.0));
        }
    }
    let status = child.wait()?;
    let errs = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("Export failed: {}", ff::tail(&errs));
    }
    progress(base + span);
    Ok(())
}
