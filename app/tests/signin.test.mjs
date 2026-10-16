// The sign-in page is one plain panel: an email form, a Slack link, and
// nothing that reads like a design note or a button that does nothing.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { signInOffersSlack } from "../assets/session.js";

const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const block = html.slice(
  html.indexOf('<div id="signin-view"'),
  html.indexOf('<footer class="ft">'),
);

test("the sign-in panel has the email form and the Slack link", () => {
  assert.match(block, /^<div id="signin-view"[^>]*\shidden[\s>]/);
  assert.match(block, /<form id="email-form"/);
  assert.match(block, /<input\s[^>]*id="email"[^>]*type="email"/);
  assert.match(block, /<label for="email">/);
  assert.match(block, /<div id="email-status"[^>]*role="status"/);
  assert.match(block, /<a id="signin-slack"[^>]*>Continue with Slack<\/a>/);
});

test("the sign-in panel carries no design notes, passkeys or Discord", () => {
  assert.doesNotMatch(block, /ADR/);
  assert.doesNotMatch(block, /discord/i);
  assert.doesNotMatch(block, /passkey/i);
  assert.doesNotMatch(block, /HttpOnly/i);
});

test("the divider is decorative and the submit button is a submit", () => {
  assert.match(block, /class="signin-divider" aria-hidden="true"/);
  assert.match(block, /<button type="submit"[^>]*>Email me a sign-in link<\/button>/);
});

test("Slack is hidden by default, so a failed answer leaves the email form alone", () => {
  assert.match(block, /<a id="signin-slack"[^>]*\shidden[\s>]/);
  assert.match(block, /<div id="signin-or" class="signin-divider"[^>]*\shidden[\s>]/);
});

test("Slack is offered only when the server says slack is true", () => {
  assert.equal(signInOffersSlack({ email: true, slack: true }), true);
  assert.equal(signInOffersSlack({ email: true, slack: false }), false);
  assert.equal(signInOffersSlack({ email: true }), false);
  assert.equal(signInOffersSlack({ email: true, slack: "yes" }), false);
  assert.equal(signInOffersSlack(null), false);
  assert.equal(signInOffersSlack(undefined), false);
});
