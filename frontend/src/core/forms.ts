// Form primitives shared by the auth screens and admin apps: validation that
// mirrors the server-side policy, password strength feedback and small helpers
// for keeping inline errors accessible.

import { h } from "./dom";

export const PASSWORD_MIN_LENGTH = 10;
export const PASSWORD_MAX_BYTES = 256;

const GIB = 1024 * 1024 * 1024;

let fieldSeq = 0;

// ---------------------------------------------------------------------------
// Password policy (mirrors src/auth/password.rs)
// ---------------------------------------------------------------------------

export interface PasswordCheck {
  id: "length" | "whitespace" | "classes" | "digits" | "bytes";
  label: string;
  ok: boolean;
}

export function passwordChecks(value: string): PasswordCheck[] {
  const length = [...value].length;
  const classes = [
    /[a-z]/.test(value),
    /[A-Z]/.test(value),
    /\d/.test(value),
    /[^a-zA-Z0-9]/.test(value),
  ].filter(Boolean).length;
  const allDigits = length > 0 && /^\d+$/.test(value);
  const noOuterWhitespace = value.length > 0 && value.trim() === value;
  return [
    { id: "length", label: `At least ${PASSWORD_MIN_LENGTH} characters`, ok: length >= PASSWORD_MIN_LENGTH },
    {
      id: "classes",
      label: "Mix at least three of: lowercase, uppercase, digits, symbols",
      ok: classes >= 3,
    },
    { id: "digits", label: "Not only digits", ok: !allDigits },
    { id: "whitespace", label: "No leading or trailing spaces", ok: noOuterWhitespace },
    { id: "bytes", label: "At most 256 bytes", ok: new TextEncoder().encode(value).length <= PASSWORD_MAX_BYTES },
  ];
}

/** 0 (empty/very weak) .. 4 (strong). */
export function passwordScore(value: string): number {
  if (!value) return 0;
  const checks = passwordChecks(value);
  const passed = checks.filter((check) => check.ok).length;
  const length = [...value].length;
  let score = Math.min(4, Math.floor((passed / checks.length) * 3));
  if (length >= 16) score += 1;
  if (length >= 12 && /[^a-zA-Z0-9]/.test(value) && /\d/.test(value)) score += 1;
  return Math.max(0, Math.min(4, score));
}

export function passwordStrengthLabel(score: number): string {
  switch (score) {
    case 0:
      return "Empty";
    case 1:
      return "Weak";
    case 2:
      return "Fair";
    case 3:
      return "Good";
    default:
      return "Strong";
  }
}

/** Returns the first policy violation, or null when the password is valid. */
export function validatePassword(value: string): string | null {
  if (!value) return "Password is required";
  const failed = passwordChecks(value).find((check) => !check.ok);
  return failed ? failed.label : null;
}

/** Mirrors users::validate_username on the server. */
export function validateUsername(value: string): string | null {
  const trimmed = value.trim();
  if (!trimmed) return "Username is required";
  if (trimmed.length > 32) return "Username must be at most 32 characters";
  if (!/^[a-zA-Z0-9]/.test(trimmed)) return "Username must start with a letter or digit";
  if (!/^[a-zA-Z0-9._-]+$/.test(trimmed)) {
    return "Use only letters, digits, dot, dash and underscore";
  }
  if (trimmed === "." || trimmed === ".." || trimmed.endsWith(".")) return "Invalid username";
  return null;
}

export function validateRequiredText(value: string, label: string, max = 200): string | null {
  const trimmed = value.trim();
  if (!trimmed) return `${label} is required`;
  if ([...trimmed].length > max) return `${label} must be at most ${max} characters`;
  return null;
}

// ---------------------------------------------------------------------------
// Byte helpers (quotas are expressed in GiB in the UI)
// ---------------------------------------------------------------------------

export function gibToBytes(gib: number): number {
  if (!Number.isFinite(gib) || gib < 0) return 0;
  return Math.round(gib * GIB);
}

export function bytesToGib(bytes: number): number {
  if (!Number.isFinite(bytes) || bytes <= 0) return 0;
  return Math.round((bytes / GIB) * 100) / 100;
}

export function parseGib(value: string): number {
  const normalized = value.trim().replace(",", ".");
  if (normalized === "") return 0;
  const parsed = Number(normalized);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : 0;
}

export function validateGib(value: string, options: { allowZero?: boolean; maxGib?: number } = {}): string | null {
  const normalized = value.trim().replace(",", ".");
  if (normalized === "") return "Enter a number of GiB (0 = unlimited)";
  const parsed = Number(normalized);
  if (!Number.isFinite(parsed) || parsed < 0) return "Enter a positive number of GiB";
  if (!options.allowZero && parsed === 0) return "Must be greater than zero";
  if (parsed > (options.maxGib ?? 1024 * 1024)) return "Quota is too large";
  return null;
}

// ---------------------------------------------------------------------------
// Accessible form fields
// ---------------------------------------------------------------------------

export interface FormFieldOptions<T extends HTMLElement> {
  label: string;
  control: T;
  hint?: string;
  required?: boolean;
  /** Extra content rendered under the control (e.g. a strength meter). */
  extra?: Node;
}

export interface FormField<T extends HTMLElement> {
  el: HTMLElement;
  control: T;
  hintEl: HTMLElement | null;
  errorEl: HTMLElement;
  setError(message: string | null): void;
  clear(): void;
}

export function formField<T extends HTMLElement>(options: FormFieldOptions<T>): FormField<T> {
  fieldSeq += 1;
  const hintId = `field-hint-${fieldSeq}`;
  const errorId = `field-error-${fieldSeq}`;
  const control = options.control;
  const hintEl = options.hint ? h("span", { class: "field-hint", id: hintId }, options.hint) : null;
  const errorEl = h("div", { class: "field-error", id: errorId, hidden: true });
  const describedBy = [hintEl ? hintId : null, errorId].filter(Boolean).join(" ");
  if (control instanceof HTMLInputElement || control instanceof HTMLSelectElement || control instanceof HTMLTextAreaElement) {
    control.setAttribute("aria-describedby", describedBy);
    if (options.required) control.setAttribute("aria-required", "true");
  }
  const el = h(
    "div",
    { class: "field" },
    h("label", { class: "field" }, h("span", { class: "field-label" }, options.label, options.required ? h("span", { class: "field-required", ariaLabel: "required" }, " *") : null), control),
    hintEl,
    options.extra ?? null,
    errorEl,
  );
  const field: FormField<T> = {
    el,
    control,
    hintEl,
    errorEl,
    setError(message: string | null) {
      if (!message) {
        errorEl.hidden = true;
        errorEl.textContent = "";
        control.classList.remove("has-error");
        control.removeAttribute("aria-invalid");
        return;
      }
      errorEl.hidden = false;
      errorEl.textContent = message;
      control.classList.add("has-error");
      control.setAttribute("aria-invalid", "true");
    },
    clear() {
      field.setError(null);
    },
  };
  control.addEventListener("input", () => field.setError(null));
  control.addEventListener("change", () => field.setError(null));
  return field;
}

export interface PasswordFieldOptions {
  label?: string;
  autocomplete?: "current-password" | "new-password" | "off";
  placeholder?: string;
  hint?: string;
  showStrength?: boolean;
  required?: boolean;
  /** Show a reveal (eye) toggle, default true. */
  revealToggle?: boolean;
}

export interface PasswordField {
  el: HTMLElement;
  input: HTMLInputElement;
  setError(message: string | null): void;
  clear(): void;
  value(): string;
  reset(): void;
}

export function passwordField(options: PasswordFieldOptions = {}): PasswordField {
  const input = h("input", {
    class: "input",
    type: "password",
    autocomplete: options.autocomplete ?? "new-password",
    placeholder: options.placeholder ?? "",
    ariaLabel: options.label ?? "Password",
  });
  const message = h("span", { class: "field-hint strength-label" });
  const meter = h("div", { class: "strength", ariaHidden: true });
  const bars = Array.from({ length: 4 }, () => h("span", { class: "strength-bar" }));
  meter.append(...bars);

  const checkList = h("ul", { class: "password-checks" });
  const checks = passwordChecks("");
  const checkEls = checks.map((check) => {
    const item = h(
      "li",
      { class: `check${check.ok ? " is-ok" : ""}` },
      h("span", { class: "check-mark", ariaHidden: true }),
      h("span", {}, check.label),
    );
    checkList.appendChild(item);
    return { id: check.id, item };
  });

  const update = () => {
    const value = input.value;
    const score = passwordScore(value);
    const strength = passwordStrengthLabel(score);
    message.textContent = value ? `Strength: ${strength}` : "";
    meter.dataset.score = String(score);
    for (const [index, bar] of bars.entries()) {
      bar.classList.toggle("is-on", index < score);
    }
    const current = passwordChecks(value);
    for (const entry of checkEls) {
      const check = current.find((candidate) => candidate.id === entry.id);
      entry.item.classList.toggle("is-ok", Boolean(check?.ok));
    }
    checkList.classList.toggle("is-visible", value.length > 0);
  };
  input.addEventListener("input", update);

  const controlWrap = h("div", { class: "input-with-action" }, input);
  if (options.revealToggle !== false) {
    const reveal = h(
      "button",
      { class: "input-action", type: "button", ariaLabel: "Show password", title: "Show password" },
      "Show",
    );
    reveal.addEventListener("click", () => {
      const showing = input.type === "text";
      input.type = showing ? "password" : "text";
      reveal.textContent = showing ? "Show" : "Hide";
      reveal.setAttribute("aria-label", showing ? "Show password" : "Hide password");
      input.focus();
    });
    controlWrap.appendChild(reveal);
  }

  const wrapper = h("div", { class: "password-field" }, controlWrap);
  const extraNodes: Node[] = [];
  if (options.showStrength !== false) extraNodes.push(meter, message, checkList);
  const field = formField({
    label: options.label ?? "Password",
    control: input,
    ...(options.hint ? { hint: options.hint } : {}),
    ...(options.required ? { required: true } : {}),
    ...(extraNodes.length > 0 ? { extra: h("div", { class: "password-extra" }, ...extraNodes) } : {}),
  });
  wrapper.appendChild(field.el);
  return {
    el: wrapper,
    input,
    setError: field.setError,
    clear: field.clear,
    value: () => input.value,
    reset: () => {
      input.value = "";
      update();
      field.clear();
    },
  };
}

/** Run an async task with a button in a busy state; restores the label after. */
export async function withButtonBusy<T>(
  button: HTMLButtonElement,
  busyLabel: string,
  task: () => Promise<T>,
): Promise<T> {
  const originalNodes = Array.from(button.childNodes);
  button.disabled = true;
  button.setAttribute("aria-busy", "true");
  button.replaceChildren(document.createTextNode(busyLabel));
  try {
    return await task();
  } finally {
    button.disabled = false;
    button.removeAttribute("aria-busy");
    button.replaceChildren(...originalNodes);
  }
}
