//! Live preview of what is being recorded. A second, tiny ffmpeg runs only while the Preview
//! tab is open: it captures the same window or region as the recorder at a few frames a second,
//! and sends JPEG frames to the UI. Leaving the tab stops it, so it costs nothing otherwise.
use crate::recorder::RecordSpec;
use crate::{ff, win};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter};

const FPS: u32 = 5;
const WIDTH: u32 = 960;

static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn args(s: &RecordSpec) -> Vec<String> {
    let src = if let Some(hwnd) = s.window {
        format!(
            "gfxcapture=hwnd={hwnd}:capture_cursor={}:display_border=0:max_framerate={FPS}:width=-2:height=-2:resize_mode=scale_aspect,fps={FPS}",
            s.draw_mouse as u8
        )
    } else {
        let mut v = format!("ddagrab=output_idx={}:framerate={FPS}:draw_mouse={}", s.output, s.draw_mouse as u8);
        if let Some(c) = s.crop {
            v += &format!(":video_size={}x{}:offset_x={}:offset_y={}", c.w, c.h, c.x, c.y);
        }
        v
    };
    let vf = format!("{src},hwdownload,format=bgra,scale={WIDTH}:-2:flags=fast_bilinear,format=yuvj420p[v]");
    [
        "-hide_banner", "-loglevel", "error", "-nostdin",
        "-init_hw_device", &format!("d3d11va=d3d:{}", s.adapter), "-filter_hw_device", "d3d",
        "-filter_complex", &vf, "-map", "[v]",
        "-c:v", "mjpeg", "-q:v", "7", "-f", "image2pipe", "pipe:1",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect()
}

pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(mut c) = CHILD.lock().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Start (or restart) the preview of whatever `spec` records.
pub fn start(app: &AppHandle, spec: &RecordSpec) -> Result<()> {
    stop();
    let mut child = ff::cmd_low(&spec.ffmpeg)
        .args(args(spec))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Couldn't start ffmpeg (\"{}\"). Is it installed?", spec.ffmpeg))?;
    win::tie_to_app(&child);
    let mut out = child.stdout.take().expect("piped stdout");
    *CHILD.lock() = Some(child);
    let gen = GENERATION.load(Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            let n = match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if GENERATION.load(Ordering::SeqCst) != gen {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // Whole frames end with FF D9. Only the newest one is worth sending.
            let Some(end) = buf.windows(2).rposition(|w| w == [0xff, 0xd9]) else { continue };
            let done: Vec<u8> = buf.drain(..end + 2).collect();
            if let Some(start) = done.windows(2).rposition(|w| w == [0xff, 0xd8]) {
                let _ = app.emit("preview-frame", b64(&done[start..]));
            }
            if buf.len() > 8 << 20 {
                buf.clear();
            }
        }
        if GENERATION.load(Ordering::SeqCst) == gen {
            let _ = app.emit("preview-ended", ());
        }
    });
    Ok(())
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(super::b64(b""), "");
        assert_eq!(super::b64(b"f"), "Zg==");
        assert_eq!(super::b64(b"fo"), "Zm8=");
        assert_eq!(super::b64(b"foobar"), "Zm9vYmFy");
        assert_eq!(super::b64(&[0xff, 0xd8, 0xff]), "/9j/");
    }
}
