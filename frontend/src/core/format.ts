// Formatting helpers shared by every app.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

export function formatBytes(bytes: number | undefined | null, digits = 1): string {
  if (bytes === undefined || bytes === null || Number.isNaN(bytes)) return "—";
  if (bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded = value >= 100 ? value.toFixed(0) : value.toFixed(digits);
  return `${rounded} ${UNITS[unit]}`;
}

/** Fixed-width byte count for table columns (e.g. "1.4 GiB"). */
export function formatBytesShort(bytes: number | undefined | null): string {
  return formatBytes(bytes, bytes !== null && bytes !== undefined && bytes < 10 * 1024 ? 0 : 1);
}

export function formatPercent(value: number | undefined | null, digits = 0): string {
  if (value === undefined || value === null || Number.isNaN(value)) return "—";
  return `${value.toFixed(digits)}%`;
}

export function formatSpeed(bytesPerSec: number | undefined | null): string {
  if (!bytesPerSec || bytesPerSec <= 0) return "0 B/s";
  return `${formatBytes(bytesPerSec, 1)}/s`;
}

export function formatDuration(seconds: number | undefined | null): string {
  if (seconds === undefined || seconds === null || seconds < 0) return "—";
  const s = Math.floor(seconds);
  const days = Math.floor(s / 86400);
  const hours = Math.floor((s % 86400) / 3600);
  const minutes = Math.floor((s % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${s % 60}s`;
  return `${s}s`;
}

export function pad2(value: number): string {
  return value.toString().padStart(2, "0");
}

export function formatDateTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  const date = new Date(ts);
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${pad2(
    date.getHours(),
  )}:${pad2(date.getMinutes())}`;
}

export function formatTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  const date = new Date(ts);
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}

export function formatClock(ts: number): string {
  const date = new Date(ts);
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

export function formatDate(ts: number | null | undefined): string {
  if (!ts) return "—";
  const date = new Date(ts);
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`;
}

export function relativeTime(ts: number | null | undefined): string {
  if (!ts) return "never";
  const diff = Date.now() - ts;
  if (diff < 0) return "just now";
  const seconds = Math.floor(diff / 1000);
  if (seconds < 45) return "just now";
  // 45–59 s rounds down to 0 minutes; say "1 min ago", never "0 min ago".
  const minutes = Math.max(1, Math.floor(seconds / 60));
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} d ago`;
  return formatDate(ts);
}

export function formatCount(value: number | undefined | null): string {
  if (value === undefined || value === null) return "—";
  return value.toLocaleString();
}

export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function formatAge(ts: number | null | undefined): string {
  return relativeTime(ts);
}

export function formatEta(secs: number | undefined | null): string {
  if (!secs || secs <= 0 || !Number.isFinite(secs)) return "";
  return formatDuration(secs);
}

/** "2024-05-01 14:32:07" — used in audit log detail views. */
export function formatDateTimeSec(ts: number | null | undefined): string {
  if (!ts) return "—";
  return `${formatDate(ts)} ${formatTime(ts)}`;
}

/** "Mon, 1 May 2024" — used by the taskbar clock popover. */
export function formatWeekdayDate(ts: number): string {
  const date = new Date(ts);
  const weekday = date.toLocaleDateString(undefined, { weekday: "short" });
  const rest = date.toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
  return `${weekday}, ${rest}`;
}

/** "14:32:07" */
export function formatClockSeconds(ts: number): string {
  return formatTime(ts);
}

/** 1234567 → "1.2M" (compact counters for badges). */
export function formatNumberCompact(value: number | undefined | null): string {
  if (value === undefined || value === null) return "—";
  if (Math.abs(value) < 1000) return String(value);
  if (Math.abs(value) < 1_000_000) return `${(value / 1000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  if (Math.abs(value) < 1_000_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  return `${(value / 1_000_000_000).toFixed(1)}B`;
}

export function startOfDay(ts: number): number {
  const date = new Date(ts);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

export function endOfDay(ts: number): number {
  const date = new Date(ts);
  date.setHours(23, 59, 59, 999);
  return date.getTime();
}
