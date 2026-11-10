// Local API shapes and helpers for File Station. Shared DTOs live in core/types.

import type { FileEntry, FileListing } from "../../core/types";

/** GET /api/files/usage */
export interface UsageStats {
  quotaBytes: number;
  usedBytes: number;
  trashBytes: number;
  byType?: Record<string, number>;
}

/** GET /api/files/trash?page= */
export interface TrashPage {
  items: TrashItem[];
  total: number;
  page?: number;
  perPage?: number;
}

export interface TrashItem {
  id: string;
  originalPath: string;
  sizeBytes: number;
  kind: "file" | "dir";
  deletedAt: number;
}

/** Responses of copy/move/delete operations. */
export interface JobRef {
  jobId: string;
}

/** GET /api/files/text */
export interface TextPayload {
  content: string;
  truncated: boolean;
  encoding: string;
}

/** An upload may fail with 409 when the target name exists. */
export interface ConflictInfo {
  name: string;
}

export type PreviewKind = "image" | "text" | "pdf";

const TEXT_MIME_PREFIXES = [
  "text/",
  "application/json",
  "application/xml",
  "application/javascript",
  "application/x-javascript",
  "application/x-yaml",
  "application/yaml",
  "application/toml",
];

const TEXT_EXTENSIONS = [
  "txt",
  "md",
  "markdown",
  "log",
  "json",
  "yaml",
  "yml",
  "toml",
  "ini",
  "cfg",
  "conf",
  "env",
  "csv",
  "tsv",
  "xml",
  "html",
  "htm",
  "css",
  "js",
  "mjs",
  "cjs",
  "ts",
  "jsx",
  "tsx",
  "rs",
  "py",
  "sh",
  "bash",
  "zsh",
  "sql",
  "go",
  "java",
  "c",
  "h",
  "cpp",
  "hpp",
];

export function previewKind(entry: FileEntry): PreviewKind | null {
  if (entry.kind !== "file") return null;
  const mime = (entry.mime || "").toLowerCase();
  if (mime.startsWith("image/")) return "image";
  if (mime === "application/pdf") return "pdf";
  if (TEXT_MIME_PREFIXES.some((prefix) => mime.startsWith(prefix))) return "text";
  const ext = extension(entry.name);
  if (ext && TEXT_EXTENSIONS.includes(ext)) return "text";
  return null;
}

export function extension(name: string): string {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return "";
  return name.slice(dot + 1).toLowerCase();
}

/** Join a logical directory path and a child name. */
export function joinPath(parent: string, name: string): string {
  const clean = parent === "/" ? "" : parent.replace(/\/+$/, "");
  return `${clean}/${name}`;
}

export function baseName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index >= 0 ? trimmed.slice(index + 1) : trimmed;
}

export function parentPath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index <= 0 ? "/" : trimmed.slice(0, index);
}

/** File names that are valid on the server: no separators or control chars. */
export function validateFileName(value: string): string | null {
  const name = value.trim();
  if (!name) return "Name cannot be empty";
  if (name === "." || name === "..") return "This name is reserved";
  if (name.length > 255) return "Name must be 255 characters or fewer";
  if (/[/\\]/.test(name)) return "Name cannot contain / or \\";
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f]/.test(name)) return "Name cannot contain control characters";
  return null;
}

/** Sort entries client-side; used for optimistic ordering inside a page. */
export function sortEntries(
  entries: FileEntry[],
  key: "name" | "size" | "modified",
  order: "asc" | "desc",
): FileEntry[] {
  const direction = order === "asc" ? 1 : -1;
  return [...entries].sort((a, b) => {
    // Directories always come first for the "name" sort, like every file manager.
    if (key === "name") {
      if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
      return a.name.localeCompare(b.name, undefined, { sensitivity: "base", numeric: true }) * direction;
    }
    const left = key === "size" ? a.size : a.modifiedAt;
    const right = key === "size" ? b.size : b.modifiedAt;
    if (left === right) return a.name.localeCompare(b.name);
    return (left - right) * direction;
  });
}

/** Whether this listing response looks usable (defensive against older backends). */
export function listingItems(listing: FileListing | null): FileEntry[] {
  return Array.isArray(listing?.items) ? listing.items : [];
}
