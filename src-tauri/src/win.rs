//! Thin wrappers around the Win32 APIs TUFFClip needs.
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, EnumDisplaySettingsW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetForegroundWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE, MB_ICONHAND, MB_OK,
    WS_EX_TOOLWINDOW,
};

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---------------------------------------------------------------- monitors

#[derive(Clone, Debug, Serialize)]
pub struct MonitorInfo {
    /// DXGI device name, e.g. `\\.\DISPLAY1` (stable id saved in config).
    pub id: String,
    pub label: String,
    /// DXGI adapter + output index — exactly what ffmpeg's ddagrab expects.
    pub adapter: u32,
    pub output: u32,
    pub width: u32,
    pub height: u32,
    /// Top-left of the monitor on the virtual desktop.
    pub x: i32,
    pub y: i32,
    /// Current refresh rate in Hz (0 if unknown).
    pub refresh_hz: u32,
    pub primary: bool,
    #[serde(skip)]
    pub hmon: isize,
}

fn refresh_rate(device: &[u16]) -> u32 {
    unsafe {
        let mut dm = DEVMODEW::default();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            let f = dm.dmDisplayFrequency;
            if f > 1 { f } else { 0 }
        } else {
            0
        }
    }
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(d) = output.GetDesc() {
                    if d.AttachedToDesktop.as_bool() {
                        let r = d.DesktopCoordinates;
                        out.push(MonitorInfo {
                            id: wide_to_string(&d.DeviceName),
                            label: String::new(),
                            adapter: a,
                            output: o,
                            width: (r.right - r.left).unsigned_abs(),
                            height: (r.bottom - r.top).unsigned_abs(),
                            x: r.left,
                            y: r.top,
                            refresh_hz: refresh_rate(&d.DeviceName),
                            primary: r.left == 0 && r.top == 0,
                            hmon: d.Monitor.0 as isize,
                        });
                    }
                }
                o += 1;
            }
            a += 1;
        }
    }
    for (i, m) in out.iter_mut().enumerate() {
        m.label = format!(
            "Display {} ({}×{}, {} Hz){}",
            i + 1,
            m.width,
            m.height,
            m.refresh_hz,
            if m.primary { ", primary" } else { "" }
        );
    }
    out
}

// ------------------------------------------------------------- processes

pub struct Foreground {
    pub pid: u32,
    pub exe: String,
    pub hwnd: isize,
    pub hmon: isize,
}

/// Where a window's drawable area sits on the virtual desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WinGeom {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub hmon: isize,
    pub minimized: bool,
}

/// Client-area rectangle of a window (None if the window is gone).
pub fn window_geometry(hwnd: isize) -> Option<WinGeom> {
    unsafe {
        let hwnd = HWND(hwnd as _);
        if !IsWindow(hwnd).as_bool() {
            return None;
        }
        let minimized = IsIconic(hwnd).as_bool();
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc).ok()?;
        let mut pt = POINT { x: 0, y: 0 };
        if !ClientToScreen(hwnd, &mut pt).as_bool() {
            return None;
        }
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        Some(WinGeom {
            x: pt.x,
            y: pt.y,
            w: (rc.right - rc.left).max(0) as u32,
            h: (rc.bottom - rc.top).max(0) as u32,
            hmon: hmon.0 as isize,
            minimized,
        })
    }
}

fn exe_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn exe_name(pid: u32) -> Option<String> {
    let path = exe_path(pid)?;
    std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

/// The window in front, without looking up its program (cheap enough to call often).
pub fn foreground_hwnd() -> Option<isize> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd.0 as isize)
}

pub fn foreground() -> Option<Foreground> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let exe = exe_name(pid)?;
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        Some(Foreground { pid, exe, hwnd: hwnd.0 as isize, hmon: hmon.0 as isize })
    }
}

pub fn process_alive(pid: u32) -> bool {
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code).is_ok();
        let _ = CloseHandle(h);
        ok && code == STILL_ACTIVE
    }
}

/// The main Discord process (stable, PTB or Canary): the one whose parent isn't Discord itself,
/// so capturing its process tree covers the voice and every helper process.
pub fn discord_pid() -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    const NAMES: [&str; 3] = ["discord.exe", "discordptb.exe", "discordcanary.exe"];
    let mut procs: Vec<(u32, u32)> = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let name = wide_to_string(&e.szExeFile).to_lowercase();
            if NAMES.contains(&name.as_str()) {
                procs.push((e.th32ProcessID, e.th32ParentProcessID));
            }
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    procs.iter().find(|(_, parent)| !procs.iter().any(|(p, _)| p == parent)).map(|(pid, _)| *pid)
}

#[derive(Serialize)]
pub struct AppWindow {
    pub exe: String,
    pub title: String,
}

/// Visible top-level windows, one entry per executable — used for the game picker.
pub fn list_windows() -> Vec<AppWindow> {
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(u32, String)>);
        if IsWindowVisible(hwnd).as_bool() {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            // Our own windows are skipped before reading their text: for a window of this
            // process that means a message to (and a wait on) the thread that owns it.
            let len = if pid == std::process::id() { 0 } else { GetWindowTextLengthW(hwnd) };
            if len > 0 {
                let mut buf = vec![0u16; len as usize + 1];
                let n = GetWindowTextW(hwnd, &mut buf);
                v.push((pid, String::from_utf16_lossy(&buf[..n.max(0) as usize])));
            }
        }
        TRUE
    }

    let mut raw: Vec<(u32, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }

    const SKIP: &[&str] = &[
        "explorer.exe", "textinputhost.exe", "applicationframehost.exe",
        "systemsettings.exe", "msedgewebview2.exe", "tuffclip.exe", "searchhost.exe",
        "shellexperiencehost.exe", "startmenuexperiencehost.exe",
    ];
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (pid, title) in raw {
        let Some(exe) = exe_name(pid) else { continue };
        let key = exe.to_lowercase();
        if SKIP.contains(&key.as_str()) || !seen.insert(key) {
            continue;
        }
        out.push(AppWindow { exe, title });
    }
    out.sort_by_key(|a| a.exe.to_lowercase());
    out
}

/// One running window that belongs to a game from the user's list.
#[derive(Clone, Debug)]
pub struct RunWin {
    pub pid: u32,
    pub exe: String,
    pub hwnd: isize,
}

/// Program names of the processes seen by the last `running_games` call. This runs every second
/// while TUFFClip waits for a game, so a process is looked up once, not on every tick.
static EXE_NAMES: Mutex<Option<HashMap<u32, Option<String>>>> = Mutex::new(None);

/// For each of the given executables (lowercase) that has a visible window, its largest window.
/// Used to find which listed games are running right now, focused or not.
pub fn running_games(exes: &HashSet<String>) -> Vec<RunWin> {
    struct Ctx {
        me: u32,
        wins: Vec<(u32, isize, u64)>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // Never one of ours: reading our own window's text would wait on the thread that owns it.
        if pid == 0 || pid == ctx.me || GetWindowTextLengthW(hwnd) <= 0 {
            return TRUE;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 == 0 {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let area = ((rc.right - rc.left).max(0) as u64) * ((rc.bottom - rc.top).max(0) as u64);
            ctx.wins.push((pid, hwnd.0 as isize, area));
        }
        TRUE
    }
    let mut ctx = Ctx { me: std::process::id(), wins: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    let mut cache = EXE_NAMES.lock();
    let old = cache.take().unwrap_or_default();
    let mut names: HashMap<u32, Option<String>> = HashMap::new();
    let mut best: HashMap<u32, (isize, u64, String)> = HashMap::new();
    for (pid, hwnd, area) in ctx.wins {
        let exe = names
            .entry(pid)
            .or_insert_with(|| old.get(&pid).cloned().unwrap_or_else(|| exe_name(pid)))
            .clone();
        let Some(exe) = exe else { continue };
        if !exes.contains(&exe.to_lowercase()) {
            continue;
        }
        let e = best.entry(pid).or_insert((hwnd, area, exe));
        if area > e.1 {
            e.0 = hwnd;
            e.1 = area;
        }
    }
    // Only processes that still have a window are kept, so a reused process id is looked up again.
    *cache = Some(names);
    let mut out: Vec<RunWin> = best.into_iter().map(|(pid, (hwnd, _, exe))| RunWin { pid, exe, hwnd }).collect();
    out.sort_by_key(|w| w.pid);
    out
}

/// The window's title bar text.
pub fn window_title(hwnd: isize) -> Option<String> {
    unsafe {
        let h = HWND(hwnd as _);
        let len = GetWindowTextLengthW(h);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(h, &mut buf);
        Some(String::from_utf16_lossy(&buf[..n.max(0) as usize]))
    }
}

/// Whether `hwnd` still exists.
pub fn window_exists(hwnd: isize) -> bool {
    unsafe { IsWindow(HWND(hwnd as _)).as_bool() }
}

// ---------------------------------------------------------------- memory

#[derive(Serialize, Default, Clone)]
pub struct MemInfo {
    /// TUFFClip's own core plus ffmpeg (the settings window's webview is not included).
    pub tuffclip_bytes: u64,
    pub system_used: u64,
    pub system_total: u64,
}

fn working_set(h: HANDLE) -> u64 {
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS::default();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(h, &mut c, c.cb).is_ok() { c.WorkingSetSize as u64 } else { 0 }
    }
}

/// `ffmpeg` is the raw handle of the recorder's child process, if one is running.
pub fn memory_info(ffmpeg: Option<isize>) -> MemInfo {
    let mut m = MemInfo::default();
    unsafe {
        m.tuffclip_bytes = working_set(GetCurrentProcess());
        if let Some(h) = ffmpeg {
            m.tuffclip_bytes += working_set(HANDLE(h as _));
        }
        let mut s = MEMORYSTATUSEX::default();
        s.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut s).is_ok() {
            m.system_total = s.ullTotalPhys;
            m.system_used = s.ullTotalPhys.saturating_sub(s.ullAvailPhys);
        }
    }
    m
}

// ------------------------------------------------------------------ misc

pub fn beep() {
    unsafe {
        let _ = MessageBeep(MB_OK);
    }
}

/// Distinct sound for "the clip did not save", so you know without alt-tabbing.
pub fn beep_error() {
    unsafe {
        let _ = MessageBeep(MB_ICONHAND);
    }
}

/// Put a child process in a job object that is killed when TUFFClip exits —
/// even if TUFFClip crashes — so ffmpeg never keeps running orphaned.
pub fn tie_to_app(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    static JOB: OnceLock<isize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let Ok(job) = CreateJobObjectW(None, PCWSTR::null()) else { return 0 };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let _ = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        job.0 as isize
    });
    if job != 0 {
        unsafe {
            let _ = AssignProcessToJobObject(HANDLE(job as _), HANDLE(child.as_raw_handle() as _));
        }
    }
}

/// Windows' own folder picker. `start`: the folder it opens in (if it exists). None = cancelled.
pub fn pick_folder(owner: Option<isize>, title: &str, start: Option<&str>) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
        SIGDN_FILESYSPATH,
    };
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let out = (|| -> windows::core::Result<String> {
            let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dlg.SetOptions(dlg.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
            dlg.SetTitle(&HSTRING::from(title))?;
            if let Some(s) = start.filter(|s| std::path::Path::new(s).is_dir()) {
                if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(s), None::<&IBindCtx>) {
                    let _ = dlg.SetFolder(&item);
                }
            }
            dlg.Show(HWND(owner.unwrap_or(0) as _))?;
            let name = dlg.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let s = name.to_string();
            CoTaskMemFree(Some(name.0 as _));
            Ok(s?)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        out.ok()
    }
}
