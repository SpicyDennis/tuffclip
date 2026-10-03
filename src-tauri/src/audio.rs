//! Sound capture: one WASAPI thread per track, all streamed to ffmpeg as raw f32 through one pipe.
//!
//! Every track is 48 kHz stereo. The pipe carries all tracks interleaved as one multichannel
//! stream (track 0 L/R, track 1 L/R, ...) and ffmpeg splits it back into stereo tracks (plus a
//! mix of them all) in its filter graph. One pipe keeps the tracks sample-locked to each other
//! and to the video, which separate pipes would not.
//!
//! Sources:
//!  * App(pid): only that program and its child processes (process loopback, Windows 10 2004+).
//!  * Discord: process loopback on Discord, attached whenever Discord is running.
//!  * Desktop: everything the default output device plays (endpoint loopback).
//!  * Mic: a microphone (the Windows default, or one picked in Settings).
//!
//! WASAPI delivers nothing while a source is silent, which would stall ffmpeg's muxer. So a
//! pacer thread writes exactly real-time audio every 10 ms, filling gaps with silence.
use crate::config::{AudioSource, Config};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{implement, Interface, GUID, HRESULT, HSTRING, IUnknown, PROPVARIANT};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// Sample rate of every track.
pub const RATE: u32 = 48_000;
/// Samples in one 10 ms pacer step, per channel.
const STEP: usize = RATE as usize / 100;
/// Quieter than this (about -66 dB) counts as silence.
const SILENT: f32 = 0.0005;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    App(u32),
    Discord,
    Desktop,
    Mic(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub source: Source,
    pub title: String,
}

fn track(source: Source, title: &str) -> Track {
    Track { source, title: title.into() }
}

/// The tracks to record. `game_pid` is the recorded game (None in desktop mode: then the main
/// track is everything you hear, because there is no single program to follow).
pub fn plan(cfg: &Config, game_pid: Option<u32>) -> Vec<Track> {
    if !cfg.audio {
        return Vec::new();
    }
    let mut t = Vec::new();
    match (cfg.audio_source, game_pid) {
        (AudioSource::Game, Some(pid)) => {
            t.push(track(Source::App(pid), "Game"));
            if cfg.discord_track {
                t.push(track(Source::Discord, "Discord"));
            }
        }
        _ => t.push(track(Source::Desktop, "Desktop")),
    }
    if cfg.mic {
        t.push(track(Source::Mic(cfg.mic_device.clone()), "Mic"));
    }
    t
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How a recording's sound is laid out in the buffer, and when each source last made a sound.
#[derive(Clone)]
pub struct Layout {
    titles: Vec<String>,
    /// Per source: ms since the epoch of the last 10 ms that wasn't silent (0 = never).
    heard: Arc<Vec<AtomicU64>>,
}

impl Layout {
    pub fn new(tracks: &[Track]) -> Self {
        Layout {
            titles: tracks.iter().map(|t| t.title.clone()).collect(),
            heard: Arc::new(tracks.iter().map(|_| AtomicU64::new(0)).collect()),
        }
    }

    /// Several sources are recorded: the buffer's first audio stream is their mix, followed by
    /// one stream per source.
    pub fn has_mix(&self) -> bool {
        self.titles.len() > 1
    }

    /// The buffer's audio streams worth saving for a clip of `secs` seconds that ends at
    /// `end_ms`, with their titles. A separate track that stayed silent the whole clip (Discord
    /// not in a call, say) is left out. If only one source was heard, the mix is that source
    /// alone, so it's saved once under that source's name.
    pub fn pick(&self, secs: u32, end_ms: u64) -> Vec<(usize, String)> {
        if !self.has_mix() {
            return self.titles.iter().cloned().enumerate().collect();
        }
        let since = end_ms.saturating_sub((secs as u64 + 6) * 1000);
        let heard: Vec<usize> = (0..self.titles.len()).filter(|&i| self.heard[i].load(Relaxed) >= since).collect();
        match heard.as_slice() {
            [] => vec![(0, self.titles[0].clone())],
            [one] => vec![(0, self.titles[*one].clone())],
            many => {
                let mut v = vec![(0, "Mix".to_string())];
                v.extend(many.iter().map(|&i| (i + 1, self.titles[i].clone())));
                v
            }
        }
    }
}

type Queue = Arc<Mutex<VecDeque<f32>>>;

/// Start capturing `tracks` and writing them, interleaved, to `sink` until `stop` is set
/// (or ffmpeg goes away).
pub fn spawn<W: Write + Send + 'static>(mut sink: W, tracks: &[Track], layout: &Layout, stop: Arc<AtomicBool>) {
    let queues: Vec<Queue> = tracks.iter().map(|_| Arc::new(Mutex::new(VecDeque::new()))).collect();
    for (t, q) in tracks.iter().zip(&queues) {
        let (src, q, stop) = (t.source.clone(), q.clone(), stop.clone());
        thread::spawn(move || run_source(src, q, &stop));
    }
    let heard = layout.heard.clone();

    thread::spawn(move || {
        let n = queues.len();
        let per = STEP * 2; // one step of one stereo track
        let max_q = per * 25; // keep at most 250 ms queued per track
        let mut bufs = vec![vec![0f32; per]; n];
        let mut bytes = Vec::with_capacity(per * n * 4);
        let mut due = Instant::now();
        while !stop.load(Relaxed) {
            due += Duration::from_millis(10);
            let now = Instant::now();
            if due > now {
                thread::sleep(due - now);
            } else if now - due > Duration::from_secs(2) {
                // Far behind (the PC was asleep, or this thread starved): carry on from now rather
                // than pouring seconds of catch-up silence into ffmpeg at once.
                due = now;
            }
            for (i, q) in queues.iter().enumerate() {
                let mut q = q.lock();
                if q.len() > max_q {
                    let mut excess = q.len() - max_q;
                    excess -= excess % 2;
                    q.drain(..excess);
                }
                let mut loud = false;
                for s in bufs[i].iter_mut() {
                    *s = q.pop_front().unwrap_or(0.0);
                    loud |= s.abs() > SILENT;
                }
                if loud {
                    heard[i].store(now_ms(), Relaxed);
                }
            }
            bytes.clear();
            for f in 0..STEP {
                for b in &bufs {
                    bytes.extend_from_slice(&b[f * 2].to_le_bytes());
                    bytes.extend_from_slice(&b[f * 2 + 1].to_le_bytes());
                }
            }
            if sink.write_all(&bytes).is_err() {
                break; // ffmpeg exited
            }
        }
        stop.store(true, Relaxed);
    });
}

/// Capture one source for as long as the recording runs. Whatever goes wrong (device unplugged,
/// Discord not running yet, default device changed), wait a moment and open it again.
fn run_source(src: Source, q: Queue, stop: &AtomicBool) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut last_err = String::new();
    while !stop.load(Relaxed) {
        let r = open(&src).and_then(|s| s.pump(&q, stop));
        // In 100 ms steps: reopen right away when the source asked for it (new default device,
        // Discord restarted), after 2 s when it failed, and look for Discord only every 10 s while
        // it isn't running (each look lists every process).
        let wait = match &r {
            Ok(()) => 2,
            Err(_) if src == Source::Discord => 100,
            Err(_) => 20,
        };
        if let Err(e) = r {
            let e = format!("{e:#}");
            if e != last_err {
                eprintln!("[tuffclip] audio {src:?}: {e}");
                last_err = e;
            }
        }
        for _ in 0..wait {
            if stop.load(Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

// ---------------------------------------------------------------- opening a source

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    conv: Conv,
    /// Reopen when Windows' default device for this direction changes (the device id it had).
    follow_default: Option<(EDataFlow, String)>,
    /// Reopen when this process exits (Discord restarting).
    watch_pid: Option<u32>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn open(src: &Source) -> Result<Stream> {
    unsafe {
        match src {
            Source::App(pid) => match process_client(*pid) {
                Ok(c) => Stream::new(c, true, true, None, None),
                // Older Windows can't capture a single program: fall back to everything.
                Err(_) => endpoint_stream(eRender, ""),
            },
            Source::Discord => {
                let pid = crate::win::discord_pid().context("Discord isn't running")?;
                Stream::new(process_client(pid)?, true, true, None, Some(pid))
            }
            Source::Desktop => endpoint_stream(eRender, ""),
            Source::Mic(id) => endpoint_stream(eCapture, id).or_else(|e| {
                // The chosen microphone is gone: use the default one until it's back.
                if id.is_empty() { Err(e) } else { endpoint_stream(eCapture, "") }
            }),
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { Ok(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?) }
}

unsafe fn device_id(dev: &IMMDevice) -> Result<String> {
    let p = dev.GetId()?;
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok(s)
}

fn default_id(flow: EDataFlow) -> Option<String> {
    unsafe {
        let dev = enumerator().ok()?.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
        device_id(&dev).ok()
    }
}

/// Loopback of an output device (`eRender`) or a microphone (`eCapture`); empty id = the default.
unsafe fn endpoint_stream(flow: EDataFlow, id: &str) -> Result<Stream> {
    let en = enumerator()?;
    let dev = if id.is_empty() { en.GetDefaultAudioEndpoint(flow, eConsole)? } else { en.GetDevice(&HSTRING::from(id))? };
    let follow = id.is_empty().then(|| (flow, device_id(&dev).unwrap_or_default()));
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
    Stream::new(client, flow == eRender, false, follow, None)
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(isize);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Option<&IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(HANDLE(self.0 as _)) }
    }
}

impl IAgileObject_Impl for Activated_Impl {}

/// PROPVARIANT holding a VT_BLOB, laid out by hand (x64), so nothing tries to free our blob.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

/// An audio client that hears only `pid` and the processes it started.
unsafe fn process_client(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let pv = BlobVariant {
        vt: 65, // VT_BLOB
        reserved: [0; 3],
        size: std::mem::size_of_val(&params) as u32,
        data: &params as *const _ as *const u8,
    };
    let event = CreateEventW(None, false, false, None)?;
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(event.0 as isize).into();
    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&pv as *const BlobVariant as *const PROPVARIANT),
        &handler,
    );
    let op = match op {
        Ok(op) => op,
        Err(e) => {
            let _ = CloseHandle(event);
            return Err(e.into());
        }
    };
    let waited = WaitForSingleObject(event, 5000);
    if waited != WAIT_OBJECT_0 {
        // The handler may still fire later and signal this event, so it is left open (one
        // handle) rather than closed and possibly reused by something else.
        bail!("Windows didn't open the program's audio in time");
    }
    let _ = CloseHandle(event);
    let mut hr = HRESULT(0);
    let mut unk: Option<IUnknown> = None;
    op.GetActivateResult(&mut hr, &mut unk)?;
    hr.ok()?;
    Ok(unk.context("no audio client")?.cast()?)
}

const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

fn wave_format(float: bool) -> WAVEFORMATEX {
    let bits: u16 = if float { 32 } else { 16 };
    WAVEFORMATEX {
        wFormatTag: if float { TAG_FLOAT } else { TAG_PCM },
        nChannels: 2,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 2 * bits as u32 / 8,
        nBlockAlign: 2 * bits / 8,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

impl Stream {
    unsafe fn new(
        client: IAudioClient,
        loopback: bool,
        process: bool,
        follow_default: Option<(EDataFlow, String)>,
        watch_pid: Option<u32>,
    ) -> Result<Stream> {
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let convert = flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let buffer = 1_000_000; // 100 ms, in 100 ns units
        // Ask Windows for 48 kHz stereo float directly; it converts from whatever the device uses.
        let mut conv = None;
        for float in [true, false] {
            let f = wave_format(float);
            if client.Initialize(AUDCLNT_SHAREMODE_SHARED, convert, buffer, 0, &f, None).is_ok() {
                conv = Some(Conv::new(float, if float { 32 } else { 16 }, 2, RATE));
                break;
            }
            if !process {
                break;
            }
        }
        // Otherwise take the device's own format and convert it ourselves.
        let conv = match conv {
            Some(c) => c,
            None if process => bail!("Windows won't capture this program's sound"),
            None => {
                let mix = client.GetMixFormat()?;
                let f = mix.read_unaligned();
                let float = match f.wFormatTag {
                    TAG_FLOAT => true,
                    TAG_EXTENSIBLE => (mix as *const u8).add(24).cast::<GUID>().read_unaligned() == SUBTYPE_FLOAT,
                    _ => false,
                };
                let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer, 0, mix, None);
                CoTaskMemFree(Some(mix as *const _));
                r?;
                Conv::new(float, f.wBitsPerSample, f.nChannels as usize, f.nSamplesPerSec)
            }
        };
        let event = CreateEventW(None, false, false, None)?;
        if let Err(e) = client.SetEventHandle(event) {
            let _ = CloseHandle(event);
            return Err(e.into());
        }
        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(e) => {
                let _ = CloseHandle(event);
                return Err(e.into());
            }
        };
        Ok(Stream { client, capture, event, conv, follow_default, watch_pid })
    }

    /// Move captured audio into `q` until `stop`, an error, or a reason to reopen.
    fn pump(mut self, q: &Queue, stop: &AtomicBool) -> Result<()> {
        unsafe { self.client.Start()? };
        let mut out: Vec<f32> = Vec::with_capacity(STEP * 4);
        let mut checked = Instant::now();
        while !stop.load(Relaxed) {
            unsafe {
                WaitForSingleObject(self.event, 100);
                loop {
                    if self.capture.GetNextPacketSize()? == 0 {
                        break;
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    self.conv.push(data, frames as usize, silent, &mut out);
                    self.capture.ReleaseBuffer(frames)?;
                }
            }
            if !out.is_empty() {
                q.lock().extend(out.drain(..));
            }
            if checked.elapsed() > Duration::from_secs(2) {
                checked = Instant::now();
                if let Some((flow, id)) = &self.follow_default {
                    if default_id(*flow).is_some_and(|d| &d != id) {
                        return Ok(());
                    }
                }
                if self.watch_pid.is_some_and(|p| !crate::win::process_alive(p)) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- format conversion

/// Turns whatever the device delivers into 48 kHz stereo f32.
struct Conv {
    float: bool,
    bits: u16,
    ch: usize,
    /// Source frames per output frame (1.0 = no resampling).
    step: f64,
    pos: f64,
    prev: [f32; 2],
    frame: Vec<[f32; 2]>,
}

impl Conv {
    fn new(float: bool, bits: u16, ch: usize, rate: u32) -> Self {
        Conv {
            float,
            bits,
            ch: ch.max(1),
            step: rate.max(1) as f64 / RATE as f64,
            pos: 0.0,
            prev: [0.0; 2],
            frame: Vec::new(),
        }
    }

    unsafe fn sample(&self, data: *const u8, i: usize) -> f32 {
        match (self.float, self.bits) {
            (true, 64) => data.cast::<f64>().add(i).read_unaligned() as f32,
            (true, _) => data.cast::<f32>().add(i).read_unaligned(),
            (false, 16) => data.cast::<i16>().add(i).read_unaligned() as f32 / 32768.0,
            (false, 24) => {
                let p = data.add(i * 3);
                let v = (p.read() as i32) << 8 | (p.add(1).read() as i32) << 16 | (p.add(2).read() as i32) << 24;
                v as f32 / 2_147_483_648.0
            }
            (false, 32) => data.cast::<i32>().add(i).read_unaligned() as f32 / 2_147_483_648.0,
            _ => 0.0,
        }
    }

    unsafe fn push(&mut self, data: *const u8, frames: usize, silent: bool, out: &mut Vec<f32>) {
        let ch = self.ch;
        self.frame.clear();
        for f in 0..frames {
            let lr = if silent || data.is_null() {
                [0.0, 0.0]
            } else {
                let s = |c: usize| self.sample(data, f * ch + c);
                match ch {
                    1 => [s(0), s(0)],
                    2 => [s(0), s(1)],
                    // 5.1 / 7.1: fold centre and surrounds into left/right.
                    _ => {
                        let c = s(2) * 0.707;
                        let (mut l, mut r) = (s(0) + c, s(1) + c);
                        if ch >= 6 {
                            l += s(4) * 0.707;
                            r += s(5) * 0.707;
                        }
                        [l, r]
                    }
                }
            };
            self.frame.push(lr);
        }
        if (self.step - 1.0).abs() < 1e-9 {
            for lr in &self.frame {
                out.extend_from_slice(lr);
            }
            return;
        }
        // Linear resampling, carrying the last frame and position over to the next packet.
        let n = self.frame.len();
        let at = |i: usize, prev: [f32; 2], fr: &[[f32; 2]]| if i == 0 { prev } else { fr[i - 1] };
        let mut pos = self.pos;
        while pos + 1.0 < (n + 1) as f64 {
            let i = pos as usize;
            let t = (pos - i as f64) as f32;
            let a = at(i, self.prev, &self.frame);
            let b = at(i + 1, self.prev, &self.frame);
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            pos += self.step;
        }
        self.pos = pos - n as f64;
        if let Some(last) = self.frame.last() {
            self.prev = *last;
        }
    }
}

// ---------------------------------------------------------------- microphones

#[derive(Clone, Debug, Serialize)]
pub struct MicInfo {
    pub id: String,
    pub name: String,
}

/// Microphones that are plugged in and enabled.
pub fn list_mics() -> Vec<MicInfo> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(en) = enumerator() else { return Vec::new() };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let n = col.GetCount().unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..n {
            let Ok(dev) = col.Item(i) else { continue };
            let Ok(id) = device_id(&dev) else { continue };
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                .map(|v| v.to_string())
                .unwrap_or_default();
            out.push(MicInfo { name: if name.is_empty() { "Microphone".into() } else { name }, id });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(titles: &[&str]) -> Layout {
        Layout::new(&titles.iter().map(|t| track(Source::Desktop, t)).collect::<Vec<_>>())
    }

    #[test]
    fn clip_keeps_only_tracks_that_were_heard() {
        let one = layout(&["Desktop"]);
        assert_eq!(one.pick(30, 100_000), vec![(0, "Desktop".to_string())]);

        let l = layout(&["Game", "Discord", "Mic"]);
        let end = 1_000_000;
        // nothing heard: just the (silent) mix
        assert_eq!(l.pick(30, end), vec![(0, "Game".to_string())]);
        // only Discord heard: the mix is Discord alone, named so
        l.heard[1].store(end - 5_000, Relaxed);
        assert_eq!(l.pick(30, end), vec![(0, "Discord".to_string())]);
        // game and Discord: mix + both parts; the mic, heard long before the clip, is left out
        l.heard[0].store(end - 1_000, Relaxed);
        l.heard[2].store(end - 120_000, Relaxed);
        assert_eq!(l.pick(30, end), vec![(0, "Mix".into()), (1, "Game".into()), (2, "Discord".into())]);
    }

    #[test]
    fn plan_picks_sources() {
        let cfg = Config { mic: true, discord_track: true, ..Config::default() };
        let t = plan(&cfg, Some(42));
        assert_eq!(t.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Game", "Discord", "Mic"]);
        assert_eq!(t[0].source, Source::App(42));
        // no game (desktop mode): everything you hear, no Discord track
        let t = plan(&cfg, None);
        assert_eq!(t.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Desktop", "Mic"]);
        assert!(plan(&Config { audio: false, ..cfg }, Some(1)).is_empty());
    }

    #[test]
    fn conversion_resamples_and_folds_channels() {
        // 44.1 kHz mono i16 -> 48 kHz stereo f32: about 48/44.1 as many frames, both sides equal
        let mut c = Conv::new(false, 16, 1, 44_100);
        let src: Vec<i16> = (0..4410).map(|i| ((i % 100) * 300) as i16).collect();
        let mut out = Vec::new();
        for chunk in src.chunks(441) {
            unsafe { c.push(chunk.as_ptr() as *const u8, chunk.len(), false, &mut out) };
        }
        let frames = out.len() / 2;
        assert!((4798..=4801).contains(&frames), "{frames}");
        assert!(out.chunks(2).all(|lr| lr[0] == lr[1]));
        // 48 kHz 5.1 f32: centre goes to both sides, no resampling
        let mut c = Conv::new(true, 32, 6, 48_000);
        let frame = [0.1f32, 0.2, 0.5, 0.0, 0.0, 0.0];
        let mut out = Vec::new();
        unsafe { c.push(frame.as_ptr() as *const u8, 1, false, &mut out) };
        assert!((out[0] - (0.1 + 0.5 * 0.707)).abs() < 1e-6 && (out[1] - (0.2 + 0.5 * 0.707)).abs() < 1e-6);
        // silent packets become zeros
        let mut out = Vec::new();
        unsafe { c.push(std::ptr::null(), 10, true, &mut out) };
        assert_eq!(out, vec![0.0; 20]);
    }
}
