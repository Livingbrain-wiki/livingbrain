// The brain map's live mode: while "Live" is on the map polls the activity
// feed (`GET /v1/graph/activity?since=`) and says which nodes were just used.
// Everything here is plain data and functions, so the poller, the backoff and
// the toggle can be tested with fake timers and a fake button. The renderer
// (`brain-render.js`) does the drawing; `brain.js` does the wiring.

export const LIVE_KEY = "lb-brain-live";
export const POLL_MS = 4000;
export const MAX_BACKOFF_MS = 60000;
export const FEED_MAX = 20;

/** The feed's path from a cursor (none on the first poll: the newest rows). */
export function activityPath(cursor) {
  const text = String(cursor == null ? "" : cursor).trim();
  return text ? `/v1/graph/activity?since=${encodeURIComponent(text)}` : "/v1/graph/activity";
}

/** The next wait: the poll rate on success; doubling on failure, to a minute. */
export function nextDelay(current, failed) {
  if (!failed) return POLL_MS;
  const base = Number(current) > 0 ? Number(current) : POLL_MS;
  return Math.min(MAX_BACKOFF_MS, Math.max(POLL_MS, base * 2));
}

/** The events of an answer that are well formed, oldest first, as sent. */
export function liveEvents(answer) {
  const list = answer && Array.isArray(answer.events) ? answer.events : [];
  return list.filter((event) => event && typeof event.node === "string" && typeof event.label === "string");
}

/** The node index each event touches, in order; unknown nodes are left out
 * and reported, so the renderer pulses only what it draws. */
export function pulseTargets(events, indexOf) {
  const indices = [];
  const unknown = [];
  for (const event of Array.isArray(events) ? events : []) {
    const at = indexOf(event.node);
    if (at >= 0) indices.push({ index: at, event });
    else unknown.push(event);
  }
  return { indices, unknown };
}

/** "just now", "4 min ago", "3 h ago" — for the feed. */
export function relativeTime(at, now) {
  const time = Date.parse(at);
  if (Number.isNaN(time)) return "";
  const seconds = Math.max(0, Math.round((now - time) / 1000));
  if (seconds < 45) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 2) return "a minute ago";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  return `${Math.round(hours / 24)} d ago`;
}

/** One feed line: the event in plain words, the node's name, and when. */
export function feedEntry(event, nodeLabel, now) {
  const name = String(nodeLabel || "").trim() || "a page";
  return {
    node: event.node,
    text: `${name} — ${event.label}`,
    when: relativeTime(event.at, now),
  };
}

/** Whether "L" should toggle live mode: no modifier, and not while typing. */
export function isLiveKey(event, isTyping) {
  if (!event || event.ctrlKey || event.metaKey || event.altKey) return false;
  if (event.key !== "l" && event.key !== "L") return false;
  return !(typeof isTyping === "function" && isTyping(event.target));
}

/** The remembered choice. A storage that throws reads as off. */
export function readLivePref(storage) {
  try {
    return Boolean(storage) && storage.getItem(LIVE_KEY) === "1";
  } catch {
    return false;
  }
}

export function writeLivePref(storage, on) {
  try {
    if (storage) storage.setItem(LIVE_KEY, on ? "1" : "0");
  } catch {
    // Remembering is a convenience; the map works without it.
  }
}

/**
 * The poller. `fetchActivity(cursor)` answers `{cursor, events}`; `onEvents`
 * gets each non-empty batch. It polls only while running and visible, backs
 * off on errors, and never runs two requests at once.
 */
export function createPoller({
  fetchActivity,
  onEvents,
  onError = () => {},
  doc = null,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
}) {
  let running = false;
  let timer = null;
  let inFlight = false;
  let cursor = "";
  let delay = POLL_MS;

  const hidden = () => Boolean(doc && doc.hidden);
  const cancel = () => {
    if (timer != null) clearTimer(timer);
    timer = null;
  };
  const schedule = (ms) => {
    cancel();
    if (running && !inFlight && !hidden()) timer = setTimer(tick, ms);
  };

  async function tick() {
    timer = null;
    if (!running || inFlight || hidden()) return;
    inFlight = true;
    let failed = false;
    try {
      const answer = await fetchActivity(cursor);
      if (answer && answer.cursor != null) cursor = String(answer.cursor);
      const events = liveEvents(answer);
      if (events.length && running) onEvents(events);
    } catch (error) {
      failed = true;
      onError(error);
    }
    inFlight = false;
    delay = nextDelay(delay, failed);
    schedule(delay);
  }

  return {
    start() {
      if (running) return;
      running = true;
      delay = POLL_MS;
      schedule(0);
    },
    stop() {
      running = false;
      cancel();
    },
    /** Called when the tab's visibility changes: paused while hidden. */
    visibility() {
      if (!running) return;
      if (hidden()) cancel();
      else if (timer == null && !inFlight) schedule(0);
    },
    get running() {
      return running;
    },
    get cursor() {
      return cursor;
    },
  };
}

/**
 * The toggle: a button with `aria-pressed`, the "L" key, and the remembered
 * choice. `poller` is a `createPoller` result; `onChange(on)` paints the rest.
 * Returns `{ set(on) }`.
 */
export function bindLive({ button, poller, doc, storage, onChange = () => {}, isTyping }) {
  const set = (on, { remember = true } = {}) => {
    if (on) poller.start();
    else poller.stop();
    if (button) button.setAttribute("aria-pressed", String(on));
    if (remember) writeLivePref(storage, on);
    onChange(on);
  };
  if (button) button.addEventListener("click", () => set(!poller.running));
  doc.addEventListener("keydown", (event) => {
    if (!isLiveKey(event, isTyping)) return;
    event.preventDefault();
    set(!poller.running);
  });
  doc.addEventListener("visibilitychange", () => poller.visibility());
  return { set };
}

/** Paints the feed: newest first, each line a button that focuses its node. */
export function paintFeed(list, entries, doc, onPick) {
  if (!list || !doc) return list;
  list.replaceChildren();
  for (const entry of entries.slice(0, FEED_MAX)) {
    const item = doc.createElement("li");
    const pick = doc.createElement("button");
    pick.type = "button";
    pick.className = "map-feed__item";
    pick.textContent = entry.text;
    pick.setAttribute("data-node", entry.node);
    pick.addEventListener("click", () => onPick(entry.node));
    item.appendChild(pick);
    if (entry.when) {
      const when = doc.createElement("span");
      when.className = "map-feed__when tiny";
      when.textContent = entry.when;
      item.appendChild(when);
    }
    list.appendChild(item);
  }
  return list;
}
