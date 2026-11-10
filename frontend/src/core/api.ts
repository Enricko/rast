// HTTP client for the RAST API: CSRF handling, typed errors, streamed uploads.
//
// Error contract (matches docs/API.md):
//   {error: {code, message}} with HTTP status 400/401/403/404/409/413/429/503.
//   429 responses may carry a Retry-After header; it is exposed as
//   ApiError.retryAfterSecs so the UI can show a precise countdown.

import type { FileEntry } from "./types";

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  /** Seconds to wait before retrying (from Retry-After on 429), when present. */
  readonly retryAfterSecs: number | null;

  constructor(status: number, code: string, message: string, retryAfterSecs: number | null = null) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.retryAfterSecs = retryAfterSecs;
  }

  get isUnauthorized(): boolean {
    return this.status === 401;
  }

  get isForbidden(): boolean {
    return this.status === 403;
  }

  get isNotFound(): boolean {
    return this.status === 404;
  }

  get isConflict(): boolean {
    return this.status === 409;
  }

  get isRateLimited(): boolean {
    return this.status === 429;
  }

  get isNotImplemented(): boolean {
    return this.status === 501 || this.code === "not_implemented";
  }

  get isUnavailable(): boolean {
    return this.status === 503 || this.code === "unavailable";
  }

  get isNetworkError(): boolean {
    return this.status === 0;
  }

  get isServerError(): boolean {
    return this.status >= 500;
  }
}

export interface RequestOptions {
  signal?: AbortSignal;
  /** Do not throw on 401 (useful for probing). */
  allowUnauthorized?: boolean;
}

/** Human message for API errors, with optional retry countdown for 429. */
export function apiErrorMessage(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.isRateLimited) {
      const secs = error.retryAfterSecs;
      if (secs && secs > 0) {
        const mins = Math.ceil(secs / 60);
        return `${error.message} (try again in ${mins > 1 ? `${mins} minutes` : `${secs}s`})`;
      }
    }
    return error.message;
  }
  if (error instanceof Error) return error.message;
  return String(error);
}

/** True when an optional endpoint is missing or temporarily unavailable. */
export function isFeatureUnavailable(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    (error.isNotImplemented || error.isUnavailable || error.status === 0)
  );
}

function buildQuery(params: Record<string, unknown> | undefined): string {
  if (!params) return "";
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const text = search.toString();
  return text ? `?${text}` : "";
}

function parseRetryAfter(header: string | null): number | null {
  if (!header) return null;
  const seconds = Number(header.trim());
  if (Number.isFinite(seconds) && seconds >= 0) return Math.ceil(seconds);
  const date = Date.parse(header);
  if (Number.isFinite(date)) return Math.max(0, Math.ceil((date - Date.now()) / 1000));
  return null;
}

export class ApiClient {
  private csrfToken = "";
  private readonly listeners = new Set<(event: "unauthorized") => void>();

  setCsrfToken(token: string): void {
    this.csrfToken = token;
  }

  getCsrfToken(): string {
    return this.csrfToken;
  }

  onSessionLost(listener: (event: "unauthorized") => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private notifySessionLost(): void {
    for (const listener of this.listeners) {
      try {
        listener("unauthorized");
      } catch {
        /* ignore listener errors */
      }
    }
  }

  async request<T>(
    method: string,
    path: string,
    body?: unknown,
    options: RequestOptions = {},
  ): Promise<T> {
    const headers: Record<string, string> = { Accept: "application/json" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (method !== "GET" && method !== "HEAD") headers["X-CSRF-Token"] = this.csrfToken;

    let response: Response;
    try {
      response = await fetch(path, {
        method,
        headers,
        credentials: "same-origin",
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: options.signal,
      });
    } catch (error) {
      if ((error as Error).name === "AbortError") throw error;
      throw new ApiError(0, "network_error", "Cannot reach the RAST server");
    }

    if (response.status === 401 && !options.allowUnauthorized) {
      this.notifySessionLost();
    }
    if (response.status === 204) return undefined as T;

    const contentType = response.headers.get("content-type") ?? "";
    const isJson = contentType.includes("application/json");
    const payload = isJson ? await response.json().catch(() => null) : await response.text();

    if (!response.ok) {
      const err = (payload as { error?: { code?: string; message?: string } } | null)?.error;
      throw new ApiError(
        response.status,
        err?.code ?? "http_error",
        err?.message ?? `Request failed (${response.status})`,
        parseRetryAfter(response.headers.get("retry-after")),
      );
    }
    return payload as T;
  }

  get<T>(path: string, query?: Record<string, unknown>, options?: RequestOptions): Promise<T> {
    return this.request<T>("GET", path + buildQuery(query), undefined, options);
  }

  post<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
    return this.request<T>("POST", path, body ?? {}, options);
  }

  patch<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>("PATCH", path, body ?? {});
  }

  put<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>("PUT", path, body ?? {});
  }

  del<T>(path: string, query?: Record<string, unknown>): Promise<T> {
    return this.request<T>("DELETE", path + buildQuery(query));
  }

  /** Upload a file as a raw stream with progress reporting (XHR for progress). */
  upload(
    dir: string,
    file: File,
    handlers: {
      onProgress?: (loaded: number, total: number) => void;
      overwrite?: boolean;
      signal?: AbortSignal;
      name?: string;
    } = {},
  ): Promise<FileEntry> {
    const params = new URLSearchParams({ dir, name: handlers.name ?? file.name });
    if (handlers.overwrite) params.set("overwrite", "true");
    return new Promise<FileEntry>((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open("POST", `/api/files/upload?${params.toString()}`);
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      xhr.setRequestHeader("X-CSRF-Token", this.csrfToken);
      xhr.withCredentials = true;
      xhr.upload.onprogress = (event) => {
        if (event.lengthComputable && handlers.onProgress) {
          handlers.onProgress(event.loaded, event.total);
        }
      };
      xhr.onload = () => {
        let payload: unknown = null;
        try {
          payload = JSON.parse(xhr.responseText);
        } catch {
          payload = xhr.responseText;
        }
        if (xhr.status >= 200 && xhr.status < 300) {
          resolve(payload as FileEntry);
        } else {
          const err = (payload as { error?: { code?: string; message?: string } } | null)?.error;
          if (xhr.status === 401) this.notifySessionLost();
          reject(
            new ApiError(
              xhr.status,
              err?.code ?? "upload_failed",
              err?.message ?? `Upload failed (${xhr.status})`,
              parseRetryAfter(xhr.getResponseHeader("retry-after")),
            ),
          );
        }
      };
      xhr.onerror = () => reject(new ApiError(0, "network_error", "Upload connection failed"));
      xhr.onabort = () => reject(new ApiError(0, "aborted", "Upload cancelled"));
      if (handlers.signal) {
        handlers.signal.addEventListener("abort", () => xhr.abort(), { once: true });
      }
      xhr.send(file);
    });
  }

  downloadUrl(path: string): string {
    return `/api/files/download?path=${encodeURIComponent(path)}`;
  }

  previewUrl(path: string): string {
    return `/api/files/preview?path=${encodeURIComponent(path)}`;
  }

  /** Trigger a browser download without exposing credentials in the URL. */
  download(path: string): void {
    const anchor = document.createElement("a");
    anchor.href = this.downloadUrl(path);
    anchor.rel = "noopener";
    anchor.click();
  }
}
