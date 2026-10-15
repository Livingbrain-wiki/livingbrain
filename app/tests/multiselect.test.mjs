// The multiselect behind Settings → Access's Who and Where and a page's
// Share: native checkboxes under a search box, the choices as chips with a
// remove button each, an "add by hand" for a value the list does not offer
// (only when `custom` accepts it), and every label written as text.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { createMultiSelect, filterOptions, toggleValue } from "../assets/multiselect.js";
import { parseFolder } from "../assets/access.js";

/** A document just big enough for the component: elements that hold
 * children, attributes and listeners, and nothing that parses markup. */
function fakeDocument() {
  const make = (tag) => {
    const node = {
      tag,
      children: [],
      attrs: {},
      listeners: {},
      textContent: "",
      className: "",
      hidden: false,
      checked: false,
      value: "",
      type: "",
      id: "",
      focused: false,
      setAttribute(name, value) {
        this.attrs[name] = String(value);
      },
      getAttribute(name) {
        return this.attrs[name];
      },
      appendChild(child) {
        this.children.push(child);
        return child;
      },
      replaceChildren(...kids) {
        this.children = kids;
      },
      addEventListener(type, fn) {
        (this.listeners[type] ||= []).push(fn);
      },
      fire(type, extra = {}) {
        const event = { type, target: this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...extra };
        for (const fn of this.listeners[type] || []) fn(event);
        return event;
      },
      focus() {
        this.focused = true;
      },
    };
    return node;
  };
  return { createElement: make };
}

function walk(node, out = []) {
  out.push(node);
  for (const child of node.children || []) walk(child, out);
  return out;
}

const find = (root, test) => walk(root).filter(test);
const boxes = (root) => find(root, (n) => n.tag === "input" && n.type === "checkbox");
const chipLabels = (root) =>
  find(root, (n) => n.className === "ms__chip").map((chip) => chip.children[0].textContent);

const OPTIONS = [
  { value: "member:usr_a", label: "Ann", group: "People" },
  { value: "member:usr_b", label: "Bob", group: "People", hint: "admin" },
  { value: "agent:agt_1", label: "Claude Code", group: "Agent accounts" },
  { value: "role:viewer", label: "Everyone with the viewer role", group: "Role groups" },
];

function mount(settings = {}) {
  const doc = fakeDocument();
  const root = doc.createElement("fieldset");
  const changes = [];
  const handle = createMultiSelect(root, { label: "who", options: OPTIONS, onChange: (v) => changes.push(v), ...settings }, doc);
  return { doc, root, handle, changes };
}

test("filterOptions matches label, hint and value; toggleValue keeps order", () => {
  assert.deepEqual(filterOptions(OPTIONS, "bo").map((o) => o.label), ["Bob"]);
  assert.deepEqual(filterOptions(OPTIONS, "ADMIN").map((o) => o.label), ["Bob"]);
  assert.deepEqual(filterOptions(OPTIONS, "agent:").map((o) => o.label), ["Claude Code"]);
  assert.equal(filterOptions(OPTIONS, "").length, 4);
  assert.deepEqual(toggleValue(["a", "b"], "a", true), ["b", "a"]);
  assert.deepEqual(toggleValue(["a", "b"], "a", false), ["b"]);
});

test("ticking boxes chooses several, shown as chips", () => {
  const { root, handle, changes } = mount();
  const [ann, , agent] = boxes(root);
  assert.equal(boxes(root).length, 4);
  assert.deepEqual(chipLabels(root), [], "nothing chosen yet");
  ann.checked = true;
  ann.fire("change");
  agent.checked = true;
  agent.fire("change");
  assert.deepEqual(handle.values(), ["member:usr_a", "agent:agt_1"]);
  assert.deepEqual(chipLabels(root), ["Ann", "Claude Code"]);
  assert.deepEqual(changes.at(-1), ["member:usr_a", "agent:agt_1"]);
  // Groups are headed, so people, agents and roles read apart.
  assert.deepEqual(
    find(root, (n) => n.className === "ms__group").map((n) => n.textContent),
    ["People", "Agent accounts", "Role groups"],
  );
});

test("a chip's remove button unticks its box and is named for it", () => {
  const { root, handle } = mount();
  handle.setValues(["member:usr_b", "role:viewer"]);
  const remove = find(root, (n) => n.attrs && n.attrs["data-ms-remove"] === "member:usr_b")[0];
  assert.equal(remove.attrs["aria-label"], "Remove Bob");
  remove.fire("click");
  assert.deepEqual(handle.values(), ["role:viewer"]);
  assert.equal(boxes(root).find((b) => b.value === "member:usr_b").checked, false);
  assert.equal(handle.search.focused, true, "focus goes back to the search box");
});

test("search narrows the list, and Select shown picks only what is shown", () => {
  const { root, handle } = mount();
  handle.search.value = "o";
  handle.search.fire("input");
  // Everybody but Ann has an "o" in a label, hint or value.
  const shown = boxes(root).map((b) => b.value);
  assert.deepEqual(shown, ["member:usr_b", "agent:agt_1", "role:viewer"]);
  find(root, (n) => n.className === "btn-sm ms__all")[0].fire("click");
  assert.deepEqual(handle.values(), ["member:usr_b", "agent:agt_1", "role:viewer"]);
  handle.search.value = "zzz";
  handle.search.fire("input");
  assert.equal(boxes(root).length, 0);
  assert.equal(find(root, (n) => n.className === "ms__empty muted")[0].hidden, false);
  find(root, (n) => n.className === "btn-sm ms__clear")[0].fire("click");
  assert.deepEqual(handle.values(), []);
});

test("a folder typed by hand is added on Enter only when it is one", () => {
  const { root, handle } = mount({ options: [], custom: (text) => parseFolder(text), noneText: "The whole space" });
  assert.deepEqual(find(root, (n) => n.className === "ms__none muted").map((n) => n.textContent), ["The whole space"]);
  handle.search.value = "coding-agents/claude/";
  handle.search.fire("input");
  const add = find(root, (n) => n.className === "btn-sm ms__add")[0];
  assert.equal(add.hidden, false);
  assert.equal(add.textContent, "Add “coding-agents/claude”");
  const enter = handle.search.fire("keydown", { key: "Enter" });
  assert.equal(enter.defaultPrevented, true, "Enter never submits the form");
  assert.deepEqual(handle.values(), ["coding-agents/claude"]);
  // A bad one says why and adds nothing.
  handle.search.value = "a--b";
  handle.search.fire("input");
  assert.match(find(root, (n) => n.className === "field-error ms__error")[0].textContent, /not a folder/);
  handle.search.fire("keydown", { key: "Enter" });
  assert.deepEqual(handle.values(), ["coding-agents/claude"]);
  // New options keep what was added by hand.
  handle.setOptions([{ value: "coding-agents/codex", label: "coding-agents/codex" }]);
  assert.deepEqual(handle.values(), ["coding-agents/claude"]);
});

test("without `custom`, typing never invents a value", () => {
  const { root, handle } = mount();
  handle.search.value = "member:usr_ghost";
  handle.search.fire("input");
  handle.search.fire("keydown", { key: "Enter" });
  assert.deepEqual(handle.values(), []);
  assert.equal(find(root, (n) => n.className === "btn-sm ms__add")[0].hidden, true);
});

test("labels are text: a name full of markup stays text", () => {
  const { root, handle } = mount({ options: [{ value: "member:x", label: "<img src=x onerror=alert(1)>" }] });
  handle.setValues(["member:x"]);
  const texts = walk(root).map((n) => n.textContent);
  assert.ok(texts.includes("<img src=x onerror=alert(1)>"));
  assert.equal(handle.setValues(["member:not-offered"]).values().length, 0, "only offered values are chosen");
});

test("the module never touches markup or storage", () => {
  const code = readFileSync(new URL("../assets/multiselect.js", import.meta.url), "utf8")
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
  for (const forbidden of ["innerHTML", "insertAdjacentHTML", "document.write", "localStorage", "sessionStorage"]) {
    assert.ok(!code.includes(forbidden), `multiselect.js uses ${forbidden}`);
  }
});

test("the Access tab and the Share card carry the multiselect's fieldsets", () => {
  const settings = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
  for (const id of [
    "grant-who",
    "grant-spaces",
    "grant-folders",
    "grant-kinds",
    "grant-expires",
    "grant-preview",
    "grant-filter-who",
    "grant-filter-where",
    "grant-filter-what",
    "grant-select-all",
    "grant-revoke-selected",
    "agent-select-all",
    "agent-disable-selected",
    "agent-enable-selected",
  ]) {
    assert.match(settings, new RegExp(`id="${id}"`), `settings.html has #${id}`);
  }
  assert.match(settings, /<fieldset id="grant-who"[^>]*>\s*<legend>Who<\/legend>/);
  const wiki = readFileSync(new URL("../wiki.html", import.meta.url), "utf8");
  assert.match(wiki, /<fieldset id="page-share-who"[^>]*>\s*<legend>Share with<\/legend>/);
});
