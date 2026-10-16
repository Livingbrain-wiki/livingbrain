import { avatarNode, nameError } from "./profile.js";

// Members, invitations and the workspace switcher — the pure half, next to
// `tokens.js` (ADR 0004). Requests are described here and sent by `app.js`;
// the rows are painted with elements and text nodes only, as everywhere in
// `app/`.

/** Every member and invitation route hangs off this path. */
export const WORKSPACES_PATH = "/v1/workspaces";

/** The roles a person can be given here, in order of reach. */
export const ROLES = Object.freeze([
  { value: "member", label: "Member", detail: "Reads and writes the shared wiki." },
  { value: "admin", label: "Admin", detail: "Also invites, removes and re-roles members." },
  { value: "owner", label: "Owner", detail: "Everything, including other owners." },
]);

/** A role in words; anything unknown reads as itself. */
export function roleLabel(role) {
  const found = ROLES.find((r) => r.value === role);
  if (found) return found.label;
  if (role === "viewer") return "Viewer";
  return String(role || "");
}

/** Whether a signed-in member (the `/me` answer) may manage the roster. */
export function canManage(me) {
  const role = me && me.member && me.member.role;
  return role === "owner" || role === "admin" || Boolean(me && me.member && me.member.is_admin);
}

/** The roles `me` may hand out: only an owner makes owners. */
export function assignableRoles(me) {
  const owner = me && me.member && me.member.role === "owner";
  return ROLES.filter((r) => owner || r.value !== "owner");
}

/** `GET /v1/workspaces/members`. */
export function membersRequest() {
  return { method: "GET", path: `${WORKSPACES_PATH}/members` };
}

/** `GET /v1/workspaces/invites`. */
export function invitesRequest() {
  return { method: "GET", path: `${WORKSPACES_PATH}/invites` };
}

/** `GET /v1/workspaces` — the workspaces this person can switch to. */
export function workspacesRequest() {
  return { method: "GET", path: WORKSPACES_PATH };
}

const ADDRESS = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

/** The invite request. Throws on an address that cannot be one. */
export function inviteRequest(email, role) {
  const address = String(email == null ? "" : email).trim();
  if (!ADDRESS.test(address)) throw new Error("an invitation needs an email address");
  const chosen = ROLES.some((r) => r.value === role) ? role : "member";
  return { method: "POST", path: `${WORKSPACES_PATH}/invites`, body: { email: address, role: chosen } };
}

function idPath(kind, id) {
  const value = String(id == null ? "" : id).trim();
  if (!value) throw new Error(`a ${kind} needs an id`);
  return encodeURIComponent(value);
}

/** Change a member's role. The id is encoded, so it cannot walk out of the path. */
export function roleChangeRequest(userId, role) {
  if (!ROLES.some((r) => r.value === role)) throw new Error("unknown role");
  return {
    method: "PATCH",
    path: `${WORKSPACES_PATH}/members/${idPath("member", userId)}`,
    body: { role },
  };
}

/** Remove a member (or leave, when it is you). */
export function removeRequest(userId) {
  return { method: "DELETE", path: `${WORKSPACES_PATH}/members/${idPath("member", userId)}` };
}

/** Withdraw an invitation. */
export function withdrawRequest(id) {
  return { method: "DELETE", path: `${WORKSPACES_PATH}/invites/${idPath("invitation", id)}` };
}

/** `POST /v1/workspaces`: a new workspace you own, which the session moves
 * into. Throws on a name the server would refuse. */
export function createWorkspaceRequest(name) {
  const problem = nameError(name);
  if (problem) throw new Error(problem);
  return { method: "POST", path: WORKSPACES_PATH, body: { name: String(name).trim() } };
}

/**
 * `DELETE /v1/workspaces/current` (ADR 0004, "Deleting a workspace"): the
 * owner deletes the workspace they are in. The body carries the name they
 * typed, and the server refuses anything but the exact name.
 */
export function deleteWorkspaceRequest(confirm) {
  return {
    method: "DELETE",
    path: `${WORKSPACES_PATH}/current`,
    body: { confirm: String(confirm == null ? "" : confirm) },
  };
}

/**
 * Whether the typed name is the workspace's name, exactly: the only thing
 * that enables the delete button. An empty name never matches.
 */
export function deleteConfirmMatches(typed, name) {
  const expected = String(name == null ? "" : name);
  return expected !== "" && String(typed == null ? "" : typed) === expected;
}

/**
 * What the danger zone shows for this person. Only an owner sees it at all.
 * `list` is `GET /v1/workspaces`, which includes the workspace they are in:
 * with no other workspace to go to, the zone is replaced by a line saying so.
 */
export function deleteZone(me, list) {
  const owner = Boolean(me && me.member && me.member.role === "owner");
  if (!owner) return { owner: false, show: false, onlyWorkspace: false };
  const count = Array.isArray(list) ? list.length : 0;
  return { owner: true, show: count > 1, onlyWorkspace: count <= 1 };
}

/** The words for a failed deletion, by what the server said. */
export function deleteErrorMessage(error) {
  if (error && error.status === 400) {
    return "The name does not match. Type the workspace name exactly as it is shown.";
  }
  if (error && error.status === 409) {
    return "This is your only workspace. Join or create another workspace first, then delete this one.";
  }
  return memberErrorMessage(error, "delete the workspace");
}

/** Switch the session to another workspace. */
export function switchRequest(workspaceId) {
  idPath("workspace", workspaceId);
  return {
    method: "POST",
    path: `${WORKSPACES_PATH}/switch`,
    body: { workspace_id: String(workspaceId).trim() },
  };
}

/** One member row's view: named fields only. */
export function memberView(entry) {
  return {
    userId: String((entry && entry.user_id) || ""),
    name: String((entry && entry.name) || "") || "Unnamed member",
    role: String((entry && entry.role) || "member"),
    avatarUrl: String((entry && entry.avatar_url) || ""),
  };
}

/** The name cell: the member's picture (or initials) and their name, as text,
 * in one wrapper so the cell itself stays a table cell. */
function personCell(doc, view) {
  const td = doc.createElement("td");
  td.setAttribute("data-label", "Name");
  const person = doc.createElement("span");
  person.className = "person";
  const pic = avatarNode(doc, { name: view.name, id: view.userId, url: view.avatarUrl }, "avatar--sm");
  if (pic) person.appendChild(pic);
  const name = doc.createElement("span");
  name.textContent = view.name;
  person.appendChild(name);
  td.appendChild(person);
  return td;
}

/** The roster, as views. A non-array answer is an empty roster. */
export function memberViews(response) {
  return (Array.isArray(response) ? response : []).map(memberView);
}

/** One open invitation's view. */
export function inviteView(entry) {
  const expires = String((entry && entry.expires_at) || "");
  return {
    id: String((entry && entry.id) || ""),
    email: String((entry && entry.email) || ""),
    role: String((entry && entry.role) || "member"),
    expires: expires ? expires.slice(0, 10) : "—",
  };
}

export function inviteViews(response) {
  return (Array.isArray(response) ? response : []).map(inviteView);
}

/** What the notice says when a member call fails. */
export function memberErrorMessage(error, what) {
  if (error && error.status === 401) return "You are signed out — sign in again to manage members.";
  if (error && error.status === 403) return `You can't ${what}: your role does not allow it.`;
  if (error && error.status === 409) {
    const title = error.body && error.body.title;
    return title ? `Could not ${what}: ${title}.` : `Could not ${what}: it conflicts with the current state.`;
  }
  return `Could not ${what}: ${(error && error.message) || String(error)}`;
}

function cell(doc, text, label) {
  const td = doc.createElement("td");
  td.textContent = text;
  if (label) td.setAttribute("data-label", label);
  return td;
}

function button(doc, text, attrs, danger) {
  const b = doc.createElement("button");
  b.type = "button";
  b.className = danger ? "btn-sm btn-danger" : "btn-sm";
  b.textContent = text;
  for (const [name, value] of Object.entries(attrs)) b.setAttribute(name, value);
  return b;
}

/**
 * Paints the roster. A manager gets a role picker and a Remove button per row
 * (an admin's picker cannot reach owners); everyone gets a Leave button on
 * their own row.
 */
export function renderMemberRows(container, views, me, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const manage = canManage(me);
  const self = me && me.member && me.member.user_id;
  const myRole = me && me.member && me.member.role;
  const roles = assignableRoles(me);
  for (const view of Array.isArray(views) ? views : []) {
    const tr = doc.createElement("tr");
    tr.appendChild(personCell(doc, view));
    const roleCell = doc.createElement("td");
    roleCell.setAttribute("data-label", "Role");
    const touchable = manage && (view.role !== "owner" || myRole === "owner");
    if (touchable) {
      const select = doc.createElement("select");
      select.setAttribute("data-role-for", view.userId);
      select.setAttribute("aria-label", `Role of ${view.name}`);
      for (const role of roles) {
        const option = doc.createElement("option");
        option.value = role.value;
        option.textContent = role.label;
        if (role.value === view.role) option.selected = true;
        select.appendChild(option);
      }
      roleCell.appendChild(select);
    } else {
      roleCell.textContent = roleLabel(view.role);
    }
    tr.appendChild(roleCell);
    const actions = doc.createElement("td");
    actions.className = "cell-actions";
    if (view.userId === self) {
      actions.appendChild(button(doc, "Leave", { "data-remove": view.userId, "data-name": view.name, "data-self": "1" }, true));
    } else if (touchable) {
      actions.appendChild(
        button(doc, "Remove", { "data-remove": view.userId, "data-name": view.name, "aria-label": `Remove ${view.name}` }, true),
      );
    }
    tr.appendChild(actions);
    container.appendChild(tr);
  }
  return container;
}

/** Paints the open invitations, each with a Withdraw button. */
export function renderInviteRows(container, views, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = Array.isArray(views) ? views : [];
  if (!list.length) {
    const tr = doc.createElement("tr");
    const td = cell(doc, "No open invitations.");
    td.colSpan = 4;
    tr.appendChild(td);
    container.appendChild(tr);
    return container;
  }
  for (const view of list) {
    const tr = doc.createElement("tr");
    tr.appendChild(cell(doc, view.email, "Address"));
    tr.appendChild(cell(doc, roleLabel(view.role), "Role"));
    tr.appendChild(cell(doc, view.expires, "Expires"));
    const actions = doc.createElement("td");
    actions.className = "cell-actions";
    actions.appendChild(button(doc, "Withdraw", { "data-withdraw": view.id, "data-email": view.email }, true));
    tr.appendChild(actions);
    container.appendChild(tr);
  }
  return container;
}

/** Paints the workspace switcher: the current one marked, the others a button. */
export function renderWorkspaces(container, list, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  for (const item of Array.isArray(list) ? list : []) {
    const li = doc.createElement("li");
    const name = doc.createElement("span");
    name.textContent = `${String(item.name || item.id || "")} · ${roleLabel(item.role)}`;
    li.appendChild(name);
    if (item.current) {
      const here = doc.createElement("span");
      here.className = "muted";
      here.textContent = " (current)";
      li.appendChild(here);
    } else {
      li.appendChild(button(doc, "Switch", { "data-switch": String(item.id || "") }));
    }
    container.appendChild(li);
  }
  return container;
}
