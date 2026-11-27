// Small pictures for the AI Agent: a mark per provider (its logo, or its
// initials, on a colour that recalls it) and an avatar per agent (a gradient
// picked from its name, so each agent keeps its own look everywhere).

import { h } from "../../core/dom";
import { KIND_LOGOS, LOGOS } from "./logos";

interface Mark {
  text: string;
  bg: string;
  fg?: string;
}

const MARKS: Record<string, Mark> = {
  anthropic: { text: "A", bg: "#d97757" },
  openai: { text: "O", bg: "#0d0d0d" },
  gemini: { text: "G", bg: "linear-gradient(135deg, #4285f4, #9b72cb 60%, #d96570)" },
  xai: { text: "x", bg: "#111" },
  deepseek: { text: "D", bg: "#4d6bfe" },
  mistral: { text: "M", bg: "linear-gradient(135deg, #ffd800, #fa520f)" },
  moonshot: { text: "K", bg: "#16191f" },
  zai: { text: "Z", bg: "#2d5cf6" },
  qwen: { text: "Q", bg: "#615ced" },
  minimax: { text: "MM", bg: "#e73562" },
  opencode: { text: "oc", bg: "#211e1e" },
  "opencode-go": { text: "go", bg: "linear-gradient(135deg, #211e1e 55%, #3b82f6)" },
  openrouter: { text: "OR", bg: "#6467f2" },
  vercel: { text: "▲", bg: "#000" },
  huggingface: { text: "HF", bg: "#ffd21e", fg: "#1f2328" },
  groq: { text: "g", bg: "#f55036" },
  cerebras: { text: "C", bg: "#f15a29" },
  together: { text: "T", bg: "#0f6fff" },
  fireworks: { text: "F", bg: "#6720ff" },
  nvidia: { text: "N", bg: "#76b900" },
  ollama: { text: "O", bg: "#f2f2f2", fg: "#111" },
  lmstudio: { text: "LM", bg: "#4f46e5" },
  vllm: { text: "v", bg: "#fdb515", fg: "#1f2328" },
  llamacpp: { text: "ll", bg: "#8b5cf6" },
  chatgpt: { text: "C", bg: "#10a37f" },
  codex: { text: "cx", bg: "linear-gradient(135deg, #1b1b1f 40%, #4d55e8)" },
  custom: { text: "{ }", bg: "#475569" },
  "custom-anthropic": { text: "{A}", bg: "#7c5a4a" },
};

const SVG = "http://www.w3.org/2000/svg";

/** A logo as an SVG drawn in the current colour. */
function logoSvg(name: string): SVGSVGElement | null {
  const paths = LOGOS[name];
  if (!paths) return null;
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "currentColor");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add("ai-mark-logo");
  for (const p of paths) {
    const path = document.createElementNS(SVG, "path");
    path.setAttribute("d", p.d);
    if (p.fillRule) path.setAttribute("fill-rule", p.fillRule);
    if (p.clipRule) path.setAttribute("clip-rule", p.clipRule);
    if (p.fillOpacity) path.setAttribute("fill-opacity", p.fillOpacity);
    if (p.opacity) path.setAttribute("opacity", p.opacity);
    svg.appendChild(path);
  }
  return svg;
}

/** Whether a provider kind has a logo (others show initials). */
export function hasLogo(kind: string): boolean {
  return (KIND_LOGOS[kind] ?? "") in LOGOS;
}

/** A provider kind's mark (`size` in pixels): its logo on its colour, or
 * its initials. */
export function providerMark(kind: string, size = 28): HTMLElement {
  const mark = MARKS[kind] ?? { text: kind.slice(0, 2).toUpperCase(), bg: "#64748b" };
  const logo = logoSvg(KIND_LOGOS[kind] ?? "");
  const el = h("span", { class: "ai-mark", ariaHidden: true }, logo ?? mark.text);
  el.style.setProperty("--mark-size", `${size}px`);
  el.style.background = mark.bg;
  el.style.color = mark.fg ?? "#fff";
  if (logo) el.classList.add("has-logo");
  else if (mark.text.length > 1) el.classList.add("is-wide");
  return el;
}

function hash(text: string): number {
  let value = 2166136261;
  for (let i = 0; i < text.length; i += 1) {
    value ^= text.charCodeAt(i);
    value = Math.imul(value, 16777619);
  }
  return value >>> 0;
}

/** Two hues for a name: the agent's own colours. */
export function agentHues(name: string): [number, number] {
  const n = hash(name.trim().toLowerCase() || "assistant");
  const first = n % 360;
  return [first, (first + 40 + ((n >>> 9) % 80)) % 360];
}

/** An agent's avatar: a soft orb in its colours with its initial. */
export function agentAvatar(name: string, size = 28): HTMLElement {
  const [a, b] = agentHues(name);
  const initial = Array.from(name.trim())[0]?.toUpperCase() ?? "";
  const el = h("span", { class: "ai-avatar", ariaHidden: true }, initial);
  el.style.setProperty("--avatar-size", `${size}px`);
  el.style.setProperty("--avatar-a", `hsl(${a} 78% 62%)`);
  el.style.setProperty("--avatar-b", `hsl(${b} 72% 46%)`);
  return el;
}
