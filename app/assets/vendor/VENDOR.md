# Third-party and borrowed code in `app/assets`

The app loads no third-party script: the CSP's `script-src 'self'` (plus the
one hashed theme bootstrap) holds, and nothing here is fetched from a CDN.

## The brain map renderer

The Brain map (`brain.html`) draws the asker's graph with
`assets/brain-render.js`, a port of the livingbrain.wiki website's brain
renderer rather than a vendored 3D library. It was chosen over three.js +
3d-force-graph (about 700 KB minified, WebGL-only) because the website's
renderer is a few hundred lines, needs only a canvas 2D context — so there is
no WebGL to fall back from — and is the look the brief asked for.

| | |
| :--- | :--- |
| Source | `assets/brain.js` in [Livingbrain-wiki/website](https://github.com/Livingbrain-wiki/website/blob/c1cfa14a843be8a5c716bae238a5310e70664a69/assets/brain.js) |
| Version | commit `c1cfa14a843be8a5c716bae238a5310e70664a69` |
| SHA-256 of the source file | `7a509b321ef39c65f9a2951e54f4347c8ac195d1a64ac47adf00b188d39dd708` |
| Licence | Living Brain's own code (same organisation; the website repository carries no separate licence) |
| What was kept | the perspective projection and camera constants, the radial-gradient glow sprites, additive light in the dark theme, depth-binned edge strokes, travelling pulses, the seeded PRNG, the brain's hemisphere proportions |
| What changed | an ES module instead of a `window` global; the procedural brain replaced by the asker's real nodes laid out by a 3D force layout (`assets/graph.js`, Barnes–Hut repulsion inside the brain envelope); hover, pick, search focus, filters, the time slider, flat mode, keyboard and pinch controls; reduced motion |

Re-sync the look from the website by comparing its `brain.js` against the
kept parts above; there is no build step and no copy to update.

## Mermaid (diagrams)

`assets/vendor/mermaid/mermaid-11.17.2.min.js` is the UMD build of
[mermaid](https://github.com/mermaid-js/mermaid) **11.17.2**, copied unchanged
from the npm tarball (`npm pack mermaid@11.17.2`, `dist/mermaid.min.js`). Its
licence is alongside it (`LICENSE`, MIT). It is loaded only when a page has a
diagram to draw (`assets/diagram-view.js`), with
`integrity="sha384-…"` set to the file's hash (the constant `MERMAID_SRI` in
`assets/diagrams.js`). To update it: pack the new version, copy the file under
its new name, update `MERMAID_URL` and `MERMAID_SRI`, and delete the old file.
The build is not rewritten, so `tools/stamp-assets.mjs` skips
`assets/vendor/` when it looks for references.
