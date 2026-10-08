// The settings choke point's request shape.
//
// `app.js` calls `settingRequest` (through `LB.setting`) and sends whatever
// comes back; these tests pin the shape that reaches the server, because that
// is what makes the audit criterion (#30) satisfiable: every write carries its
// section, key and value.

import test from "node:test";
import assert from "node:assert/strict";

import {
  SECTIONS,
  MODEL_ROLES,
  settingRequest,
  isSection,
  auditRecord,
  describeOutcome,
  STACK,
  STACK_STATUSES,
  stackEntries,
  stackStatus,
  stackSubprocessors,
  renderStackRows,
  renderStackLinks,
} from "../assets/settings.js";

/**
 * A document that records how it was used, so the rendering is asserted the
 * way `dom.test.mjs` asserts the wiki: through `createElement` and
 * `setAttribute`, never through an HTML string. Any method the renderer does
 * not call here simply does not exist.
 */
function recordingDocument() {
  const original = globalThis.document;
  const container = {
    children: [],
    replaceChildren() {
      this.children = [];
    },
    appendChild(child) {
      this.children.push(child);
      return child;
    },
  };
  globalThis.document = {
    readyState: "complete",
    createElement: (tag) => ({
      tag,
      className: "",
      textContent: "",
      attrs: {},
      children: [],
      setAttribute(name, value) {
        this.attrs[name] = value;
      },
      appendChild(child) {
        this.children.push(child);
        return child;
      },
    }),
    createTextNode: (value) => ({ tag: "#text", attrs: {}, textContent: value }),
  };
  try {
    renderStackRows(container);
    return container;
  } finally {
    globalThis.document = original;
  }
}

/** Every element in the rendered container, flattened. */
function flatten(node, out = []) {
  for (const child of node.children || []) {
    out.push(child);
    flatten(child, out);
  }
  return out;
}

/**
 * The placeholder BYOK key these tests paste into a models payload. Built at
 * run time from words rather than written out, so nothing in this file has the
 * shape of a real provider credential — a secret scanner reads shape, not
 * intent, and a fixture is exactly the place it must not get one.
 */
const FIXTURE_KEY = ["lb", "test", "byok", "fixture"].join("-");

test("the sections the issue asks for are present", () => {
  assert.deepEqual(SECTIONS, [
    "models",
    "proactivity",
    "tools",
    "automations",
    "skills",
    "colonizer",
    "stack",
  ]);
  for (const section of SECTIONS) assert.ok(isSection(section));
  assert.ok(!isSection("billing"));
});

test("a models change is a PUT to the role's path", () => {
  const request = settingRequest("models", "main", {
    provider: "deepseek",
    base_url: "https://api.deepseek.com/v1",
    api_key: FIXTURE_KEY,
    model: "deepseek-chat",
    fallback_to_managed: false,
  });
  assert.equal(request.wired, true);
  assert.equal(request.method, "PUT");
  assert.equal(request.path, "/v1/models/main");
  assert.equal(request.section, "models");
  assert.equal(request.key, "main");
});

test("every model role maps to its own path", () => {
  for (const role of MODEL_ROLES) {
    const request = settingRequest("models", role, { provider: "deepseek" });
    assert.equal(request.path, `/v1/models/${role}`);
  }
});

test("a role with a slash is encoded into the path", () => {
  const request = settingRequest("models", "a/../b", { provider: "deepseek" });
  assert.equal(request.path, "/v1/models/a%2F..%2Fb");
});

test("the body carries the model fields the API reads", () => {
  const request = settingRequest("models", "research", {
    provider: "openai-compatible",
    base_url: null,
    api_key: FIXTURE_KEY,
    model: "gpt-x",
    fallback_to_managed: true,
  });
  // The handler reads these flat.
  assert.equal(request.body.provider, "openai-compatible");
  assert.equal(request.body.base_url, null);
  assert.equal(request.body.api_key, FIXTURE_KEY);
  assert.equal(request.body.model, "gpt-x");
  assert.equal(request.body.fallback_to_managed, true);
});

test("the body carries section, key and value next to the change", () => {
  // This is the audit triple: a server can attribute the write without
  // parsing the path.
  const value = { provider: "deepseek", api_key: FIXTURE_KEY, model: "m" };
  const request = settingRequest("models", "triage", value);
  assert.equal(request.body.section, "models");
  assert.equal(request.body.key, "triage");
  assert.deepEqual(request.body.value, value);
});

test("a non-models change carries the same triple", () => {
  const request = settingRequest("proactivity", "nightly-evolve", true);
  assert.deepEqual(request.body, {
    section: "proactivity",
    key: "nightly-evolve",
    value: true,
  });
});

test("a section with no server route is not wired, and has no path", () => {
  for (const section of [
    "proactivity",
    "tools",
    "automations",
    "skills",
    "colonizer",
    "stack",
  ]) {
    const request = settingRequest(section, "anything", true);
    assert.equal(request.wired, false, section);
    assert.equal(request.method, null);
    assert.equal(request.path, null);
    assert.deepEqual(request.body, {
      section,
      key: "anything",
      value: true,
    });
  }
});

test("an unknown section throws rather than silently doing nothing", () => {
  assert.throws(() => settingRequest("nope", "k", 1), /unknown settings section/);
  assert.throws(() => settingRequest("models", "   ", 1), /needs a key/);
  assert.throws(() => settingRequest("models", null, 1), /needs a key/);
});

test("a key is trimmed, because ' main ' and 'main' are one setting", () => {
  assert.equal(settingRequest("models", "  main  ", {}).key, "main");
  assert.equal(settingRequest("models", "  main  ", {}).path, "/v1/models/main");
});

test("auditRecord is the shape a server would log", () => {
  const request = settingRequest("models", "main", { provider: "deepseek" });
  assert.deepEqual(auditRecord(request), {
    section: "models",
    key: "main",
    value: { provider: "deepseek" },
    method: "PUT",
    path: "/v1/models/main",
  });
});

test("the outcome text says plainly when nothing was sent", () => {
  const unwired = settingRequest("tools", "mcp-tools", false);
  const wired = settingRequest("models", "main", {});
  assert.match(describeOutcome(unwired, false), /no server route yet/);
  assert.match(describeOutcome(unwired, false), /tools\.mcp-tools/);
  assert.match(describeOutcome(wired, true), /models\.main saved/);
  assert.match(describeOutcome(wired, false), /could not be saved/);
});

/* ------------------------------------------------------------ built with */

test("the Built with section is one of the settings sections", () => {
  assert.ok(SECTIONS.includes("stack"));
  assert.ok(isSection("stack"));
});

test("the vendored entry is FZ-018 and the subprocessors link is honest", () => {
  assert.equal(STACK.venture.id, "FZ-018");
  const list = stackSubprocessors();
  assert.equal(list.href, STACK.subprocessors);
  // The registry has no subprocessors page: the URL is the venture's page, so
  // the label must not claim a dedicated privacy page exists.
  assert.equal(list.href, "https://factory0.ventures/ventures/living-brain/");
  assert.match(list.label, /venture page; no privacy page yet/);
});

test("every one of the nine registry entries is rendered", () => {
  const rows = stackEntries();
  assert.equal(STACK.uses.length, 9);
  assert.equal(rows.length, 9);
  // The rendered rows are the registry's entries, one for one.
  assert.deepEqual(
    rows.map((row) => row.id).sort(),
    STACK.uses.map((entry) => entry.id).sort(),
  );
});

test("each entry carries its role phrase, its product link and its status", () => {
  for (const row of stackEntries()) {
    const source = STACK.uses.find((entry) => entry.id === row.id && entry.role === row.role);
    assert.ok(source, row.role);
    assert.equal(row.phrase, source.phrase);
    assert.equal(row.name, source.name);
    // The url is rendered as the anchor's href, not as text.
    assert.equal(row.url, source.url);
    assert.match(row.url, /^https:\/\//);
    assert.ok(row.detail.length > 0, `${row.name} has a status in words`);
  }
});

test("the two statuses read in words, from the registry's own sentences", () => {
  assert.deepEqual(STACK_STATUSES, ["live", "planned"]);
  const planned = stackEntries().find((row) => row.status === "planned");
  const live = stackEntries().find((row) => row.status === "live");
  assert.equal(planned.detail, "Decided and tracked, not in use yet.");
  assert.equal(live.detail, "In use today.");
});

test("only Cloudflare is live, and no planned entry is shown as live", () => {
  const live = stackEntries().filter((row) => row.status === "live");
  assert.equal(live.length, 1);
  assert.equal(live[0].name, "Cloudflare");
  // The acceptance criterion, asserted against the source data as well as
  // the rendered rows: nothing planned can be painted live.
  const rendered = new Set(stackEntries().map((row) => row.name));
  for (const entry of STACK.uses) {
    if (entry.status !== "live") {
      assert.ok(rendered.has(entry.name), entry.name);
      assert.equal(stackStatus(entry), "planned", entry.name);
    }
  }
});

test("a status the registry never uses falls back to planned", () => {
  // Anything that is not exactly "live" is planned, so a typo or a new status
  // cannot leak through as a claim that something is running today.
  assert.equal(stackStatus({ status: "live" }), "live");
  for (const value of ["planned", "Live", "LIVE", "shipped", "", null, undefined]) {
    assert.equal(stackStatus({ status: value }), "planned", String(value));
  }
  assert.equal(stackStatus(null), "planned");
});

test("live entries come first, registry order kept inside each group", () => {
  const rows = stackEntries();
  // Grouped live-then-planned, so the reader meets what runs today first.
  assert.equal(rows[0].name, "Cloudflare");
  assert.deepEqual(
    [...new Set(rows.map((row) => row.status))],
    ["live", "planned"],
  );
  // Registry order preserved inside the planned group.
  assert.deepEqual(
    rows.filter((row) => row.status === "planned").map((row) => row.role),
    STACK.uses.filter((entry) => entry.status === "planned").map((entry) => entry.role),
  );
});

test("the rendered rows put every product's url in an anchor's href", () => {
  const elements = flatten(recordingDocument());
  const anchors = elements.filter((el) => el.tag === "a");
  assert.equal(anchors.length, 9);
  for (const anchor of anchors) {
    assert.match(anchor.attrs.href, /^https:\/\//);
    assert.equal(anchor.attrs.rel, "noopener noreferrer");
  }
  // Cloudflare is the one that is actually in use, and it leads.
  assert.equal(anchors[0].textContent, "Cloudflare");
  assert.equal(anchors[0].attrs.href, "https://www.cloudflare.com");
});

test("the rendered pills carry the same status word as the registry", () => {
  const elements = flatten(recordingDocument());
  const pills = elements.filter((el) => el.attrs["data-state"]);
  assert.equal(pills.length, 9);
  assert.equal(pills.filter((pill) => pill.attrs["data-state"] === "live").length, 1);
  assert.equal(pills.filter((pill) => pill.attrs["data-state"] === "planned").length, 8);
  // The pill's words and its state agree for every row: no row can read as
  // live while carrying the planned state, or the other way round.
  for (const pill of pills) {
    assert.equal(pill.textContent, STACK.statuses[pill.attrs["data-state"]]);
  }
});

test("the page's subprocessors href comes from the vendored document", () => {
  // `bootStack` writes both hrefs, so the URLs live in `stack.json` and the
  // HTML is not a second source of truth for them.
  const node = () => ({
    textContent: "",
    attrs: {},
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
  });
  const nodes = {
    "#stack-venture": node(),
    "#stack-subprocessors": node(),
    "#stack-source": node(),
  };
  const doc = {
    querySelector: (sel) => nodes[sel] || null,
  };
  renderStackLinks(doc);
  assert.equal(nodes["#stack-subprocessors"].attrs.href, STACK.subprocessors);
  assert.equal(nodes["#stack-subprocessors"].attrs.rel, "noopener noreferrer");
  assert.equal(nodes["#stack-source"].attrs.href, STACK.source);
  assert.equal(nodes["#stack-venture"].textContent, "FZ-018 (Living Brain)");
});