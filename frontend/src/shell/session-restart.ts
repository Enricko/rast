// Session restart warnings (ADR 0005, section 4). Apps and terminals keep the
// Linux groups they started with, so a change of shared folder access needs
// a new session: a revocation restarts it after a short warning, a grant
// only offers the restart.

/** `session.restart` event data and `GET /api/session/restart` `pending`. */
export interface SessionRestart {
  reason: "access_revoked" | "access_granted" | string;
  forced: boolean;
  deadlineMs?: number | null;
}

/** The warning text for a restart, `now` in ms since 1970. */
export function restartMessage(restart: SessionRestart, now: number): string {
  if (!restart.forced) {
    return "You can open a new shared folder. Restart your session to see it in your apps and terminals.";
  }
  const seconds = Math.max(0, Math.ceil(((restart.deadlineMs ?? now) - now) / 1000));
  const when = seconds > 0 ? `in ${seconds} s` : "now";
  return `Your access to a shared folder changed. Your apps and terminals restart ${when}: save your work.`;
}

/** How long the warning stays: until the restart, or until dismissed. */
export function restartToastTimeout(restart: SessionRestart, now: number): number {
  if (!restart.forced || !restart.deadlineMs) return 0;
  return Math.max(1000, restart.deadlineMs - now);
}
