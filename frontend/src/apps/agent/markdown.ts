// A small Markdown renderer for AI replies. Parsing is pure (testable without
// a browser); rendering builds DOM nodes with textContent only, so nothing a
// model writes is ever interpreted as HTML. Links open only http(s)/mailto.
//
// Blocks: paragraphs, # headings, fenced ``` code, > quotes, - / * / 1.
// lists (one level of nesting by indentation), | tables |, --- rules.
// Inline: `code`, **bold**, *italic* / _italic_, ~~strike~~, [text](url),
// bare http(s) links.

export type Inline =
  | { t: "text"; v: string }
  | { t: "code"; v: string }
  | { t: "strong"; c: Inline[] }
  | { t: "em"; c: Inline[] }
  | { t: "del"; c: Inline[] }
  | { t: "link"; href: string; c: Inline[] }
  | { t: "br" };

export interface ListItem {
  text: string;
  children: Block[];
}

export type Block =
  | { t: "p"; text: string }
  | { t: "h"; level: number; text: string }
  | { t: "code"; lang: string; v: string; open: boolean }
  | { t: "quote"; children: Block[] }
  | { t: "list"; ordered: boolean; start: number; items: ListItem[] }
  | { t: "table"; head: string[]; align: Array<"left" | "center" | "right" | "">; rows: string[][] }
  | { t: "hr" };

const FENCE = /^(\s{0,3})(`{3,}|~{3,})\s*([^`\s]*)[^`]*$/;
const HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const RULE = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
const BULLET = /^(\s*)([-*+])\s+(.*)$/;
const ORDERED = /^(\s*)(\d{1,9})[.)]\s+(.*)$/;
const QUOTE = /^\s{0,3}>\s?(.*)$/;
const TABLE_SEP = /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$/;

function isTableRow(line: string): boolean {
  return line.includes("|") && line.trim().length > 1;
}

function splitRow(line: string): string[] {
  let row = line.trim();
  if (row.startsWith("|")) row = row.slice(1);
  if (row.endsWith("|") && !row.endsWith("\\|")) row = row.slice(0, -1);
  const cells: string[] = [];
  let current = "";
  let inCode = false;
  for (let i = 0; i < row.length; i += 1) {
    const ch = row[i];
    if (ch === "\\" && row[i + 1] === "|") {
      current += "|";
      i += 1;
    } else if (ch === "`") {
      inCode = !inCode;
      current += ch;
    } else if (ch === "|" && !inCode) {
      cells.push(current.trim());
      current = "";
    } else {
      current += ch;
    }
  }
  cells.push(current.trim());
  return cells;
}

function startsBlock(line: string): boolean {
  return FENCE.test(line) || HEADING.test(line) || RULE.test(line) || QUOTE.test(line) || BULLET.test(line) || ORDERED.test(line);
}

/** Parse Markdown into blocks. An unclosed code fence (a reply still being written) is `open`. */
export function parseMarkdown(source: string): Block[] {
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  return parseLines(lines);
}

function parseLines(lines: string[]): Block[] {
  const blocks: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i] ?? "";
    if (!line.trim()) {
      i += 1;
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      const marker = fence[2] ?? "```";
      const indent = (fence[1] ?? "").length;
      const body: string[] = [];
      i += 1;
      let open = true;
      while (i < lines.length) {
        const current = lines[i] ?? "";
        const trimmed = current.trim();
        if (trimmed.startsWith(marker[0]!.repeat(marker.length)) && /^[`~]+$/.test(trimmed)) {
          open = false;
          i += 1;
          break;
        }
        body.push(indent ? current.replace(new RegExp(`^ {0,${indent}}`), "") : current);
        i += 1;
      }
      blocks.push({ t: "code", lang: fence[3] ?? "", v: body.join("\n"), open });
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      blocks.push({ t: "h", level: (heading[1] ?? "#").length, text: heading[2] ?? "" });
      i += 1;
      continue;
    }
    if (RULE.test(line)) {
      blocks.push({ t: "hr" });
      i += 1;
      continue;
    }
    if (QUOTE.test(line)) {
      const inner: string[] = [];
      while (i < lines.length && QUOTE.test(lines[i] ?? "")) {
        inner.push(QUOTE.exec(lines[i] ?? "")?.[1] ?? "");
        i += 1;
      }
      blocks.push({ t: "quote", children: parseLines(inner) });
      continue;
    }
    if (BULLET.test(line) || ORDERED.test(line)) {
      const parsed = parseList(lines, i);
      blocks.push(parsed.block);
      i = parsed.next;
      continue;
    }
    if (isTableRow(line) && TABLE_SEP.test(lines[i + 1] ?? "") && (lines[i + 1] ?? "").includes("-")) {
      const head = splitRow(line);
      const align = splitRow(lines[i + 1] ?? "").map((cell) => {
        const left = cell.startsWith(":");
        const right = cell.endsWith(":");
        return left && right ? "center" : right ? "right" : left ? "left" : "";
      }) as Array<"left" | "center" | "right" | "">;
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && isTableRow(lines[i] ?? "") && (lines[i] ?? "").trim()) {
        const cells = splitRow(lines[i] ?? "");
        rows.push(head.map((_, index) => cells[index] ?? ""));
        i += 1;
      }
      blocks.push({ t: "table", head, align: head.map((_, index) => align[index] ?? ""), rows });
      continue;
    }
    const paragraph: string[] = [line];
    i += 1;
    while (i < lines.length) {
      const next = lines[i] ?? "";
      if (!next.trim() || startsBlock(next)) break;
      if (isTableRow(next) && TABLE_SEP.test(lines[i + 1] ?? "")) break;
      paragraph.push(next);
      i += 1;
    }
    blocks.push({ t: "p", text: paragraph.join("\n") });
  }
  return blocks;
}

function parseList(lines: string[], from: number): { block: Block; next: number } {
  const first = lines[from] ?? "";
  const ordered = !BULLET.test(first);
  const firstMatch = (ordered ? ORDERED : BULLET).exec(first);
  const baseIndent = (firstMatch?.[1] ?? "").length;
  const start = ordered ? Number(firstMatch?.[2] ?? 1) : 1;
  const items: Array<{ text: string[]; sub: string[] }> = [];
  let i = from;
  while (i < lines.length) {
    const line = lines[i] ?? "";
    const match = BULLET.exec(line) ?? ORDERED.exec(line);
    const indent = match ? (match[1] ?? "").length : line.length - line.trimStart().length;
    if (match && indent <= baseIndent + 1) {
      const sameKind = ordered ? ORDERED.test(line) : BULLET.test(line);
      if (!sameKind) break;
      items.push({ text: [match[3] ?? ""], sub: [] });
      i += 1;
      continue;
    }
    if (!line.trim()) {
      // A blank line ends the list unless the next line continues it.
      const next = lines[i + 1] ?? "";
      const nextIndent = next.length - next.trimStart().length;
      const nextItem = (BULLET.exec(next) ?? ORDERED.exec(next)) !== null;
      if (next.trim() && (nextIndent > baseIndent || nextItem)) {
        i += 1;
        continue;
      }
      break;
    }
    const current = items[items.length - 1];
    if (!current) break;
    if (indent > baseIndent) {
      current.sub.push(line.slice(Math.min(indent, baseIndent + 2)));
      i += 1;
      continue;
    }
    if (startsBlock(line)) break;
    // A lazy continuation of the item's text.
    if (current.sub.length) current.sub.push(line);
    else current.text.push(line.trim());
    i += 1;
  }
  return {
    block: {
      t: "list",
      ordered,
      start,
      items: items.map((item) => ({ text: item.text.join("\n"), children: item.sub.length ? parseLines(item.sub) : [] })),
    },
    next: i,
  };
}

/** Links that may be opened: http(s) and mailto only. */
export function safeHref(raw: string): string | null {
  const href = raw.trim().replace(/^<|>$/g, "");
  if (/^https?:\/\/[^\s]+$/i.test(href) || /^mailto:[^\s]+$/i.test(href)) return href;
  return null;
}

const URL_RE = /^https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"\]]/i;

/** Parse inline Markdown. */
export function parseInline(text: string): Inline[] {
  const out: Inline[] = [];
  let buffer = "";
  const flush = (): void => {
    if (buffer) out.push({ t: "text", v: buffer });
    buffer = "";
  };
  let i = 0;
  while (i < text.length) {
    const ch = text[i]!;
    const rest = text.slice(i);
    if (ch === "\\" && i + 1 < text.length && /[\\`*_~[\]()#|>-]/.test(text[i + 1]!)) {
      buffer += text[i + 1];
      i += 2;
      continue;
    }
    if (ch === "\n") {
      // Lines inside a paragraph keep their breaks (models write them on purpose).
      flush();
      out.push({ t: "br" });
      i += 1;
      continue;
    }
    if (ch === "`") {
      const ticks = /^`+/.exec(rest)?.[0] ?? "`";
      const end = text.indexOf(ticks, i + ticks.length);
      if (end > i) {
        flush();
        out.push({ t: "code", v: text.slice(i + ticks.length, end).replace(/^ (.*) $/, "$1") });
        i = end + ticks.length;
        continue;
      }
    }
    if (ch === "[") {
      const close = findClosing(text, i + 1, "[", "]");
      if (close > i && text[close + 1] === "(") {
        const end = findClosing(text, close + 2, "(", ")");
        if (end > close) {
          const target = text.slice(close + 2, end).trim().split(/\s+/)[0] ?? "";
          const href = safeHref(target);
          const label = text.slice(i + 1, close);
          flush();
          if (href) out.push({ t: "link", href, c: parseInline(label) });
          else out.push(...parseInline(label));
          i = end + 1;
          continue;
        }
      }
    }
    if ((ch === "h" || ch === "H") && /^https?:\/\//i.test(rest) && !/[\w/]$/.test(buffer)) {
      const match = URL_RE.exec(rest);
      if (match) {
        flush();
        out.push({ t: "link", href: match[0], c: [{ t: "text", v: match[0] }] });
        i += match[0].length;
        continue;
      }
    }
    const strong = delimited(text, i, ch === "*" ? "**" : ch === "_" ? "__" : "");
    if (strong) {
      flush();
      out.push({ t: "strong", c: parseInline(strong.inner) });
      i = strong.end;
      continue;
    }
    if (ch === "~") {
      const del = delimited(text, i, "~~");
      if (del) {
        flush();
        out.push({ t: "del", c: parseInline(del.inner) });
        i = del.end;
        continue;
      }
    }
    if (ch === "*" || (ch === "_" && !/\w/.test(text[i - 1] ?? ""))) {
      const em = delimited(text, i, ch);
      if (em && (ch === "*" || !/\w/.test(text[em.end] ?? ""))) {
        flush();
        out.push({ t: "em", c: parseInline(em.inner) });
        i = em.end;
        continue;
      }
    }
    buffer += ch;
    i += 1;
  }
  flush();
  return out;
}

function findClosing(text: string, from: number, open: string, close: string): number {
  let depth = 0;
  for (let i = from; i < text.length; i += 1) {
    const ch = text[i];
    if (ch === "\\") {
      i += 1;
    } else if (ch === "\n" && open === "(") {
      return -1;
    } else if (ch === open) {
      depth += 1;
    } else if (ch === close) {
      if (depth === 0) return i;
      depth -= 1;
    }
  }
  return -1;
}

/** `marker`…`marker` starting at `at`, not empty and not padded with spaces. */
function delimited(text: string, at: number, marker: string): { inner: string; end: number } | null {
  if (!marker || !text.startsWith(marker, at)) return null;
  const from = at + marker.length;
  const first = text[from];
  if (!first || /\s/.test(first) || text.startsWith(marker[0]!, from)) return null;
  let search = from;
  while (search < text.length) {
    const end = text.indexOf(marker, search);
    if (end < 0) return null;
    const inner = text.slice(from, end);
    // `**a*b**`-style nesting: skip a single marker char that is part of a longer run.
    if (inner && !/\s$/.test(inner) && !(marker.length === 1 && text[end + 1] === marker)) {
      if (!inner.includes("\n\n")) return { inner, end: end + marker.length };
      return null;
    }
    search = end + marker.length;
  }
  return null;
}

// ------------------------------------------------------------------ DOM

function inlineNodes(items: Inline[], parent: Node): void {
  for (const item of items) {
    switch (item.t) {
      case "text":
        parent.appendChild(document.createTextNode(item.v));
        break;
      case "br":
        parent.appendChild(document.createElement("br"));
        break;
      case "code": {
        const el = document.createElement("code");
        el.textContent = item.v;
        parent.appendChild(el);
        break;
      }
      case "strong":
      case "em":
      case "del": {
        const el = document.createElement(item.t === "strong" ? "strong" : item.t === "em" ? "em" : "del");
        inlineNodes(item.c, el);
        parent.appendChild(el);
        break;
      }
      case "link": {
        const el = document.createElement("a");
        el.href = item.href;
        el.target = "_blank";
        el.rel = "noopener noreferrer nofollow";
        inlineNodes(item.c, el);
        parent.appendChild(el);
        break;
      }
    }
  }
}

function inlineInto(el: HTMLElement, text: string): HTMLElement {
  inlineNodes(parseInline(text), el);
  return el;
}

export interface RenderOptions {
  /** Adds a copy button to code blocks. */
  onCopy?: (code: string) => void;
  /** Headings keep their level (a document); in a chat `#` is an h3. */
  fullHeadings?: boolean;
}

function blockNodes(blocks: Block[], parent: HTMLElement, options: RenderOptions): void {
  for (const block of blocks) {
    switch (block.t) {
      case "p":
        parent.appendChild(inlineInto(document.createElement("p"), block.text));
        break;
      case "h": {
        const level = Math.min(6, block.level + (options.fullHeadings ? 0 : 2));
        parent.appendChild(inlineInto(document.createElement(`h${level}` as "h3"), block.text));
        break;
      }
      case "hr":
        parent.appendChild(document.createElement("hr"));
        break;
      case "code": {
        const wrap = document.createElement("div");
        wrap.className = "md-code";
        const head = document.createElement("div");
        head.className = "md-code-head";
        const lang = document.createElement("span");
        lang.textContent = block.lang || "text";
        head.appendChild(lang);
        if (options.onCopy && !block.open) {
          const copy = document.createElement("button");
          copy.type = "button";
          copy.className = "md-code-copy";
          copy.textContent = "Copy";
          const code = block.v;
          copy.addEventListener("click", () => options.onCopy?.(code));
          head.appendChild(copy);
        }
        const pre = document.createElement("pre");
        const code = document.createElement("code");
        code.textContent = block.v;
        pre.appendChild(code);
        wrap.append(head, pre);
        parent.appendChild(wrap);
        break;
      }
      case "quote": {
        const el = document.createElement("blockquote");
        blockNodes(block.children, el, options);
        parent.appendChild(el);
        break;
      }
      case "list": {
        const el = document.createElement(block.ordered ? "ol" : "ul");
        if (block.ordered && block.start !== 1) (el as HTMLOListElement).start = block.start;
        for (const item of block.items) {
          const li = document.createElement("li");
          const task = /^\[([ xX])\]\s+/.exec(item.text);
          if (task) {
            li.className = "md-task";
            const box = document.createElement("span");
            box.className = `md-check${task[1] === " " ? "" : " is-done"}`;
            box.setAttribute("aria-label", task[1] === " " ? "not done" : "done");
            li.appendChild(box);
            inlineNodes(parseInline(item.text.slice(task[0].length)), li);
          } else {
            inlineNodes(parseInline(item.text), li);
          }
          if (item.children.length) blockNodes(item.children, li, options);
          el.appendChild(li);
        }
        parent.appendChild(el);
        break;
      }
      case "table": {
        const scroll = document.createElement("div");
        scroll.className = "md-table";
        const table = document.createElement("table");
        const thead = document.createElement("thead");
        const headRow = document.createElement("tr");
        block.head.forEach((cell, index) => {
          const th = inlineInto(document.createElement("th"), cell);
          if (block.align[index]) th.style.textAlign = block.align[index]!;
          headRow.appendChild(th);
        });
        thead.appendChild(headRow);
        const tbody = document.createElement("tbody");
        for (const row of block.rows) {
          const tr = document.createElement("tr");
          row.forEach((cell, index) => {
            const td = inlineInto(document.createElement("td"), cell);
            if (block.align[index]) td.style.textAlign = block.align[index]!;
            tr.appendChild(td);
          });
          tbody.appendChild(tr);
        }
        table.append(thead, tbody);
        scroll.appendChild(table);
        parent.appendChild(scroll);
        break;
      }
    }
  }
}

/** Render Markdown into `target` (its old content is replaced). */
export function renderMarkdown(target: HTMLElement, source: string, options: RenderOptions = {}): void {
  const fragment = document.createElement("div");
  blockNodes(parseMarkdown(source), fragment, options);
  target.replaceChildren(...Array.from(fragment.childNodes));
}
