// "Ask AI": an app hands AI Agent something to talk about. AI Agent opens on
// a new chat with the text in its message box, for the person to finish and
// send (the window parameter `{ ask }`); nothing is sent by itself.

import type { AppContext } from "./app";

/** Whether this account may open AI Agent now. */
export function canAskAi(ctx: AppContext): boolean {
  return ctx.desktop.apps().some((app) => app.id === "agent");
}

/** Open AI Agent on a new chat with `text` in its message box. */
export function askAi(ctx: AppContext, text: string): void {
  ctx.openApp("agent", { ask: text });
}

/** `text` as a Markdown code block (a longer fence when it holds backticks). */
export function fenced(text: string, language = ""): string {
  const runs = [...text.matchAll(/`+/g)].map((match) => match[0].length);
  const fence = "`".repeat(Math.max(2, ...runs) + 1);
  return `${fence}${language}\n${text.replace(/\n+$/, "")}\n${fence}`;
}

/** The end of `text`, at most `max` characters, from a line start when it can. */
export function tailOf(text: string, max: number): string {
  if (text.length <= max) return text;
  const cut = text.slice(text.length - max);
  const newline = cut.indexOf("\n");
  return newline >= 0 && newline < max / 4 ? cut.slice(newline + 1) : cut;
}
