// Members, invitations and the switcher (ADR 0004): requests are built from
// validated input with ids encoded into the path, only an owner is offered the
// owner role, and every row is painted with elements and text nodes — a name
// full of markup stays text.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  inviteRequest,
  roleChangeRequest,
  removeRequest,
  withdrawRequest,
  switchRequest,
  memberViews,
  canManage,
  assignableRoles,
  renderMemberRows,
  renderInviteRows,
  memberErrorMessage,
} from "../assets/members.js";

function recorderDoc() {
  const make = (tag) => ({
    tag,
    textContent: "",
    value: "",
    selected: false,
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

function container() {
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

const owner = { member: { user_id: "usr_a", role: "owner", is_admin: false } };
const admin = { member: { user_id: "usr_b", role: "admin", is_admin: false } };
const member = { member: { user_id: "usr_c", role: "member", is_admin: false } };

test("an invitation needs an address and a known role", () => {
  assert.deepEqual(inviteRequest(" bob@example.test ", "admin"), {
    method: "POST",
    path: "/v1/workspaces/invites",
    body: { email: "bob@example.test", role: "admin" },
  });
  assert.equal(inviteRequest("bob@example.test", "superuser").body.role, "member");
  assert.throws(() => inviteRequest("not an address", "member"));
  assert.throws(() => inviteRequest("", "member"));
});

test("ids are encoded into the path and cannot walk out of it", () => {
  assert.equal(removeRequest("../tokens").path, "/v1/workspaces/members/..%2Ftokens");
  assert.equal(withdrawRequest("inv_1/x").path, "/v1/workspaces/invites/inv_1%2Fx");
  assert.equal(roleChangeRequest("usr_1", "member").method, "PATCH");
  assert.throws(() => roleChangeRequest("usr_1", "root"));
  assert.throws(() => removeRequest(""));
  assert.deepEqual(switchRequest(" ws_2 ").body, { workspace_id: "ws_2" });
});

test("only owners are offered the owner role, and members manage nobody", () => {
  assert.ok(assignableRoles(owner).some((r) => r.value === "owner"));
  assert.ok(!assignableRoles(admin).some((r) => r.value === "owner"));
  assert.equal(canManage(owner), true);
  assert.equal(canManage(admin), true);
  assert.equal(canManage(member), false);
  assert.equal(canManage({ member: { role: "member", is_admin: true } }), true, "a Slack admin");
});

test("rows are text: a name full of markup stays text", () => {
  const doc = recorderDoc();
  const rows = container();
  const views = memberViews([
    { user_id: "usr_a", name: "Alice", role: "owner" },
    { user_id: "usr_x", name: "<img src=x onerror=alert(1)>", role: "member" },
  ]);
  renderMemberRows(rows, views, admin, doc);
  assert.equal(rows.children.length, 2);
  const [aliceRow, evilRow] = rows.children;
  // The name cell is a picture and the name; the name is a text node's worth.
  const [pic, evilName] = evilRow.children[0].children[0].children;
  assert.equal(evilName.textContent, "<img src=x onerror=alert(1)>");
  assert.equal(pic.textContent, "IO", "no avatar: the initials, as text");
  assert.equal(aliceRow.children[0].children[0].children[1].textContent, "Alice");
  // An admin sees an owner's role as text, with no picker and no Remove.
  assert.equal(aliceRow.children[1].textContent, "Owner");
  assert.equal(aliceRow.children[2].children.length, 0);
  // and a member's role as a picker without the owner option.
  const select = evilRow.children[1].children[0];
  assert.equal(select.tag, "select");
  assert.ok(!select.children.some((option) => option.value === "owner"));
});

test("a member sees the roster and only a Leave button on their own row", () => {
  const rows = container();
  renderMemberRows(
    rows,
    memberViews([
      { user_id: "usr_a", name: "Alice", role: "owner" },
      { user_id: "usr_c", name: "Carol", role: "member" },
    ]),
    member,
    recorderDoc(),
  );
  const [alice, carol] = rows.children;
  assert.equal(alice.children[2].children.length, 0);
  assert.equal(carol.children[2].children[0].textContent, "Leave");
});

test("an empty invitation list says so", () => {
  const rows = container();
  renderInviteRows(rows, [], recorderDoc());
  assert.equal(rows.children[0].children[0].textContent, "No open invitations.");
});

test("errors read as what happened", () => {
  assert.match(memberErrorMessage({ status: 403 }, "remove them"), /role does not allow/);
  assert.match(
    memberErrorMessage({ status: 409, body: { title: "A workspace keeps at least one owner" } }, "leave"),
    /at least one owner/,
  );
});

test("the module never touches markup or storage", () => {
  const code = readFileSync(new URL("../assets/members.js", import.meta.url), "utf8")
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
  for (const forbidden of ["innerHTML", "insertAdjacentHTML", "document.write", "localStorage", "sessionStorage"]) {
    assert.ok(!code.includes(forbidden), `members.js uses ${forbidden}`);
  }
});

test("a new workspace is a POST with a clean name, and a bad name never leaves", async () => {
  const { createWorkspaceRequest } = await import("../assets/members.js");
  assert.deepEqual(createWorkspaceRequest("  Side project "), {
    method: "POST",
    path: "/v1/workspaces",
    body: { name: "Side project" },
  });
  assert.throws(() => createWorkspaceRequest("   "), /visible character/);
  assert.throws(() => createWorkspaceRequest("x".repeat(65)), /64/);
});
