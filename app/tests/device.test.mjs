// The approval page's workspace picker: names the workspace a token will be
// for, offers the others, and paints names as text.

import test from "node:test";
import assert from "node:assert/strict";

import { workspaceChoices, renderWorkspacePicker } from "../assets/device.js";

function recorderDoc() {
  const make = (tag) => {
    const el = {
      tag,
      id: "",
      value: "",
      selected: false,
      disabled: false,
      children: [],
      attrs: {},
      listeners: {},
      _text: "",
      get textContent() {
        return this._text + this.children.map((c) => c.textContent).join("");
      },
      set textContent(v) {
        this._text = String(v);
        this.children = [];
      },
      appendChild(child) {
        this.children.push(child);
        return child;
      },
      setAttribute(k, v) {
        this.attrs[k] = v;
      },
      addEventListener(type, fn) {
        this.listeners[type] = fn;
      },
    };
    return el;
  };
  return { createElement: make, make };
}

test("choices keep id, a name (falling back to the id) and which is current", () => {
  assert.deepEqual(
    workspaceChoices([
      { id: "ws_a", name: " Alpha ", current: true, role: "owner" },
      { id: "ws_b", name: "" },
      { name: "no id" },
      null,
    ]),
    [
      { id: "ws_a", name: "Alpha", current: true },
      { id: "ws_b", name: "ws_b", current: false },
    ],
  );
  assert.deepEqual(workspaceChoices({ not: "a list" }), []);
});

test("one workspace is named as text", () => {
  const doc = recorderDoc();
  const slot = doc.make("dd");
  renderWorkspacePicker(slot, [{ id: "ws_a", name: "Alpha", current: true }], () => {}, doc);
  assert.equal(slot.textContent, "Alpha");
  assert.equal(slot.children.length, 0);
});

test("several workspaces become a picker with the current one selected", () => {
  const doc = recorderDoc();
  const slot = doc.make("dd");
  const picked = [];
  renderWorkspacePicker(
    slot,
    [
      { id: "ws_a", name: "<b>Alpha</b>", current: false },
      { id: "ws_b", name: "Beta", current: true },
    ],
    (id) => picked.push(id),
    doc,
  );
  const select = slot.children[0];
  assert.equal(select.tag, "select");
  assert.deepEqual(
    select.children.map((o) => [o.value, o.textContent, o.selected]),
    [
      ["ws_a", "<b>Alpha</b>", false],
      ["ws_b", "Beta", true],
    ],
  );
  select.value = "ws_a";
  select.listeners.change();
  assert.equal(select.disabled, true);
  assert.deepEqual(picked, ["ws_a"]);
});
