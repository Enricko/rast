// Taskbar ordering.
//
// Pinned applications own fixed slots in the user's order. Opening a pinned
// application shows its window(s) in that slot instead of moving them to the
// end, so buttons never jump. Windows of applications that are not pinned
// follow the pinned slots in the order they were opened.

export interface TaskbarWindow {
  id: string;
  appId: string;
}

export type TaskbarSlot<W extends TaskbarWindow> =
  | { kind: "launcher"; appId: string }
  | { kind: "window"; win: W; pinned: boolean };

/**
 * @param pins      the account's pinned application ids, in order
 * @param windows   open windows, in opening order
 * @param canLaunch whether a pinned application may be shown as a launcher
 *                  (known to this client and allowed for the account)
 */
export function taskbarSlots<W extends TaskbarWindow>(
  pins: ReadonlyArray<string>,
  windows: ReadonlyArray<W>,
  canLaunch: (appId: string) => boolean,
): Array<TaskbarSlot<W>> {
  const slots: Array<TaskbarSlot<W>> = [];
  const placed = new Set<string>();
  for (const appId of pins) {
    if (placed.has(appId)) continue;
    placed.add(appId);
    const own = windows.filter((win) => win.appId === appId);
    if (own.length > 0) {
      for (const win of own) slots.push({ kind: "window", win, pinned: true });
    } else if (canLaunch(appId)) {
      slots.push({ kind: "launcher", appId });
    }
  }
  for (const win of windows) {
    if (!placed.has(win.appId)) slots.push({ kind: "window", win, pinned: false });
  }
  return slots;
}
