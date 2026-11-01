// Inserting a dictated phrase (or an emoji) into the draft at the caret, with a space where
// words would otherwise touch. Mirrors src-tauri/src/draft.rs — keep the two in step.
// Carets are UTF-16 offsets (textarea selectionStart), which is what JS strings index by.

const HUGS_LEFT = /^[.,!?;:)…]/;
const HUGS_RIGHT = /^[.,!?;:)…]/;

export function splice(draft: string, caret: number, piece: string): { text: string; caret: number } {
  const p = piece.trim();
  const at = Math.max(0, Math.min(caret, draft.length));
  if (!p) return { text: draft, caret: at };
  let before = draft.slice(0, at);
  const after = draft.slice(at);
  if (before && !/\s$/.test(before) && !HUGS_LEFT.test(p)) before += " ";
  const head = before + p;
  const gap = after && !/^\s/.test(after) && !HUGS_RIGHT.test(after) && !p.endsWith("(") ? " " : "";
  return { text: head + gap + after, caret: head.length };
}
