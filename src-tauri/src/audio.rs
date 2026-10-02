//! WASAPI loopback capture (what you hear) via cpal, streamed to ffmpeg as raw f32.
//!
//! WASAPI loopback delivers nothing while the system is silent, which would
//! stall ffmpeg's muxer. So a pacer thread writes exactly real-time audio every
//! 10 ms, filling gaps with silence. ffmpeg therefore always sees a steady,
//! gap-free stream whose clock matches the video.
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u16,
}

pub fn loopback_format() -> Option<AudioFormat> {
    let dev = cpal::default_host().default_output_device()?;
    let c = dev.default_output_config().ok()?;
    Some(AudioFormat { rate: c.sample_rate().0, channels: c.channels() })
}

type Queue = Arc<Mutex<VecDeque<f32>>>;

pub fn spawn_loopback<W: Write + Send + 'static>(mut sink: W, fmt: AudioFormat, stop: Arc<AtomicBool>) {
    let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));

    // Capture thread: owns the cpal stream (it is !Send).
    {
        let q = queue.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            if let Err(e) = capture(q, &stop) {
                eprintln!("[clipr] audio capture error: {e:#}");
            }
        });
    }

    // Pacer/writer thread.
    thread::spawn(move || {
        let ch = fmt.channels as usize;
        let chunk = (fmt.rate as usize / 100) * ch; // 10 ms of interleaved samples
        let max_q = chunk * 25; // keep at most 250 ms queued
        let mut bytes = Vec::with_capacity(chunk * 4);
        let start = Instant::now();
        let mut n: u64 = 0;
        while !stop.load(Relaxed) {
            n += 1;
            let due = start + Duration::from_millis(n * 10);
            let now = Instant::now();
            if due > now {
                thread::sleep(due - now);
            }
            bytes.clear();
            {
                let mut q = queue.lock();
                if q.len() > max_q {
                    let mut excess = q.len() - max_q;
                    excess -= excess % ch;
                    q.drain(..excess);
                }
                for _ in 0..chunk {
                    let s = q.pop_front().unwrap_or(0.0);
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
            }
            if sink.write_all(&bytes).is_err() {
                break; // ffmpeg exited
            }
        }
        stop.store(true, Relaxed);
    });
}

fn on_err(e: cpal::StreamError) {
    eprintln!("[clipr] audio stream error: {e}");
}

fn capture(q: Queue, stop: &AtomicBool) -> Result<()> {
    let dev = cpal::default_host()
        .default_output_device()
        .context("no audio output device")?;
    let cfg = dev.default_output_config()?;
    let scfg = cfg.config();
    // On WASAPI, an *input* stream on an *output* device is a loopback capture.
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => {
            let q = q.clone();
            dev.build_input_stream(&scfg, move |d: &[f32], _: &cpal::InputCallbackInfo| {
                q.lock().extend(d.iter().copied())
            }, on_err, None)?
        }
        SampleFormat::I16 => {
            let q = q.clone();
            dev.build_input_stream(&scfg, move |d: &[i16], _: &cpal::InputCallbackInfo| {
                q.lock().extend(d.iter().map(|&s| s as f32 / 32768.0))
            }, on_err, None)?
        }
        SampleFormat::U16 => {
            let q = q.clone();
            dev.build_input_stream(&scfg, move |d: &[u16], _: &cpal::InputCallbackInfo| {
                q.lock().extend(d.iter().map(|&s| (s as f32 - 32768.0) / 32768.0))
            }, on_err, None)?
        }
        other => bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    while !stop.load(Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}
