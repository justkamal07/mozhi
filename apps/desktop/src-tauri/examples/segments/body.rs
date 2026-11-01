// Body of the `segments` example; `main.rs` provides the `types` and `engine` modules.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use types::OutputMode;

/// Counts whisper.cpp encode failures and prints warnings/errors (all with MOZHI_DEBUG=1).
struct Log {
    /// Real failures: logged as errors.
    encode_failures: AtomicUsize,
    /// Preview passes cancelled on purpose (whisper.cpp still says "failed to encode", at debug).
    aborted: AtomicUsize,
}

static LOG: Log = Log { encode_failures: AtomicUsize::new(0), aborted: AtomicUsize::new(0) };

impl log::Log for Log {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        let msg = r.args().to_string();
        if msg.contains("failed to encode") {
            if r.level() <= log::Level::Warn {
                self.encode_failures.fetch_add(1, Ordering::Relaxed);
            } else {
                self.aborted.fetch_add(1, Ordering::Relaxed);
            }
        }
        let debug = std::env::var_os("MOZHI_DEBUG").is_some();
        let ours = r.target().contains("engine");
        if debug || r.level() <= log::Level::Warn && (ours || msg.contains("failed")) {
            eprintln!("[{} {}] {}", r.level(), r.target(), msg);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct Collect {
    started: Mutex<Option<Instant>>,
    segments: Mutex<Vec<(f32, String)>>,
    partials: AtomicUsize,
}

impl engine::EngineSink for Collect {
    fn level(&self, _: f32) {}
    fn partial(&self, _text: &str) {
        self.partials.fetch_add(1, Ordering::Relaxed);
    }
    fn segment(&self, text: &str) {
        let at = self.started.lock().unwrap().map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
        self.segments.lock().unwrap().push((at, text.to_string()));
    }
}

fn models_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/app.mozhi.desktop/models")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or(home).join("app.mozhi.desktop/models")
    } else {
        home.join(".local/share/app.mozhi.desktop/models")
    }
}

fn read_wav(path: &str) -> (Vec<f32>, u32) {
    let mut r = hound::WavReader::open(path).expect("readable wav");
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let s: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|v| v.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|v| v.unwrap() as f32 * scale).collect()
        }
    };
    (s.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect(), spec.sample_rate)
}

/// Character error rate between two texts, ignoring punctuation, case and spacing.
fn cer(reference: &str, hyp: &str) -> f32 {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric() || ('\u{0B80}'..='\u{0BFF}').contains(c)).flat_map(char::to_lowercase).collect::<Vec<_>>();
    let (a, b) = (norm(reference), norm(hyp));
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + usize::from(ca != cb)));
        }
        prev = cur;
    }
    prev[b.len()] as f32 / a.len().max(1) as f32
}

struct Live {
    segments: Vec<(f32, String)>,
    tail: String,
    combined: String,
    partials: usize,
    finish_ms: u128,
}

fn live(eng: &engine::Engine, samples: &[f32], rate: u32, opts: &engine::RecordOptions) -> anyhow::Result<Live> {
    let sink = Arc::new(Collect::default());
    *sink.started.lock().unwrap() = Some(Instant::now());
    eng.start_with_samples(opts.clone(), sink.clone(), samples.to_vec(), rate)?;
    // The whole clip, plus a beat of silence the way a user pauses before ⌃⌃.
    std::thread::sleep(Duration::from_secs_f32(samples.len() as f32 / rate as f32 + 1.2));
    let t = Instant::now();
    let r = eng.finish()?;
    let finish_ms = t.elapsed().as_millis();
    let segments = sink.segments.lock().unwrap().clone();
    let mut parts: Vec<&str> = segments.iter().map(|(_, s)| s.as_str()).collect();
    if !r.text.is_empty() {
        parts.push(&r.text);
    }
    Ok(Live { combined: parts.join(" "), segments, tail: r.text, partials: sink.partials.load(Ordering::Relaxed), finish_ms })
}

fn main() -> anyhow::Result<()> {
    let _ = log::set_logger(&LOG).map(|_| log::set_max_level(log::LevelFilter::Debug));
    let mut files = Vec::new();
    let mut model = engine::models::default_model_id().to_string();
    let mut mode = OutputMode::Auto;
    let mut emoji = false;
    let mut stress = 0usize;
    let mut retry = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--model" => model = args.next().unwrap_or_default(),
            "--mode" => {
                mode = match args.next().as_deref() {
                    Some("tamil") => OutputMode::Tamil,
                    Some("tanglish") => OutputMode::Tanglish,
                    Some("english") => OutputMode::English,
                    _ => OutputMode::Auto,
                }
            }
            "--emoji" => emoji = true,
            "--stress" => stress = args.next().and_then(|n| n.parse().ok()).unwrap_or(20),
            "--retry" => retry = true,
            _ => files.push(a),
        }
    }
    let eng = engine::Engine::new(models_dir());
    eng.preload(&model)?;
    let opts = engine::RecordOptions { device: None, model_id: model.clone(), mode, live_preview: true, voice_emoji: emoji };
    println!("model {model} · mode {mode:?} · voice emoji {emoji}");

    for f in &files {
        let (samples, rate) = read_wav(f);
        let single = eng.transcribe_samples(&samples, rate, mode, &model, false)?;
        let single_text = if emoji { engine::apply_voice_emoji(&single.text) } else { single.text.clone() };
        let l = live(&eng, &samples, rate, &opts)?;
        println!("\n── {f}  ({:.1}s)", samples.len() as f32 / rate as f32);
        for (at, s) in &l.segments {
            println!("   segment @ {at:>4.1}s  {s}");
        }
        println!("   tail             {}", if l.tail.is_empty() { "∅" } else { &l.tail });
        println!("   live    = {}", l.combined);
        println!("   single  = {single_text}");
        println!("   CER(live vs single) {:.1}% · {} partials · finish() {} ms", cer(&single_text, &l.combined) * 100.0, l.partials, l.finish_ms);
    }

    if stress > 0 {
        let (samples, rate) = read_wav(&files[0]);
        let before = LOG.encode_failures.load(Ordering::Relaxed);
        let aborted_before = LOG.aborted.load(Ordering::Relaxed);
        let t = Instant::now();
        let mut ok = 0;
        for i in 0..stress {
            match live(&eng, &samples, rate, &opts) {
                Ok(l) if !l.combined.is_empty() => ok += 1,
                Ok(_) => println!("   run {i}: empty transcript"),
                Err(e) => println!("   run {i}: {e:#}"),
            }
        }
        let fails = LOG.encode_failures.load(Ordering::Relaxed) - before;
        let aborted = LOG.aborted.load(Ordering::Relaxed) - aborted_before;
        println!(
            "\nstress: {ok}/{stress} live sessions produced text in {:.0}s · \"failed to encode\" errors: {fails} · deliberately aborted previews: {aborted}",
            t.elapsed().as_secs_f32()
        );
    }

    if retry {
        let (samples, rate) = read_wav(&files[0]);
        std::env::set_var("MOZHI_FAULT_FINISH", "1");
        let failed = live(&eng, &samples, rate, &opts);
        std::env::remove_var("MOZHI_FAULT_FINISH");
        println!("\nretry: finish() → {}", failed.err().map(|e| format!("Err({e:#})")).unwrap_or_else(|| "Ok (fault hook needs a debug build)".into()));
        let r = eng.retry_last(mode, emoji)?;
        println!("retry: retry_last() → {:?} in {} ms", r.text, r.inference_ms);
        eng.cancel();
        println!("retry: after cancel() → {}", eng.retry_last(mode, emoji).err().map(|e| e.to_string()).unwrap_or_else(|| "still available (bug)".into()));
    }
    Ok(())
}
