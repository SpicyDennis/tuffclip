//! Recording benchmark: how much does recording cost the game?
//!
//! While you play, recording is switched on and off in rounds (on/off, then off/on, so slow
//! drift such as heat or loading hits both sides equally). The first seconds after each switch
//! are skipped, as are seconds where the game wasn't in front. Each second records:
//! - Basic (no admin): GPU engine load from the same counters Task Manager reads, and CPU load.
//! - Full: also the game's real frames, from a small helper (`etw.rs`) that writes the QPC time
//!   of every frame the game shows. It needs admin rights or "Performance Log Users"; without
//!   them TUFFClip asks through a UAC prompt.
//!
//! The result compares each round's "on" and "off" and reports the difference together with
//! how much it varied between rounds, so a difference smaller than the noise is called that.
//! Nothing here runs unless you start a test.
use crate::config::data_dir;
use crate::engine::Engine;
use crate::win;
use anyhow::{bail, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    QueryPerformanceCounter, QueryPerformanceFrequency, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, GetSystemTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

/// Seconds skipped after recording is switched on or off (ffmpeg starting, the hitch of the switch).
const WARM_SECS: u32 = 3;
/// Give up when the game isn't in front for this long in one go.
const AWAY_LIMIT: u32 = 120;
/// How long to wait for the game to be in front and recorded before the test starts.
const WAIT_LIMIT: u32 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Basic,
    Full,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BenchRequest {
    pub access: Access,
    pub rounds: u32,
    pub phase_secs: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Progress {
    /// "admin" (waiting for the UAC answer), "waiting" (for the game), "running", "away", "finishing"
    pub stage: String,
    pub round: u32,
    pub rounds: u32,
    pub on: bool,
    pub secs_left: u32,
    pub pct: f64,
    pub live_fps: Option<f64>,
    pub game: Option<String>,
}

/// Averages for one side (recording on or off). Percentages are 0-100.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Side {
    pub fps: Option<f64>,
    pub low1: Option<f64>,
    /// How busy the GPU's 3D engine was in total.
    pub gpu_total: f64,
    /// The game's own share of it.
    pub gpu_game: f64,
    /// Everything else on it (recording, Windows' capture work, other programs).
    pub gpu_other: f64,
    pub encoder: f64,
    pub cpu_total: f64,
    /// TUFFClip + ffmpeg, as a share of the whole CPU.
    pub cpu_rec: f64,
    pub secs: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchResult {
    pub access: Access,
    pub game: String,
    /// "1920×1080 · 144 fps · 30 Mbps · HEVC"
    pub settings: String,
    pub rounds: u32,
    pub on: Side,
    pub off: Side,
    /// Real frames were counted (Full test, and the game's frames were visible).
    pub frames: bool,
    /// Average FPS change with recording on, % (negative = fewer frames), and its 95% range.
    pub fps_diff: Option<f64>,
    pub fps_noise: Option<f64>,
    pub low_diff: Option<f64>,
    pub low_noise: Option<f64>,
    /// Extra GPU work with recording on (percentage points of the 3D engine).
    pub rec_gpu: f64,
    /// The GPU was (nearly) fully busy with recording on, so its extra work comes out of the game's frames.
    pub gpu_bound: bool,
    /// Basic test: estimated FPS cost in %, from the game's GPU share when the GPU is maxed out.
    pub est_cost: Option<f64>,
    /// Recording settings, for advice: captured height, output height (0 = native), frame rate.
    pub src_h: u32,
    pub height: u32,
    pub rec_fps: u32,
    pub finished_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub running: bool,
    pub progress: Option<Progress>,
    pub result: Option<BenchResult>,
}

static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static PROGRESS: Mutex<Option<Progress>> = parking_lot::const_mutex(None);
static LAST: Mutex<Option<BenchResult>> = parking_lot::const_mutex(None);

const CANCELLED: &str = "The test was cancelled.";

pub fn snapshot() -> Snapshot {
    Snapshot { running: RUNNING.load(SeqCst), progress: PROGRESS.lock().clone(), result: LAST.lock().clone() }
}

pub fn cancel() {
    CANCEL.store(true, SeqCst);
}

pub fn start(app: AppHandle, eng: Arc<Engine>, req: BenchRequest) -> Result<(), String> {
    if RUNNING.swap(true, SeqCst) {
        return Err("A test is already running".into());
    }
    CANCEL.store(false, SeqCst);
    let req = BenchRequest { rounds: req.rounds.clamp(2, 12), phase_secs: req.phase_secs.clamp(10, 60), ..req };
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&app, &eng, &req)));
        // Whatever happened, recording goes back to normal.
        eng.bench_pause(false);
        eng.set_bench_note(None);
        *PROGRESS.lock() = None;
        RUNNING.store(false, SeqCst);
        match res {
            Ok(Ok(r)) => {
                *LAST.lock() = Some(r.clone());
                let _ = app.emit("bench-done", r);
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                let _ = app.emit("bench-error", BenchError { cancelled: msg == CANCELLED, msg });
            }
            Err(_) => {
                let _ = app.emit("bench-error", BenchError { cancelled: false, msg: "The test stopped unexpectedly.".into() });
            }
        }
    });
    Ok(())
}

#[derive(Clone, Serialize)]
struct BenchError {
    msg: String,
    cancelled: bool,
}

// ---------------------------------------------------------------- timing

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

fn qpf() -> f64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut v);
    }
    v.max(1) as f64
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Sleep in small steps so Cancel answers quickly.
fn nap(ms: u64) -> Result<()> {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if CANCEL.load(SeqCst) {
            bail!(CANCELLED);
        }
        std::thread::sleep(Duration::from_millis(50).min(end.saturating_duration_since(Instant::now())));
    }
    if CANCEL.load(SeqCst) {
        bail!(CANCELLED);
    }
    Ok(())
}

// ---------------------------------------------------------------- GPU (no admin)

#[derive(Clone, Copy, Debug, Default)]
struct GpuSample {
    total: f64,
    game: f64,
    encoder: f64,
}

/// `\GPU Engine(*)\Utilization Percentage`: one instance per process and engine, named like
/// `pid_1234_luid_0x0_0xD1B4_phys_0_eng_0_engtype_3D`.
struct Gpu {
    query: isize,
    counter: isize,
}

impl Gpu {
    fn new() -> Option<Gpu> {
        unsafe {
            let mut q = 0isize;
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut q) != 0 {
                return None;
            }
            let mut c = 0isize;
            if PdhAddEnglishCounterW(q, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut c) != 0 {
                PdhCloseQuery(q);
                return None;
            }
            // Rates need two readings; this is the first.
            PdhCollectQueryData(q);
            Some(Gpu { query: q, counter: c })
        }
    }

    fn sample(&mut self, game_pid: u32) -> Option<GpuSample> {
        let mut items: Vec<(u32, String, String, f64)> = Vec::new();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let r = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if r == 0 && count == 0 {
                return Some(GpuSample::default());
            }
            if r != PDH_MORE_DATA {
                return None;
            }
            let mut buf = vec![0u64; size as usize / 8 + 1];
            let ptr = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(ptr)) != 0 {
                return None;
            }
            for it in std::slice::from_raw_parts(ptr, count as usize) {
                if it.FmtValue.CStatus > 1 {
                    continue;
                }
                let Ok(name) = it.szName.to_string() else { continue };
                if let Some((pid, engine, kind)) = parse_engine(&name) {
                    items.push((pid, engine, kind, it.FmtValue.Anonymous.doubleValue));
                }
            }
        }
        // Per engine: everyone's use added up, and the game's.
        let mut d3: HashMap<String, (f64, f64)> = HashMap::new();
        let mut enc: HashMap<String, f64> = HashMap::new();
        for (pid, engine, kind, v) in items {
            if kind == "3D" {
                let e = d3.entry(engine).or_default();
                e.0 += v;
                if pid == game_pid {
                    e.1 += v;
                }
            } else if kind.starts_with("VideoEncode") {
                *enc.entry(engine).or_default() += v;
            }
        }
        // The busiest 3D engine is the game's GPU (an iGPU next to it idles).
        let (total, game) = d3.values().copied().fold((0.0, 0.0), |a, b| if b.0 > a.0 { b } else { a });
        let encoder = enc.values().copied().fold(0.0, f64::max);
        Some(GpuSample { total: total.min(100.0), game: game.min(100.0), encoder: encoder.min(100.0) })
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// `pid_1234_luid_0x0_0xD1B4_phys_0_eng_0_engtype_3D` -> (1234, "0x0_0xD1B4_phys_0_eng_0", "3D")
fn parse_engine(name: &str) -> Option<(u32, String, String)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    let pid = pid.parse().ok()?;
    let rest = rest.strip_prefix("luid_")?;
    let (engine, kind) = rest.split_once("_engtype_")?;
    Some((pid, engine.to_string(), kind.to_string()))
}

// ---------------------------------------------------------------- CPU

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

fn sys_times() -> Option<(u64, u64)> {
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
    // Kernel time includes idle time.
    Some((ft(idle), ft(kernel) + ft(user)))
}

fn proc_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let r = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(ft(k) + ft(u))
    }
}

#[derive(Default)]
struct Cpu {
    sys: Option<(u64, u64)>,
    procs: HashMap<u32, u64>,
}

impl Cpu {
    /// (whole CPU busy %, the given processes' share of the whole CPU %)
    fn sample(&mut self, pids: &[u32]) -> Option<(f64, f64)> {
        let now = sys_times()?;
        let prev = self.sys.replace(now);
        let mut used = 0u64;
        let mut seen = HashMap::new();
        for &pid in pids {
            if let Some(t) = proc_time(pid) {
                if let Some(old) = self.procs.get(&pid) {
                    used += t.saturating_sub(*old);
                }
                seen.insert(pid, t);
            }
        }
        self.procs = seen;
        let (pi, pt) = prev?;
        let total = now.1.saturating_sub(pt);
        if total == 0 {
            return None;
        }
        let idle = now.0.saturating_sub(pi);
        let busy = 100.0 * (1.0 - idle as f64 / total as f64);
        Some((busy.clamp(0.0, 100.0), (100.0 * used as f64 / total as f64).clamp(0.0, 100.0)))
    }
}

// ---------------------------------------------------------------- admin helper

struct Helper {
    process: isize,
    out: PathBuf,
    ctl: PathBuf,
    pos: u64,
    partial: String,
    ready: bool,
    /// Windows refused tracing without admin.
    denied: bool,
    frames: Vec<i64>,
}

impl Helper {
    /// Start the frame helper: as a normal child first (enough for admin accounts and members of
    /// "Performance Log Users"), and through a UAC prompt only if Windows refuses that.
    /// `asking` runs just before the prompt appears.
    fn launch(dir: &Path, asking: impl Fn()) -> Result<Helper> {
        let _ = fs::create_dir_all(dir);
        // Old runs' files; a helper still holding one just keeps it.
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
        if let Some(h) = Self::start(dir, false)? {
            return Ok(h);
        }
        asking();
        match Self::start(dir, true)? {
            Some(h) => Ok(h),
            None => bail!("Windows didn't allow frame tracing, even as admin."),
        }
    }

    /// None: Windows said no (not allowed without admin).
    fn start(dir: &Path, elevated: bool) -> Result<Option<Helper>> {
        let stamp = format!("{}{}", now_ms(), if elevated { "a" } else { "" });
        let out = dir.join(format!("frames-{stamp}.txt"));
        let ctl = dir.join(format!("ctl-{stamp}.txt"));
        fs::write(&ctl, "wait")?;
        let exe = std::env::current_exe()?;
        let process = if elevated {
            let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
            let file = wide(&exe.to_string_lossy());
            let params = wide(&format!(
                "--bench-helper \"{}\" \"{}\" {}",
                out.display(),
                ctl.display(),
                std::process::id()
            ));
            let mut info = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
                lpVerb: w!("runas"),
                lpFile: PCWSTR(file.as_ptr()),
                lpParameters: PCWSTR(params.as_ptr()),
                nShow: 0,
                ..Default::default()
            };
            if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
                if e.code() == ERROR_CANCELLED.to_hresult() {
                    bail!("You didn't allow admin access, so the Full test can't count frames. Try again, or run the Basic test.");
                }
                bail!("Couldn't start the frame counter: {}", e.message());
            }
            if info.hProcess.is_invalid() {
                bail!("Couldn't start the frame counter.");
            }
            info.hProcess.0 as isize
        } else {
            use std::os::windows::io::IntoRawHandle;
            use std::os::windows::process::CommandExt;
            let child = std::process::Command::new(&exe)
                .arg("--bench-helper")
                .arg(&out)
                .arg(&ctl)
                .arg(std::process::id().to_string())
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .spawn()?;
            win::tie_to_app(&child);
            child.into_raw_handle() as isize
        };
        let mut h = Helper {
            process,
            out,
            ctl,
            pos: 0,
            partial: String::new(),
            ready: false,
            denied: false,
            frames: Vec::new(),
        };
        // Wait for it to say it's tracing (or why it can't).
        let start = Instant::now();
        loop {
            nap(150)?;
            h.read()?;
            if h.ready {
                return Ok(Some(h));
            }
            if h.denied {
                return Ok(None);
            }
            if h.exited() {
                h.read()?;
                if h.ready {
                    return Ok(Some(h));
                }
                if h.denied {
                    return Ok(None);
                }
                bail!("The frame counter stopped before it started.");
            }
            if start.elapsed() > Duration::from_secs(20) {
                bail!("The frame counter didn't start in time.");
            }
        }
    }

    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(HANDLE(self.process as _), 0) == WAIT_OBJECT_0 }
    }

    fn command(&self, cmd: &str) {
        let _ = fs::write(&self.ctl, cmd);
    }

    /// Pick up the frames written since last time.
    fn read(&mut self) -> Result<()> {
        let Ok(mut f) = fs::File::open(&self.out) else { return Ok(()) };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return Ok(());
        }
        let mut s = String::new();
        let n = f.read_to_string(&mut s).unwrap_or(0);
        self.pos += n as u64;
        self.partial.push_str(&s);
        let Some(cut) = self.partial.rfind('\n') else { return Ok(()) };
        let done: String = self.partial.drain(..=cut).collect();
        for line in done.lines() {
            let line = line.trim();
            if line == "ready" {
                self.ready = true;
            } else if line == "error denied" {
                self.denied = true;
            } else if let Some(e) = line.strip_prefix("error ") {
                bail!("The frame counter couldn't start: {e}.");
            } else if let Ok(t) = line.parse::<i64>() {
                self.frames.push(t);
            }
        }
        Ok(())
    }

    /// Frames per second over the last couple of seconds (events arrive up to ~1 s late).
    fn live_fps(&self, now: i64, freq: f64) -> Option<f64> {
        let (a, b) = (now - (3.0 * freq) as i64, now - freq as i64);
        let n = self.frames.iter().rev().take(5000).filter(|&&t| t > a && t <= b).count();
        (n > 0).then(|| n as f64 / 2.0)
    }

    /// Stop it and collect the last frames.
    fn finish(&mut self) {
        self.command("stop");
        for _ in 0..30 {
            if self.exited() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.read();
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.command("stop");
        unsafe {
            let _ = CloseHandle(HANDLE(self.process as _));
        }
    }
}

// ---------------------------------------------------------------- the test

/// One second of measurement.
struct Bucket {
    phase: usize,
    on: bool,
    a: i64,
    b: i64,
    gpu: Option<GpuSample>,
    cpu: Option<(f64, f64)>,
}

/// On/off order per round: ABBA, so neither side always goes first.
fn phases(rounds: u32) -> Vec<bool> {
    (0..rounds).flat_map(|r| if r % 2 == 0 { [true, false] } else { [false, true] }).collect()
}

fn run(app: &AppHandle, eng: &Arc<Engine>, req: &BenchRequest) -> Result<BenchResult> {
    let freq = qpf();
    let plan = phases(req.rounds);
    let total_secs: u32 = plan.len() as u32 * req.phase_secs;
    let mut prog = Progress { rounds: req.rounds, ..Default::default() };
    let emit = |p: &Progress| {
        *PROGRESS.lock() = Some(p.clone());
        let _ = app.emit("bench-progress", p.clone());
    };

    let mut helper = if req.access == Access::Full {
        prog.stage = "starting".into();
        emit(&prog);
        let asking = || {
            let p = Progress { stage: "admin".into(), ..prog.clone() };
            emit(&p);
            eng.set_bench_note(Some("Benchmark: waiting for admin permission".into()));
        };
        Some(Helper::launch(&data_dir().join("bench"), asking)?)
    } else {
        None
    };

    // Wait until a game is recorded and in front.
    prog.stage = "waiting".into();
    let (pid, game) = {
        let mut waited = 0;
        loop {
            let target = eng.bench_target();
            prog.game = target.as_ref().map(|t| t.1.clone());
            emit(&prog);
            eng.set_bench_note(Some(format!("Benchmark: switch to {}", prog.game.as_deref().unwrap_or("your game"))));
            if let Some((pid, name)) = target {
                if eng.status().recording && win::foreground().is_some_and(|f| f.pid == pid) {
                    break (pid, name);
                }
            }
            waited += 1;
            if waited > WAIT_LIMIT {
                bail!(match eng.status().error {
                    Some(e) => format!("The test couldn't start: {e}"),
                    None => "The test didn't start: no game was recorded and in front. Start one of your games, switch to it, and try again.".into(),
                });
            }
            nap(1000)?;
        }
    };
    if let Some(h) = &helper {
        h.command(&format!("pid {pid}"));
    }
    let (settings, src_h, height, rec_fps) = {
        let st = eng.status();
        let spec = eng.spec();
        (
            st.summary,
            spec.as_ref().map(|s| s.src_h).unwrap_or(0),
            spec.as_ref().map(|s| s.height).unwrap_or(0),
            spec.as_ref().map(|s| s.fps).unwrap_or(0),
        )
    };
    // Tell the player it has started.
    crate::overlay::flash(true);
    if eng.cfg.lock().beep {
        win::beep();
    }

    let mut buckets: Vec<Bucket> = Vec::new();
    let mut cpu = Cpu::default();
    let mut elapsed_phases = 0u32;
    let measure = req.phase_secs - WARM_SECS;
    for (pi, &on) in plan.iter().enumerate() {
        let switched = pi == 0 || plan[pi - 1] != on;
        eng.bench_pause(!on);
        prog.stage = "running".into();
        prog.round = pi as u32 / 2 + 1;
        prog.on = on;
        eng.set_bench_note(Some(format!(
            "Benchmark: round {} of {} · recording {}",
            prog.round,
            req.rounds,
            if on { "on" } else { "off" }
        )));
        let phase_left = |done: u32| -> u32 {
            let rest = total_secs.saturating_sub((elapsed_phases + 1) * req.phase_secs);
            rest + req.phase_secs.saturating_sub(done)
        };

        // The GPU counter list is made fresh each phase so a new ffmpeg process shows up in it.
        let mut gpu = Gpu::new();
        let warm = if switched { WARM_SECS } else { 0 };
        for s in 0..warm {
            prog.secs_left = phase_left(s);
            prog.pct = 1.0 - prog.secs_left as f64 / total_secs as f64;
            prog.live_fps = helper.as_mut().and_then(|h| h.read().ok().and_then(|_| h.live_fps(qpc(), freq)));
            emit(&prog);
            nap(1000)?;
        }
        if let Some(g) = gpu.as_mut() {
            g.sample(pid);
        }
        let rec_pids = |eng: &Engine| -> Vec<u32> {
            let mut v = vec![std::process::id()];
            v.extend(eng.recorder_pid());
            v
        };
        cpu.sample(&rec_pids(eng));

        let (mut valid, mut away, mut stuck, mut last_ok) = (0u32, 0u32, 0u32, true);
        let mut prev = qpc();
        while valid < measure {
            nap(1000)?;
            let now = qpc();
            if !win::process_alive(pid) {
                bail!("{game} closed, so the test stopped.");
            }
            let g = gpu.as_mut().and_then(|g| g.sample(pid));
            let c = cpu.sample(&rec_pids(eng));
            let front = win::foreground().is_some_and(|f| f.pid == pid);
            let state_ok = eng.status().recording == on;
            let ok = front && state_ok;
            // The second after coming back to the game still has the switch in it.
            if ok && last_ok {
                buckets.push(Bucket { phase: pi, on, a: prev, b: now, gpu: g, cpu: c });
                valid += 1;
                away = 0;
            } else if !front {
                away += 1;
                if away > AWAY_LIMIT {
                    bail!("{game} wasn't in front for two minutes, so the test stopped.");
                }
            } else if !state_ok {
                // In the game, but recording won't come back (ffmpeg failing).
                stuck += 1;
                if stuck > 60 {
                    bail!(match eng.status().error {
                        Some(e) => format!("Recording didn't start again, so the test stopped: {e}"),
                        None => "Recording didn't start again, so the test stopped.".into(),
                    });
                }
            }
            if state_ok {
                stuck = 0;
            }
            last_ok = ok;
            prev = now;
            prog.stage = if front { "running" } else { "away" }.into();
            prog.secs_left = phase_left(warm + valid);
            prog.pct = 1.0 - prog.secs_left as f64 / total_secs as f64;
            prog.live_fps = helper.as_mut().and_then(|h| h.read().ok().and_then(|_| h.live_fps(now, freq)));
            emit(&prog);
        }
        elapsed_phases += 1;
    }

    eng.bench_pause(false);
    prog.stage = "finishing".into();
    prog.secs_left = 0;
    prog.pct = 1.0;
    emit(&prog);
    let frames = match helper.as_mut() {
        Some(h) => {
            // Events arrive up to a second late: give the last ones time to land.
            nap(1500)?;
            h.finish();
            let mut f = std::mem::take(&mut h.frames);
            f.sort_unstable();
            Some(f)
        }
        None => None,
    };
    drop(helper);
    if !buckets.iter().any(|b| b.gpu.is_some()) && frames.as_ref().is_none_or(|f| f.is_empty()) {
        bail!("Windows didn't report any GPU load on this PC, so there's nothing to compare. Try the Full test.");
    }
    crate::overlay::flash(true);
    if eng.cfg.lock().beep {
        win::beep();
    }

    Ok(summarize(req, &plan, &buckets, frames.as_deref(), freq, game, settings, src_h, height, rec_fps))
}

// ---------------------------------------------------------------- results

/// Frames shown in one bucket, and the times between them (ms).
fn frames_in(ts: &[i64], a: i64, b: i64, freq: f64) -> (usize, Vec<f64>) {
    let lo = ts.partition_point(|&t| t <= a);
    let hi = ts.partition_point(|&t| t <= b);
    let s = &ts[lo..hi];
    let gaps = s.windows(2).map(|w| (w[1] - w[0]) as f64 * 1000.0 / freq).collect();
    (s.len(), gaps)
}

/// "1% low" FPS: the frame rate the slowest 1% of frames ran at.
fn low1(gaps: &mut [f64]) -> Option<f64> {
    if gaps.len() < 20 {
        return None;
    }
    gaps.sort_unstable_by(|a, b| a.total_cmp(b));
    let p99 = gaps[((gaps.len() as f64 * 0.99) as usize).min(gaps.len() - 1)];
    (p99 > 0.0).then(|| 1000.0 / p99)
}

/// Mean of the per-round differences and its 95% range (Student's t).
fn mean_range(d: &[f64]) -> (Option<f64>, Option<f64>) {
    const T95: [f64; 10] = [12.71, 4.30, 3.18, 2.78, 2.57, 2.45, 2.36, 2.31, 2.26, 2.23];
    if d.is_empty() {
        return (None, None);
    }
    let n = d.len() as f64;
    let mean = d.iter().sum::<f64>() / n;
    if d.len() < 2 {
        return (Some(mean), None);
    }
    let var = d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let t = T95.get(d.len() - 2).copied().unwrap_or(2.0);
    (Some(mean), Some(t * (var / n).sqrt()))
}

fn pct_change(on: f64, off: f64) -> Option<f64> {
    (off > 0.0).then(|| (on - off) / off * 100.0)
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    req: &BenchRequest,
    plan: &[bool],
    buckets: &[Bucket],
    frames: Option<&[i64]>,
    freq: f64,
    game: String,
    settings: String,
    src_h: u32,
    height: u32,
    rec_fps: u32,
) -> BenchResult {
    // Per phase: (frames, seconds, gaps)
    let mut per_phase: Vec<(usize, f64, Vec<f64>)> = vec![(0, 0.0, Vec::new()); plan.len()];
    for b in buckets {
        let p = &mut per_phase[b.phase];
        p.1 += (b.b - b.a) as f64 / freq;
        if let Some(ts) = frames {
            let (n, gaps) = frames_in(ts, b.a, b.b, freq);
            p.0 += n;
            p.2.extend(gaps);
        }
    }
    let saw_frames = frames.is_some() && per_phase.iter().all(|p| p.1 == 0.0 || p.0 > 0);

    let side = |on: bool| -> Side {
        let bs: Vec<&Bucket> = buckets.iter().filter(|b| b.on == on).collect();
        let avg = |f: &dyn Fn(&Bucket) -> Option<f64>| -> f64 {
            let v: Vec<f64> = bs.iter().filter_map(|b| f(b)).collect();
            if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }
        };
        let mut s = Side {
            gpu_total: avg(&|b| b.gpu.map(|g| g.total)),
            gpu_game: avg(&|b| b.gpu.map(|g| g.game)),
            encoder: avg(&|b| b.gpu.map(|g| g.encoder)),
            cpu_total: avg(&|b| b.cpu.map(|c| c.0)),
            cpu_rec: avg(&|b| b.cpu.map(|c| c.1)),
            secs: bs.iter().map(|b| (b.b - b.a) as f64 / freq).sum(),
            ..Default::default()
        };
        s.gpu_other = (s.gpu_total - s.gpu_game).max(0.0);
        if saw_frames {
            let (mut n, mut secs, mut gaps) = (0usize, 0.0, Vec::new());
            for (i, p) in per_phase.iter().enumerate() {
                if plan[i] == on {
                    n += p.0;
                    secs += p.1;
                    gaps.extend_from_slice(&p.2);
                }
            }
            s.fps = (secs > 0.0).then(|| n as f64 / secs);
            s.low1 = low1(&mut gaps);
        }
        s
    };
    let on = side(true);
    let off = side(false);

    // Paired per round: each round has one "on" and one "off" phase.
    let (mut fps_d, mut low_d) = (Vec::new(), Vec::new());
    if saw_frames {
        for r in 0..plan.len() / 2 {
            let (i, j) = if plan[2 * r] { (2 * r, 2 * r + 1) } else { (2 * r + 1, 2 * r) };
            let (pon, poff) = (&per_phase[i], &per_phase[j]);
            if pon.1 < 3.0 || poff.1 < 3.0 {
                continue;
            }
            if let Some(d) = pct_change(pon.0 as f64 / pon.1, poff.0 as f64 / poff.1) {
                fps_d.push(d);
            }
            let (mut ga, mut gb) = (pon.2.clone(), poff.2.clone());
            if let (Some(a), Some(b)) = (low1(&mut ga), low1(&mut gb)) {
                if let Some(d) = pct_change(a, b) {
                    low_d.push(d);
                }
            }
        }
    }
    let (fps_diff, fps_noise) = mean_range(&fps_d);
    let (low_diff, low_noise) = mean_range(&low_d);

    let rec_gpu = (on.gpu_other - off.gpu_other).max(0.0);
    let gpu_bound = on.gpu_total >= 92.0;
    // A maxed-out GPU shares its time: the game's frames follow the game's share of it.
    let est_cost = gpu_bound.then(|| {
        if off.gpu_game > 5.0 { ((off.gpu_game - on.gpu_game) / off.gpu_game * 100.0).max(0.0) } else { rec_gpu }
    });

    BenchResult {
        access: req.access,
        game,
        settings,
        rounds: req.rounds,
        on,
        off,
        frames: saw_frames,
        fps_diff,
        fps_noise,
        low_diff,
        low_noise,
        rec_gpu,
        gpu_bound,
        est_cost,
        src_h,
        height,
        rec_fps,
        finished_ms: now_ms(),
    }
}
