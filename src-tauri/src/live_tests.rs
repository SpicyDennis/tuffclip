//! End-to-end checks against the real FFmpeg and GPU: record in each capture mode, save a clip from
//! the RAM and the disk buffer, then export it every way the library can. They need FFmpeg 8+ on
//! PATH, an NVIDIA GPU and a desktop session, so they only run on request:
//! `cargo test -- --ignored --test-threads=1`
use crate::audio::{Source, Track};
use crate::config::{Codec, Config, Encoder};
use crate::export::{self, ExportCodec, ExportFormat, ExportRequest, Level};
use crate::recorder::{self, RecordSpec, Recorder};
use crate::{sound, win};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tuffclip-live-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg(dir: &Path) -> Config {
    Config {
        ffmpeg: "ffmpeg".into(),
        encoder: Encoder::Nvenc,
        codec: Codec::H264,
        fps: 60,
        clip_seconds: 6,
        clips_dir: dir.to_path_buf(),
        buffer_dir: Some(dir.join("buffer")),
        ..Config::default()
    }
}

fn primary() -> win::MonitorInfo {
    let m = win::list_monitors();
    m.iter().find(|m| m.primary).or(m.first()).cloned().expect("no monitor")
}

/// (width, height, audio streams) of a clip, read from ffmpeg's description of it.
fn probe(clip: &Path) -> (u32, u32, usize) {
    let out = crate::ff::cmd("ffmpeg").args(["-hide_banner", "-i"]).arg(clip).output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    let video = text.lines().find(|l| l.contains("Video:")).unwrap_or_else(|| panic!("no video in {}:\n{text}", clip.display()));
    let size = video
        .split([',', ' '])
        .find_map(|p| {
            let (w, h) = p.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap();
    (size.0, size.1, text.lines().filter(|l| l.contains("Audio:")).count())
}

/// Record `spec` for a few seconds and save a clip from it.
fn record(spec: RecordSpec, dir: &Path, name: &str, sound: Option<&[(usize, String)]>) -> PathBuf {
    let mut rec = Recorder::default();
    let ram = spec.ram;
    let buffer = spec.buffer_dir.clone();
    rec.start(spec, &dir.join("ffmpeg.log")).unwrap();
    std::thread::sleep(Duration::from_secs(9));
    let log = || std::fs::read_to_string(dir.join("ffmpeg.log")).unwrap_or_default();
    assert!(rec.is_alive(), "ffmpeg stopped:\n{}", log());
    let info = rec.buffer_info().unwrap();
    assert!(info.bytes > 100_000 && info.idle_secs < 3, "buffer isn't growing ({} bytes):\n{}", info.bytes, log());
    let out = dir.join("Raw").join("Test").join(format!("{name}.mp4"));
    if ram {
        recorder::save_ram("ffmpeg", &rec.ram().unwrap(), 6, &out, true, sound).unwrap();
    } else {
        recorder::save_buffer("ffmpeg", &buffer, 6, &out, true, sound).unwrap();
    }
    rec.stop();
    assert!(std::fs::metadata(&out).unwrap().len() > 10_000, "the clip is (nearly) empty");
    out
}

#[test]
#[ignore]
fn record_display_to_ram_with_three_sound_tracks() {
    let dir = scratch("ram");
    let mon = primary();
    let c = Config { buffer_in_ram: true, ..cfg(&dir) };
    let mut spec = RecordSpec::new(&c, &mon, None, None);
    spec.tracks = vec![
        Track { source: Source::Desktop, title: "Desktop".into() },
        Track { source: Source::App(std::process::id()), title: "Game".into() },
    ];
    let sel = [(0, "Mix".to_string()), (1, "Desktop".to_string()), (2, "Game".to_string())];
    let clip = record(spec, &dir, "ram", Some(&sel));
    let (w, h, audio) = probe(&clip);
    assert_eq!((w, h, audio), (mon.width, mon.height, 3));
    assert_eq!(sound::titles("ffmpeg", &clip), ["Mix", "Desktop", "Game"]);
    // the library's level editor decodes every track
    let tracks = sound::tracks("ffmpeg", &clip, &dir.join(".playback")).unwrap();
    assert_eq!(tracks.len(), 3);
    assert!(tracks.iter().all(|t| Path::new(&t.file).is_file() && t.peaks.len() >= 100));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn record_display_below_native_to_disk() {
    let dir = scratch("down");
    let mon = primary();
    let c = Config { height: 720, ..cfg(&dir) };
    let mut spec = RecordSpec::new(&c, &mon, None, None);
    spec.tracks = vec![Track { source: Source::Desktop, title: "Desktop".into() }];
    let clip = record(spec, &dir, "down", None);
    let (w, h, audio) = probe(&clip);
    assert_eq!((h, audio), (720, 1));
    assert_eq!(w, (mon.width * 720 / mon.height) / 2 * 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn record_cropped_window_region_at_native() {
    let dir = scratch("crop");
    let mon = primary();
    let crop = recorder::Crop { x: 100, y: 100, w: 1280, h: 720 };
    let spec = RecordSpec::new(&cfg(&dir), &mon, Some(crop), None);
    let clip = record(spec, &dir, "crop", None);
    assert_eq!(probe(&clip), (1280, 720, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn record_one_window_with_window_capture() {
    let dir = scratch("window");
    // A plain app window (Windows won't let shell windows like the taskbar be captured).
    let mut np = std::process::Command::new("notepad.exe").spawn().unwrap();
    let want: std::collections::HashSet<String> = ["notepad.exe".to_string()].into();
    let mut found = None;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        found = win::running_games(&want).into_iter().find(|w| w.pid == np.id());
        if found.is_some() {
            break;
        }
    }
    let hwnd = found.expect("notepad's window didn't show up").hwnd;
    let g = win::window_geometry(hwnd).unwrap();
    let (gw, gh) = (g.w / 2 * 2, g.h / 2 * 2);
    let c = Config { codec: Codec::Hevc, ..cfg(&dir) };
    let spec = RecordSpec::new(&c, &primary(), None, Some((hwnd, gw, gh)));
    let result = std::panic::catch_unwind(|| record(spec, &dir, "window", None));
    let _ = np.kill();
    let clip = result.unwrap();
    let (w, h, _) = probe(&clip);
    assert_eq!((w, h), (gw, gh));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn export_every_way() {
    let dir = scratch("export");
    let c = Config { buffer_in_ram: true, ..cfg(&dir) };
    let mut spec = RecordSpec::new(&c, &primary(), None, None);
    spec.tracks = vec![
        Track { source: Source::Desktop, title: "Desktop".into() },
        Track { source: Source::App(std::process::id()), title: "Game".into() },
    ];
    let sel = [(0, "Mix".to_string()), (1, "Desktop".to_string()), (2, "Game".to_string())];
    let clip = record(spec, &dir, "source", Some(&sel));
    let size = std::fs::metadata(&clip).unwrap().len() as f64;
    let src_kbps = size * 8.0 / 1000.0 / 6.0;
    let base = ExportRequest {
        path: clip.to_string_lossy().into_owned(),
        start: 1.0,
        end: 5.0,
        mode: "original".into(),
        format: ExportFormat::Mp4,
        codec: ExportCodec::Keep,
        target_mb: 0.0,
        target_kbps: 0,
        height: -1,
        precise: false,
        fps: 15,
        name: String::new(),
        src_kbps,
        low_impact: true,
        levels: None,
    };
    let run = |label: &str, r: ExportRequest| -> PathBuf {
        let last = std::sync::Mutex::new(0.0);
        let out = export::export(&c, &r, |p| *last.lock().unwrap() = p).unwrap_or_else(|e| panic!("{label}: {e:#}"));
        assert!(std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) > 1000, "{label}: empty output");
        assert_eq!(*last.lock().unwrap(), 1.0, "{label}: progress didn't reach 100%");
        out
    };
    let trim = run("trim", base.clone());
    assert_eq!(probe(&trim).2, 1, "one sound track: the mix");
    run("mkv", ExportRequest { format: ExportFormat::Mkv, ..base.clone() });
    run("levels", ExportRequest { levels: Some(vec![Level { index: 1, gain: 0.5 }, Level { index: 2, gain: 1.5 }]), ..base.clone() });
    let hevc = run("hevc", ExportRequest { codec: ExportCodec::Hevc, ..base.clone() });
    assert_eq!(crate::ff::probe_codec("ffmpeg", &hevc), "hevc");
    let target = (size / 1e6 * 4.0 / 6.0 * 0.4).max(0.5);
    let small = run("size", ExportRequest { mode: "size".into(), target_mb: target, ..base.clone() });
    let got = std::fs::metadata(&small).unwrap().len() as f64 / 1e6;
    assert!(got <= target * 1.08, "size mode: {got:.2} MB for a {target:.2} MB target");
    run("bitrate", ExportRequest { mode: "bitrate".into(), target_kbps: 2500, height: 720, ..base.clone() });
    run("exact", ExportRequest { mode: "size".into(), target_mb: target, precise: true, ..base.clone() });
    run("exact hevc", ExportRequest { mode: "size".into(), target_mb: target, precise: true, codec: ExportCodec::Hevc, ..base.clone() });
    run("webm", ExportRequest { format: ExportFormat::Webm, end: 3.0, height: 480, ..base.clone() });
    let gif = run("gif", ExportRequest { format: ExportFormat::Gif, end: 3.0, height: 240, ..base.clone() });
    assert_eq!(probe(&gif).2, 0);
    let png = run("png", ExportRequest { format: ExportFormat::Png, start: 2.5, ..base.clone() });
    assert!(png.parent().unwrap().ends_with("Screenshots"));
    // a name of your own, and a second export with it doesn't overwrite the first
    let named = run("named", ExportRequest { name: "my clip".into(), ..base.clone() });
    let named2 = run("named again", ExportRequest { name: "my clip".into(), ..base.clone() });
    assert_ne!(named, named2);
    // asking for more than the clip has is refused, not exported
    assert!(export::export(&c, &ExportRequest { mode: "size".into(), target_mb: size / 1e6 * 2.0, ..base.clone() }, |_| {}).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
