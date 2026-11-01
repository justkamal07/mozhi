# Mozhi architecture

Mozhi is a menu-bar app (macOS) / tray app (Windows) built with **Tauri 2**: a Rust core plus two
small webviews. Speech recognition runs on-device with **whisper.cpp** (via `whisper-rs`); an
optional clean-up pass uses a **local LLM through Ollama**. Nothing leaves the machine.

```
 ┌──────────── Rust core (apps/desktop/src-tauri) ─────────────┐        ┌──── webviews (apps/desktop/src) ────┐
 │                                                              │        │                                      │
 │ platform/  global key hook ──KeyEvent──▶ controller.rs ──────┼─emit──▶│ overlay.html  floating ஃ pill        │
 │            frontmost app / focused field ◀──── poll 250 ms   │        │               live transcript, target │
 │            insert_text (clipboard + ⌘V)  ◀──── commit        │        │                                      │
 │                                                              │◀invoke─│ index.html    settings, models,       │
 │ engine/    cpal mic → 16 kHz → whisper.cpp ──partial/level──▶│        │               history, onboarding     │
 │            models (download/verify), translit, polish        │        │                                      │
 └──────────────────────────────────────────────────────────────┘        └──────────────────────────────────────┘
```

## The dictation loop

1. **⌃⌃** (double-tap Control; Ctrl on Windows): `platform` reports `KeyEvent::DoubleTap`.
2. `controller` captures the **frontmost app** (always shown in the overlay) and the **focused field**
   (shown when available), shows the overlay *without* taking focus, and starts `engine`.
3. While listening, `engine` streams `level` (drives the voice indicator) and `partial` text — the
   words still being heard. At every pause (≥650 ms after ≥1 s of speech) it closes a **phrase**,
   transcribes it at final quality and emits `segment`; the overlay splices it into the draft at the
   user's caret (`src/shared/splice.ts`, mirrored by `src-tauri/src/draft.rs`). The controller polls
   the frontmost app every 250 ms so the overlay follows the user if they switch apps. Enter and
   Escape are **captured** (swallowed) by the key hook only during this phase.
4. **Live editing.** Clicking the text gives the overlay keyboard focus (`platform::focus_overlay`; on
   macOS a non-activating panel becoming key, so the user's app stays frontmost) **while it keeps
   listening**. The user moves between words, fixes them and adds emoji; new phrases land at the
   caret. Key capture is released so the overlay handles ↵ (insert), ⇧↵ (new line) and esc itself.
   Clicking back into their app ends live editing; dictation carries on.
5. **⌃⌃ or ↵** commits: the engine transcribes the tail since the last phrase → the overlay splices it
   and returns the full draft (`final_draft`; after 1.2 s the controller uses its own mirror) →
   optional LLM polish (skipped if the user edited, so it never rewrites their changes) → paste into
   the app the user is in → restore the clipboard → history.
6. **Esc** cancels and discards the audio.
7. **Emoji**: the 😊 button opens a picker (everyday row, recents, all of Unicode ≤ 14, search in
   English, Tamil and Tanglish). Saying *"heart emoji"* or *"இதயம் எமோஜி"* inserts ❤️. Both use
   `src/shared/emoji-words.json`.
8. **Errors keep the words.** The engine keeps the last session's audio; a failed transcription
   offers **Try again** (re-transcribes it, then shows it for review), **Copy text**, the fix that
   matches the cause (microphone or Accessibility settings, Models) and **Report**, which opens a
   prefilled GitHub issue (version, OS, model, error, log tail — never dictated text) for the user
   to review and send.
9. *Pause to edit before inserting* (setting) stops in the overlay with the final text editable.
10. The **clipboard button** saves the transcript to History and the clipboard without pasting.

**Self-test.** `mozhi --selftest speech.wav --selftest-out result.json --selftest-exit` runs one real
dictation with the WAV in place of the microphone (phrases, overlay, paste into the frontmost app,
History) and writes the outcome as JSON.

## Modules and ownership

| Path | Responsibility |
| --- | --- |
| `src-tauri/src/types.rs` | Serialisable types shared with the UI (mirrored in `src/shared/ipc.ts`). |
| `src-tauri/src/controller.rs` | State machine, event emission, insertion, history. |
| `src-tauri/src/commands.rs` | IPC commands. |
| `src-tauri/src/engine/` | Audio capture, VAD/trim, whisper inference, model catalog + download, Tamil→Tanglish, Ollama polish. |
| `src-tauri/src/platform/` | Key hook, frontmost app + icon, focused element, paste, permissions, overlay panel. |
| `src/shared/` | IPC bridge, brand tokens, shared UI pieces (ஃ dots). |
| `src/overlay/` | The floating pill. |
| `src/app/` | Settings / models / history / onboarding window. |

## Events (Rust → UI)

| Event | Payload |
| --- | --- |
| `mozhi:state` | `{ phase, message, mode, actions }` — idle · starting · listening · transcribing · editing · inserting · done · error; `actions` are the fixes offered |
| `mozhi:level` | `number` 0–1, ~30 Hz, overlay only |
| `mozhi:partial` | `string` words still being heard (since the last phrase), overlay only |
| `mozhi:segment` | `{ text, last }` a committed phrase to splice at the caret, overlay only |
| `mozhi:target` | `{ app: FrontApp \| null, element: FocusedElement \| null }` |
| `mozhi:final` | `string` |
| `mozhi:settings` · `mozhi:history` · `mozhi:navigate` · `mozhi:download` | see `ipc.ts` |

## Models

Default: **Tamil Small** — Whisper small fine-tuned on Tamil by IIT Madras SPRING Lab
(vasista22/whisper-tamil-small, q8_0 ggml, ~265 MB). It is the most accurate Tamil model in the
catalog but Tamil-only: English words come out in Tamil script. **whisper large-v3-turbo (q5_0)**,
~575 MB, is the pick for heavy code-switching and English. With no model chosen, Mozhi uses the
default if it is downloaded, otherwise any installed model. See `engine/models.rs`.

## Language modes

| Mode | Whisper | Post-processing |
| --- | --- | --- |
| Auto | language auto-detect, constrained to ta/en | — |
| தமிழ் | `ta` | optional polish keeps English words in English |
| Tanglish | `ta` | Tamil script → casual Latin (`engine/translit.rs`) |
| English | `en` | — |
