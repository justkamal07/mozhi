//! End-to-end self-test: `mozhi --selftest speech.wav [--selftest-out result.json] [--selftest-exit]`
//!
//! Runs one real dictation through the whole app — engine, live phrases, overlay, paste into the
//! frontmost app, History — with the WAV standing in for the microphone (fed in real time).
//! Writes what happened as JSON so release checks can assert on it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::controller::Controller;
use crate::types::Phase;

pub struct Args {
    pub wav: PathBuf,
    pub out: Option<PathBuf>,
    pub exit: bool,
}

pub fn parse_args() -> Option<Args> {
    let args: Vec<String> = std::env::args().collect();
    let at = args.iter().position(|a| a == "--selftest")?;
    let wav = PathBuf::from(args.get(at + 1)?);
    let out = args.iter().position(|a| a == "--selftest-out").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    Some(Args { wav, out, exit: args.iter().any(|a| a == "--selftest-exit") })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    ok: bool,
    phase: Phase,
    text: Option<String>,
    inserted: Option<bool>,
    app_name: Option<String>,
    audio_ms: u64,
    total_ms: u64,
    error: Option<String>,
}

pub fn spawn(ctrl: Arc<Controller>, args: Args) {
    thread::spawn(move || {
        let report = run(&ctrl, &args.wav);
        let json = serde_json::to_string_pretty(&report).unwrap_or_default();
        log::info!("selftest: {json}");
        if let Some(out) = &args.out {
            let _ = std::fs::write(out, &json);
        }
        if args.exit {
            thread::sleep(Duration::from_millis(400));
            ctrl.app_handle().exit(if report.ok { 0 } else { 1 });
        }
    });
}

fn run(ctrl: &Arc<Controller>, wav: &Path) -> Report {
    let fail = |e: String| Report { ok: false, phase: ctrl.phase(), text: None, inserted: None, app_name: None, audio_ms: 0, total_ms: 0, error: Some(e) };
    let (samples, rate) = match read_wav(wav) {
        Ok(v) => v,
        Err(e) => return fail(format!("read {}: {e}", wav.display())),
    };
    let audio_ms = samples.len() as u64 * 1000 / rate as u64;
    // Let the model preload settle so the run measures dictation, not first load.
    thread::sleep(Duration::from_millis(2500));
    let before = ctrl.history.list().first().map(|h| h.id.clone());
    let t = Instant::now();
    ctrl.start_with_samples(samples, rate);
    thread::sleep(Duration::from_millis(audio_ms + 900));
    ctrl.commit();

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && !matches!(ctrl.phase(), Phase::Done | Phase::Error | Phase::Editing | Phase::Idle) {
        thread::sleep(Duration::from_millis(50));
    }
    let phase = ctrl.phase();
    let total_ms = t.elapsed().as_millis() as u64;
    let item = ctrl.history.list().into_iter().next().filter(|h| Some(&h.id) != before.as_ref());
    Report {
        ok: phase == Phase::Done && item.as_ref().map(|i| !i.text.is_empty()).unwrap_or(false),
        phase,
        text: item.as_ref().map(|i| i.text.clone()),
        inserted: item.as_ref().map(|i| i.inserted),
        app_name: item.as_ref().and_then(|i| i.app_name.clone()),
        audio_ms,
        total_ms,
        error: None,
    }
}

/// 16-bit PCM or 32-bit float WAV → mono f32 at its own sample rate.
pub fn read_wav(path: &Path) -> anyhow::Result<(Vec<f32>, u32)> {
    let b = std::fs::read(path)?;
    anyhow::ensure!(b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE", "not a WAV file");
    let (mut fmt, mut channels, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into()?) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]).max(1);
            rate = u32::from_le_bytes(body[4..8].try_into()?);
            bits = u16::from_le_bytes([body[14], body[15]]);
            if fmt == 0xFFFE && body.len() >= 26 {
                fmt = u16::from_le_bytes([body[24], body[25]]); // WAVE_FORMAT_EXTENSIBLE sub-format
            }
        } else if id == b"data" {
            let frames: Vec<f32> = match (fmt, bits) {
                (1, 16) => body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
                (3, 32) => body.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
                _ => anyhow::bail!("unsupported WAV encoding (format {fmt}, {bits} bit); use 16-bit PCM or 32-bit float"),
            };
            let ch = channels as usize;
            let mono = frames.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
            return Ok((mono, rate));
        }
        i += 8 + len + (len & 1);
    }
    anyhow::bail!("WAV has no data chunk")
}
