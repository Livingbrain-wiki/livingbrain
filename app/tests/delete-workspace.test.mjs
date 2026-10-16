// Deleting a workspace (ADR 0004, "Deleting a workspace"): the red button is
// enabled only by the exact name, the request carries what was typed, only an
// owner sees the danger zone at all, and a person with one workspace is told
// so rather than shown a delete they cannot finish.

import test from "node:test";
import assert from "node:assert/strict";

import {
  deleteWorkspaceRequest,
  deleteConfirmMatches,
  deleteZone,
  deleteErrorMessage,
} from "../assets/members.js";

test("the delete request names the workspace's current path and carries the typed name", () => {
  const request = deleteWorkspaceRequest("Side");
  assert.equal(request.method, "DELETE");
  assert.equal(request.path, "/v1/workspaces/current");
  assert.deepEqual(request.body, { confirm: "Side" });
});

test("the button is enabled only when the typed name is the workspace name exactly", () => {
  assert.equal(deleteConfirmMatches("Side", "Side"), true);
  assert.equal(deleteConfirmMatches("side", "Side"), false, "case matters");
  assert.equal(deleteConfirmMatches("Side ", "Side"), false, "a trailing space is not the name");
  assert.equal(deleteConfirmMatches("Sid", "Side"), false, "a prefix is not the name");
  assert.equal(deleteConfirmMatches("", "Side"), false);
  assert.equal(deleteConfirmMatches(undefined, "Side"), false);
  assert.equal(deleteConfirmMatches("", ""), false, "an empty name never matches");
  assert.equal(deleteConfirmMatches(null, null), false);
});

test("only an owner sees the danger zone, whatever workspaces they have", () => {
  const list = [{ id: "ws_a", current: true }, { id: "ws_b", current: false }];
  const owner = { member: { role: "owner" } };
  const admin = { member: { role: "admin", is_admin: true } };
  const member = { member: { role: "member" } };
  assert.deepEqual(deleteZone(owner, list), { owner: true, show: true, onlyWorkspace: false });
  assert.equal(deleteZone(admin, list).owner, false);
  assert.equal(deleteZone(admin, list).show, false);
  assert.equal(deleteZone(member, list).show, false);
  assert.equal(deleteZone(member, list).onlyWorkspace, false, "not an owner: no line either");
});

test("an owner with only one workspace is told so and offered no delete", () => {
  const only = [{ id: "ws_a", current: true }];
  const state = deleteZone({ member: { role: "owner" } }, only);
  assert.equal(state.owner, true);
  assert.equal(state.show, false);
  assert.equal(state.onlyWorkspace, true);
});

test("a refused deletion says what went wrong in plain words", () => {
  assert.match(
    deleteErrorMessage({ status: 409, body: { title: "This is your only workspace." } }),
    /only workspace/,
  );
  assert.match(deleteErrorMessage({ status: 400 }), /does not match/);
  assert.match(deleteErrorMessage({ status: 403 }), /role does not allow/);
  for (const text of [
    deleteErrorMessage({ status: 409 }),
    deleteErrorMessage({ status: 400 }),
    deleteErrorMessage({ status: 500, message: "boom" }),
  ]) {
    assert.doesNotMatch(text, /tombstone|erasure|shred|scope|workspace_id/i, text);
  }
});
