// Names and avatars: the person's and the workspace's.
//
// The pure half — the rules a name follows (the server's, mirrored so the
// form can say so before a round trip), the requests, the initials and the
// colour a person without an avatar gets — and the two small DOM helpers that
// paint an avatar. Every node is built with `createElement` and text nodes,
// never parsed markup, like the rest of `app/`.
//
// No external avatar service is ever asked (no Gravatar): an avatar is either
// the image the person or an admin uploaded, served from this origin to
// members only, or initials drawn here.

/** The longest name, in characters, after trimming — the server's limit. */
export const NAME_MAX = 64;

/** The largest image the server accepts, in bytes. */
export const AVATAR_MAX_BYTES = 512 * 1024;

/** The image types the server accepts. */
export const AVATAR_TYPES = Object.freeze(["image/png", "image/jpeg", "image/webp"]);

/** The side of the square the app crops and scales an upload to. */
export const AVATAR_SIDE = 256;

/** How many avatar colours `app.css` defines (`.avatar--t0` … `--t7`). */
export const AVATAR_TONES = 8;

export const ME_PATH = "/v1/me";
export const MY_AVATAR_PATH = "/v1/me/avatar";
export const WORKSPACE_PATH = "/v1/workspaces/current";
export const WORKSPACE_AVATAR_PATH = "/v1/workspaces/current/avatar";

// Control characters, and the invisible ones that change how text reads: the
// same set the server refuses (`livingbrain-workspaces`, `clean_name`).
const INVISIBLE =
  /[\u0000-\u001F\u007F-\u009F\u00AD\u180E\u200B-\u200F\u202A-\u202E\u2060-\u2064\u2066-\u2069\uFEFF]/u;

/** What is wrong with a name, in words, or "" when it is fine. */
export function nameError(value) {
  const name = String(value == null ? "" : value).trim();
  if (!name) return "A name needs at least one visible character.";
  if (INVISIBLE.test(name)) return "A name cannot hold control characters or invisible formatting characters.";
  if (Array.from(name).length > NAME_MAX) return `A name is at most ${NAME_MAX} characters.`;
  return "";
}

/** `PATCH /v1/me`: the name you are shown by here, or `null` to go back to
 * the one your sign-in reports. Throws on a name the server would refuse. */
export function displayNameRequest(value) {
  if (value === null) return { method: "PATCH", path: ME_PATH, body: { display_name: null } };
  const problem = nameError(value);
  if (problem) throw new Error(problem);
  return { method: "PATCH", path: ME_PATH, body: { display_name: String(value).trim() } };
}

/** `PATCH /v1/workspaces/current`: the workspace's name. Owners and admins. */
export function workspaceNameRequest(value) {
  const problem = nameError(value);
  if (problem) throw new Error(problem);
  return { method: "PATCH", path: WORKSPACE_PATH, body: { name: String(value).trim() } };
}

/** The upload request for an image `blob`, to the person's or the workspace's
 * avatar. The body is the raw image with its own type, never JSON. */
export function avatarUploadRequest(blob, target = "me") {
  const type = blob && blob.type;
  if (!AVATAR_TYPES.includes(type)) throw new Error("An avatar is a PNG, JPEG or WebP image.");
  if (typeof blob.size === "number" && blob.size > AVATAR_MAX_BYTES) throw new Error("An avatar is at most 512 KB.");
  return {
    method: "PUT",
    path: target === "workspace" ? WORKSPACE_AVATAR_PATH : MY_AVATAR_PATH,
    raw: blob,
    contentType: type,
  };
}

/** The request that goes back to initials. */
export function avatarRemoveRequest(target = "me") {
  return { method: "DELETE", path: target === "workspace" ? WORKSPACE_AVATAR_PATH : MY_AVATAR_PATH };
}

/** Whether a picked file can be an avatar at all, before it is cropped: an
 * image of one of the three types. Its size is checked after the crop, which
 * is what is sent. */
export function avatarFileError(file) {
  if (!file) return "Choose an image.";
  if (!AVATAR_TYPES.includes(file.type)) return "Choose a PNG, JPEG or WebP image.";
  return "";
}

/** Up to two initials: the first letters of the first and last words, or
 * the first letter of a single word. Letters and digits only; "?" when there
 * are none. */
export function initials(name) {
  const words = String(name == null ? "" : name)
    .trim()
    .split(/\s+/u)
    .map((word) => Array.from(word).filter((ch) => /[\p{L}\p{N}]/u.test(ch)))
    .filter((letters) => letters.length);
  if (!words.length) return "?";
  const first = words[0];
  const pick = words.length > 1 ? [first[0], words[words.length - 1][0]] : [first[0]];
  return pick.join("").toLocaleUpperCase();
}

/** A colour for someone with no avatar: the same id is always the same of the
 * `AVATAR_TONES`, so a person keeps their colour on every page and device.
 * FNV-1a over the id's UTF-16 units. */
export function avatarTone(id) {
  let hash = 0x811c9dc5;
  const text = String(id == null ? "" : id);
  for (let i = 0; i < text.length; i += 1) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash % AVATAR_TONES;
}

/** Only a same-origin avatar path is ever put in an `src`: the server answers
 * `/v1/avatars/av_…`, and anything else (another host, `javascript:`, a data
 * URL) is treated as no avatar. */
export function safeAvatarUrl(url) {
  const value = String(url == null ? "" : url);
  return /^\/v1\/avatars\/av_[0-9A-Z]{26}$/.test(value) ? value : "";
}

/**
 * Paints an avatar into `el` (replacing what it held): the image when there is
 * a usable URL, the initials on a deterministic colour otherwise — and the
 * initials again if the image fails to load. Decorative: the name beside it
 * is what is read out, so the image has an empty `alt`.
 */
export function paintAvatar(el, { name, id, url } = {}, doc = typeof document === "undefined" ? null : document) {
  if (!el || !doc) return el;
  const tone = avatarTone(id || name || "");
  const base = String(el.className || "")
    .split(/\s+/)
    .filter((cls) => cls && !/^avatar--t\d$/.test(cls) && cls !== "avatar--img");
  if (!base.includes("avatar")) base.push("avatar");
  const showInitials = () => {
    el.className = [...base, `avatar--t${tone}`].join(" ");
    // `textContent` drops whatever the element held, image included.
    el.textContent = initials(name);
  };
  const src = safeAvatarUrl(url);
  if (!src) {
    showInitials();
    return el;
  }
  const img = doc.createElement("img");
  img.setAttribute("alt", "");
  img.setAttribute("src", src);
  img.setAttribute("decoding", "async");
  img.setAttribute("loading", "lazy");
  img.setAttribute("referrerpolicy", "no-referrer");
  if (typeof img.addEventListener === "function") img.addEventListener("error", showInitials, { once: true });
  el.className = [...base, "avatar--img"].join(" ");
  el.textContent = "";
  el.appendChild(img);
  return el;
}

/** A new avatar element (`<span class="avatar …">`), painted. */
export function avatarNode(doc, person, className = "") {
  if (!doc) return null;
  const el = doc.createElement("span");
  el.className = className ? `avatar ${className}` : "avatar";
  el.setAttribute("aria-hidden", "true");
  return paintAvatar(el, person, doc);
}

/**
 * The profile a page shows, from a `/v1/workspaces/me` answer: the member's
 * name (the one everybody reads), the one they chose (or ""), their avatar,
 * and the same for the workspace, with whether they may change it.
 */
export function profileView(me) {
  const member = (me && me.member) || {};
  const workspace = (me && me.workspace) || {};
  const role = member.role;
  return {
    userId: String(member.user_id || ""),
    name: String(member.name || "").trim() || String(member.user_id || "") || "You",
    displayName: typeof member.display_name === "string" ? member.display_name : "",
    avatarUrl: safeAvatarUrl(member.avatar_url),
    workspaceId: String(workspace.id || ""),
    workspaceName: String(workspace.name || "").trim() || "Your workspace",
    workspaceAvatarUrl: safeAvatarUrl(workspace.avatar_url),
    canEditWorkspace: Boolean(me && (me.is_owner || role === "owner" || role === "admin" || member.is_admin)),
  };
}

/** What a failed profile write says. */
export function profileErrorMessage(error, what) {
  if (error && error.status === 401) return `You are signed out — sign in again to ${what}.`;
  if (error && error.status === 403) return `You can't ${what}: your role does not allow it.`;
  if (error && error.status === 413) return "That image is larger than 512 KB, even after cropping. Try a smaller one.";
  if (error && error.status === 415) return "That file is not a PNG, JPEG or WebP image.";
  if (error && error.status === 503) return "Avatars are not available on this server yet.";
  const detail = error && error.body && (error.body.detail || error.body.title);
  return `Could not ${what}: ${detail || (error && error.message) || String(error)}`;
}

/** The centred square of a `width`×`height` image: the crop the preview and
 * the upload both use. */
export function squareCrop(width, height) {
  const side = Math.max(0, Math.min(Number(width) || 0, Number(height) || 0));
  return { sx: Math.floor((width - side) / 2), sy: Math.floor((height - side) / 2), side };
}

/**
 * Crops `file` to its centred square, scaled to `AVATAR_SIDE`, on `canvas` —
 * which is also the preview, so nothing needs a `blob:` URL and the page's
 * `img-src` stays `'self' data:`. Answers the canvas and the image to upload:
 * WebP where the browser encodes it, PNG otherwise, and JPEG if that is still
 * over the limit. Re-encoding through a canvas is also what leaves the
 * original's metadata (EXIF, GPS) behind before anything is sent; the server
 * strips it again regardless.
 */
export async function cropToSquare(file, canvas, { createBitmap = globalThis.createImageBitmap } = {}) {
  if (typeof createBitmap !== "function") throw new Error("This browser cannot read the image.");
  const bitmap = await createBitmap(file);
  const { sx, sy, side } = squareCrop(bitmap.width, bitmap.height);
  if (!side) throw new Error("The image has no pixels.");
  canvas.width = AVATAR_SIDE;
  canvas.height = AVATAR_SIDE;
  const context = canvas.getContext("2d");
  context.imageSmoothingQuality = "high";
  context.drawImage(bitmap, sx, sy, side, side, 0, 0, AVATAR_SIDE, AVATAR_SIDE);
  if (typeof bitmap.close === "function") bitmap.close();
  const encode = (type, quality) =>
    new Promise((resolve) => canvas.toBlob((blob) => resolve(blob), type, quality));
  let blob = await encode("image/webp", 0.9);
  if (!blob || blob.type !== "image/webp") blob = await encode("image/png");
  if (blob && blob.size > AVATAR_MAX_BYTES) blob = await encode("image/jpeg", 0.85);
  if (!blob) throw new Error("The image could not be prepared.");
  return { canvas, blob };
}
