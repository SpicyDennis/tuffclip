//! The replay buffer.
//!
//! One long-lived ffmpeg process does capture + encode entirely on the GPU:
//!   ddagrab (DXGI Desktop Duplication, frames stay in VRAM as D3D11 textures)
//!     -> optional scale_d3d11 (GPU)
//!     -> NVENC / AMF / QSV hardware encoder
//!     -> either 2-second MPEG-TS segments in a small rotating ring on disk,
//!        or an MPEG-TS stream on stdout that TUFFClip keeps in a RAM ring.
//! No frame is ever copied to system memory, which is why the in-game cost is
//! about the same as ShadowPlay / SteelSeries Moments.
//!
//! Saving a clip = concatenating the newest segments with `-c copy` (no re-encode),
//! which takes well under a second.
use crate::audio::{self, Layout, Track};
use crate::config::{Codec, Config, Encoder};
use crate::{ff, win::{self, MonitorInfo}};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub const SEG_SECS: u32 = 2;

const TS_PACKET: usize = 188;
/// ffmpeg's mpegts muxer puts the first output stream (our video) on this PID.
const VIDEO_PID: u16 = 256;
const PMT_PID: u16 = 4096;

/// A capture region on one monitor, in that monitor's own pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Everything that, if changed, requires restarting ffmpeg.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordSpec {
    pub adapter: u32,
    pub output: u32,
    /// Size of what is captured (the window, or the whole monitor).
    pub src_w: u32,
    pub src_h: u32,
    pub crop: Option<Crop>,
    /// Capture just this window (Windows Graphics Capture) instead of a monitor, so windows in front never show.
    pub window: Option<isize>,
    /// Window capture always outputs `src_w`×`src_h`, scaling the window to fit (capture card window).
    pub force_size: bool,
    pub fps: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub encoder: Encoder,
    pub codec: Codec,
    /// Sound sources, one track each (empty = no sound). Set by the engine, which knows the game.
    pub tracks: Vec<Track>,
    pub audio_kbps: u32,
    pub audio_offset_ms: i32,
    pub draw_mouse: bool,
    pub segments: u32,
    pub ram: bool,
    pub ffmpeg: String,
    pub buffer_dir: PathBuf,
}

impl RecordSpec {
    pub fn new(cfg: &Config, m: &MonitorInfo, crop: Option<Crop>, window: Option<(isize, u32, u32)>) -> Self {
        let (src_w, src_h) = match (window, crop) {
            (Some((_, w, h)), _) => (w, h),
            (None, Some(c)) => (c.w, c.h),
            _ => (m.width, m.height),
        };
        let fps = if cfg.fps == 0 {
            if m.refresh_hz > 0 { m.refresh_hz.clamp(24, 240) } else { 60 }
        } else {
            // Never faster than the display: the extra frames would only be copies.
            if m.refresh_hz > 0 { cfg.fps.min(m.refresh_hz) } else { cfg.fps }
        };
        let mut spec = RecordSpec {
            adapter: m.adapter,
            output: m.output,
            src_w,
            src_h,
            crop,
            window: window.map(|w| w.0),
            force_size: false,
            fps,
            height: cfg.height,
            bitrate_kbps: cfg.bitrate_kbps,
            encoder: cfg.encoder,
            codec: cfg.codec,
            tracks: Vec::new(),
            audio_kbps: cfg.audio_kbps,
            audio_offset_ms: cfg.audio_offset_ms,
            draw_mouse: cfg.draw_mouse,
            segments: cfg.clip_seconds.div_ceil(SEG_SECS) + 4,
            ram: cfg.buffer_in_ram,
            ffmpeg: cfg.ffmpeg.clone(),
            buffer_dir: cfg.buffer_dir(),
        };
        spec.apply_auto_bitrate(cfg);
        spec
    }

    /// With automatic bitrate, derive it from this spec's output size, frame rate and codec.
    /// Call again after changing `fps` by hand.
    pub fn apply_auto_bitrate(&mut self, cfg: &Config) {
        if !cfg.bitrate_auto {
            return;
        }
        let (w, h) = if self.height != 0 && self.height < self.src_h {
            (even_down(self.src_w * self.height / self.src_h), even_down(self.height))
        } else {
            (self.src_w, self.src_h)
        };
        self.bitrate_kbps = crate::config::auto_bitrate(w, h, self.fps, self.codec);
    }
}

fn even_down(x: u32) -> u32 {
    x / 2 * 2
}

/// Turn a window's client rectangle into a capture region on `m`.
/// Returns None when the window fills the monitor (record the whole screen).
pub fn crop_for(m: &MonitorInfo, g: &win::WinGeom) -> Option<Crop> {
    let (mw, mh) = (m.width as i64, m.height as i64);
    let rx = g.x as i64 - m.x as i64;
    let ry = g.y as i64 - m.y as i64;
    let x0 = rx.max(0);
    let y0 = ry.max(0);
    let x1 = (rx + g.w as i64).min(mw);
    let y1 = (ry + g.h as i64).min(mh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (mut w, mut h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    // Covers (nearly) the whole monitor: that's a fullscreen game.
    if (w as u64) * (h as u64) * 100 >= (mw as u64) * (mh as u64) * 97 {
        return None;
    }
    // Encoders choke on tiny or odd-sized frames.
    w = w.max(256).min(m.width);
    h = h.max(144).min(m.height);
    let x = (x0 as u32).min(m.width - w);
    let y = (y0 as u32).min(m.height - h);
    Some(Crop { x: even_down(x), y: even_down(y), w: even_down(w), h: even_down(h) })
}

// -------------------------------------------------------------- RAM buffer

struct RamInner {
    chunks: VecDeque<Arc<Vec<u8>>>,
    cur: Vec<u8>,
    cur_start: Instant,
    seen_key: bool,
    pat: Vec<u8>,
    pmt: Vec<u8>,
    total: usize,
    last_write: Instant,
    max_chunks: usize,
}

/// Rolling buffer of the newest few seconds, kept in memory.
/// ffmpeg writes MPEG-TS to a pipe; we cut it into chunks at video keyframes.
pub struct RamBuf {
    inner: Mutex<RamInner>,
}

impl RamBuf {
    fn new(max_chunks: usize) -> Self {
        RamBuf {
            inner: Mutex::new(RamInner {
                chunks: VecDeque::new(),
                cur: Vec::new(),
                cur_start: Instant::now(),
                seen_key: false,
                pat: Vec::new(),
                pmt: Vec::new(),
                total: 0,
                last_write: Instant::now(),
                max_chunks: max_chunks.max(2),
            }),
        }
    }

    pub fn bytes(&self) -> u64 {
        let g = self.inner.lock();
        (g.total + g.cur.len()) as u64
    }

    pub fn idle(&self) -> Duration {
        self.inner.lock().last_write.elapsed()
    }

    /// Header packets plus the newest `n` chunks (including the one being written).
    fn snapshot(&self, n: usize) -> (Vec<u8>, Vec<Arc<Vec<u8>>>) {
        let g = self.inner.lock();
        let mut parts: Vec<Arc<Vec<u8>>> = g.chunks.iter().cloned().collect();
        if !g.cur.is_empty() {
            parts.push(Arc::new(g.cur.clone()));
        }
        let skip = parts.len().saturating_sub(n);
        let parts = parts.split_off(skip);
        let mut header = g.pat.clone();
        header.extend_from_slice(&g.pmt);
        (header, parts)
    }

    fn feed(&self, packets: &[u8]) {
        let mut g = self.inner.lock();
        g.last_write = Instant::now();
        for pkt in packets.chunks_exact(TS_PACKET) {
            g.push(pkt);
        }
    }
}

impl RamInner {
    fn push(&mut self, pkt: &[u8]) {
        let pid = (((pkt[1] & 0x1f) as u16) << 8) | pkt[2] as u16;
        if pid == 0 {
            self.pat = pkt.to_vec();
        } else if pid == PMT_PID {
            self.pmt = pkt.to_vec();
        }
        let start = pkt[1] & 0x40 != 0;
        let has_af = (pkt[3] >> 4) & 2 != 0;
        let key = pid == VIDEO_PID && start && has_af && pkt[4] > 0 && pkt[5] & 0x40 != 0;
        if key {
            if self.seen_key {
                self.rotate();
            } else {
                // Anything before the first keyframe can't be decoded; drop it.
                self.cur.clear();
                self.seen_key = true;
                self.cur_start = Instant::now();
            }
        } else if self.cur_start.elapsed() > Duration::from_secs(6) && !self.cur.is_empty() {
            // No keyframe flag seen for a while: cut by time so memory stays bounded.
            self.rotate();
        }
        self.cur.extend_from_slice(pkt);
    }

    fn rotate(&mut self) {
        let done = std::mem::take(&mut self.cur);
        self.total += done.len();
        self.chunks.push_back(Arc::new(done));
        while self.chunks.len() > self.max_chunks {
            if let Some(old) = self.chunks.pop_front() {
                self.total -= old.len();
            }
        }
        self.cur_start = Instant::now();
    }
}

fn read_ram(mut out: ChildStdout, ram: Arc<RamBuf>) {
    let mut buf = vec![0u8; TS_PACKET * 512];
    let mut carry: Vec<u8> = Vec::with_capacity(TS_PACKET * 600);
    loop {
        let n = match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        let mut i = 0;
        // Find the packet boundary, then hand over whole packets only.
        while carry.len() - i >= TS_PACKET && carry[i] != 0x47 {
            i += 1;
        }
        let whole = (carry.len() - i) / TS_PACKET * TS_PACKET;
        if whole > 0 {
            ram.feed(&carry[i..i + whole]);
        }
        carry.drain(..i + whole);
    }
}

// ---------------------------------------------------------------- recorder

pub struct BufferInfo {
    pub bytes: u64,
    pub ram: bool,
    /// Seconds since ffmpeg last produced any data.
    pub idle_secs: u64,
}

#[derive(Default)]
pub struct Recorder {
    child: Option<Child>,
    spec: Option<RecordSpec>,
    stop_audio: Option<Arc<AtomicBool>>,
    started: Option<Instant>,
    ram: Option<Arc<RamBuf>>,
    layout: Option<Layout>,
}

/// A stopped recording's buffer, kept so a clip can still be saved from it.
pub struct Kept {
    pub dir: PathBuf,
    pub ram: Option<Arc<RamBuf>>,
    pub layout: Option<Layout>,
}

impl Recorder {
    pub fn spec(&self) -> Option<&RecordSpec> {
        self.spec.as_ref()
    }

    pub fn ram(&self) -> Option<Arc<RamBuf>> {
        self.ram.clone()
    }

    pub fn layout(&self) -> Option<Layout> {
        self.layout.clone()
    }

    pub fn is_alive(&mut self) -> bool {
        match &mut self.child {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// How much the buffer holds right now, and whether it is still growing.
    pub fn buffer_info(&self) -> Option<BufferInfo> {
        let spec = self.spec.as_ref()?;
        if let Some(r) = &self.ram {
            return Some(BufferInfo { bytes: r.bytes(), ram: true, idle_secs: r.idle().as_secs() });
        }
        let mut bytes = 0u64;
        let mut newest = SystemTime::UNIX_EPOCH;
        for e in fs::read_dir(&spec.buffer_dir).ok()?.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) != Some("ts") {
                continue;
            }
            if let Ok(m) = e.metadata() {
                bytes += m.len();
                if let Ok(t) = m.modified() {
                    newest = newest.max(t);
                }
            }
        }
        let idle = if newest == SystemTime::UNIX_EPOCH {
            self.uptime().as_secs()
        } else {
            SystemTime::now().duration_since(newest).map(|d| d.as_secs()).unwrap_or(0)
        };
        Some(BufferInfo { bytes, ram: false, idle_secs: idle })
    }

    pub fn start(&mut self, spec: RecordSpec, log_path: &Path) -> Result<()> {
        self.stop();
        if !spec.ram {
            clear_buffer(&spec.buffer_dir)?;
        }

        let args = build_args(&spec);

        // Keep the log from growing forever.
        if fs::metadata(log_path).map(|m| m.len() > 4 << 20).unwrap_or(false) {
            let _ = fs::remove_file(log_path);
        }
        let mut log = OpenOptions::new().create(true).append(true).open(log_path)?;
        let _ = writeln!(log, "\n=== {} | {} {}", chrono::Local::now(), spec.ffmpeg, args.join(" "));

        let mut cmd = ff::cmd(&spec.ffmpeg);
        cmd.args(&args)
            .stdout(if spec.ram { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::from(log))
            .stdin(if spec.tracks.is_empty() { Stdio::null() } else { Stdio::piped() });
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Couldn't start ffmpeg (\"{}\"). Is it installed?", spec.ffmpeg))?;
        win::tie_to_app(&child);

        if spec.ram {
            let ram = Arc::new(RamBuf::new(spec.segments as usize));
            let out = child.stdout.take().expect("piped stdout");
            let r = ram.clone();
            std::thread::spawn(move || read_ram(out, r));
            self.ram = Some(ram);
        }

        if !spec.tracks.is_empty() {
            let stdin = child.stdin.take().expect("piped stdin");
            let stop = Arc::new(AtomicBool::new(false));
            let layout = Layout::new(&spec.tracks);
            audio::spawn(stdin, &spec.tracks, &layout, stop.clone());
            self.stop_audio = Some(stop);
            self.layout = Some(layout);
        }

        self.child = Some(child);
        self.spec = Some(spec);
        self.started = Some(Instant::now());
        Ok(())
    }

    /// Raw handle of the ffmpeg process, for memory readouts.
    pub fn child_handle(&self) -> Option<isize> {
        use std::os::windows::io::AsRawHandle;
        self.child.as_ref().map(|c| c.as_raw_handle() as isize)
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Stop ffmpeg but hand back what the buffer holds, so a clip can still be saved afterwards.
    /// Returns None if nothing was running or the buffer is empty.
    pub fn stop_and_keep(&mut self) -> Option<Kept> {
        let spec = self.spec.clone()?;
        let ram = self.ram.clone();
        let layout = self.layout.clone();
        let has_data = match &ram {
            Some(r) => r.bytes() > 0,
            None => dir_bytes(&spec.buffer_dir) > 0,
        };
        self.stop();
        has_data.then_some(Kept { dir: spec.buffer_dir, ram, layout })
    }

    pub fn stop(&mut self) {
        if let Some(s) = self.stop_audio.take() {
            s.store(true, Relaxed);
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.spec = None;
        self.started = None;
        self.ram = None;
        self.layout = None;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Size of the buffer segments in `dir`.
pub fn dir_bytes(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("ts"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn clear_buffer(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    for e in fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if matches!(p.extension().and_then(|x| x.to_str()), Some("ts" | "txt")) {
            let _ = fs::remove_file(p);
        }
    }
    Ok(())
}

fn even(x: u32) -> u32 {
    (x / 2 * 2).max(2)
}

fn build_args(s: &RecordSpec) -> Vec<String> {
    let tracks = s.tracks.len();
    let mut a: Vec<String> = Vec::new();
    macro_rules! arg { ($($x:expr),* $(,)?) => {{ $( a.push($x.to_string()); )* }} }

    arg!("-hide_banner", "-loglevel", "warning", "-y");
    if tracks == 0 {
        arg!("-nostdin");
    }
    // Create the D3D11 device on the adapter that owns this monitor.
    arg!("-init_hw_device", format!("d3d11va=d3d:{}", s.adapter), "-filter_hw_device", "d3d");

    if tracks > 0 {
        if s.audio_offset_ms != 0 {
            arg!("-itsoffset", format!("{:.3}", s.audio_offset_ms as f64 / 1000.0));
        }
        if ff::input_queue_size_ok(&s.ffmpeg) {
            arg!("-thread_queue_size", 4096);
        }
        // All tracks interleaved as one stream: track 0 left/right, track 1 left/right, ...
        arg!("-f", "f32le", "-ar", audio::RATE, "-ac", tracks * 2, "-i", "pipe:0");
    }

    let mut vf = if let Some(hwnd) = s.window {
        // Window capture: only this window's pixels, however many windows are on top of it.
        // Frames only arrive when the window changes, so `fps` fills the gaps to keep a steady rate.
        // A resized window is letterboxed into the original size instead of restarting the recording.
        // The capture card window is scaled to the card's own resolution, whatever size the window is.
        let size = if s.force_size { format!("width={}:height={}", s.src_w, s.src_h) } else { "width=-2:height=-2".into() };
        format!(
            "gfxcapture=hwnd={hwnd}:capture_cursor={}:display_border=0:max_framerate={}:{size}:resize_mode=scale_aspect,fps={}",
            s.draw_mouse as u8, s.fps, s.fps
        )
    } else {
        let mut v = format!(
            "ddagrab=output_idx={}:framerate={}:draw_mouse={}",
            s.output, s.fps, s.draw_mouse as u8
        );
        if let Some(c) = s.crop {
            v += &format!(":video_size={}x{}:offset_x={}:offset_y={}", c.w, c.h, c.x, c.y);
        }
        v
    };
    if s.height != 0 && s.height < s.src_h {
        let w = even(s.src_w * s.height / s.src_h);
        vf += &format!(",scale_d3d11={}:{}", w, even(s.height));
    }
    if s.encoder == Encoder::Qsv {
        vf += ",hwmap=derive_device=qsv,format=qsv";
    }
    vf += "[v]";
    if tracks > 1 {
        // Split the pipe back into one stereo track per source, plus a mix of them all first,
        // so the clip sounds right in any player (which only plays the first track).
        vf += &format!(";[0:a]asplit={}[mixin]", tracks + 1);
        for i in 0..tracks {
            vf += &format!("[in{i}]");
        }
        let sum = |side: usize| (0..tracks).map(|i| format!("c{}", i * 2 + side)).collect::<Vec<_>>().join("+");
        vf += &format!(";[mixin]pan=stereo|c0={}|c1={}[amix]", sum(0), sum(1));
        for i in 0..tracks {
            vf += &format!(";[in{i}]pan=stereo|c0=c{}|c1=c{}[a{i}]", i * 2, i * 2 + 1);
        }
    }
    arg!("-filter_complex", vf, "-map", "[v]");

    let codec = match s.codec { Codec::H264 => "h264", Codec::Hevc => "hevc" };
    let enc = match s.encoder { Encoder::Nvenc => "nvenc", Encoder::Amf => "amf", Encoder::Qsv => "qsv" };
    let b = s.bitrate_kbps;
    let gop = s.fps * SEG_SECS;
    arg!("-c:v", format!("{codec}_{enc}"));
    match s.encoder {
        // p4 = balanced; encoding happens on the dedicated NVENC block, not the 3D cores.
        Encoder::Nvenc => arg!("-preset", "p4", "-rc", "vbr", "-bf", 0),
        Encoder::Amf => arg!("-usage", "transcoding", "-quality", "speed", "-rc", "vbr_peak"),
        Encoder::Qsv => arg!("-preset", "veryfast"),
    }
    arg!(
        "-b:v", format!("{b}k"),
        "-maxrate", format!("{}k", b * 3 / 2),
        "-bufsize", format!("{}k", b * 2),
        "-g", gop, "-keyint_min", gop,
    );

    if tracks == 1 {
        arg!("-map", "0:a");
    } else if tracks > 1 {
        arg!("-map", "[amix]");
        for i in 0..tracks {
            arg!("-map", format!("[a{i}]"));
        }
    }
    if tracks > 0 {
        arg!("-c:a", "aac", "-b:a", format!("{}k", s.audio_kbps), "-ac", 2);
    }

    if s.ram {
        arg!(
            "-f", "mpegts",
            "-mpegts_start_pid", VIDEO_PID,
            "-mpegts_pmt_start_pid", PMT_PID,
            "-flush_packets", 1,
            "pipe:1",
        );
    } else {
        arg!(
            "-f", "segment",
            "-segment_time", SEG_SECS,
            "-segment_format", "mpegts",
            "-segment_wrap", s.segments,
            "-reset_timestamps", 1,
            s.buffer_dir.join("seg%03d.ts").to_string_lossy(),
        );
    }
    a
}

fn prepare_out(out: &Path) -> Result<()> {
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// `-map` arguments for the sound of a saved clip: the chosen buffer streams with their titles
/// (None = every audio stream, untitled).
fn audio_maps(sel: Option<&[(usize, String)]>) -> Vec<String> {
    let Some(sel) = sel else { return vec!["-map".into(), "0:a?".into()] };
    let mut a = Vec::new();
    for (k, (i, title)) in sel.iter().enumerate() {
        a.extend(["-map".into(), format!("0:a:{i}")]);
        a.extend([format!("-metadata:s:a:{k}"), format!("title={title}")]);
        a.extend([format!("-metadata:s:a:{k}"), format!("handler_name={title}")]);
        a.extend([format!("-disposition:a:{k}"), (if k == 0 { "default" } else { "0" }).into()]);
    }
    a
}

/// Stitch the newest segments into an mp4 at `out`. Stream copy, no re-encode.
/// `sound`: which audio streams to keep, and their names (None = all).
pub fn save_buffer(ffmpeg: &str, buffer_dir: &Path, seconds: u32, out: &Path, gentle: bool, sound: Option<&[(usize, String)]>) -> Result<()> {
    let mut segs: Vec<(SystemTime, PathBuf)> = fs::read_dir(buffer_dir)?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "ts" {
                return None;
            }
            let m = e.metadata().ok()?;
            (m.len() > 0).then_some((m.modified().ok()?, p))
        })
        .collect();
    segs.sort();

    // +1 because the newest segment is still being written (it's partial).
    let need = (seconds.div_ceil(SEG_SECS) + 1) as usize;
    let chosen = &segs[segs.len().saturating_sub(need)..];
    if chosen.is_empty() {
        bail!("The buffer is empty. Recording may have just started.");
    }

    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let list = buffer_dir.join(format!("concat_{stamp}.txt"));
    let mut body = String::new();
    for (_, p) in chosen {
        let s = p.to_string_lossy().replace('\\', "/").replace('\'', "'\\''");
        body += &format!("file '{s}'\n");
    }
    fs::write(&list, body)?;
    prepare_out(out)?;

    let mut c = if gentle { ff::cmd_low(ffmpeg) } else { ff::cmd(ffmpeg) };
    c.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-map", "0:v:0"])
        .args(audio_maps(sound))
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(out);
    let r = ff::run(c);
    let _ = fs::remove_file(&list);
    r
}

/// Same as `save_buffer`, but the segments come from memory and are piped into ffmpeg.
pub fn save_ram(ffmpeg: &str, ram: &RamBuf, seconds: u32, out: &Path, gentle: bool, sound: Option<&[(usize, String)]>) -> Result<()> {
    let need = (seconds.div_ceil(SEG_SECS) + 1) as usize;
    let (header, parts) = ram.snapshot(need);
    if parts.is_empty() {
        bail!("The buffer is empty. Recording may have just started.");
    }
    prepare_out(out)?;

    let mut c = if gentle { ff::cmd_low(ffmpeg) } else { ff::cmd(ffmpeg) };
    c.args([
        "-hide_banner", "-loglevel", "error", "-y", "-f", "mpegts", "-i", "pipe:0", "-map", "0:v:0",
    ])
    .args(audio_maps(sound))
    .args(["-c", "copy", "-avoid_negative_ts", "make_zero", "-movflags", "+faststart"])
    .arg(out)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let mut child = c.spawn().context("Couldn't start ffmpeg")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&header);
        for p in parts {
            if stdin.write_all(&p).is_err() {
                break;
            }
        }
    });
    let o = child.wait_with_output()?;
    let _ = writer.join();
    if !o.status.success() {
        bail!("ffmpeg failed: {}", ff::tail(&String::from_utf8_lossy(&o.stderr)));
    }
    Ok(())
}
