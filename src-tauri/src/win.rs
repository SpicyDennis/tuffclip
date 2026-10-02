//! Thin wrappers around the Win32 APIs Clipr needs.
use serde::Serialize;
use std::collections::HashSet;
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
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, MB_ICONHAND, MB_OK,
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
            let len = GetWindowTextLengthW(hwnd);
            if len > 0 {
                let mut buf = vec![0u16; len as usize + 1];
                let n = GetWindowTextW(hwnd, &mut buf);
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
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
        "systemsettings.exe", "msedgewebview2.exe", "clipr.exe", "searchhost.exe",
        "shellexperiencehost.exe", "startmenuexperiencehost.exe",
    ];
    let me = std::process::id();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (pid, title) in raw {
        if pid == me {
            continue;
        }
        let Some(exe) = exe_name(pid) else { continue };
        let key = exe.to_lowercase();
        if SKIP.contains(&key.as_str()) || !seen.insert(key) {
            continue;
        }
        out.push(AppWindow { exe, title });
    }
    out.sort_by(|a, b| a.exe.to_lowercase().cmp(&b.exe.to_lowercase()));
    out
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

/// Put a child process in a job object that is killed when Clipr exits —
/// even if Clipr crashes — so ffmpeg never keeps running orphaned.
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
