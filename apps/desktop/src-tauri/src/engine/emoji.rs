//! Voice emoji: "thumbs up emoji" → 👍, "இதயம் எமோஜி" → ❤️, "sirippu emoji" → 😂.
//!
//! A keyword of one to four words, immediately followed by a trigger word ("emoji",
//! "எமோஜி", …), becomes the emoji. Longer keywords win ("broken heart" before "heart");
//! among entries, the first in the list wins. A trigger with no known keyword before it
//! stays as text. The vocabulary is shared with the UI's emoji picker:
//! `src/shared/emoji-words.json`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const WORDS_JSON: &str = include_str!("../../../src/shared/emoji-words.json");
const MAX_KEYWORD_WORDS: usize = 4;

/// How the speech models actually write a spoken "எமோஜி" (Tamil Small hears "மோயை").
/// Engine-only: these are mis-hearings, not words to offer in the emoji picker.
const ASR_TRIGGERS: &[&str] = &["மோயை", "எமோயை", "இமோயை", "ஈமோஜ்"];

#[derive(Deserialize)]
struct Words {
    triggers: Vec<String>,
    emoji: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    e: String,
    #[serde(default)]
    en: Vec<String>,
    #[serde(default)]
    ta: Vec<String>,
    #[serde(default)]
    tl: Vec<String>,
}

struct Vocab {
    triggers: Vec<String>,
    /// Normalised keyword phrase → emoji (first entry wins).
    keywords: HashMap<String, String>,
}

fn vocab() -> &'static Vocab {
    static V: OnceLock<Vocab> = OnceLock::new();
    V.get_or_init(|| {
        let words: Words = serde_json::from_str(WORDS_JSON).expect("emoji-words.json is valid");
        let mut keywords = HashMap::new();
        for entry in &words.emoji {
            for k in entry.en.iter().chain(&entry.ta).chain(&entry.tl) {
                keywords.entry(norm_phrase(k)).or_insert_with(|| entry.e.clone());
            }
        }
        let mut triggers: Vec<String> = words.triggers.iter().map(|t| norm(t)).collect();
        triggers.extend(ASR_TRIGGERS.iter().map(|t| norm(t)));
        Vocab { triggers, keywords }
    })
}

fn is_edge_punct(c: char) -> bool {
    c.is_ascii_punctuation() || matches!(c, '…' | '“' | '”' | '‘' | '’' | '«' | '»' | '।' | '॥')
}

/// Lower-case, with surrounding punctuation removed ("Emoji!" → "emoji").
fn norm(word: &str) -> String {
    word.trim_matches(is_edge_punct).to_lowercase()
}

fn norm_phrase(phrase: &str) -> String {
    phrase.split_whitespace().map(norm).collect::<Vec<_>>().join(" ")
}

/// Punctuation trailing a word ("emoji," → ","), kept after the emoji.
fn trailing_punct(word: &str) -> &str {
    let keep = word.trim_end_matches(is_edge_punct).len();
    &word[keep..]
}

fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + usize::from(ca != cb)));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The trigger word, or a near miss of it. Speech models spell an unfamiliar "எமோஜி" several
/// ways; a near miss only counts when a known keyword precedes it, and stays tight on short
/// words so names like "மோதி" never turn into an emoji.
fn is_trigger(v: &Vocab, word: &str) -> bool {
    let w = norm(word);
    if v.triggers.contains(&w) {
        return true;
    }
    let chars: Vec<char> = w.chars().collect();
    let max = match chars.len() {
        0..=3 => return false,
        4..=5 => 1,
        _ => 2,
    };
    v.triggers.iter().any(|t| edit_distance(&chars, &t.chars().collect::<Vec<_>>()) <= max)
}

/// Replaces spoken emoji phrases. Apply before Tanglish transliteration so Tamil keywords match.
pub fn apply(text: &str) -> String {
    let v = vocab();
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || !words.iter().any(|w| is_trigger(v, w)) {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    for word in &words {
        if is_trigger(v, word) {
            // Longest keyword ending right before the trigger, from the words emitted so far.
            let hit = (1..=MAX_KEYWORD_WORDS.min(out.len())).rev().find_map(|k| {
                let phrase = norm_phrase(&out[out.len() - k..].join(" "));
                v.keywords.get(&phrase).map(|e| (k, e.clone()))
            });
            if let Some((k, emoji)) = hit {
                out.truncate(out.len() - k);
                out.push(format!("{emoji}{}", trailing_punct(word)));
                continue;
            }
        }
        out.push((*word).to_string());
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::apply;

    #[test]
    fn english_keywords() {
        assert_eq!(apply("thumbs up emoji"), "👍");
        assert_eq!(apply("great job thumbs up emoji"), "great job 👍");
        assert_eq!(apply("I love the heart emoji."), "I love the ❤️.");
        assert_eq!(apply("Heart Emoji!"), "❤️!");
        assert_eq!(apply("tears of joy emoji"), "😂");
    }

    #[test]
    fn longest_keyword_wins() {
        assert_eq!(apply("that hurt broken heart emoji"), "that hurt 💔");
        assert_eq!(apply("yellow heart emoji"), "💛");
    }

    #[test]
    fn tamil_and_tanglish() {
        assert_eq!(apply("இதயம் எமோஜி"), "❤️");
        assert_eq!(apply("சிரிப்பு இமோஜி"), "😂");
        assert_eq!(apply("நாளைக்கு பார்க்கலாம் வணக்கம் எமோஜி"), "நாளைக்கு பார்க்கலாம் 🙏");
        assert_eq!(apply("sirippu emoji"), "😂");
        // Tamil Small's spelling of a spoken "எமோஜி"
        assert_eq!(apply("ரொம்ப நன்றி இதயம் மோயை"), "ரொம்ப நன்றி ❤️");
        assert_eq!(apply("ரொம்ப நன்றி இதயம் இமோஜ"), "ரொம்ப நன்றி ❤️");
        assert_eq!(apply("heart emoj"), "❤️");
        assert_eq!(apply("meeting super ah pochu fire emoji, thanks"), "meeting super ah pochu 🔥, thanks");
    }

    #[test]
    fn mid_sentence_and_several() {
        assert_eq!(apply("happy birthday party emoji have fun heart emoji"), "happy birthday 🎉 have fun ❤️");
    }

    #[test]
    fn unknown_or_bare_trigger_stays_text() {
        assert_eq!(apply("send me an emoji"), "send me an emoji");
        assert_eq!(apply("emoji"), "emoji");
        assert_eq!(apply("spaceship emoji"), "spaceship emoji");
        assert_eq!(apply("no triggers here at all"), "no triggers here at all");
        // near misses need a keyword right before them
        assert_eq!(apply("my emoj phase"), "my emoj phase");
        assert_eq!(apply("நன்றி மோதி"), "நன்றி மோதி");
        assert_eq!(apply("சரி மோஜோ"), "சரி மோஜோ");
        assert_eq!(apply("I really enjoy it"), "I really enjoy it");
    }
}
