// Agent accounts, grants and the audit log (ADR 0004): requests are built
// from validated input with ids encoded into the path, a folder cannot smuggle
// a slug separator or a dot-segment, no list view can carry a key, the one key
// a mint returns reaches only its element and leaves it again, and every
// painter writes text — never markup.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  agentCreateRequest,
  agentKeyMintRequest,
  agentKeyRevokeRequest,
  grantCreateRequest,
  parseFolder,
  pageAccessRequest,
  keyViews,
  agentViews,
  mintedKey,
  revealAgentKey,
  clearAgentKey,
  renderAgentRows,
  renderGrantRows,
  renderReaders,
  principalLabel,
  grantPlan,
  grantPreview,
  grantBatchRequest,
  grantRevokeBatchRequest,
  agentBatchRequest,
  needsConfirm,
  expiryOf,
  principalOptions,
  folderOptions,
  grantViews,
  grantFilterOptions,
  filterGrants,
  keepSelected,
  selectionText,
  MAX_BATCH,
} from "../assets/access.js";
import { projectSegment } from "../assets/wiki.js";

const FIXTURE_KEY = ["lba", "fixture", "notsecret"].join("_");

function doc() {
  const make = (tag) => ({
    tag,
    textContent: "",
    className: "",
    colSpan: 0,
    attrs: {},
    children: [],
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    appendChild(child) {
      this.children.push(child);
    },
  });
  return { createElement: make };
}

function box() {
  return {
    children: [],
    replaceChildren() {
      this.children = [];
    },
    appendChild(child) {
      this.children.push(child);
    },
  };
}

test("requests validate and encode", () => {
  assert.throws(() => agentCreateRequest("  "));
  assert.equal(agentKeyRevokeRequest("agt_1", "lba/../x").path, "/v1/agents/agt_1/keys/lba%2F..%2Fx");
  assert.deepEqual(agentKeyMintRequest("agt_1", "ci", "").body, { name: "ci" });
  assert.equal(agentKeyMintRequest("agt_1", "ci", "30").body.expires_in_days, 30);
  assert.throws(() => agentKeyMintRequest("agt_1", "ci", "0"));
  assert.throws(() => agentKeyMintRequest("agt_1", "ci", "999"));
  assert.equal(pageAccessRequest("a--b").path, "/v1/access/pages/a--b");
});

test("a folder is plain segments: no dot-segments, no slug separator", () => {
  assert.equal(parseFolder(" coding-agents/claude/ "), "coding-agents/claude");
  assert.equal(parseFolder(""), "");
  for (const bad of ["../x", "a/./b", "a--b", "a b", "a/<b>"]) {
    assert.throws(() => parseFolder(bad), bad);
  }
});

test("a grant names who, where and what, and deny is explicit", () => {
  const request = grantCreateRequest({
    principal: "agent:agt_1",
    space: "personal:usr_a",
    folder: "coding-agents/claude",
    kind: "pages",
    permission: "write",
  });
  assert.deepEqual(request.body, {
    principal: "agent:agt_1",
    space: "personal:usr_a",
    path: "coding-agents/claude",
    permission: "write",
    kind: "pages",
  });
  assert.equal(grantCreateRequest({ principal: "member:b", space: "shared", permission: "read", effect: "deny" }).body.effect, "deny");
  assert.throws(() => grantCreateRequest({ principal: "", space: "shared", permission: "read" }));
  assert.throws(() => grantCreateRequest({ principal: "member:b", space: "elsewhere", permission: "read" }));
});

test("no list view carries a key; the mint reveals it once and it leaves again", () => {
  const listed = keyViews({ keys: [{ prefix: "lba_00", name: "ci", key: FIXTURE_KEY }] });
  assert.ok(!JSON.stringify(listed).includes(FIXTURE_KEY));
  const node = { textContent: "" };
  revealAgentKey(node, mintedKey({ prefix: "lba_00", key: FIXTURE_KEY }));
  assert.equal(node.textContent, FIXTURE_KEY);
  clearAgentKey(node);
  assert.equal(node.textContent, "");
});

test("painters write text, so a name full of markup stays text", () => {
  const rows = box();
  renderAgentRows(rows, agentViews({ agents: [{ id: "agt_1", name: "<script>x</script>", disabled: false }] }), doc());
  // The first cell is the row's selection box; the name is text after it.
  assert.equal(rows.children[0].children[0].children[0].type, "checkbox");
  assert.equal(rows.children[0].children[1].textContent, "<script>x</script>");
  const list = box();
  renderReaders(
    list,
    { readers: [{ principal: "member:usr_x", permission: "read", via: "grant", grant_id: "g1" }], you_can_share: true },
    [{ user_id: "usr_x", name: "<b>Eve</b>" }],
    [],
    doc(),
  );
  assert.equal(list.children[0].children[0].textContent, "<b>Eve</b>");
  assert.equal(principalLabel("role:member", [], []), "everyone with the member role");
});

test("the module never touches markup or storage", () => {
  const code = readFileSync(new URL("../assets/access.js", import.meta.url), "utf8")
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
  for (const forbidden of ["innerHTML", "insertAdjacentHTML", "document.write", "localStorage", "sessionStorage"]) {
    assert.ok(!code.includes(forbidden), `access.js uses ${forbidden}`);
  }
});

test("the grant form makes the cross product of who × where × what", () => {
  const plan = grantPlan({
    principals: ["member:usr_a", "agent:agt_1", "member:usr_a"],
    spaces: ["personal:me"],
    folders: ["coding-agents/claude", " coding-agents/codex/ ", "coding-agents/claude"],
    kinds: ["pages"],
    permission: "write",
    expires: "2027-01-31",
  });
  assert.equal(plan.count, 4, "duplicates count once");
  assert.equal(grantPreview(plan), "This creates 4 grants: 2 grantees × 1 space × 2 folders × 1 kind.");
  assert.deepEqual(plan.grants[0], {
    principal: "member:usr_a",
    space: "personal:me",
    path: "coding-agents/claude",
    kind: "pages",
    permission: "write",
    expires_at: "2027-01-31T23:59:59Z",
  });
  // Six: 2 who × 1 space × 3 kinds.
  const six = grantPlan({ principals: ["member:a", "role:viewer"], spaces: ["shared"], kinds: ["pages", "sources", "facts"], permission: "read" });
  assert.equal(six.count, 6);
  assert.match(grantPreview(six), /^This creates 6 grants/);
  assert.equal(six.grants[0].path, "", "no folder is the whole space");
  // Everything swallows the kinds; deny is a read deny.
  const all = grantPlan({ principals: ["member:a"], spaces: ["shared", "personal:me"], kinds: ["pages", "*"], permission: "deny" });
  assert.equal(all.count, 2);
  assert.ok(all.grants.every((g) => g.kind === "*" && g.effect === "deny" && g.permission === "read"));
  assert.equal(grantPreview(grantPlan({ principals: ["member:a"], spaces: ["shared"], kinds: ["*"] })), "This creates 1 grant: 1 grantee × 1 space × 1 folder × 1 kind.");
});

test("the grant form says what is missing, and refuses a bad folder or date", () => {
  const base = { principals: ["member:a"], spaces: ["shared"], kinds: ["*"], permission: "read" };
  assert.throws(() => grantPlan({ ...base, principals: [] }), /who/);
  assert.throws(() => grantPlan({ ...base, principals: ["usr_a"] }), /who/);
  assert.throws(() => grantPlan({ ...base, spaces: [] }), /space/);
  assert.throws(() => grantPlan({ ...base, spaces: ["elsewhere"] }), /space/);
  assert.throws(() => grantPlan({ ...base, kinds: [] }), /covers/);
  assert.throws(() => grantPlan({ ...base, folders: ["a--b"] }));
  assert.throws(() => grantPlan({ ...base, folders: ["../x"] }));
  assert.throws(() => expiryOf("next week"), /date/);
  assert.equal(expiryOf(""), null);
});

test("preview and confirm: over ten asks, over the cap cannot be sent", () => {
  assert.equal(needsConfirm(10), false);
  assert.equal(needsConfirm(11), true);
  const many = grantPlan({
    principals: Array.from({ length: 11 }, (_, n) => `member:u${n}`),
    spaces: ["shared"],
    folders: Array.from({ length: 10 }, (_, n) => `f${n}`),
    kinds: ["*"],
  });
  assert.equal(many.count, 110);
  assert.match(grantPreview(many), /more than the 100 one request makes/);
  assert.throws(() => grantBatchRequest(many.grants), /at most 100/);
  const request = grantBatchRequest(many.grants.slice(0, MAX_BATCH));
  assert.equal(request.method, "POST");
  assert.equal(request.path, "/v1/access/grants/batch");
  assert.equal(request.body.grants.length, 100);
  assert.throws(() => grantBatchRequest([]));
});

test("bulk revoke and bulk disable send one batch of distinct ids", () => {
  assert.deepEqual(grantRevokeBatchRequest(["g1", "g2", "g1", " "]), {
    method: "POST",
    path: "/v1/access/grants/batch/revoke",
    body: { ids: ["g1", "g2"] },
  });
  assert.throws(() => grantRevokeBatchRequest([]));
  assert.deepEqual(agentBatchRequest(["agt_1", "agt_2"], true), {
    method: "POST",
    path: "/v1/agents/batch",
    body: { ids: ["agt_1", "agt_2"], disabled: true },
  });
  assert.equal(agentBatchRequest(["agt_1"], 0).body.disabled, false);
  assert.throws(() => agentBatchRequest(Array.from({ length: 101 }, (_, n) => `a${n}`), true));
});

test("who: people (not yourself), agent accounts and role groups", () => {
  const options = principalOptions(
    [{ user_id: "usr_me", name: "Me" }, { user_id: "usr_b", name: "Bob", role: "member" }],
    [{ id: "agt_1", name: "Claude Code", disabled: false }],
    "usr_me",
  );
  assert.deepEqual(
    options.map((o) => [o.value, o.group]),
    [
      ["member:usr_b", "People"],
      ["agent:agt_1", "Agent accounts"],
      ["role:member", "Role groups"],
      ["role:admin", "Role groups"],
      ["role:viewer", "Role groups"],
    ],
  );
});

test("where: folders from the coding-agents tree, page slugs and source projects", () => {
  const options = folderOptions(
    {
      tree: { agents: [{ agent: "Claude Code", folder: "claude", projects: [{ project: "Chi Infra" }, { project: null }] }] },
      pages: { pages: [{ slug: "coding-agents--claude--chi-infra--decisions" }, { slug: "people--nick" }, { slug: "plain" }] },
      projects: { projects: [{ project: "earthos-world", sources: 3 }, { project: "has space" }] },
    },
    projectSegment,
  );
  const values = options.map((o) => o.value);
  for (const want of [
    "coding-agents",
    "coding-agents/claude",
    "coding-agents/claude/chi-infra",
    "coding-agents/claude/no-project",
    "coding-agents/claude/chi-infra",
    "people",
    "project/earthos-world",
  ]) {
    assert.ok(values.includes(want), `${want} offered: ${values}`);
  }
  assert.ok(!values.includes("plain"), "a page is not a folder");
  assert.ok(!values.includes("project/has space"), "only names a folder can spell");
  assert.equal(new Set(values).size, values.length, "each folder once");
});

test("the grants list filters by who, where and what, and selection follows the filter", () => {
  const views = grantViews({
    grants: [
      { id: "g1", principal: "member:a", space: "personal:me", path: "coding-agents/claude", kind: "pages", permission: "read" },
      { id: "g2", principal: "agent:x", space: "personal:me", path: "coding-agents/codex", kind: "*", permission: "write" },
      { id: "g3", principal: "member:a", space: "shared", path: "", kind: "facts", permission: "read" },
    ],
  });
  const choices = grantFilterOptions(views, [{ user_id: "a", name: "Ann" }], [{ id: "x", name: "Bot" }]);
  assert.deepEqual(choices.who.map((c) => c.label), ["Ann", "Bot (agent)"]);
  assert.ok(choices.where.some((c) => c.value === "folder:coding-agents/claude"));
  assert.deepEqual(filterGrants(views, { who: "member:a" }).map((v) => v.id), ["g1", "g3"]);
  assert.deepEqual(filterGrants(views, { where: "space:shared" }).map((v) => v.id), ["g3"]);
  assert.deepEqual(filterGrants(views, { where: "folder:coding-agents" }).map((v) => v.id), ["g1", "g2"]);
  assert.deepEqual(filterGrants(views, { what: "*" }).map((v) => v.id), ["g2"]);
  assert.deepEqual(filterGrants(views, {}).length, 3);
  const shown = filterGrants(views, { who: "member:a" });
  assert.deepEqual([...keepSelected(new Set(["g1", "g2"]), shown)], ["g1"]);
  assert.equal(selectionText(0, "grant"), "None selected");
  assert.equal(selectionText(2, "grant"), "2 grants selected");

  const rows = box();
  renderGrantRows(rows, shown, [{ user_id: "a", name: "<i>Ann</i>" }], [], doc(), new Set(["g3"]));
  const [first, second] = rows.children;
  assert.equal(first.children[0].children[0].attrs["data-grant-select"], "g1");
  assert.equal(first.children[0].children[0].checked, false);
  assert.equal(second.children[0].children[0].checked, true);
  assert.match(second.children[0].children[0].attrs["aria-label"], /<i>Ann<\/i>/);
  assert.equal(first.children[1].textContent, "<i>Ann</i>");
});
