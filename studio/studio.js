"use strict";
// Wonder Vision Studio: a scene editor (docs/scenes.md). The playlist document is edited here as a whole and saved
// as it changes; the canvas draws scenes exactly as the player does, so what you see is what the cooler shows.

const $ = (id) => document.getElementById(id);
const api = (path, opts = {}) => fetch(path, { ...opts, headers: { "X-Studio": "1", ...opts.headers } });
async function postJson(path, body, opts = {}) {
  const res = await api(path, { method: "POST", headers: { "Content-Type": "application/json" },
                                body: JSON.stringify(body), ...opts });
  const out = await res.json();
  if (!res.ok) throw new Error(out.error || res.statusText);
  return out;
}
function make(tag, cls = "", text) {
  const el = document.createElement(tag);
  el.className = cls;
  if (text !== undefined) el.textContent = text;
  return el;
}
const num = (v, fallback) => (typeof v === "number" && Number.isFinite(v) ? v : fallback);
const hex = (v, fallback) => (/^#[0-9a-f]{6}$/i.test(v) ? v.toLowerCase()
  : /^#[0-9a-f]{3}$/i.test(v) ? "#" + [...v.slice(1)].map((c) => c + c).join("").toLowerCase() : fallback);
const clamp01 = (v) => Math.min(1, Math.max(0, num(v, 1)));
const uid = (prefix, taken) => {
  let id;
  do id = prefix + Math.random().toString(36).slice(2, 7); while (taken.has(id));
  return id;
};
function message(text, cls = "") { $("msg").textContent = text; $("msg").className = cls; }

let W = 1600, H = 720, SAFE = { left: 0, right: 0, top: 0, bottom: 0 };  // replaced from the server
let CAMERA = null;  // the camera punch-hole, {x, y, r}: no pixels there
let DEFAULT_FONT = "C:\\Windows\\Fonts\\segoeui.ttf";
const M = 140;  // off-screen margin shown around the screen, in screen px
const FRONT = () => ({ x: SAFE.left, y: SAFE.top, w: W - SAFE.left - SAFE.right, h: H - SAFE.top - SAFE.bottom });
const WHOLE = () => ({ x: 0, y: 0, w: W, h: H });

// --- The document and the selection ------------------------------------------------------
//
// `doc` is playlist.json as the page holds it. Everything in it is reached by id, never by held references: undo
// replaces the whole thing. `sel.scene` is a scene id or "global"; `sel.ids` the selected objects (last = primary);
// `sel.layer` the selected layer when no object is.

let doc = { version: 2, seconds: 30, global: { layers: [] }, scenes: [] };
const sel = { scene: null, layer: null, ids: [] };
let lastScene = null;  // the scene id shown dimmed behind the global layers
const view = { zoom: "fit", k: 0.3, guides: true, ghost: true, paused: false, picking: null, snapped: null, rubber: null };
const tool = { name: "select", shape: "rect", space: false };

const stage = $("stage"), screenCv = $("screen"), ghostCv = $("ghost"), overlayCv = $("overlay");
const sctx = screenCv.getContext("2d");

function sceneById(id) { return doc.scenes.find((s) => s.id === id) || null; }
/** What's being edited: a scene, or the global pseudo-scene. */
function holder() { return sel.scene === "global" ? doc.global : sceneById(sel.scene); }
function isGlobal() { return sel.scene === "global"; }
function layersOf(h = holder()) { return (h && Array.isArray(h.layers)) ? h.layers : []; }
function layerById(id, h = holder()) { return layersOf(h).find((l) => l.id === id) || null; }
/** {layer, obj, index} for an object id in the current holder, or null. */
function find(id, h = holder()) {
  for (const layer of layersOf(h)) {
    const index = (layer.objects || []).findIndex((o) => o.id === id);
    if (index >= 0) return { layer, obj: layer.objects[index], index };
  }
  return null;
}
function primary() { return sel.ids.length ? find(sel.ids[sel.ids.length - 1])?.obj || null : null; }
function selectedObjects() { return sel.ids.map((id) => find(id)?.obj).filter(Boolean); }
function selectedLayer() {
  const p = sel.ids.length ? find(sel.ids[sel.ids.length - 1]) : null;
  return p ? p.layer : layerById(sel.layer);
}
function takenIds(h = holder()) {
  const s = new Set();
  for (const l of layersOf(h)) { s.add(l.id); for (const o of l.objects || []) s.add(o.id); }
  return s;
}
function isLayerUsable(layer) { return layer.visible !== false && !layer.locked; }
function isSelectable(o, layer) { return o.visible !== false && !o.locked && isLayerUsable(layer); }
const isMedia = (o) => o.type === "image" || o.type === "video";

// --- Media: kept files, their frames and timing ------------------------------------------------

const media = new Map();  // src -> {src, info, frames, video, ready, error, turned, scaled, total, starts}
let mediaVersion = 0;     // bumps when a file arrives or fails, so what depends on it is drawn again
const t0 = performance.now();
const clock = () => (view.paused ? view.pausedAt : performance.now() - t0);

function getMedia(src) {
  if (typeof src !== "string" || !src) return null;
  let m = media.get(src);
  if (m) return m;
  m = { src, info: null, frames: [], video: false, ready: false, error: null, turned: new Map(), scaled: new Map(), total: 0, starts: [] };
  media.set(src, m);
  loadMedia(m).catch((err) => { m.error = err.message; mediaVersion++; redraw(); });
  return m;
}

async function loadMedia(m) {
  const res = await api(`/api/media/${encodeURIComponent(m.src.split("/").pop())}`);
  const info = await res.json();
  if (!res.ok) throw new Error(info.error || res.statusText);
  useMediaInfo(m, info);
  await loadFrames(m);
}

function useMediaInfo(m, info) {
  m.info = info;
  m.video = !!info.video;
  m.starts = [];
  let t = 0;
  for (const d of info.durations) { m.starts.push(t); t += Math.max(1, d); }
  m.total = t;
}

async function loadFrames(m) {
  const info = m.info;
  if (m.video) {
    const video = make("video");
    Object.assign(video, { muted: true, loop: true, playsInline: true, preload: "auto" });
    await new Promise((ok, fail) => {
      video.onloadeddata = ok;
      video.onerror = () => fail(new Error("the browser couldn't play the preview"));
      video.src = info.video.url;
    });
    m.frames = [video];
    if (!view.paused) video.play().catch(() => {});
  } else {
    m.frames = await Promise.all(info.durations.map(async (_, i) => {
      const blob = await (await api(`/api/frame/${info.id}/${i}.png`)).blob();
      return createImageBitmap(blob, { colorSpaceConversion: "none" });
    }));
  }
  m.ready = true;
  mediaVersion++;
  redraw();
  thumbsSoon();
}

function frameIndex(m, t) {
  if (m.video || m.frames.length < 2) return 0;
  const at = ((t % m.total) + m.total) % m.total;
  let lo = 0, hi = m.starts.length - 1;
  while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (m.starts[mid] <= at) lo = mid; else hi = mid - 1; }
  return lo;
}
const isAnimated = (o) => isMedia(o) && !!(media.get(o.src)?.info?.animated);

/** The source's size after turning. */
function turnedSize(m, o) {
  const [w, h] = [m.info.width, m.info.height];
  return o.rotate % 180 ? [h, w] : [w, h];
}

/** A frame turned and flipped, still at file resolution: whole pixels land on whole pixels, nothing is resampled. */
function turnedFrame(m, o, i) {
  const key = `${i}|${o.rotate | 0}|${!!o.flip}`;
  let c = m.turned.get(key);
  if (c && !m.video) return c;  // a video's one "frame" changes as it plays
  const [w, h] = turnedSize(m, o), sw = m.info.width, sh = m.info.height;
  if (!c) { if (m.turned.size > 300) m.turned.clear(); c = document.createElement("canvas"); m.turned.set(key, c); }
  c.width = w; c.height = h;
  const g = c.getContext("2d");
  g.imageSmoothingEnabled = false;
  if (o.flip) g.setTransform(-1, 0, 0, 1, w, 0);
  g.transform(...{ 0: [1, 0, 0, 1, 0, 0], 90: [0, 1, -1, 0, sh, 0], 180: [-1, 0, 0, -1, sw, sh], 270: [0, -1, 1, 0, 0, sw] }[o.rotate % 360 || 0]);
  g.drawImage(m.frames[i], 0, 0);
  return c;
}

/** The frame scaled to the object's box: nearest neighbour unless smooth (the player's rule, pixel for pixel). */
function scaledFrame(m, o, i) {
  const dw = Math.max(1, Math.round(o.w)), dh = Math.max(1, Math.round(o.h));
  const key = `${i}|${dw}|${dh}|${!!o.smooth}|${o.rotate | 0}|${!!o.flip}`;
  let c = m.scaled.get(key);
  if (c && !m.video) return c;
  if (!c) { if (m.scaled.size > 300) m.scaled.clear(); c = document.createElement("canvas"); m.scaled.set(key, c); }
  if (c.width !== dw || c.height !== dh) { c.width = dw; c.height = dh; }
  const g = c.getContext("2d");
  g.clearRect(0, 0, dw, dh);
  g.imageSmoothingEnabled = !!o.smooth;
  g.imageSmoothingQuality = "high";
  g.drawImage(turnedFrame(m, o, i), 0, 0, dw, dh);
  return c;
}

// --- Drawing a scene (docs/scenes.md, Drawing a frame) -----------------------------------------

const pool = [];  // layer buffers, W x H, for layers drawn at less than full opacity
function layerBuffer(depth) {
  const c = pool[depth] ||= document.createElement("canvas");
  if (c.width !== W || c.height !== H) { c.width = W; c.height = H; }
  return c;
}

/** Draws a scene (null: only the global layers) at time t. With `dim`, the scene is darkened under the global
 *  layers, for editing them. `thumb` draws with the raw text, the way the player would without live values. */
function renderScene(ctx, scene, t, { withGlobal = true, dim = false } = {}) {
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = 1;
  ctx.fillStyle = scene ? hex(scene.background, "#000000") : "#000000";
  ctx.fillRect(0, 0, W, H);
  if (scene) for (const layer of layersOf(scene)) drawLayer(ctx, layer, t);
  if (dim) { ctx.fillStyle = "rgba(0,0,0,.55)"; ctx.fillRect(0, 0, W, H); }
  if (withGlobal) for (const layer of layersOf(doc.global)) drawLayer(ctx, layer, t);
}

/** A layer: its objects together onto a buffer, blended at the layer's opacity (so they don't show through each
 *  other). At full opacity that's the same as drawing them straight on, which is cheaper. */
function drawLayer(ctx, layer, t) {
  if (layer.visible === false) return;
  const op = clamp01(layer.opacity);
  if (op <= 0) return;
  const objects = (layer.objects || []).filter((o) => o && o.visible !== false);
  if (op >= 1) { for (const o of objects) drawObject(ctx, o, t); return; }
  const buf = layerBuffer(0), g = buf.getContext("2d");
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.globalAlpha = 1;
  g.clearRect(0, 0, W, H);
  for (const o of objects) drawObject(g, o, t);
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = op;
  ctx.drawImage(buf, 0, 0);
  ctx.globalAlpha = 1;
}

function drawObject(ctx, o, t) {
  const op = clamp01(o.opacity);
  if (op <= 0) return;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = op;
  if (isMedia(o)) drawMedia(ctx, o, t);
  else if (o.type === "text") drawWidget(ctx, o);
  else if (o.type === "shape") drawShape(ctx, o);
  ctx.globalAlpha = 1;
}

function drawMedia(ctx, o, t) {
  const m = getMedia(o.src);
  if (!m?.ready) {  // not here yet, or not at all: its box, so it can still be moved
    ctx.fillStyle = m?.error ? "rgba(255,120,90,.25)" : "rgba(140,150,160,.25)";
    ctx.fillRect(o.x, o.y, o.w, o.h);
    ctx.fillStyle = "rgba(255,255,255,.7)";
    ctx.font = "20px system-ui, sans-serif";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(m?.error ? `can't open ${o.src}` : "loading…", o.x + o.w / 2, o.y + o.h / 2);
    return;
  }
  const img = scaledFrame(m, o, frameIndex(m, t));
  paint(ctx, img, Math.round(o.x), Math.round(o.y), o.fill || "none");
}

/** Draws a scaled frame at x, y, repeated outward over the whole canvas for the extend, mirror and tile fills. */
function paint(ctx, img, x, y, fill) {
  const dw = img.width, dh = img.height;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.imageSmoothingEnabled = false;
  if (fill === "tile" || fill === "mirror") {
    const mirror = fill === "mirror";
    if ((W / dw + 2) * (H / dh + 2) > 300) {
      // lots of small tiles: one pattern fill, mirroring baked into a 2x2 block
      let tile = img;
      if (mirror) {
        const m2 = document.createElement("canvas");
        m2.width = 2 * dw; m2.height = 2 * dh;
        const g = m2.getContext("2d");
        for (const [fx, fy] of [[0, 0], [1, 0], [0, 1], [1, 1]]) {
          g.setTransform(fx ? -1 : 1, 0, 0, fy ? -1 : 1, fx * 2 * dw, fy * 2 * dh);
          g.drawImage(img, 0, 0);
        }
        tile = m2;
      }
      const pattern = ctx.createPattern(tile, "repeat");
      pattern.setTransform(new DOMMatrix([1, 0, 0, 1, x, y]));
      ctx.fillStyle = pattern;
      ctx.fillRect(0, 0, W, H);
      return;
    }
    for (let j = Math.floor(-y / dh); y + j * dh < H; j++) {
      for (let i = Math.floor(-x / dw); x + i * dw < W; i++) {
        const fx = mirror ? i & 1 : 0, fy = mirror ? j & 1 : 0;
        ctx.setTransform(fx ? -1 : 1, 0, 0, fy ? -1 : 1, x + (i + fx) * dw, y + (j + fy) * dh);
        ctx.drawImage(img, 0, 0);
      }
    }
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    return;
  }
  ctx.drawImage(img, x, y);
  if (fill !== "extend") return;
  const r = x + dw, b = y + dh;
  const strip = (sx, sy, sw, sh, tx, ty, tw, th) => {
    if (tw > 0 && th > 0) ctx.drawImage(img, sx, sy, sw, sh, tx, ty, tw, th);
  };
  strip(0, 0, 1, dh, 0, y, x, dh);              // left edge column, stretched
  strip(dw - 1, 0, 1, dh, r, y, W - r, dh);     // right
  strip(0, 0, dw, 1, x, 0, dw, y);              // top row
  strip(0, dh - 1, dw, 1, x, b, dw, H - b);     // bottom
  strip(0, 0, 1, 1, 0, 0, x, y);                // corner pixels fill the corners
  strip(dw - 1, 0, 1, 1, r, 0, W - r, y);
  strip(0, dh - 1, 1, 1, 0, b, x, H - b);
  strip(dw - 1, dh - 1, 1, 1, r, b, W - r, H - b);
}

// --- Shapes --------------------------------------------------------------------------------------
//
// The same rules as the player's shape.rs: a point is in the fill when its signed distance to the outline is <= 0
// (a plain rect is half-open, like fillRect), and in the stroke when within half the width of the outline (a line's
// stroke has round caps). Crisp shapes paint exactly the pixels whose centres pass; smooth ones use canvas paths,
// close to the player's supersampling. Fill and stroke are drawn as one group at the object's opacity.

const shapeCv = document.createElement("canvas"), shapeCtx = shapeCv.getContext("2d", { willReadFrequently: true });

/** The gradient's line for a box: through its centre at `angle`, as long as the box's projection, as CSS does it. */
function gradientLine(fill, box) {
  const a = num(fill.angle, 0) * Math.PI / 180, dx = Math.cos(a), dy = Math.sin(a);
  const L = Math.max(Math.abs(box.w * dx) + Math.abs(box.h * dy), 1e-9), cx = box.x + box.w / 2, cy = box.y + box.h / 2;
  return { x0: cx - dx * L / 2, y0: cy - dy * L / 2, x1: cx + dx * L / 2, y1: cy + dy * L / 2, L, dx, dy, cx, cy };
}
function gradientStops(fill) {
  const stops = (Array.isArray(fill.stops) ? fill.stops : []).map((s) => ({ at: Math.min(1, Math.max(0, num(s?.at, 0))), color: hex(s?.color, "#ffffff") }))
    .sort((a, b) => a.at - b.at);
  return stops.length ? stops : [{ at: 0, color: "#ffffff" }];
}
const rgb = (c) => [1, 3, 5].map((i) => parseInt(c.slice(i, i + 2), 16));
/** The colour at a fraction along the gradient, as [r, g, b]. */
function gradientAt(stops, t) {
  if (t <= stops[0].at) return rgb(stops[0].color);
  if (t >= stops[stops.length - 1].at) return rgb(stops[stops.length - 1].color);
  const i = stops.findIndex((s) => s.at > t), a = stops[i - 1], b = stops[i], k = b.at > a.at ? (t - a.at) / (b.at - a.at) : 1;
  const ca = rgb(a.color), cb = rgb(b.color);
  return ca.map((v, j) => Math.round(v + (cb[j] - v) * k));
}

/** The shape's box as drawn (a line's bounding box). */
function shapeBox(o) {
  const x0 = Math.min(o.x, o.x + o.w), x1 = Math.max(o.x, o.x + o.w), y0 = Math.min(o.y, o.y + o.h), y1 = Math.max(o.y, o.y + o.h);
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}

/** The signed distance from a point to the fill's outline (negative inside); for a line, to the segment. */
function shapeDistance(o, box, px, py) {
  if (o.shape === "line") {
    const ax = o.x, ay = o.y, vx = o.w, vy = o.h, len2 = vx * vx + vy * vy;
    const t = len2 ? Math.min(1, Math.max(0, ((px - ax) * vx + (py - ay) * vy) / len2)) : 0;
    return Math.hypot(px - ax - t * vx, py - ay - t * vy);
  }
  if (o.shape === "ellipse") {
    const rx = box.w / 2, ry = box.h / 2;
    if (rx <= 0 || ry <= 0) return Infinity;
    const nx = (px - box.x - rx) / rx, ny = (py - box.y - ry) / ry, q = Math.hypot(nx, ny);
    const k = q === 0 ? Math.min(rx, ry) : Math.hypot(nx * rx, ny * ry) / q;  // scaled distance: exact on the axes
    return (q - 1) * k;
  }
  const hw = box.w / 2, hh = box.h / 2, r = Math.min(Math.max(num(o.radius, 0), 0), hw, hh);
  const dx = Math.abs(px - (box.x + hw)) - hw + r, dy = Math.abs(py - (box.y + hh)) - hh + r;
  return Math.hypot(Math.max(dx, 0), Math.max(dy, 0)) + Math.min(Math.max(dx, dy), 0) - r;
}

function canvasFill(ctx, fill, box) {
  if (!fill || fill.type === "none") return null;
  if (fill.type === "solid") return hex(fill.color, "#ffffff");
  if (fill.type === "linear") {
    const l = gradientLine(fill, box), g = ctx.createLinearGradient(l.x0, l.y0, l.x1, l.y1);
    for (const s of gradientStops(fill)) g.addColorStop(s.at, s.color);
    return g;
  }
  return null;
}

function drawShape(ctx, o) {
  const stroke = o.stroke && num(o.stroke.width, 0) > 0 ? { color: hex(o.stroke.color, "#ffffff"), width: num(o.stroke.width, 1) } : null;
  const fill = o.shape === "line" || !o.fill || o.fill.type === "none" ? null : o.fill;
  if (!fill && !stroke) return;
  const box = shapeBox(o), s = stroke ? stroke.width / 2 : 0;
  const x0 = Math.max(0, Math.floor(box.x - s - 1)), y0 = Math.max(0, Math.floor(box.y - s - 1));
  const x1 = Math.min(W, Math.ceil(box.x + box.w + s + 1)), y1 = Math.min(H, Math.ceil(box.y + box.h + s + 1));
  if (x1 <= x0 || y1 <= y0) return;
  const bw = x1 - x0, bh = y1 - y0;
  if (shapeCv.width !== bw || shapeCv.height !== bh) { shapeCv.width = bw; shapeCv.height = bh; }
  const g = shapeCtx;
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.clearRect(0, 0, bw, bh);
  if (o.crisp) {
    const img = g.createImageData(bw, bh), d = img.data;
    const solid = fill?.type === "solid" ? rgb(hex(fill.color, "#ffffff")) : null;
    const stops = fill?.type === "linear" ? gradientStops(fill) : null, gl = stops && gradientLine(fill, box);
    const sc = stroke && rgb(stroke.color), plain = o.shape === "rect" && num(o.radius, 0) <= 0;
    const inFill = (px, py) => fill && (plain ? px >= box.x && px < box.x + box.w && py >= box.y && py < box.y + box.h
                                             : shapeDistance(o, box, px, py) <= 0);
    const inStroke = (px, py) => sc && (o.shape === "line" ? shapeDistance(o, box, px, py) <= s : Math.abs(shapeDistance(o, box, px, py)) <= s);
    for (let py = 0; py < bh; py++) {
      for (let px = 0; px < bw; px++) {
        const cx = x0 + px + 0.5, cy = y0 + py + 0.5;
        let c = null;
        if (inStroke(cx, cy)) c = sc;
        else if (inFill(cx, cy)) c = solid || gradientAt(stops, ((cx - gl.cx) * gl.dx + (cy - gl.cy) * gl.dy) / gl.L + 0.5);
        if (c) { const i = 4 * (py * bw + px); d[i] = c[0]; d[i + 1] = c[1]; d[i + 2] = c[2]; d[i + 3] = 255; }
      }
    }
    g.putImageData(img, 0, 0);
  } else {
    g.setTransform(1, 0, 0, 1, -x0, -y0);
    g.beginPath();
    if (o.shape === "line") { g.moveTo(o.x, o.y); g.lineTo(o.x + o.w, o.y + o.h); }
    else if (o.shape === "ellipse") g.ellipse(box.x + box.w / 2, box.y + box.h / 2, Math.max(0, box.w / 2), Math.max(0, box.h / 2), 0, 0, 2 * Math.PI);
    else {
      const r = Math.min(Math.max(0, num(o.radius, 0)), box.w / 2, box.h / 2);
      if (r > 0) g.roundRect(box.x, box.y, box.w, box.h, r); else g.rect(box.x, box.y, box.w, box.h);
    }
    const f = canvasFill(g, fill, box);
    if (f) { g.fillStyle = f; g.fill(); }
    if (stroke) { g.strokeStyle = stroke.color; g.lineWidth = stroke.width; g.lineJoin = "round"; g.lineCap = "round"; g.stroke(); }
    g.setTransform(1, 0, 0, 1, 0, 0);
  }
  ctx.drawImage(shapeCv, x0, y0);  // as one group, at the object's opacity
}

// --- Text (docs/widgets.md), laid out as the player does ----------------------------------------

let fontList = [];       // [{path, family, style}]: the fonts the picker offers
const fonts = new Map(); // font path (lower case) -> {family, state}: a FontFace per file, loaded when first used
let fontsArrived = 0;    // counts loads finished, so the text is drawn again
const textCtx = document.createElement("canvas").getContext("2d");  // for measuring
// one widget at a time: outline (or everything), and the fill on its own for crisp text
const glyphs = [0, 1].map(() => document.createElement("canvas").getContext("2d", { willReadFrequently: true }));

/** A font file as a CSS family, loaded from the studio the first time it's asked for. */
function fontFace(path) {
  const key = path.toLowerCase();  // Windows paths
  let f = fonts.get(key);
  if (!f) {
    f = { family: `wv-font-${fonts.size}`, state: "loading" };
    fonts.set(key, f);
    const face = new FontFace(f.family, `url("/api/font?path=${encodeURIComponent(path)}")`);
    face.load().then(() => { document.fonts.add(face); f.state = "ready"; }, () => { f.state = "failed"; })
      .finally(() => { fontsArrived++; redraw(); thumbsSoon(); });
  }
  return f;
}

/** A widget's font family; Segoe UI when its own can't be read, as in the player. Null while it loads, or when
 *  neither font will do (then the widget isn't drawn). */
function widgetFamily(w) {
  for (const path of [typeof w.font === "string" && w.font ? w.font : DEFAULT_FONT, DEFAULT_FONT]) {
    const f = fontFace(path);
    if (f.state !== "failed") return f.state === "ready" ? f.family : null;
  }
  return null;
}

/** A text widget laid out (docs/widgets.md, Geometry): each line's text, start, width and baseline, the block's
 *  box, and the font's ascent and descent. Null for other kinds of object, and until the font has loaded. */
function textLayout(w) {
  const family = w.type === "text" && widgetFamily(w);
  if (!family) return null;
  const g = textCtx, size = Math.max(1, num(w.size, 48)), step = size * num(w.line_height, 1.2);
  const font = `${size}px "${family}"`;
  g.font = font;
  g.fontKerning = "normal";
  const m = g.measureText("x"), ascent = m.fontBoundingBoxAscent, descent = m.fontBoundingBoxDescent;
  const texts = shownText(w).split("\n"), height = ascent + descent + (texts.length - 1) * step;
  const x = num(w.x, 0), y = num(w.y, 0);
  const top = w.valign === "middle" ? y - height / 2 : w.valign === "bottom" ? y - height : y;
  const lines = texts.map((t, i) => {
    const width = g.measureText(t).width;  // the advances, kerned
    return { text: t, width, baseline: top + ascent + i * step,
             x: w.align === "center" ? x - width / 2 : w.align === "right" ? x - width : x };
  });
  const left = Math.min(...lines.map((l) => l.x)), right = Math.max(...lines.map((l) => l.x + l.width));
  return { lines, ascent, descent, font, box: { x: left, y: top, w: right - left, h: height } };
}

/** Crisp text: each pixel all or nothing at 50% coverage, in the one colour drawn there. */
function threshold(g, x, y, w, h, colour) {
  const img = g.getImageData(x, y, w, h), d = img.data;
  const [r, gr, b] = rgb(colour);
  for (let i = 0; i < d.length; i += 4) {
    const on = d[i + 3] >= 128;
    d[i] = r; d[i + 1] = gr; d[i + 2] = b; d[i + 3] = on ? 255 : 0;
  }
  g.putImageData(img, x, y);
}

/** Draws one text object onto `out`: the outline (the glyphs grown by its width) under the fill. The object's
 *  opacity is `out`'s globalAlpha, set by the caller. */
function drawWidget(out, w) {
  const lay = textLayout(w);
  if (!lay) return;
  const o = Math.max(0, num(w.outline?.width, 0)), colour = hex(w.color, "#ffffff");
  const outline = hex(w.outline?.color, "#000000"), pad = Math.ceil(o + lay.ascent + lay.descent);  // glyphs can overhang
  const x0 = Math.max(0, Math.floor(lay.box.x - pad)), y0 = Math.max(0, Math.floor(lay.box.y - pad));
  const x1 = Math.min(W, Math.ceil(lay.box.x + lay.box.w + pad)), y1 = Math.min(H, Math.ceil(lay.box.y + lay.box.h + pad));
  if (x1 <= x0 || y1 <= y0) return;  // all off the screen
  const [a, b] = glyphs, bw = x1 - x0, bh = y1 - y0;
  for (const g of glyphs) {
    if (g.canvas.width !== W || g.canvas.height !== H) { g.canvas.width = W; g.canvas.height = H; }
    g.clearRect(x0, y0, bw, bh);
    Object.assign(g, { font: lay.font, fontKerning: "normal", textAlign: "left", textBaseline: "alphabetic",
                       lineWidth: 2 * o, lineJoin: "round" });
  }
  const fill = (g, c) => { g.fillStyle = c; for (const l of lay.lines) g.fillText(l.text, l.x, l.baseline); };
  const stroke = (g, c) => { g.strokeStyle = c; for (const l of lay.lines) g.strokeText(l.text, l.x, l.baseline); };
  if (w.crisp) {
    if (o > 0) {  // the whole grown shape, then the fill over it, each thresholded
      stroke(a, outline);
      fill(a, outline);
      threshold(a, x0, y0, bw, bh, outline);
    }
    fill(b, colour);
    threshold(b, x0, y0, bw, bh, colour);
    a.drawImage(b.canvas, x0, y0, bw, bh, x0, y0, bw, bh);
  } else {
    if (o > 0) stroke(a, outline);
    fill(a, colour);
  }
  out.drawImage(a.canvas, x0, y0, bw, bh, x0, y0, bw, bh);
}

// --- Boxes, hit testing -------------------------------------------------------------------------------

/** An object's box on screen: the media or shape box (a line's bounding box), or the text block. */
function objBox(o) {
  if (o.type === "text") return textLayout(o)?.box || { x: num(o.x, 0), y: num(o.y, 0), w: 0, h: 0 };
  if (o.type === "shape") return shapeBox(o);
  return { x: o.x, y: o.y, w: o.w, h: o.h };
}

/** The handles of the primary object: four corners, or a line's two ends. [[x, y, kind], ...] */
function handles(o) {
  if (!o || o.type === "text") return [];
  if (o.type === "shape" && o.shape === "line") return [[o.x, o.y, "a"], [o.x + o.w, o.y + o.h, "b"]];
  return [[o.x, o.y, 0], [o.x + o.w, o.y, 1], [o.x, o.y + o.h, 2], [o.x + o.w, o.y + o.h, 3]];
}

function screenPoint(e, exact = false) {
  const r = screenCv.getBoundingClientRect(), x = (e.clientX - r.left) / view.k, y = (e.clientY - r.top) / view.k;
  return exact ? [x, y] : [Math.floor(x), Math.floor(y)];
}

/** Whether the point is on the object: its box for media and shapes (a line: near the segment), a text's lines. */
function hitObject(o, px, py, slop) {
  const inside = (x, y, w, h, grow) => px >= x - grow && px <= x + w + grow && py >= y - grow && py <= y + h + grow;
  if (o.type === "text") {
    const lay = textLayout(o);
    if (!lay) return false;
    if (sel.ids.includes(o.id) && inside(lay.box.x, lay.box.y, lay.box.w, lay.box.h, slop)) return true;
    const grow = Math.max(0, num(o.outline?.width, 0)) + slop;
    return lay.lines.some((l) => l.width > 0 && inside(l.x, l.baseline - lay.ascent, l.width, lay.ascent + lay.descent, grow));
  }
  if (o.type === "shape" && o.shape === "line") {
    const ax = o.x, ay = o.y, bx = o.x + o.w, by = o.y + o.h, len2 = (bx - ax) ** 2 + (by - ay) ** 2;
    const t = len2 ? Math.min(1, Math.max(0, ((px - ax) * (bx - ax) + (py - ay) * (by - ay)) / len2)) : 0;
    const reach = Math.max(num(o.stroke?.width, 0) / 2, 0) + slop + 2;
    return (px - (ax + t * (bx - ax))) ** 2 + (py - (ay + t * (by - ay))) ** 2 <= reach * reach;
  }
  const b = objBox(o);
  return inside(b.x, b.y, b.w, b.h, slop);
}

/** The topmost selectable object under the pointer in the current holder, with its layer. */
function objectAt(e) {
  const [px, py] = screenPoint(e, true), slop = 3 / view.k, layers = layersOf();
  for (let li = layers.length - 1; li >= 0; li--) {
    const layer = layers[li];
    if (!isLayerUsable(layer)) continue;
    const list = layer.objects || [];
    for (let i = list.length - 1; i >= 0; i--) {
      const o = list[i];
      if (o.visible !== false && !o.locked && hitObject(o, px, py, slop)) return { obj: o, layer };
    }
  }
  return null;
}

/** Which handle of the primary object the pointer is on, or null. */
function handleAt(e) {
  const o = primary();
  if (!o || sel.ids.length !== 1 || o.locked) return null;
  const [px, py] = screenPoint(e, true), reach = 9 / view.k;
  return handles(o).find(([hx, hy]) => Math.abs(px - hx) <= reach && Math.abs(py - hy) <= reach) || null;
}

// --- The stage: zoom, the canvases, the overlay -------------------------------------------------------

let drawQueued = false;
/** Draws on the next frame: many changes in one go are drawn once. */
function redraw() {
  if (drawQueued) return;
  drawQueued = true;
  requestAnimationFrame(() => { drawQueued = false; draw(); });
}

function draw() {
  const t = clock(), scene = isGlobal() ? sceneById(lastScene) : holder();
  renderScene(sctx, scene, t, { dim: isGlobal() });
  drawGhost(t);
  drawOverlay();
}

/** The selected object faintly where it hangs off the screen (the margin around the canvas). */
function drawGhost(t) {
  const g = ghostCv.getContext("2d"), q = ghostCv.width / (W + 2 * M), o = primary();
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.clearRect(0, 0, ghostCv.width, ghostCv.height);
  if (!view.ghost || !o || o.type === "text") return;
  g.setTransform(q, 0, 0, q, M * q, M * q);
  g.globalAlpha = 0.3;
  if (isMedia(o)) {
    const m = getMedia(o.src);
    if (!m?.ready) return;
    g.imageSmoothingEnabled = q < 1 || !!o.smooth;
    g.drawImage(scaledFrame(m, o, frameIndex(m, t)), Math.round(o.x), Math.round(o.y));
  } else if (o.type === "shape") {
    drawShape(g, { ...o, crisp: false });
  }
  g.globalAlpha = 1;
}

function drawOverlay() {
  const g = overlayCv.getContext("2d"), q = overlayCv.width / (W + 2 * M);
  const css = 1 / view.k;  // one CSS pixel, in screen pixels
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.clearRect(0, 0, overlayCv.width, overlayCv.height);
  g.setTransform(q, 0, 0, q, M * q, M * q);
  g.lineWidth = css;
  g.strokeStyle = "rgba(255,255,255,.35)";
  g.strokeRect(-css / 2, -css / 2, W + css, H + css);
  if (view.guides) {
    g.fillStyle = "rgba(255,176,32,.13)";
    for (const [x, y, w, h] of [[0, 0, SAFE.left, H], [W - SAFE.right, 0, SAFE.right, H],
                                [0, 0, W, SAFE.top], [0, H - SAFE.bottom, W, SAFE.bottom]]) {
      if (w > 0 && h > 0) g.fillRect(x, y, w, h);
    }
    g.strokeStyle = "rgba(255,176,32,.85)";
    g.setLineDash([6 * css, 5 * css]);
    g.beginPath();
    for (const x of [SAFE.left, W - SAFE.right]) {
      if (x > 0 && x < W) { g.moveTo(x, -22 * css); g.lineTo(x, H + 22 * css); }  // none where there's no curve
    }
    g.stroke();
    g.setLineDash([]);
    g.font = `${12 * css}px system-ui, "Segoe UI", sans-serif`;
    g.fillStyle = "rgba(255,176,32,.95)";
    g.textAlign = "center";
    g.textBaseline = "bottom";
    const f = FRONT();
    g.fillText(`flat front, ${f.w} px`, f.x + f.w / 2, -8 * css);
    if (SAFE.right) g.fillText("wraps around the side", W - SAFE.right / 2, -8 * css);
    if (SAFE.left) { g.textAlign = "right"; g.fillText("curls away", SAFE.left, -8 * css); }
    if (CAMERA) {  // the punch-hole: drawn as it looks on the cooler, a black dot, outlined so it shows on black
      g.beginPath();
      g.arc(CAMERA.x, CAMERA.y, CAMERA.r, 0, 2 * Math.PI);
      g.fillStyle = "#000";
      g.fill();
      g.strokeStyle = g.fillStyle = "rgba(255,176,32,.95)";
      g.stroke();
      g.textAlign = "left";
      g.textBaseline = "middle";
      g.fillText("camera", -8 * css - g.measureText("camera").width, CAMERA.y);
    }
  }
  const p = primary();
  for (const o of selectedObjects()) {  // each selected object's box; the primary's with handles
    const b = objBox(o), isP = o === p;
    if (o.type === "text") {
      g.strokeStyle = isP ? "rgba(120,255,170,.95)" : "rgba(120,255,170,.6)";
      g.setLineDash([4 * css, 3 * css]);
      g.strokeRect(b.x, b.y, Math.max(b.w, css), b.h);
      g.setLineDash([]);
      const ax = num(o.x, 0), ay = num(o.y, 0), arm = 5 * css;
      g.beginPath();
      g.moveTo(ax - arm, ay); g.lineTo(ax + arm, ay);
      g.moveTo(ax, ay - arm); g.lineTo(ax, ay + arm);
      g.stroke();
    } else {
      g.strokeStyle = isP ? "rgba(124,196,255,.9)" : "rgba(124,196,255,.55)";
      g.setLineDash([4 * css, 4 * css]);
      if (o.type === "shape" && o.shape === "line") { g.beginPath(); g.moveTo(o.x, o.y); g.lineTo(o.x + o.w, o.y + o.h); g.stroke(); }
      else g.strokeRect(b.x, b.y, b.w, b.h);
      g.setLineDash([]);
    }
  }
  if (view.rubber) {  // a shape being drawn
    const r = view.rubber;
    g.strokeStyle = "rgba(124,196,255,.9)";
    g.setLineDash([4 * css, 4 * css]);
    if (r.line) { g.beginPath(); g.moveTo(r.x, r.y); g.lineTo(r.x + r.w, r.y + r.h); g.stroke(); }
    else g.strokeRect(r.x, r.y, r.w, r.h);
    g.setLineDash([]);
  }
  if (view.snapped) {  // while dragging
    g.strokeStyle = "#ff4fd8";
    g.lineWidth = 2 * css;
    g.beginPath();
    if (view.snapped.x !== null) { g.moveTo(view.snapped.x, -M); g.lineTo(view.snapped.x, H + M); }
    if (view.snapped.y !== null) { g.moveTo(-M, view.snapped.y); g.lineTo(W + M, view.snapped.y); }
    g.stroke();
    g.lineWidth = css;
  }
  if (p && sel.ids.length === 1 && !p.locked) {
    const half = 5 * css;  // handles for resizing
    g.fillStyle = "#fff";
    g.strokeStyle = "rgba(0,0,0,.85)";
    for (const [cx, cy] of handles(p)) {
      g.fillRect(cx - half, cy - half, 2 * half, 2 * half);
      g.strokeRect(cx - half, cy - half, 2 * half, 2 * half);
    }
  }
}

function layout() {
  const wrap = $("stageWrap"), fw = W + 2 * M, fh = H + 2 * M;
  view.k = view.zoom === "fit"
    ? Math.max(0.05, Math.min((wrap.clientWidth - 24) / fw, (wrap.clientHeight - 24) / fh)) : +view.zoom;
  const k = view.k, res = Math.min(window.devicePixelRatio || 1, 6000 / (fw * k));
  stage.style.width = `${fw * k}px`;
  stage.style.height = `${fh * k}px`;
  Object.assign(screenCv.style, { left: `${M * k}px`, top: `${M * k}px`, width: `${W * k}px`, height: `${H * k}px`,
                                  imageRendering: k >= 1 ? "pixelated" : "auto" });
  for (const cv of [ghostCv, overlayCv]) {
    cv.style.width = `${fw * k}px`;
    cv.style.height = `${fh * k}px`;
    cv.width = Math.round(fw * k * res);
    cv.height = Math.round(fh * k * res);
  }
  draw();
}

function setZoom(z) {
  const wrap = $("stageWrap"), cx = wrap.scrollLeft + wrap.clientWidth / 2, cy = wrap.scrollTop + wrap.clientHeight / 2;
  const k0 = view.k;
  view.zoom = z;
  $("zoom").value = [...$("zoom").options].some((o) => o.value === String(z)) ? String(z) : "";
  layout();
  if (z !== "fit") { wrap.scrollLeft = cx * view.k / k0 - wrap.clientWidth / 2; wrap.scrollTop = cy * view.k / k0 - wrap.clientHeight / 2; }
}
function zoomStep(dir) {
  const steps = [0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.7, 1, 1.5, 2, 3, 4];
  const k = view.k, next = dir > 0 ? steps.find((s) => s > k * 1.01) : [...steps].reverse().find((s) => s < k * 0.99);
  if (next) setZoom(next);
}

// --- Changing the document: history, saving --------------------------------------------------------
//
// Every edit goes through `edit`: it snapshots the document for undo, applies the change, and then draws, refreshes
// the panels and saves a moment later. Drags snapshot once at the start and commit at the end.

const hist = { undo: [], redo: [], key: "", at: 0 };
const snapshot = () => JSON.stringify(doc);
function pushUndo(before, key = "") {
  const now = Date.now();
  if (key && key === hist.key && now - hist.at < 1000 && hist.undo.length) { hist.at = now; return; }  // typing: one step
  hist.undo.push(before);
  if (hist.undo.length > 200) hist.undo.shift();
  hist.redo.length = 0;
  hist.key = key;
  hist.at = now;
}

/** Applies `fn` to the document as one undo step (`key` merges quick repeats, like typing), then refreshes. */
function edit(fn, key = "") {
  const before = snapshot();
  if (fn() === false) return false;  // the change won't do (a bad value typed): nothing happened
  const after = snapshot();
  if (after === before) return false;
  pushUndo(before, key);
  changed();
  return true;
}

function restore(state) {
  doc = JSON.parse(state);
  if (sel.scene !== "global" && !sceneById(sel.scene)) sel.scene = doc.scenes[0]?.id ?? "global";
  sel.ids = sel.ids.filter((id) => find(id));
  if (sel.layer && !layerById(sel.layer)) sel.layer = null;
  changed();
}
function undo() {
  if (!hist.undo.length) return;
  hist.redo.push(snapshot());
  hist.key = "";
  restore(hist.undo.pop());
}
function redo() {
  if (!hist.redo.length) return;
  hist.undo.push(snapshot());
  hist.key = "";
  restore(hist.redo.pop());
}

/** After the document changed: draw it, refresh the panels and save. */
function changed({ save = true } = {}) {
  redraw();
  showLayers();
  showProps();
  showStrip();
  thumbsSoon();
  if (liveTexts().join("\n") !== live.asked) resolveSoon();
  if (save) saveSoon();
}

const saving = { timer: 0, last: Promise.resolve(), pending: false };
function saveSoon(ms = 400) { saving.pending = true; clearTimeout(saving.timer); saving.timer = setTimeout(flushSave, ms); }
function flushSave() {  // now, after the one before it, so saves arrive in order
  clearTimeout(saving.timer);
  if (!saving.pending) return saving.last;
  saving.pending = false;
  const body = snapshot();
  return (saving.last = saving.last.then(() => api("/api/playlist", { method: "POST", headers: { "Content-Type": "application/json" },
                                                                    body, keepalive: true }).then(async (res) => {
    if (!res.ok) throw new Error((await res.json()).error || res.statusText);
  })).catch((err) => message(`Couldn't save the playlist: ${err.message}`, "warn")));
}
window.addEventListener("pagehide", flushSave);

// --- Selection ----------------------------------------------------------------------------------

function select(ids, layer = null) {
  sel.ids = [...new Set(ids)];
  if (layer) sel.layer = layer;
  if (sel.ids.length) sel.layer = find(sel.ids[sel.ids.length - 1])?.layer.id ?? sel.layer;
  showLayers();
  showProps();
  redraw();
}
function toggleSelect(id) { select(sel.ids.includes(id) ? sel.ids.filter((i) => i !== id) : [...sel.ids, id]); }

/** Opens a scene (or "global") for editing. */
function showScene(id) {
  if (id !== "global") lastScene = id;
  else if (!lastScene) lastScene = doc.scenes[0]?.id ?? null;
  sel.scene = id;
  sel.ids = [];
  sel.layer = null;  // nothing selected: the scene's own properties show
  endDrag();
  $("sceneTitle").textContent = id === "global" ? "Global layers (over every scene)" : (holder()?.name || "scene");
  changed({ save: false });
}

/** The layer new objects go on: the selected one, else the topmost usable one, made if there is none. */
function targetLayer() {
  const h = holder();
  if (!h) return null;
  const chosen = selectedLayer();
  if (chosen && isLayerUsable(chosen)) return chosen;
  const usable = [...layersOf(h)].reverse().find(isLayerUsable);
  if (usable) return usable;
  const layer = { id: uid("l", takenIds(h)), name: `Layer ${layersOf(h).length + 1}`, visible: true, locked: false, opacity: 1, objects: [] };
  h.layers = [...layersOf(h), layer];
  return layer;
}

function addObject(o, at = null) {
  edit(() => {
    const layer = targetLayer();
    if (!layer) return;
    o.id = uid(o.type[0], takenIds());
    layer.objects = layer.objects || [];
    layer.objects.splice(at ?? layer.objects.length, 0, o);
    sel.ids = [o.id];
    sel.layer = layer.id;
  });
  return o;
}

function deleteSelected() {
  if (!sel.ids.length) return;
  edit(() => {
    for (const id of sel.ids) { const f = find(id); if (f) f.layer.objects.splice(f.index, 1); }
    sel.ids = [];
  });
}

// --- Copy, paste, duplicate ------------------------------------------------------------------------

let clipboard = [];
function copySelected() {
  const objs = selectedObjects();
  if (!objs.length) return;
  clipboard = structuredClone(objs);
  try { navigator.clipboard.writeText(JSON.stringify({ wvObjects: clipboard })).catch(() => {}); } catch {}
  message(`Copied ${objs.length} object${objs.length > 1 ? "s" : ""}.`, "muted");
}
function pasteObjects(list, dx = 12, dy = 12) {
  if (!list.length || !holder()) return;
  edit(() => {
    const layer = targetLayer(), ids = [];
    for (const src of list) {
      const o = structuredClone(src);
      o.id = uid(o.type[0], takenIds());
      o.x = num(o.x, 0) + dx; o.y = num(o.y, 0) + dy;
      layer.objects.push(o);
      ids.push(o.id);
    }
    sel.ids = ids;
    sel.layer = layer.id;
  });
}
function duplicateSelected() {
  const objs = selectedObjects();
  if (!objs.length) return;
  edit(() => {
    const ids = [];
    for (const src of objs) {
      const f = find(src.id), o = structuredClone(src);
      o.id = uid(o.type[0], takenIds());
      o.x = num(o.x, 0) + 12; o.y = num(o.y, 0) + 12;
      f.layer.objects.splice(f.index + 1, 0, o);
      ids.push(o.id);
    }
    sel.ids = ids;
  });
}

// --- Snapping (edges onto the screen's edges and the curve guides) -----------------------------------

const SNAP_REACH = 10;  // CSS pixels on screen
function snapLines() {
  const unique = (a) => [...new Set(a)];
  return { x: unique([0, W, SAFE.left, W - SAFE.right]), y: unique([0, H, SAFE.top, H - SAFE.bottom]) };
}
/** Shifts a span starting at `start` so whichever end is nearest a line within reach sits on it.
 *  Returns [new start, the line, or null]. */
function snapSpan(start, size, lines) {
  const reach = SNAP_REACH / view.k;
  let best = null;
  for (const line of lines) {
    for (const edge of [start, start + size]) {
      if (Math.abs(line - edge) <= reach && (!best || Math.abs(line - edge) < Math.abs(best.line - best.edge)))
        best = { line, edge };
    }
  }
  return best ? [start + best.line - best.edge, best.line] : [start, null];
}
/** The nearest line within reach of a point's coordinate, or null. */
function snapPoint(v, lines) {
  const reach = SNAP_REACH / view.k;
  let best = null;
  for (const line of lines) if (Math.abs(line - v) <= reach && (best === null || Math.abs(line - v) < Math.abs(best - v))) best = line;
  return best;
}

// --- Media placement and sizing -------------------------------------------------------------------

/** The art size of a media object: its turned source in art pixels (file pixels / `pixel`). */
function artSize(o) {
  const m = getMedia(o.src);
  if (!m?.info) return [Math.max(1, o.w), Math.max(1, o.h)];
  const [w, h] = turnedSize(m, o), p = Math.max(1, num(o.pixel, 1));
  return [w / p, h / p];
}
function scaleOf(o) { const [aw] = artSize(o); return o.w / aw; }
function setScale(o, s, centre = true) {
  const [aw, ah] = artSize(o), cx = o.x + o.w / 2, cy = o.y + o.h / 2;
  s = Math.max(Math.max(1 / aw, 1 / ah, 0.01), Math.min(s, 4 * W / aw, 4 * H / ah));
  o.w = Math.max(1, Math.round(aw * s)); o.h = Math.max(1, Math.round(ah * s));
  if (centre) { o.x = Math.round(cx - o.w / 2); o.y = Math.round(cy - o.h / 2); }
}
/** Into (or over) a box: whole-number scales for pixel art, in-between (and smooth) only when it has to shrink. */
function fitMedia(o, box, cover) {
  const [aw, ah] = artSize(o);
  const exact = (cover ? Math.max : Math.min)(box.w / aw, box.h / ah);
  let s = cover ? Math.ceil(exact - 1e-9) : Math.floor(exact + 1e-9);
  if (s < 1) { s = exact; o.smooth = true; }
  setScale(o, s, false);
  o.x = Math.round(box.x + (box.w - o.w) / 2);
  o.y = Math.round(box.y + (box.h - o.h) / 2);
}

/** A new image or video object from an upload's info, fitted to the flat front, or centred where it was dropped. */
function placeMedia(info, at = null) {
  const o = { id: "", type: info.type === "video" ? "video" : "image", name: info.name.replace(/\.[^.]+$/, ""), visible: true, locked: false,
              opacity: 1, x: 0, y: 0, w: info.width, h: info.height, rotate: 0, flip: false, src: info.src, smooth: false, fill: "none",
              pixel: Math.max(1, num(info.pixel, 1)) };
  const m = getMedia(info.src);
  if (!m.info) { useMediaInfo(m, info); loadFrames(m).catch((err) => { m.error = err.message; redraw(); }); }
  fitMedia(o, FRONT(), false);
  if (at) { o.x = Math.round(at[0] - o.w / 2); o.y = Math.round(at[1] - o.h / 2); }
  addObject(o);
  if (holder() && !isGlobal() && hex(holder().background, null) === null) holder().background = "#000000";
  return o;
}

async function uploadFile(file, at = null) {
  if (!holder()) return message("Add a scene first.", "warn");
  message(`Opening ${file.name}…`);
  try {
    const res = await api(`/api/upload?name=${encodeURIComponent(file.name)}`, { method: "POST", body: file });
    const info = await res.json();
    if (!res.ok) throw new Error(info.error || res.statusText);
    placeMedia(info, at);
    setTool("select");
    message("");
  } catch (err) {
    message(`Couldn't open ${file.name}: ${err.message}`, "warn");
  }
}

function newText(x, y) {
  return { id: "", type: "text", name: "Text", visible: true, locked: false, opacity: 1, x, y, w: 0, h: 0, rotate: 0, flip: false,
           text: "Text", font: DEFAULT_FONT, size: 48, color: "#ffffff", align: "left", valign: "top", line_height: 1.2,
           outline: { color: "#000000", width: 0 }, crisp: false };
}
function newShape(kind, x, y, w, h) {
  const o = { id: "", type: "shape", name: { rect: "Rectangle", ellipse: "Ellipse", line: "Line" }[kind], visible: true, locked: false,
              opacity: 1, x, y, w, h, rotate: 0, flip: false, shape: kind, radius: 0, crisp: false,
              fill: { type: "solid", color: "#ffffff" }, stroke: null };
  if (kind === "line") { o.fill = null; o.stroke = { color: "#ffffff", width: 4 }; }
  return o;
}

// --- Tools and the pointer ------------------------------------------------------------------------

function setTool(name) {
  if (name === "shape" && tool.name === "shape") $("shapeMenu").classList.toggle("open");
  else $("shapeMenu").classList.toggle("open", name === "shape" && !tool.shape);
  tool.name = name;
  for (const b of document.querySelectorAll("[data-tool]")) b.setAttribute("aria-pressed", b.dataset.tool === name);
  for (const b of document.querySelectorAll("[data-shape]")) b.setAttribute("aria-pressed", b.dataset.shape === tool.shape);
  stage.classList.toggle("hand", name === "hand");
  stage.classList.toggle("place", name === "text" || name === "shape" || name === "image");
  if (name === "image") $("file").click();
}

let drag = null;  // {kind: "move"|"resize"|"rubber"|"hand", ...}

function setPicking(what) {  // "bg": the next click on the canvas sets the scene's background
  view.picking = what;
  stage.classList.toggle("picking", !!what);
  for (const id of ["sPick", "mPick"]) $(id).setAttribute("aria-pressed", !!what);
  message(what ? "Click a colour on the canvas (Esc to cancel)." : "");
}

stage.addEventListener("pointerdown", (e) => {
  if (e.button === 1 || (e.button === 0 && (tool.name === "hand" || tool.space))) {
    drag = { kind: "hand", x: e.clientX, y: e.clientY, sl: $("stageWrap").scrollLeft, st: $("stageWrap").scrollTop };
    stage.classList.add("dragging");
    try { stage.setPointerCapture(e.pointerId); } catch {}
    return e.preventDefault();
  }
  if (e.button !== 0) return;
  if (view.picking) {
    const [px, py] = screenPoint(e), what = view.picking;
    setPicking(null);
    if (px >= 0 && py >= 0 && px < W && py < H && what === "bg" && !isGlobal() && holder()) {
      const [r, g, b] = sctx.getImageData(px, py, 1, 1).data;
      edit(() => { holder().background = `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`; });
    }
    return;
  }
  if (!holder()) return;
  try { stage.setPointerCapture(e.pointerId); } catch {}
  const [px, py] = screenPoint(e, true);
  if (tool.name === "text") {
    const o = addObject(newText(Math.round(px), Math.round(py)));
    setTool("select");
    showProps();
    $("wText").focus();
    $("wText").select();
    return;
  }
  if (tool.name === "shape") {
    drag = { kind: "rubber", x: Math.round(px), y: Math.round(py), before: snapshot() };
    view.rubber = { x: drag.x, y: drag.y, w: 0, h: 0, line: tool.shape === "line" };
    return;
  }
  if (tool.name === "image") { $("file").click(); return; }
  const handle = handleAt(e);
  if (handle) {
    const o = primary(), [, , kind] = handle;
    drag = { kind: "resize", id: o.id, handle: kind, before: snapshot(), start: { x: o.x, y: o.y, w: o.w, h: o.h } };
    if (typeof kind === "number") drag.anchor = handles(o)[3 - kind];
    return;
  }
  const hit = objectAt(e);
  if (!hit) { if (!e.shiftKey && sel.ids.length) select([]); return; }
  if (e.shiftKey) { toggleSelect(hit.obj.id); if (!sel.ids.includes(hit.obj.id)) return; }
  else if (!sel.ids.includes(hit.obj.id)) select([hit.obj.id]);
  else if (sel.ids[sel.ids.length - 1] !== hit.obj.id) select([...sel.ids.filter((i) => i !== hit.obj.id), hit.obj.id]);
  drag = { kind: "move", cx: e.clientX, cy: e.clientY, before: snapshot(), lead: hit.obj.id,
           starts: selectedObjects().map((o) => ({ id: o.id, x: num(o.x, 0), y: num(o.y, 0) })) };
  stage.classList.add("dragging");
});

stage.addEventListener("pointermove", (e) => {
  const [px, py] = screenPoint(e);
  $("cursor").textContent = px >= 0 && py >= 0 && px < W && py < H ? `x ${px}, y ${py}` : "";
  if (!drag) {
    let cursor = "";
    if (tool.name === "select" && !view.picking && !tool.space) {
      const h = handleAt(e);
      cursor = h ? (typeof h[2] === "number" ? ["nwse-resize", "nesw-resize", "nesw-resize", "nwse-resize"][h[2]] : "move") : objectAt(e) ? "move" : "";
    }
    stage.style.cursor = cursor;
    return;
  }
  if (drag.kind === "hand") {
    $("stageWrap").scrollLeft = drag.sl - (e.clientX - drag.x);
    $("stageWrap").scrollTop = drag.st - (e.clientY - drag.y);
  } else if (drag.kind === "rubber") {
    const [x, y] = screenPoint(e, true);
    let w = Math.round(x) - drag.x, h = Math.round(y) - drag.y;
    if (e.shiftKey) { const s = Math.max(Math.abs(w), Math.abs(h)); w = Math.sign(w || 1) * s; h = Math.sign(h || 1) * s; }
    view.rubber = { x: drag.x, y: drag.y, w, h, line: tool.shape === "line" };
    drawOverlay();
  } else if (drag.kind === "resize") resizeTo(e);
  else if (drag.kind === "move") moveTo(e);
});

function moveTo(e) {
  let dx = (e.clientX - drag.cx) / view.k, dy = (e.clientY - drag.cy) / view.k;
  if (e.shiftKey) { if (Math.abs(dx) > Math.abs(dy)) dy = 0; else dx = 0; }
  dx = Math.round(dx); dy = Math.round(dy);
  view.snapped = { x: null, y: null };
  const lead = find(drag.lead)?.obj, start = drag.starts.find((s) => s.id === drag.lead);
  if (lead && start && !e.altKey) {  // the lead object's box edges onto the lines (not along an axis Shift has locked)
    const b = objBox(lead), ox = b.x - num(lead.x, 0), oy = b.y - num(lead.y, 0), lines = snapLines();
    let edge;
    if (dx || !e.shiftKey) { [edge, view.snapped.x] = snapSpan(start.x + dx + ox, b.w, lines.x); dx = Math.round(edge - ox - start.x); }
    if (dy || !e.shiftKey) { [edge, view.snapped.y] = snapSpan(start.y + dy + oy, b.h, lines.y); dy = Math.round(edge - oy - start.y); }
  }
  let moved = false;
  for (const s of drag.starts) {
    const o = find(s.id)?.obj;
    if (o && (o.x !== s.x + dx || o.y !== s.y + dy)) { o.x = s.x + dx; o.y = s.y + dy; moved = true; }
  }
  if (moved) { redraw(); showProps(); } else drawOverlay();
}

/** Resizes from the dragged handle: media keep their shape (and snap to whole-number scales), shapes are free,
 *  a line's ends move on their own. */
function resizeTo(e) {
  const o = find(drag.id)?.obj;
  if (!o) return;
  let [px, py] = screenPoint(e, true);
  const lines = snapLines();
  view.snapped = { x: null, y: null };
  const before = [o.x, o.y, o.w, o.h].join();
  if (drag.handle === "a" || drag.handle === "b") {  // a line's end
    px = Math.round(px); py = Math.round(py);
    if (!e.altKey) { const sx = snapPoint(px, lines.x), sy = snapPoint(py, lines.y); if (sx !== null) { px = sx; view.snapped.x = sx; } if (sy !== null) { py = sy; view.snapped.y = sy; } }
    if (drag.handle === "a") { const ex = o.x + o.w, ey = o.y + o.h; o.x = px; o.y = py; o.w = ex - px; o.h = ey - py; }
    else { o.w = px - o.x; o.h = py - o.y; }
  } else {
    const [ax, ay] = drag.anchor, left = drag.handle % 2 === 0, top = drag.handle < 2;
    if (isMedia(o)) {
      const [aw, ah] = artSize(o);
      let s = (Math.abs(px - ax) * aw + Math.abs(py - ay) * ah) / (aw * aw + ah * ah), snapped = false;
      if (!e.altKey) {  // the dragged corner onto the nearest line within reach
        const reach = SNAP_REACH / view.k, corner = { x: left ? ax - aw * s : ax + aw * s, y: top ? ay - ah * s : ay + ah * s };
        let best = null;
        for (const [axis, anchor, size] of [["x", ax, aw], ["y", ay, ah]]) {
          for (const line of lines[axis]) {
            const d = Math.abs(line - corner[axis]), k = Math.abs(line - anchor) / size;
            if (d <= reach && k > 0 && (!best || d < best.d)) best = { d, k, axis, line };
          }
        }
        if (best) { s = best.k; view.snapped[best.axis] = best.line; snapped = true; }
        else if (Math.abs(s - Math.round(s)) < 0.08 && Math.round(s) >= 1) s = Math.round(s);  // whole art pixels
      }
      if (!snapped) s = Math.round(s * 1000) / 1000;
      setScale(o, s, false);
      o.x = left ? Math.round(ax - o.w) : ax;
      o.y = top ? Math.round(ay - o.h) : ay;
    } else {
      px = Math.round(px); py = Math.round(py);
      if (!e.altKey) { const sx = snapPoint(px, lines.x), sy = snapPoint(py, lines.y); if (sx !== null) { px = sx; view.snapped.x = sx; } if (sy !== null) { py = sy; view.snapped.y = sy; } }
      const x0 = Math.min(ax, px), x1 = Math.max(ax, px), y0 = Math.min(ay, py), y1 = Math.max(ay, py);
      o.x = x0; o.y = y0; o.w = Math.max(1, x1 - x0); o.h = Math.max(1, y1 - y0);
    }
  }
  if ([o.x, o.y, o.w, o.h].join() !== before) { redraw(); showProps(); } else drawOverlay();
}

function endDrag() {
  if (!drag) return;
  const d = drag;
  drag = null;
  stage.classList.remove("dragging");
  if (d.kind === "rubber") {
    const r = view.rubber;
    view.rubber = null;
    if (r && (Math.abs(r.w) >= 2 || Math.abs(r.h) >= 2)) {
      let o;
      if (tool.shape === "line") o = newShape("line", r.x, r.y, r.w, r.h);
      else o = newShape(tool.shape, Math.min(r.x, r.x + r.w), Math.min(r.y, r.y + r.h), Math.abs(r.w), Math.abs(r.h));
      addObject(o);
      setTool("select");
    } else drawOverlay();
  } else if (d.before !== undefined) {
    if (snapshot() !== d.before) { pushUndo(d.before); changed(); }
  }
  if (view.snapped) { view.snapped = null; drawOverlay(); }
}
stage.addEventListener("pointerup", endDrag);
stage.addEventListener("pointercancel", endDrag);
stage.addEventListener("pointerleave", () => { $("cursor").textContent = ""; });
$("stageWrap").addEventListener("wheel", (e) => {
  if (!e.ctrlKey) return;
  e.preventDefault();
  zoomStep(e.deltaY < 0 ? 1 : -1);
}, { passive: false });

// --- Keys ----------------------------------------------------------------------------------------

window.addEventListener("keydown", (e) => {
  if ($("sourcesDlg").open) return;  // the window has its own keys, and Esc closes it
  const typing = e.target.closest?.("input:not([type=checkbox]):not([type=range]), select, textarea");
  if (e.key === "Escape") {
    if (view.picking) return setPicking(null);
    if (typing) return e.target.blur();
    if (sel.ids.length) select([]);
    return;
  }
  if (typing) return;
  const ctrl = e.ctrlKey || e.metaKey;
  if (ctrl) {
    const k = e.key.toLowerCase();
    if (k === "z" && e.shiftKey) redo();
    else if (k === "z") undo();
    else if (k === "y") redo();
    else if (k === "c") copySelected();
    else if (k === "v") pasteObjects(clipboard);
    else if (k === "d") duplicateSelected();
    else if (k === "a") select(layersOf().filter(isLayerUsable).flatMap((l) => (l.objects || []).filter((o) => isSelectable(o, l)).map((o) => o.id)));
    else if (k === "=" || k === "+") zoomStep(1);
    else if (k === "-") zoomStep(-1);
    else if (k === "0") setZoom("fit");
    else return;
    return e.preventDefault();
  }
  if (e.altKey) return;
  const step = e.shiftKey ? 10 : 1;
  const move = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[e.key];
  if (move) {
    if (sel.ids.length) edit(() => { for (const o of selectedObjects()) { o.x = num(o.x, 0) + move[0]; o.y = num(o.y, 0) + move[1]; } }, "nudge");
  } else if (e.key === "Delete" || e.key === "Backspace") deleteSelected();
  else if (e.key === " ") { if (!tool.space) { tool.space = true; stage.classList.add("hand"); } }
  else if (/^[vtirhg]$/i.test(e.key)) {
    const k = e.key.toLowerCase();
    if (k === "g") { $("showGuides").click(); }
    else setTool({ v: "select", t: "text", i: "image", r: "shape", h: "hand" }[k]);
  } else return;
  e.preventDefault();
});
window.addEventListener("keyup", (e) => {
  if (e.key === " " && tool.space) { tool.space = false; if (tool.name !== "hand") stage.classList.remove("hand"); }
});

window.addEventListener("dragover", (e) => { e.preventDefault(); document.body.classList.add("dragover"); });
window.addEventListener("dragleave", (e) => { if (!e.relatedTarget) document.body.classList.remove("dragover"); });
window.addEventListener("drop", (e) => {
  e.preventDefault();
  document.body.classList.remove("dragover");
  const f = e.dataTransfer.files[0];
  if (!f) return;
  const onStage = e.target.closest("#stage"), at = onStage ? screenPoint(e, true) : null;
  uploadFile(f, at && at[0] >= 0 && at[0] < W && at[1] >= 0 && at[1] < H ? at : null);
});
window.addEventListener("paste", (e) => {
  if (e.target.closest?.("input, textarea")) return;
  const f = [...e.clipboardData.files][0];
  if (f) return uploadFile(new File([f], f.name || "pasted.png", { type: f.type }));
  try {
    const data = JSON.parse(e.clipboardData.getData("text/plain"));
    if (Array.isArray(data.wvObjects)) pasteObjects(data.wvObjects);
  } catch {}
});

// --- The layers panel ------------------------------------------------------------------------------
//
// Layers top first, each with its objects nested, top first (the document lists them bottom first). Rows drag to
// reorder objects or move them between layers, and to reorder layers.

/** Whether a text object is live (has a placeholder), and whether the player will have to draw it on top of
 *  everything: something animated is above it, in its scene or the global layers. */
function liveOnTop(o, h = holder()) {
  if (o.type !== "text" || !isLive(o.text)) return false;
  let above = false;
  const stacks = h === doc.global ? [doc.global] : [h, doc.global];
  for (const stack of stacks) {
    for (const layer of layersOf(stack)) {
      if (layer.visible === false) continue;
      for (const other of layer.objects || []) {
        if (other === o) { above = true; continue; }
        if (above && other.visible !== false && isAnimated(other)) return true;
      }
    }
  }
  return false;
}

const dnd = { kind: null, id: null };  // what's being dragged in the panel
function showLayers() {
  const h = holder(), list = $("layers"), rows = [];
  const tog = (on, cls, titleOn, titleOff, fn) => {
    const b = make("button", `tog ${cls}`, cls === "eye" ? (on ? "👁" : "◌") : (on ? "🔒" : "🔓"));
    b.setAttribute("aria-pressed", on);
    b.title = on ? titleOn : titleOff;
    b.addEventListener("click", (e) => { e.stopPropagation(); fn(); });
    return b;
  };
  const rename = (nameEl, get, set) => nameEl.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    const input = make("input"); input.type = "text"; input.value = get(); input.spellcheck = false;
    nameEl.replaceChildren(input);
    input.focus(); input.select();
    const done = () => { const v = input.value.trim(); if (v && v !== get()) edit(() => set(v)); else showLayers(); };
    input.addEventListener("blur", done);
    input.addEventListener("keydown", (ev) => { if (ev.key === "Enter") input.blur(); else if (ev.key === "Escape") { input.value = get(); input.blur(); } ev.stopPropagation(); });
    input.addEventListener("click", (ev) => ev.stopPropagation());
  });
  const layers = layersOf(h);
  for (let li = layers.length - 1; li >= 0; li--) {
    const layer = layers[li], item = make("li", "layer"), row = make("div", `row${layer.visible === false ? " hidden-row" : ""}${!sel.ids.length && sel.layer === layer.id ? " selected" : ""}`);
    row.draggable = true;
    row.dataset.layer = layer.id;
    const name = make("span", "name", layer.name || layer.id);
    rename(name, () => layer.name || "", (v) => { layerById(layer.id).name = v; });
    row.append(tog(layer.visible !== false, "eye", "Shown: click to hide", "Hidden: click to show", () => edit(() => { const l = layerById(layer.id); l.visible = l.visible === false; })),
               tog(!!layer.locked, "lock", "Locked: click to unlock", "Unlocked: click to lock", () => edit(() => { const l = layerById(layer.id); l.locked = !l.locked; })),
               name);
    if (clamp01(layer.opacity) < 1) row.append(make("span", "badge op", `${Math.round(100 * clamp01(layer.opacity))}%`));
    row.addEventListener("click", () => select([], layer.id));
    dragRow(row, "layer", layer.id);
    item.append(row);
    const sub = make("ul"), objects = layer.objects || [];
    for (let i = objects.length - 1; i >= 0; i--) {
      const o = objects[i], oi = make("li", "obj");
      const r = make("div", `row${o.visible === false ? " hidden-row" : ""}${sel.ids.includes(o.id) ? " selected" : ""}`);
      r.draggable = true;
      r.dataset.obj = o.id;
      const icon = make("span", "icon", { image: "🖼", video: "🎞", text: "T", shape: { rect: "▭", ellipse: "◯", line: "╱" }[o.shape] || "▭" }[o.type] || "?");
      const n = make("span", "name", o.name || (o.type === "text" ? String(o.text ?? "").split("\n")[0] : o.id) || o.id);
      n.title = o.type === "text" ? String(o.text ?? "") : o.src || "";
      rename(n, () => o.name || "", (v) => { find(o.id).obj.name = v; });
      r.append(tog(o.visible !== false, "eye", "Shown: click to hide", "Hidden: click to show", () => edit(() => { const x = find(o.id).obj; x.visible = x.visible === false; })),
               tog(!!o.locked, "lock", "Locked: click to unlock", "Unlocked: click to lock", () => edit(() => { const x = find(o.id).obj; x.locked = !x.locked; })),
               icon, n);
      if (isAnimated(o)) { const b = make("span", "badge anim", "anim"); b.title = "Animated: loops on its own timing"; r.append(b); }
      if (o.type === "text" && isLive(o.text)) {
        const top = liveOnTop(o, h), b = make("span", "badge" + (top ? " top" : " live"), top ? "live, on top" : "live");
        b.title = top ? "Live text with something animated above it: the player draws it on top of everything"
          : "Live text: the player fills in its values while playing";
        r.append(b);
      }
      if (clamp01(o.opacity) < 1) r.append(make("span", "badge op", `${Math.round(100 * clamp01(o.opacity))}%`));
      r.addEventListener("click", (e) => { if (e.shiftKey) toggleSelect(o.id); else select([o.id]); });
      dragRow(r, "obj", o.id);
      oi.append(r);
      sub.append(oi);
    }
    item.append(sub);
    rows.push(item);
  }
  if (!h) rows.push(make("li", "empty", "No scene: add one below."));
  else if (!layers.length) rows.push(make("li", "empty", "No layers yet: + Layer, or just add something."));
  list.replaceChildren(...rows);
  $("deleteLayer").disabled = !selectedLayer();
  $("addLayer").disabled = !h;
}

function dragRow(row, kind, id) {
  row.addEventListener("dragstart", (e) => { dnd.kind = kind; dnd.id = id; e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", id); });
  row.addEventListener("dragend", () => { dnd.kind = null; clearOver(); });
  row.addEventListener("dragover", (e) => {
    if (!dnd.kind) return;
    const where = dropWhere(row, e);
    if (!where) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    clearOver();
    row.classList.add(`over-${where}`);
  });
  row.addEventListener("drop", (e) => {
    const where = dropWhere(row, e);
    clearOver();
    if (!where || !dnd.kind) return;
    e.preventDefault();
    if (dnd.kind === "obj") {
      if (kind === "obj") moveObject(dnd.id, find(id).layer.id, id, where === "top" ? "before" : "after");
      else moveObject(dnd.id, id, null, "top");
    } else if (kind === "layer") moveLayer(dnd.id, id, where === "top" ? "above" : "below");
  });
}
/** Where a drop on this row would go: "top" or "bottom" (of the row, as shown), "into" a layer for an object. */
function dropWhere(row, e) {
  const same = (dnd.kind === "obj" && row.dataset.obj === dnd.id) || (dnd.kind === "layer" && row.dataset.layer === dnd.id);
  if (same) return null;
  if (dnd.kind === "layer" && !row.dataset.layer) return null;
  if (dnd.kind === "obj" && row.dataset.layer) return "into";
  const r = row.getBoundingClientRect();
  return e.clientY < r.top + r.height / 2 ? "top" : "bottom";
}
function clearOver() { for (const el of $("layers").querySelectorAll(".over-top, .over-bottom, .over-into")) el.classList.remove("over-top", "over-bottom", "over-into"); }

/** Moves an object to a layer: before or after another object (in the shown order: "before" = above it), or to the
 *  top of the layer. */
function moveObject(id, layerId, otherId, where) {
  edit(() => {
    const f = find(id), layer = layerById(layerId);
    if (!f || !layer) return;
    const [o] = f.layer.objects.splice(f.index, 1);
    layer.objects = layer.objects || [];
    let at = layer.objects.length;
    if (otherId) {
      const j = layer.objects.findIndex((x) => x.id === otherId);
      if (j >= 0) at = where === "before" ? j + 1 : j;  // the list is bottom first: above = after in the list
    }
    layer.objects.splice(at, 0, o);
    sel.ids = [id];
    sel.layer = layer.id;
  });
}
function moveLayer(id, otherId, where) {
  edit(() => {
    const layers = layersOf(), i = layers.findIndex((l) => l.id === id);
    if (i < 0) return;
    const [l] = layers.splice(i, 1);
    const j = layers.findIndex((x) => x.id === otherId);
    layers.splice(j < 0 ? layers.length : where === "above" ? j + 1 : j, 0, l);
  });
}

$("addLayer").addEventListener("click", () => {
  const h = holder();
  if (!h) return;
  edit(() => {
    const layers = layersOf(h), chosen = selectedLayer(), id = uid("l", takenIds(h));
    const layer = { id, name: `Layer ${layers.length + 1}`, visible: true, locked: false, opacity: 1, objects: [] };
    const at = chosen ? layers.indexOf(chosen) + 1 : layers.length;
    layers.splice(at, 0, layer);
    h.layers = layers;
    sel.ids = [];
    sel.layer = id;
  });
});
$("deleteLayer").addEventListener("click", () => {
  const layer = selectedLayer();
  if (!layer) return;
  edit(() => {
    const layers = layersOf(), i = layers.findIndex((l) => l.id === layer.id);
    layers.splice(i, 1);
    sel.ids = [];
    sel.layer = layers[Math.min(i, layers.length - 1)]?.id ?? null;
  });
});

// --- The properties panel ----------------------------------------------------------------------------

const set = (id, v) => { if (document.activeElement !== $(id)) $(id).value = v; };
const seg = (id, value) => { for (const b of $(id).children) b.setAttribute("aria-pressed", b.dataset.value === String(value)); };

function showProps() {
  const h = holder(), p = primary(), layer = !p && selectedLayer();
  $("pScene").hidden = !(h && !isGlobal() && !p && !layer);
  $("pGlobal").hidden = !(isGlobal() && !p && !layer);
  $("pLayer").hidden = !layer;
  $("pObject").hidden = !p;
  $("pMedia").hidden = !(p && isMedia(p));
  $("pText").hidden = !(p && p.type === "text");
  $("pShape").hidden = !(p && p.type === "shape");
  $("pExport").hidden = !(h && !isGlobal() && !p && !layer);
  if (!h) return;
  if (!$("pScene").hidden) {
    set("sName", h.name || "");
    set("sSeconds", num(h.seconds, 30));
    set("sBg", hex(h.background, "#000000"));
    const n = layersOf(h).reduce((a, l) => a + (l.objects || []).length, 0);
    $("sceneNote").textContent = `${layersOf(h).length} layer${layersOf(h).length === 1 ? "" : "s"}, ${n} object${n === 1 ? "" : "s"}. Click an object on the canvas or in Layers to edit it.`;
  }
  if (layer) {
    set("lName", layer.name || "");
    set("lOpacity", Math.round(100 * clamp01(layer.opacity)));
    set("lOpacityR", Math.round(100 * clamp01(layer.opacity)));
  }
  if (!p) return;
  $("oKind").textContent = { image: "Image", video: "Video", text: "Text", shape: "Shape" }[p.type] || p.type;
  set("oName", p.name || "");
  set("oOpacity", Math.round(100 * clamp01(p.opacity)));
  set("oOpacityR", Math.round(100 * clamp01(p.opacity)));
  set("oX", num(p.x, 0)); set("oY", num(p.y, 0)); set("oW", num(p.w, 0)); set("oH", num(p.h, 0));
  $("oSizeRow").hidden = p.type === "text";
  $("oSizeRow").style.display = p.type === "text" ? "none" : "contents";
  $("oTurnRow").hidden = !isMedia(p);
  seg("oRotate", num(p.rotate, 0) % 360);
  $("oFlip").checked = !!p.flip;
  if (isMedia(p)) showMediaProps(p);
  else if (p.type === "text") showTextProps(p);
  else showShapeProps(p);
}

function showMediaProps(p) {
  const m = getMedia(p.src), info = m?.info;
  $("mKind").textContent = p.type === "video" ? "Video" : "Image";
  const [aw, ah] = artSize(p), s = scaleOf(p);
  $("mInfo").textContent = !info ? (m?.error ? `Can't open ${p.src}: ${m.error}` : "Loading…")
    : `${info.width} × ${info.height} file${num(p.pixel, 1) > 1 ? `, ${+aw.toFixed(2)} × ${+ah.toFixed(2)} art pixels at ${p.pixel}×` : ""}`
      + (info.video ? `; video, ${(m.total / 1000).toFixed(1)} s at ${+info.video.fps.toFixed(2)} fps${info.video.trimmed ? " (the first 30 s)" : ""}`
         : info.durations.length > 1 ? `; ${info.durations.length} frames, ${(m.total / 1000).toFixed(2)} s loop` : "");
  $("mScale").textContent = `× ${Number.isInteger(+s.toFixed(3)) ? s : s.toFixed(2)}`;
  set("mPixel", Math.max(1, num(p.pixel, 1)));
  $("mSmooth").checked = !!p.smooth;
  seg("mFill", p.fill || "none");
  const notes = [];
  if (!Number.isInteger(+s.toFixed(3)) && !p.smooth) notes.push(`in-between size: art pixels come out uneven (${Math.floor(s)} or ${Math.ceil(s)} px)`);
  else if (!p.smooth && Number.isInteger(s)) notes.push(s % 2 ? "odd size: art pixels come out slightly uneven on the glass" : `each art pixel is ${s * 1.5}×${s * 1.5} glass pixels`);
  if (p.smooth) notes.push("smooth preview is close; the player is a little sharper");
  $("mNote").textContent = notes.join(". ");
  $("mEdge").disabled = !info?.edge;
}

function showTextProps(w) {
  set("wText", String(w.text ?? ""));
  set("wSize", num(w.size, 48));
  set("wColor", hex(w.color, "#ffffff"));
  set("wLineHeight", num(w.line_height, 1.2));
  set("wOutlineColor", hex(w.outline?.color, "#000000"));
  set("wOutlineWidth", num(w.outline?.width, 0));
  $("wCrisp").checked = !!w.crisp;
  seg("wAlign", w.align || "left");
  seg("wValign", w.valign || "top");
  showFonts(w);
  const top = liveOnTop(w);
  $("liveNote").hidden = !top;
  $("liveNote").textContent = top ? "This text is live and something animated is above it, so the player draws it on top of everything." : "";
  showLiveNote();
}

function showShapeProps(o) {
  $("shKind").textContent = { rect: "Rectangle", ellipse: "Ellipse", line: "Line" }[o.shape] || "Shape";
  const line = o.shape === "line", f = o.fill, type = line ? "none" : f?.type === "linear" ? "linear" : f?.type === "solid" ? "solid" : "none";
  $("shFillType").disabled = line;
  set("shFillType", type);
  const stops = f?.type === "linear" ? gradientStops(f) : null;
  set("shColor", type === "linear" ? stops[0].color : hex(f?.color, "#ffffff"));
  $("shColor").hidden = type === "none";
  $("shGradRow").hidden = type !== "linear";
  if (stops) { set("shColor2", stops[stops.length - 1].color); set("shAngle", num(f.angle, 0)); }
  const s = o.stroke;
  $("shStrokeOn").checked = !!s && num(s.width, 0) > 0;
  set("shStrokeColor", hex(s?.color, "#ffffff"));
  set("shStrokeWidth", s ? num(s.width, 0) : 0);
  $("shRadiusRow").hidden = o.shape !== "rect";
  set("shRadius", num(o.radius, 0));
  $("shCrisp").checked = !!o.crisp;
}

let fontsShown = "";
/** The font picker: the fonts whose names have every word typed in the filter, plus the widget's own. */
function showFonts(w) {
  const words = $("wFontFilter").value.toLowerCase().split(/\s+/).filter(Boolean);
  const current = typeof w.font === "string" && w.font ? w.font : DEFAULT_FONT;
  const key = JSON.stringify([words, current, fontList.length]);
  if (key === fontsShown) return;  // rebuilt only when it changes, so the list keeps its scroll position
  fontsShown = key;
  const name = (f) => `${f.family} ${f.style}`.trim(), count = {};
  for (const f of fontList) count[name(f)] = (count[name(f)] || 0) + 1;
  const label = (f) => count[name(f)] > 1 ? `${name(f)} · ${f.path.split("\\").pop()}` : name(f);  // which file
  const same = (path) => path.toLowerCase() === current.toLowerCase();
  const options = fontList.filter((f) => words.every((word) => name(f).toLowerCase().includes(word)))
    .map((f) => new Option(label(f), f.path, false, same(f.path)));
  if (!options.some((o) => o.selected)) {
    const known = fontList.find((f) => same(f.path)), file = current.split("\\").pop();
    options.unshift(new Option(known ? label(known) : fontList.length ? `${file} (not listed: previewed in Segoe UI)` : file,
                               current, false, true));
  }
  $("wFont").replaceChildren(...options);
}

// Wiring the controls: each changes the selection through `edit`, looked up fresh by id.
const number = (el, ok) => { const v = parseFloat(el.value); return ok(v) ? v : null; };
/** A control on the primary object: `apply(o, el)` changes it, or returns false when the value won't do. */
const objControl = (id, apply, event = "input", key = id) => $(id).addEventListener(event, () => {
  const p = primary();
  if (p) edit(() => apply(find(p.id).obj, $(id)), key);
});

objControl("oName", (o, el) => { o.name = el.value; });
objControl("oOpacity", (o, el) => { const v = number(el, (v) => v >= 0 && v <= 100); if (v === null) return false; o.opacity = v / 100; });
objControl("oOpacityR", (o, el) => { o.opacity = +el.value / 100; });
objControl("oX", (o, el) => { const v = number(el, Number.isFinite); if (v === null) return false; o.x = Math.round(v); });
objControl("oY", (o, el) => { const v = number(el, Number.isFinite); if (v === null) return false; o.y = Math.round(v); });
objControl("oW", (o, el) => { const v = number(el, Number.isFinite); if (v === null || (o.shape !== "line" && v < 1)) return false; o.w = Math.round(v); }, "change");
objControl("oH", (o, el) => { const v = number(el, Number.isFinite); if (v === null || (o.shape !== "line" && v < 1)) return false; o.h = Math.round(v); }, "change");
objControl("oFlip", (o, el) => { o.flip = el.checked; }, "change");
for (const b of $("oRotate").children) b.addEventListener("click", () => {
  const p = primary();
  if (!p || !isMedia(p)) return;
  edit(() => {
    const o = find(p.id).obj, to = +b.dataset.value, from = num(o.rotate, 0) % 360;
    if (((to - from) % 180 + 180) % 180) { const cx = o.x + o.w / 2, cy = o.y + o.h / 2; [o.w, o.h] = [o.h, o.w]; o.x = Math.round(cx - o.w / 2); o.y = Math.round(cy - o.h / 2); }
    o.rotate = to;
  });
});
// layer and scene
const layerControl = (id, apply, event = "input") => $(id).addEventListener(event, () => {
  const l = !primary() && selectedLayer();
  if (l) edit(() => apply(layerById(l.id), $(id)), id);
});
layerControl("lName", (l, el) => { l.name = el.value; });
layerControl("lOpacity", (l, el) => { const v = number(el, (v) => v >= 0 && v <= 100); if (v !== null) l.opacity = v / 100; });
layerControl("lOpacityR", (l, el) => { l.opacity = +el.value / 100; });
const sceneControl = (id, apply, event = "input") => $(id).addEventListener(event, () => {
  if (holder() && !isGlobal()) edit(() => apply(holder(), $(id)), id);
});
sceneControl("sName", (s, el) => { s.name = el.value; $("sceneTitle").textContent = s.name; });
sceneControl("sSeconds", (s, el) => { const v = number(el, (v) => v >= 1); if (v !== null) s.seconds = Math.round(v); }, "change");
sceneControl("sBg", (s, el) => { s.background = el.value; });
$("sBlack").addEventListener("click", () => { if (holder() && !isGlobal()) edit(() => { holder().background = "#000000"; }); });
$("sPick").addEventListener("click", () => setPicking(view.picking ? null : "bg"));
$("mPick").addEventListener("click", () => setPicking(view.picking ? null : "bg"));
$("mEdge").addEventListener("click", () => {
  const p = primary(), edge = p && getMedia(p.src)?.info?.edge;
  if (edge && !isGlobal()) edit(() => { holder().background = edge; });
});
// media
objControl("mSmaller", (o) => { setScale(o, Math.ceil(scaleOf(o) - 1e-6) - 1); }, "click");
objControl("mBigger", (o) => { setScale(o, Math.floor(scaleOf(o) + 1e-6) + 1); }, "click");
objControl("mPixel", (o, el) => { const v = number(el, (v) => v >= 1 && v <= 32); if (v === null) return false; o.pixel = Math.round(v); fitMedia(o, FRONT(), false); }, "change");
objControl("mSmooth", (o, el) => { o.smooth = el.checked; }, "change");
objControl("fitFront", (o) => fitMedia(o, FRONT(), false), "click");
objControl("fitScreen", (o) => fitMedia(o, WHOLE(), false), "click");
objControl("fillScreen", (o) => fitMedia(o, WHOLE(), true), "click");
objControl("fillHeight", (o) => {
  const [, ah] = artSize(o), f = FRONT();
  setScale(o, Math.ceil(H / ah - 1e-9), false);
  o.x = Math.round(f.x + (f.w - o.w) / 2); o.y = Math.round((H - o.h) / 2);
}, "click");
for (const b of $("mFill").children) b.addEventListener("click", () => { const p = primary(); if (p) edit(() => { find(p.id).obj.fill = b.dataset.value; }); });
// text
objControl("wText", (o, el) => { o.text = el.value; }, "input", "wText");
$("wFontFilter").addEventListener("input", () => { const w = primary(); if (w?.type === "text") showFonts(w); });
objControl("wFont", (o, el) => { o.font = el.value; }, "change");
objControl("wSize", (o, el) => { const v = number(el, (v) => v > 0); if (v === null) return false; o.size = v; });
objControl("wColor", (o, el) => { o.color = el.value; });
objControl("wLineHeight", (o, el) => { const v = number(el, (v) => v > 0); if (v === null) return false; o.line_height = v; });
objControl("wOutlineColor", (o, el) => { o.outline = { width: 0, ...o.outline, color: el.value }; });
objControl("wOutlineWidth", (o, el) => { const v = number(el, (v) => v >= 0); if (v === null) return false; o.outline = { color: "#000000", ...o.outline, width: v }; });
objControl("wCrisp", (o, el) => { o.crisp = el.checked; }, "change");
for (const [group, key] of [["wAlign", "align"], ["wValign", "valign"]]) {
  for (const b of $(group).children) b.addEventListener("click", () => { const p = primary(); if (p) edit(() => { find(p.id).obj[key] = b.dataset.value; }); });
}
// shapes
objControl("shFillType", (o, el) => {
  const t = el.value, c = $("shColor").value;
  o.fill = t === "none" ? null : t === "solid" ? { type: "solid", color: hex(o.fill?.type === "linear" ? gradientStops(o.fill)[0].color : c, c) }
    : { type: "linear", angle: 0, stops: [{ at: 0, color: hex(o.fill?.color, c) }, { at: 1, color: "#000000" }] };
}, "change");
objControl("shColor", (o, el) => {
  if (o.fill?.type === "linear") { const s = gradientStops(o.fill); s[0].color = el.value; o.fill.stops = s; }
  else o.fill = { type: "solid", color: el.value };
});
objControl("shColor2", (o, el) => { if (o.fill?.type === "linear") { const s = gradientStops(o.fill); s[s.length - 1].color = el.value; o.fill.stops = s; } });
objControl("shAngle", (o, el) => { const v = number(el, Number.isFinite); if (v === null || o.fill?.type !== "linear") return false; o.fill.angle = v; });
objControl("shStrokeOn", (o, el) => { o.stroke = el.checked ? { color: $("shStrokeColor").value, width: Math.max(1, number($("shStrokeWidth"), (v) => v > 0) || 2) } : null; }, "change");
objControl("shStrokeColor", (o, el) => { o.stroke = { width: 2, ...o.stroke, color: el.value }; });
objControl("shStrokeWidth", (o, el) => { const v = number(el, (v) => v >= 0); if (v === null) return false; o.stroke = v > 0 ? { color: "#ffffff", ...o.stroke, width: v } : null; });
objControl("shRadius", (o, el) => { const v = number(el, (v) => v >= 0); if (v === null) return false; o.radius = v; });
objControl("shCrisp", (o, el) => { o.crisp = el.checked; }, "change");

// --- The playlist strip ----------------------------------------------------------------------------

const thumbCv = document.createElement("canvas"), thumbs = new Map();  // scene id -> {canvas, key}
let thumbTimer = 0;
function thumbsSoon() { clearTimeout(thumbTimer); thumbTimer = setTimeout(renderThumbs, 150); }
function thumbKey(h) { return JSON.stringify([h, mediaVersion, fontsArrived, live.version, live.raw]); }
function renderThumbs() {
  for (const [id, t] of thumbs) {
    const h = id === "global" ? doc.global : sceneById(id);
    if (!h) { thumbs.delete(id); continue; }
    const key = thumbKey(h);
    if (key === t.key) continue;
    t.key = key;
    if (thumbCv.width !== W || thumbCv.height !== H) { thumbCv.width = W; thumbCv.height = H; }
    renderScene(thumbCv.getContext("2d"), id === "global" ? null : h, 0, { withGlobal: id === "global" });
    const g = t.canvas.getContext("2d");
    g.imageSmoothingEnabled = true;
    g.imageSmoothingQuality = "high";
    g.clearRect(0, 0, t.canvas.width, t.canvas.height);
    g.drawImage(thumbCv, 0, 0, t.canvas.width, t.canvas.height);
  }
}

const sdnd = { id: null };
function showStrip() {
  const cards = [], current = sel.scene;
  const card = (id, name, h) => {
    const c = make("div", `scene${id === current ? " current" : ""}${id === "global" ? " global" : ""}`);
    c.dataset.scene = id;
    let t = thumbs.get(id);
    if (!t) { t = { canvas: make("canvas"), key: "" }; t.canvas.width = 320; t.canvas.height = 144; thumbs.set(id, t); }
    c.append(t.canvas, make("span", "sc-name", name));
    c.addEventListener("click", (e) => { if (!e.target.closest("button, input") && sel.scene !== id) showScene(id); });
    return c;
  };
  const g = card("global", "Global", doc.global);
  g.title = "Layers drawn over every scene";
  g.append(make("span", "sc-meta", "pinned"));
  cards.push(g);
  doc.scenes.forEach((s, i) => {
    const c = card(s.id, s.name || `scene ${i + 1}`, s);
    c.draggable = true;
    const meta = make("span", "sc-meta"), seconds = make("input");
    seconds.type = "number"; seconds.min = "1"; seconds.value = num(s.seconds, 30); seconds.title = "Seconds on screen";
    seconds.addEventListener("change", () => { const v = parseInt(seconds.value, 10); if (v >= 1) edit(() => { sceneById(s.id).seconds = v; }); });
    const btn = (label, title, fn) => { const b = make("button", "", label); b.title = title; b.addEventListener("click", fn); return b; };
    meta.append(seconds, "s",
      btn("⧉", "Duplicate this scene", () => edit(() => {
        const copy = structuredClone(sceneById(s.id)), ids = new Set(doc.scenes.map((x) => x.id));
        copy.id = uid("s", ids); copy.name = `${s.name || "scene"} copy`;
        doc.scenes.splice(doc.scenes.indexOf(sceneById(s.id)) + 1, 0, copy);
      })),
      btn("✕", "Delete this scene", () => edit(() => {
        const i = doc.scenes.indexOf(sceneById(s.id));
        doc.scenes.splice(i, 1);
        if (sel.scene === s.id) { const next = doc.scenes[Math.min(i, doc.scenes.length - 1)]; sel.scene = next ? next.id : "global"; sel.ids = []; lastScene = next?.id ?? null; }
      })));
    c.append(meta);
    c.addEventListener("dragstart", (e) => { sdnd.id = s.id; e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", s.id); });
    c.addEventListener("dragend", () => { sdnd.id = null; for (const el of $("scenes").querySelectorAll(".over-left, .over-right")) el.classList.remove("over-left", "over-right"); });
    c.addEventListener("dragover", (e) => {
      if (!sdnd.id || sdnd.id === s.id) return;
      e.preventDefault();
      const r = c.getBoundingClientRect(), left = e.clientX < r.left + r.width / 2;
      c.classList.toggle("over-left", left); c.classList.toggle("over-right", !left);
    });
    c.addEventListener("drop", (e) => {
      if (!sdnd.id || sdnd.id === s.id) return;
      e.preventDefault();
      const r = c.getBoundingClientRect(), left = e.clientX < r.left + r.width / 2;
      moveScene(sdnd.id, s.id, left ? "before" : "after");
    });
    cards.push(c);
  });
  $("scenes").replaceChildren(...cards, $("addScene"));  // the + sits after the last card
  $("sceneTitle").textContent = current === "global" ? "Global layers (over every scene)" : (holder()?.name || "");
  document.body.classList.toggle("loaded", !!holder() && layersOf().some((l) => (l.objects || []).length));
  thumbsSoon();
}
function moveScene(id, otherId, where) {
  edit(() => {
    const i = doc.scenes.findIndex((s) => s.id === id);
    const [s] = doc.scenes.splice(i, 1);
    const j = doc.scenes.findIndex((x) => x.id === otherId);
    doc.scenes.splice(where === "before" ? j : j + 1, 0, s);
  });
}
function addScene() {
  let id;
  edit(() => {
    id = uid("s", new Set(doc.scenes.map((s) => s.id)));
    doc.scenes.push({ id, name: `scene ${doc.scenes.length + 1}`, seconds: num(doc.seconds, 30), background: "#000000",
                      layers: [{ id: "l1", name: "Layer 1", visible: true, locked: false, opacity: 1, objects: [] }] });
  });
  showScene(id);
}
$("addScene").addEventListener("click", addScene);

// --- The player -------------------------------------------------------------------------------------

function showPlayer(state) {
  const waiting = (state.log || "").match(/(?:waiting for the panel|nothing to play yet): (.*)/);
  const el = $("playerState");
  el.className = `${waiting ? "warn" : state.running ? "info" : "muted"}`;
  el.textContent = !state.built ? "The player isn't built yet: run cargo build --release in the player folder."
    : waiting ? `The player is running but can't reach the cooler: ${waiting[1]}`
    : state.running ? "Playing on the cooler. Changes show up within a second."
    : "Not playing.";
  el.title = el.textContent;
  $("playerStart").disabled = state.running || !state.built;
  $("playerStop").disabled = !state.running;
  $("autostart").checked = !!state.autostart;
  $("autostart").disabled = !state.built;
  $("rivals").textContent = state.rivals?.length
    ? `Also set to start with Windows, and would take the cooler first: ${state.rivals.join(", ")}. `
      + "Turn it off there (Task Scheduler, or Task Manager's Startup apps)."
    : "";
}
async function player(action) {
  $(action === "start" ? "playerStart" : "playerStop").disabled = true;
  try {
    await flushSave();
    showPlayer(await postJson(`/api/player/${action}`, {}));
  } catch (err) {
    message(`Couldn't ${action} the player: ${err.message}`, "warn");
    showPlayer(await (await api("/api/player")).json());
  }
}
$("playerStart").addEventListener("click", () => player("start"));
$("playerStop").addEventListener("click", () => player("stop"));
$("autostart").addEventListener("change", async () => {
  try {
    showPlayer(await postJson("/api/autostart", { on: $("autostart").checked }));
  } catch (err) {
    message(`Couldn't change the startup setting: ${err.message}`, "warn");
  }
});
setInterval(async () => {
  try { showPlayer(await (await api("/api/player")).json()); } catch {}
}, 4000);

async function doExport() {
  const h = holder(), i = doc.scenes.indexOf(h);
  if (i < 0) return;
  const button = $("export"), out = $("exportResult");
  button.disabled = true;
  out.textContent = "Rendering…";
  try {
    await flushSave();
    const res = await postJson("/api/export", { scene: i, name: h.name || `scene${i + 1}` });
    out.textContent = `${res.files[0].name} saved in ${res.folder}.`;
  } catch (err) {
    out.textContent = "";
    message(`Export failed: ${err.message}`, "warn");
  } finally {
    button.disabled = false;
  }
}
$("export").addEventListener("click", doExport);
$("reveal").addEventListener("click", () => api("/api/reveal", { method: "POST" }));

// --- Live data -----------------------------------------------------------------------------------------
//
// Text can show live values, written as placeholders like {pc.cpu.load:d} (docs/data.md). Their sources are set up
// in the sources window (docs/sources.md) and saved to the playlist's data.json: what each source is, needs and
// provides comes from the player's catalog, and the values, and the formatting, from the player through the studio.
// Objects keep their placeholders: only the preview shows them filled in, and as written when the player isn't running.

const live = {
  config: null,         // data.json as the studio gives it: no passwords, `<key>_set` instead
  catalog: [],          // the player's catalog: manifests, each with its `status`
  pluginsDir: "",       // where plugins go
  player: null,         // whether the player answered last time (null: not asked yet)
  values: {},           // key -> {value, at, stale, source}
  resolved: new Map(),  // a text -> as the player would draw it now
  asked: "",            // the texts last sent to be filled in
  version: 0,           // changes whenever the preview's text does
  raw: false,           // show placeholders as written
  selected: null,       // the source shown in the window
  formKey: "",          // what the settings form was built for
  timer: 0, seq: 0, saveTimer: 0, saving: Promise.resolve(),
  clearedNote: "",      // the note last shown for a saved password the studio dropped
};
const SECRET_KEY = /(^|_)(token|password|secret|api_?key)$/i;  // a setting the page never gets back
// A password kept as typed, spaces at the ends included, as LHM compares it exactly (studio.py's AS_TYPED). Any
// other, a pasted token say, is trimmed. One that's all spaces is blank either way.
const secretValue = (id, key, value) => (id === "pc" && key === "lhm_password" ? value : value.trim());

/** Whether a text has a placeholder: `{key}` and the rest, not counting {{ and }}. */
const isLive = (t) => /\{[^{}]+\}/.test(String(t ?? "").replace(/\{\{|\}\}/g, ""));
function liveTexts() {
  const all = [];
  for (const h of [holder(), doc.global]) for (const l of layersOf(h)) for (const o of l.objects || []) if (o.type === "text" && isLive(o.text)) all.push(String(o.text));
  return [...new Set(all)];
}
/** The text an object shows in the preview: filled in with the current values, when the player could. */
function shownText(w) {
  const raw = String(w.text ?? "");
  return !live.raw && live.resolved.has(raw) ? live.resolved.get(raw) : raw;
}
function resolveSoon(ms = 300) { clearTimeout(live.timer); live.timer = setTimeout(resolveTexts, ms); }
async function resolveTexts() {
  clearTimeout(live.timer);
  const texts = liveTexts(), seq = ++live.seq;
  live.asked = texts.join("\n");
  if (!texts.length) return showLiveNote();
  let out;
  try { out = await postJson("/api/data/resolve", { texts }); } catch { out = {}; }
  if (seq !== live.seq) return;  // a newer ask went out meanwhile
  if (out.player === false) live.player = false;
  else if (Array.isArray(out.texts)) live.player = true;
  const resolved = new Map(Array.isArray(out.texts) ? texts.map((t, i) => [t, String(out.texts[i] ?? t)]) : []);
  if (JSON.stringify([...resolved]) !== JSON.stringify([...live.resolved])) {
    live.resolved = resolved;
    live.version++;
    redraw();
    thumbsSoon();
  }
  showLiveNote();
}
function showLiveNote() {
  $("dataNote").hidden = !(live.player === false && liveTexts().length);
  showDataStatus();
}

/** A value as the page shows it: near the player's default form. */
function formatValue(v) {
  if (typeof v === "number") return String(Math.round(v * 10) / 10);
  if (typeof v === "string" && /^\d{4}-\d\d-\d\dT\d\d:\d\d/.test(v) && !isNaN(Date.parse(v))) {
    const d = new Date(v), time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
    return d.toDateString() === new Date().toDateString() ? time : `${d.toLocaleDateString([], { weekday: "short" })} ${time}`;
  }
  return v === null || v === undefined ? "--" : typeof v === "object" ? JSON.stringify(v) : String(v);
}
function valueRow(key, entry) {
  const full = entry !== null && typeof entry === "object" && "value" in entry, v = full ? entry.value : entry;
  const b = make("button", `dv${full && entry.stale ? " stale" : ""}`);
  b.append(make("span", "k", key), make("span", "v", formatValue(v)));
  b.title = `Click to put {${key}} in the selected text` + (full ? `\nFrom ${entry.source || "?"}, read ${
    entry.at ? new Date(entry.at).toLocaleString() : "?"}${entry.stale ? ": stale, no update for a while" : ""}` : "");
  b.addEventListener("click", () => insertKeyOrCopy(key));
  return b;
}
async function refreshValues() {
  let out;
  try { out = await (await api("/api/data/values")).json(); } catch { out = { player: false }; }
  live.player = out.player !== false;
  live.values = live.player && !out.error ? out : {};
  showLiveNote();
  if ($("sourcesDlg").open) showSource();
  if (ac.open) acUpdate();
}
/** Puts {key} into the selected text where its cursor is: at the end when it hasn't been in it. */
function insertKey(key) {
  const w = primary(), el = $("wText");
  if (!w || w.type !== "text") return message("Select a text first: the value goes into it.", "warn");
  const text = String(w.text ?? ""), put = `{${key}}`;
  if (el.value !== text) el.value = text;
  const at = el.selectionStart, end = el.selectionEnd;
  const next = text.slice(0, at) + put + text.slice(end);
  edit(() => { find(w.id).obj.text = next; });
  if (el.value !== next) el.value = next;
  el.setSelectionRange(at + put.length, at + put.length);  // the next one goes after it
}
function insertKeyOrCopy(key) {
  const w = primary(), put = `{${key}}`;
  if (w?.type === "text") { insertKey(key); return srcNote(`${put} put into ${w.name || w.id}.`); }
  const copied = () => srcNote(`${put} copied: no text is selected to put it in.`);
  const failed = () => srcNote(`No text is selected: write ${put} in one.`, "warn");
  try { navigator.clipboard.writeText(put).then(copied, failed); } catch { failed(); }
}
function srcNote(text, cls = "") { $("srcMsg").textContent = text; $("srcMsg").className = `small ${cls}`; }

// --- The catalog and the sources window ------------------------------------------------

const manifestOf = (id) => live.catalog.find((m) => m.id === id) || null;
const sourceName = (id) => manifestOf(id)?.name || id || "?";
const settingsOf = (m) => (Array.isArray(m?.settings) ? m.settings : []).filter((s) => s && typeof s.key === "string");
const providesOf = (m) => (Array.isArray(m?.provides) ? m.provides : []).filter((p) => p && typeof p.key === "string");
const isSecret = (id, key) => settingsOf(manifestOf(id)).some((s) => s.key === key && s.type === "password") || SECRET_KEY.test(key);
const intervalOf = (m) => ({ default: Math.max(1, num(m?.interval?.default, 60)), min: Math.max(1, num(m?.interval?.min, 5)) });
function defaultsOf(m) {
  const d = { interval: intervalOf(m).default };
  for (const s of settingsOf(m)) {
    if (s.type !== "password") d[s.key] = s.default ?? ({ bool: false, list: [], number: 0 }[s.type] ?? "");
  }
  return d;
}
function sourceFor(type, add = true) {
  const list = live.config.sources;
  let s = list.find((x) => x && x.type === type);
  if (!s && add) list.push(s = { type, enabled: false, ...structuredClone(defaultsOf(manifestOf(type))) });
  return s;
}
const currentSource = (m) => ({ ...defaultsOf(m), ...(live.config && sourceFor(m.id, false)) });
function isOn(m) {
  const s = live.config && sourceFor(m.id, false);
  return s ? !!s.enabled : !!m.status?.enabled;
}
function dotFor(m) {
  const st = m.status || {};
  if (!isOn(m)) return ["off", "Off"];
  if (st.error) return ["err", `Error: ${st.error}`];
  if (st.ok || st.updated_at) return ["on", "On" + (st.updated_at ? `, updated ${new Date(st.updated_at).toLocaleTimeString()}` : "")];
  return ["wait", live.player === false ? "On, once the player runs" : "On: waiting for its first update"];
}
function showDataStatus() {
  const on = live.catalog.filter(isOn), errors = on.filter((m) => m.status?.error).length;
  $("dataStatus").textContent = !live.catalog.length ? "" : !on.length ? "No data sources on"
    : `${on.length} data source${on.length > 1 ? "s" : ""} on` + (errors ? `, ${errors} error${errors > 1 ? "s" : ""}` : "");
}
async function refreshCatalog() {
  let out;
  try { out = await (await api("/api/data/catalog")).json(); } catch { out = { player: false, sources: [] }; }
  live.catalog = (Array.isArray(out.sources) ? out.sources : []).filter((m) => m && typeof m.id === "string");
  live.pluginsDir = typeof out.plugins_dir === "string" ? out.plugins_dir : "";
  if (out.player === false) live.player = false;
  showDataStatus();
  if ($("sourcesDlg").open) showSources();
}
function openSources() {
  const d = $("sourcesDlg");
  if (!d.open) d.showModal();
  srcNote("");
  if (!manifestOf(live.selected)) live.selected = live.catalog[0]?.id ?? null;
  showSources();
  refreshCatalog();
  refreshValues();
}
function showSources() {
  if (!manifestOf(live.selected)) live.selected = live.catalog[0]?.id ?? null;
  const rows = live.catalog.map((m) => {
    const li = make("li", m.id === live.selected ? "selected" : "");
    li.dataset.id = m.id;
    const [cls, title] = dotFor(m), dot = make("span", `dot ${cls}`);
    dot.title = title;
    const sw = make("label", "switch"), cb = make("input");
    cb.type = "checkbox";
    cb.checked = isOn(m);
    cb.disabled = !live.config;
    cb.title = cb.checked ? "On: click to turn off" : "Off: click to turn on";
    cb.addEventListener("change", () => { sourceFor(m.id).enabled = cb.checked; saveDataSoon(); showSources(); });
    sw.addEventListener("click", (e) => e.stopPropagation());
    sw.append(cb, make("span", "slider"));
    li.append(dot, make("span", "name", m.name || m.id));
    if (m.plugin) li.append(make("span", "small muted", "plugin"));
    li.append(sw);
    li.addEventListener("click", () => { live.selected = m.id; showSources(); });
    return li;
  });
  if (!rows.length) rows.push(make("li", "empty small", live.player === false ? "The player isn't running: start it with Play on cooler." : "Asking the player…"));
  $("srcList").replaceChildren(...rows);
  $("srcHeadNote").textContent = live.player === false ? "The player isn't running: built-in sources only, and no live values."
    : !live.config ? "data.json couldn't be read." : "";
  $("pluginsDir").textContent = live.pluginsDir;
  showSource();
}
function showSource() {
  const m = manifestOf(live.selected), panel = $("srcPanel");
  if (!m) { panel.replaceChildren(); live.formKey = ""; return; }
  const key = JSON.stringify([m.id, m.settings, m.interval, m.provides]);
  if (key !== live.formKey) {
    live.formKey = key;
    buildSource(m);
  }
  const st = m.status || {}, [cls, title] = dotFor(m);
  const status = $("srcStatus");
  status.className = `small ${cls === "err" ? "warn" : "muted"}`;
  status.textContent = (st.error ? `Error: ${st.error}` : title) + (st.notes ? ` · ${st.notes}` : "");
  fillForm(m);
  showProvides(m);
}
function buildSource(m) {
  const panel = $("srcPanel"), head = make("div", "row");
  head.append(make("h3", "", m.name || m.id), make("span", "spacer"), Object.assign(make("span", "small muted"), { id: "srcStatus" }));
  const form = Object.assign(make("div"), { id: "srcForm" });
  for (const s of settingsOf(m)) form.append(settingRow(m, s));
  const iv = intervalOf(m), ivRow = make("div", "row setting"), ivIn = make("input");
  ivIn.type = "number"; ivIn.min = iv.min; ivIn.step = 1; ivIn.dataset.k = "interval";
  ivIn.title = `How often it's read: ${iv.default} s unless set, ${iv.min} s at least`;
  const ivBox = make("span");
  ivBox.append(ivIn, make("span", "small muted", ` s (default ${iv.default}, at least ${iv.min})`));
  ivRow.append(make("span", "label", "Read every"), ivBox);
  form.append(ivRow);
  ivIn.addEventListener("change", () => {
    const v = parseInt(ivIn.value, 10);
    if (!(v > 0) || !live.config) return;
    sourceFor(m.id).interval = Math.max(iv.min, v);
    saveDataSoon();
  });
  const test = Object.assign(make("button", "", "Test"), { id: "srcTest", title: "Read it once now, as set up here, without saving" });
  test.addEventListener("click", () => testSource(m));
  const testRow = make("div", "row");
  testRow.append(test);
  const out = Object.assign(make("div", "test-out small"), { id: "srcTestOut" });
  const table = Object.assign(make("table", "provides"), { id: "srcProvides" });
  panel.replaceChildren(head, make("div", "small muted", m.description || ""), form, testRow, out,
                        make("h4", "", "Provides"), make("div", "small muted", "Click a key to put it in the selected text."), table);
}
function settingRow(m, s) {
  const row = make("div", "row setting"), label = make("span", "label", s.label || s.key);
  let el;
  if (s.type === "bool") {
    el = make("input"); el.type = "checkbox";
    const wrap = make("label"); wrap.append(el, make("span", "", s.label || s.key));
    row.append(make("span", "label", ""), wrap);
  } else {
    if (s.type === "choice") {
      el = make("select");
      for (const c of Array.isArray(s.choices) ? s.choices : []) el.append(new Option(String(c), String(c)));
    } else if (s.type === "list") {
      el = make("textarea"); el.rows = 3; el.title = "One per line";
    } else {
      el = make("input");
      el.type = s.type === "number" ? "number" : s.type === "password" ? "password" : "text";
      if (s.type === "number") { el.step = "any"; if (s.min !== undefined) el.min = s.min; if (s.max !== undefined) el.max = s.max; }
      if (s.type === "password") el.autocomplete = "off";
    }
    if (typeof s.placeholder === "string") el.placeholder = s.placeholder;
    row.append(label, el);
  }
  el.spellcheck = false;
  el.dataset.k = s.key;
  el.dataset.type = s.type || "text";
  if (s.help) { el.title = s.help; row.append(make("div", "help small muted", s.help)); }
  const onInput = s.type === "text" || s.type === "list" || s.type === "number";
  el.addEventListener(onInput ? "input" : "change", () => {
    if (!live.config) return;
    const src = sourceFor(m.id);
    if (s.type === "bool") src[s.key] = el.checked;
    else if (s.type === "number") { const v = parseFloat(el.value); if (!Number.isFinite(v)) return; src[s.key] = v; }
    else if (s.type === "list") src[s.key] = el.value.split("\n").map((e) => e.trim()).filter(Boolean);
    else if (s.type === "password") {
      if (!el.value.trim()) return;
      src[s.key] = secretValue(m.id, s.key, el.value);
      if ($("srcMsg").textContent === live.clearedNote) srcNote("");  // typed again
    }
    else src[s.key] = el.value;
    saveDataSoon();
  });
  return row;
}
function fillForm(m) {
  const cur = currentSource(m), specs = Object.fromEntries(settingsOf(m).map((s) => [s.key, s]));
  for (const el of $("srcForm").querySelectorAll("[data-k]")) {
    const k = el.dataset.k, s = specs[k] || {};
    el.disabled = !live.config;
    if (el === document.activeElement) continue;
    if (s.type === "bool") el.checked = !!cur[k];
    else if (s.type === "list") el.value = (Array.isArray(cur[k]) ? cur[k] : [cur[k] ?? ""]).filter((e) => e !== "").join("\n");
    else if (s.type === "password") {
      el.value = "";
      el.placeholder = cur[`${k}_set`] ? "Set: type a new one to replace it" : s.placeholder || "Not set";
    } else el.value = cur[k] ?? "";
  }
}
const globRe = (pattern) => new RegExp("^" + pattern.split("*").map((p) => p.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$");
function showProvides(m) {
  const table = $("srcProvides"), provides = providesOf(m), explicit = new Set(provides.filter((p) => !p.pattern).map((p) => p.key));
  const header = make("tr");
  for (const h of ["Key", "Label", "Unit", "Value"]) header.append(make("th", "", h));
  const rows = [header], covered = new Set();
  const row = (key, label, unit, entry) => {
    const tr = make("tr", entry?.stale ? "stale" : ""), td = make("td"), b = make("button", "key", key);
    b.title = `Click to put {${key}} in the selected text` + (entry?.at ? `\nRead ${new Date(entry.at).toLocaleString()}${entry.stale ? ": stale" : ""}` : "");
    b.addEventListener("click", () => insertKeyOrCopy(key));
    td.append(b);
    const val = make("td", entry ? "val" : "none", entry ? formatValue(entry.value) : "–");
    tr.append(td, make("td", "", label || ""), make("td", "", unit || ""), val);
    return tr;
  };
  for (const p of provides) {
    if (!p.pattern) { covered.add(p.key); rows.push(row(p.key, p.label, p.unit, live.values[p.key])); continue; }
    const re = globRe(p.key), keys = Object.keys(live.values).filter((k) => re.test(k) && !explicit.has(k) && !covered.has(k)).sort();
    const tr = make("tr", "group"), td = make("td");
    td.colSpan = 4;
    td.append(make("code", "", p.key), ` ${p.label || ""}` + (keys.length ? "" : ": none yet"));
    rows.push(tr);
    tr.append(td);
    for (const k of keys) { covered.add(k); rows.push(row(k, "", p.unit, live.values[k])); }
  }
  const others = Object.keys(live.values).filter((k) => live.values[k]?.source === m.id && !covered.has(k)).sort();
  if (others.length) {
    const tr = make("tr", "group"), td = make("td", "", "Also");
    td.colSpan = 4;
    tr.append(td);
    rows.push(tr, ...others.map((k) => row(k, "", "", live.values[k])));
  }
  table.replaceChildren(...rows);
}
function saveDataSoon() { clearTimeout(live.saveTimer); live.saveTimer = setTimeout(saveData, 500); }
function saveData() {
  clearTimeout(live.saveTimer);
  live.saveTimer = 0;
  if (!live.config) return live.saving;
  const body = { sources: structuredClone(live.config.sources) };
  for (const s of live.config.sources) {
    if (!s) continue;
    for (const k of Object.keys(s)) if (k !== "type" && isSecret(s.type, k)) { delete s[k]; s[`${k}_set`] = true; }
  }
  live.saving = live.saving.then(() => postJson("/api/data/config", body, { keepalive: true }))
    .then((saved) => { takeSecretFlags(saved); showDataConfig(); },
          (err) => message(`Couldn't save the data sources: ${err.message}`, "warn"));
  return live.saving;
}
/** Whether each password is still set, from the saved config the studio answers with: it drops a saved one when a
 *  text setting it went with (an address, a user name) changes, which the page would otherwise go on showing as
 *  set. A password typed since stays as it is, for the next save. Says so when one was dropped. */
function takeSecretFlags(saved) {
  if (!live.config || !Array.isArray(saved?.sources)) return;
  const nth = (list, s) => list.filter((x) => x && x.type === s.type).indexOf(s);  // matched as studio.py does
  const cleared = [];
  for (const s of live.config.sources) {
    if (!s) continue;
    const stored = saved.sources.filter((x) => x && x.type === s.type)[nth(live.config.sources, s)];
    for (const [flag, on] of Object.entries(stored || {})) {
      const k = flag.slice(0, -4);
      if (!flag.endsWith("_set") || typeof on !== "boolean" || k in s) continue;
      if (s[flag] && !on) cleared.push(settingsOf(manifestOf(s.type)).find((x) => x.key === k)?.label || `${sourceName(s.type)}: ${k}`);
      s[flag] = on;
    }
  }
  if (cleared.length) {
    live.clearedNote = `${cleared.join(", ")}: cleared, as a setting it was saved with (an address, a user name) changed. Type it again.`;
    srcNote(live.clearedNote, "warn");
  }
}
window.addEventListener("pagehide", () => { if (live.saveTimer) saveData(); });
function showDataConfig() {
  showDataStatus();
  if ($("sourcesDlg").open) showSources();
}
async function loadDataConfig() {
  try {
    const res = await api("/api/data/config"), out = await res.json();
    if (!res.ok) throw new Error(out.error || res.statusText);
    live.config = { ...out, sources: Array.isArray(out.sources) ? out.sources : [] };
  } catch (err) {
    message(`Couldn't read the data sources: ${err.message}`, "warn");
  }
  showDataConfig();
}
async function testSource(m) {
  const out = $("srcTestOut"), button = $("srcTest");
  const show = (cls, ...content) => { out.className = `test-out small ${cls}`; out.replaceChildren(...content); };
  button.disabled = true;
  show("muted", "Testing…");
  try {
    const source = { ...currentSource(m), type: m.id, enabled: true };
    for (const el of $("srcForm").querySelectorAll("input[type=password]")) {
      if (el.value.trim()) source[el.dataset.k] = secretValue(m.id, el.dataset.k, el.value);
    }
    const res = await postJson("/api/data/test", { source });
    const values = res.values && typeof res.values === "object" ? res.values : {}, keys = Object.keys(values).sort();
    // parts the source skipped (Libre Hardware Monitor not answering, say), under the values
    const notes = (Array.isArray(res.notes) ? res.notes : []).map((note) => make("div", "warn", String(note)));
    if (res.player === false) show("warn", "The player isn't running: start it with Play on cooler to test.");
    else if (res.error) show("warn", String(res.error));
    else if (!keys.length) show("muted", "It worked, but gave no values.", ...notes);
    else {
      const list = make("div", "values");
      list.append(...keys.map((k) => valueRow(k, values[k])));
      show("", make("span", "info", `${keys.length} value${keys.length > 1 ? "s" : ""}, now:`), list, ...notes);
    }
  } catch (err) {
    show("warn", err.message);
  } finally {
    button.disabled = false;
  }
}
async function rescanPlugins() {
  const button = $("srcRescan");
  button.disabled = true;
  srcNote("Rescanning…", "muted");
  try {
    const out = await postJson("/api/data/catalog/rescan", {});
    if (out.player === false) srcNote("The player isn't running: it reads the plugins folder.", "warn");
    else if (out.error) srcNote(String(out.error), "warn");
    else {
      await refreshCatalog();
      const n = Number.isInteger(out.plugins) ? out.plugins : live.catalog.filter((m) => m.plugin).length;
      const problems = Array.isArray(out.problems) ? out.problems : [];
      srcNote(`Rescanned: ${n} plugin${n === 1 ? "" : "s"}.` + (problems.length ? ` Skipped: ${problems.join("; ")}` : ""),
              problems.length ? "warn" : "");
    }
  } catch (err) {
    srcNote(err.message, "warn");
  } finally {
    button.disabled = false;
  }
}
$("openSources").addEventListener("click", openSources);
$("openSources2").addEventListener("click", openSources);
$("srcClose").addEventListener("click", () => $("sourcesDlg").close());
$("srcRescan").addEventListener("click", rescanPlugins);
$("sourcesDlg").addEventListener("click", (e) => { if (e.target === e.currentTarget) e.currentTarget.close(); });
$("showRaw").addEventListener("change", () => { live.raw = $("showRaw").checked; live.version++; redraw(); thumbsSoon(); });
setInterval(() => { if (!document.hidden) { refreshValues(); resolveTexts(); refreshCatalog(); } }, 10000);

// --- Placeholder autocomplete ---------------------------------------------------------
//
// Typing { in the text box lists the keys the catalog documents and the live ones, filtered by what follows; after
// {key: it offers formats that fit the value. It never touches the text except when an entry is accepted.

const ac = { open: false, items: [], index: 0, mode: "key", from: 0, to: 0, mirror: null };
const FORMATS = { number: [["d", "whole number"], [".1f", "one decimal"]],
                  time: [["%H:%M", "hour and minute"], ["%a %H:%M", "weekday and time"], ["%d %b", "day and month"]] };
function acContext(el) {
  const text = el.value, caret = el.selectionStart;
  if (el.selectionEnd !== caret) return null;
  const open = text.lastIndexOf("{", caret - 1);
  if (open < 0) return null;
  let run = 0;
  for (let i = open; i >= 0 && text[i] === "{"; i--) run++;
  if (run % 2 === 0) return null;  // {{ is a literal brace
  const inside = text.slice(open + 1, caret);
  if (/[{}\n|]/.test(inside)) return null;
  const colon = inside.indexOf(":");
  if (colon < 0) return { mode: "key", query: inside, from: open + 1, to: caret };
  return { mode: "fmt", key: inside.slice(0, colon), query: inside.slice(colon + 1), from: open + colon + 2, to: caret };
}
function kindOf(key) {
  for (const m of live.catalog) {
    const p = providesOf(m).find((p) => p.pattern ? globRe(p.key).test(key) : p.key === key);
    if (p?.kind) return p.kind;
  }
  const v = live.values[key]?.value;
  return typeof v === "number" ? "number" : typeof v === "string" && /^\d{4}-\d\d-\d\dT\d\d:\d\d/.test(v) ? "time" : v === undefined ? null : "text";
}
function acKeys() {
  const seen = new Map();
  for (const m of live.catalog) {
    for (const p of providesOf(m)) if (!p.pattern) seen.set(p.key, { key: p.key, label: p.label || "", source: m.name || m.id });
  }
  for (const [k, e] of Object.entries(live.values)) {
    if (!seen.has(k)) {
      const m = live.catalog.find((m) => providesOf(m).some((p) => p.pattern && globRe(p.key).test(k))) || manifestOf(e?.source);
      const p = m && providesOf(m).find((p) => p.pattern && globRe(p.key).test(k));
      seen.set(k, { key: k, label: p?.label || "", source: m ? m.name || m.id : sourceName(e?.source) });
    }
    seen.get(k).entry = e;
  }
  return [...seen.values()];
}
function acUpdate() {
  const el = $("wText"), ctx = document.activeElement === el ? acContext(el) : null;
  if (!ctx) return acClose();
  const q = ctx.query.toLowerCase();
  let items;
  if (ctx.mode === "key") {
    const rank = (it) => it.key.toLowerCase().startsWith(q) ? 0 : it.key.toLowerCase().includes(q) ? 1 : it.label.toLowerCase().includes(q) ? 2 : 3;
    items = acKeys().map((it) => ({ ...it, rank: rank(it) })).filter((it) => it.rank < 3)
      .sort((a, b) => a.rank - b.rank || a.key.localeCompare(b.key))
      .map((it) => ({ insert: it.key, key: it.key, meta: [it.label, it.source].filter(Boolean).join(" · "),
                      value: it.entry ? formatValue(it.entry.value) + (it.entry.stale ? " (stale)" : "") : "" }));
  } else {
    const kind = kindOf(ctx.key), v = live.values[ctx.key]?.value;
    items = (FORMATS[kind] || []).filter(([f]) => f.toLowerCase().startsWith(q))
      .map(([f, what]) => ({ insert: f, key: f, meta: what, value: v === undefined ? "" : sampleFormat(v, f) }));
  }
  if (!items.length) return acClose();
  Object.assign(ac, { open: true, items, mode: ctx.mode, from: ctx.from, to: ctx.to, index: 0 });
  acRender();
}
function sampleFormat(v, f) {
  if (typeof v === "number") return f === "d" ? String(Math.round(v)) : f === ".1f" ? v.toFixed(1) : formatValue(v);
  const d = new Date(v);
  if (isNaN(d)) return formatValue(v);
  const two = (n) => String(n).padStart(2, "0");
  return f.replace(/%[HMadb]/g, (c) => ({ "%H": two(d.getHours()), "%M": two(d.getMinutes()), "%d": two(d.getDate()),
    "%a": d.toLocaleDateString([], { weekday: "short" }), "%b": d.toLocaleDateString([], { month: "short" }) })[c]);
}
function acRender() {
  const pop = $("acPop"), el = $("wText");
  pop.replaceChildren(...ac.items.map((it, i) => {
    const d = make("div", "ac-item");
    d.setAttribute("role", "option");
    d.setAttribute("aria-selected", i === ac.index);
    d.append(make("span", "ac-key", it.key), make("span", "ac-val", it.value));
    if (it.meta) d.append(make("span", "ac-meta", it.meta));
    d.addEventListener("mousedown", (e) => e.preventDefault());  // the text box keeps the focus
    d.addEventListener("click", () => acAccept(i));
    return d;
  }));
  pop.hidden = false;
  const c = caretXY(el), w = pop.offsetWidth, h = pop.offsetHeight;
  let top = c.y + c.h + 2;
  if (top + h > innerHeight - 8) top = Math.max(8, c.y - h - 2);
  pop.style.left = `${Math.max(8, Math.min(c.x, innerWidth - w - 8))}px`;
  pop.style.top = `${top}px`;
  pop.children[ac.index]?.scrollIntoView({ block: "nearest" });
}
function caretXY(el) {
  const mirror = ac.mirror ||= document.body.appendChild(make("div", "ac-mirror"));
  const cs = getComputedStyle(el);
  for (const p of ["fontFamily", "fontSize", "fontWeight", "fontStyle", "letterSpacing", "lineHeight", "tabSize", "textIndent",
                   "wordSpacing", "paddingTop", "paddingLeft", "paddingRight", "paddingBottom", "borderTopWidth",
                   "borderLeftWidth", "borderRightWidth", "borderBottomWidth", "boxSizing"]) mirror.style[p] = cs[p];
  mirror.style.width = `${el.clientWidth + parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth)}px`;
  mirror.textContent = el.value.slice(0, el.selectionStart);
  const mark = make("span", "", "\u200b");
  mirror.append(mark);
  const r = el.getBoundingClientRect();
  return { x: r.left + mark.offsetLeft - el.scrollLeft, y: r.top + mark.offsetTop - el.scrollTop,
           h: mark.offsetHeight || parseFloat(cs.lineHeight) || 16 };
}
function acClose() {
  if (!ac.open) return;
  ac.open = false;
  ac.items = [];
  $("acPop").hidden = true;
}
function acAccept(i = ac.index) {
  const it = ac.items[i], el = $("wText"), w = primary();
  if (!it) return acClose();
  const text = el.value, closed = text[ac.to] === "}", put = it.insert + (closed ? "" : "}");
  el.value = text.slice(0, ac.from) + put + text.slice(ac.to);
  const pos = ac.from + put.length + (closed ? 1 : 0);
  el.setSelectionRange(pos, pos);
  if (w?.type === "text") { const v = el.value; edit(() => { find(w.id).obj.text = v; }, "wText"); }
  acClose();
}
$("wText").addEventListener("input", acUpdate);
$("wText").addEventListener("click", () => { if (ac.open) acUpdate(); });
$("wText").addEventListener("blur", acClose);
$("wText").addEventListener("keydown", (e) => {
  if (!ac.open) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    ac.index = (ac.index + (e.key === "ArrowDown" ? 1 : ac.items.length - 1)) % ac.items.length;
    acRender();
  } else if (e.key === "Enter" || e.key === "Tab") {
    acAccept();
  } else if (e.key === "Escape") {
    acClose();
    e.stopPropagation();
  } else if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) {
    return setTimeout(acUpdate);
  } else return;
  e.preventDefault();
});
addEventListener("resize", () => { if (ac.open) acRender(); });

// --- Wiring the rest, and starting ----------------------------------------------------------------

for (const b of document.querySelectorAll("[data-tool]")) b.addEventListener("click", () => setTool(b.dataset.tool));
for (const b of document.querySelectorAll("[data-shape]")) b.addEventListener("click", () => { tool.shape = b.dataset.shape; $("shapeMenu").classList.remove("open"); setTool("shape"); });
$("file").addEventListener("change", () => { if ($("file").files[0]) uploadFile($("file").files[0]); $("file").value = ""; });
$("zoomIn").addEventListener("click", () => zoomStep(1));
$("zoomOut").addEventListener("click", () => zoomStep(-1));
$("zoomFit").addEventListener("click", () => setZoom("fit"));
$("zoom").addEventListener("change", () => setZoom($("zoom").value === "fit" ? "fit" : +$("zoom").value));
$("showGuides").addEventListener("change", () => { view.guides = $("showGuides").checked; redraw(); });
$("showGhost").addEventListener("change", () => { view.ghost = $("showGhost").checked; redraw(); });
$("pause").addEventListener("change", () => {
  view.pausedAt = clock();
  view.paused = $("pause").checked;
  for (const m of media.values()) if (m.video && m.frames[0]) { view.paused ? m.frames[0].pause() : m.frames[0].play().catch(() => {}); }
  redraw();
});
let resizeQueued = false;
window.addEventListener("resize", () => {
  if (!resizeQueued) { resizeQueued = true; requestAnimationFrame(() => { resizeQueued = false; layout(); }); }
});

// Animation: the preview is drawn again whenever any animated object's frame changes.
let lastFrames = "";
requestAnimationFrame(function tick() {
  if (!view.paused) {
    const t = clock(), parts = [];
    for (const h of [holder(), doc.global]) for (const l of layersOf(h)) {
      if (l.visible === false) continue;
      for (const o of l.objects || []) {
        if (o.visible === false || !isMedia(o)) continue;
        const m = media.get(o.src);
        if (!m?.ready || !m.info.animated) continue;
        parts.push(m.video ? m.frames[0].currentTime : frameIndex(m, t));
      }
    }
    const key = parts.join();
    if (key !== lastFrames) { lastFrames = key; draw(); }
  }
  requestAnimationFrame(tick);
});

(async () => {
  try {
    const cfg = await (await api("/api/config")).json();
    [W, H] = cfg.screen;
    SAFE = cfg.safe;
    CAMERA = cfg.camera;
    if (cfg.font) DEFAULT_FONT = cfg.font;
    screenCv.width = W;
    screenCv.height = H;
    setTool("select");
    layout();
    loadDataConfig();
    refreshCatalog();
    refreshValues();
    api("/api/fonts").then((res) => res.json()).then((list) => { fontList = list; fontsShown = ""; showProps(); })
      .catch((err) => message(`Couldn't list the fonts: ${err.message}`, "warn"));
    const res = await api("/api/playlist"), out = await res.json();
    if (!res.ok) throw new Error(out.error || res.statusText);
    doc = out;
    doc.global ||= { layers: [] };
    doc.scenes ||= [];
    const want = new URLSearchParams(location.hash.slice(1));  // #scene=<id>&sel=<object id>: open there
    showScene(want.get("scene") === "global" || sceneById(want.get("scene")) ? want.get("scene") : doc.scenes[0]?.id ?? "global");
    if (want.get("sel") && find(want.get("sel"))) select([want.get("sel")]);
    if (cfg.initial) {
      if (!doc.scenes.length) addScene();
      placeMedia(cfg.initial);
    }
    try { showPlayer(await (await api("/api/player")).json()); } catch {}
  } catch (err) {
    message(`Couldn't reach the studio server: ${err.message}`, "warn");
  }
})();
