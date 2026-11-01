//! The dictation state machine.
//!
//!   Idle ──⌃⌃──▶ Starting ──▶ Listening ──⌃⌃ / ↵──▶ Transcribing ──▶ Inserting ──▶ Done ──▶ Idle
//!                   │             │                    │   │                ▲
//!                   │             │ click text:        │   └─▶ Editing ──↵ / ⌃⌃─┘   (review on)
//!                   │             │ edit while it      │
//!                   │             │ keeps listening    └─▶ Error ──Retry──▶ Editing
//!                   └──── esc ────┴──────── esc ─────────────────────┴──▶ Idle (discarded)
//!
//! Live editing: while listening, the engine commits a phrase at every pause (`segment`) and
//! the overlay splices it into the draft at the user's caret, so they can click into the
//! text, move between words, fix them or add emoji while still speaking. The overlay owns the
//! draft and reports it back; the controller keeps a mirror (spliced with the same rule) so a
//! dictation still lands if the overlay ever fails to answer.
//!
//! Errors never lose words: the engine keeps the last session's audio, so Retry re-transcribes
//! it, and the overlay offers the fix that matches the cause (permissions, model, report).
//!
//! Every transition is emitted to the overlay as `mozhi:state`. A session counter makes late
//! results from a cancelled session harmless.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tauri::{AppHandle, Emitter, Manager};

use crate::draft;
use crate::engine::{self, Engine, EngineSink, RecordOptions};
use crate::platform::{self, InsertOptions, KeyEvent};
use crate::store::{now_ms, History, SettingsStore};
use crate::types::*;

pub const EV_STATE: &str = "mozhi:state";
pub const EV_LEVEL: &str = "mozhi:level";
pub const EV_PARTIAL: &str = "mozhi:partial";
pub const EV_SEGMENT: &str = "mozhi:segment";
pub const EV_TARGET: &str = "mozhi:target";
pub const EV_FINAL: &str = "mozhi:final";
pub const EV_SETTINGS: &str = "mozhi:settings";
pub const EV_HISTORY: &str = "mozhi:history";
pub const EV_NAVIGATE: &str = "mozhi:navigate";

const OVERLAY: &str = "overlay";
const TRACK_EVERY: Duration = Duration::from_millis(250);
const LINGER_DONE: Duration = Duration::from_millis(1400);
/// An error the user can act on stays until they do (or for this long).
const LINGER_ACTIONABLE: Duration = Duration::from_secs(20);
/// How long to wait for the overlay to hand back the final draft before using the mirror.
const DRAFT_REPLY: Duration = Duration::from_millis(1200);

struct Inner {
    phase: Phase,
    target: Option<FrontApp>,
    element: Option<FocusedElement>,
    mode: OutputMode,
    /// The transcript as it stands in the overlay (mirror while listening, authoritative
    /// while editing). `caret` is a UTF-16 offset, like a textarea's.
    draft: String,
    caret: usize,
    /// Phrases spliced into the draft this dictation (see `SegmentPayload::rev`).
    rev: u32,
    /// The user typed into the draft — skip LLM polish so it never rewrites their edits.
    user_edited: bool,
    /// The overlay holds keyboard focus while still listening.
    live_edit: bool,
    /// The overlay holds keyboard focus (live editing or review), so a paste must first hand
    /// it back to the target app.
    focused: bool,
    /// Waiting for the overlay to return the final draft.
    final_reply: Option<mpsc::Sender<String>>,
    /// Engine result behind the draft, for the history entry.
    pending: Option<TranscriptResult>,
    /// For the Copy action and reports.
    last_text: Option<String>,
    last_error: Option<String>,
}

/// What happens once the final transcript is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// Paste into the app the user is in.
    Insert,
    /// Show it editable in the overlay.
    Edit,
    /// Save to History (and the clipboard) and open History.
    Keep,
}

pub struct Controller {
    app: AppHandle,
    pub engine: Engine,
    pub settings: Arc<SettingsStore>,
    pub history: Arc<History>,
    pub hotkey: Arc<RwLock<HotkeyConfig>>,
    pub capture: Arc<AtomicBool>,
    session: AtomicU64,
    inner: Mutex<Inner>,
    /// Self-test guard: paste only into the app with this id; anything else is copied instead.
    pub paste_only_into: Mutex<Option<String>>,
}

struct Sink {
    app: AppHandle,
    session: u64,
    ctrl: std::sync::Weak<Controller>,
}

impl Sink {
    fn live(&self) -> Option<Arc<Controller>> {
        self.ctrl.upgrade().filter(|c| c.session.load(Ordering::SeqCst) == self.session)
    }
}

impl EngineSink for Sink {
    fn level(&self, level: f32) {
        if self.live().is_some() {
            let _ = self.app.emit_to(OVERLAY, EV_LEVEL, level);
        }
    }
    fn partial(&self, text: &str) {
        if self.live().is_some() {
            let _ = self.app.emit_to(OVERLAY, EV_PARTIAL, text);
        }
    }
    fn segment(&self, text: &str) {
        if let Some(c) = self.live() {
            c.on_segment(text, false);
        }
    }
}

impl Controller {
    pub fn new(app: AppHandle, engine: Engine, settings: Arc<SettingsStore>, history: Arc<History>) -> Arc<Self> {
        let s = settings.get();
        Arc::new(Self {
            app,
            engine,
            hotkey: Arc::new(RwLock::new(s.hotkey.clone())),
            capture: Arc::new(AtomicBool::new(false)),
            settings,
            history,
            session: AtomicU64::new(0),
            paste_only_into: Mutex::new(None),
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                target: None,
                element: None,
                mode: s.output_mode,
                draft: String::new(),
                caret: 0,
                rev: 0,
                user_edited: false,
                live_edit: false,
                focused: false,
                final_reply: None,
                pending: None,
                last_text: None,
                last_error: None,
            }),
        })
    }

    pub fn app_handle(&self) -> &AppHandle {
        &self.app
    }

    pub fn phase(&self) -> Phase {
        self.inner.lock().phase
    }

    pub fn snapshot(&self) -> (StatePayload, TargetPayload) {
        let i = self.inner.lock();
        (
            StatePayload { phase: i.phase, message: None, mode: i.mode, actions: Vec::new() },
            TargetPayload { app: i.target.clone(), element: i.element.clone() },
        )
    }

    /// The chosen model; with no explicit choice, the default — or, if that isn't downloaded,
    /// whichever model is (so changing the default never strands existing users).
    pub fn model_id(&self) -> String {
        let id = self.settings.get().model_id;
        if !id.is_empty() {
            return id;
        }
        let default = engine::models::default_model_id();
        let catalog = engine::models::catalog(&self.engine.models_dir());
        if catalog.iter().any(|m| m.id == default && m.installed) {
            return default.to_string();
        }
        catalog.into_iter().find(|m| m.installed).map(|m| m.id).unwrap_or_else(|| default.to_string())
    }

    fn set_phase(&self, phase: Phase, message: Option<String>) {
        self.set_state(phase, message, Vec::new());
    }

    fn set_state(&self, phase: Phase, message: Option<String>, actions: Vec<ErrorAction>) {
        let mode = {
            let mut i = self.inner.lock();
            i.phase = phase;
            i.mode
        };
        let _ = self.app.emit(EV_STATE, StatePayload { phase, message, mode, actions });
    }

    fn overlay(&self) -> Option<tauri::WebviewWindow> {
        self.app.get_webview_window(OVERLAY)
    }

    // ── input ──────────────────────────────────────────────────────────────

    pub fn handle_key(self: &Arc<Self>, ev: KeyEvent) {
        let phase = self.phase();
        match (ev, phase) {
            (KeyEvent::DoubleTap, Phase::Idle | Phase::Done | Phase::Error) => self.start(),
            (KeyEvent::DoubleTap, Phase::Starting | Phase::Listening) => self.commit(),
            (KeyEvent::DoubleTap, Phase::Editing) => {
                let draft = self.inner.lock().draft.clone();
                self.insert_edited(draft);
            }
            (KeyEvent::Enter, Phase::Starting | Phase::Listening) if self.settings.get().commit_on_enter => self.commit(),
            (KeyEvent::Escape, Phase::Starting | Phase::Listening | Phase::Transcribing | Phase::Editing) => self.cancel(),
            (KeyEvent::Escape, Phase::Done | Phase::Error) => self.dismiss(),
            _ => {}
        }
    }

    pub fn toggle(self: &Arc<Self>) {
        self.handle_key(KeyEvent::DoubleTap);
    }

    // ── transitions ────────────────────────────────────────────────────────

    pub fn start(self: &Arc<Self>) {
        self.start_from(None);
    }

    /// End-to-end self-test: a dictation fed from recorded audio instead of the microphone.
    pub fn start_with_samples(self: &Arc<Self>, samples: Vec<f32>, sample_rate: u32) {
        self.start_from(Some((samples, sample_rate)));
    }

    fn start_from(self: &Arc<Self>, source: Option<(Vec<f32>, u32)>) {
        let settings = self.settings.get();
        let model_id = self.model_id();
        let session = self.session.fetch_add(1, Ordering::SeqCst) + 1;

        let installed = engine::models::catalog(&self.engine.models_dir()).iter().any(|m| m.id == model_id && m.installed);
        if !installed {
            self.show_overlay(&settings);
            self.set_state(Phase::Error, Some("No speech model yet. Download one in Models".into()), vec![ErrorAction::OpenModels]);
            self.hide_later(session, LINGER_ACTIONABLE);
            return;
        }

        let target = self.current_target();
        let element = platform::focused_element();
        {
            let mut i = self.inner.lock();
            i.target = target.clone();
            i.element = element.clone();
            i.mode = settings.output_mode;
            i.draft.clear();
            i.caret = 0;
            i.rev = 0;
            i.user_edited = false;
            i.live_edit = false;
            i.final_reply = None;
            i.pending = None;
            i.last_error = None;
        }
        let _ = self.app.emit(EV_TARGET, TargetPayload { app: target, element });
        let _ = self.app.emit_to(OVERLAY, EV_PARTIAL, "");
        self.set_phase(Phase::Starting, None);
        self.show_overlay(&settings);
        self.capture.store(true, Ordering::SeqCst);

        let sink = Arc::new(Sink { app: self.app.clone(), session, ctrl: Arc::downgrade(self) });
        let opts = RecordOptions {
            device: settings.mic_device.clone(),
            model_id,
            mode: settings.output_mode,
            live_preview: settings.live_preview,
            voice_emoji: settings.voice_emoji,
        };
        let started = match source {
            Some((samples, rate)) => self.engine.start_with_samples(opts, sink, samples, rate),
            None => self.engine.start(opts, sink),
        };
        match started {
            Ok(()) => {
                self.set_phase(Phase::Listening, None);
                self.spawn_tracker(session);
            }
            Err(e) => {
                log::error!("could not start recording: {e:#}");
                self.capture.store(false, Ordering::SeqCst);
                self.fail(session, &e, false);
            }
        }
    }

    /// ⌃⌃ / ↵ / Insert while listening: transcribe the rest and paste (or review, if asked to).
    pub fn commit(self: &Arc<Self>) {
        let after = if self.settings.get().review_before_insert { After::Edit } else { After::Insert };
        self.finish_with(after);
    }

    /// The user clicked into the transcript while listening: give the overlay keyboard focus so
    /// they can edit, and keep listening. `false` hands focus back (they clicked elsewhere).
    pub fn set_live_edit(self: &Arc<Self>, on: bool) {
        if !matches!(self.phase(), Phase::Starting | Phase::Listening) {
            return;
        }
        let target = {
            let mut i = self.inner.lock();
            if i.live_edit == on {
                return;
            }
            i.live_edit = on;
            i.focused = on;
            i.target.clone()
        };
        // While the overlay is focused it handles ↵ and esc itself (⇧↵ adds a line).
        self.capture.store(!on, Ordering::SeqCst);
        if let Some(w) = self.overlay() {
            let r = if on { platform::focus_overlay(&w) } else { platform::unfocus_overlay(&w, target.filter(|t| !t.is_self).as_ref()) };
            if let Err(e) = r {
                log::warn!("overlay focus ({on}): {e:#}");
            }
        }
    }

    fn finish_with(self: &Arc<Self>, after: After) {
        if !matches!(self.phase(), Phase::Starting | Phase::Listening) {
            return;
        }
        let session = self.session.load(Ordering::SeqCst);
        self.set_phase(Phase::Transcribing, None);
        let me = self.clone();
        thread::Builder::new()
            .name("mozhi-commit".into())
            .spawn(move || me.finish_session(session, after))
            .ok();
    }

    /// A phrase from the engine: splice it into the mirror, then let the overlay splice it
    /// into the real draft (its answer replaces the mirror).
    fn on_segment(&self, text: &str, last: bool) {
        let text = text.trim();
        let rev = {
            let mut i = self.inner.lock();
            if !text.is_empty() {
                let (d, c) = draft::splice(&i.draft, i.caret, text);
                i.draft = d;
                i.caret = c;
            }
            i.rev += 1;
            i.rev
        };
        let _ = self.app.emit_to(OVERLAY, EV_SEGMENT, SegmentPayload { text: text.to_string(), last, rev });
    }

    /// The draft as the controller knows it: (text, caret, phrases applied).
    pub fn draft_state(&self) -> (String, usize, u32) {
        let i = self.inner.lock();
        (i.draft.clone(), i.caret, i.rev)
    }

    /// The overlay reports the draft after every change, phrase or keystroke. A report from an
    /// overlay that hasn't applied every phrase yet is stale (it would drop words): ignore it.
    pub fn update_draft(&self, text: String, caret: Option<usize>, by_user: bool, rev: Option<u32>) {
        let mut i = self.inner.lock();
        if rev.is_some_and(|r| r != i.rev) {
            log::warn!("ignored a stale draft from the overlay (rev {rev:?}, expected {})", i.rev);
            return;
        }
        if matches!(i.phase, Phase::Starting | Phase::Listening | Phase::Transcribing | Phase::Editing) {
            i.caret = caret.unwrap_or_else(|| draft::utf16_len(&text));
            i.draft = text;
            i.user_edited |= by_user;
        }
    }

    /// The overlay's answer to the last segment: the complete draft to insert. If it missed a
    /// phrase, the controller's own copy (which has every phrase) is used instead.
    pub fn final_draft(&self, text: String, rev: Option<u32>) {
        let mut i = self.inner.lock();
        let Some(tx) = i.final_reply.take() else { return };
        if rev.is_some_and(|r| r != i.rev) {
            log::warn!("overlay's final draft missed a phrase (rev {rev:?}, expected {}); using the controller's copy", i.rev);
            let _ = tx.send(i.draft.clone());
        } else {
            let _ = tx.send(text);
        }
    }

    /// ↵ (or ⌃⌃) while editing: give focus back to the target app and paste the edited text.
    pub fn insert_edited(self: &Arc<Self>, text: String) {
        if self.phase() != Phase::Editing {
            return;
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return self.cancel();
        }
        let session = self.session.load(Ordering::SeqCst);
        self.set_phase(Phase::Inserting, None);
        let me = self.clone();
        thread::Builder::new()
            .name("mozhi-insert".into())
            .spawn(move || {
                let (target, result) = {
                    let mut i = me.inner.lock();
                    (i.target.clone(), i.pending.take())
                };
                me.release_focus(target.as_ref());
                if me.session.load(Ordering::SeqCst) == session {
                    me.deliver(session, text, result, target);
                }
            })
            .ok();
    }

    /// The clipboard button: keep what was said (in History and on the clipboard, never lost
    /// mid-dictation) and open History in the main window.
    pub fn open_history(self: &Arc<Self>) {
        match self.phase() {
            Phase::Starting | Phase::Listening => self.finish_with(After::Keep),
            Phase::Editing => {
                let session = self.session.load(Ordering::SeqCst);
                let (draft, result) = {
                    let mut i = self.inner.lock();
                    (i.draft.clone(), i.pending.take())
                };
                self.keep(session, draft, result);
            }
            Phase::Done | Phase::Error => self.dismiss(),
            _ => {}
        }
        crate::show_main_window(&self.app);
        let _ = self.app.emit(EV_NAVIGATE, "history");
    }

    fn finish_session(self: Arc<Self>, session: u64, after: After) {
        let live = || self.session.load(Ordering::SeqCst) == session;
        let settings = self.settings.get();
        let mode = settings.output_mode;

        let result = match self.engine.finish() {
            Ok(r) => r,
            Err(e) => {
                if live() {
                    log::error!("transcription failed: {e:#}");
                    self.capture.store(false, Ordering::SeqCst);
                    let target = self.inner.lock().target.clone();
                    self.release_focus(target.as_ref());
                    self.fail(session, &e, true);
                }
                return;
            }
        };
        if !live() {
            return;
        }

        // Hand the tail to the overlay; it splices it at the caret and returns the full draft.
        let (rx, mirror) = {
            let (tx, rx) = mpsc::channel();
            let mut i = self.inner.lock();
            i.final_reply = Some(tx);
            let (d, _) = draft::splice(&i.draft, i.caret, result.text.trim());
            (rx, d)
        };
        self.on_segment(&result.text, true);
        let mut text = match rx.recv_timeout(DRAFT_REPLY) {
            Ok(t) => t,
            Err(_) => {
                log::warn!("overlay didn't return the draft in time; using the mirror");
                mirror
            }
        };
        self.inner.lock().final_reply = None;
        text = text.trim().to_string();
        if !live() {
            return;
        }

        if text.is_empty() {
            self.capture.store(false, Ordering::SeqCst);
            let target = self.inner.lock().target.clone();
            self.release_focus(target.as_ref());
            self.set_phase(Phase::Done, Some("Didn't catch that. Try again".into()));
            self.hide_later(session, LINGER_DONE);
            return;
        }

        let user_edited = self.inner.lock().user_edited;
        if settings.polish.enabled && !user_edited {
            let cfg = settings.polish.clone();
            let input = text.clone();
            let polished = tauri::async_runtime::block_on(async move {
                tokio::time::timeout(Duration::from_millis(cfg.timeout_ms as u64), engine::polish::polish(&cfg, &input, mode)).await
            });
            match polished {
                Ok(Ok(p)) if !p.trim().is_empty() => text = p.trim().to_string(),
                Ok(Err(e)) => log::warn!("polish failed, using raw transcript: {e:#}"),
                Err(_) => log::warn!("polish timed out, using raw transcript"),
                _ => {}
            }
            if !live() {
                return;
            }
        }

        let _ = self.app.emit(EV_FINAL, &text);
        self.capture.store(false, Ordering::SeqCst);
        self.inner.lock().last_text = Some(text.clone());

        match after {
            After::Edit => self.review(text, Some(result)),
            After::Keep => self.keep(session, text, Some(result)),
            After::Insert => {
                // Paste into the app the user is in *now*: they may have switched while speaking.
                // While the overlay held focus the tracker was paused, so keep the known target.
                let (live_edit, known) = {
                    let i = self.inner.lock();
                    (i.live_edit, i.target.clone())
                };
                let target = if live_edit {
                    self.release_focus(known.as_ref());
                    known
                } else {
                    self.current_target().or(known)
                };
                self.deliver(session, text, Some(result), target);
            }
        }
    }

    /// Shows `text` editable in the overlay; ↵ inserts it.
    fn review(self: &Arc<Self>, text: String, result: Option<TranscriptResult>) {
        {
            let mut i = self.inner.lock();
            i.caret = draft::utf16_len(&text);
            i.draft = text.clone();
            i.pending = result;
            i.live_edit = false;
            i.focused = true;
        }
        let _ = self.app.emit(EV_FINAL, &text);
        self.set_phase(Phase::Editing, None);
        if let Some(w) = self.overlay() {
            if let Err(e) = platform::focus_overlay(&w) {
                log::warn!("focus overlay: {e:#}");
            }
        }
    }

    /// Pastes `text` into `target` (or copies it when there's nowhere to paste), records it
    /// in History and shows the outcome.
    fn deliver(self: &Arc<Self>, session: u64, text: String, result: Option<TranscriptResult>, target: Option<FrontApp>) {
        let settings = self.settings.get();
        let mode = self.inner.lock().mode;
        self.inner.lock().last_text = Some(text.clone());
        let allowed = match self.paste_only_into.lock().as_deref() {
            Some(id) => target.as_ref().map(|t| t.id == id).unwrap_or(false),
            None => true,
        };
        let pasteable = allowed && settings.auto_paste && target.as_ref().map(|t| !t.is_self).unwrap_or(false);

        let mut actions = Vec::new();
        let (inserted, message) = if pasteable {
            let target = target.clone().unwrap();
            self.set_phase(Phase::Inserting, None);
            match platform::insert_text(&text, &target, InsertOptions { restore_clipboard: settings.restore_clipboard }) {
                Ok(()) => (true, format!("Inserted into {}", target.name)),
                Err(e) => {
                    log::error!("paste failed: {e:#}");
                    self.inner.lock().last_error = Some(format!("paste into {}: {e:#}", target.id));
                    let _ = platform::copy_to_clipboard(&text);
                    let ax = platform::permissions().accessibility;
                    if !matches!(ax, PermState::Granted | PermState::NotRequired) {
                        actions.push(ErrorAction::OpenAccessibility);
                        (false, "Copied, not pasted: Mozhi needs Accessibility to paste".to_string())
                    } else {
                        actions.push(ErrorAction::Report);
                        (false, "Couldn't paste, so it was copied. Press ⌘V".to_string())
                    }
                }
            }
        } else {
            match platform::copy_to_clipboard(&text) {
                Ok(()) => (false, "Copied to clipboard".to_string()),
                Err(_) => (false, "Ready. Copy it from Mozhi → History".to_string()),
            }
        };

        self.record(session, &text, result.as_ref(), mode, target.map(|t| t.name), inserted);
        let linger = if actions.is_empty() { LINGER_DONE } else { LINGER_ACTIONABLE };
        self.set_state(Phase::Done, Some(message), actions);
        self.hide_later(session, linger);
    }

    /// Saves the transcript to History and the clipboard without pasting, and closes the overlay.
    fn keep(self: &Arc<Self>, session: u64, text: String, result: Option<TranscriptResult>) {
        let text = text.trim().to_string();
        if !text.is_empty() {
            let _ = platform::copy_to_clipboard(&text);
            let mode = self.inner.lock().mode;
            self.record(session, &text, result.as_ref(), mode, None, false);
        }
        self.dismiss();
    }

    fn record(&self, session: u64, text: &str, result: Option<&TranscriptResult>, mode: OutputMode, app_name: Option<String>, inserted: bool) {
        if !self.settings.get().history_enabled {
            return;
        }
        self.history.add(HistoryItem {
            id: format!("{}-{}", now_ms(), session),
            at: now_ms(),
            text: text.to_string(),
            raw: result.map(|r| r.raw.clone()).unwrap_or_default(),
            language: result.map(|r| r.language.clone()).unwrap_or_default(),
            mode,
            app_name,
            audio_ms: result.map(|r| r.audio_ms).unwrap_or(0),
            inference_ms: result.map(|r| r.inference_ms).unwrap_or(0),
            inserted,
        });
        let _ = self.app.emit(EV_HISTORY, ());
    }

    pub fn cancel(self: &Arc<Self>) {
        self.session.fetch_add(1, Ordering::SeqCst);
        let target = {
            let mut i = self.inner.lock();
            i.pending = None;
            i.final_reply = None;
            i.target.clone()
        };
        self.engine.cancel();
        self.capture.store(false, Ordering::SeqCst);
        self.release_focus(target.as_ref());
        self.set_phase(Phase::Idle, None);
        self.hide_overlay();
    }

    fn dismiss(self: &Arc<Self>) {
        self.session.fetch_add(1, Ordering::SeqCst);
        let target = {
            let mut i = self.inner.lock();
            i.pending = None;
            i.target.clone()
        };
        self.capture.store(false, Ordering::SeqCst);
        self.release_focus(target.as_ref());
        self.set_phase(Phase::Idle, None);
        self.hide_overlay();
    }

    pub fn set_mode(&self, mode: OutputMode) {
        if let Ok(s) = self.settings.update(|s| s.output_mode = mode) {
            self.inner.lock().mode = mode;
            let _ = self.app.emit(EV_SETTINGS, &s);
        }
    }

    // ── error loop ─────────────────────────────────────────────────────────

    /// Shows a failure with the actions that fix its cause. `kept_audio`: the engine still has
    /// the recording, so Retry can transcribe it again.
    fn fail(self: &Arc<Self>, session: u64, e: &anyhow::Error, kept_audio: bool) {
        let (message, mut actions) = classify(e);
        if kept_audio {
            actions.insert(0, ErrorAction::Retry);
        }
        let has_text = !self.inner.lock().draft.trim().is_empty();
        if has_text {
            actions.push(ErrorAction::Copy);
        }
        actions.push(ErrorAction::Report);
        self.inner.lock().last_error = Some(format!("{e:#}"));
        self.set_state(Phase::Error, Some(message), actions);
        self.hide_later(session, LINGER_ACTIONABLE);
    }

    /// Retry: transcribe the kept audio from scratch, then show it for review — the user just
    /// saw something go wrong, so they get to check it before it's inserted.
    pub fn retry(self: &Arc<Self>) {
        if self.phase() != Phase::Error {
            return;
        }
        let session = self.session.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_phase(Phase::Transcribing, None);
        let me = self.clone();
        thread::Builder::new()
            .name("mozhi-retry".into())
            .spawn(move || {
                let s = me.settings.get();
                let result = me.engine.retry_last(s.output_mode, s.voice_emoji);
                if me.session.load(Ordering::SeqCst) != session {
                    return;
                }
                match result {
                    Ok(r) if !r.text.trim().is_empty() => {
                        let text = r.text.trim().to_string();
                        me.inner.lock().last_text = Some(text.clone());
                        me.review(text, Some(r));
                    }
                    Ok(_) => {
                        me.set_phase(Phase::Done, Some("Didn't catch any words in that recording".into()));
                        me.hide_later(session, LINGER_DONE);
                    }
                    Err(e) => {
                        log::error!("retry failed: {e:#}");
                        me.fail(session, &e, false);
                    }
                }
            })
            .ok();
    }

    /// The Copy action: whatever text exists (draft or last result) to the clipboard and History.
    pub fn copy_current(self: &Arc<Self>) {
        let session = self.session.load(Ordering::SeqCst);
        let text = {
            let i = self.inner.lock();
            if i.draft.trim().is_empty() { i.last_text.clone().unwrap_or_default() } else { i.draft.clone() }
        };
        if text.trim().is_empty() {
            return;
        }
        let _ = platform::copy_to_clipboard(text.trim());
        let mode = self.inner.lock().mode;
        self.record(session, text.trim(), None, mode, None, false);
        self.set_phase(Phase::Done, Some("Copied to clipboard".into()));
        self.hide_later(session, LINGER_DONE);
    }

    /// A prefilled GitHub issue: version, OS, model, the error and the tail of the log. Mozhi's
    /// logs hold timings and errors, never what was said. The user reviews it in the browser
    /// before anything is sent.
    pub fn report_url(&self) -> String {
        let s = self.settings.get();
        let (error, phase) = {
            let i = self.inner.lock();
            (i.last_error.clone().unwrap_or_default(), i.phase)
        };
        let version = self.app.package_info().version.to_string();
        let logs = self.app.path().app_log_dir().ok().and_then(|dir| log_tail(&dir, 40)).unwrap_or_default();
        let title = if error.is_empty() { "Problem report".to_string() } else { format!("Error: {}", first_line(&error, 70)) };
        let what = format!(
            "<!-- What were you doing when this happened? -->\n\n\nError: {error}\nModel: {} · mode: {:?} · live preview: {} · phase: {phase:?}",
            self.model_id(),
            s.output_mode,
            s.live_preview
        );
        let os_choice = if cfg!(target_os = "macos") {
            if std::env::consts::ARCH == "aarch64" { "macOS (Apple Silicon)" } else { "macOS (Intel)" }
        } else {
            "Windows 11"
        };
        let mut url = String::from("https://github.com/Kamal007OLica/mozhi/issues/new?template=bug_report.yml");
        for (k, v) in [("title", title.as_str()), ("what", what.as_str()), ("os", os_choice), ("version", version.as_str()), ("logs", logs.as_str())] {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.push_str(&encode(v));
        }
        // Browsers and GitHub cope with ~8 KB; the log tail is what gives way.
        url.truncate(7800);
        url
    }

    // ── helpers ────────────────────────────────────────────────────────────

    /// Hands keyboard focus back from the overlay to `target` if the overlay holds it.
    fn release_focus(&self, target: Option<&FrontApp>) {
        let was = {
            let mut i = self.inner.lock();
            i.live_edit = false;
            std::mem::replace(&mut i.focused, false)
        };
        if !was {
            return;
        }
        if let Some(w) = self.overlay() {
            if let Err(e) = platform::unfocus_overlay(&w, target.filter(|t| !t.is_self)) {
                log::warn!("unfocus overlay: {e:#}");
            }
        }
    }

    fn current_target(&self) -> Option<FrontApp> {
        platform::frontmost_app().map(|mut app| {
            if app.icon.is_none() {
                app.icon = platform::app_icon(&app);
            }
            app
        })
    }

    /// While listening, follow the user across apps so the overlay always names the real target.
    /// Paused while the overlay itself holds focus (on Windows it would report Mozhi).
    fn spawn_tracker(self: &Arc<Self>, session: u64) {
        let me = Arc::downgrade(self);
        thread::Builder::new()
            .name("mozhi-target".into())
            .spawn(move || loop {
                thread::sleep(TRACK_EVERY);
                let Some(me) = me.upgrade() else { return };
                if me.session.load(Ordering::SeqCst) != session || !matches!(me.phase(), Phase::Starting | Phase::Listening) {
                    return;
                }
                if me.inner.lock().live_edit {
                    continue;
                }
                let app = platform::frontmost_app();
                if app.as_ref().map(|a| a.is_self).unwrap_or(false) {
                    continue; // a click on Mozhi's own window doesn't change where text goes
                }
                let element = platform::focused_element();
                let changed = {
                    let i = me.inner.lock();
                    let app_changed = match (&i.target, &app) {
                        (Some(a), Some(b)) => a.pid != b.pid || a.window_title != b.window_title,
                        (None, None) => false,
                        _ => true,
                    };
                    app_changed || i.element != element
                };
                if changed {
                    let app = app.map(|mut a| {
                        a.icon = platform::app_icon(&a);
                        a
                    });
                    {
                        let mut i = me.inner.lock();
                        i.target = app.clone();
                        i.element = element.clone();
                    }
                    let _ = me.app.emit(EV_TARGET, TargetPayload { app, element });
                }
            })
            .ok();
    }

    fn show_overlay(&self, settings: &Settings) {
        if let Some(w) = self.overlay() {
            if let Err(e) = platform::show_overlay(&w, settings.overlay_position) {
                log::warn!("show overlay: {e:#}");
            }
        }
    }

    fn hide_overlay(&self) {
        if let Some(w) = self.overlay() {
            let _ = platform::hide_overlay(&w);
        }
    }

    fn hide_later(self: &Arc<Self>, session: u64, after: Duration) {
        let me = Arc::downgrade(self);
        thread::spawn(move || {
            thread::sleep(after);
            if let Some(me) = me.upgrade() {
                if me.session.load(Ordering::SeqCst) == session && matches!(me.phase(), Phase::Done | Phase::Error) {
                    me.set_phase(Phase::Idle, None);
                    me.hide_overlay();
                }
            }
        });
    }
}

/// A plain message plus the action that fixes the cause.
fn classify(e: &anyhow::Error) -> (String, Vec<ErrorAction>) {
    let s = format!("{e:#}").to_lowercase();
    if s.contains("permission") || s.contains("not authorized") || s.contains("denied") {
        ("Microphone is blocked. Allow Mozhi in Settings".into(), vec![ErrorAction::OpenMicSettings])
    } else if s.contains("device") || s.contains("no input") {
        ("No microphone found. Check it's connected".into(), vec![ErrorAction::OpenMicSettings])
    } else if s.contains("not downloaded") || s.contains("failed to load") {
        ("Speech model failed to load. Download it again in Models".into(), vec![ErrorAction::OpenModels])
    } else if s.contains("taking too long") {
        ("The speech model is still loading. Try again in a moment".into(), Vec::new())
    } else {
        ("Couldn't transcribe that. Your words are kept, so try again".into(), Vec::new())
    }
}

fn first_line(s: &str, max: usize) -> String {
    s.lines().next().unwrap_or("").chars().take(max).collect()
}

/// The newest `n` lines of the newest log file in `dir`.
fn log_tail(dir: &std::path::Path, n: usize) -> Option<String> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "log").unwrap_or(false))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    let text = std::fs::read_to_string(newest.path()).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    Some(lines[lines.len().saturating_sub(n)..].join("\n"))
}

/// Percent-encodes for a URL query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_their_fix() {
        let (_, a) = classify(&anyhow::anyhow!("microphone permission denied"));
        assert_eq!(a, vec![ErrorAction::OpenMicSettings]);
        let (_, a) = classify(&anyhow::anyhow!("speech model tamil-small-q8 is not downloaded"));
        assert_eq!(a, vec![ErrorAction::OpenModels]);
        let (m, a) = classify(&anyhow::anyhow!("whisper inference failed: -6"));
        assert!(a.is_empty() && m.contains("kept"));
    }

    #[test]
    fn url_encoding() {
        assert_eq!(encode("a b&c=ஃ"), "a%20b%26c%3D%E0%AE%83");
    }
}
