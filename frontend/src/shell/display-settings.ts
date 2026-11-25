// "Display settings" dialog: the desktop's resolution (the browser window, in
// device pixels) and its scale, like an operating system's display settings.
// Changes apply immediately and are kept per device and screen (core/display.ts).

import { formatScale, getDisplay, onDisplayChange, setDisplayScale, type DisplayInfo } from "../core/display";
import { h } from "../core/dom";
import { modal } from "../core/ui";

let openClose: (() => void) | null = null;

export function openDisplaySettings(): void {
  if (openClose) return;

  const resolutionValue = h("div", { class: "display-value" });
  const desktopHint = h("span", { class: "field-hint" });
  const scaleSelect = h("select", { class: "select", ariaLabel: "Scale" });

  const render = (info: DisplayInfo): void => {
    resolutionValue.textContent = `${info.resolutionWidth} × ${info.resolutionHeight}`;
    desktopHint.textContent = `Follows the browser window. Desktop area: ${Math.round(info.width)} × ${Math.round(info.height)}.`;
    const options = info.options.map((scale) =>
      h(
        "option",
        { value: String(scale) },
        scale === info.recommended ? `${formatScale(scale)} (Recommended)` : formatScale(scale),
      ),
    );
    scaleSelect.replaceChildren(...options);
    scaleSelect.value = String(info.scale);
  };

  scaleSelect.addEventListener("change", () => {
    setDisplayScale(Number(scaleSelect.value));
  });

  const body = h(
    "div",
    { class: "display-settings" },
    h(
      "div",
      { class: "field" },
      h("span", { class: "field-label" }, "Resolution"),
      resolutionValue,
      desktopHint,
    ),
    h(
      "label",
      { class: "field" },
      h("span", { class: "field-label" }, "Scale"),
      scaleSelect,
      h("span", { class: "field-hint" }, "Browser zoom no longer changes the desktop. Use Scale to make everything bigger or smaller."),
    ),
  );

  render(getDisplay());
  const stopWatching = onDisplayChange(() => render(getDisplay()));

  openClose = modal({
    title: "Display",
    body,
    actions: [
      {
        label: "Use recommended",
        variant: "ghost",
        keepOpen: true,
        onClick: () => {
          setDisplayScale(getDisplay().recommended);
        },
      },
      { label: "Done", onClick: () => undefined },
    ],
    onClose: () => {
      stopWatching();
      openClose = null;
    },
  });
}
