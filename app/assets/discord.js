// The Discord connection in Settings → Integrations: the pure half (the
// requests, what the card shows for each state, the outcome banner) and the
// mount that paints it. The card is shown only when the server says Discord is
// configured; otherwise its section stays hidden, with nothing said about it.
// Every row is painted with elements and text nodes only, as everywhere in `app/`.
//
// Nothing here reads history: the server's cron imports it, and there is no
// button for that. The routes are in docs/discord.md.

/** Every Discord settings route hangs off this path. */
export const DISCORD_PATH = "/v1/discord";

/** Where the browser goes to add the bot: a server-side redirect to Discord. */
export const CONNECT_PATH = `${DISCORD_PATH}/connect`;

/** A Discord snowflake: digits only. */
const SNOWFLAKE = /^[0-9]{1,20}$/;

/** `PATCH /v1/discord/{guild}/channels/{channel}`: include or exclude one channel. */
export function channelRequest(guildId, channelId, included) {
  if (!SNOWFLAKE.test(String(guildId)) || !SNOWFLAKE.test(String(channelId))) {
    throw new Error("That channel is not one Living Brain can change.");
  }
  return {
    method: "PATCH",
    path: `${DISCORD_PATH}/${guildId}/channels/${channelId}`,
    body: { included: Boolean(included) },
  };
}

/** `DELETE /v1/discord/{guild}`: stop reading the server. */
export function disconnectRequest(guildId) {
  if (!SNOWFLAKE.test(String(guildId))) throw new Error("That server is not one Living Brain can disconnect.");
  return { method: "DELETE", path: `${DISCORD_PATH}/${guildId}` };
}

/** A count with thousands separators, the way the page writes numbers. */
export function count(value) {
  const n = Math.max(0, Math.floor(Number(value) || 0));
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** What a channel shows as, in a sentence. */
export const CHANNEL_STATUS_TEXT = Object.freeze({
  waiting: "Waiting to start",
  backfilling: "Importing history",
  live: "Up to date",
  unavailable: "Discord would not let the bot read this channel",
  removed: "No longer in the server",
});

/** One channel's status sentence; an unknown status says nothing. */
export function channelStatusText(status) {
  return Object.prototype.hasOwnProperty.call(CHANNEL_STATUS_TEXT, status) ? CHANNEL_STATUS_TEXT[status] : "";
}

/**
 * The line under a server's name: how many messages are in, and what is being
 * read now. `progress` is the server's `progress` object.
 */
export function progressLine(progress = {}) {
  const imported = Number(progress.messages_imported) || 0;
  const now = progress.backfilling ? String(progress.backfilling) : "";
  if (imported === 0 && !now) return "Nothing imported yet. The first messages arrive within a few minutes.";
  const head = `${count(imported)} messages imported`;
  return now ? `${head} · importing #${now}` : head;
}

/** The outcome banner for `?discord=<outcome>&reason=<reason>`, or null. */
export function outcomeBanner(search = "") {
  const params = new URLSearchParams(String(search || "").replace(/^\?/, ""));
  const outcome = params.get("discord");
  if (!outcome) return null;
  if (outcome === "connected") {
    return {
      tone: "good",
      text: "Discord is connected. Its history is imported in the background; progress shows below.",
    };
  }
  if (outcome === "error") {
    const reason = params.get("reason") || "";
    const known = Object.prototype.hasOwnProperty.call(ERROR_TEXT, reason);
    return { tone: "bad", text: known ? ERROR_TEXT[reason] : "Discord setup did not finish. Try again." };
  }
  return null;
}

/** What each `discord=error` reason means to the person who sees it. */
export const ERROR_TEXT = Object.freeze({
  "signed-out": "Sign in to finish connecting Discord.",
  "bad-request": "Discord sent back a request Living Brain could not read. Start again from here.",
  cancelled: "Discord setup was cancelled, so nothing was connected.",
  state: "That Discord step expired or was already used. Start it again.",
  forbidden: "Only a workspace owner or admin can connect Discord.",
  "not-in-server":
    "The Living Brain bot is not in that server. Add it from the Connect Discord button, then try again.",
  "bound-elsewhere": "That Discord server is already connected to another workspace.",
  discord: "Discord did not answer as expected. Try again in a moment.",
  store: "Living Brain could not save the Discord connection. Try again.",
  "not-configured": "Discord isn't available here yet.",
});

/** One channel row, as the card paints it. */
export function channelView(channel) {
  const name = String((channel && channel.name) || "");
  return {
    channelId: String((channel && channel.channel_id) || ""),
    label: channel && channel.thread ? `${name} (thread)` : `#${name}`,
    included: Boolean(channel && channel.included),
    status: channelStatusText(channel && channel.status),
    messages: count(channel && channel.messages),
    error: channel && channel.last_error ? String(channel.last_error) : "",
  };
}

/** One server's block: its name, its state, its progress, and its channels. */
export function connectionView(connection) {
  const status = String((connection && connection.status) || "");
  const connected = status === "connected";
  const error = connection && connection.last_error ? String(connection.last_error) : "";
  return {
    guildId: String((connection && connection.guild_id) || ""),
    name: String((connection && connection.guild_name) || "Discord server"),
    connected,
    line: !connected
      ? "Disconnected. Nothing is read from this server."
      : connection.listed === false
        ? "Finding the server's channels. This takes a few minutes."
        : progressLine(connection.progress),
    error,
    channels: connected && Array.isArray(connection.channels) ? connection.channels.map(channelView) : [],
  };
}

/**
 * Everything the card shows, from `GET /v1/discord`. `shown` is false when
 * Discord is not configured here: the section then stays hidden.
 */
export function discordView(data) {
  const payload = data && typeof data === "object" ? data : {};
  const connections = Array.isArray(payload.connections) ? payload.connections.map(connectionView) : [];
  return { shown: payload.configured === true, connections };
}

/** Shows or hides several elements that may be missing from the page. */
function setHidden(elements, hidden) {
  for (const element of elements) if (element) element.hidden = hidden;
}

/** A plain element with its text, painted with `createElement`. */
function el(doc, tag, text, attrs = {}) {
  const node = doc.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  if (text != null && text !== "") node.textContent = String(text);
  return node;
}

/**
 * Mounts the Discord card: reads `GET /v1/discord` and paints the servers, with
 * a checkbox per channel and a disconnect button. A non-admin (403) or an
 * unconfigured deployment hides the whole section. Resolves after the first
 * paint.
 */
export async function mountDiscord(deps) {
  const doc = deps.document;
  const q = (sel) => doc.querySelector(sel);
  const { api, confirm, notify } = deps;
  const section = q("#discord");
  const status = q("#discord-status");
  const banner = q("#discord-banner");
  const connect = q("#discord-connect");
  const servers = q("#discord-servers");

  const outcome = outcomeBanner(deps.location && deps.location.search);
  if (outcome && banner) {
    notify(banner, outcome.text, outcome.tone);
    try {
      if (deps.history) deps.history.replaceState(null, "", `${deps.location.pathname}#discord`);
    } catch {
      /* keeping the query in the address is harmless */
    }
  }

  const paint = (view) => {
    setHidden([section], !view.shown);
    if (!view.shown) return;
    if (servers) servers.replaceChildren();
    if (view.connections.length === 0 && servers) {
      servers.appendChild(el(doc, "p", "No server is connected yet.", { class: "hint" }));
    }
    for (const connection of view.connections) {
      if (servers) servers.appendChild(serverBlock(connection));
    }
  };

  const serverBlock = (connection) => {
    const block = el(doc, "article", "", { class: "discord-server" });
    block.appendChild(el(doc, "h3", connection.name));
    block.appendChild(el(doc, "p", connection.line, { class: "tiny" }));
    if (connection.error) block.appendChild(el(doc, "p", connection.error, { class: "notice" }));
    if (!connection.connected) return block;
    const list = el(doc, "ul", "", { class: "plain-list discord-channels" });
    for (const channel of connection.channels) {
      const li = el(doc, "li", "");
      const label = el(doc, "label", "");
      const box = el(doc, "input", "", { type: "checkbox", "aria-label": `Import ${channel.label} in ${connection.name}` });
      box.checked = channel.included;
      box.addEventListener("change", async () => {
        const want = box.checked;
        box.disabled = true;
        try {
          const request = channelRequest(connection.guildId, channel.channelId, want);
          await api(request.path, { method: request.method, body: request.body });
          await refresh();
        } catch (error) {
          box.checked = !want;
          box.disabled = false;
          notify(status, `Could not change ${channel.label}: ${(error && error.message) || "the request failed"}.`, "bad");
        }
      });
      label.appendChild(box);
      label.appendChild(doc.createTextNode(` ${channel.label}`));
      li.appendChild(label);
      const detail = [channel.status, channel.messages ? `${channel.messages} messages` : ""].filter(Boolean).join(" · ");
      if (detail) li.appendChild(el(doc, "span", ` — ${detail}`, { class: "tiny" }));
      if (channel.error) li.appendChild(el(doc, "span", ` (${channel.error})`, { class: "tiny" }));
      list.appendChild(li);
    }
    block.appendChild(list);
    const leave = el(doc, "button", "Disconnect", { type: "button" });
    leave.addEventListener("click", async () => {
      const ok = await confirm({
        title: `Disconnect ${connection.name}?`,
        body: "Living Brain stops reading this Discord server. Messages already imported stay. The bot stays in the server until someone removes it on Discord.",
        confirm: "Disconnect",
      });
      if (!ok) return;
      leave.disabled = true;
      try {
        const request = disconnectRequest(connection.guildId);
        await api(request.path, { method: request.method });
        await refresh();
      } catch (error) {
        leave.disabled = false;
        notify(status, `Could not disconnect: ${(error && error.message) || "the request failed"}.`, "bad");
      }
    });
    block.appendChild(leave);
    return block;
  };

  const refresh = async () => {
    const data = await api(DISCORD_PATH);
    const view = discordView(data);
    paint(view);
    return view;
  };

  if (connect) {
    connect.addEventListener("click", () => {
      deps.location.assign(CONNECT_PATH);
    });
  }

  try {
    return await refresh();
  } catch (error) {
    // Not an admin, or the server could not answer: the card is not for this
    // person, so it is hidden rather than showing an error.
    setHidden([section], true);
    return null;
  }
}
