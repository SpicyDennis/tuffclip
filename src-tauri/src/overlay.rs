//! The "recording" indicator: one small, dim, click-through dot in a corner of the recorded
//! window. It lives on its own tiny thread that only moves a 12 px window, so it costs nothing.
use crate::config::Indicator;
use crate::win;
use parking_lot::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateEllipticRgn, CreateSolidBrush, InvalidateRect, SetWindowRgn};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW, SetClassLongPtrW,
    SetLayeredWindowAttributes, GCLP_HBRBACKGROUND,
    SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

const DOT: i32 = 12;
const MARGIN: i32 = 14;
/// Size of the dot while it flashes to confirm a save, and how long the flash lasts.
const BIG: i32 = 28;
const FLASH: Duration = Duration::from_millis(1400);

static FLASH_AT: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Make the dot flash (green = clip saved, red = it failed). Does nothing if the dot isn't showing.
pub fn flash(ok: bool) {
    if STARTED.get().is_some() {
        *FLASH_AT.lock() = Some((Instant::now(), ok));
    }
}

/// What the dot should sit on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    /// A game window: the dot follows it, and only shows while that window is in front.
    pub hwnd: Option<isize>,
    /// Used when there is no window (desktop mode): the monitor's rectangle.
    pub rect: (i32, i32, i32, i32),
    pub pos: Indicator,
}

static TARGET: Mutex<Option<Target>> = Mutex::new(None);
static STARTED: OnceLock<()> = OnceLock::new();

/// Set (or clear, with None) where the dot goes. Starts the overlay thread on first use.
pub fn set(t: Option<Target>) {
    if t.is_none() && STARTED.get().is_none() {
        return;
    }
    *TARGET.lock() = t;
    STARTED.get_or_init(|| {
        std::thread::spawn(run);
    });
}

fn spot(t: &Target) -> Option<(i32, i32)> {
    let (x, y, w, h) = match t.hwnd {
        Some(h) => {
            let g = win::window_geometry(h)?;
            if g.minimized || win::foreground().map(|f| f.hwnd) != Some(h) {
                return None;
            }
            (g.x, g.y, g.w as i32, g.h as i32)
        }
        None => t.rect,
    };
    let left = x + MARGIN;
    let right = x + w - MARGIN - DOT;
    let top = y + MARGIN;
    let bottom = y + h - MARGIN - DOT;
    Some(match t.pos {
        Indicator::Off => return None,
        Indicator::TopLeft => (left, top),
        Indicator::TopRight => (right, top),
        Indicator::BottomLeft => (left, bottom),
        Indicator::BottomRight => (right, bottom),
    })
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

fn run() {
    unsafe {
        let Ok(inst) = GetModuleHandleW(None) else { return };
        let class = w!("TUFFClipDot");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: inst.into(),
            lpszClassName: class,
            // 0x00BBGGRR: the danger red, kept dim by the window's own transparency
            hbrBackground: CreateSolidBrush(COLORREF(0x005c_67e5)),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            DOT,
            DOT,
            None,
            None,
            inst,
            None,
        ) else {
            return;
        };
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 110, LWA_ALPHA);
        SetWindowRgn(hwnd, CreateEllipticRgn(0, 0, DOT + 1, DOT + 1), true);

        let red = CreateSolidBrush(COLORREF(0x005c_67e5));
        let green = CreateSolidBrush(COLORREF(0x009a_c56c));
        let mut shown: Option<(i32, i32)> = None;
        // 0 = normal, 1 = flashing green, 2 = flashing red
        let mut look = 0;
        let mut shown_size = DOT;
        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let target = *TARGET.lock();
            let at = target.as_ref().and_then(spot);
            let flash = {
                let mut f = FLASH_AT.lock();
                match *f {
                    Some((t, ok)) if t.elapsed() < FLASH => Some((t.elapsed(), ok)),
                    Some(_) => {
                        *f = None;
                        None
                    }
                    None => None,
                }
            };
            // Pulse: big for the first and third 350 ms, normal in between.
            let big = flash.is_some_and(|(e, _)| (e.as_millis() / 350) % 2 == 0);
            let want = match flash {
                Some((_, true)) => 1,
                Some((_, false)) => 2,
                None => 0,
            };
            if want != look {
                let brush = if want == 1 { green } else { red };
                SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush.0 as isize);
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), if want == 0 { 110 } else { 255 }, LWA_ALPHA);
                let _ = InvalidateRect(hwnd, None, true);
                look = want;
                shown = None;
            }
            let size = if big { BIG } else { DOT };
            let at = at.map(|(x, y)| (x - (size - DOT) / 2, y - (size - DOT) / 2, size));
            let key = at.map(|(x, y, _)| (x, y));
            if key != shown || big != (shown_size != DOT) {
                match at {
                    Some((x, y, sz)) => {
                        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, sz, sz, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                        SetWindowRgn(hwnd, CreateEllipticRgn(0, 0, sz + 1, sz + 1), true);
                        shown_size = sz;
                    }
                    None => {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                }
                shown = key;
            }
            let ms = if flash.is_some() { 50 } else if target.is_some() { 250 } else { 1000 };
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}
