//! The benchmark's admin helper: counts a game's real frames.
//!
//! Started as `TUFFClip.exe --bench-helper <out> <ctl> <parent pid>`, first as a normal child
//! (enough for admins' and "Performance Log Users" accounts), and through a UAC prompt when
//! Windows refuses that ("error denied"). It listens to the DXGI and D3D9
//! "Present" events (the same ones PresentMon uses) and writes the QPC timestamp of every
//! frame the chosen game shows, one per line, to `<out>`. TUFFClip steers it through the
//! small `<ctl>` file ("pid <n>" picks the game, "stop" ends it). It also stops when TUFFClip
//! exits or after 30 minutes, so a forgotten helper never keeps running.
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW, CONTROLTRACE_HANDLE,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_INFORMATION, WNODE_FLAG_TRACED_GUID,
};

const SESSION: &str = "TUFFClip Benchmark";
/// Microsoft-Windows-DXGI: Present_Start (42) and PresentMultiplaneOverlay_Start (55).
const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
/// Microsoft-Windows-D3D9: Present_Start (1), for older games.
const D3D9: GUID = GUID::from_u128(0x783ACA0A_790E_4D7F_8451_AA850511C6B9);
const MAX_RUN: Duration = Duration::from_secs(30 * 60);

/// The game's pid (0 = not chosen yet: drop everything).
static TARGET: AtomicU32 = AtomicU32::new(0);
static OUT: Mutex<Option<BufWriter<File>>> = Mutex::new(None);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn write_line(s: &str) {
    if let Ok(mut o) = OUT.lock() {
        if let Some(w) = o.as_mut() {
            let _ = writeln!(w, "{s}");
        }
    }
}

fn flush() {
    if let Ok(mut o) = OUT.lock() {
        if let Some(w) = o.as_mut() {
            let _ = w.flush();
        }
    }
}

unsafe extern "system" fn on_event(r: *mut EVENT_RECORD) {
    let h = &(*r).EventHeader;
    let target = TARGET.load(Relaxed);
    if target == 0 || h.ProcessId != target {
        return;
    }
    let id = h.EventDescriptor.Id;
    let present = (h.ProviderId == DXGI && (id == 42 || id == 55)) || (h.ProviderId == D3D9 && id == 1);
    if present {
        write_line(&h.TimeStamp.to_string());
    }
}

/// EVENT_TRACE_PROPERTIES followed by room for the session name, 8-byte aligned.
struct Props(Vec<u64>);

impl Props {
    fn new() -> Self {
        let bytes = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + (SESSION.len() + 1) * 2 + 64;
        let mut v = Props(vec![0u64; bytes / 8 + 1]);
        let len = (v.0.len() * 8) as u32;
        let p = v.ptr();
        unsafe {
            (*p).Wnode.BufferSize = len;
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            // QPC timestamps, so they line up with TUFFClip's own QueryPerformanceCounter.
            (*p).Wnode.ClientContext = 1;
            (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*p).FlushTimer = 1;
            (*p).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        }
        v
    }
    fn ptr(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.0.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES
    }
}

fn stop_session(h: CONTROLTRACE_HANDLE) {
    let name = wide(SESSION);
    let mut p = Props::new();
    unsafe {
        let n = if h.Value == 0 { PCWSTR(name.as_ptr()) } else { PCWSTR::null() };
        let _ = ControlTraceW(h, n, p.ptr(), EVENT_TRACE_CONTROL_STOP);
    }
}

fn start_session() -> Result<CONTROLTRACE_HANDLE, String> {
    let name = wide(SESSION);
    let mut h = CONTROLTRACE_HANDLE { Value: 0 };
    unsafe {
        let mut p = Props::new();
        let mut r = StartTraceW(&mut h, PCWSTR(name.as_ptr()), p.ptr());
        if r == ERROR_ALREADY_EXISTS {
            // Left over from a helper that was killed: take it down and start again.
            stop_session(CONTROLTRACE_HANDLE { Value: 0 });
            let mut p = Props::new();
            r = StartTraceW(&mut h, PCWSTR(name.as_ptr()), p.ptr());
        }
        if r == ERROR_ACCESS_DENIED {
            // Not admin and not in "Performance Log Users": TUFFClip retries through UAC.
            return Err("denied".into());
        }
        if r != ERROR_SUCCESS {
            return Err(format!("Windows wouldn't start frame tracing (error {})", r.0));
        }
        let level = TRACE_LEVEL_INFORMATION as u8;
        let ctl = EVENT_CONTROL_CODE_ENABLE_PROVIDER.0;
        let r = EnableTraceEx2(h, &DXGI, ctl, level, u64::MAX, 0, 0, None);
        if r != ERROR_SUCCESS {
            stop_session(h);
            return Err(format!("Windows wouldn't trace DirectX frames (error {})", r.0));
        }
        let _ = EnableTraceEx2(h, &D3D9, ctl, level, u64::MAX, 0, 0, None);
    }
    Ok(h)
}

/// Entry point for `--bench-helper`. Returns the process exit code.
pub fn helper_main(args: &[String]) -> i32 {
    let (Some(out), Some(ctl), Some(parent)) =
        (args.first(), args.get(1), args.get(2).and_then(|p| p.parse::<u32>().ok()))
    else {
        return 2;
    };
    // create_new: never write through a file (or link) that someone else put there.
    let Ok(file) = OpenOptions::new().write(true).create_new(true).open(out) else { return 3 };
    *OUT.lock().unwrap() = Some(BufWriter::new(file));

    let session = match start_session() {
        Ok(h) => h,
        Err(e) => {
            write_line(&format!("error {e}"));
            flush();
            return 1;
        }
    };

    let mut name = wide(SESSION);
    let mut lf: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    lf.LoggerName = PWSTR(name.as_mut_ptr());
    lf.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
    lf.Anonymous2.EventRecordCallback = Some(on_event);
    let trace = unsafe { OpenTraceW(&mut lf) };
    if trace.Value == u64::MAX {
        stop_session(session);
        write_line("error Windows wouldn't open the frame trace");
        flush();
        return 1;
    }
    let tv = trace.Value;
    let pump = std::thread::spawn(move || unsafe {
        let h = windows::Win32::System::Diagnostics::Etw::PROCESSTRACE_HANDLE { Value: tv };
        let _ = ProcessTrace(&[h], None, None);
    });

    write_line("ready");
    flush();

    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        flush();
        if let Ok(cmd) = fs::read_to_string(ctl) {
            let cmd = cmd.trim();
            if cmd == "stop" {
                break;
            }
            if let Some(pid) = cmd.strip_prefix("pid ").and_then(|p| p.trim().parse::<u32>().ok()) {
                TARGET.store(pid, Relaxed);
            }
        }
        if !crate::win::process_alive(parent) || started.elapsed() > MAX_RUN {
            break;
        }
    }

    stop_session(session);
    unsafe {
        let _ = CloseTrace(windows::Win32::System::Diagnostics::Etw::PROCESSTRACE_HANDLE { Value: tv });
    }
    let _ = pump.join();
    flush();
    0
}
