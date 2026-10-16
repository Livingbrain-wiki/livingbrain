// The brain map's full-screen control (`assets/brain-fullscreen.js`): the
// button asks the browser for the stage, follows `fullscreenchange`, the "F"
// key toggles it outside fields, and a browser without the API hides it.

import test from "node:test";
import assert from "node:assert/strict";

import { bindFullscreen, fullscreenApi, isFullscreenKey, isTypingTarget } from "../assets/brain-fullscreen.js";

function fakeDoc() {
  const listeners = {};
  const doc = {
    fullscreenElement: null,
    listeners,
    exits: 0,
    defaultView: { resized: 0, dispatchEvent(event) { if (event.type === "resize") this.resized += 1; return true; } },
    addEventListener(type, fn) {
      (listeners[type] ||= []).push(fn);
    },
    fire(type, event = {}) {
      for (const fn of listeners[type] || []) fn(event);
    },
    exitFullscreen() {
      doc.exits += 1;
      doc.fullscreenElement = null;
      return Promise.resolve();
    },
  };
  return doc;
}

function fakeStage(doc, { prefix = "requestFullscreen" } = {}) {
  const stage = { requests: 0, tagName: "DIV" };
  stage[prefix] = () => {
    stage.requests += 1;
    doc.fullscreenElement = stage;
    return Promise.resolve();
  };
  return stage;
}

function fakeButton() {
  const label = { textContent: "" };
  return {
    hidden: false,
    label,
    attrs: {},
    listeners: {},
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    addEventListener(type, fn) {
      this.listeners[type] = fn;
    },
    click() {
      this.listeners.click();
    },
    querySelector(selector) {
      return selector === "[data-label]" ? this.label : null;
    },
  };
}

test("no Fullscreen API (iPhone Safari): the button is hidden and nothing is bound", () => {
  const doc = fakeDoc();
  const stage = { tagName: "DIV" };
  const button = fakeButton();
  assert.equal(fullscreenApi(doc, stage), null);
  assert.equal(bindFullscreen({ button, stage, doc }), null);
  assert.equal(button.hidden, true);
  assert.equal(doc.listeners.keydown, undefined, "no F key without the API");
});

test("the WebKit names work when the standard ones are missing", () => {
  const doc = fakeDoc();
  doc.webkitExitFullscreen = () => {
    doc.fullscreenElement = null;
    return Promise.resolve();
  };
  doc.webkitFullscreenElement = null;
  const stage = fakeStage(doc, { prefix: "webkitRequestFullscreen" });
  const button = fakeButton();
  const bound = bindFullscreen({ button, stage, doc });
  assert.ok(bound);
  assert.equal(button.hidden, false);
  button.click();
  assert.equal(stage.requests, 1);
});

test("the button enters, follows fullscreenchange, and exits", () => {
  const doc = fakeDoc();
  const stage = fakeStage(doc);
  const button = fakeButton();
  bindFullscreen({ button, stage, doc });
  assert.equal(button.hidden, false);
  assert.equal(button.label.textContent, "Full screen");
  assert.equal(button.attrs["aria-pressed"], "false");

  button.click();
  assert.equal(stage.requests, 1);
  // The browser reports the change; the label and pressed state follow it.
  doc.fire("fullscreenchange");
  assert.equal(button.label.textContent, "Exit full screen");
  assert.equal(button.attrs["aria-pressed"], "true");
  assert.equal(doc.defaultView.resized, 1, "a resize goes out on the change");

  button.click();
  assert.equal(doc.exits, 1);
  doc.fire("fullscreenchange");
  assert.equal(button.label.textContent, "Full screen");
  assert.equal(button.attrs["aria-pressed"], "false");
});

test("Esc leaves full screen by the browser, and the label follows", () => {
  const doc = fakeDoc();
  const stage = fakeStage(doc);
  const button = fakeButton();
  bindFullscreen({ button, stage, doc });
  button.click();
  doc.fire("fullscreenchange");
  doc.fullscreenElement = null; // what Esc does
  doc.fire("fullscreenchange");
  assert.equal(button.label.textContent, "Full screen");
});

test("a browser refusal leaves the button as it was instead of throwing", () => {
  const doc = fakeDoc();
  const stage = fakeStage(doc);
  stage.requestFullscreen = () => Promise.reject(new Error("no gesture"));
  const button = fakeButton();
  bindFullscreen({ button, stage, doc });
  assert.doesNotThrow(() => button.click());
  assert.equal(button.label.textContent, "Full screen");
});

test("the F key toggles full screen, but not while typing or with a modifier", () => {
  const doc = fakeDoc();
  const stage = fakeStage(doc);
  const button = fakeButton();
  bindFullscreen({ button, stage, doc });
  const press = (target, extra = {}) => {
    const event = { key: "f", target, preventDefault() { this.prevented = true; }, ...extra };
    doc.fire("keydown", event);
    return event;
  };
  const search = { tagName: "INPUT" };
  const area = { tagName: "TEXTAREA" };
  const picker = { tagName: "select" };
  const editable = { tagName: "DIV", isContentEditable: true };

  assert.equal(press(search).prevented, undefined, "ignored inside an input");
  press(area);
  press(picker);
  press(editable);
  assert.equal(stage.requests, 0, "all four ignored");

  const canvas = { tagName: "CANVAS" };
  const pressed = press(canvas);
  assert.equal(pressed.prevented, true);
  assert.equal(stage.requests, 1);
  press(canvas, { key: "F" });
  // Already full screen: the second press exits instead.
  assert.equal(doc.exits, 1);
  assert.equal(doc.fullscreenElement, null);

  press(canvas, { ctrlKey: true });
  press(canvas, { metaKey: true });
  press(canvas, { key: "g" });
  assert.equal(stage.requests, 1, "modifiers and other keys do nothing");
});

test("helpers: key and typing rules", () => {
  assert.equal(isTypingTarget(null), false);
  assert.equal(isTypingTarget({ tagName: "input" }), true);
  assert.equal(isFullscreenKey({ key: "f", target: { tagName: "BUTTON" } }), true);
  assert.equal(isFullscreenKey({ key: "f", target: { tagName: "INPUT" } }), false);
  assert.equal(isFullscreenKey({ key: "f", altKey: true, target: null }), false);
});
