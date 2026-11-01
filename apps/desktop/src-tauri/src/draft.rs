//! Splicing a dictated phrase into the draft at the user's caret — the rule the overlay
//! applies as phrases arrive while the user edits (mirrored in `src/shared/splice.ts`).
//! The controller uses it as the fallback when the overlay doesn't answer in time.
//!
//! Carets are UTF-16 offsets, because that's what a textarea's `selectionStart` is.

/// Inserts `piece` at `caret` with one space on either side where words would otherwise
/// touch. Returns the new text and the caret just after the inserted piece.
pub fn splice(draft: &str, caret: usize, piece: &str) -> (String, usize) {
    let piece = piece.trim();
    let at = byte_index(draft, caret);
    if piece.is_empty() {
        return (draft.to_string(), utf16_len(&draft[..at]));
    }
    let (before, after) = draft.split_at(at);
    let mut out = String::with_capacity(draft.len() + piece.len() + 2);
    out.push_str(before);
    if needs_space_before(before, piece) {
        out.push(' ');
    }
    out.push_str(piece);
    let caret = utf16_len(&out);
    if needs_space_after(piece, after) {
        out.push(' ');
    }
    out.push_str(after);
    (out, caret)
}

fn needs_space_before(before: &str, piece: &str) -> bool {
    let Some(prev) = before.chars().last() else { return false };
    let first = piece.chars().next().unwrap_or(' ');
    !prev.is_whitespace() && !matches!(first, '.' | ',' | '!' | '?' | ';' | ':' | ')' | '…')
}

fn needs_space_after(piece: &str, after: &str) -> bool {
    let Some(next) = after.chars().next() else { return false };
    let last = piece.chars().last().unwrap_or(' ');
    !next.is_whitespace() && !matches!(next, '.' | ',' | '!' | '?' | ';' | ':' | ')' | '…') && last != '('
}

/// Byte index for a UTF-16 offset, clamped to the text and to a char boundary.
pub fn byte_index(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_with_a_space() {
        assert_eq!(splice("வணக்கம்", 7, "எப்படி இருக்கீங்க?"), ("வணக்கம் எப்படி இருக்கீங்க?".into(), 26));
    }

    #[test]
    fn empty_draft() {
        assert_eq!(splice("", 0, "  hello "), ("hello".into(), 5));
    }

    #[test]
    fn inserts_mid_sentence_at_the_caret() {
        // caret after "naan " → "naan romba late-ah varuven"
        let (t, c) = splice("naan varuven", 5, "romba late-ah");
        assert_eq!(t, "naan romba late-ah varuven");
        assert_eq!(&t[..byte_index(&t, c)], "naan romba late-ah");
    }

    #[test]
    fn inserts_inside_a_word_gets_spaces_both_sides() {
        assert_eq!(splice("ab", 1, "X").0, "a X b");
    }

    #[test]
    fn punctuation_hugs_the_previous_word() {
        assert_eq!(splice("Sari", 4, ", naalaikku paarkalaam").0, "Sari, naalaikku paarkalaam");
        assert_eq!(splice("hello world", 5, "!").0, "hello! world");
    }

    #[test]
    fn caret_past_end_clamps() {
        assert_eq!(splice("ok", 99, "done").0, "ok done");
    }

    #[test]
    fn emoji_and_surrogate_pairs() {
        // 😊 is two UTF-16 units; caret 2 is right after it.
        let (t, c) = splice("😊", 2, "nandri");
        assert_eq!(t, "😊 nandri");
        assert_eq!(c, 2 + 1 + 6);
        assert_eq!(byte_index("😊x", 1), 4, "a caret inside a surrogate pair snaps forward");
    }

    #[test]
    fn empty_piece_is_a_no_op() {
        assert_eq!(splice("text", 2, "   "), ("text".into(), 2));
    }
}
