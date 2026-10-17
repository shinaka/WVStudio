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

// The canvas being edited: the cooler's screen for a scene or Global, a widget's container while one is open
// (useCanvas). Renderers size and clip by W and H.
let W = 1600, H = 720, SAFE = { left: 0, right: 0, top: 0, bottom: 0 };  // replaced from the server
let CAMERA = null;  // the camera punch-hole, {x, y, r}: no pixels there
let DEFAULT_FONT = "C:\\Windows\\Fonts\\segoeui.ttf";
let PANEL = { w: 1600, h: 720, safe: { ...SAFE }, camera: null };  // the cooler's screen, from the server
let M = 140;  // off-screen margin shown around the screen, in screen px
const FRONT = () => ({ x: SAFE.left, y: SAFE.top, w: W - SAFE.left - SAFE.right, h: H - SAFE.top - SAFE.bottom });
const WHOLE = () => ({ x: 0, y: 0, w: W, h: H });

// Reusable widgets (docs/widgets.md) come from widgets.js (the model, `WV`), library.js and copies.js, loaded around
// this file. They're used only while `widgetsOn`: when the studio server has widgets too and all three scripts loaded
// (decided once the page has loaded). While it's false the page works exactly as without them, apart from showing
// copies as boxes, and nothing here calls into those files.
let widgetsOn = false;
const lib = new Map();      // widget id -> its widget.json content, exactly as on disk
const libMeta = new Map();  // id -> {stamp, gen, saved, resolvedFonts, ownFonts, readOnly}
let libProblems = [];       // [{folder, problem}]: folders in widgets\ that aren't widgets
let libDeleted = [];        // [{trash, id, name, when}]: Recently deleted
let libEpoch = 0;           // bumped whenever the library may have changed: expansions and walks follow it
const nav = [];             // [{key, ids}]: where ← Back returns to, last on top
const rnd = (v) => (widgetsOn ? WV.rnd(v) : Math.round(v));  // the one rounding rule (docs/widgets.md, Geometry)

/** Offers an event to the widget scripts first (`WV.hooks[kind]`: key, drop, dblclick, props), each function in
 *  turn. True once one returns true: it has handled the event, and the editor leaves it alone. Props hooks never
 *  stop the others: each fills its own rows. A hook that throws is logged and passed over. */
function offerHooks(kind, ...args) {
  if (!widgetsOn) return false;
  for (const fn of WV.hooks[kind]) {
    try {
      if (fn(...args) === true && kind !== "props") return true;
    } catch (err) {
      console.error(err);
    }
  }
  return false;
}

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
/** What's being edited: a scene, the global pseudo-scene, or an open widget's content. */
function holder() {
  if (isWidgetHolder()) return lib.get(holderWidgetId()) || null;
  return sel.scene === "global" ? doc.global : sceneById(sel.scene);
}
function isGlobal() { return sel.scene === "global"; }
/** Whether a widget is open in the editor (`sel.scene` is "widget:<id>"), and its id. Never while widgets are off.
 *  A scene id can't take that form: one that does (made by hand) gets a new id when the playlist is read. */
function isWidgetHolder(key = sel.scene) { return widgetsOn && typeof key === "string" && key.startsWith("widget:"); }
function holderWidgetId(key = sel.scene) { return isWidgetHolder(key) ? key.slice("widget:".length) : null; }
/** Whether a holder key still names something: a scene, Global, or a widget in the library. */
function holderExists(key) { return key === "global" || (isWidgetHolder(key) ? lib.has(holderWidgetId(key)) : !!sceneById(key)); }
/** An example (tracked in git) or a widget from a newer studio: it opens for viewing, and nothing changes it. */
function isReadOnly(id) {
  if (!widgetsOn || typeof id !== "string") return false;
  const w = lib.get(id);
  return id.startsWith("example-") || !!libMeta.get(id)?.readOnly || (typeof w?.version === "number" && w.version > 1);
}
/** A widget's name as the page shows it. */
function widgetName(id) { const n = lib.get(id)?.name; return typeof n === "string" && n.trim() ? n : id; }
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

// Media are keyed by a ref: `sources/<file>` (a scene's src as it is), `inputs/<file>` (a picture chosen for a copy),
// or `widgets/<id>/<path>` (a file in a widget's folder). Refs live only in the page: files keep the paths they're
// written with, and a widget's own objects say `media/…`, relative to its folder.
const media = new Map();  // ref -> {src, info, frames, video, ready, error, turned, scaled, total, starts}
let mediaVersion = 0;     // bumps when a file arrives or fails, so what depends on it is drawn again
const t0 = performance.now();
const clock = () => (view.paused ? view.pausedAt : performance.now() - t0);

/** The ref of a `src` written in a holder: itself in a scene or Global, under the widget's folder in a widget. */
function mediaRef(src, key = sel.scene) {
  return isWidgetHolder(key) && typeof src === "string" && src ? `widgets/${holderWidgetId(key)}/${src}` : src;
}
/** The media of an object in the open holder (not a leaf, whose src is a ref already). */
function holderMedia(o) { return getMedia(mediaRef(o.src)); }

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
  const ref = m.src, under = (prefix) => widgetsOn && ref.startsWith(prefix);
  const path = under("inputs/") ? `inputs/${encodeURIComponent(ref.slice(7))}`
    : under("widgets/") ? ref.split("/").map(encodeURIComponent).join("/")
    : encodeURIComponent(ref.split("/").pop());
  const res = await api(`/api/media/${path}`);
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
/** Whether a media object moves: a leaf (its src a ref), or with `key`, an object of that holder. */
const isAnimated = (o, key) => isMedia(o) && !!(media.get(key === undefined ? o.src : mediaRef(o.src, key))?.info?.animated);

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

/** Nearest neighbour exactly as docs/scenes.md has it: pixel u shows source pixel floor((2u + 1) * sw / (2 * w)), and
 *  the same for rows. (The canvas's own nearest can pick the neighbour where that lands right on a pixel's edge, as
 *  at 1.5x.) Drawn in runs, across then down or the other way round, whichever needs the smaller canvas between: each
 *  run of pixels that shows one source column (or row) is that column stretched, which copies it exactly. No pixels
 *  are read back, so a video can do it every frame. */
const nearestCv = document.createElement("canvas");
function nearest(g, src, dw, dh) {
  const sw = src.width, sh = src.height;
  g.imageSmoothingEnabled = false;
  if (sw === dw && sh === dh) {
    g.drawImage(src, 0, 0);
    return;
  }
  const runs = (n, size) => {  // [[source pixel, first pixel, count]]
    const out = [];
    for (let u = 0; u < n; u++) {
      const s = Math.floor((2 * u + 1) * size / (2 * n)), last = out[out.length - 1];
      if (last?.[0] === s) last[2]++; else out.push([s, u, 1]);
    }
    return out;
  };
  const across = dw * sh <= sw * dh, [mw, mh] = across ? [dw, sh] : [sw, dh];
  if (nearestCv.width !== mw || nearestCv.height !== mh) { nearestCv.width = mw; nearestCv.height = mh; }
  const mid = nearestCv.getContext("2d");
  mid.setTransform(1, 0, 0, 1, 0, 0);
  mid.clearRect(0, 0, mw, mh);
  mid.imageSmoothingEnabled = false;
  if (across) {
    for (const [s, u, n] of runs(dw, sw)) mid.drawImage(src, s, 0, 1, sh, u, 0, n, sh);
    for (const [s, v, n] of runs(dh, sh)) g.drawImage(nearestCv, 0, s, dw, 1, 0, v, dw, n);
  } else {
    for (const [s, v, n] of runs(dh, sh)) mid.drawImage(src, 0, s, sw, 1, 0, v, sw, n);
    for (const [s, u, n] of runs(dw, sw)) g.drawImage(nearestCv, s, 0, 1, dh, u, 0, n, dh);
  }
}

/** The frame scaled to the object's box: nearest neighbour unless smooth (the player's rule, pixel for pixel), once,
 *  from the file. With a tint, every pixel then takes the tint's colour and keeps its own alpha. */
function scaledFrame(m, o, i) {
  const dw = Math.max(1, rnd(o.w)), dh = Math.max(1, rnd(o.h));
  const tint = widgetsOn ? WV.color(o.tint) : null;
  const key = `${i}|${dw}|${dh}|${!!o.smooth}|${o.rotate | 0}|${!!o.flip}` + (tint ? `|${tint}` : "");
  let c = m.scaled.get(key);
  if (c && !m.video) return c;
  if (!c) { if (m.scaled.size > 300) m.scaled.clear(); c = document.createElement("canvas"); m.scaled.set(key, c); }
  if (c.width !== dw || c.height !== dh) { c.width = dw; c.height = dh; }
  const g = c.getContext("2d", tint ? { willReadFrequently: true } : undefined);
  g.clearRect(0, 0, dw, dh);
  if (widgetsOn && !o.smooth) nearest(g, turnedFrame(m, o, i), dw, dh);
  else {
    g.imageSmoothingEnabled = !!o.smooth;
    g.imageSmoothingQuality = "high";
    g.drawImage(turnedFrame(m, o, i), 0, 0, dw, dh);
  }
  if (tint) {
    const img = g.getImageData(0, 0, dw, dh), d = img.data, [r, gr, b] = rgb(tint);
    for (let k = 0; k < d.length; k += 4) { d[k] = r; d[k + 1] = gr; d[k + 2] = b; }
    g.putImageData(img, 0, 0);
  }
  return c;
}

// --- Expanding copies (docs/widgets.md, Drawing) -----------------------------------------------------
//
// Copies are drawn as the objects they expand into (WV.expandLayers): leaves in canvas pixels, each with its clip and
// the translucent groups it sits in. A holder's own objects come out as they are. Expansions are kept until the
// document, the library or the canvas changes (docEpoch, libEpoch, W and H).

let docEpoch = 0;  // bumped whenever the document or an open widget may have changed: edits, drags, undo, opening
const expansions = new WeakMap();  // a holder's layers -> {key, out}
const widgetViews = new Map();     // widget id -> {key, out}: the editor's 1× copy of it

/** A scene's or Global's layers as drawn: [{layer, leaves, marks}], copies expanded. */
function expanded(h) {
  const layers = layersOf(h);
  if (!widgetsOn) return layers.map((layer) => ({ layer, leaves: (layer.objects || []).filter((o) => o && o.visible !== false), marks: [] }));
  const key = `${docEpoch}|${libEpoch}|${W}x${H}`, hit = expansions.get(layers);
  if (hit?.key === key) return hit.out;
  const out = WV.expandLayers(layers, { lib, folder: "", canvas: [W, H], scope: null });
  expansions.set(layers, { key, out });
  return out;
}

/** A widget as its editor draws it: one 1× copy at the container's top-left, filled in with the preview values
 *  (`preview`: null for its defaults, or {holder, copy} naming the copy it was opened from). Its leaves' `top` is
 *  the widget's own object each came from, and its marks are its own copies that draw nothing. */
function widgetView(id, preview = id === holderWidgetId() ? view.preview : null) {
  if (!widgetsOn || !lib.has(id)) return [];
  let content = lib.get(id), def = WV.readDef(id, content);
  if (def.problem === "newer") { content = { ...content, version: 1 }; def = WV.readDef(id, content); }  // shown read-only
  if (def.problem) return [];
  const scope = previewScope(id, def, preview);
  const key = `${docEpoch}|${libEpoch}|${W}x${H}|${JSON.stringify([...scope.text, ...scope.color, ...scope.image])}`;
  const hit = widgetViews.get(id);
  if (hit?.key === key) return hit.out;
  // the copy takes the preview's values as they are, so it draws exactly what the copy they're from (or a copy left at
  // the defaults) draws
  const copy = { id, type: "widget", widget: id, visible: true, opacity: 1, x: 0, y: 0, scale: 1 };
  const holderLib = content === lib.get(id) ? lib : new Map([...lib, [id, content]]);
  const out = WV.expandLayers([{ id: "editor", visible: true, opacity: 1, objects: [copy] }],
                              { lib: holderLib, folder: "", canvas: [W, H], preview: { copy, scope }, markDepth: 1 });
  if (widgetViews.size > 200) widgetViews.clear();
  widgetViews.set(id, { key, out });
  return out;
}

/** The values a widget's editor previews (docs/widgets.md, The widget editor): its defaults; a copy's values, for a
 *  copy in a scene or Global; or, for a copy inside widget H, what it gets in a copy of H left at H's defaults. */
function previewScope(id, def, preview) {
  const copy = preview && findCopy(preview.holder, preview.copy);
  if (copy?.widget === id) {
    if (!isWidgetHolder(preview.holder)) return WV.scopeOf(def, copy.inputs, null, "");
    const hid = holderWidgetId(preview.holder), hdef = WV.readDef(hid, lib.get(hid));
    if (!hdef.problem) return WV.scopeOf(def, copy.inputs, WV.scopeOf(hdef, {}, null, ""), hdef.folder);
  }
  return WV.scopeOf(def, {}, null, "");
}
/** A copy in a holder (a scene id, "global" or "widget:<id>") by its id, or null. */
function findCopy(key, id) {
  const h = isWidgetHolder(key) ? lib.get(holderWidgetId(key)) : key === "global" ? doc.global : sceneById(key);
  const o = h ? find(id, h)?.obj : null;
  return o?.type === "widget" ? o : null;
}

/** What the canvas shows now: the open holder's layers expanded (and Global's), with the live placement. Kept while
 *  nothing it depends on changes: the animation tick asks for it every frame. */
let viewNow = { key: "", value: null };
function currentView() {
  const key = `${sel.scene}|${docEpoch}|${libEpoch}|${W}x${H}|${mediaVersion}|${JSON.stringify(view.preview)}`;
  if (viewNow.key === key) return viewNow.value;
  let own, over;
  if (isWidgetHolder()) [own, over] = [widgetView(holderWidgetId()), []];
  else [own, over] = [isGlobal() ? [] : expanded(holder()), expanded(doc.global)];
  const value = { own, over, place: widgetsOn ? WV.placement([own, over], (l) => isAnimated(l)) : null };
  viewNow = { key, value };
  return value;
}

/** A copy's scale, as both renderers read it: `scale`, else its stored width over the container's, clamped. */
function copyScale(o, def) {
  let s = o.scale;
  if (typeof s !== "number" || !(s > 0)) s = typeof o.w === "number" && o.w > 0 ? o.w / def.w : 1;
  return Math.min(16, Math.max(0.01, s));
}
/** A layer's objects that are objects. A file made by hand can hold anything there, which the player passes over. */
function objectsIn(l) {
  return l && typeof l === "object" && Array.isArray(l.objects) ? l.objects.filter((o) => o && typeof o === "object") : [];
}
/** Brings the stored w and h of a holder's copies up to date with their scale and their widgets' containers. Older
 *  tools and a missing widget's box use them; renderers don't. */
function refreshCopySizes(h) {
  for (const l of layersOf(h)) {
    for (const o of objectsIn(l)) {
      if (o.type !== "widget" || !WV.idOk(o.widget) || !lib.has(o.widget)) continue;
      const def = WV.readDef(o.widget, lib.get(o.widget));
      if (def.problem) continue;
      const s = copyScale(o, def), w = WV.rnd(s * def.w), hh = WV.rnd(s * def.h);
      if (o.w !== w || o.h !== hh) [o.w, o.h] = [w, hh];
    }
  }
}

/** The text inputs whose text runs past the widget's edge as drawn now (docs/widgets.md, A copy's properties: "cut
 *  off by the widget's edge"): for the copy `copyId` in holder `key`, or with no copy, for the open widget itself
 *  (its defaults, or what it previews). The container always clips, so a long value loses its end. */
function cutOffInputs(key = sel.scene, copyId = null) {
  const out = new Set(), view = isWidgetHolder(key) ? widgetView(holderWidgetId(key)) : expanded(key === "global" ? doc.global : sceneById(key));
  const copy = copyId === null ? null : findCopy(key, copyId);
  const wid = copy ? copy.widget : holderWidgetId(key), def = wid && lib.has(wid) ? WV.readDef(wid, lib.get(wid)) : null;
  if (!def || def.problem) return out;
  const texts = new Set(def.inputs.filter((i) => i.type === "text").map((i) => i.id));
  for (const e of view) {
    for (const l of e.leaves) {
      if (l.type !== "text" || !l.clip || (copy && l.top !== copy.id)) continue;
      const lay = textLayout(l);
      if (!lay) continue;
      textCtx.font = lay.font;  // each line's ink, not its line box, against the clip
      const past = lay.lines.some((line) => {
        if (!line.text) return false;
        const m = textCtx.measureText(line.text);
        return line.x - m.actualBoundingBoxLeft < l.clip[0] || line.x + m.actualBoundingBoxRight > l.clip[2]
          || line.baseline - m.actualBoundingBoxAscent < l.clip[1] || line.baseline + m.actualBoundingBoxDescent > l.clip[3];
      });
      if (!past) continue;
      const o = find(copy ? l.part : l.top, def)?.obj;  // the widget's text, or the copy inside it that made the leaf
      for (const v of o?.type === "text" ? [o.text] : Object.values(o?.inputs || {})) {
        if (typeof v === "string") WV.replaceRefs(v, (b) => { if (texts.has(b)) out.add(b); return null; });
      }
    }
  }
  return out;
}

/** A copy's box in its holder (docs/widgets.md, Geometry), or its stored box when its widget can't be drawn. */
function copyBox(o) {
  const def = WV.idOk(o.widget) && lib.has(o.widget) ? WV.readDef(o.widget, lib.get(o.widget)) : null;
  if (!def || def.problem) return { x: num(o.x, 0), y: num(o.y, 0), w: num(o.w, 0), h: num(o.h, 0) };
  const s = copyScale(o, def);
  return { x: WV.rnd(num(o.x, 0)), y: WV.rnd(num(o.y, 0)), w: Math.max(1, WV.rnd(s * def.w)), h: Math.max(1, WV.rnd(s * def.h)) };
}

// --- Drawing a scene (docs/scenes.md, Drawing a frame) -----------------------------------------

const pool = [];  // buffers, W x H: [0] for a layer below full opacity, [1…] for the groups open inside it
function layerBuffer(depth) {
  const c = pool[depth] ||= document.createElement("canvas");
  if (c.width !== W || c.height !== H) { c.width = W; c.height = H; }
  return c;
}

/** Draws a scene (null: only the global layers) at time t. With `dim`, the scene is darkened under the global
 *  layers, for editing them. Live text is drawn in its place, or on top of everything when something animated is
 *  above it (docs/widgets.md, Live text). */
function renderScene(ctx, scene, t, { withGlobal = true, dim = false } = {}) {
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = 1;
  ctx.fillStyle = scene ? hex(scene.background, "#000000") : "#000000";
  ctx.fillRect(0, 0, W, H);
  const own = scene ? expanded(scene) : [], over = withGlobal ? expanded(doc.global) : [];
  const place = widgetsOn ? WV.placement([own, over], (l) => isAnimated(l)) : null;
  for (const e of own) drawLayer(ctx, e, t, place);
  if (dim) { ctx.fillStyle = "rgba(0,0,0,.55)"; ctx.fillRect(0, 0, W, H); }
  for (const e of over) drawLayer(ctx, e, t, place);
  drawOnTop(ctx, place, t);
}

/** The widget editor's canvas: the widget drawn exactly as a 1× copy at its top-left (widgetView), on a cleared
 *  canvas, so the backdrop behind #screen shows where it's transparent. Cards draw it the same way, at defaults. */
function renderWidget(ctx, id, t, preview) {
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = 1;
  ctx.clearRect(0, 0, W, H);
  const out = preview === undefined ? widgetView(id) : widgetView(id, preview);
  const place = WV.placement([out, []], (l) => isAnimated(l));
  for (const e of out) drawLayer(ctx, e, t, place);
  drawOnTop(ctx, place, t);
}

/** Live text drawn on top of everything, when something animated is above it: in order, at its opacity times its
 *  layer's and groups', each inside its clip. */
function drawOnTop(ctx, place, t) {
  if (!place?.onTop) return;
  for (const l of place.live) drawClipped(ctx, l, t, place.fold.get(l) ?? 1);
}

/** A layer ({layer, leaves}, as expanded): its leaves together onto a buffer, blended at the layer's opacity (so they
 *  don't show through each other). At full opacity that's the same as drawing them straight on, which is cheaper.
 *  Live text that `place` puts on top waits for drawOnTop. */
function drawLayer(ctx, { layer, leaves }, t, place = null) {
  if (layer.visible === false) return;
  const op = clamp01(layer.opacity);
  if (op <= 0) return;
  const skip = place?.onTop ? place.fold : null;
  if (op >= 1) return drawLeaves(ctx, leaves, t, skip);
  const buf = layerBuffer(0), g = buf.getContext("2d");
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.globalAlpha = 1;
  g.clearRect(0, 0, W, H);
  drawLeaves(g, leaves, t, skip);
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = op;
  ctx.drawImage(buf, 0, 0);
  ctx.globalAlpha = 1;
}

/** Draws leaves in order, opening and closing their groups (docs/widgets.md, Opacity): a group's leaves go onto a
 *  buffer of their own, cleared over the group's rect, which is then laid over what holds it at the group's
 *  opacity. Leaves in `skip` are left out. */
function drawLeaves(ctx, leaves, t, skip = null) {
  const open = [];  // the groups open, outermost first: {id, opacity, rect, g}
  const under = () => (open.length ? open[open.length - 1].g : ctx);
  const close = () => {
    const { opacity, rect: [l, tp, r, b], g } = open.pop(), to = under();
    if (r <= l || b <= tp) return;
    to.setTransform(1, 0, 0, 1, 0, 0);
    to.globalAlpha = opacity;
    to.drawImage(g.canvas, l, tp, r - l, b - tp, l, tp, r - l, b - tp);
    to.globalAlpha = 1;
  };
  for (const leaf of leaves) {
    if (!leaf || leaf.visible === false) continue;
    const groups = leaf.groups || [];
    let same = 0;
    while (same < open.length && same < groups.length && open[same].id === groups[same].id) same++;
    while (open.length > same) close();
    while (open.length < groups.length) {
      const grp = groups[open.length], g = layerBuffer(1 + open.length).getContext("2d"), [l, tp, r, b] = grp.rect;
      g.setTransform(1, 0, 0, 1, 0, 0);
      g.globalAlpha = 1;
      if (r > l && b > tp) g.clearRect(l, tp, r - l, b - tp);
      open.push({ ...grp, g });
    }
    if (!skip?.has(leaf)) drawClipped(under(), leaf, t);
  }
  while (open.length) close();
}

/** A leaf inside its clip, a whole-pixel rect (a copy's box): the renderers' own transforms don't reset it. */
function drawClipped(ctx, o, t, fold = 1) {
  const c = o.clip;
  if (!c) return drawObject(ctx, o, t, null, fold);
  if (c[2] <= c[0] || c[3] <= c[1]) return;
  ctx.save();
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.beginPath();
  ctx.rect(c[0], c[1], c[2] - c[0], c[3] - c[1]);
  ctx.clip();
  drawObject(ctx, o, t, c, fold);
  ctx.restore();
}

/** One object at its opacity (times `fold`). `clip` limits what its fills cover: the extend, mirror and tile
 *  patterns of media, and a shape's pixels. */
function drawObject(ctx, o, t, clip = null, fold = 1) {
  const op = clamp01(o.opacity) * fold;
  if (op <= 0) return;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.globalAlpha = op;
  const area = clip ? [Math.max(0, clip[0]), Math.max(0, clip[1]), Math.min(W, clip[2]), Math.min(H, clip[3])] : [0, 0, W, H];
  if (isMedia(o)) drawMedia(ctx, o, t, area);
  else if (o.type === "text") drawText(ctx, o);
  else if (o.type === "shape") drawShape(ctx, o, area);
  ctx.globalAlpha = 1;
}

function drawMedia(ctx, o, t, area = [0, 0, W, H]) {
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
  let box = o;
  if (widgetsOn && o.keep_aspect === true) {  // the file fitted inside its box, keeping its shape: that box is drawn
    const [fw, fh] = Array.isArray(m.info.native) ? m.info.native : [m.info.width, m.info.height];
    const [x, y, w, h] = WV.fitAspect([rnd(o.x), rnd(o.y), Math.max(1, rnd(o.w)), Math.max(1, rnd(o.h))], o.rotate % 180 ? [fh, fw] : [fw, fh]);
    box = { ...o, x, y, w, h };
  }
  const img = scaledFrame(m, box, frameIndex(m, t));
  paint(ctx, img, rnd(box.x), rnd(box.y), o.fill || "none", area);
}

/** Draws a scaled frame at x, y, repeated outward over the fill `area` ([left, top, right, bottom]: the canvas, or a
 *  copy's clip inside it) for the extend, mirror and tile fills. The pattern stays anchored to x, y. */
function paint(ctx, img, x, y, fill, area = [0, 0, W, H]) {
  const dw = img.width, dh = img.height, [ax, ay, ar, ab] = area;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.imageSmoothingEnabled = false;
  if (fill === "tile" || fill === "mirror") {
    const mirror = fill === "mirror";
    if (((ar - ax) / dw + 2) * ((ab - ay) / dh + 2) > 300) {
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
      ctx.fillRect(ax, ay, ar - ax, ab - ay);
      return;
    }
    for (let j = Math.floor((ay - y) / dh); y + j * dh < ab; j++) {
      for (let i = Math.floor((ax - x) / dw); x + i * dw < ar; i++) {
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
  strip(0, 0, 1, dh, ax, y, x - ax, dh);              // left edge column, stretched
  strip(dw - 1, 0, 1, dh, r, y, ar - r, dh);          // right
  strip(0, 0, dw, 1, x, ay, dw, y - ay);              // top row
  strip(0, dh - 1, dw, 1, x, b, dw, ab - b);          // bottom
  strip(0, 0, 1, 1, ax, ay, x - ax, y - ay);          // corner pixels fill the corners
  strip(dw - 1, 0, 1, 1, r, ay, ar - r, y - ay);
  strip(0, dh - 1, 1, 1, ax, b, x - ax, ab - b);
  strip(dw - 1, dh - 1, 1, 1, r, b, ar - r, ab - b);
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

/** Draws a shape, painting only within `area` ([left, top, right, bottom]: the canvas, or a copy's clip in it). */
function drawShape(ctx, o, area = [0, 0, W, H]) {
  const stroke = o.stroke && num(o.stroke.width, 0) > 0 ? { color: hex(o.stroke.color, "#ffffff"), width: num(o.stroke.width, 1) } : null;
  const fill = o.shape === "line" || !o.fill || o.fill.type === "none" ? null : o.fill;
  if (!fill && !stroke) return;
  const box = shapeBox(o), s = stroke ? stroke.width / 2 : 0;
  const x0 = Math.max(area[0], Math.floor(box.x - s - 1)), y0 = Math.max(area[1], Math.floor(box.y - s - 1));
  const x1 = Math.min(area[2], Math.ceil(box.x + box.w + s + 1)), y1 = Math.min(area[3], Math.ceil(box.y + box.h + s + 1));
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

// --- Text (docs/text.md), laid out as the player does -------------------------------------------

const MAX_TEXT_SIZE = 2000, MAX_OUTLINE = 500;  // the player's limits (text.rs), on the scaled values in a copy
let fontList = [];       // [{path, family, style}]: the fonts the picker offers
const fonts = new Map(); // font path (lower case) -> {family, state}: a FontFace per file, loaded when first used
let fontsArrived = 0;    // counts loads finished, so the text is drawn again
const textCtx = document.createElement("canvas").getContext("2d");  // for measuring
// one text at a time: outline (or everything), and the fill on its own for crisp text
const glyphs = [0, 1].map(() => document.createElement("canvas").getContext("2d", { willReadFrequently: true }));

/** A font file as a CSS family, loaded from the studio the first time it's asked for. `path` is a scene text's font
 *  (a path), or a widget text's font ref, "<widget id>|<file name>", which the studio looks up for that widget
 *  (docs/widgets.md, Fonts). Those are kept by the file the lookup found, so one installed or bundled later loads. */
function fontFace(path) {
  let key = path.toLowerCase(), url = `/api/font?path=${encodeURIComponent(path)}`;  // Windows paths
  const bar = widgetsOn ? path.indexOf("|") : -1;
  if (bar > 0) {
    const id = path.slice(0, bar), name = path.slice(bar + 1), found = libMeta.get(id)?.resolvedFonts?.[name];
    if (found && !found.path) return { state: "failed" };  // not on this PC: drawn in Segoe UI
    key = found ? `${String(found.path).toLowerCase()}|${found.stamp ?? ""}` : `${id}|${name}|?`;
    url = `/api/font?widget=${encodeURIComponent(id)}&name=${encodeURIComponent(name)}`;
  }
  let f = fonts.get(key);
  if (!f) {
    f = { family: `wv-font-${fonts.size}`, state: "loading" };
    fonts.set(key, f);
    const face = new FontFace(f.family, `url("${url}")`);
    face.load().then(() => { document.fonts.add(face); f.state = "ready"; }, () => { f.state = "failed"; })
      .finally(() => { fontsArrived++; redraw(); thumbsSoon(); });
  }
  return f;
}

/** A text's font family; Segoe UI when its own can't be read, as in the player. Null while it loads, or when
 *  neither font will do (then the text isn't drawn). */
function textFamily(w) {
  for (const path of [typeof w.font === "string" && w.font ? w.font : DEFAULT_FONT, DEFAULT_FONT]) {
    const f = fontFace(path);
    if (f.state !== "failed") return f.state === "ready" ? f.family : null;
  }
  return null;
}

/** A text object laid out (docs/text.md, Geometry): each line's text, start, width and baseline, the block's
 *  box, and the font's ascent and descent. Null for other kinds of object, and until the font has loaded. */
function textLayout(w) {
  const family = w.type === "text" && textFamily(w);
  if (!family) return null;
  const cap = widgetsOn ? MAX_TEXT_SIZE : Infinity;  // with widgets on, the player's limit: a copy scales text up to 16x
  const g = textCtx, size = Math.max(1, Math.min(cap, num(w.size, 48))), step = size * num(w.line_height, 1.2);
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
function drawText(out, w) {
  if (widgetsOn && !(num(w.size, 48) > 0)) return;  // the player draws nothing at size 0 or less
  const lay = textLayout(w);
  if (!lay) return;
  const o = Math.max(0, Math.min(widgetsOn ? MAX_OUTLINE : Infinity, num(w.outline?.width, 0)));
  const colour = hex(w.color, "#ffffff"), outline = hex(w.outline?.color, "#000000");
  const pad = Math.ceil(o + lay.ascent + lay.descent);  // glyphs can overhang
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

/** A text of the open widget as its editor draws it, its inputs filled in (the leaf it became), so its box fits
 *  what's drawn. Anything else as it is. */
function shown(o) {
  if (!isWidgetHolder() || o?.type !== "text") return o;
  for (const e of widgetView(holderWidgetId())) for (const l of e.leaves) if (l.top === o.id && l.type === "text") return l;
  return o;
}

/** An object's box on screen: the media or shape box (a line's bounding box), the text block, or a copy's box. */
function objBox(o) {
  if (o.type === "text") return textLayout(shown(o))?.box || { x: num(o.x, 0), y: num(o.y, 0), w: 0, h: 0 };
  if (o.type === "shape") return shapeBox(o);
  if (o.type === "widget" && widgetsOn) return copyBox(o);
  return { x: o.x, y: o.y, w: o.w, h: o.h };
}

/** The handles of the primary object: four corners, or a line's two ends. [[x, y, kind], ...] */
function handles(o) {
  if (!o || o.type === "text") return [];
  if (o.type === "shape" && o.shape === "line") return [[o.x, o.y, "a"], [o.x + o.w, o.y + o.h, "b"]];
  if (o.type === "widget" && widgetsOn) {
    const b = copyBox(o);
    return [[b.x, b.y, 0], [b.x + b.w, b.y, 1], [b.x, b.y + b.h, 2], [b.x + b.w, b.y + b.h, 3]];
  }
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
    const lay = textLayout(shown(o));
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
  const t = clock();
  if (isWidgetHolder()) renderWidget(sctx, holderWidgetId(), t);
  else renderScene(sctx, isGlobal() ? sceneById(lastScene) : holder(), t, { dim: isGlobal() });
  drawGhost(t);
  drawOverlay();
}

/** The selected object faintly where it hangs off the screen (the margin around the canvas). */
function drawGhost(t) {
  const g = ghostCv.getContext("2d"), q = ghostCv.width / (W + 2 * M), o = primary();
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.clearRect(0, 0, ghostCv.width, ghostCv.height);
  if (!view.ghost || !o || o.type === "text" || o.type === "widget") return;
  g.setTransform(q, 0, 0, q, M * q, M * q);
  g.globalAlpha = 0.3;
  if (isMedia(o)) {
    const m = holderMedia(o);
    if (!m?.ready) return;
    g.imageSmoothingEnabled = q < 1 || !!o.smooth;
    g.drawImage(scaledFrame(m, o, frameIndex(m, t)), rnd(o.x), rnd(o.y));
  } else if (o.type === "shape") {
    drawShape(g, { ...o, crisp: false });
  }
  g.globalAlpha = 1;
}

/** The copies the open holder can't draw, for the overlay: [{x, y, w, h, label}] at their stored boxes. With widgets
 *  off, every copy, as the newer page's server can't draw them. */
function overlayMarks() {
  const LABELS = { missing: "Missing widget: ", newer: "Made by a newer studio: ", cycle: "contains itself",
                   deep: "more than 8 deep", budget: "too many objects" };
  if (!widgetsOn) {
    return layersOf().filter((l) => l.visible !== false).flatMap((l) => (l.objects || []).filter((o) => o?.type === "widget" && o.visible !== false))
      .map((o) => ({ x: num(o.x, 0), y: num(o.y, 0), w: num(o.w, 0), h: num(o.h, 0), label: "Widget: restart the studio to see it" }));
  }
  const { own, over } = currentView();
  return (isGlobal() ? over : own).flatMap((e) => e.marks)
    .map((m) => ({ ...m, label: m.kind === "missing" || m.kind === "newer" ? LABELS[m.kind] + m.widget : LABELS[m.kind] }));
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
  for (const m of overlayMarks()) {  // copies drawn as nothing: a red dashed box with the reason
    g.strokeStyle = "rgba(255,90,70,.95)";
    g.setLineDash([5 * css, 4 * css]);
    g.strokeRect(m.x, m.y, Math.max(m.w, css), Math.max(m.h, css));
    g.setLineDash([]);
    g.font = `${12 * css}px system-ui, "Segoe UI", sans-serif`;
    g.fillStyle = "rgba(255,90,70,.95)";
    g.textAlign = "left";
    g.textBaseline = "top";
    g.fillText(m.label, m.x + 4 * css, m.y + 3 * css);
  }
  if (view.guides && !isWidgetHolder()) {  // the panel's guides: a widget's container has none
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
  view.k = view.zoom === "fit"  // at most 8: a tiny widget isn't enormous
    ? Math.max(0.05, Math.min(8, (wrap.clientWidth - 24) / fw, (wrap.clientHeight - 24) / fh)) : +view.zoom;
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

/** A widget's container as "<w>x<h>", read as the player reads it (one from a newer studio too). */
function containerSize(id) {
  const def = WV.readDef(id, { ...lib.get(id), version: 1 });
  return `${def.w}x${def.h}`;
}
/** Makes the canvas the open holder's: the panel, its safe bands and camera for a scene or Global; a widget's
 *  container, with no guides and a margin to suit, while one is open. */
function useCanvas() {
  const id = holderWidgetId(), def = id !== null ? WV.readDef(id, { ...lib.get(id), version: 1 }) : null;
  const was = `${W}x${H}x${M}`;
  if (def) {
    [W, H, SAFE, CAMERA] = [def.w, def.h, { left: 0, right: 0, top: 0, bottom: 0 }, null];
    M = Math.max(16, WV.rnd(Math.max(W, H) / 10));
  } else {
    [W, H, SAFE, CAMERA, M] = [PANEL.w, PANEL.h, { ...PANEL.safe }, PANEL.camera, 140];
  }
  const locked = !!def && isReadOnly(id);
  document.body.classList.toggle("widget", !!def);
  document.body.classList.toggle("readonly", locked);
  for (const b of document.querySelectorAll("#tools [data-tool]")) {  // a read-only widget is for viewing
    if (b.dataset.tool !== "select" && b.dataset.tool !== "hand") b.disabled = locked;
  }
  // the media fit buttons fit the container: Fit inside and Fill (no flat front, no curves)
  $("fitFront").textContent = def ? "Fit inside" : "Fit flat front";
  $("fitFront").title = def ? "Largest size that fits inside the widget" : "Largest size that fits on the flat front";
  $("fillScreen").textContent = def ? "Fill" : "Fill screen";
  $("fillScreen").title = def ? "Cover the whole widget, cropping what's left over" : "Cover the whole screen, cropping what's left over";
  $("fitScreen").hidden = $("fillHeight").hidden = !!def;
  if (screenCv.width !== W || screenCv.height !== H) { screenCv.width = W; screenCv.height = H; }
  if (`${W}x${H}x${M}` !== was) layout();  // a scene after a scene keeps its zoom
}

/** Runs `fn` (one synchronous draw) with the canvas `w` x `h` and no guides: a scene's thumbnail while a widget is
 *  open, a widget's while a scene is. Every renderer sizes and clips by W and H. */
function withCanvasSize(w, h, fn) {
  const was = [W, H, SAFE];
  [W, H, SAFE] = [w, h, { left: 0, right: 0, top: 0, bottom: 0 }];
  try { return fn(); } finally { [W, H, SAFE] = was; }
}

/** What shows behind a transparent widget in its editor (only there): "checks", "black" or "white". Kept per
 *  browser. */
function setBackdrop(kind) {
  kind = ["checks", "black", "white"].includes(kind) ? kind : "checks";
  for (const k of ["checks", "black", "white"]) document.body.classList.toggle(`backdrop-${k}`, k === kind);
  try { localStorage.setItem("wv.backdrop", kind); } catch {}
  return kind;
}
function backdrop() {
  try { return localStorage.getItem("wv.backdrop") || "checks"; } catch { return "checks"; }
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
//
// With widgets on, one history covers the playlist and every widget that can be edited (docs/widgets.md, Undo): a
// state holds both, with each widget's `gen`. Each entry remembers the holder its step was made in (`at`), which
// undo and redo open, and the media refs its state names (`refs`), which the server is asked to keep while the page
// could still undo back to them. Library actions are explicit: the server moves a folder first, then the page
// changes; only Delete is a step (`action`), undone and redone with the server again.

const hist = { undo: [], redo: [], key: "", at: 0 };  // entries: {state, at, refs, action}
const stateRefs = new Map();  // the last few states made -> the media refs they name
function snapshot() {
  if (!widgetsOn) return JSON.stringify(doc);
  const docJson = JSON.stringify(doc), parts = [], gens = {};
  const refs = new Set([...docJson.matchAll(/"((?:sources|inputs)\/[^"\\]+)"/g)].map((m) => m[1]));
  for (const [id, w] of lib) {
    if (isReadOnly(id)) continue;  // nothing changes them
    const json = JSON.stringify(w);
    parts.push(`${JSON.stringify(id)}:${json}`);
    gens[id] = libMeta.get(id)?.gen ?? 0;
    for (const m of json.matchAll(/"(media\/[^"\\]+)"/g)) refs.add(`widgets/${id}/${m[1]}`);
  }
  const state = `{"doc":${docJson},"widgets":{${parts.join(",")}},"gens":${JSON.stringify(gens)}}`;
  stateRefs.set(state, [...refs]);
  while (stateRefs.size > 8) stateRefs.delete(stateRefs.keys().next().value);
  return state;
}
/** The media refs a history entry's state names: `sources/…` and `inputs/…` in its playlist, and each widget's
 *  `media/…` as `widgets/<id>/media/…`. */
function entryRefs(e) {
  if (e.refs) return e.refs;
  if (stateRefs.has(e.state)) return (e.refs = stateRefs.get(e.state));
  const refs = new Set();
  try {
    const s = JSON.parse(e.state);
    if (s.widgets) {
      for (const m of JSON.stringify(s.doc).matchAll(/"((?:sources|inputs)\/[^"\\]+)"/g)) refs.add(m[1]);
      for (const [id, w] of Object.entries(s.widgets)) for (const m of JSON.stringify(w).matchAll(/"(media\/[^"\\]+)"/g)) refs.add(`widgets/${id}/${m[1]}`);
    }
  } catch {}
  return (e.refs = [...refs]);
}
function pushUndo(before, key = "", action = null) {
  const now = Date.now(), at = sel.scene;
  if (key && widgetsOn) key += `|${at}`;  // a nudge in a widget and one in a scene are two steps
  if (!action && key && key === hist.key && now - hist.at < 1000 && hist.undo.length) { hist.at = now; return; }  // typing: one step
  hist.undo.push({ state: before, at, refs: null, action });
  if (hist.undo.length > 200) hist.undo.shift();
  hist.redo.length = 0;
  hist.key = key;
  hist.at = now;
}

/** Applies `fn` to the document as one undo step (`key` merges quick repeats, like typing), then refreshes. Refused
 *  (nothing happens) when it would change a read-only widget. */
function edit(fn, key = "") {
  const locked = widgetsOn ? [...lib.keys()].filter(isReadOnly).map((id) => [id, JSON.stringify(lib.get(id))]) : [];
  const before = snapshot();
  if (fn() === false) return false;  // the change won't do (a bad value typed): nothing happened
  if (locked.some(([id, json]) => JSON.stringify(lib.get(id)) !== json)) {
    putBack(before);
    for (const [id, json] of locked) lib.set(id, JSON.parse(json));
    message(`${widgetName(holderWidgetId() ?? locked[0][0])} is read-only: duplicate it to edit.`, "warn");
    changed({ save: false });
    return false;
  }
  // an edit of a widget brings the stored sizes of the copies in it up to date (only then: see tidyDocument)
  if (isWidgetHolder() && snapshot() !== before) refreshCopySizes(holder());
  const after = snapshot();
  if (after === before) return false;
  pushUndo(before, key);
  changed();
  return true;
}

/** Puts a state's document back, and each widget it holds that the library still has with the same `gen` (one
 *  replaced from outside the history since, or made since, is left as it is). */
function putBack(state) {
  const s = JSON.parse(state);
  if (!widgetsOn) { doc = s; return; }
  doc = s.doc;
  for (const [id, content] of Object.entries(s.widgets || {})) {
    if (lib.has(id) && !isReadOnly(id) && (libMeta.get(id)?.gen ?? 0) === (s.gens?.[id] ?? 0)) lib.set(id, content);
  }
  libEpoch++;
}
/** Puts a state back and opens `at`, the holder its step was made in (if it's gone: where ← Back goes, else the
 *  first scene). */
function restore(state, at = sel.scene) {
  putBack(state);
  if (!widgetsOn) {
    if (sel.scene !== "global" && !sceneById(sel.scene)) sel.scene = doc.scenes[0]?.id ?? "global";
  } else {
    const was = sel.scene, size = `${W}x${H}`;
    dropStaleNav();
    const key = holderExists(at) ? at : nav.length ? nav[nav.length - 1].key : doc.scenes[0]?.id ?? "global";
    if (key !== was) {
      sel.scene = key;
      sel.ids = [];
      sel.layer = null;
      if (key !== "global" && !isWidgetHolder(key)) lastScene = key;
    }
    if (key !== was || (isWidgetHolder() && containerSize(holderWidgetId()) !== size)) useCanvas();
  }
  sel.ids = sel.ids.filter((id) => find(id));
  if (sel.layer && !layerById(sel.layer)) sel.layer = null;
  changed();
}
async function undo() {
  const e = hist.undo.pop();
  if (!e) return;
  hist.key = "";
  hist.redo.push({ state: snapshot(), at: e.at, refs: null, action: e.action });
  restore(e.state, e.at);
  if (e.action?.action === "delete") await restoreFolder(e.action);  // the copies are back: now the widget
}
async function redo() {
  const e = hist.redo[hist.redo.length - 1];
  if (!e) return;
  hist.key = "";
  if (e.action?.action === "delete" && !(await trashFolder(e.action))) return;  // the folder first, then the page
  hist.redo.pop();
  hist.undo.push({ state: snapshot(), at: e.at, refs: null, action: e.action });
  restore(e.state, e.at);
}

/** After the document changed: draw it, refresh the panels and save. */
function changed({ save = true } = {}) {
  docEpoch++;
  redraw();
  showLayers();
  showProps();
  showStrip();
  thumbsSoon();
  if (liveTexts().join("\n") !== live.asked) resolveSoon();
  if (save) saveSoon();
}

// Saving: each widget and the playlist is its own file, saved when it differs from what was last saved (a widget's
// `saved`, the playlist's `saving.doc`), widgets first, so the player never reads a playlist naming a widget whose
// file isn't there yet. One flush at a time, in order. What each file has on its way is kept (`sending`), so a page
// going away can send the rest at once.
const saving = { timer: 0, last: Promise.resolve(), pending: false, doc: null, sending: null, held: "", heldAt: 0, holding: null };
function saveSoon(ms = 400) { saving.pending = true; clearTimeout(saving.timer); saving.timer = setTimeout(flushSave, ms); }
/** Saves now, after the one before it, so saves arrive in order. */
function flushSave() {
  clearTimeout(saving.timer);
  if (!saving.pending) return saving.last;
  saving.pending = false;
  if (widgetsOn) return afterSaves(saveAll);
  const body = snapshot();
  return (saving.last = saving.last.then(() => api("/api/playlist", { method: "POST", headers: { "Content-Type": "application/json" },
                                                                    body, keepalive: true }).then(async (res) => {
    if (!res.ok) throw new Error((await res.json()).error || res.statusText);
  })).catch((err) => message(`Couldn't save the playlist: ${err.message}`, "warn")));
}
window.addEventListener("pagehide", () => (widgetsOn ? saveNow() : flushSave()));
/** Runs `fn` after the saves (and reads) before it, so they happen in order. Whatever goes wrong in one, the next
 *  still runs. */
function afterSaves(fn) {
  return (saving.last = saving.last.then(fn).catch((err) => {
    console.error(err);
    message(`Couldn't save: ${err.message}`, "warn");
  }));
}

/** A flush: the hold, then each widget that needs saving, then the playlist, each after the answer to the one before. */
async function saveAll() {
  await sendHold();  // first, so no tidy a save sets off takes a file the history still needs
  for (const id of [...lib.keys()]) {
    await libMeta.get(id)?.sending?.done;  // sent as the page went away, and the page came back: its answer first
    await sendWidget(id);
  }
  await saving.sending?.done;
  await sendPlaylist();
}

/** As the page goes away (pagehide): everything not saved yet is sent at once, with nothing waiting for an answer in
 *  between, since the page may be gone before one comes. That includes what a flush still running hasn't sent yet. A
 *  file already on its way with older content goes after it: a widget's save needs the stamp that one gets back. */
function saveNow() {
  clearTimeout(saving.timer);
  saving.pending = false;
  const budget = { left: 60 * 1024 }, sends = [sendHold(budget)];
  for (const id of [...lib.keys()]) {
    if (widgetToSave(id) === null) continue;
    const before = libMeta.get(id).sending;
    sends.push(before ? before.done.then(() => sendWidget(id)) : sendWidget(id, budget));
  }
  if (playlistToSave() !== null) sends.push(saving.sending ? saving.sending.done.then(() => sendPlaylist()) : sendPlaylist(budget));
  saving.last = Promise.allSettled([saving.last, ...sends]);
}
/** Whether a request made as the page goes away uses keepalive, which lets it outlive the page. Browsers allow 64 KiB
 *  of those in flight in all, so each takes its size from `budget` (60 KiB, leaving some room), and one that doesn't
 *  fit goes as a plain request. Never at other times (no `budget`). */
function keepalive(budget, body) {
  if (!budget) return false;
  const size = new Blob([body]).size;
  if (size >= budget.left) return false;
  budget.left -= size;
  return true;
}

/** A widget's JSON when it needs saving (it differs from what was last saved, or Keep mine said so) and isn't on its
 *  way already; else null. */
function widgetToSave(id) {
  const meta = libMeta.get(id), content = lib.get(id);
  if (!meta || !content || isReadOnly(id) || conflicts.has(id)) return null;
  const body = JSON.stringify(content);
  return (body === meta.saved && !meta.force) || body === meta.sending?.body ? null : body;
}
/** Posts a widget's save now, if it needs one, and takes in the answer when it comes. */
function sendWidget(id, budget = null) {
  const body = widgetToSave(id);
  if (body === null) return Promise.resolve();
  const meta = libMeta.get(id), sending = { body };
  const req = JSON.stringify({ id, widget: lib.get(id), stamp: meta.stamp, ...(meta.force ? { force: true } : {}) });
  const posted = api("/api/widgets/save", { method: "POST", headers: { "Content-Type": "application/json" }, body: req,
                                            keepalive: keepalive(budget, req) });
  sending.done = (async () => {
    let res, out;
    try {
      res = await posted;
      out = await res.json().catch(() => ({}));
    } catch (err) {
      return message(`Couldn't save the widget ${widgetName(id)}: ${err.message}`, "warn");
    } finally {
      if (meta.sending === sending) meta.sending = null;
    }
    if (libMeta.get(id) !== meta) return;  // dropped from the library meanwhile
    if (res.ok) {
      meta.stamp = out.stamp ?? meta.stamp;
      meta.force = false;
      libTouch(id);
      if (JSON.stringify(lib.get(id)) === body && out.widget && typeof out.widget === "object") {
        lib.set(id, wellFormed(out.widget));  // as the server wrote it (normalised), without an undo step
        meta.saved = JSON.stringify(lib.get(id));
        libEpoch++;
        docEpoch++;
        redraw();
      } else meta.saved = body;  // changed meanwhile: those edits stay to save, and the next save normalises them
    } else if (res.status === 409) askConflict(id);
    else if (res.status === 404) widgetGone(id);
    else message(`Couldn't save the widget ${widgetName(id)}: ${out.error || res.statusText}`, "warn");
  })();
  meta.sending = sending;
  return sending.done;
}

/** The playlist's JSON when it needs saving and isn't on its way already; else null. (Editing only a widget doesn't
 *  rewrite playlist.json.) */
function playlistToSave() {
  const body = JSON.stringify(doc);
  return body === saving.doc || body === saving.sending?.body ? null : body;
}
/** Posts the playlist now, if it needs saving. */
function sendPlaylist(budget = null) {
  const body = playlistToSave();
  if (body === null) return Promise.resolve();
  const sending = { body };
  const posted = api("/api/playlist", { method: "POST", headers: { "Content-Type": "application/json" }, body,
                                        keepalive: keepalive(budget, body) });
  sending.done = (async () => {
    try {
      const res = await posted;
      if (!res.ok) throw new Error((await res.json()).error || res.statusText);
      saving.doc = body;
    } catch (err) {
      message(`Couldn't save the playlist: ${err.message}`, "warn");
    } finally {
      if (saving.sending === sending) saving.sending = null;
    }
  })();
  saving.sending = sending;
  return sending.done;
}

/** Asks the server to keep every file the history could still undo back to (docs/widgets.md, Undo): when that set
 *  changed since the last time, or every 5 minutes. A hold already on its way is waited for, not sent again. */
function sendHold(budget = null) {
  const refs = [...new Set([...hist.undo, ...hist.redo].flatMap(entryRefs))].sort(), key = JSON.stringify(refs);
  if (saving.holding?.key === key) return saving.holding.done;
  if (!refs.length || (key === saving.held && Date.now() - saving.heldAt < 5 * 60e3)) return Promise.resolve();
  const body = JSON.stringify({ refs }), holding = { key };
  holding.done = api("/api/media/hold", { method: "POST", headers: { "Content-Type": "application/json" }, body,
                                          keepalive: keepalive(budget, body) })
    .then((res) => { if (res.ok) [saving.held, saving.heldAt] = [key, Date.now()]; }, () => {})
    .finally(() => { if (saving.holding === holding) saving.holding = null; });
  saving.holding = holding;
  return holding.done;
}

// --- The library: widgets as the server has them ------------------------------------------------------

const lostGens = new Map();  // id -> the gen a widget had when it left the library, should it come back
const conflicts = new Set(); // widgets changed both here and on disk, waiting for an answer
// What this page changed in the library, and when, so a listing asked for before that leaves it alone: each widget
// added, dropped, taken in or saved here, with the count of such changes then.
let libActs = 0;
const libActAt = new Map();
function libTouch(id) { libActAt.set(id, ++libActs); }

/** A widget's content with what the player passes over taken out: layers and objects that aren't objects, and
 *  `layers` or `objects` that aren't lists (only a file made by hand has these: a save refuses them). When a layer
 *  goes, each layer without an id gets the l<index> it was drawn with, so its groups keep their ids. Anything else
 *  stays as it is. */
function wellFormed(w) {
  const isObject = (v) => !!v && typeof v === "object" && !Array.isArray(v);
  if (!isObject(w)) return w;
  const listed = (l) => isObject(l) && (l.objects === undefined || (Array.isArray(l.objects) && l.objects.every(isObject)));
  if (Array.isArray(w.layers) && w.layers.every(listed)) return w;
  const layers = Array.isArray(w.layers) ? w.layers : [], gone = !layers.every(isObject);
  return { ...w, layers: layers.flatMap((l, i) => (!isObject(l) ? [] : [{
    ...l,
    ...(gone && !(typeof l.id === "string" && l.id) ? { id: `l${i}` } : {}),
    ...(l.objects === undefined ? {} : { objects: Array.isArray(l.objects) ? l.objects.filter(isObject) : [] }),
  }])) };
}

/** Adds a widget the server has (an entry of GET /api/widgets, or what New, Duplicate, Import or Restore answer:
 *  {id, widget, stamp, ...}) to the library, outside the history. What the page holds is well formed; it's
 *  written back only if it's edited. */
function libAdd(e) {
  const old = libMeta.get(e.id), content = wellFormed(e.widget);
  lib.set(e.id, content);
  libMeta.set(e.id, { stamp: e.stamp ?? null, gen: old?.gen ?? lostGens.get(e.id) ?? 0, saved: JSON.stringify(content),
                      resolvedFonts: e.resolved_fonts || old?.resolvedFonts || {}, ownFonts: e.own_fonts || old?.ownFonts || [],
                      readOnly: e.read_only ?? old?.readOnly ?? null, force: false });
  libTouch(e.id);
  libEpoch++;
}
/** The one way content under an id the library holds is replaced from outside the history (a change on disk, Use
 *  the file's version, Import with Replace, Restore): every undo and redo state takes it too, so no undo or redo can
 *  write the older content back over it. */
function takeInWidget(id, content, stamp) {
  const meta = libMeta.get(id) || { gen: lostGens.get(id) ?? 0 };
  meta.gen = (meta.gen ?? 0) + 1;
  libAdd({ id, widget: content, stamp });
  libMeta.get(id).gen = meta.gen;
  rewriteHistory(id, lib.get(id), meta.gen);
}
/** Drops a widget from the library (its copies show as missing). `history`: from every undo and redo state too, for
 *  a folder gone from disk. */
function libDrop(id, { history = false } = {}) {
  libTouch(id);
  if (libMeta.has(id)) lostGens.set(id, libMeta.get(id).gen);
  lib.delete(id);
  libMeta.delete(id);
  conflicts.delete(id);
  libEpoch++;
  if (history) rewriteHistory(id, undefined);
  if (holderWidgetId() === id) {  // it was open: back to where you came from
    dropStaleNav();
    const back = nav.pop();
    openHolder(back?.key ?? doc.scenes[0]?.id ?? "global");
  }
}
function rewriteHistory(id, content, gen) {
  for (const e of [...hist.undo, ...hist.redo]) {
    if (!e.state.includes(JSON.stringify(id))) continue;
    const s = JSON.parse(e.state);
    if (!s.widgets || !Object.hasOwn(s.widgets, id)) continue;
    if (content === undefined) { delete s.widgets[id]; delete s.gens?.[id]; }
    else { s.widgets[id] = content; (s.gens ||= {})[id] = gen; }
    e.state = JSON.stringify(s);
    e.refs = null;
  }
}

/** Reads the library again (GET /api/widgets): new folders come in, gone ones go, fonts are found again, and a
 *  widget whose file changed outside this page is taken in, or, with unsaved changes here too, asked about. `take`
 *  lists widgets whose file wins anyway (Use the file's version). */
async function refreshLibrary(take = []) {
  const asked = libActs;
  let out;
  try {
    const res = await api("/api/widgets");
    out = await res.json();
    if (!res.ok) throw new Error(out.error || res.statusText);
  } catch (err) {
    return message(`Couldn't read the widgets: ${err.message}`, "warn");
  }
  // a widget this page added, dropped, took in or saved while the listing was on its way: the page knows better
  const since = (id) => (libActAt.get(id) ?? 0) > asked;
  const seen = new Set();
  for (const e of Array.isArray(out.widgets) ? out.widgets : []) {
    if (!e || typeof e.id !== "string") continue;
    seen.add(e.id);
    if (since(e.id)) continue;
    const meta = libMeta.get(e.id);
    if (!meta) { libAdd(e); continue; }
    Object.assign(meta, { resolvedFonts: e.resolved_fonts || {}, ownFonts: e.own_fonts || [], readOnly: e.read_only ?? null });
    if (JSON.stringify(e.stamp) === JSON.stringify(meta.stamp)) continue;
    if (JSON.stringify(lib.get(e.id)) === meta.saved || take.includes(e.id)) takeInWidget(e.id, e.widget, e.stamp);
    else askConflict(e.id);
  }
  for (const id of [...lib.keys()]) if (!seen.has(id) && !since(id)) libDrop(id, { history: true });
  libProblems = Array.isArray(out.problems) ? out.problems : [];
  libDeleted = Array.isArray(out.deleted) ? out.deleted : [];
  libEpoch++;
  fontsShown = "";
  changed({ save: false });
}
let libPolled = 0;
window.addEventListener("focus", () => {  // folders copied in by hand, edits from elsewhere: at most every 2 s
  if (!widgetsOn || Date.now() - libPolled < 2000) return;
  libPolled = Date.now();
  afterSaves(() => refreshLibrary());
});

/** A widget changed both here and on disk: library.js's #conflictDlg asks which to keep (its `openConflict(id)`),
 *  then calls keepConflict. Until it's there, a plain question. */
function askConflict(id) {
  if (conflicts.has(id)) return;
  conflicts.add(id);
  if (typeof openConflict === "function") return openConflict(id);
  keepConflict(id, confirm(`${widgetName(id)} was changed outside this page too. OK keeps yours, Cancel takes the file's version.`));
}
/** Settles a conflict: `mine` saves this page's version over the file, else the file's version is taken in. */
async function keepConflict(id, mine) {
  conflicts.delete(id);
  const meta = libMeta.get(id);
  if (!meta) return;
  if (mine) { meta.force = true; saveSoon(0); }
  else afterSaves(() => refreshLibrary([id]));
}
/** A save found the widget's folder gone: offers to restore it from Recently deleted, else forgets it. */
async function widgetGone(id) {
  const name = widgetName(id);
  await refreshDeleted();
  const trash = libDeleted.find((d) => d.id === id)?.trash;
  if (trash && confirm(`${name} was deleted outside this page. Restore it? (Cancel forgets it.)`)) {
    try {
      const out = await postJson("/api/widgets/restore", { trash });
      const meta = libMeta.get(id);
      if (meta) {  // this page's edits save over it
        meta.stamp = out.stamp;
        meta.saved = JSON.stringify(out.widget);
        libTouch(id);
      }
      return saveSoon(0);
    } catch (err) {
      message(`Couldn't restore ${name}: ${err.message}`, "warn");
    }
  } else message(`${name} was deleted outside this page.`, "warn");
  libDrop(id, { history: true });
  changed({ save: false });
}
async function refreshDeleted() {
  try { libDeleted = (await (await api("/api/widgets")).json()).deleted || libDeleted; } catch {}
}

/** Moves a widget's folder to Recently deleted (the server first) and drops it from the library. Its trash name,
 *  or null when that didn't happen (in use, say): then nothing has changed. */
async function trashFolder(action) {
  try {
    const out = await postJson("/api/widgets/delete", { id: action.id });
    action.trash = out.trash;
  } catch (err) {
    message(`Couldn't delete ${widgetName(action.id)}: ${err.message}`, "warn");
    return null;
  }
  libDrop(action.id);
  return action.trash;
}
/** Brings a deleted widget's folder back (undoing a delete). If its id is taken, says so and leaves its copies
 *  showing as missing. */
async function restoreFolder(action) {
  try {
    const out = await postJson("/api/widgets/restore", { trash: action.trash });
    if (lib.has(out.id)) takeInWidget(out.id, out.widget, out.stamp);
    else libAdd(out);
    afterSaves(() => refreshLibrary());  // its fonts and the deleted list
  } catch (err) {
    message(`Couldn't bring ${action.id} back: ${err.message}`, "warn");
  }
  changed({ save: false });
}
/** Deletes widget `id` as one undo step (docs/widgets.md, Undo): its folder moves to Recently deleted first; then,
 *  with `copies`, every copy of it goes too, in the playlist and in each widget that isn't read-only. Ctrl+Z brings
 *  back both. Resolves to whether it happened. */
async function deleteWidget(id, { copies = false } = {}) {
  const action = { action: "delete", id, trash: null }, before = snapshot();
  try {
    if (!(await trashFolder(action))) return false;
    if (copies) {
      const holders = [doc.global, ...doc.scenes, ...[...lib].filter(([w]) => !isReadOnly(w)).map(([, c]) => c)];
      for (const h of holders) {
        for (const l of layersOf(h)) {
          if (Array.isArray(l?.objects)) l.objects = l.objects.filter((o) => !(o?.type === "widget" && o.widget === id));
        }
      }
    }
  } finally {
    if (action.trash) {  // the folder has moved: whatever happened since, the step that brings it back is there
      pushUndo(before, "", action);
      changed();
    }
  }
  return true;
}

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

/** Opens a scene, "global" or "widget:<id>" for editing (docs/widgets.md, The widget editor). With `back` (Edit
 *  widget, a "Used in" link) where you were goes onto `nav`, for ← Back. `preview` ({holder, copy}: the copy a
 *  widget was opened from) fills the widget in with that copy's values instead of its defaults. */
function openHolder(key, { back = false, preview = null } = {}) {
  if (widgetsOn && !holderExists(key)) return;
  if (back && sel.scene !== null && sel.scene !== key) nav.push({ key: sel.scene, ids: [...sel.ids] });
  if (key === "global") { if (!lastScene) lastScene = doc.scenes[0]?.id ?? null; }
  else if (!isWidgetHolder(key)) lastScene = key;
  sel.scene = key;
  sel.ids = [];
  sel.layer = null;  // nothing selected: the scene's (or widget's) own properties show
  view.preview = isWidgetHolder(key) ? preview : null;
  endDrag();
  if (widgetsOn) useCanvas();
  showTitle();
  changed({ save: false });
}
const showScene = (id) => openHolder(id);

/** Drops the ← Back entries whose holder is gone (a scene deleted, a widget deleted or gone from disk). */
function dropStaleNav() {
  for (let i = nav.length - 1; i >= 0; i--) if (!holderExists(nav[i].key)) nav.splice(i, 1);
}
/** A holder's name as the page shows it. */
function holderName(key) {
  return key === "global" ? "Global" : isWidgetHolder(key) ? widgetName(holderWidgetId(key)) : sceneById(key)?.name || "scene";
}

/** The title over the canvas: the scene, Global, or `Widget: <name> · <w> × <h> · used by <n> copies`; then ← Back
 *  while there's somewhere to go back to, what a widget is previewed with, and why it's read-only. */
function showTitle() {
  const id = holderWidgetId();
  if (id === null) $("sceneTitle").textContent = sel.scene === "global" ? "Global layers (over every scene)" : (holder()?.name || "");
  else {
    let n = 0;
    for (const list of WV.uses(lib, doc, closureMemo()).get(id) || []) n += list.direct;
    $("sceneTitle").textContent = `Widget: ${widgetName(id)} · ${containerSize(id).replace("x", " × ")} · used by ${n} cop${n === 1 ? "y" : "ies"}`;
  }
  dropStaleNav();
  const top = nav[nav.length - 1];
  $("back").hidden = !top;
  $("back").textContent = top ? `← Back to ${holderName(top.key)}` : "";
  if (!widgetsOn) return;
  const copy = id !== null && view.preview ? findCopy(view.preview.holder, view.preview.copy) : null;
  const note = $("previewNote"), why = $("readOnlyNote");
  if (copy?.widget === id) {
    const h = view.preview.holder, inside = isWidgetHolder(h) ? ` (${holderName(h)}'s defaults)` : "";
    $("previewText").textContent = `Previewing with ${copy.name || widgetName(id)} in ${holderName(h)}${inside} · `;
    note.hidden = false;
  } else note.hidden = true;
  why.hidden = !(id !== null && isReadOnly(id));
  if (!why.hidden) {
    $("readOnlyText").textContent = typeof lib.get(id)?.version === "number" && lib.get(id).version > 1 || libMeta.get(id)?.readOnly === "newer"
      ? "Made by a newer studio: shown read-only. " : "An example: shown read-only, as it's shipped with the studio. ";
  }
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
  if (isWidgetHolder()) return { x: [0, W / 2, W], y: [0, H / 2, H] };  // a container's edges and centre lines
  return { x: unique([0, W, SAFE.left, W - SAFE.right]), y: unique([0, H, SAFE.top, H - SAFE.bottom]) };
}
/** Shifts a span starting at `start` so whichever end (or in a widget, its centre) is nearest a line within reach
 *  sits on it. Returns [new start, the line, or null]. */
function snapSpan(start, size, lines) {
  const reach = SNAP_REACH / view.k;
  let best = null;
  for (const line of lines) {
    for (const edge of isWidgetHolder() ? [start, start + size, start + size / 2] : [start, start + size]) {
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

/** The art size of a media object: its turned source in art pixels (file pixels / `pixel`). A copy's is its
 *  widget's container (its stored size when the widget can't be drawn). */
function artSize(o) {
  if (o.type === "widget" && widgetsOn) {
    const def = WV.idOk(o.widget) && lib.has(o.widget) ? WV.readDef(o.widget, lib.get(o.widget)) : null;
    return def && !def.problem ? [def.w, def.h] : [Math.max(1, num(o.w, 1)), Math.max(1, num(o.h, 1))];
  }
  const m = holderMedia(o);
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

/** A new image or video object from an upload's info, fitted to the flat front (a widget's container), or centred
 *  where it was dropped. In a widget, `info.src` is the file's path in its folder and `info.ref` its media ref. */
function placeMedia(info, at = null) {
  const o = { id: "", type: info.type === "video" ? "video" : "image", name: info.name.replace(/\.[^.]+$/, ""), visible: true, locked: false,
              opacity: 1, x: 0, y: 0, w: info.width, h: info.height, rotate: 0, flip: false, src: info.src, smooth: false, fill: "none",
              pixel: Math.max(1, num(info.pixel, 1)) };
  const m = getMedia(isWidgetHolder() ? info.ref || mediaRef(info.src) : info.src);
  if (!m.info) { useMediaInfo(m, info); loadFrames(m).catch((err) => { m.error = err.message; redraw(); }); }
  fitMedia(o, FRONT(), false);
  if (at) { o.x = Math.round(at[0] - o.w / 2); o.y = Math.round(at[1] - o.h / 2); }
  addObject(o);
  // a scene's background goes black under its first picture; a widget's stays as its author set it
  if (holder() && !isGlobal() && !isWidgetHolder() && hex(holder().background, null) === null) holder().background = "#000000";
  return o;
}

async function uploadFile(file, at = null) {
  if (!holder()) return message("Add a scene first.", "warn");
  const id = holderWidgetId();
  if (id !== null && isReadOnly(id)) return message(`${widgetName(id)} is read-only: duplicate it to edit.`, "warn");
  message(`Opening ${file.name}…`);
  try {
    const url = id !== null ? `/api/widgets/upload?id=${encodeURIComponent(id)}&name=${encodeURIComponent(file.name)}`
      : `/api/upload?name=${encodeURIComponent(file.name)}`;  // into the open widget's media\, or the playlist's sources\
    const res = await api(url, { method: "POST", body: file });
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
           text: "Text", font: isWidgetHolder() ? "" : DEFAULT_FONT,  // a widget's fonts are file names: "" is Segoe UI
           size: 48, color: "#ffffff", align: "left", valign: "top", line_height: 1.2, outline: { color: "#000000", width: 0 }, crisp: false };
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
      const [r, g, b, a] = sctx.getImageData(px, py, 1, 1).data;
      if (isWidgetHolder() && a === 0) return;  // a widget's transparent pixels: no colour to take
      edit(() => { holder().background = `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`; });
    }
    return;
  }
  if (!holder()) return;
  try { stage.setPointerCapture(e.pointerId); } catch {}
  const [px, py] = screenPoint(e, true);
  if (isReadOnly(holderWidgetId())) {  // for viewing: things can be selected, nothing moves or is added
    const hit = objectAt(e);
    select(hit ? [hit.obj.id] : []);
    return;
  }
  if (tool.name === "text") {
    const o = addObject(newText(Math.round(px), Math.round(py)));
    setTool("select");
    showProps();
    $("tText").focus();
    $("tText").select();
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
  if (moved) { docEpoch++; redraw(); showProps(); } else drawOverlay();
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
  if ([o.x, o.y, o.w, o.h].join() !== before) { docEpoch++; redraw(); showProps(); } else drawOverlay();
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
  if (document.querySelector("dialog[open]")) return;  // a window has its own keys, and Esc closes it
  const typing = e.target.closest?.("input:not([type=checkbox]):not([type=range]), select, textarea");
  if (offerHooks("key", e, typing || null)) return;  // widgets' keys (W, Ctrl+G, Enter) first
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
  if (offerHooks("drop", e)) return;  // widgets first: a widget's card, a .zip to import
  const f = e.dataTransfer.files[0];
  if (!f) return;
  const onStage = e.target.closest("#stage"), at = onStage ? screenPoint(e, true) : null;
  uploadFile(f, at && at[0] >= 0 && at[0] < W && at[1] >= 0 && at[1] < H ? at : null);
});
stage.addEventListener("dblclick", (e) => { offerHooks("dblclick", e); });  // widgets: a copy opens its widget
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
 *  everything: something animated is above it, in its scene or the global layers (docs/widgets.md, Live text). */
function liveOnTop(o, h = holder()) {
  if (widgetsOn) {
    if (o.type !== "text") return false;
    const { place } = currentView();
    return !!place?.onTop && place.live.some((l) => l === o || (isWidgetHolder() && l.top === o.id));
  }
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

/** The open holder's objects as drawn: each one's leaves, by its id; why each copy that draws nothing doesn't; and
 *  the live placement. */
function drawnObjects() {
  const { own, over, place } = currentView(), leaves = new Map(), marks = new Map();
  for (const e of isGlobal() ? over : own) {
    for (const l of e.leaves) {
      const k = l.top ?? l.id;
      if (!leaves.has(k)) leaves.set(k, []);
      leaves.get(k).push(l);
    }
    for (const m of e.marks) marks.set(m.id, m.kind);
  }
  return { leaves, marks, place };
}
/** Whether an object of a widget uses one of its inputs: a reference in its text, a bound colour, tint or picture,
 *  or a copy's value passing one down. */
function usesInput(o) {
  let found = false;
  const look = (v) => { if (typeof v === "string") WV.replaceRefs(v, () => { found = true; return null; }); };
  for (const v of [o.text, o.color, o.outline?.color, o.fill?.color, o.stroke?.color, o.src, o.tint]) look(v);
  for (const s of Array.isArray(o.fill?.stops) ? o.fill.stops : []) look(s?.color);
  if (o.type === "widget" && o.inputs && typeof o.inputs === "object") for (const v of Object.values(o.inputs)) look(v);
  return found;
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
  const layers = layersOf(h), drawn = widgetsOn ? drawnObjects() : null;
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
      const copy = widgetsOn && o.type === "widget";
      const icon = make("span", "icon", copy ? "▣" : { image: "🖼", video: "🎞", text: "T", shape: { rect: "▭", ellipse: "◯", line: "╱" }[o.shape] || "▭" }[o.type] || "?");
      // a copy without a name of its own goes by its widget's current name
      const n = make("span", "name", o.name || (copy ? widgetName(o.widget) : o.type === "text" ? String(o.text ?? "").split("\n")[0] : o.id) || o.id);
      n.title = copy ? `Copy of ${widgetName(o.widget)} (widgets\\${o.widget})` : o.type === "text" ? String(o.text ?? "") : o.src || "";
      rename(n, () => o.name || "", (v) => { find(o.id).obj.name = v; });
      r.append(tog(o.visible !== false, "eye", "Shown: click to hide", "Hidden: click to show", () => edit(() => { const x = find(o.id).obj; x.visible = x.visible === false; })),
               tog(!!o.locked, "lock", "Locked: click to unlock", "Unlocked: click to lock", () => edit(() => { const x = find(o.id).obj; x.locked = !x.locked; })),
               icon, n);
      // what a copy, or a widget's own object, draws is its leaves: badges come from them
      const leaves = drawn?.leaves.get(o.id) || [], inside = !!drawn && (copy || isWidgetHolder());
      if (inside ? leaves.some((l) => isAnimated(l)) : isAnimated(o, sel.scene)) {
        const b = make("span", "badge anim", "anim"); b.title = "Animated: loops on its own timing"; r.append(b);
      }
      if (inside ? leaves.some((l) => l.type === "text" && l.live) : o.type === "text" && isLive(o.text)) {
        const top = inside ? !!drawn.place?.onTop && leaves.some((l) => drawn.place.fold.has(l)) : liveOnTop(o, h);
        const b = make("span", "badge" + (top ? " top" : " live"), top ? "live, on top" : "live");
        b.title = top ? "Live text with something animated above it: the player draws it on top of everything"
          : "Live text: the player fills in its values while playing";
        r.append(b);
      }
      const why = drawn?.marks.get(o.id);
      if (why) {
        const b = make("span", "badge warn", { missing: "missing", newer: "newer", cycle: "cycle", deep: "too deep", budget: "too many" }[why]);
        b.title = { missing: `There's no widget ${o.widget} in widgets\\`, newer: "Made by a newer studio", cycle: "It contains itself",
                    deep: "More than 8 copies deep", budget: "Too many objects in this scene's copies" }[why];
        r.append(b);
      }
      if (isWidgetHolder() && usesInput(o)) { const b = make("span", "badge in", "in"); b.title = "Uses an input of this widget"; r.append(b); }
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

/** The properties panel: the sections for the selection (or the scene, widget or layer), filled in. Then the widget
 *  scripts' props hooks, which fill their own rows in those sections. */
function showProps() {
  fillProps();
  offerHooks("props", primary());
}

function fillProps() {
  const h = holder(), p = primary(), layer = !p && selectedLayer();
  $("pScene").hidden = !(h && !isGlobal() && !isWidgetHolder() && !p && !layer);
  $("pGlobal").hidden = !(isGlobal() && !p && !layer);
  $("pWidget").hidden = !(isWidgetHolder() && !p && !layer);
  $("pLayer").hidden = !layer;
  $("pObject").hidden = !p;
  $("pMedia").hidden = !(p && isMedia(p));
  $("pText").hidden = !(p && p.type === "text");
  $("pShape").hidden = !(p && p.type === "shape");
  $("pCopy").hidden = !(widgetsOn && p?.type === "widget");
  $("pExport").hidden = !(h && !isGlobal() && !isWidgetHolder() && !p && !layer);
  if (!h) return;
  if (!$("pScene").hidden) {
    set("sName", h.name || "");
    set("sSeconds", num(h.seconds, 30));
    set("sBg", hex(h.background, "#000000"));
    const n = layersOf(h).reduce((a, l) => a + (l.objects || []).length, 0);
    const first = `${layersOf(h).length} layer${layersOf(h).length === 1 ? "" : "s"}, ${n} object${n === 1 ? "" : "s"}. Click an object on the canvas or in Layers to edit it.`;
    const more = widgetsOn ? sceneNotes(h) : [];
    if (more.length) $("sceneNote").replaceChildren(...[first, ...more].map((line) => make("div", "", line)));
    else $("sceneNote").textContent = first;
  }
  if (layer) {
    set("lName", layer.name || "");
    set("lOpacity", Math.round(100 * clamp01(layer.opacity)));
    set("lOpacityR", Math.round(100 * clamp01(layer.opacity)));
  }
  if (!$("pWidget").hidden) showWidgetProps(holderWidgetId());
  if (!p) return;
  $("oKind").textContent = { image: "Image", video: "Video", text: "Text", shape: "Shape", widget: "Widget copy" }[p.type] || p.type;
  set("oName", p.name || "");
  set("oOpacity", Math.round(100 * clamp01(p.opacity)));
  set("oOpacityR", Math.round(100 * clamp01(p.opacity)));
  set("oX", num(p.x, 0)); set("oY", num(p.y, 0)); set("oW", num(p.w, 0)); set("oH", num(p.h, 0));
  $("oSizeRow").hidden = p.type === "text";
  $("oSizeRow").style.display = p.type === "text" ? "none" : "contents";
  $("oTurnRow").hidden = !isMedia(p);
  seg("oRotate", num(p.rotate, 0) % 360);
  $("oFlip").checked = !!p.flip;
  $("oWidgetRow").hidden = !widgetsOn;
  if (isMedia(p)) showMediaProps(p);
  else if (p.type === "text") showTextProps(p);
  else if (widgetsOn && p.type === "widget") showCopyProps(p);
  else showShapeProps(p);
}

/** What the scene note adds (docs/widgets.md, Live text; Animation and the timeline): that live values here redraw
 *  every frame of an animation, and which copy's animation stretches the loop to the 60 s cap. */
function sceneNotes(h) {
  const own = expanded(h), over = expanded(doc.global), notes = [];
  const moving = [...own, ...over].flatMap((e) => e.leaves).filter((l) => isAnimated(l));
  if (!moving.length) return notes;
  if (WV.placement([own, over], (l) => isAnimated(l)).live.length) {
    const { frames } = timeline(moving);
    notes.push(`Live values here redraw ${frames} frame${frames === 1 ? "" : "s"} each time they change, so the cooler updates them less often (the longer the animation, the longer between updates).`);
  }
  const plain = moving.filter((l) => !WV.isLeaf(l));
  if (!timeline(moving).capped || timeline(plain).capped) return notes;
  for (const [stack, holderOf, loop] of [[own, h, "this scene's loop"], [over, doc.global, "every scene's loop"]]) {
    const tops = [...new Set(stack.flatMap((e) => e.leaves).filter((l) => WV.isLeaf(l)).map((l) => l.top))];
    for (const top of tops) {
      if (!timeline([...plain, ...moving.filter((l) => WV.isLeaf(l) && l.top === top)]).capped) continue;
      const copy = find(top, holderOf)?.obj;
      notes.push(`${widgetName(copy?.widget)}'s animation makes ${loop} 60 s long.`);
      return notes;
    }
  }
  return notes;
}
/** The player's timeline (scene.rs) over animated leaves: how many frames one cycle has, and whether the cycle (the
 *  least common multiple of their loops) reached the 60 s cap. */
function timeline(leaves) {
  const loops = [...new Set(leaves.map((l) => media.get(l.src)))].filter((m) => m?.ready && m.starts.length > 1);
  if (!loops.length) return { frames: 1, capped: false };
  const gcd = (a, b) => (b ? gcd(b, a % b) : a), CAP = 60000;
  let cycle = 1, capped = false;
  for (const m of loops) {
    const l = Math.max(1, Math.round(m.total));
    cycle = cycle / gcd(cycle, l) * l;
    if (cycle >= CAP) { cycle = CAP; capped = true; break; }
  }
  const times = new Set();
  for (const m of loops) {
    for (let base = 0; base < cycle; base += m.total) for (const s of m.starts) { if (base + s >= cycle) break; times.add(base + s); }
  }
  let frames = 0, last = -Infinity;
  for (const t of [...times].sort((a, b) => a - b)) if (t - last >= 20) { frames++; last = t; }
  return { frames, capped };
}

function showMediaProps(p) {
  const m = holderMedia(p), info = m?.info;
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
  set("tText", String(w.text ?? ""));
  set("tSize", num(w.size, 48));
  set("tColor", hex(w.color, "#ffffff"));
  set("tLineHeight", num(w.line_height, 1.2));
  set("tOutlineColor", hex(w.outline?.color, "#000000"));
  set("tOutlineWidth", num(w.outline?.width, 0));
  $("tCrisp").checked = !!w.crisp;
  seg("tAlign", w.align || "left");
  seg("tValign", w.valign || "top");
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
/** The font picker: the fonts whose names have every word typed in the filter, plus the text's own. */
function showFonts(w) {
  if (widgetsOn) {
    if (isWidgetHolder()) return showWidgetFonts(w);
    if ($("tFontOffer")) $("tFontOffer").hidden = true;
  }
  const words = $("tFontFilter").value.toLowerCase().split(/\s+/).filter(Boolean);
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
  $("tFont").replaceChildren(...options);
}

/** The font picker in a widget (docs/widgets.md, Fonts): fonts by file name, the widget's own (its fonts\ folder)
 *  first, then the PC's. A name not found anywhere says so, and offers a font installed under another file name
 *  with the same family and style, when there's one. */
function showWidgetFonts(w) {
  const id = holderWidgetId(), meta = libMeta.get(id) || {}, current = typeof w.font === "string" ? w.font : "";
  const words = $("tFontFilter").value.toLowerCase().split(/\s+/).filter(Boolean), found = meta.resolvedFonts?.[current];
  const key = JSON.stringify([id, words, current, fontList.length, meta.ownFonts, found]);
  if (key === fontsShown) return;
  fontsShown = key;
  const file = (path) => String(path).split("\\").pop(), seen = new Set(), entries = [];
  for (const f of [...(meta.ownFonts || []).map((f) => ({ file: f.name, family: f.family, style: f.style, own: true })),
                   ...fontList.map((f) => ({ file: file(f.path), family: f.family, style: f.style }))]) {
    if (typeof f.file === "string" && !seen.has(f.file.toLowerCase())) { seen.add(f.file.toLowerCase()); entries.push(f); }
  }
  const name = (f) => `${f?.family || ""} ${f?.style || ""}`.trim();
  const label = (f) => `${name(f) || f.file}${f.own ? " · this widget's own" : ""} · ${f.file}`;
  const shown = (current || file(DEFAULT_FONT)).toLowerCase();
  const options = entries.filter((f) => words.every((word) => label(f).toLowerCase().includes(word)))
    .map((f) => new Option(label(f), f.file, false, f.file.toLowerCase() === shown));
  if (!options.some((o) => o.selected)) {
    const missing = found && !found.path;
    options.unshift(new Option(missing ? `${name(found) || current} · ${current} (not on this PC: shown in Segoe UI)` : current || file(DEFAULT_FONT),
                               current, false, true));
  }
  $("tFont").replaceChildren(...options);
  let offer = $("tFontOffer");
  if (!offer) {
    offer = make("button", "small");
    offer.id = "tFontOffer";
    $("tFont").after(offer);
    offer.addEventListener("click", () => {
      const to = offer.dataset.to, from = offer.dataset.from, h = holder();
      if (to && isWidgetHolder()) edit(() => { for (const l of layersOf(h)) for (const o of l.objects || []) if (o.type === "text" && o.font === from) o.font = to; });
    });
  }
  const installed = found && !found.path && Array.isArray(found.installed) ? found.installed[0] : null;
  offer.hidden = !installed;
  if (installed) {
    offer.textContent = `Use the installed ${name(found) || installed} (${installed})`;  // one step for every text in that font
    [offer.dataset.from, offer.dataset.to] = [current, installed];
  }
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
$("oFromSelection").addEventListener("click", () => { if (widgetsOn) WV.fromSelection(); });
// layer and scene
const layerControl = (id, apply, event = "input") => $(id).addEventListener(event, () => {
  const l = !primary() && selectedLayer();
  if (l) edit(() => apply(layerById(l.id), $(id)), id);
});
layerControl("lName", (l, el) => { l.name = el.value; });
layerControl("lOpacity", (l, el) => { const v = number(el, (v) => v >= 0 && v <= 100); if (v !== null) l.opacity = v / 100; });
layerControl("lOpacityR", (l, el) => { l.opacity = +el.value / 100; });
const sceneControl = (id, apply, event = "input") => $(id).addEventListener(event, () => {
  if (holder() && !isGlobal() && !isWidgetHolder()) edit(() => apply(holder(), $(id)), id);
});
sceneControl("sName", (s, el) => { s.name = el.value; $("sceneTitle").textContent = s.name; });
sceneControl("sSeconds", (s, el) => { const v = number(el, (v) => v >= 1); if (v !== null) s.seconds = Math.round(v); }, "change");
sceneControl("sBg", (s, el) => { s.background = el.value; });
$("sBlack").addEventListener("click", () => { if (holder() && !isGlobal() && !isWidgetHolder()) edit(() => { holder().background = "#000000"; }); });
$("sPick").addEventListener("click", () => setPicking(view.picking ? null : "bg"));
$("mPick").addEventListener("click", () => setPicking(view.picking ? null : "bg"));
$("mEdge").addEventListener("click", () => {  // in a widget, its background
  const p = primary(), edge = p && holderMedia(p)?.info?.edge;
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
objControl("tText", (o, el) => { o.text = el.value; }, "input", "tText");
$("tFontFilter").addEventListener("input", () => { const w = primary(); if (w?.type === "text") showFonts(w); });
objControl("tFont", (o, el) => { o.font = el.value; }, "change");
objControl("tSize", (o, el) => { const v = number(el, (v) => v > 0); if (v === null) return false; o.size = v; });
objControl("tColor", (o, el) => { o.color = el.value; });
objControl("tLineHeight", (o, el) => { const v = number(el, (v) => v > 0); if (v === null) return false; o.line_height = v; });
objControl("tOutlineColor", (o, el) => { o.outline = { width: 0, ...o.outline, color: el.value }; });
objControl("tOutlineWidth", (o, el) => { const v = number(el, (v) => v >= 0); if (v === null) return false; o.outline = { color: "#000000", ...o.outline, width: v }; });
objControl("tCrisp", (o, el) => { o.crisp = el.checked; }, "change");
for (const [group, key] of [["tAlign", "align"], ["tValign", "valign"]]) {
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
/** What a scene's (or Global's) card shows depends on: itself, every widget it uses at any depth, the media and fonts
 *  that have arrived, and the live values. A card is drawn again only when this changes. */
function thumbKey(h) {
  const used = widgetsOn ? [...usedWidgets(h)].sort().map((id) => [id, lib.get(id) ?? null]) : [];
  return JSON.stringify([h, ...(used.length ? [used] : []), mediaVersion, fontsArrived, live.version, live.raw]);
}
/** What a widget's card shows depends on: its content, that of every widget it uses at any depth, the media and
 *  fonts that have arrived, and the live values (drawn with its defaults: renderWidget(ctx, id, t, null)). */
function widgetThumbKey(id) {
  const used = [...usedWidgets(lib.get(id))].sort().map((w) => [w, lib.get(w) ?? null]);
  return JSON.stringify([id, lib.get(id) ?? null, used, mediaVersion, fontsArrived, live.version, live.raw]);
}
/** The widgets a holder's copies use, at any depth (WV.closure, each widget walked once). */
function usedWidgets(h) {
  const out = new Set(), memo = closureMemo();
  for (const l of layersOf(h)) {
    for (const o of objectsIn(l)) {
      if (o.type !== "widget" || typeof o.widget !== "string" || out.has(o.widget)) continue;
      out.add(o.widget);
      for (const id of WV.closure(lib, o.widget, memo)) out.add(id);
    }
  }
  return out;
}
let closureAt = "", closures = new Map();
/** The memo WV.closure keeps its walks in, emptied whenever the library or a widget may have changed. */
function closureMemo() {
  const at = `${libEpoch}|${docEpoch}`;
  if (at !== closureAt) [closureAt, closures] = [at, new Map()];
  return closures;
}
function renderThumbs() {
  for (const [id, t] of thumbs) {
    const h = id === "global" ? doc.global : sceneById(id);
    if (!h) { thumbs.delete(id); continue; }
    const key = thumbKey(h);
    if (key === t.key) continue;
    t.key = key;
    withCanvasSize(PANEL.w, PANEL.h, () => {  // at the panel's size, whatever is open
      if (thumbCv.width !== W || thumbCv.height !== H) { thumbCv.width = W; thumbCv.height = H; }
      renderScene(thumbCv.getContext("2d"), id === "global" ? null : h, 0, { withGlobal: id === "global" });
    });
    const g = t.canvas.getContext("2d");
    g.imageSmoothingEnabled = true;
    g.imageSmoothingQuality = "high";
    g.clearRect(0, 0, t.canvas.width, t.canvas.height);
    g.drawImage(thumbCv, 0, 0, t.canvas.width, t.canvas.height);
  }
  if (widgetsOn) renderWidgetCards();  // the widgets' cards, drawn at their own sizes (library.js)
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
    c.addEventListener("click", (e) => {
      if (e.target.closest("button, input") || sel.scene === id) return;
      nav.length = 0;  // a card starts afresh: nothing to go back to
      openHolder(id);
    });
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
  if (widgetsOn) showWidgetCards();  // the Scenes | Widgets tabs, and the widgets' cards (library.js)
  showTitle();
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
  if (widgetsOn) notePlayer(undefined, state.running && "widgets" in state ? state.widgets : null);  // its version
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

/** Whether a text has a placeholder: `{key}` and the rest, not counting {{ and }}. With widgets on, exactly as the
 *  player reads it (data.rs, ported in WV.isLive). */
const isLive = (t) => (widgetsOn ? WV.isLive(t) : /\{[^{}]+\}/.test(String(t ?? "").replace(/\{\{|\}\}/g, "")));
/** The live texts the preview shows, to be filled in: those drawn in the open holder and Global (a widget: itself),
 *  in visible layers, copies' substituted. And the texts the widget scripts ask for (a copy's input rows). */
function liveTexts() {
  const all = [];
  if (widgetsOn) {
    const { own, over } = currentView();
    for (const e of [...own, ...over]) for (const l of e.leaves) if (l.type === "text" && isLive(l.text)) all.push(String(l.text));
    for (const more of WV.texts) { try { all.push(...more().filter((t) => typeof t === "string" && isLive(t))); } catch (err) { console.error(err); } }
  } else {
    for (const h of [holder(), doc.global]) for (const l of layersOf(h)) for (const o of l.objects || []) if (o.type === "text" && isLive(o.text)) all.push(String(o.text));
  }
  return [...new Set(all)];
}
/** The text an object (or leaf) shows in the preview: filled in with the current values, when the player could; or,
 *  for static text with widgets on, with {{ and }} as plain braces, as the player draws it. */
function shownText(w) {
  const raw = String(w.text ?? "");
  if (widgetsOn && !WV.isLive(raw)) return WV.plain(raw);
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
  const w = primary(), el = $("tText");
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
const LONGEST_INTERVAL = 86400; // seconds: the player never waits longer between polls
const intervalOf = (m) => ({ default: Math.max(1, num(m?.interval?.default, 60)), min: Math.max(1, num(m?.interval?.min, 5)) });
// What a setting starts as without a default: a choice's first choice (as the player falls back to), else empty.
const blankOf = (s) => s.type === "choice" ? (Array.isArray(s.choices) && s.choices.length ? String(s.choices[0]) : "")
  : s.type === "bool" ? false : s.type === "list" ? [] : s.type === "number" ? 0 : "";
function defaultsOf(m) {
  const d = { interval: intervalOf(m).default };
  for (const s of settingsOf(m)) {
    if (s.type !== "password") d[s.key] = s.default ?? blankOf(s);
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
  ivIn.type = "number"; ivIn.min = iv.min; ivIn.max = LONGEST_INTERVAL; ivIn.step = 1; ivIn.dataset.k = "interval";
  ivIn.title = `How often it's read: ${iv.default} s unless set, ${iv.min} s at least`;
  const ivBox = make("span");
  ivBox.append(ivIn, make("span", "small muted", ` s (default ${iv.default}, at least ${iv.min})`));
  ivRow.append(make("span", "label", "Read every"), ivBox);
  form.append(ivRow);
  ivIn.addEventListener("change", () => {
    const v = parseInt(ivIn.value, 10);
    if (!(v > 0) || !live.config) return;
    sourceFor(m.id).interval = Math.min(LONGEST_INTERVAL, Math.max(iv.min, v));
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
  const el = $("tText"), ctx = document.activeElement === el ? acContext(el) : null;
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
  const pop = $("acPop"), el = $("tText");
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
  const it = ac.items[i], el = $("tText"), w = primary();
  if (!it) return acClose();
  const text = el.value, closed = text[ac.to] === "}", put = it.insert + (closed ? "" : "}");
  el.value = text.slice(0, ac.from) + put + text.slice(ac.to);
  const pos = ac.from + put.length + (closed ? 1 : 0);
  el.setSelectionRange(pos, pos);
  if (w?.type === "text") { const v = el.value; edit(() => { find(w.id).obj.text = v; }, "tText"); }
  acClose();
}
$("tText").addEventListener("input", acUpdate);
$("tText").addEventListener("click", () => { if (ac.open) acUpdate(); });
$("tText").addEventListener("blur", acClose);
$("tText").addEventListener("keydown", (e) => {
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
    // what's drawn: the open holder's and Global's leaves (a widget: its own), animations inside copies too
    const shown = widgetsOn ? (({ own, over }) => [...own, ...over])(currentView())
      : [holder(), doc.global].flatMap((h) => layersOf(h).map((layer) => ({ layer, leaves: layer.objects || [] })));
    for (const { layer, leaves } of shown) {
      if (layer.visible === false) continue;
      for (const o of leaves) {
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

// The widget editor's notes, after the title: what it's previewed with, and why it's read-only (showTitle).
{
  const note = make("span", "small muted"), text = make("span"), defaults = make("button", "link", "use defaults");
  Object.assign(note, { id: "previewNote", hidden: true });
  text.id = "previewText";
  defaults.title = "Show the widget with its own default values";
  defaults.addEventListener("click", () => { view.preview = null; changed({ save: false }); });
  note.append(text, defaults);
  const why = make("span", "small warn"), whyText = make("span"), dup = make("button", "", "Duplicate to edit");
  Object.assign(why, { id: "readOnlyNote", hidden: true });
  whyText.id = "readOnlyText";
  dup.addEventListener("click", () => duplicateToEdit(holderWidgetId()));
  why.append(whyText, dup);
  $("back").after(note, why);
}
$("back").addEventListener("click", () => {  // back to where Edit widget came from, the copy selected again
  dropStaleNav();
  const top = nav.pop();
  if (!top) return showTitle();
  openHolder(top.key);
  select(top.ids.filter((id) => find(id)));
});
/** Duplicate to edit, for a read-only widget: library.js's window when it has one (its `duplicateWidget(id)`), else
 *  straight away, as "Copy of <name>" (whose id can't start with example-), which then opens. */
async function duplicateToEdit(id) {
  if (id === null) return;
  if (typeof duplicateWidget === "function") return duplicateWidget(id);
  try {
    const out = await postJson("/api/widgets/duplicate", { id, name: `Copy of ${widgetName(id)}` });
    libAdd(out);
    openHolder(`widget:${out.id}`);
  } catch (err) {
    message(`Couldn't duplicate ${widgetName(id)}: ${err.message}`, "warn");
  }
}

/** Reads the library for the first time (GET /api/widgets), before the playlist, so copies draw at once. */
async function loadLibrary() {
  try {
    const res = await api("/api/widgets"), out = await res.json();
    if (!res.ok) throw new Error(out.error || res.statusText);
    for (const e of Array.isArray(out.widgets) ? out.widgets : []) if (e && typeof e.id === "string") libAdd(e);
    libProblems = Array.isArray(out.problems) ? out.problems : [];
    libDeleted = Array.isArray(out.deleted) ? out.deleted : [];
  } catch (err) {
    message(`Couldn't read the widgets: ${err.message}`, "warn");
  }
}
/** As the playlist is read: a scene id a widget's holder key could take ("widget:…", only by hand) gets a new one,
 *  and each copy's stored w and h follow its scale and its widget's container (docs/widgets.md, A copy: an older
 *  studio's resize is undone). Saved with the next save, without an undo step. A widget's own copies are brought up
 *  to date only when it's edited, so one nobody edits is never rewritten. */
function tidyDocument() {
  const ids = new Set(doc.scenes.map((s) => s?.id));
  for (const s of doc.scenes) {
    if (typeof s?.id === "string" && s.id.startsWith("widget:")) { s.id = uid("s", ids); ids.add(s.id); }
  }
  for (const h of [doc.global, ...doc.scenes]) refreshCopySizes(h);
}
const hasCopies = () => [doc.global, ...doc.scenes].some((h) => layersOf(h).some((l) => objectsIn(l).some((o) => o.type === "widget")));
// The player's version (docs/widgets.md, Compatibility): said once a session, and again when it changes, once the
// document has copies. `built` is whether the build for Play and Export PNG draws them (from /api/config), `running`
// whether the player running does (polled with /api/player); null for no such player.
const playerNote = { built: null, running: null, config: null };
function notePlayer(built = playerNote.config, running) {
  if (!widgetsOn || !hasCopies()) return;
  if (built === false && playerNote.built !== false) {
    message("The player was built before widgets: rebuild it (cargo build --release in player\\) to see them on the cooler and in Export PNG.", "warn");
  }
  if (running === false && playerNote.running !== false) {
    message("The player running is older than this studio: widgets won't show on the cooler until it's rebuilt and restarted.", "warn");
  }
  if (built !== undefined) playerNote.built = built;
  if (running !== undefined) playerNote.running = running;
}

const domReady = new Promise((ok) => (document.readyState === "loading" ? document.addEventListener("DOMContentLoaded", ok, { once: true }) : ok()));
(async () => {
  try {
    const cfg = await (await api("/api/config")).json();
    [W, H] = cfg.screen;
    SAFE = cfg.safe;
    CAMERA = cfg.camera;
    PANEL = { w: W, h: H, safe: { ...SAFE }, camera: CAMERA };
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
    await domReady;  // all three widget scripts have run (or failed) by now
    widgetsOn = cfg.widgets === 1 && ["widgets", "library", "copies"].every((n) => window.WV?.loaded?.[n] === true);
    if (widgetsOn) {
      setBackdrop(backdrop());
      await loadLibrary();
    } else if (cfg.widgets !== 1) message("This page is newer than the running studio. Restart the studio to use widgets.", "warn");
    else message("Widgets are off: one of the page's scripts didn't load. Reload the page.", "warn");
    const res = await api("/api/playlist"), out = await res.json();
    if (!res.ok) throw new Error(out.error || res.statusText);
    doc = out;
    doc.global ||= { layers: [] };
    doc.scenes ||= [];
    if (widgetsOn) {
      saving.doc = JSON.stringify(doc);  // as read: what tidyDocument changes goes with the next save
      tidyDocument();
    }
    const want = new URLSearchParams(location.hash.slice(1));  // #scene=<id>&sel=<object id>, #widget=<id>&sel=…: open there
    if (widgetsOn && lib.has(want.get("widget"))) openHolder(`widget:${want.get("widget")}`);
    else showScene(want.get("scene") === "global" || sceneById(want.get("scene")) ? want.get("scene") : doc.scenes[0]?.id ?? "global");
    if (want.get("sel") && find(want.get("sel"))) select([want.get("sel")]);
    playerNote.config = cfg.player_widgets ?? null;
    notePlayer();
    if (cfg.initial) {
      if (!doc.scenes.length) addScene();
      placeMedia(cfg.initial);
    }
    try { showPlayer(await (await api("/api/player")).json()); } catch {}
  } catch (err) {
    message(`Couldn't reach the studio server: ${err.message}`, "warn");
  }
})();
