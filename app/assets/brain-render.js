// The brain map's renderer: a 3D graph drawn on a 2D canvas, in the
// livingbrain.wiki brain's style — the same perspective projection, the same
// glow sprites with additive light in the dark theme, the same depth-binned
// edges and travelling pulses. Ported from the website's `assets/brain.js`
// (Livingbrain-wiki/website; see `vendor/VENDOR.md`): the procedural brain
// there becomes the asker's real nodes, laid out by `graph.js`.
//
// No WebGL and no library: a canvas 2D context is everywhere, so there is
// nothing to fall back *from*; "Flat" draws the same nodes face-on without
// perspective or rotation, and a page without canvas support gets the list.

function withAlpha(c, a) {
  const colour = String(c || "#888").trim();
  if (/^(oklch|oklab|lch|lab)\(/.test(colour) && !colour.includes("/")) {
    return colour.replace(/\)\s*$/, ` / ${a})`);
  }
  if (/^#([0-9a-f]{6})$/i.test(colour)) return colour + Math.round(a * 255).toString(16).padStart(2, "0");
  return colour;
}

/**
 * Mounts the renderer on `canvas` for `graph` (`normalizeGraph`'s model)
 * with `layout` (`createLayout`'s). Options: `reduced` (no rotation, no
 * pulses, the layout settled before the first frame), `kinds` (the legend's
 * colour tokens by kind index), `onHover(node|null, x, y)`,
 * `onPick(node)`. Returns the view controls.
 */
export function mountBrain(canvas, graph, layout, options = {}) {
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  const o = {
    reduced: false,
    tokens: [],
    onHover: () => {},
    onPick: () => {},
    ...options,
  };
  const n = graph.nodes.length;
  const pos = layout.positions;
  // Screen-space projection, per node.
  const sx = new Float32Array(n);
  const sy = new Float32Array(n);
  const sp = new Float32Array(n);
  const sd = new Float32Array(n);
  const vis = new Float32Array(n); // 0..1, eased toward the mask
  let mask = new Uint8Array(n).fill(1);
  let focus = new Uint8Array(0); // search matches, glowing
  let selected = -1;
  let hovered = -1;
  const glow = new Float32Array(n);
  // A big brain glows less per node, or the halos merge into fog.
  const halo = Math.min(1, Math.sqrt(260 / Math.max(260, n)));

  let W = 0;
  let H = 0;
  let angle = 0.42;
  let tilt = -0.22;
  let zoom = 1;
  let panX = 0;
  let panY = 0;
  let flat = false;
  let speed = o.reduced ? 0 : 0.045;
  let settling = true;
  // The point the camera looks at (world space), eased toward a target.
  const look = { x: 0, y: 0, z: 0 };
  const lookTo = { x: 0, y: 0, z: 0 };
  let zoomTo = 1;

  let colours = null;
  let sprites = null;
  let light = false;
  let raf = 0;
  let last = 0;
  let dead = false;
  let dirty = true;
  const pulses = [];
  let pulseClock = 0;

  function sprite(colour) {
    const S = 64;
    const s = document.createElement("canvas");
    s.width = S;
    s.height = S;
    const g = s.getContext("2d");
    const gr = g.createRadialGradient(S / 2, S / 2, 0, S / 2, S / 2, S / 2);
    gr.addColorStop(0, colour);
    gr.addColorStop(0.2, withAlpha(colour, 0.5));
    gr.addColorStop(1, withAlpha(colour, 0));
    g.fillStyle = gr;
    g.fillRect(0, 0, S, S);
    return s;
  }

  function readColours() {
    const cs = getComputedStyle(document.documentElement);
    const v = (name, fallback) => cs.getPropertyValue(name).trim() || fallback;
    light = document.documentElement.dataset.theme === "light";
    colours = {
      kinds: o.tokens.map((token) => v(token, "#8899aa")),
      accent: v("--accent", "#5fe0b0"),
      edge: v("--graph-edge", "#7a9aa0"),
      ink: v("--ink", "#eef"),
    };
    sprites = { kinds: colours.kinds.map(sprite), accent: sprite(colours.accent) };
    dirty = true;
  }

  function resize() {
    const r = canvas.getBoundingClientRect();
    const d = Math.min(2, window.devicePixelRatio || 1);
    W = r.width;
    H = r.height;
    canvas.width = Math.max(1, Math.round(W * d));
    canvas.height = Math.max(1, Math.round(H * d));
    dirty = true;
    schedule();
  }

  function project() {
    const ca = Math.cos(flat ? 0 : angle);
    const sa = Math.sin(flat ? 0 : angle);
    const t = flat ? 0 : tilt;
    const ct = Math.cos(t);
    const st = Math.sin(t);
    const sc = Math.min(W, H) * 0.5 * zoom * (flat ? 1.05 : 1);
    const cx = W / 2 + panX;
    const cy = H / 2 + panY;
    const cam = 3.4;
    for (let i = 0; i < n; i += 1) {
      const x = pos[i * 3] - look.x;
      const y = pos[i * 3 + 1] - look.y;
      const z = pos[i * 3 + 2] - look.z;
      // Flat: the brain from above-front, x across and z down the screen,
      // so both hemispheres and their depth read without perspective.
      if (flat) {
        sx[i] = cx + x * sc;
        sy[i] = cy + (z * 0.62 - y * 0.38) * sc;
        sp[i] = 1;
        sd[i] = 0.5 + y * 0.4;
        continue;
      }
      const xr = x * ca + z * sa;
      const zr = -x * sa + z * ca;
      const y2 = y * ct - zr * st;
      const z2 = y * st + zr * ct;
      const p = cam / (cam - z2);
      sx[i] = cx + xr * sc * p;
      sy[i] = cy - y2 * sc * p;
      sp[i] = p;
      sd[i] = Math.max(0, Math.min(1, (z2 + 0.9) / 1.8));
    }
  }

  function radius(i) {
    const node = graph.nodes[i];
    return (0.8 + node.weight * 1.35) * sp[i] * (0.65 + 0.55 * sd[i]) * Math.max(0.6, Math.min(W, H) / 560) * Math.sqrt(zoom);
  }

  function spawnPulse() {
    if (!graph.edges.length) return;
    for (let k = 0; k < 6; k += 1) {
      const edge = graph.edges[(Math.random() * graph.edges.length) | 0];
      if (vis[edge.a] < 0.6 || vis[edge.b] < 0.6) continue;
      const forward = Math.random() < 0.5;
      pulses.push({ a: forward ? edge.a : edge.b, b: forward ? edge.b : edge.a, t: 0, s: 1 / (0.7 + Math.random() * 0.8) });
      return;
    }
  }

  function step(dt) {
    let moving = false;
    if (settling) {
      settling = layout.run(o.reduced ? 0 : 3);
      moving = true;
    }
    if (speed && !flat) {
      angle += speed * dt;
      moving = true;
    }
    // Ease the camera toward its target.
    const e = Math.min(1, dt * (o.reduced ? 60 : 4));
    for (const key of ["x", "y", "z"]) {
      const dv = lookTo[key] - look[key];
      if (Math.abs(dv) > 1e-4) {
        look[key] += dv * e;
        moving = true;
      }
    }
    if (Math.abs(zoomTo - zoom) > 1e-3) {
      zoom += (zoomTo - zoom) * e;
      moving = true;
    }
    for (let i = 0; i < n; i += 1) {
      const target = mask[i] ? 1 : 0;
      if (Math.abs(target - vis[i]) > 0.01) {
        vis[i] += (target - vis[i]) * Math.min(1, o.reduced ? 1 : dt * 5);
        moving = true;
      } else vis[i] = target;
      if (glow[i] > 0.01) {
        glow[i] *= Math.exp(-dt * 2);
        moving = true;
      }
    }
    if (!o.reduced) {
      pulseClock -= dt;
      while (pulseClock < 0) {
        pulseClock += Math.max(0.05, 1.6 / Math.max(1, Math.sqrt(graph.edges.length)));
        if (pulses.length < 40) spawnPulse();
      }
      for (let i = pulses.length - 1; i >= 0; i -= 1) {
        const p = pulses[i];
        p.t += dt * p.s;
        if (p.t >= 1) {
          glow[p.b] = Math.min(1, glow[p.b] + 0.45);
          pulses.splice(i, 1);
        }
      }
      if (pulses.length) moving = true;
    }
    return moving;
  }

  function draw() {
    if (!W || !colours) return;
    const d = Math.min(2, window.devicePixelRatio || 1);
    ctx.setTransform(d, 0, 0, d, 0, 0);
    ctx.clearRect(0, 0, W, H);
    project();
    const add = light ? "source-over" : "lighter";
    const highlight = selected >= 0 || hovered >= 0;
    const near = new Set();
    const anchor = hovered >= 0 ? hovered : selected;
    if (anchor >= 0) {
      near.add(anchor);
      for (const edge of graph.edges) {
        if (edge.a === anchor) near.add(edge.b);
        else if (edge.b === anchor) near.add(edge.a);
      }
    }

    // Edges, in three depth bins, one path each.
    ctx.lineWidth = 0.7;
    ctx.strokeStyle = colours.edge;
    const bins = [[], [], []];
    const lit = [];
    for (const edge of graph.edges) {
      const v = Math.min(vis[edge.a], vis[edge.b]);
      if (v < 0.15) continue;
      if (anchor >= 0 && (edge.a === anchor || edge.b === anchor)) {
        lit.push(edge);
        continue;
      }
      const dd = (sd[edge.a] + sd[edge.b]) / 2;
      bins[dd < 0.4 ? 0 : dd < 0.7 ? 1 : 2].push(edge);
    }
    const ea = light ? [0.14, 0.22, 0.34] : [0.09, 0.17, 0.28];
    bins.forEach((bin, at) => {
      if (!bin.length) return;
      ctx.globalAlpha = ea[at] * (highlight ? 0.45 : 1);
      ctx.beginPath();
      for (const edge of bin) {
        ctx.moveTo(sx[edge.a], sy[edge.a]);
        ctx.lineTo(sx[edge.b], sy[edge.b]);
      }
      ctx.stroke();
    });
    if (lit.length) {
      ctx.globalAlpha = 0.85;
      ctx.strokeStyle = colours.accent;
      ctx.lineWidth = 1.2;
      ctx.beginPath();
      for (const edge of lit) {
        ctx.moveTo(sx[edge.a], sy[edge.a]);
        ctx.lineTo(sx[edge.b], sy[edge.b]);
      }
      ctx.stroke();
    }

    // Nodes, back to front.
    const order = [];
    for (let i = 0; i < n; i += 1) if (vis[i] > 0.02) order.push(i);
    order.sort((a, b) => sd[a] - sd[b]);
    for (const i of order) {
      const node = graph.nodes[i];
      const r = radius(i);
      const dim = highlight && !near.has(i) ? 0.35 : 1;
      const hot = focus[i] || i === selected || i === hovered;
      ctx.globalCompositeOperation = add;
      const gs = r * (light ? 3.2 : 3 + 2.5 * halo) * (1 + glow[i] * 0.8 + (hot ? 0.6 : 0));
      ctx.globalAlpha = Math.min(1, ((light ? 0.08 : 0.12) + (light ? 0.12 : 0.28) * sd[i]) * halo * vis[i] * dim + glow[i] * 0.45 + (hot ? 0.35 : 0));
      ctx.drawImage(hot || glow[i] > 0.2 ? sprites.accent : sprites.kinds[node.k], sx[i] - gs, sy[i] - gs, gs * 2, gs * 2);
      ctx.globalCompositeOperation = "source-over";
      ctx.globalAlpha = (0.45 + 0.55 * sd[i]) * vis[i] * dim;
      ctx.fillStyle = hot ? colours.accent : colours.kinds[node.k];
      ctx.beginPath();
      ctx.arc(sx[i], sy[i], r, 0, 6.2832);
      ctx.fill();
      if (i === selected) {
        ctx.globalAlpha = 1;
        ctx.strokeStyle = colours.accent;
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        ctx.arc(sx[i], sy[i], r + 4, 0, 6.2832);
        ctx.stroke();
      }
    }

    // Pulses travelling along the edges.
    ctx.globalCompositeOperation = add;
    for (const p of pulses) {
      const x = sx[p.a] + (sx[p.b] - sx[p.a]) * p.t;
      const y = sy[p.a] + (sy[p.b] - sy[p.a]) * p.t;
      const s = 6 * (0.7 + 0.5 * ((sd[p.a] + sd[p.b]) / 2));
      ctx.globalAlpha = 0.8;
      ctx.drawImage(sprites.accent, x - s, y - s, s * 2, s * 2);
    }
    ctx.globalCompositeOperation = "source-over";

    // Labels: the hovered and selected node, the search matches, and the
    // biggest hubs when the map is small enough to read.
    const labelled = new Set();
    if (hovered >= 0) labelled.add(hovered);
    if (selected >= 0) labelled.add(selected);
    focus.forEach((on, i) => on && labelled.add(i));
    if (!highlight) {
      const hubs = order.slice().sort((a, b) => graph.nodes[b].weight - graph.nodes[a].weight).slice(0, n > 400 ? 6 : 10);
      for (const i of hubs) if (sd[i] > 0.45) labelled.add(i);
    }
    ctx.font = "500 12px 'Hanken Grotesk', system-ui, sans-serif";
    ctx.textBaseline = "middle";
    // The hovered and selected labels first, then the rest wherever they do
    // not collide with one already drawn.
    const placed = [];
    const ranked = [...labelled].sort((a, b) => (b === hovered || b === selected) - (a === hovered || a === selected));
    for (const i of ranked) {
      if (vis[i] < 0.5) continue;
      const text = graph.nodes[i].label.length > 32 ? `${graph.nodes[i].label.slice(0, 31)}…` : graph.nodes[i].label;
      const x = sx[i] + radius(i) + 6;
      const w = ctx.measureText(text).width;
      const box = { x0: x - 2, x1: x + w + 2, y0: sy[i] - 8, y1: sy[i] + 8 };
      if (placed.some((b) => box.x0 < b.x1 && box.x1 > b.x0 && box.y0 < b.y1 && box.y1 > b.y0)) continue;
      placed.push(box);
      ctx.globalAlpha = 0.92 * vis[i];
      ctx.fillStyle = colours.ink;
      ctx.fillText(text, x, sy[i]);
    }
    ctx.globalAlpha = 1;
    dirty = false;
  }

  function loop(now) {
    raf = 0;
    if (dead) return;
    const dt = last ? Math.min(0.05, (now - last) / 1000) : 0.016;
    last = now;
    const moving = step(dt);
    if (moving || dirty) draw();
    if ((moving || dirty) && !document.hidden) raf = requestAnimationFrame(loop);
    else last = 0;
  }

  function schedule() {
    if (!raf && !dead) raf = requestAnimationFrame(loop);
  }

  /** The front-most visible node under canvas-local CSS pixels, or -1. */
  function pick(x, y) {
    let best = -1;
    let score = -Infinity;
    for (let i = 0; i < n; i += 1) {
      if (vis[i] < 0.6) continue;
      const r = Math.max(7, radius(i) + 3);
      const q = (sx[i] - x) ** 2 + (sy[i] - y) ** 2;
      if (q > r * r) continue;
      const s = sd[i] - q / (r * r) * 0.3;
      if (s > score) {
        score = s;
        best = i;
      }
    }
    return best;
  }

  /* -- pointer: orbit, pan, zoom, hover, pick --------------------------- */

  const pointers = new Map();
  let drag = null;
  let moved = 0;
  let pinch = 0;

  canvas.addEventListener("pointerdown", (event) => {
    canvas.setPointerCapture(event.pointerId);
    pointers.set(event.pointerId, { x: event.offsetX, y: event.offsetY });
    moved = 0;
    drag = { x: event.offsetX, y: event.offsetY, pan: event.shiftKey || event.button === 2 || event.button === 1 };
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      pinch = Math.hypot(a.x - b.x, a.y - b.y);
    }
    canvas.classList.add("is-dragging");
  });
  canvas.addEventListener("pointermove", (event) => {
    const x = event.offsetX;
    const y = event.offsetY;
    if (pointers.has(event.pointerId)) pointers.set(event.pointerId, { x, y });
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      if (pinch) zoomTo = zoom = Math.max(0.5, Math.min(6, zoom * (dist / pinch)));
      pinch = dist;
      dirty = true;
      schedule();
      return;
    }
    if (drag) {
      const dx = x - drag.x;
      const dy = y - drag.y;
      moved += Math.abs(dx) + Math.abs(dy);
      drag.x = x;
      drag.y = y;
      if (drag.pan || flat) {
        panX += dx;
        panY += dy;
      } else {
        angle += dx * 0.008;
        tilt = Math.max(-1.3, Math.min(1.3, tilt + dy * 0.006));
      }
      dirty = true;
      schedule();
      return;
    }
    const hit = pick(x, y);
    if (hit !== hovered) {
      hovered = hit;
      dirty = true;
      schedule();
      o.onHover(hit >= 0 ? graph.nodes[hit] : null, x, y);
    } else if (hit >= 0) o.onHover(graph.nodes[hit], x, y);
  });
  const release = (event) => {
    pointers.delete(event.pointerId);
    if (pointers.size < 2) pinch = 0;
    if (drag && moved < 6 && event.type === "pointerup") {
      const hit = pick(event.offsetX, event.offsetY);
      if (hit >= 0) o.onPick(graph.nodes[hit]);
    }
    drag = null;
    canvas.classList.remove("is-dragging");
  };
  canvas.addEventListener("pointerup", release);
  canvas.addEventListener("pointercancel", release);
  canvas.addEventListener("pointerleave", () => {
    if (hovered >= 0) {
      hovered = -1;
      dirty = true;
      schedule();
      o.onHover(null, 0, 0);
    }
  });
  canvas.addEventListener("contextmenu", (event) => event.preventDefault());
  canvas.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      zoomTo = zoom = Math.max(0.5, Math.min(6, zoom * Math.exp(-event.deltaY * 0.0015)));
      dirty = true;
      schedule();
    },
    { passive: false },
  );

  const ro = new ResizeObserver(resize);
  ro.observe(canvas);
  const onTheme = () => {
    readColours();
    schedule();
  };
  document.addEventListener("lb-theme", onTheme);
  const onVisible = () => {
    if (!document.hidden) schedule();
  };
  document.addEventListener("visibilitychange", onVisible);

  if (o.reduced) layout.run(400);
  readColours();
  resize();

  return {
    /** Which nodes show (`visibleMask`'s answer). */
    setMask(next) {
      mask = next;
      dirty = true;
      schedule();
    },
    /** Lights nodes as if they were just used: the glow fades, and linked nodes
     * catch some of it. Out-of-range indices are ignored. */
    pulse(indices) {
      for (const i of indices) {
        if (i >= 0 && i < n) glow[i] = 1;
      }
      dirty = true;
      schedule();
    },
    /** Where node `i` is now, in CSS pixels of the canvas (or null). */
    screenOf(i) {
      return i >= 0 && i < n ? { x: sx[i], y: sy[i] } : null;
    },
    /** Nodes to glow and label (search matches); an empty list clears. */
    setFocus(list) {
      focus = new Uint8Array(n);
      for (const node of list) focus[node.i] = 1;
      dirty = true;
      schedule();
    },
    /** Selects a node (−1 clears) and, with `centre`, flies to it. */
    select(i, centre = false) {
      selected = i;
      if (centre && i >= 0) {
        lookTo.x = pos[i * 3];
        lookTo.y = pos[i * 3 + 1];
        lookTo.z = pos[i * 3 + 2];
        zoomTo = Math.max(zoom, 1.8);
        panX = 0;
        panY = 0;
        glow[i] = 1;
      }
      dirty = true;
      schedule();
    },
    /** Back to the whole brain. */
    reset() {
      lookTo.x = lookTo.y = lookTo.z = 0;
      zoomTo = 1;
      panX = 0;
      panY = 0;
      angle = 0.42;
      tilt = -0.22;
      dirty = true;
      schedule();
    },
    zoomBy(factor) {
      zoomTo = Math.max(0.5, Math.min(6, zoomTo * factor));
      schedule();
    },
    rotateBy(da, dt = 0) {
      angle += da;
      tilt = Math.max(-1.3, Math.min(1.3, tilt + dt));
      dirty = true;
      schedule();
    },
    setFlat(on) {
      flat = Boolean(on);
      dirty = true;
      schedule();
    },
    setSpin(on) {
      speed = on && !o.reduced ? 0.045 : 0;
      schedule();
    },
    get spinning() {
      return speed > 0;
    },
    get settled() {
      return !settling;
    },
    redraw() {
      dirty = true;
      schedule();
    },
    destroy() {
      dead = true;
      ro.disconnect();
      document.removeEventListener("lb-theme", onTheme);
      document.removeEventListener("visibilitychange", onVisible);
      if (raf) cancelAnimationFrame(raf);
    },
  };
}
