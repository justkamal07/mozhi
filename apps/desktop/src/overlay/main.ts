// The floating dictation overlay.
//
// Listening: the pill never takes focus, so the user's app keeps its caret and the Rust key hook
// delivers ⌃⌃ / ↵ / esc. Each phrase the engine commits at a pause is spliced into the draft;
// the words still being heard show on the live line underneath.
//
// Live editing: click into the text and the pill takes keyboard focus (on macOS without
// activating Mozhi) while it KEEPS LISTENING. Move between words, fix them, add emoji; new
// phrases land at the caret. ↵ inserts, ⇧↵ adds a line, esc cancels.
//
// Review (setting, or after Retry): the final text sits here editable until ↵.
import "../shared/base.css";
import "./overlay.css";

import { emit } from "@tauri-apps/api/event";
import { Voice, type VoiceState } from "../shared/voice";
import { h, fmtDuration } from "../shared/dom";
import { splice } from "../shared/splice";
import { EmojiPicker } from "./emoji";
import {
  api,
  events,
  inTauri,
  MODE_LABELS,
  triggerGlyph,
  type ErrorAction,
  type OutputMode,
  type Phase,
  type SegmentPayload,
  type Settings,
  type TargetPayload,
} from "../shared/ipc";
import { setLang, t, type Key } from "../shared/i18n";

const MODES: OutputMode[] = ["auto", "tamil", "tanglish", "english"];
const WINDOW_WIDTH = 600;
const GLOW_ROOM = 64; // transparent margin above and below the card for its shadow and aura
const LINES = 3;

let phase: Phase = "idle";
let mode: OutputMode = "auto";
let settings: Settings | null = null;
let startedAt = 0;
/** Words still being heard (not yet committed). */
let live = "";
/** Where the next phrase goes when the textarea isn't focused (UTF-16 offset). */
let caret = 0;
/** The pill holds keyboard focus while still listening. */
let liveEditing = false;
/** An IME (Tamil99, phonetic…) is mid-composition: phrases wait so they don't break it. */
let composing = false;
const queued: SegmentPayload[] = [];

const stage = h("div", { class: "stage" });
const voice = new Voice({ height: 30, onLevel: (l) => stage.style.setProperty("--lvl", l.toFixed(3)) });

const appIcon = h("img", { class: "target-icon", alt: "" }) as HTMLImageElement;
const appName = h("strong", { class: "target-app" });
const fieldName = h("span", { class: "target-field" });
const targetEl = h("div", { class: "target", "data-tauri-drag-region": true, title: "" }, appIcon, h("div", { class: "target-text", "data-tauri-drag-region": true }, h("span", { class: "eyebrow", "data-tauri-drag-region": true }), h("div", { class: "target-line", "data-tauri-drag-region": true }, appName, fieldName)));
const modeBtn = h("button", { class: "mode", type: "button", onclick: cycleMode });
const textEl = h("textarea", { class: "transcript", id: "transcript", rows: 1, readonly: true, spellcheck: "false", autocomplete: "off" }) as HTMLTextAreaElement;
const liveEl = h("div", { class: "live", "aria-live": "polite" }, h("span", { class: "live-text" }), h("span", { class: "live-where" }));
const actionsEl = h("div", { class: "fixes", role: "group" });
const statusEl = h("span", { class: "status" });
const timerEl = h("span", { class: "timer mono" });
const hintEl = h("span", { class: "hints" });
const picker = new EmojiPicker(insertEmoji, () => {
  if (isLive()) startLiveEdit();
});
const emojiBtn = h("button", {
  class: "icon-btn emoji-btn",
  type: "button",
  "aria-label": "",
  "aria-expanded": "false",
  onmousedown: (e: MouseEvent) => e.preventDefault(), // keep the caret where it is
  onclick: () => toggleEmoji(),
  html: `<svg viewBox="0 0 24 24" width="17" height="17" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M8.5 14.5c.9 1.3 2.1 2 3.5 2s2.6-.7 3.5-2"/><circle cx="9" cy="10" r=".9" fill="currentColor"/><circle cx="15" cy="10" r=".9" fill="currentColor"/></svg>`,
});
const cancelBtn = h("button", { class: "icon-btn", type: "button", "aria-label": "", onclick: () => api.cancelDictation() }, "✕");
const historyBtn = h("button", {
  class: "icon-btn history-btn",
  type: "button",
  "aria-label": "",
  onclick: () => api.openHistory(),
  html: `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5" y="4" width="14" height="17" rx="2.5"/><path d="M9 4.5V3.6c0-.6.4-1 1-1h4c.6 0 1 .4 1 1v.9M9 10h6M9 14h6M9 18h3.5"/></svg>`,
});
const insertBtn = h("button", { class: "insert", type: "button", onclick: insertNow });

const card = h(
  "section",
  { class: "card", "data-phase": "idle" },
  h("i", { class: "edge", "aria-hidden": "true" }),
  h("i", { class: "shine", "aria-hidden": "true" }),
  h("header", { class: "head", "data-tauri-drag-region": true }, targetEl, modeBtn),
  picker.el,
  textEl,
  liveEl,
  actionsEl,
  h("footer", { class: "foot" }, h("div", { class: "well" }, voice.el), h("div", { class: "state" }, statusEl, timerEl), h("div", { class: "actions" }, hintEl, emojiBtn, historyBtn, cancelBtn, insertBtn)),
);
stage.append(h("i", { class: "aura", "aria-hidden": "true" }), card);
document.getElementById("overlay")!.append(stage);

const isLive = () => phase === "starting" || phase === "listening";
const focused = () => document.activeElement === textEl;

// ── rendering ────────────────────────────────────────────────────────────────

function applyLang() {
  setLang(settings?.uiLanguage ?? "ta");
  (targetEl.querySelector(".eyebrow") as HTMLElement).textContent = t("ov.into");
  cancelBtn.setAttribute("aria-label", t("ov.cancel"));
  cancelBtn.title = `${t("ov.cancel")} (esc)`;
  historyBtn.setAttribute("aria-label", t("ov.history"));
  historyBtn.title = t("ov.history");
  emojiBtn.setAttribute("aria-label", t("ov.emoji"));
  emojiBtn.title = t("ov.emoji");
  textEl.placeholder = t("ov.placeholder");
  (liveEl.querySelector(".live-where") as HTMLElement).textContent = t("ov.liveAt");
  insertBtn.textContent = t("ov.insert");
  modeBtn.title = t("ov.mode");
  renderHints();
  renderMode();
  fitText();
}

function renderHints() {
  const key = triggerGlyph(settings?.hotkey.key ?? "control");
  hintEl.innerHTML = "";
  if (phase === "editing" || liveEditing) {
    hintEl.append(h("span", { class: "hint" }, h("kbd", null, "↵"), t("ov.hintInsert")), h("span", { class: "hint" }, h("kbd", null, "⇧↵"), t("ov.hintNewline")));
    if (picker.open) hintEl.replaceChildren(); // the picker needs the room
    return;
  }
  // esc is on the ✕ button's tooltip; the footer only has room for the one key that matters.
  hintEl.append(h("span", { class: "hint" }, h("kbd", null, `${key}${key}`), settings?.commitOnEnter !== false ? h("kbd", null, "↵") : "", t("ov.hintInsert")));
}

function renderMode() {
  modeBtn.textContent = MODE_LABELS[mode].short;
  modeBtn.dataset.mode = mode;
}

function renderTarget(p: TargetPayload) {
  const app = p.app;
  targetEl.classList.toggle("warn", !app || app.isSelf);
  if (!app) {
    appIcon.hidden = true;
    appName.textContent = t("ov.noApp");
    fieldName.textContent = "";
    return;
  }
  if (app.isSelf) {
    appIcon.hidden = true;
    appName.textContent = t("ov.self");
    fieldName.textContent = "";
    return;
  }
  appIcon.hidden = !app.icon;
  if (app.icon) appIcon.src = app.icon;
  appName.textContent = app.name;
  const where = p.element?.label || app.windowTitle;
  fieldName.textContent = where ? where : "";
  targetEl.title = [app.name, app.windowTitle, p.element?.label].filter(Boolean).join(" · ");
}

/** At most three lines tall; while listening the newest words stay in view. */
function fitText() {
  const cs = getComputedStyle(textEl);
  const line = parseFloat(cs.lineHeight) || 28;
  const pad = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
  textEl.style.height = "auto";
  textEl.style.height = `${Math.min(textEl.scrollHeight, line * LINES + pad)}px`;
  textEl.classList.toggle("empty", !textEl.value);
  textEl.lang = /[஀-௿]/.test(textEl.value) ? "ta" : "en";
  markClipped();
}

function markClipped() {
  textEl.classList.toggle("clipped", textEl.scrollTop > 2);
}

/** Replace the whole draft (new session, final text, review) unless the user is typing in it. */
function setDraft(text: string) {
  if (focused() && (phase === "editing" || liveEditing)) return;
  textEl.value = text;
  caret = text.length;
  fitText();
  textEl.scrollTop = textEl.scrollHeight;
  markClipped();
}

function renderLive() {
  const text = live.trim();
  const showing = !!text && (isLive() || phase === "transcribing");
  liveEl.hidden = !showing;
  (liveEl.querySelector(".live-text") as HTMLElement).textContent = text;
  liveEl.lang = /[஀-௿]/.test(text) ? "ta" : "en";
  // Only worth saying where it will go when that isn't simply "at the end".
  liveEl.classList.toggle("at-caret", showing && !!textEl.value && currentCaret() < textEl.value.length);
}

function currentCaret() {
  return focused() ? textEl.selectionStart : caret;
}

function renderFixes(actions: ErrorAction[]) {
  actionsEl.replaceChildren(
    ...actions.map((a, i) =>
      h("button", { class: `fix${i === 0 ? " primary" : ""}`, type: "button", onclick: () => runFix(a) }, t(`ov.act.${a}` as Key)),
    ),
  );
  actionsEl.hidden = actions.length === 0;
}

function runFix(a: ErrorAction) {
  switch (a) {
    case "retry":
      return api.retryDictation();
    case "copy":
      return api.copyDictation();
    case "openMicSettings":
      return api.openPermissionSettings("microphone");
    case "openAccessibility":
      return api.openPermissionSettings("accessibility");
    case "openModels":
      api.cancelDictation();
      return api.openMainWindow().then(() => emit("mozhi:navigate", "models"));
    case "report":
      return api.reportIssue();
  }
}

function renderPhase(next: Phase, message?: string | null, actions: ErrorAction[] = []) {
  const prev = phase;
  phase = next;
  card.dataset.phase = next;
  stage.dataset.phase = next;
  const voiceState: Record<Phase, VoiceState> = {
    idle: "idle",
    starting: "idle",
    listening: "listening",
    transcribing: "transcribing",
    editing: "idle",
    inserting: "transcribing",
    done: "done",
    error: "error",
  };
  voice.setState(voiceState[next]);
  if (next !== "listening") voice.setLevel(0);

  const labels: Partial<Record<Phase, string>> = {
    starting: t("ov.starting"),
    listening: t("ov.listening"),
    transcribing: t("ov.transcribing"),
    inserting: t("ov.inserting"),
  };
  statusEl.textContent = message ?? labels[next] ?? "";
  if (next === "done" && message) statusEl.textContent = `✓ ${message}`;
  renderFixes(next === "error" || next === "done" ? actions : []);

  const live_ = next === "starting" || next === "listening";
  const editing = next === "editing";
  if (!live_) liveEditing = false;
  insertBtn.disabled = !(live_ || editing);
  cancelBtn.disabled = next === "idle" || next === "inserting";
  historyBtn.disabled = next === "idle" || next === "transcribing" || next === "inserting";
  emojiBtn.hidden = !(live_ || editing);
  if (!(live_ || editing) && picker.open) toggleEmoji(false);
  textEl.title = live_ && !liveEditing ? t("ov.editHint") : "";

  if (editing) beginEditing();
  else if (!liveEditing) {
    textEl.readOnly = true;
    if (focused()) textEl.blur();
  }
  renderHints();
  if (editing) statusEl.textContent = t("ov.editing");
  if (liveEditing) statusEl.textContent = t("ov.editingLive");

  if (next === "starting" && prev !== "starting") {
    // replay the entrance each time the pill appears
    card.classList.remove("enter");
    void card.offsetWidth;
    card.classList.add("enter");
    startedAt = performance.now();
    live = "";
    queued.length = 0;
    textEl.value = "";
    caret = 0;
    fitText();
    chime("start");
  }
  renderLive();
  if (next === "done") chime("done");
  if (next === "error") chime("error");
}

// ── editing ──────────────────────────────────────────────────────────────────

function beginEditing() {
  textEl.readOnly = false;
  // The native panel becomes key a moment after we ask; focus once it has.
  const focus = (toEnd: boolean) => {
    textEl.focus({ preventScroll: true });
    if (toEnd) {
      const end = textEl.value.length;
      textEl.setSelectionRange(end, end);
      textEl.scrollTop = textEl.scrollHeight;
    }
  };
  focus(true);
  setTimeout(() => focus(false), 60);
  setTimeout(() => focus(false), 200);
}

/** Click into the text while listening: take keyboard focus and keep listening. */
function startLiveEdit() {
  if (liveEditing || !isLive()) return;
  liveEditing = true;
  textEl.readOnly = false;
  textEl.title = "";
  api.editDictation(true);
  // Keep the caret where the click put it once the panel is key.
  const keep = () => {
    if (!focused()) textEl.focus({ preventScroll: true });
  };
  setTimeout(keep, 60);
  setTimeout(keep, 200);
  statusEl.textContent = t("ov.editingLive");
  renderHints();
}

function stopLiveEdit() {
  if (!liveEditing) return;
  caret = textEl.selectionStart;
  liveEditing = false;
  textEl.readOnly = true;
  api.editDictation(false);
  if (phase === "listening") statusEl.textContent = t("ov.listening");
  renderHints();
}

/** A committed phrase: splice it in at the caret and report the new draft. */
function applySegment(p: SegmentPayload) {
  if (composing) {
    queued.push(p);
    return;
  }
  const at = currentCaret();
  const r = splice(textEl.value, at, p.text);
  const atEnd = at >= textEl.value.length;
  textEl.value = r.text;
  caret = r.caret;
  if (focused()) textEl.setSelectionRange(caret, caret);
  live = "";
  fitText();
  if (atEnd) textEl.scrollTop = textEl.scrollHeight;
  renderLive();
  if (p.last) api.finalDraft(textEl.value);
  else api.updateDraft(textEl.value, caret, false).catch(() => {});
}

function insertEmoji(e: string) {
  if (!(isLive() || phase === "editing")) return;
  const r = splice(textEl.value, currentCaret(), e);
  textEl.value = r.text;
  caret = r.caret;
  if (focused()) textEl.setSelectionRange(caret, caret);
  fitText();
  api.updateDraft(textEl.value, caret, true).catch(() => {});
}

function toggleEmoji(show = !picker.open) {
  picker.toggle(show);
  emojiBtn.setAttribute("aria-expanded", String(show));
  card.classList.toggle("with-emoji", show);
  renderHints();
}

/** Send the draft immediately (before ⌃⌃/↵ so the controller has the latest words). */
function flushDraft() {
  clearTimeout(draftTimer);
  return api.updateDraft(textEl.value, textEl.selectionStart, true).catch(() => {});
}

function insertNow() {
  if (phase === "editing") api.insertDraft(textEl.value);
  else flushDraft().then(() => api.commitDictation());
}

// ── behaviour ────────────────────────────────────────────────────────────────

textEl.addEventListener("mousedown", () => {
  if (isLive()) startLiveEdit();
});

textEl.addEventListener("scroll", markClipped, { passive: true });
// Tamil glyphs are taller than the fallback font: measure again once the web fonts are in.
document.fonts?.ready.then(() => {
  fitText();
  if (!focused()) textEl.scrollTop = textEl.scrollHeight;
});

let draftTimer = 0;
textEl.addEventListener("input", () => {
  caret = textEl.selectionStart;
  fitText();
  renderLive();
  clearTimeout(draftTimer);
  draftTimer = window.setTimeout(() => api.updateDraft(textEl.value, textEl.selectionStart, true).catch(() => {}), 40);
});

// Caret moves (arrows, clicks) decide where the next phrase lands.
const trackCaret = () => {
  if (!focused()) return;
  caret = textEl.selectionStart;
  renderLive();
  clearTimeout(draftTimer);
  draftTimer = window.setTimeout(() => api.updateDraft(textEl.value, caret, false).catch(() => {}), 60);
};
textEl.addEventListener("keyup", trackCaret);
textEl.addEventListener("mouseup", trackCaret);
document.addEventListener("selectionchange", trackCaret);

textEl.addEventListener("compositionstart", () => (composing = true));
textEl.addEventListener("compositionend", () => {
  composing = false;
  queued.splice(0).forEach(applySegment);
});

textEl.addEventListener("keydown", (e) => {
  if (!(phase === "editing" || liveEditing) || e.isComposing || e.keyCode === 229) return; // let Tamil IMEs finish
  if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
    e.preventDefault();
    insertNow();
  } else if (e.key === "Escape") {
    e.preventDefault();
    if (picker.open) toggleEmoji(false);
    else api.cancelDictation();
  }
});

picker.search.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    toggleEmoji(false);
    textEl.focus({ preventScroll: true });
  } else if (e.key === "Enter") {
    e.preventDefault();
    (picker.el.querySelector(".emoji-grid .emoji") as HTMLElement | null)?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
  }
});

// Clicking back into their own app ends live editing; dictation carries on.
window.addEventListener("blur", () => {
  setTimeout(() => {
    if (!document.hasFocus()) stopLiveEdit();
  }, 150);
});

function cycleMode() {
  mode = MODES[(MODES.indexOf(mode) + 1) % MODES.length];
  renderMode();
  api.setOutputMode(mode);
}

setInterval(() => {
  timerEl.textContent = isLive() ? fmtDuration(performance.now() - startedAt) : "";
}, 250);

// Grow the native window with the card so nothing transparent sits over other apps.
new ResizeObserver(() => {
  if (!inTauri) return;
  const r = card.getBoundingClientRect();
  api.overlayResize(WINDOW_WIDTH, Math.ceil(r.height + GLOW_ROOM)).catch(() => {});
}).observe(card);

// Remember where the user drags the overlay.
let dragging = false;
card.addEventListener("mousedown", (e) => {
  if ((e.target as HTMLElement).closest("[data-tauri-drag-region]")) dragging = true;
});
window.addEventListener("mouseup", () => {
  if (!dragging) return;
  dragging = false;
  setTimeout(() => api.overlayDragged().catch(() => {}), 120);
});

// Soft cues, synthesised — a wooden "tok" up, two notes down. Off when sounds are disabled.
let audio: AudioContext | null = null;
function chime(kind: "start" | "done" | "error") {
  if (!settings?.sounds) return;
  try {
    audio ??= new AudioContext();
    const notes = kind === "start" ? [660, 880] : kind === "done" ? [784, 523] : [220, 196];
    notes.forEach((f, i) => {
      const o = audio!.createOscillator();
      const g = audio!.createGain();
      const at = audio!.currentTime + i * 0.07;
      o.type = "sine";
      o.frequency.value = f;
      g.gain.setValueAtTime(0.0001, at);
      g.gain.exponentialRampToValueAtTime(0.09, at + 0.012);
      g.gain.exponentialRampToValueAtTime(0.0001, at + 0.16);
      o.connect(g).connect(audio!.destination);
      o.start(at);
      o.stop(at + 0.18);
    });
  } catch {
    /* audio is optional */
  }
}

// ── wiring ───────────────────────────────────────────────────────────────────

async function init() {
  if (!inTauri) return demo();
  const snap = await api.getOverlayState();
  settings = snap.settings;
  mode = snap.state.mode;
  applyLang();
  renderTarget(snap.target);
  renderPhase(snap.state.phase);

  events.onState((p) => {
    mode = p.mode;
    renderMode();
    renderPhase(p.phase, p.message, p.actions ?? []);
  });
  events.onLevel((l) => voice.setLevel(l));
  events.onPartial((s) => {
    live = s;
    renderLive();
  });
  events.onSegment(applySegment);
  events.onFinal(setDraft);
  events.onTarget(renderTarget);
  events.onSettings((s) => {
    settings = s;
    mode = s.outputMode;
    applyLang();
  });
}

/** Browser preview (`npm run dev` → /overlay.html): plays a dictation with live phrases.
 *  `?phase=editing|error|done|emoji` freezes a state for screenshots. */
function demo() {
  settings = { uiLanguage: "ta", sounds: false, commitOnEnter: true, hotkey: { key: "control", doubleTapMs: 380, maxTapMs: 260 } } as Settings;
  applyLang();
  renderTarget({ app: { name: "Slack", id: "com.tinyspeck.slackmacgap", pid: 1, windowTitle: "#general", icon: null, isSelf: false }, element: { role: "textArea", label: "Message #general", editable: true } });
  const phrases = ["வணக்கம் எல்லாருக்கும், இன்னைக்கு meeting-க்கு கொஞ்சம் late ஆகும்.", "பத்து நிமிஷத்துல வந்துடுறேன் 🙏", "அதுக்குள்ள agenda-வ ஒரு தடவை பாத்துருங்க."];
  const frozen = new URLSearchParams(location.search).get("phase");
  let t0 = 0;
  const wobble = (now: number) => {
    t0 = now / 1000;
    voice.setLevel(phase === "listening" ? 0.3 + 0.6 * Math.abs(Math.sin(t0 * 3.1) * Math.sin(t0 * 1.3)) : 0);
    requestAnimationFrame(wobble);
  };
  requestAnimationFrame(wobble);
  if (frozen) {
    renderPhase("starting");
    renderPhase("listening");
    applySegment({ text: phrases[0], last: false });
    applySegment({ text: phrases[1], last: false });
    if (frozen === "editing") {
      textEl.readOnly = false;
      liveEditing = true;
      textEl.focus();
      textEl.setSelectionRange(12, 12);
      caret = 12;
      live = "எல்லாருக்கும்";
      renderPhase("listening");
      renderLive();
    } else if (frozen === "emoji") {
      toggleEmoji(true);
    } else if (frozen === "error") {
      renderPhase("error", "Couldn't transcribe that. Your words are kept, so try again", ["retry", "copy", "report"]);
    } else if (frozen === "done") {
      renderPhase("done", "Inserted into Slack");
    } else {
      live = "அதுக்குள்ள agenda-வ ஒரு";
      renderLive();
    }
    return;
  }
  const run = () => {
    renderPhase("starting");
    let at = 400;
    setTimeout(() => renderPhase("listening"), at);
    phrases.forEach((p, n) => {
      const words = p.split(" ");
      words.forEach((_, i) =>
        setTimeout(() => {
          live = words.slice(0, i + 1).join(" ");
          renderLive();
        }, at + 500 + i * 380),
      );
      at += 500 + words.length * 380 + 600;
      setTimeout(() => applySegment({ text: p, last: false }), at);
      if (n === phrases.length - 1) at += 300;
    });
    setTimeout(() => renderPhase("transcribing"), at + 200);
    setTimeout(() => renderPhase("done", "Inserted into Slack"), at + 1400);
    setTimeout(run, at + 4200);
  };
  run();
}

init();
