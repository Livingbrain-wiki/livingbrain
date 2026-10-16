// The device sign-in approval page (`livingbrain login`, the Mac app's
// "Add Workspace…"): a token is minted for the workspace this browser is
// signed in to, so the page says which one and lets the person pick another
// before they approve. Picking switches this browser's session (the same
// `POST /v1/workspaces/switch` the app's switcher uses) and reloads the page,
// whose code stays pending. Without this script the page still works; it just
// approves for the current workspace.

import { workspacesRequest, switchRequest } from "./members.js";

/** The list `GET /v1/workspaces` answers, reduced to what the picker shows. */
export function workspaceChoices(list) {
  if (!Array.isArray(list)) return [];
  return list
    .filter((item) => item && typeof item.id === "string" && item.id)
    .map((item) => ({
      id: item.id,
      name: typeof item.name === "string" && item.name.trim() ? item.name.trim() : item.id,
      current: item.current === true,
    }));
}

/** Paints the workspace row: plain text for one workspace, a picker for more. */
export function renderWorkspacePicker(slot, choices, onPick, doc = document) {
  if (!slot || choices.length === 0) return;
  slot.textContent = "";
  if (choices.length === 1) {
    slot.textContent = choices[0].name;
    return;
  }
  const select = doc.createElement("select");
  select.id = "device-workspace-pick";
  select.setAttribute("aria-label", "Workspace this token is for");
  for (const choice of choices) {
    const option = doc.createElement("option");
    option.value = choice.id;
    option.textContent = choice.name;
    option.selected = choice.current;
    select.appendChild(option);
  }
  select.addEventListener("change", () => {
    select.disabled = true;
    onPick(select.value);
  });
  slot.appendChild(select);
}

async function call({ method, path, body }) {
  const response = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body ? { "content-type": "application/json", accept: "application/json" } : { accept: "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

async function main() {
  const slot = document.getElementById("device-workspace");
  if (!slot) return;
  let choices;
  try {
    choices = workspaceChoices(await call(workspacesRequest()));
  } catch {
    return;
  }
  renderWorkspacePicker(slot, choices, async (id) => {
    try {
      await call(switchRequest(id));
      location.reload();
    } catch {
      slot.textContent = "Could not switch workspace. Reload and try again.";
    }
  });
}

if (typeof document !== "undefined") main();
