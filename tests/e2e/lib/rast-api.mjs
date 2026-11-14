// HTTP client for the RAST API used by the E2E scenarios.
//
// Keeps the session cookie and CSRF token like a browser would (the login
// response returns the token; every mutation must send it as
// `X-CSRF-Token`). Uploads are streamed so the 147 MB Chrome .deb does not
// need to be fully buffered.

import { createReadStream, readFileSync, statSync } from "node:fs";

export class RastApiError extends Error {
  constructor(method, path, status, body) {
    const detail =
      body && typeof body === "object"
        ? body.error?.message || body.message || JSON.stringify(body).slice(0, 500)
        : String(body ?? "").slice(0, 500);
    super(`${method} ${path} -> HTTP ${status}: ${detail}`);
    this.name = "RastApiError";
    this.status = status;
    this.body = body;
    this.path = path;
    this.method = method;
  }
}

export class RastApi {
  constructor(baseUrl) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.cookies = new Map();
    this.csrf = null;
    this.user = null;
  }

  cookieHeader() {
    return [...this.cookies.entries()].map(([key, value]) => `${key}=${value}`).join("; ");
  }

  storeCookies(response) {
    const setCookies = response.headers.getSetCookie?.() ?? [];
    for (const raw of setCookies) {
      const pair = raw.split(";")[0];
      const index = pair.indexOf("=");
      if (index <= 0) continue;
      this.cookies.set(pair.slice(0, index), pair.slice(index + 1));
    }
  }

  async raw(method, path, { body, contentType, headers = {}, buffer, stream } = {}) {
    const requestHeaders = { ...headers };
    if (this.cookies.size > 0) requestHeaders.Cookie = this.cookieHeader();
    if (this.csrf) requestHeaders["X-CSRF-Token"] = this.csrf;
    let payload;
    let duplex;
    if (stream) {
      requestHeaders["Content-Type"] = contentType || "application/octet-stream";
      requestHeaders["Content-Length"] = String(stream.size);
      payload = stream.body;
      duplex = "half";
    } else if (buffer) {
      requestHeaders["Content-Type"] = contentType || "application/octet-stream";
      requestHeaders["Content-Length"] = String(buffer.length);
      payload = buffer;
    } else if (body !== undefined) {
      requestHeaders["Content-Type"] = contentType || "application/json";
      payload = JSON.stringify(body);
    }
    const response = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: requestHeaders,
      body: payload,
      duplex,
      redirect: "manual",
    });
    this.storeCookies(response);
    const text = await response.text();
    let json = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      /* non-JSON response (downloads, HTML) */
    }
    return { status: response.status, headers: response.headers, text, json };
  }

  async request(method, path, options = {}) {
    const result = await this.raw(method, path, options);
    if (result.status < 200 || result.status >= 300) {
      throw new RastApiError(method, path, result.status, result.json ?? result.text);
    }
    return result.json;
  }

  get(path) {
    return this.request("GET", path);
  }

  post(path, body) {
    return this.request("POST", path, body === undefined ? {} : { body });
  }

  patch(path, body) {
    return this.request("PATCH", path, { body });
  }

  delete(path) {
    return this.request("DELETE", path);
  }

  async login(username, password) {
    const result = await this.request("POST", "/api/auth/login", { body: { username, password } });
    this.csrf = result.csrfToken;
    this.user = result.user;
    return result;
  }

  async loginAs(credentials) {
    return this.login(credentials.username, credentials.password);
  }

  async logout() {
    try {
      await this.post("/api/auth/logout");
    } finally {
      this.cookies.clear();
      this.csrf = null;
      this.user = null;
    }
  }

  // ---- users -------------------------------------------------------------
  listUsers(query = "") {
    return this.get(`/api/users?perPage=100${query ? `&q=${encodeURIComponent(query)}` : ""}`);
  }

  async createUser({ username, password, role = "user", displayName }) {
    return this.post("/api/users", { username, password, role, displayName });
  }

  async ensureUser({ username, password, role = "user", displayName }) {
    const listing = await this.listUsers(username);
    const existing = listing.items.find((item) => item.username === username);
    if (existing) return existing;
    return this.createUser({ username, password, role, displayName });
  }

  // ---- files -------------------------------------------------------------
  /** Upload a local file into the caller's File Station at `dir`. */
  async uploadFile(localPath, { dir = "/", name, overwrite = true } = {}) {
    const fileName = name || localPath.split("/").pop();
    const size = statSync(localPath).size;
    const path = `/api/files/upload?dir=${encodeURIComponent(dir)}&name=${encodeURIComponent(
      fileName,
    )}&overwrite=${overwrite ? "true" : "false"}`;
    return this.request("POST", path, {
      stream: { body: createReadStream(localPath), size },
      contentType: "application/octet-stream",
    });
  }

  // ---- Linux apps --------------------------------------------------------
  listApps() {
    return this.get("/api/linux-apps");
  }

  async findApp(predicate) {
    const listing = await this.listApps();
    return listing.apps.find(predicate) ?? null;
  }

  getApp(id) {
    return this.get(`/api/linux-apps/${encodeURIComponent(id)}`);
  }

  installFromPath(path) {
    return this.post("/api/linux-apps/install", { path });
  }

  installFromUrl(url) {
    return this.post("/api/linux-apps/install", { url });
  }

  updateApp(id) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/update`);
  }

  checkApp(id) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/check`);
  }

  uninstall(id, removeData = false) {
    return this.delete(
      `/api/linux-apps/${encodeURIComponent(id)}?removeData=${removeData ? "true" : "false"}`,
    );
  }

  setVisibility(id, visibility) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/visibility`, { visibility });
  }

  openApp(id) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/open`);
  }

  // Docker/legacy per-copy sessions.
  session(sessionId) {
    return this.get(`/api/linux-apps/sessions/${encodeURIComponent(sessionId)}`);
  }

  closeSession(sessionId) {
    return this.post(`/api/linux-apps/sessions/${encodeURIComponent(sessionId)}/close`);
  }

  sessionWindows(sessionId) {
    return this.get(`/api/linux-apps/sessions/${encodeURIComponent(sessionId)}/windows`);
  }

  streamWindow(sessionId, windowId) {
    return this.post(
      `/api/linux-apps/sessions/${encodeURIComponent(sessionId)}/windows/${encodeURIComponent(
        windowId,
      )}/stream`,
    );
  }

  closeSessionWindow(sessionId, windowId) {
    return this.post(
      `/api/linux-apps/sessions/${encodeURIComponent(sessionId)}/windows/${encodeURIComponent(
        windowId,
      )}/close`,
    );
  }

  // Native backend.
  preview({ path, url }) {
    return this.post("/api/linux-apps/preview", path ? { path } : { url });
  }

  launch(id) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/launch`);
  }

  /** Native session (from GET /api/linux-apps {session} or the dedicated endpoint). */
  async nativeSessionState() {
    try {
      const state = await this.nativeSession();
      return { session: state?.session ?? null, limits: state?.limits ?? null, source: "session" };
    } catch (error) {
      if (error.status !== 404) throw error;
      const listing = await this.listApps();
      return { session: listing.session ?? null, limits: listing.limits ?? null, source: "list" };
    }
  }

  nativeSession() {
    return this.get("/api/linux-apps/session");
  }

  closeNativeSession() {
    return this.post("/api/linux-apps/session/close");
  }

  closeNativeRun(runId) {
    return this.post(`/api/linux-apps/session/runs/${encodeURIComponent(runId)}`);
  }

  migrate(id, { path, url } = {}) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/migrate`, path ? { path } : url ? { url } : {});
  }

  rollbackMigration(id) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/migration/rollback`);
  }

  cleanupMigration(id, removeBackup = false) {
    return this.post(`/api/linux-apps/${encodeURIComponent(id)}/migration/cleanup`, {
      removeBackup,
    });
  }

  // ---- jobs --------------------------------------------------------------
  job(jobId) {
    return this.get(`/api/jobs/${encodeURIComponent(jobId)}`);
  }

  async waitJob(jobId, { timeoutMs = 15 * 60_000, intervalMs = 1000, onTick } = {}) {
    const started = Date.now();
    for (;;) {
      const job = await this.job(jobId);
      onTick?.(job);
      // The jobs API reports "done" for success; accept the other spellings too.
      if (["done", "succeeded", "success", "completed"].includes(job.status)) {
        return job;
      }
      if (["failed", "cancelled", "canceled"].includes(job.status)) {
        const error = new Error(`job ${jobId} ${job.status}: ${job.error || job.message || ""}`);
        error.job = job;
        throw error;
      }
      if (Date.now() - started > timeoutMs) {
        throw new Error(`job ${jobId} did not finish within ${timeoutMs} ms (last: ${job.status})`);
      }
      await sleep(intervalMs);
    }
  }

  /** Poll one app until `predicate(app)` is true. */
  async waitApp(id, predicate, { timeoutMs = 15 * 60_000, intervalMs = 1000, onTick } = {}) {
    const started = Date.now();
    for (;;) {
      const app = await this.getApp(id);
      onTick?.(app);
      if (predicate(app)) return app;
      if (Date.now() - started > timeoutMs) {
        throw new Error(
          `app ${id} did not reach the expected state within ${timeoutMs} ms (last: ${JSON.stringify(
            app,
          ).slice(0, 600)})`,
        );
      }
      await sleep(intervalMs);
    }
  }

  async waitAppReady(id, options = {}) {
    return this.waitApp(
      id,
      (app) => app.status === "ready" && (app.build ?? 0) > 0,
      options,
    );
  }

  /** Find an installed app by file name (or id) and install it when missing. */
  async ensureDebInstalled(localPath, { name, timeoutMs } = {}) {
    const fileName = name || localPath.split("/").pop();
    let app = await this.findApp(
      (candidate) => candidate.fileName === fileName || candidate.id === fileName.replace(/\.[^.]+$/, ""),
    );
    if (app) return { app, installedNow: false };
    await this.uploadFile(localPath, { name: fileName });
    const started = await this.installFromPath(`/${fileName}`);
    if (started.jobId) await this.waitJob(started.jobId, { timeoutMs });
    if (started.app) {
      app = await this.getApp(started.app.id ?? started.app);
    } else {
      // Native installs answer {"jobId","installing"} and the app row appears
      // while the job runs: wait for it by file name.
      const startedAt = Date.now();
      for (;;) {
        app = await this.findApp((candidate) => candidate.fileName === fileName);
        if (app) break;
        if (Date.now() - startedAt > 60_000) throw new Error(`install job finished without an app row for ${fileName}`);
        await sleep(500);
      }
    }
    return { app, installedNow: true };
  }

  /** Wait until an app is visible to non-admins (shared, ready). */
  async waitAppVisibleToUsers(id, { timeoutMs = 30_000 } = {}) {
    const started = Date.now();
    for (;;) {
      const app = await this.getApp(id);
      if (app.status === "ready" && app.visibility === "users") return app;
      if (Date.now() - started > timeoutMs) throw new Error(`app ${id} is not user-visible`);
      await sleep(500);
    }
  }
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** A pair of clients sharing no session state. */
export async function loginClients(config) {
  const admin = new RastApi(config.baseUrl);
  await admin.loginAs(config.admin);
  const users = [];
  for (const credentials of config.users) {
    const client = new RastApi(config.baseUrl);
    await client.loginAs(credentials);
    users.push(client);
  }
  return { admin, users };
}

export function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}
