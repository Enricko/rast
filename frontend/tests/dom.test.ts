import assert from "node:assert/strict";
import test from "node:test";

import { isElementNode } from "../src/core/dom";

// DOM nodes adopted from another document (the Xpra HTML5 viewer iframe) keep
// the creating realm's prototypes: they are real elements with nodeType 1 but
// fail `instanceof Element` in the page that adopted them. The desktop's
// marquee guard needs to recognise these, or a click inside a native
// application window is treated as a desktop-background click and its
// `preventDefault()` suppresses the mouse-button events the app needs.
test("isElementNode recognises elements from another document realm", () => {
  const adoptedFromViewer = Object.assign(Object.create(null), {
    nodeType: 1,
    closest: () => null,
  });
  assert.equal(isElementNode(adoptedFromViewer as unknown as EventTarget), true);
  assert.equal(isElementNode({ nodeType: 1 } as unknown as EventTarget), true);
});

test("isElementNode rejects non-elements and missing targets", () => {
  assert.equal(isElementNode(null), false);
  assert.equal(isElementNode({} as unknown as EventTarget), false);
  assert.equal(isElementNode({ nodeType: 3 } as unknown as EventTarget), false);
  assert.equal(isElementNode({ nodeType: 9 } as unknown as EventTarget), false);
});
