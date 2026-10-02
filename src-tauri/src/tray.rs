//! The tray icon: the app icon with a coloured dot (red = recording, amber = a closed game's
//! buffer is still saveable) and a tooltip that says what is being recorded.
use std::sync::OnceLock;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::AppHandle;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Idle,
    Recording,
    Held,
}

static ICONS: OnceLock<[Image<'static>; 3]> = OnceLock::new();

/// Copy of `base` with a dot in the bottom-right corner (dark rim so it reads on any taskbar).
fn with_dot(base: &Image, rgb: [u8; 3]) -> Image<'static> {
    let (w, h) = (base.width(), base.height());
    let mut px = base.rgba().to_vec();
    let (cx, cy) = (w as f32 * 0.72, h as f32 * 0.72);
    let r = w as f32 * 0.21;
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * w + x) * 4) as usize;
            if d <= r {
                px[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            } else if d <= r * 1.35 {
                px[i..i + 4].copy_from_slice(&[0x1b, 0x21, 0x29, 255]);
            }
        }
    }
    Image::new_owned(px, w, h)
}

pub fn build(app: &tauri::App) -> tauri::Result<()> {
    let base = app.default_window_icon().unwrap();
    let icons = ICONS.get_or_init(|| {
        [
            Image::new_owned(base.rgba().to_vec(), base.width(), base.height()),
            with_dot(base, [0xe5, 0x67, 0x5c]), // danger red
            with_dot(base, [0xf2, 0xb3, 0x3d]), // tally amber
        ]
    });

    let open = MenuItem::with_id(app, "open", "Open TUFFClip", true, None::<&str>)?;
    let save = MenuItem::with_id(app, "save", "Save clip now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit TUFFClip", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &save, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(icons[0].clone())
        .tooltip("TUFFClip")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "open" => crate::show_main(app),
            "save" => crate::save_in_background(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                crate::show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn update(app: &AppHandle, kind: Kind, tip: &str) {
    let Some(tray) = app.tray_by_id("main") else { return };
    if let Some(icons) = ICONS.get() {
        let _ = tray.set_icon(Some(icons[kind as usize].clone()));
    }
    let _ = tray.set_tooltip(Some(tip));
}
