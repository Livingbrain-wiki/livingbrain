// Names and avatars (`assets/profile.js`): the name rules the form checks
// before a round trip (the server's, mirrored), the requests, the initials and
// the colour someone without a picture gets, painting an avatar with elements
// only, the square crop, and that the settings page has its editors and the
// CSP stayed tight.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  NAME_MAX,
  AVATAR_MAX_BYTES,
  AVATAR_TONES,
  nameError,
  displayNameRequest,
  workspaceNameRequest,
  avatarUploadRequest,
  avatarRemoveRequest,
  avatarFileError,
  initials,
  avatarTone,
  safeAvatarUrl,
  paintAvatar,
  avatarNode,
  profileView,
  profileErrorMessage,
  squareCrop,
  cropToSquare,
} from "../assets/profile.js";
import { api } from "../assets/app.js";
import { sessionView } from "../assets/session.js";
import { settingsTab } from "../assets/nav.js";

const AVATAR = "/v1/avatars/av_01HZZZBBBBBBBBBBBBBBBBBBBB";

/** The few element methods `paintAvatar` uses, recorded. */
function stubDoc() {
  const make = (tag) => {
    const el = {
      tagName: tag.toUpperCase(),
      className: "",
      attrs: {},
      children: [],
      listeners: {},
      _text: "",
      get textContent() {
        return this._text + this.children.map((c) => c.textContent || "").join("");
      },
      set textContent(value) {
        this._text = String(value);
        this.children = [];
      },
      setAttribute(name, value) {
        this.attrs[name] = String(value);
      },
      appendChild(child) {
        this.children.push(child);
        return child;
      },
      addEventListener(type, fn) {
        (this.listeners[type] = this.listeners[type] || []).push(fn);
      },
    };
    return el;
  };
  return { createElement: make };
}

test("a name is 1 to 64 visible characters, as the server counts them", () => {
  assert.equal(nameError("Ada Lovelace"), "");
  assert.equal(nameError("  Ada  "), "");
  assert.equal(nameError("Zoë Ångström-李"), "");
  assert.equal(nameError("a".repeat(NAME_MAX)), "");
  assert.equal(nameError("🧠".repeat(NAME_MAX)), "", "characters, not UTF-16 units");
  assert.match(nameError(""), /visible character/);
  assert.match(nameError("   "), /visible character/);
  assert.match(nameError("a".repeat(NAME_MAX + 1)), /64/);
  for (const bad of ["Ada\u0000", "Ada\nLovelace", "Ada\u202Eecalevol", "Ada\u200B", "A\uFEFFda"]) {
    assert.match(nameError(bad), /invisible|control/, JSON.stringify(bad));
  }
});

test("the name requests: trimmed, refused before sending, null resets", () => {
  assert.deepEqual(displayNameRequest("  Ada "), { method: "PATCH", path: "/v1/me", body: { display_name: "Ada" } });
  assert.deepEqual(displayNameRequest(null), { method: "PATCH", path: "/v1/me", body: { display_name: null } });
  assert.throws(() => displayNameRequest("   "));
  assert.deepEqual(workspaceNameRequest(" Kestrel Labs "), {
    method: "PATCH",
    path: "/v1/workspaces/current",
    body: { name: "Kestrel Labs" },
  });
  assert.throws(() => workspaceNameRequest("x".repeat(65)));
});

test("an avatar upload is the raw image with its own type, PNG/JPEG/WebP only, ≤ 512 KB", () => {
  const png = { type: "image/png", size: 1000 };
  assert.deepEqual(avatarUploadRequest(png), { method: "PUT", path: "/v1/me/avatar", raw: png, contentType: "image/png" });
  assert.equal(avatarUploadRequest({ type: "image/webp", size: 1 }, "workspace").path, "/v1/workspaces/current/avatar");
  assert.throws(() => avatarUploadRequest({ type: "image/svg+xml", size: 10 }), /PNG, JPEG or WebP/);
  assert.throws(() => avatarUploadRequest({ type: "image/gif", size: 10 }));
  assert.throws(() => avatarUploadRequest({ type: "image/png", size: AVATAR_MAX_BYTES + 1 }), /512 KB/);
  assert.deepEqual(avatarRemoveRequest(), { method: "DELETE", path: "/v1/me/avatar" });
  assert.deepEqual(avatarRemoveRequest("workspace"), { method: "DELETE", path: "/v1/workspaces/current/avatar" });
  assert.equal(avatarFileError({ type: "image/jpeg" }), "");
  assert.match(avatarFileError({ type: "image/svg+xml" }), /PNG, JPEG or WebP/);
  assert.match(avatarFileError(null), /Choose/);
});

test("api sends a raw body as it is, with its own type, and JSON otherwise", async () => {
  const calls = [];
  const original = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    calls.push({ url: String(url), init });
    return new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } });
  };
  try {
    const image = new Blob([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], { type: "image/png" });
    const request = avatarUploadRequest(image);
    await api(request.path, { method: request.method, raw: request.raw, contentType: request.contentType });
    assert.equal(calls[0].init.method, "PUT");
    assert.equal(calls[0].init.body, image, "the image itself, not JSON");
    assert.equal(calls[0].init.headers["Content-Type"], "image/png");
    assert.equal(calls[0].init.credentials, "same-origin");
    await api("/v1/me", { method: "PATCH", body: { display_name: "Ada" } });
    assert.equal(calls[1].init.headers["Content-Type"], "application/json");
    assert.equal(calls[1].init.body, JSON.stringify({ display_name: "Ada" }));
  } finally {
    globalThis.fetch = original;
  }
});

test("initials: first and last word, or the first letter of one", () => {
  assert.equal(initials("Ada Lovelace"), "AL");
  assert.equal(initials("Ada King Lovelace"), "AL");
  assert.equal(initials("ada"), "A");
  assert.equal(initials("  zoë  ångström "), "ZÅ");
  assert.equal(initials("李 小龍"), "李小");
  assert.equal(initials("(Kestrel) Labs!"), "KL");
  assert.equal(initials(""), "?");
  assert.equal(initials("!!! ..."), "?");
  assert.equal(initials(null), "?");
});

test("a person's colour is fixed by their id and spread over every tone", () => {
  assert.equal(avatarTone("usr_01ABC"), avatarTone("usr_01ABC"));
  const seen = new Set();
  for (let i = 0; i < 200; i += 1) {
    const tone = avatarTone(`usr_${i}`);
    assert.ok(Number.isInteger(tone) && tone >= 0 && tone < AVATAR_TONES);
    seen.add(tone);
  }
  assert.equal(seen.size, AVATAR_TONES, "every tone is used");
});

test("only a same-origin avatar path is ever an src", () => {
  assert.equal(safeAvatarUrl(AVATAR), AVATAR);
  for (const bad of [
    "https://evil.example/a.png",
    "//evil.example/v1/avatars/av_01HZZZBBBBBBBBBBBBBBBBBBBB",
    "javascript:alert(1)",
    "data:image/svg+xml,<svg/>",
    "/v1/avatars/../tokens",
    "/v1/avatars/av_lower",
    `${AVATAR}?x=1`,
    null,
    42,
  ]) {
    assert.equal(safeAvatarUrl(bad), "", String(bad));
  }
});

test("paintAvatar: the picture when there is one, initials when not or when it fails", async () => {
  const doc = stubDoc();
  const el = avatarNode(doc, { name: "Ada Lovelace", id: "usr_1" }, "avatar--sm");
  assert.equal(el.attrs["aria-hidden"], "true");
  assert.equal(el.textContent, "AL");
  assert.match(el.className, new RegExp(`\\bavatar--t${avatarTone("usr_1")}\\b`));
  assert.match(el.className, /\bavatar--sm\b/);

  paintAvatar(el, { name: "Ada Lovelace", id: "usr_1", url: AVATAR }, doc);
  assert.equal(el.children.length, 1);
  const [img] = el.children;
  assert.equal(img.tagName, "IMG");
  assert.equal(img.attrs.src, AVATAR);
  assert.equal(img.attrs.alt, "", "decorative: the name beside it is read");
  assert.match(el.className, /\bavatar--img\b/);
  assert.doesNotMatch(el.className, /avatar--t\d/);

  // A picture that does not load falls back to the initials.
  for (const fn of img.listeners.error || []) fn();
  assert.equal(el.textContent, "AL");
  assert.equal(el.children.length, 0);

  // A URL that is not ours is no picture at all.
  paintAvatar(el, { name: "Mallory", id: "usr_2", url: "https://evil.example/x.png" }, doc);
  assert.equal(el.children.length, 0);
  assert.equal(el.textContent, "M");
});

test("profileView: what the editors show, and who may edit the workspace", () => {
  const view = profileView({
    workspace: { id: "T0", name: "Kestrel", avatar_url: AVATAR },
    member: { user_id: "usr_1", name: "Ada", display_name: "Ada", avatar_url: null, role: "member", is_admin: false },
    is_owner: false,
  });
  assert.equal(view.name, "Ada");
  assert.equal(view.displayName, "Ada");
  assert.equal(view.avatarUrl, "");
  assert.equal(view.workspaceAvatarUrl, AVATAR);
  assert.equal(view.canEditWorkspace, false);
  for (const me of [
    { member: { role: "owner" } },
    { member: { role: "admin" } },
    { member: { role: "member", is_admin: true } },
    { member: { role: "member" }, is_owner: true },
  ]) {
    assert.equal(profileView(me).canEditWorkspace, true, JSON.stringify(me));
  }
  // An older server without the fields still reads.
  const old = profileView({ workspace: { id: "T0", name: "" }, member: { user_id: "usr_1", name: "" } });
  assert.equal(old.displayName, "");
  assert.equal(old.workspaceName, "Your workspace");
  assert.equal(old.name, "usr_1");
});

test("the session view carries the avatars to the top bar", () => {
  const view = sessionView(
    { workspace: { id: "T0", name: "Acme", avatar_url: AVATAR }, member: { user_id: "u1", name: "Ada", avatar_url: AVATAR } },
    "",
  );
  assert.equal(view.avatarUrl, AVATAR);
  assert.equal(view.workspaceAvatarUrl, AVATAR);
  assert.equal(view.userId, "u1");
});

test("profile errors say what to do", () => {
  assert.match(profileErrorMessage({ status: 403 }, "rename the workspace"), /role/);
  assert.match(profileErrorMessage({ status: 413 }, "save your picture"), /512 KB/);
  assert.match(profileErrorMessage({ status: 415 }, "save your picture"), /PNG, JPEG or WebP/);
  assert.match(profileErrorMessage({ status: 422, body: { detail: "A name is at most 64 characters." } }, "x"), /64/);
});

test("the crop is the centred square, scaled to 256, encoded WebP, else PNG", async () => {
  assert.deepEqual(squareCrop(400, 300), { sx: 50, sy: 0, side: 300 });
  assert.deepEqual(squareCrop(300, 500), { sx: 0, sy: 100, side: 300 });
  const drawn = [];
  const asked = [];
  const canvas = {
    width: 0,
    height: 0,
    getContext: () => ({ drawImage: (...args) => drawn.push(args) }),
    // A browser that cannot encode WebP answers PNG for it.
    toBlob(done, type) {
      asked.push(type);
      done({ type: type === "image/webp" ? "image/png" : type, size: 2048 });
    },
  };
  let closed = false;
  const { blob } = await cropToSquare({ type: "image/jpeg" }, canvas, {
    createBitmap: async () => ({ width: 400, height: 300, close: () => (closed = true) }),
  });
  assert.equal(canvas.width, 256);
  assert.equal(canvas.height, 256);
  assert.deepEqual(drawn[0].slice(1), [50, 0, 300, 300, 0, 0, 256, 256]);
  assert.deepEqual(asked, ["image/webp", "image/png"]);
  assert.equal(blob.type, "image/png");
  assert.equal(closed, true);
  await assert.rejects(
    cropToSquare({ type: "image/png" }, canvas, { createBitmap: async () => ({ width: 0, height: 10 }) }),
    /no pixels/,
  );
});

test("profile.js builds no markup from strings and stores nothing", () => {
  const code = readFileSync(new URL("../assets/profile.js", import.meta.url), "utf8")
    .split("\n")
    .filter((line) => !line.trim().startsWith("//") && !line.trim().startsWith("*"))
    .join("\n");
  for (const forbidden of ["innerHTML", "insertAdjacentHTML", "document.write", "localStorage", "sessionStorage", "gravatar"]) {
    assert.ok(!code.includes(forbidden), forbidden);
  }
});

test("settings has both editors, the workspace's heads the Members tab, and the CSP stayed tight", () => {
  const html = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
  for (const prefix of ["profile", "workspace"]) {
    for (const id of ["avatar", "preview", "file", "avatar-save", "avatar-cancel", "avatar-remove", "form", "name", "save"]) {
      assert.match(html, new RegExp(`id="${prefix}-${id}"`), `#${prefix}-${id}`);
    }
    assert.match(html, new RegExp(`id="${prefix}-file"[\\s\\S]*?accept="image/png,image/jpeg,image/webp"`));
    // The preview is a canvas, so no blob: URL is needed.
    assert.match(html, new RegExp(`<canvas\\s+id="${prefix}-preview"`));
  }
  assert.equal(settingsTab("#workspace"), "members");
  const headers = readFileSync(new URL("../_headers", import.meta.url), "utf8");
  assert.match(headers, /img-src 'self' data:;/, "no blob:, no remote image hosts");
});
