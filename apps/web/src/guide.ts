import "./fonts.css";
import "./tokens.css";
import "./styles.css";
import "./guide.css";

// Each scene is a 360×270 stage that steps through states s1…sN (classes accumulate, so CSS
// for a later step can rely on earlier ones), holds, fades, and loops — only while on screen.
// The numbers are how long each step lasts, in ms.
const TIMELINES: Record<string, number[]> = {
  install: [900, 1100, 900, 2400],
  allow: [900, 1000, 1000, 2400],
  model: [800, 2300, 2400],
  speak: [800, 700, 420, 420, 420, 420, 1800],
  insert: [700, 1000, 1500, 1800],
  edit: [1100, 1200, 1300, 2000],
  modes: [1700, 1700, 1700, 1700],
  emoji: [800, 1000, 700, 1300, 1300, 1800],
  errors: [1500, 700, 1400, 2000],
  settings: [1700, 1700, 1700, 1700],
};
const LEAD_IN = 700;
const FADE = 450;

const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

function sequence(scene: HTMLElement) {
  const stage = scene.querySelector<HTMLElement>(".stage");
  const steps = TIMELINES[scene.dataset.scene ?? ""];
  if (!stage || !steps) return;

  const all = steps.map((_, i) => `s${i + 1}`);
  if (reduced) {
    stage.classList.add(...all.slice(0, -1)); // the settled, final picture
    return;
  }

  let visible = false;
  let step = 0;
  let timer = 0;
  const next = () => {
    timer = 0;
    if (!visible) return;
    if (step < steps.length) {
      stage.classList.add(all[step]);
      timer = window.setTimeout(next, steps[step++]);
    } else {
      stage.classList.add("fading");
      timer = window.setTimeout(() => {
        stage.classList.remove("fading", ...all);
        step = 0;
        timer = window.setTimeout(next, LEAD_IN);
      }, FADE);
    }
  };
  new IntersectionObserver(
    ([e]) => {
      visible = e.isIntersecting;
      if (visible && !timer) timer = window.setTimeout(next, step === 0 ? LEAD_IN : 0);
    },
    { threshold: 0.35 },
  ).observe(scene);
}

// Stages are drawn at 360×270 and scaled to whatever width the layout gives them.
const fit = new ResizeObserver((entries) => {
  for (const e of entries) {
    const el = e.target as HTMLElement;
    el.style.setProperty("--s", String(e.contentRect.width / 360));
  }
});

document.querySelectorAll<HTMLElement>(".scene").forEach((scene) => {
  fit.observe(scene);
  sequence(scene);
});

// Highlight the chapter being read in the table of contents.
const links = new Map<string, HTMLAnchorElement>();
document.querySelectorAll<HTMLAnchorElement>('.toc a[href^="#"]').forEach((a) => links.set(a.hash.slice(1), a));
const io = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      links.forEach((a) => a.removeAttribute("aria-current"));
      links.get(e.target.id)?.setAttribute("aria-current", "step");
    }
  },
  { rootMargin: "-45% 0px -50% 0px" },
);
links.forEach((_, id) => {
  const el = document.getElementById(id);
  if (el) io.observe(el);
});

// A thin turmeric line across the top shows how far through the guide you are.
const bar = document.createElement("div");
bar.className = "gprogress";
bar.setAttribute("aria-hidden", "true");
document.body.append(bar);
const progress = () => {
  const max = document.documentElement.scrollHeight - innerHeight;
  bar.style.transform = `scaleX(${max > 0 ? Math.min(1, scrollY / max) : 0})`;
};
addEventListener("scroll", progress, { passive: true });
progress();
