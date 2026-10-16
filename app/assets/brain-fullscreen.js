// Full screen for the brain map: the Fullscreen API (with the WebKit names as
// fallback), the button's label and pressed state, and the "F" key. Browsers
// without the API (iPhone Safari) get no button at all. The pure helpers take
// plain values; `bindFullscreen` takes its document so tests can pass a fake.

export const ENTER_LABEL = "Full screen";
export const EXIT_LABEL = "Exit full screen";

/** The Fullscreen API for `stage`, or null when this browser has none. */
export function fullscreenApi(doc, stage) {
  if (!doc || !stage) return null;
  const request = stage.requestFullscreen || stage.webkitRequestFullscreen;
  const exit = doc.exitFullscreen || doc.webkitExitFullscreen;
  if (typeof request !== "function" || typeof exit !== "function") return null;
  return {
    request: () =>
      stage.requestFullscreen ? stage.requestFullscreen() : stage.webkitRequestFullscreen(),
    exit: () => (doc.exitFullscreen ? doc.exitFullscreen() : doc.webkitExitFullscreen()),
  };
}

/** Whether `stage` is the element on full screen right now. */
export function isFullscreenOn(doc, stage) {
  return Boolean(doc) && (doc.fullscreenElement || doc.webkitFullscreenElement) === stage;
}

/** True when the key event is focus-free "f" (no modifier) outside a field. */
export function isFullscreenKey(event) {
  if (!event || event.ctrlKey || event.metaKey || event.altKey) return false;
  if (event.key !== "f" && event.key !== "F") return false;
  return !isTypingTarget(event.target);
}

/** Inputs, selects, text areas and editable content take their own letters. */
export function isTypingTarget(target) {
  if (!target) return false;
  const tag = String(target.tagName || "").toUpperCase();
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || Boolean(target.isContentEditable);
}

/**
 * Wires the button, the key and the change events. Returns `null` (and hides
 * the button) when the API is missing. Otherwise returns `{ toggle, paint }`.
 */
export function bindFullscreen({ button, stage, doc = document, win = doc.defaultView, api }) {
  const fs = api || fullscreenApi(doc, stage);
  if (!fs) {
    if (button) button.hidden = true;
    return null;
  }
  if (button) button.hidden = false;
  const label = (button && button.querySelector && button.querySelector("[data-label]")) || button;

  const paint = () => {
    const on = isFullscreenOn(doc, stage);
    if (label) label.textContent = on ? EXIT_LABEL : ENTER_LABEL;
    if (button) button.setAttribute("aria-pressed", String(on));
    return on;
  };

  const toggle = () => {
    const run = isFullscreenOn(doc, stage) ? fs.exit : fs.request;
    try {
      // A refusal (no user gesture, say) leaves the button as it was.
      Promise.resolve(run()).catch(() => {});
    } catch {
      // Same: the browser said no.
    }
  };

  const onChange = () => {
    paint();
    // The renderer measures its canvas with a ResizeObserver; this nudges
    // any other listener that waits on the window size.
    if (win && typeof win.dispatchEvent === "function" && typeof Event === "function") {
      win.dispatchEvent(new Event("resize"));
    }
  };

  if (button) button.addEventListener("click", toggle);
  doc.addEventListener("keydown", (event) => {
    if (!isFullscreenKey(event)) return;
    event.preventDefault();
    toggle();
  });
  doc.addEventListener("fullscreenchange", onChange);
  doc.addEventListener("webkitfullscreenchange", onChange);
  paint();
  return { toggle, paint };
}
