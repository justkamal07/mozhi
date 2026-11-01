// The pill's emoji picker: a quick row of everyday picks, recents, search in English, Tamil
// and Tanglish, and every emoji by group. The full Unicode set loads only when the picker
// first opens; the Tamil vocabulary is shared with the engine's voice emoji.
import words from "../shared/emoji-words.json";
import { h } from "../shared/dom";
import { getLang } from "../shared/i18n";

interface Entry {
  e: string;
  name: string;
  /** Lower-cased: the Unicode name, then the English, Tamil and Tanglish words. */
  aliases: string[];
}
interface Group {
  name: string;
  icon: string;
  items: Entry[];
}

const RECENTS_KEY = "mozhi.emoji.recents";
/** macOS 12 and Windows 10 render emoji up to 14.0; newer ones show as boxes there. */
const MAX_VERSION = 14.0;
const GROUP_ICON: Record<string, string> = {
  "Smileys & Emotion": "😊",
  "People & Body": "👋",
  "Animals & Nature": "🐯",
  "Food & Drink": "🍛",
  "Travel & Places": "🛕",
  Activities: "🎉",
  Objects: "💡",
  Symbols: "❤️",
  Flags: "🏳️",
};
const GROUP_TA: Record<string, string> = {
  "Smileys & Emotion": "முகங்கள்",
  "People & Body": "மனிதர்கள்",
  "Animals & Nature": "இயற்கை",
  "Food & Drink": "உணவு",
  "Travel & Places": "இடங்கள்",
  Activities: "கொண்டாட்டம்",
  Objects: "பொருட்கள்",
  Symbols: "குறியீடுகள்",
  Flags: "கொடிகள்",
};

const extra = new Map<string, string[]>();
for (const w of words.emoji) extra.set(w.e, [...w.en, ...w.ta, ...w.tl].map((x) => x.toLowerCase()));

/** 0 = an alias is exactly the query, 1 = an alias starts with it, 2 = a word does, 3 = contains. */
function rank(x: Entry, q: string): number {
  let best = 4;
  for (const a of x.aliases) {
    if (a === q) return 0;
    if (a.startsWith(q)) best = Math.min(best, 1);
    else if (a.split(/\s+/).some((w) => w.startsWith(q))) best = Math.min(best, 2);
    else if (a.includes(q)) best = Math.min(best, 3);
  }
  return best;
}

let groups: Group[] | null = null;
async function load(): Promise<Group[]> {
  if (groups) return groups;
  const data = (await import("unicode-emoji-json/data-by-group.json")).default as {
    name: string;
    emojis: { emoji: string; name: string; emoji_version: string; skin_tone_support: boolean }[];
  }[];
  groups = data.map((g) => ({
    name: g.name,
    icon: GROUP_ICON[g.name] ?? g.emojis[0]?.emoji ?? "•",
    items: g.emojis
      .filter((x) => parseFloat(x.emoji_version) <= MAX_VERSION)
      .map((x) => ({ e: x.emoji, name: x.name, aliases: [...(extra.get(x.emoji) ?? []), x.name.toLowerCase()] })),
  }));
  return groups;
}

function recents(): string[] {
  try {
    return JSON.parse(localStorage.getItem(RECENTS_KEY) ?? "[]").slice(0, 16);
  } catch {
    return [];
  }
}
function remember(e: string) {
  try {
    localStorage.setItem(RECENTS_KEY, JSON.stringify([e, ...recents().filter((x) => x !== e)].slice(0, 16)));
  } catch {
    /* recents are a convenience */
  }
}

export class EmojiPicker {
  readonly el: HTMLElement;
  readonly search: HTMLInputElement;
  private grid: HTMLElement;
  private tabs: HTMLElement;
  private group = 0;

  constructor(private onPick: (emoji: string) => void, onSearchFocus: () => void) {
    this.search = h("input", {
      class: "emoji-search",
      id: "emoji-search",
      type: "search",
      spellcheck: false,
      autocomplete: "off",
      oninput: () => this.render(),
      onfocus: onSearchFocus,
    }) as HTMLInputElement;
    this.tabs = h("div", { class: "emoji-tabs", role: "tablist" });
    this.grid = h("div", { class: "emoji-grid", role: "listbox" });
    const quick = h("div", { class: "emoji-quick", role: "group" }, ...words.quick.map((e) => this.button(e, e)));
    this.el = h("div", { class: "emoji-panel", hidden: true }, quick, h("div", { class: "emoji-bar" }, this.search, this.tabs), this.grid);
  }

  get open() {
    return !this.el.hidden;
  }

  async toggle(show = !this.open) {
    this.el.hidden = !show;
    if (!show) return;
    this.search.placeholder = getLang() === "ta" ? "தேடு — சிரிப்பு, heart, sapadu…" : "Search — heart, sirippu, coffee…";
    await load();
    this.renderTabs();
    this.render();
  }

  private button(e: string, title: string) {
    return h("button", {
      class: "emoji",
      type: "button",
      title,
      "aria-label": title,
      // mousedown, not click: keep the textarea's caret and focus where they are
      onmousedown: (ev: MouseEvent) => {
        ev.preventDefault();
        remember(e);
        this.onPick(e);
      },
    }, e);
  }

  private renderTabs() {
    if (!groups) return;
    const lang = getLang();
    this.tabs.replaceChildren(
      ...groups.map((g, i) =>
        h("button", {
          class: "emoji-tab",
          type: "button",
          role: "tab",
          title: lang === "ta" ? GROUP_TA[g.name] ?? g.name : g.name,
          "aria-selected": String(i === this.group && !this.search.value),
          onmousedown: (ev: MouseEvent) => {
            ev.preventDefault();
            this.group = i;
            this.search.value = "";
            this.renderTabs();
            this.render();
          },
        }, g.icon),
      ),
    );
  }

  private render() {
    if (!groups) return;
    const q = this.search.value.trim().toLowerCase();
    let items: Entry[];
    let label: string;
    if (q) {
      // Exact words first ("heart" / "இதயம்" → ❤️ before 💔), then prefixes, then substrings;
      // within a rank, Unicode order (the common ones come first).
      items = groups
        .flatMap((g) => g.items)
        .map((x, i) => ({ x, r: rank(x, q), i }))
        .filter((m) => m.r < 4)
        .sort((a, b) => a.r - b.r || a.i - b.i)
        .slice(0, 120)
        .map((m) => m.x);
      label = items.length ? "" : getLang() === "ta" ? "எதுவும் கிடைக்கல" : "Nothing found";
    } else {
      const r = recents();
      const g = groups[this.group];
      items = g.items;
      label = "";
      if (this.group === 0 && r.length) {
        this.grid.replaceChildren(
          h("div", { class: "emoji-label" }, getLang() === "ta" ? "சமீபத்தில்" : "Recent"),
          ...r.map((e) => this.button(e, e)),
          h("div", { class: "emoji-label" }, getLang() === "ta" ? GROUP_TA[g.name] : g.name),
          ...items.map((x) => this.button(x.e, x.name)),
        );
        return;
      }
    }
    this.grid.replaceChildren(...(label ? [h("div", { class: "emoji-empty" }, label)] : []), ...items.map((x) => this.button(x.e, x.name)));
    this.grid.scrollTop = 0;
  }
}
