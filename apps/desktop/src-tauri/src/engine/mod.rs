//! Speech engine — microphone capture, whisper.cpp inference, model management,
//! Tamil → Tanglish transliteration, voice emoji and optional local-LLM polish.
//!
//! CONTRACT: the controller (src/controller.rs) depends only on the public items in
//! this file and in `models`, `translit`, `polish`. Keep these signatures stable;
//! add private submodules freely.
//!
//!   start()   opens the mic (≈10–40 ms) and, if needed, loads the model on a background
//!             thread; audio is buffered meanwhile.
//!   preview   with live preview on, a worker every ~700 ms either closes a phrase — ≥1 s of
//!             speech followed by ≥650 ms of silence, or 25 s without a pause — transcribing it
//!             at final quality and emitting `segment` (never revised), or re-transcribes the
//!             open tail cheaply and emits it as `partial`.
//!   finish()  stops the mic, lets an in-flight segment land, then transcribes only the audio
//!             after the last segment.
//!
//! All inference on a model is serialised by the model's GPU lock (see `whisper::Model`).

mod capture;
mod clean;
mod dsp;
mod emoji;
pub mod models;
pub mod polish;
mod resample;
pub mod translit;
mod vad;
mod whisper;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use parking_lot::{Condvar, Mutex, RwLock};

use crate::types::{OutputMode, TranscriptResult};
use capture::Recorder;
use dsp::SAMPLE_RATE;
use whisper::{Decode, Lang, Model, Quality};

/// Callbacks from the audio / inference threads. Implementations must be cheap
/// and never block (they forward to Tauri events).
pub trait EngineSink: Send + Sync + 'static {
    /// Normalised input level 0.0–1.0 (perceptual, not raw RMS), ~30×/s while recording.
    fn level(&self, level: f32);
    /// Rolling transcript of the open phrase only: everything said since the last `segment`.
    /// Replaces the previous partial; `""` right after a segment.
    fn partial(&self, text: &str);
    /// A closed phrase at final quality — cleaned, mode-converted, emoji applied. Never revised.
    fn segment(&self, _text: &str) {}
}

#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// Input device name from [`Engine::input_devices`]; None = system default.
    pub device: Option<String>,
    /// Model id from [`models::catalog`]. Empty = [`models::default_model_id`].
    pub model_id: String,
    pub mode: OutputMode,
    /// Emit `partial` and `segment` updates while recording.
    pub live_preview: bool,
    /// "heart emoji" → ❤️, "இதயம் எமோஜி" → ❤️.
    pub voice_emoji: bool,
}

const PREVIEW_EVERY: Duration = Duration::from_millis(700);
const MIN_SPEECH_MS: u64 = 300;
const MODEL_WAIT: Duration = Duration::from_secs(180);
const LID_AFTER: usize = SAMPLE_RATE * 3 / 2;
const LID_AGAIN: usize = SAMPLE_RATE * 8;
/// A phrase closes after at least this much speech…
const SEGMENT_MIN_VOICED: usize = SAMPLE_RATE;
/// …followed by at least this much silence.
const SEGMENT_PAUSE: usize = SAMPLE_RATE * 65 / 100;
/// Without a pause, a phrase is force-closed at its quietest point once it gets this long.
const SEGMENT_MAX: usize = SAMPLE_RATE * 25;

/// Tamil or English, decided by language ID while the user speaks (Auto mode only).
type Lid = Arc<Mutex<Option<&'static str>>>;

/// Set while language ID runs. Its full-window encode holds the GPU lock for a while, so
/// the preview skips ticks instead of queueing behind it.
type Busy = Arc<AtomicBool>;

/// A model that may still be loading.
type Slot = Arc<(Mutex<Option<Result<Arc<Model>, String>>>, Condvar)>;

type Audio = Arc<RwLock<Vec<f32>>>;

fn wait_slot(slot: &Slot, timeout: Duration) -> anyhow::Result<Arc<Model>> {
    let (lock, cv) = &**slot;
    let mut guard = lock.lock();
    let deadline = Instant::now() + timeout;
    while guard.is_none() {
        if cv.wait_until(&mut guard, deadline).timed_out() {
            return Err(anyhow!("speech model is taking too long to load"));
        }
    }
    guard.as_ref().unwrap().clone().map_err(|e| anyhow!(e))
}

fn peek_slot(slot: &Slot) -> Option<Arc<Model>> {
    slot.0.lock().as_ref().and_then(|r| r.as_ref().ok().cloned())
}

/// What the phrase segmenter has committed so far.
#[derive(Default)]
struct Committed {
    /// Samples before this index are covered by emitted segments.
    upto: AtomicUsize,
    /// Raw Whisper text of each emitted segment, in order.
    raw: Mutex<Vec<String>>,
}

struct Preview {
    /// Leave the loop at the next opportunity.
    stop: Arc<AtomicBool>,
    /// Abort an in-flight preview pass (never a segment pass).
    abort: Arc<AtomicBool>,
    /// Session cancelled: drop the result of an in-flight segment pass.
    cancelled: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Preview {
    /// Stops the loop, aborting a preview pass but letting a segment pass finish and emit.
    fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.abort.store(true, Ordering::SeqCst);
        self.handle.thread().unpark();
        let _ = self.handle.join();
    }
}

struct Session {
    recorder: Recorder,
    opts: RecordOptions,
    slot: Slot,
    preview: Option<Preview>,
    committed: Arc<Committed>,
    lid: Lid,
    lid_stop: Option<Arc<AtomicBool>>,
}

/// The audio of the last finished (or failed) dictation, for `retry_last`.
struct LastAudio {
    audio: Vec<f32>,
    model_id: String,
}

struct Inner {
    models_dir: PathBuf,
    /// The loaded model (at most one in memory).
    model: Mutex<Option<Arc<Model>>>,
    /// Serialises model loads.
    loading: Mutex<()>,
    session: Mutex<Option<Session>>,
    last: Mutex<Option<LastAudio>>,
}

/// Cheap to clone; all clones share one recorder and one loaded model.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(models_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                models_dir,
                model: Mutex::new(None),
                loading: Mutex::new(()),
                session: Mutex::new(None),
                last: Mutex::new(None),
            }),
        }
    }

    pub fn models_dir(&self) -> PathBuf {
        self.inner.models_dir.clone()
    }

    /// Names of available input devices, default first.
    pub fn input_devices() -> Vec<String> {
        capture::input_devices()
    }

    /// Opens the mic and starts buffering 16 kHz mono audio. Must return within ~50 ms
    /// (the model may still be loading — buffer audio meanwhile). Errors if the mic is
    /// unavailable or the model file is missing.
    pub fn start(&self, opts: RecordOptions, sink: Arc<dyn EngineSink>) -> anyhow::Result<()> {
        let device = opts.device.clone();
        self.start_with(opts, sink.clone(), move || Recorder::start(device.as_deref(), sink))
    }

    /// Same as [`Engine::start`], but the audio comes from `samples` (mono, any rate), fed in
    /// real time in 20 ms chunks and followed by silence until `finish()` / `cancel()`.
    /// For end-to-end self-tests.
    #[allow(dead_code)]
    pub fn start_with_samples(&self, opts: RecordOptions, sink: Arc<dyn EngineSink>, samples: Vec<f32>, sample_rate: u32) -> anyhow::Result<()> {
        self.start_with(opts, sink.clone(), move || Recorder::from_samples(samples, sample_rate, sink))
    }

    fn start_with(&self, opts: RecordOptions, sink: Arc<dyn EngineSink>, open: impl FnOnce() -> anyhow::Result<Recorder>) -> anyhow::Result<()> {
        self.cancel_now();
        *self.inner.last.lock() = None;
        let model_id = if opts.model_id.is_empty() { models::default_model_id().to_string() } else { opts.model_id.clone() };
        if models::resolve(&self.inner.models_dir, &model_id).is_none() {
            return Err(anyhow!("speech model {model_id} is not downloaded"));
        }
        let opts = RecordOptions { model_id: model_id.clone(), ..opts };

        let t = Instant::now();
        let recorder = open()?;
        log::debug!("audio source open in {:?}", t.elapsed());

        let slot: Slot = Arc::new((Mutex::new(None), Condvar::new()));
        match self.loaded(&model_id) {
            Some(m) => *slot.0.lock() = Some(Ok(m)),
            None => {
                let me = self.clone();
                let s = slot.clone();
                thread::Builder::new()
                    .name("mozhi-load".into())
                    .spawn(move || {
                        let r = me.load(&model_id).map_err(|e| format!("{e:#}"));
                        *s.0.lock() = Some(r);
                        s.1.notify_all();
                    })
                    .map_err(|e| anyhow!("spawn model loader: {e}"))?;
            }
        }

        let lid: Lid = Arc::new(Mutex::new(None));
        let busy: Busy = Arc::new(AtomicBool::new(false));
        let committed = Arc::new(Committed::default());
        let lid_stop = (opts.mode == OutputMode::Auto).then(|| spawn_lid(recorder.shared(), slot.clone(), lid.clone(), busy.clone()));
        let preview = if opts.live_preview {
            let ctx = PreviewCtx {
                models_dir: self.inner.models_dir.clone(),
                audio: recorder.shared(),
                slot: slot.clone(),
                mode: opts.mode,
                voice_emoji: opts.voice_emoji,
                lid: lid.clone(),
                busy,
                committed: committed.clone(),
                sink,
            };
            Some(spawn_preview(ctx))
        } else {
            None
        };
        *self.inner.session.lock() = Some(Session { recorder, opts, slot, preview, committed, lid, lid_stop });
        Ok(())
    }

    /// Stops capture and transcribes what the live segments have not covered yet.
    /// `text` is that remaining tail only (may be empty); `raw` covers the whole session.
    /// Blocking — call from a worker thread.
    pub fn finish(&self) -> anyhow::Result<TranscriptResult> {
        let session = self.inner.session.lock().take().ok_or_else(|| anyhow!("not recording"))?;
        if let Some(stop) = &session.lid_stop {
            stop.store(true, Ordering::SeqCst);
        }
        // Freeze the audio first so nothing the user says after stopping is included…
        let audio = session.recorder.stop();
        // …then let an in-flight segment pass land (its text is emitted before we return).
        if let Some(p) = session.preview {
            p.shutdown();
        }
        let audio = audio?;
        *self.inner.last.lock() = Some(LastAudio { audio: audio.clone(), model_id: session.opts.model_id.clone() });
        #[cfg(debug_assertions)]
        if std::env::var_os("MOZHI_FAULT_FINISH").is_some() {
            // Developer aid for testing the error → retry path.
            return Err(anyhow!("injected failure (MOZHI_FAULT_FINISH)"));
        }

        if dsp::is_digital_silence(&audio) && audio.len() > SAMPLE_RATE * 4 / 10 {
            return Err(anyhow!(
                "microphone permission denied: Mozhi received only silence. Allow it in System Settings → Privacy & Security → Microphone"
            ));
        }
        let model = wait_slot(&session.slot, MODEL_WAIT)?;
        let mut lid = *session.lid.lock();
        if session.opts.mode == OutputMode::Auto && lid.is_none() && audio.len() >= SAMPLE_RATE / 2 {
            // Too short for the background pass to have finished: decide now.
            lid = model.identify(&audio).ok().map(|(l, _)| l);
        }
        let lang = lang_for(session.opts.mode, lid);
        let upto = session.committed.upto.load(Ordering::SeqCst).min(audio.len());
        log::debug!("final pass: mode {:?}, language id {lid:?} → {lang:?}, tail from {:.1}s", session.opts.mode, upto as f32 / SAMPLE_RATE as f32);

        let audio_ms = (audio.len() * 1000 / SAMPLE_RATE) as u64;
        let mut tail = run_final(&self.inner.models_dir, &audio[upto..], session.opts.mode, session.opts.voice_emoji, lang, Quality::Final, || Ok(model))?;
        let mut raw = session.committed.raw.lock().clone();
        if !tail.raw.is_empty() {
            raw.push(tail.raw.clone());
        }
        tail.raw = raw.join(" ");
        tail.audio_ms = audio_ms;
        tail.language = match lang {
            Lang::English => "en".into(),
            Lang::Tamil => "ta".into(),
            Lang::Auto => whisper::script_language(&tail.raw).into(),
        };
        Ok(tail)
    }

    /// Re-transcribes the whole audio of the last finished or failed dictation from scratch.
    /// Kept until the next `start()`; `cancel()` discards it.
    #[allow(dead_code)]
    pub fn retry_last(&self, mode: OutputMode, voice_emoji: bool) -> anyhow::Result<TranscriptResult> {
        let (audio, model_id) = {
            let last = self.inner.last.lock();
            let last = last.as_ref().ok_or_else(|| anyhow!("nothing to retry"))?;
            (last.audio.clone(), last.model_id.clone())
        };
        let model = self.load(&model_id)?;
        let lid = if mode == OutputMode::Auto && audio.len() >= SAMPLE_RATE / 2 { model.identify(&audio).ok().map(|(l, _)| l) } else { None };
        run_final(&self.inner.models_dir, &audio, mode, voice_emoji, lang_for(mode, lid), Quality::Final, || Ok(model))
    }

    /// Transcribes recorded audio (any sample rate, mono) exactly like a dictation would —
    /// trim, normalise, final pass, clean-up, mode conversion. Blocking. Used by
    /// `examples/transcribe.rs`; `preview` runs the cheaper live-preview pass instead.
    #[allow(dead_code)]
    pub fn transcribe_samples(&self, samples: &[f32], sample_rate: u32, mode: OutputMode, model_id: &str, preview: bool) -> anyhow::Result<TranscriptResult> {
        let mut audio = Vec::with_capacity(samples.len() * SAMPLE_RATE / sample_rate.max(1) as usize + 64);
        let mut r = resample::Resampler::new(sample_rate, SAMPLE_RATE as u32);
        r.process(samples, &mut audio);
        r.flush(&mut audio);
        let quality = if preview { Quality::Preview } else { Quality::Final };
        let id = if model_id.is_empty() { models::default_model_id() } else { model_id };
        let model = self.load(id)?;
        // In the app, language ID runs while the user speaks; do it up front here.
        let lid = if mode == OutputMode::Auto && audio.len() >= LID_AFTER { model.identify(&audio).ok().map(|(l, _)| l) } else { None };
        run_final(&self.inner.models_dir, &audio, mode, false, lang_for(mode, lid), quality, || Ok(model))
    }

    /// Stops capture and discards audio — including the audio kept for retry. Safe to call at any time.
    pub fn cancel(&self) {
        *self.inner.last.lock() = None;
        if let Some(session) = self.inner.session.lock().take() {
            // Joining the audio threads takes a few tens of ms; keep the caller snappy.
            thread::spawn(move || teardown(session));
        }
    }

    #[allow(dead_code)]
    pub fn is_recording(&self) -> bool {
        self.inner.session.lock().is_some()
    }

    /// Loads the model into memory so the first dictation starts instantly. Blocking.
    pub fn preload(&self, model_id: &str) -> anyhow::Result<()> {
        let id = if model_id.is_empty() { models::default_model_id() } else { model_id };
        self.load(id).map(|_| ())
    }

    // ── internals ──────────────────────────────────────────────────────────

    fn cancel_now(&self) {
        if let Some(session) = self.inner.session.lock().take() {
            teardown(session);
        }
    }

    fn loaded(&self, id: &str) -> Option<Arc<Model>> {
        self.inner.model.lock().as_ref().filter(|m| m.id == id).cloned()
    }

    fn load(&self, id: &str) -> anyhow::Result<Arc<Model>> {
        let _serial = self.inner.loading.lock();
        if let Some(m) = self.loaded(id) {
            return Ok(m);
        }
        let (path, kind) = models::resolve(&self.inner.models_dir, id).ok_or_else(|| anyhow!("speech model {id} is not downloaded"))?;
        // Free the previous model first — two large models may not fit in memory.
        *self.inner.model.lock() = None;
        let t = Instant::now();
        let model = Arc::new(Model::load(id, &path, kind)?);
        if let Err(e) = model.warm_up() {
            log::warn!("model warm-up: {e:#}");
        }
        log::info!("loaded {id} in {:?}", t.elapsed());
        *self.inner.model.lock() = Some(model.clone());
        Ok(model)
    }
}

/// Shared tail of every transcription: find speech, normalise, decode, clean, convert.
fn run_final(
    models_dir: &Path,
    audio: &[f32],
    mode: OutputMode,
    voice_emoji: bool,
    lang: Lang,
    quality: Quality,
    model: impl FnOnce() -> anyhow::Result<Arc<Model>>,
) -> anyhow::Result<TranscriptResult> {
    let audio_ms = (audio.len() * 1000 / SAMPLE_RATE) as u64;
    let (mut clip, voiced_ms) = speech_only(models_dir, audio);
    if voiced_ms < MIN_SPEECH_MS {
        return Ok(TranscriptResult { text: String::new(), raw: String::new(), language: String::new(), audio_ms, inference_ms: 0 });
    }
    dsp::normalize(&mut clip);
    if let Some(path) = std::env::var_os("MOZHI_DEBUG_WAV") {
        // Developer aid: the exact audio the final pass sees.
        let _ = dsp::write_wav(Path::new(&path), audio);
        let _ = dsp::write_wav(&Path::new(&path).with_extension("clip.wav"), &clip);
    }

    let model = model()?;
    let t = Instant::now();
    let (raw, lang) = model.transcribe(&clip, &Decode { lang, quality, previous: None, abort: None })?;
    let inference_ms = t.elapsed().as_millis() as u64;
    let cleaned = clean::clean(&raw, voiced_ms);
    let text = finalize(&cleaned, mode, voice_emoji);
    log::info!(
        "transcribed {:.1}s ({} ms voiced) in {inference_ms} ms with {} [{lang}]",
        audio_ms as f32 / 1000.0,
        voiced_ms,
        model.id
    );
    Ok(TranscriptResult { text, raw: raw.trim().to_string(), language: lang.into(), audio_ms, inference_ms })
}

/// The speech in `audio` with silence and noise removed, plus voiced time. Silero VAD when
/// its model is present, otherwise an energy threshold.
fn speech_only(models_dir: &Path, audio: &[f32]) -> (Vec<f32>, u64) {
    if let Some(map) = vad::Vad::load(models_dir).and_then(|mut v| v.speech(audio)) {
        return (vad::splice(audio, &map), map.voiced_ms);
    }
    let speech = dsp::find_speech(audio);
    (audio[speech.start..speech.end].to_vec(), speech.voiced_ms)
}

/// Unpadded speech intervals: Silero VAD, or the energy detector as a single span.
fn speech_intervals(vad: Option<&mut vad::Vad>, audio: &[f32]) -> Vec<(usize, usize)> {
    if let Some(iv) = vad.and_then(|v| v.intervals(audio)) {
        return iv;
    }
    let s = dsp::find_speech(audio);
    if s.voiced_ms == 0 {
        return Vec::new();
    }
    // find_speech pads 250 ms on each side; undo that to measure the real pause.
    let pad = SAMPLE_RATE / 4;
    vec![((s.start + pad).min(s.end), s.end.saturating_sub(pad).max(s.start))]
}

/// Where to close the open phrase (an offset into it), or None to keep listening: at the
/// first pause of ≥650 ms that follows ≥1 s of speech, cut in the middle of the pause. Pauses
/// inside the open audio count too, so a slow preview tick that misses the moment of silence
/// still closes the phrase.
fn pause_cut(intervals: &[(usize, usize)], open_len: usize) -> Option<usize> {
    let mut voiced = 0;
    for (i, &(a, b)) in intervals.iter().enumerate() {
        voiced += b - a;
        let next = intervals.get(i + 1).map(|n| n.0).unwrap_or(open_len);
        let pause = next.saturating_sub(b);
        if voiced >= SEGMENT_MIN_VOICED && pause >= SEGMENT_PAUSE {
            return Some(b + pause / 2);
        }
    }
    None
}

/// A phrase that ran past 25 s without a pause: cut in the middle of its longest gap after
/// 8 s, or at the quietest point if there is none.
fn force_cut(intervals: &[(usize, usize)], open: &[f32]) -> usize {
    let lo = SAMPLE_RATE * 8;
    let best = intervals
        .windows(2)
        .map(|w| (w[0].1, w[1].0))
        .filter(|&(a, b)| b > a && (a + b) / 2 >= lo && (a + b) / 2 <= SEGMENT_MAX)
        .max_by_key(|&(a, b)| b - a);
    match best {
        Some((a, b)) => (a + b) / 2,
        None => dsp::split_points(&open[..open.len().min(SEGMENT_MAX + SAMPLE_RATE)], 15.0, 25.0)[0].1,
    }
}

fn teardown(session: Session) {
    if let Some(stop) = &session.lid_stop {
        stop.store(true, Ordering::SeqCst);
    }
    if let Some(p) = session.preview {
        p.detach();
    }
    let _ = session.recorder.stop();
}

/// Auto mode uses the language ID result when there is one; otherwise it decodes as
/// Tamil and labels by script (see `Model::transcribe`).
fn lang_for(mode: OutputMode, lid: Option<&'static str>) -> Lang {
    match (mode, lid) {
        (OutputMode::Tamil | OutputMode::Tanglish, _) => Lang::Tamil,
        (OutputMode::English, _) => Lang::English,
        (OutputMode::Auto, Some("en")) => Lang::English,
        (OutputMode::Auto, Some(_)) => Lang::Tamil,
        (OutputMode::Auto, None) => Lang::Auto,
    }
}

/// Whisper language code for a preview pass.
fn decode_lang(mode: OutputMode, lid: Option<&'static str>) -> &'static str {
    match lang_for(mode, lid) {
        Lang::English => "en",
        _ => "ta",
    }
}

/// Replaces spoken emoji phrases ("heart emoji" → ❤️) in already-final text.
#[allow(dead_code)]
pub fn apply_voice_emoji(text: &str) -> String {
    emoji::apply(text)
}

/// Emoji first, so Tamil keywords still match before Tanglish transliteration.
fn finalize(text: &str, mode: OutputMode, voice_emoji: bool) -> String {
    let text = if voice_emoji { emoji::apply(text) } else { text.to_string() };
    match mode {
        OutputMode::Tanglish => translit::tamil_to_tanglish(&text),
        _ => text,
    }
}

/// Language ID for Auto mode, off the critical path: once ~1.5 s of audio exists, and again
/// at ~8 s only if the first answer was unsure. `finish()` uses whatever result is ready and
/// never waits for it.
fn spawn_lid(audio: Audio, slot: Slot, lid: Lid, busy: Busy) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let spawned = thread::Builder::new().name("mozhi-lid".into()).spawn(move || {
        for at in [LID_AFTER, LID_AGAIN] {
            loop {
                if stop_t.load(Ordering::SeqCst) {
                    return;
                }
                if audio.read().len() >= at && peek_slot(&slot).is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let Some(model) = peek_slot(&slot) else { return };
            let head = {
                let a = audio.read();
                a[..a.len().min(30 * SAMPLE_RATE)].to_vec()
            };
            busy.store(true, Ordering::SeqCst);
            let result = model.identify(&head);
            busy.store(false, Ordering::SeqCst);
            match result {
                Ok((l, confident)) if !stop_t.load(Ordering::SeqCst) => {
                    *lid.lock() = Some(l);
                    if confident {
                        return;
                    }
                }
                Ok(_) => return,
                Err(e) => log::warn!("language id: {e:#}"),
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("spawn language id: {e}");
    }
    stop
}

struct PreviewCtx {
    models_dir: PathBuf,
    audio: Audio,
    slot: Slot,
    mode: OutputMode,
    voice_emoji: bool,
    lid: Lid,
    busy: Busy,
    committed: Arc<Committed>,
    sink: Arc<dyn EngineSink>,
}

impl Preview {
    /// Cancel: stop without waiting, and drop whatever an in-flight segment pass produces.
    fn detach(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.abort.store(true, Ordering::SeqCst);
        self.cancelled.store(true, Ordering::SeqCst);
        self.handle.thread().unpark();
    }
}

/// The live worker: closes phrases into `segment`s at pauses and keeps the open tail's
/// `partial` fresh. Every ~700 ms, whichever applies.
fn spawn_preview(ctx: PreviewCtx) -> Preview {
    let stop = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (stop_t, abort_t, cancelled_t) = (stop.clone(), abort.clone(), cancelled.clone());
    let handle = thread::Builder::new()
        .name("mozhi-preview".into())
        .spawn(move || {
            let PreviewCtx { models_dir, audio, slot, mode, voice_emoji, lid, busy, committed, sink } = ctx;
            let mut vad = vad::Vad::load(&models_dir);
            let mut last_len = 0usize;
            let mut last_emitted = String::new();
            loop {
                thread::park_timeout(PREVIEW_EVERY);
                if stop_t.load(Ordering::SeqCst) {
                    return;
                }
                let Some(model) = peek_slot(&slot) else { continue };
                if busy.load(Ordering::SeqCst) {
                    continue;
                }
                let base = committed.upto.load(Ordering::SeqCst);
                let open = {
                    let a = audio.read();
                    // finish() takes the buffer; nothing left to preview.
                    if a.len() < base || stop_t.load(Ordering::SeqCst) {
                        return;
                    }
                    if a.len() == last_len || a.len() - base < SAMPLE_RATE * 6 / 10 {
                        continue;
                    }
                    last_len = a.len();
                    a[base..].to_vec()
                };
                let intervals = speech_intervals(vad.as_mut(), &open);

                // 1 · Close the phrase at a pause (or when it runs too long).
                let cut = pause_cut(&intervals, open.len()).or_else(|| (open.len() > SEGMENT_MAX).then(|| force_cut(&intervals, &open)));
                if let Some(cut) = cut {
                    let lang = lang_for(mode, *lid.lock());
                    let t = Instant::now();
                    // Final quality and no abort flag: a closed phrase is never redone.
                    match run_final(&models_dir, &open[..cut], mode, voice_emoji, lang, Quality::Final, || Ok(model.clone())) {
                        Ok(r) => {
                            if cancelled_t.load(Ordering::SeqCst) {
                                return;
                            }
                            if !r.raw.is_empty() {
                                committed.raw.lock().push(r.raw.clone());
                            }
                            committed.upto.store(base + cut, Ordering::SeqCst);
                            log::debug!("segment: {:.1}s closed in {:?}", cut as f32 / SAMPLE_RATE as f32, t.elapsed());
                            if !r.text.is_empty() {
                                sink.segment(&r.text);
                            }
                            sink.partial("");
                            last_emitted.clear();
                        }
                        // Not committed: the final pass will cover this audio instead.
                        Err(e) => log::warn!("segment pass failed: {e:#}"),
                    }
                    continue;
                }

                // 2 · Re-transcribe the open phrase cheaply for the live view.
                let voiced_ms = (intervals.iter().map(|(a, b)| b - a).sum::<usize>() * 1000 / SAMPLE_RATE) as u64;
                if voiced_ms < 400 {
                    continue;
                }
                let (mut chunk, _) = speech_only(&models_dir, &open);
                dsp::normalize(&mut chunk);
                let l = decode_lang(mode, *lid.lock());
                let opts = Decode { lang: lang_for(mode, *lid.lock()), quality: Quality::Preview, previous: None, abort: Some(abort_t.clone()) };
                let t = Instant::now();
                let text = match model.transcribe_chunk(&chunk, l, &opts) {
                    Ok(text) => text,
                    Err(e) if whisper::is_aborted(&e) => {
                        log::debug!("preview pass aborted");
                        continue;
                    }
                    Err(e) => {
                        log::debug!("preview pass skipped: {e:#}");
                        continue;
                    }
                };
                log::debug!("preview pass: {:.1}s of audio in {:?}", chunk.len() as f32 / SAMPLE_RATE as f32, t.elapsed());
                if stop_t.load(Ordering::SeqCst) {
                    return;
                }
                let shown = finalize(&clean::clean(&text, voiced_ms), mode, voice_emoji);
                if shown != last_emitted {
                    sink.partial(&shown);
                    last_emitted = shown;
                }
            }
        })
        .expect("spawn preview thread");
    Preview { stop, abort, cancelled, handle }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: usize = SAMPLE_RATE;

    #[test]
    fn closes_after_speech_and_a_pause() {
        // 1.5 s of speech, then 0.8 s of silence → cut in the middle of the silence.
        assert_eq!(pause_cut(&[(0, 3 * S / 2)], 3 * S / 2 + 8 * S / 10), Some(3 * S / 2 + 4 * S / 10));
    }

    #[test]
    fn keeps_listening_on_short_pause_or_short_speech() {
        assert_eq!(pause_cut(&[(0, 3 * S / 2)], 3 * S / 2 + 4 * S / 10), None);
        assert_eq!(pause_cut(&[(0, S / 2)], S / 2 + S), None);
        assert_eq!(pause_cut(&[], 3 * S), None);
    }

    #[test]
    fn closes_at_a_pause_inside_the_open_audio() {
        // Speech resumed before the tick: the 1 s pause at 2–3 s still closes the first phrase.
        let iv = [(0, 2 * S), (3 * S, 4 * S)];
        assert_eq!(pause_cut(&iv, 4 * S + S / 10), Some(2 * S + S / 2));
    }

    #[test]
    fn counts_speech_across_intervals() {
        let iv = [(0, S / 2), (S, S + S / 2 + S / 10)];
        assert!(pause_cut(&iv, S + S / 2 + S / 10 + 7 * S / 10).is_some());
    }

    #[test]
    fn force_cut_uses_the_longest_gap_after_eight_seconds() {
        let iv = [(0, 9 * S), (9 * S + S / 5, 14 * S), (14 * S + S / 2, 26 * S)];
        let audio = vec![0.1f32; 26 * S];
        assert_eq!(force_cut(&iv, &audio), 14 * S + S / 4);
    }

    #[test]
    fn finalize_applies_emoji_before_tanglish() {
        assert_eq!(finalize("நன்றி இதயம் எமோஜி", OutputMode::Tanglish, true), "nandri ❤️");
        assert_eq!(finalize("நன்றி இதயம் எமோஜி", OutputMode::Tamil, false), "நன்றி இதயம் எமோஜி");
    }
}
