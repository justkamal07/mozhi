//! Exercises live dictation end to end on WAV files, through the same path as the app:
//! `start_with_samples` feeds the audio in real time, phrases close into segments at pauses,
//! `finish()` transcribes the tail. Compares segments + tail with a single-pass transcript.
//!
//!   cargo run --example segments -- <file.wav>… [--model ID] [--mode auto|tamil|tanglish|english]
//!       [--emoji] [--stress N] [--retry]
//!
//! --stress N   N back-to-back live sessions on the first file; counts "failed to encode".
//! --retry      forces finish() to fail (debug builds only) and checks `retry_last` recovers.
#![allow(dead_code)]

#[path = "../../src/types.rs"]
mod types;
#[path = "../../src/engine/mod.rs"]
mod engine;

include!("body.rs");
