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
  renderReaders,
  principalLabel,
} from "../assets/access.js";

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
  assert.equal(rows.children[0].children[0].textContent, "<script>x</script>");
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
