// The Integrations panel's chat-app buttons. One click must copy the same
// endpoint the panel's field shows — that is what gets pasted into ChatGPT
// or Claude — and reveal exactly that app's steps: never both, never none
// once one was picked. The document is a stub in the style of
// `dom.test.mjs` (a switch to `innerHTML` would throw, not pass quietly);
// the clipboard is injected for the same reason.

import test from "node:test";
import assert from "node:assert/strict";

import { connectorUrl, revealConnectorSteps, chooseConnectorApp } from "../assets/app.js";

/** The only two elements this feature touches, addressable by id. */
function stubDocument() {
  const els = { "steps-chatgpt": { hidden: true }, "steps-claude": { hidden: true } };
  return { els, querySelector: (sel) => els[sel.replace(/^#/, "")] || null };
}

/** Runs `body(doc)` with the stub as the app's document. */
function withDocument(body) {
  const original = globalThis.document;
  const doc = stubDocument();
  globalThis.document = doc;
  try {
    return body(doc);
  } finally {
    globalThis.document = original;
  }
}

test("the connector URL is the MCP endpoint the panel already shows", () => {
  assert.equal(connectorUrl("https://app.livingbrain.wiki"), "https://app.livingbrain.wiki/v1/pages/mcp");
  assert.equal(connectorUrl("https://staging-api.livingbrain.wiki/"), "https://staging-api.livingbrain.wiki/v1/pages/mcp");
});

test("revealing one app's steps hides the other's, and only a pick shows any", () => {
  withDocument(({ els }) => {
    revealConnectorSteps("chatgpt");
    assert.equal(els["steps-chatgpt"].hidden, false);
    assert.equal(els["steps-claude"].hidden, true);
    revealConnectorSteps("claude");
    assert.equal(els["steps-chatgpt"].hidden, true);
    assert.equal(els["steps-claude"].hidden, false);
    revealConnectorSteps(null);
    assert.equal(els["steps-chatgpt"].hidden, true);
    assert.equal(els["steps-claude"].hidden, true);
  });
});

test("choosing an app copies the endpoint and opens its steps", async () => {
  await withDocument(async ({ els }) => {
    const wrote = [];
    const copied = await chooseConnectorApp("chatgpt", {
      url: connectorUrl("https://app.livingbrain.wiki"),
      write: (text) => {
        wrote.push(text);
      },
    });
    assert.equal(copied, true);
    assert.deepEqual(wrote, ["https://app.livingbrain.wiki/v1/pages/mcp"]);
    assert.equal(els["steps-chatgpt"].hidden, false);
    assert.equal(els["steps-claude"].hidden, true);
  });
});

test("a refused clipboard still opens the steps, and says it failed", async () => {
  await withDocument(async ({ els }) => {
    const copied = await chooseConnectorApp("claude", {
      write: () => {
        throw new Error("blocked");
      },
    });
    assert.equal(copied, false);
    assert.equal(els["steps-claude"].hidden, false);
    assert.equal(els["steps-chatgpt"].hidden, true);
  });
});
