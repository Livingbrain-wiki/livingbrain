// The Discord card in Settings → Integrations (`assets/discord.js`): what each
// state shows, the requests the buttons send, the outcome banner, and the
// plain English the page uses. The mount runs on a recording DOM, the same
// shape as the GitHub tab's tests; content reaches it only through
// `createElement`, `setAttribute` and `textContent`.

import test from "node:test";
import assert from "node:assert/strict";

import {
  CONNECT_PATH,
  CHANNEL_STATUS_TEXT,
  ERROR_TEXT,
  channelRequest,
  disconnectRequest,
  count,
  channelStatusText,
  progressLine,
  outcomeBanner,
  discordView,
  mountDiscord,
} from "../assets/discord.js";

const GUILD = "900000000000000001";
const GENERAL = "900000000000000011";

/* ------------------------------------------------------------ a recording DOM */

function element(tag, doc) {
  const node = {
    tag,
    doc,
    attrs: {},
    children: [],
    listeners: {},
    hidden: false,
    disabled: false,
    checked: false,
    value: "",
    _text: "",
    setAttribute(name, value) {
      this.attrs[name] = String(value);
    },
    getAttribute(name) {
      return name in this.attrs ? this.attrs[name] : null;
    },
    appendChild(child) {
      this.children.push(child);
      return child;
    },
    replaceChildren() {
      this.children = [];
      this._text = "";
    },
    addEventListener(type, handler) {
      (this.listeners[type] = this.listeners[type] || []).push(handler);
    },
    async dispatch(type) {
      for (const handler of this.listeners[type] || []) await handler({ target: this });
    },
    focus() {},
    get textContent() {
      return this._text + this.children.map((child) => child.textContent).join("");
    },
    set textContent(value) {
      this._text = String(value == null ? "" : value);
      this.children = [];
    },
  };
  return node;
}

/** A document with the settings page's Discord ids, and a body to append to. */
function discordDocument() {
  const registry = new Map();
  const doc = {
    registry,
    body: null,
    createElement: (tag) => element(tag, doc),
    createTextNode: (text) => ({ tag: "#text", textContent: String(text), children: [] }),
    querySelector(selector) {
      if (selector.startsWith("#")) return registry.get(selector.slice(1)) || null;
      return null;
    },
    querySelectorAll() {
      return [];
    },
  };
  doc.body = element("body", doc);
  for (const [id, tag] of [
    ["discord", "section"],
    ["discord-banner", "div"],
    ["discord-status", "div"],
    ["discord-connect", "button"],
    ["discord-servers", "div"],
  ]) {
    const node = element(tag, doc);
    node.attrs.id = id;
    registry.set(id, node);
  }
  registry.get("discord").hidden = true;
  return doc;
}

/** Every visible string the card can paint, gathered from a subtree. */
function texts(node, out = []) {
  if (!node) return out;
  if (node._text) out.push(node._text);
  if (node.textContent && node.tag === "#text") out.push(node.textContent);
  for (const child of node.children || []) texts(child, out);
  return out;
}

const CONFIGURED = {
  configured: true,
  connections: [
    {
      guild_id: GUILD,
      guild_name: "Test Guild",
      status: "connected",
      listed: true,
      last_error: null,
      progress: { messages_imported: 12340, backfilling: "general", days_waiting: 2, days_filed: 9 },
      channels: [
        { channel_id: GENERAL, name: "general", thread: false, included: true, status: "backfilling", messages: 12340, last_error: null },
        { channel_id: "900000000000000013", name: "adults", thread: false, included: false, status: "waiting", messages: 0, last_error: null },
      ],
    },
  ],
};

/* ------------------------------------------------------------------ the helpers */

test("the card is shown only when Discord is configured here", () => {
  assert.equal(discordView({ configured: false, connections: [] }).shown, false);
  assert.equal(discordView(null).shown, false);
  const view = discordView(CONFIGURED);
  assert.equal(view.shown, true);
  assert.equal(view.connections[0].name, "Test Guild");
  assert.equal(view.connections[0].channels[0].label, "#general");
  assert.equal(view.connections[0].channels[1].included, false);
});

test("a thread is labelled as one, and a channel's status is a sentence", () => {
  const view = discordView({
    configured: true,
    connections: [{ guild_id: GUILD, guild_name: "G", status: "connected", channels: [{ channel_id: "9", name: "launch", thread: true, included: true, status: "live", messages: 3 }] }],
  });
  assert.equal(view.connections[0].channels[0].label, "launch (thread)");
  assert.equal(channelStatusText("live"), "Up to date");
  assert.equal(channelStatusText("nonsense"), "");
});

test("the progress line says how many are in and what is being read", () => {
  assert.equal(progressLine({ messages_imported: 12340, backfilling: "general" }), "12,340 messages imported · importing #general");
  assert.equal(progressLine({ messages_imported: 7, backfilling: null }), "7 messages imported");
  assert.equal(progressLine({}), "Nothing imported yet. The first messages arrive within a few minutes.");
  assert.equal(count(1234567), "1,234,567");
  assert.equal(count(-4), "0");
});

test("a server whose channels are not listed yet says so, and a disconnected one says nothing is read", () => {
  const unlisted = discordView({ configured: true, connections: [{ guild_id: GUILD, guild_name: "G", status: "connected", listed: false, channels: [] }] });
  assert.equal(unlisted.connections[0].line, "Finding the server's channels. This takes a few minutes.");
  const gone = discordView({ configured: true, connections: [{ guild_id: GUILD, guild_name: "G", status: "disconnected", channels: [] }] });
  assert.equal(gone.connections[0].connected, false);
  assert.equal(gone.connections[0].line, "Disconnected. Nothing is read from this server.");
});

test("the outcome banner names a connection and a known failure, and never echoes the address", () => {
  assert.deepEqual(outcomeBanner("?discord=connected").tone, "good");
  assert.equal(outcomeBanner("?discord=error&reason=not-in-server").text, ERROR_TEXT["not-in-server"]);
  const unknown = outcomeBanner("?discord=error&reason=%3Cscript%3E");
  assert.equal(unknown.tone, "bad");
  assert.doesNotMatch(unknown.text, /script/);
  assert.equal(outcomeBanner("?github=installed"), null);
});

test("the requests are the ones the routes take, and only digits are accepted as ids", () => {
  assert.deepEqual(channelRequest(GUILD, GENERAL, false), {
    method: "PATCH",
    path: `/v1/discord/${GUILD}/channels/${GENERAL}`,
    body: { included: false },
  });
  assert.deepEqual(disconnectRequest(GUILD), { method: "DELETE", path: `/v1/discord/${GUILD}` });
  assert.throws(() => channelRequest("../admin", GENERAL, true));
  assert.throws(() => disconnectRequest("abc"));
  assert.equal(CONNECT_PATH, "/v1/discord/connect");
});

test("the words a customer reads carry no internal jargon", () => {
  const strings = [
    ...Object.values(CHANNEL_STATUS_TEXT),
    ...Object.values(ERROR_TEXT),
    progressLine({ messages_imported: 3, backfilling: "general" }),
    progressLine({}),
    discordView(CONFIGURED).connections[0].line,
  ];
  for (const text of strings) {
    assert.doesNotMatch(text, /\b(cron|drain|intent|D1|lease|cursor|snowflake)\b/i, text);
  }
});

/* ---------------------------------------------------------------- the mount */

test("an unconfigured deployment shows nothing, and asks nothing about the card", async () => {
  const doc = discordDocument();
  const calls = [];
  const api = async (path) => {
    calls.push(path);
    return { configured: false, connections: [] };
  };
  await mountDiscord({ api, confirm: async () => true, notify() {}, document: doc, location: { search: "", assign() {} }, history: null });
  assert.equal(doc.registry.get("discord").hidden, true);
  assert.deepEqual(calls, ["/v1/discord"]);
});

test("a configured page paints each channel with its checkbox, and a change is sent as a PATCH", async () => {
  const doc = discordDocument();
  let data = CONFIGURED;
  const requests = [];
  const api = async (path, options = {}) => {
    if (options.method) {
      requests.push({ path, ...options });
      return { included: options.body.included };
    }
    return data;
  };
  const notified = [];
  await mountDiscord({
    api,
    confirm: async () => true,
    notify: (node, text) => notified.push(text),
    document: doc,
    location: { search: "?discord=connected", pathname: "/settings.html", assign() {} },
    history: { replaceState() {} },
  });
  assert.equal(doc.registry.get("discord").hidden, false);
  assert.ok(notified.some((text) => /Discord is connected/.test(text)));
  const painted = texts(doc.registry.get("discord-servers"));
  assert.ok(painted.some((text) => text.includes("12,340 messages imported · importing #general")), painted.join("|"));
  assert.ok(painted.some((text) => text.includes("Importing history")), painted.join("|"));

  const list = doc.registry.get("discord-servers").children[0].children.find((c) => c.tag === "ul");
  const general = list.children[0].children[0].children[0];
  assert.equal(general.tag, "input");
  assert.equal(general.checked, true);
  assert.equal(general.getAttribute("aria-label"), "Import #general in Test Guild");

  data = { ...CONFIGURED, connections: [{ ...CONFIGURED.connections[0], channels: [{ ...CONFIGURED.connections[0].channels[0], included: false }, CONFIGURED.connections[0].channels[1]] }] };
  general.checked = false;
  await general.dispatch("change");
  assert.deepEqual(requests[0], {
    path: `/v1/discord/${GUILD}/channels/${GENERAL}`,
    method: "PATCH",
    body: { included: false },
  });
});

test("the connect button follows a server-side redirect to Discord, and a non-admin sees no card", async () => {
  const doc = discordDocument();
  const assigned = [];
  await mountDiscord({
    api: async () => CONFIGURED,
    confirm: async () => true,
    notify() {},
    document: doc,
    location: { search: "", assign: (url) => assigned.push(url) },
    history: null,
  });
  await doc.registry.get("discord-connect").dispatch("click");
  assert.deepEqual(assigned, [CONNECT_PATH]);

  const hidden = discordDocument();
  await mountDiscord({
    api: async () => {
      const error = new Error("forbidden");
      error.status = 403;
      throw error;
    },
    confirm: async () => true,
    notify() {},
    document: hidden,
    location: { search: "", assign() {} },
    history: null,
  });
  assert.equal(hidden.registry.get("discord").hidden, true);
});
