"use strict";
// Reusable widgets (docs/widgets.md): the page's model, `WV`. Pure functions, no DOM, shared by the drawing in
// studio.js and the panels in library.js and copies.js. Loaded first, before studio.js. It's the page's twin of the
// player's widget.rs and its live-text split: both expand copies with the same rules and the same arithmetic, and
// the shared vectors and fixtures in player/tests/widgets/ hold both to them.

const WV = {
  // studio.js offers these events to each function in turn before handling them itself (offerHooks), while widgets
  // are on: key(e, typing), drop(e), dblclick(e) return true when they've handled the event; props(p) are all called
  // at the end of showProps, with the primary object or null. library.js and copies.js add their own.
  hooks: { drop: [], key: [], dblclick: [], props: [] },
  texts: [],           // functions returning more texts to fill in with live values (a copy's input rows, say)
  fromSelection() {},  // From selection: library.js fills it in; Ctrl+G and #pObject's button call it
  loaded: {},          // each of the three scripts sets its name here as its last line
};
window.WV = WV;

(() => {
  const MAX_DEPTH = 8, BUDGET = 4000, MAX_INPUTS = 32, MAX_PLACED = [6400, 2880];
  const MAX_TEXT = 2000, MAX_LEAF_TEXT = 4000, MAX_ID = 64;  // code points: a value, a copy's text, an id in a path
  const ID_RE = /^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/;
  const INPUT_ID_RE = /^[a-z][a-z0-9_]{0,31}$/;
  const PART_RE = /^[A-Za-z0-9_](?:[A-Za-z0-9_.-]{0,98}[A-Za-z0-9_-])?$/;
  const FONT_RE = /^[^\\/:*?"<>|\x00-\x1f]{1,100}\.(?:ttf|otf)$/iu;
  const RESERVED = new Set(["con", "prn", "aux", "nul", ...[1, 2, 3, 4, 5, 6, 7, 8, 9].flatMap((i) => [`com${i}`, `lpt${i}`])]);
  // What Rust's str::trim removes (Unicode White_Space), as data.rs trims keys. JS's own trim differs on U+FEFF
  // (removes it) and U+0085 (keeps it).
  const WS = "\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000";
  const BLANK = new RegExp(`^[${WS}]*$`), EDGES = new RegExp(`^[${WS}]+|[${WS}]+$`, "g");
  const rustTrim = (s) => s.replace(EDGES, "");
  const spaces = (s) => s.replace(/^ +| +$/g, "");  // whole values are compared with U+0020 trimmed, nothing else

  /** floor(v + 0.5): the one rounding rule for everything a copy places (docs/widgets.md, Geometry). */
  const rnd = (v) => Math.floor(v + 0.5);
  const num = (v, fallback) => (typeof v === "number" && Number.isFinite(v) ? v : fallback);
  const unit = (v) => Math.min(1, Math.max(0, num(v, 1)));  // an opacity, as the player reads it
  const idOf = (o) => (typeof o.id === "string" ? o.id : "");  // as the player reads an id: one that isn't text is ""
  /** The first `n` code points of `s` ([...s].slice(0, n), without making the array): never half an emoji. */
  function cut(s, n = MAX_TEXT) {
    if (s.length <= n) return s;
    let i = 0;
    for (let k = 0; k < n && i < s.length; k++) i += s.codePointAt(i) > 0xffff ? 2 : 1;
    return s.slice(0, i);
  }
  const cutId = (s) => cut(s, MAX_ID);  // a copy's, layer's or object's id, as it goes into leaf and group ids

  const idOk = (id) => typeof id === "string" && ID_RE.test(id) && !RESERVED.has(id);
  const stem = (name) => name.split(".")[0].toLowerCase();
  /** A path in a widget's folder (docs/widgets.md, Paths): 1 to 4 parts, each a PART and not a reserved name. */
  function pathOk(p) {
    if (typeof p !== "string") return false;
    const parts = p.split("/");
    return parts.length <= 4 && parts.every((part) => PART_RE.test(part) && !RESERVED.has(stem(part)));
  }
  // The media a widget holds (docs/widgets.md, Media types), by extension in any letter case: what the studio serves
  // for a widget. A `src` or an image value with another (an .avi, a JPEG named .jfif) is left out, as a bad path is.
  const MEDIA_TYPES = new Set(["png", "apng", "gif", "webp", "jpg", "jpeg", "bmp", "mp4", "m4v", "mov", "webm", "mkv"]);
  function mediaOk(p) {
    const name = p.slice(p.lastIndexOf("/") + 1), dot = name.lastIndexOf(".");
    return dot > 0 && MEDIA_TYPES.has(name.slice(dot + 1).toLowerCase());
  }
  /** A path in a widget's folder that names one of its media (a `src`, an image input's value or default). */
  const mediaPathOk = (p) => pathOk(p) && mediaOk(p);
  /** A font file name (docs/widgets.md, A font name). */
  function fontNameOk(n) {
    return typeof n === "string" && FONT_RE.test(n) && !/^[ .]|[ .]$/.test(n) && !RESERVED.has(stem(n));
  }

  /** A colour (docs/widgets.md, widget.json) as lowercase "#rrggbb", or null. */
  function color(s) {
    if (typeof s !== "string") return null;
    const t = spaces(s);
    if (/^#[0-9a-fA-F]{6}$/.test(t)) return t.toLowerCase();
    if (/^#[0-9a-fA-F]{3}$/.test(t)) return "#" + [...t.slice(1)].map((c) => c + c).join("").toLowerCase();
    return null;
  }

  /** The id a whole value names when it's a reference `{$id}` (spaces around it aside), or null. */
  function refOf(v) {
    const m = typeof v === "string" ? /^\{\$([^{}\n]*)\}$/.exec(spaces(v)) : null;
    return m ? m[1] : null;
  }

  // --- Live text: data.rs's scan, is_live and fill with no values, ported exactly -----------------------------

  /** Text split as data.rs's scan splits it: {text} as written (no braces in it), {brace} for one literal brace
   *  ({{, }}, or a brace that starts nothing), and {hole, body, key, fmt, fallback} for a placeholder. */
  function scan(text) {
    const s = String(text ?? ""), parts = [];
    let i = 0;
    for (;;) {
      let at = i;
      while (at < s.length && s[at] !== "{" && s[at] !== "}") at++;
      if (at > i) parts.push({ text: s.slice(i, at) });
      if (at >= s.length) return parts;
      if (s.startsWith("{{", at) || s.startsWith("}}", at)) {
        parts.push({ brace: s[at] });
        i = at + 2;
        continue;
      }
      if (s[at] === "{") {
        let e = at + 1;
        while (e < s.length && s[e] !== "{" && s[e] !== "}" && s[e] !== "\n") e++;
        if (s[e] === "}") {
          const body = s.slice(at + 1, e), bar = body.indexOf("|");
          const spec = bar < 0 ? body : body.slice(0, bar), colon = spec.indexOf(":");
          const key = colon < 0 ? spec : spec.slice(0, colon);
          if (!BLANK.test(key)) {
            parts.push({ hole: true, body, key: rustTrim(key), fmt: colon < 0 ? "" : rustTrim(spec.slice(colon + 1)),
                         fallback: bar < 0 ? null : body.slice(bar + 1) });
            i = e + 1;
            continue;
          }
        }
      }
      parts.push({ brace: s[at] });
      i = at + 1;
    }
  }
  /** Whether text has a placeholder, which makes it live. */
  const isLive = (text) => scan(text).some((p) => p.hole);
  /** Text as drawn with no values: each placeholder its fallback or "--", {{ and }} as plain braces. */
  const plain = (text) => scan(text).map((p) => (p.hole ? p.fallback ?? "--" : p.brace ?? p.text)).join("");

  // --- Inputs: the text pass (docs/widgets.md, Substitution) ---------------------------------------------------

  const lookupOf = (input) => (typeof input === "function" ? input
    : input instanceof Map ? (id) => input.get(id)
    : input && typeof input === "object" ? (id) => (Object.hasOwn(input, id) ? input[id] : undefined)
    : () => undefined);

  /** The text pass's tokens: {{ and }} copied as they are, and each reference `{$body}` (ended by `}`, before any
   *  other brace or line break) replaced by `each(body)` when that's a string, else kept. With `limit`, it stops once
   *  it has made more than `limit` UTF-16 units: the start of what it made is then final. */
  function references(text, each, limit = Infinity) {
    const s = String(text ?? "");
    let out = "", i = 0;
    while (i < s.length && out.length <= limit) {
      if (s.startsWith("{{", i) || s.startsWith("}}", i)) {
        out += s.slice(i, i + 2);
        i += 2;
        continue;
      }
      if (s.startsWith("{$", i)) {
        let e = i + 2;
        while (e < s.length && s[e] !== "{" && s[e] !== "}" && s[e] !== "\n") e++;
        if (s[e] === "}") {
          const got = each(s.slice(i + 2, e));
          out += typeof got === "string" ? got : s.slice(i, e + 1);
          i = e + 1;
          continue;
        }
      }
      out += s[i];
      i++;
    }
    return out;
  }

  /** The text pass: each `{$id}` of a text input (`input(id)` gives its value, undefined for anything else)
   *  replaced by that value made safe; any other reference escaped to `{{$id}}`, so it shows as written. (`limit`
   *  is substituteCut's, below.) */
  function substitute(text, input, limit = Infinity) {
    const value = lookupOf(input);
    return references(text, (body) => {
      const v = value(body);
      return typeof v === "string" ? makeSafe(v) : `{{$${body}}}`;
    }, limit);
  }
  /** The text pass, its result cut to `n` code points (docs/widgets.md, Limits), as a copy's values and the text of
   *  what it draws are. The pass stops early, once what it adds can't be in the cut: n code points take at most 2n
   *  units, so the same n come out, without making a text that's far longer first. */
  const substituteCut = (text, input, n) => cut(substitute(text, input, 2 * n), n);

  /** A value made safe to insert: the text pass with no inputs, then written out canonically (each placeholder as
   *  `{` + its body + `}`, every other brace doubled), so it can't join with the text around it. */
  function makeSafe(value) {
    return scan(substitute(String(value ?? ""), null))
      .map((p) => (p.hole ? `{${p.body}}` : p.brace ? p.brace + p.brace : p.text)).join("");
  }

  /** Text with each reference `{$body}` replaced by `fn(body)` when that's a string, kept when it's null: the
   *  page's tool for rewriting a widget's own texts (Delete input). Everything else is copied unchanged. */
  const replaceRefs = (text, fn) => references(text, (body) => fn(body) ?? null);

  // --- Images in a box -----------------------------------------------------------------------------------------

  /** keep_aspect's box (docs/widgets.md, Images in a box): a file of `native` [w, h] (after turning) fitted
   *  inside `box` [x, y, w, h], keeping its shape, centred. */
  function fitAspect(box, native) {
    const [x, y, bw, bh] = box, [fw, fh] = native;
    if (!(fw > 0 && fh > 0)) return [x, y, bw, bh];
    const q = Math.min(bw / fw, bh / fh), vw = Math.max(1, rnd(fw * q)), vh = Math.max(1, rnd(fh * q));
    return [x + Math.floor((bw - vw) / 2), y + Math.floor((bh - vh) / 2), vw, vh];
  }

  // --- Reading a widget --------------------------------------------------------------------------------------------

  /** A widget's content read leniently, as the player reads it: {id, folder (its media refs' prefix), name, w, h,
   *  background, inputs, layers}, or {problem: "broken" | "newer"} when it can't be drawn. */
  function readDef(id, json) {
    if (!json || typeof json !== "object" || Array.isArray(json)) return { problem: "broken" };
    if (typeof json.version === "number" && json.version > 1) return { problem: "newer" };
    const size = (v, max, fallback) => (typeof v === "number" && Number.isFinite(v) ? Math.min(max, Math.max(1, rnd(v))) : fallback);
    const inputs = [], seen = new Set();
    for (const e of Array.isArray(json.inputs) ? json.inputs : []) {
      if (inputs.length >= MAX_INPUTS) break;
      if (!e || typeof e !== "object" || typeof e.id !== "string" || !INPUT_ID_RE.test(e.id) || seen.has(e.id)) continue;
      if (e.type !== "text" && e.type !== "color" && e.type !== "image") continue;
      seen.add(e.id);
      inputs.push({ id: e.id, type: e.type, name: typeof e.name === "string" ? e.name : e.id,
                    default: typeof e.default === "string" ? e.default : "" });
    }
    return { id, folder: `widgets/${id}/`, name: typeof json.name === "string" && json.name ? json.name : id,
             w: size(json.w, 1600, 360), h: size(json.h, 720, 120),
             background: typeof json.background === "string" ? json.background : null,
             inputs, layers: Array.isArray(json.layers) ? json.layers : [] };
  }

  const EMPTY = Object.freeze({ text: new Map(), color: new Map(), image: new Map() });
  /** The values one copy of `def` draws with (docs/widgets.md, A copy's values; Passing inputs down): {text, color,
   *  image} maps of input id to text (substituted in its holder's terms), "#rrggbb" and a media ref ("" for no
   *  image). `values` is the copy's `inputs`, `outer` the scope of the widget around it (none at the top), and
   *  `folder` the refs' prefix of the file the copy is written in ("" for the playlist). */
  function scopeOf(def, values, outer = EMPTY, folder = "") {
    outer = asScope(outer);
    const own = values && typeof values === "object" && !Array.isArray(values) ? values : {};
    const scope = { text: new Map(), color: new Map(), image: new Map() };
    for (const input of def.inputs || []) {
      const v = Object.hasOwn(own, input.id) && typeof own[input.id] === "string" ? own[input.id] : null;
      if (input.type === "text") {
        // a value is cut before its text pass, and again after it, so a value at any depth stays within 2000
        scope.text.set(input.id, v !== null ? substituteCut(cut(v), outer.text, MAX_TEXT) : cut(input.default));
      } else if (input.type === "color") {
        const ref = v === null ? null : refOf(v);
        const c = v === null ? null : ref !== null ? outer.color.get(ref) ?? null : color(v);
        scope.color.set(input.id, c ?? color(input.default) ?? "#ffffff");
      } else {
        let src = null;  // not set: the default
        if (v !== null) {
          const t = spaces(v), ref = refOf(t);
          if (ref !== null) src = outer.image.has(ref) ? outer.image.get(ref) : null;
          else if (t === "") src = "";
          else if (mediaPathOk(t)) src = folder + t;
        }
        const d = spaces(input.default);
        if (src === null) src = mediaPathOk(d) ? def.folder + d : "";
        scope.image.set(input.id, src);
      }
    }
    return scope;
  }
  /** A scope from what a caller holds: scopeOf's answer, or {text, color, image} as plain objects. */
  function asScope(s) {
    if (!s || typeof s !== "object") return EMPTY;
    if (s.text instanceof Map && s.color instanceof Map && s.image instanceof Map) return s;
    const map = (m) => (m instanceof Map ? m : new Map(m && typeof m === "object" ? Object.entries(m) : []));
    return { text: map(s.text), color: map(s.color), image: map(s.image) };
  }

  // --- Expanding copies (docs/widgets.md, Drawing; the player's widget.rs Expander, statement for statement) ------

  const LEAVES = new WeakSet();  // what expansion made, as against a holder's own objects passed through
  const isLeaf = (o) => LEAVES.has(o);
  const meet = (a, b) => [Math.max(a[0], b[0]), Math.max(a[1], b[1]), Math.min(a[2], b[2]), Math.min(a[3], b[3])];
  const FALLBACK = { fill: "#ffffff", stroke: "#ffffff", text: "#ffffff", outline: "#000000" };
  const plainObject = (v) => !!v && typeof v === "object" && !Array.isArray(v);
  // A scene's or Global's own layer's opacity as the player reads it, a 32-bit float: 0.99999999 is 1 there, and
  // 1e-46 is 0. A widget's layers and copies are read as 64-bit numbers, in both.
  const holderOpacity = (layer) => Math.fround(unit(layer?.opacity));

  class Expander {
    constructor(ctx) {
      this.lib = ctx.lib instanceof Map ? ctx.lib : new Map(Object.entries(ctx.lib || {}));
      const [cw, ch] = Array.isArray(ctx.canvas) ? ctx.canvas : [1600, 720];
      this.panel = [0, 0, cw, ch];
      this.left = BUDGET;
      this.defs = new Map();
      this.markDepth = ctx.markDepth ?? 0;  // where the holder's own objects sit: 1 inside the editor's copy
      this.preview = ctx.preview ?? null;   // {copy, scope}: a copy that draws with these values, as they are
      this.top = { folder: ctx.folder || "", scope: asScope(ctx.scope), s: 1, ox: 0, oy: 0, clip: null, groups: [],
                   stack: [], depth: 0, path: "", wid: null, top: undefined, part: undefined };
    }
    /** One step of the budget, or false once it's spent (and from then on). */
    step() {
      if (this.left <= 0) return false;
      this.left--;
      return true;
    }
    def(id) {
      if (!this.defs.has(id)) this.defs.set(id, this.lib.has(id) ? readDef(id, this.lib.get(id)) : null);
      return this.defs.get(id);
    }

    holder(layers) {
      return layers.map((layer) => {
        const entry = { layer, leaves: [], marks: [] };
        if (!plainObject(layer) || layer.visible === false) return entry;
        const reached = holderOpacity(layer) > 0;  // a copy in a layer at 0 isn't reached: it costs nothing
        for (const o of Array.isArray(layer.objects) ? layer.objects : []) {
          if (!plainObject(o)) continue;
          if (o.type === "widget") { if (reached) this.copy(o, this.top, entry.leaves, entry.marks); }
          else if (o.visible !== false) entry.leaves.push(o);  // the holder's own object, as it is
        }
        return entry;
      });
    }

    /** Expands one copy onto `out` (docs/widgets.md: A copy becomes ordinary objects; Geometry; Limits). */
    copy(o, frame, out, marks) {
      const mark = (kind) => {
        if (frame.depth !== this.markDepth) return;
        marks.push({ id: o.id ?? "", kind, widget: typeof o.widget === "string" ? o.widget : "",
                     x: num(o.x, 0), y: num(o.y, 0), w: num(o.w, 0), h: num(o.h, 0) });
      };
      if (!this.step()) return mark("budget");
      const opacity = unit(o.opacity);
      if (o.visible === false || opacity <= 0) return;
      const wid = o.widget;
      if (!idOk(wid)) return mark("missing");
      if (frame.stack.includes(wid)) return mark("cycle");
      if (frame.depth >= MAX_DEPTH) return mark("deep");
      const def = this.def(wid);
      if (!def || def.problem) return mark(def?.problem === "newer" ? "newer" : "missing");
      let scale = o.scale;
      if (typeof scale !== "number" || !(scale > 0)) scale = typeof o.w === "number" && o.w > 0 ? o.w / def.w : 1;
      scale = Math.min(16, Math.max(0.01, scale));
      const S = frame.s * scale;
      const L = frame.ox + rnd(frame.s * num(o.x, 0)), T = frame.oy + rnd(frame.s * num(o.y, 0));
      const box = [L, T, L + Math.max(1, rnd(S * def.w)), T + Math.max(1, rnd(S * def.h))];
      const clip = frame.clip ? meet(box, frame.clip) : box;
      if (clip[2] <= clip[0] || clip[3] <= clip[1]) return;
      const scope = o === this.preview?.copy ? asScope(this.preview.scope) : scopeOf(def, o.inputs, frame.scope, frame.folder);
      const id = cutId(idOf(o)), path = frame.path + id + "/";
      const groups = opacity < 1 ? [...frame.groups, { id: frame.path + id, opacity, rect: meet(clip, this.panel) }] : frame.groups;
      // the page's own objects, as they are: the holder's (`top`), and the one inside its widget (`part`)
      const top = frame.top ?? (frame.depth === this.markDepth ? o.id ?? "" : undefined);
      const part = frame.part ?? (frame.depth === this.markDepth + 1 ? o.id ?? "" : undefined);
      const ref = refOf(def.background), bg = ref !== null ? scope.color.get(ref) ?? null : color(def.background);
      if (bg) {
        if (!this.step()) return;
        out.push(this.made({ id: path + "background", type: "shape", visible: true, opacity: 1, x: box[0], y: box[1],
                             w: box[2] - box[0], h: box[3] - box[1], rotate: 0, flip: false, shape: "rect", radius: 0,
                             fill: { type: "solid", color: bg }, stroke: null, crisp: true, clip, groups, top, part }));
      }
      for (let li = 0; li < def.layers.length; li++) {
        const layer = def.layers[li];
        if (!plainObject(layer) || layer.visible === false || unit(layer.opacity) <= 0) continue;
        const lop = unit(layer.opacity), lid = cutId(typeof layer.id === "string" && layer.id ? layer.id : `l${li}`);
        const child = { folder: def.folder, scope, s: S, ox: L, oy: T, clip, stack: [...frame.stack, wid],
                        depth: frame.depth + 1, path, wid, top, part,
                        groups: lop < 1 ? [...groups, { id: path + lid, opacity: lop, rect: meet(clip, this.panel) }] : groups };
        for (const x of Array.isArray(layer.objects) ? layer.objects : []) {
          if (!plainObject(x)) continue;  // not an object at all: nothing, no step
          if (x.type === "widget") {
            this.copy(x, child, out, marks);
            continue;
          }
          if (x.visible === false) continue;
          if (!this.step()) return;  // out of budget: the rest of this copy is skipped
          const leaf = this.leaf(x, child);
          if (leaf) out.push(leaf);
        }
      }
    }

    made(leaf) {
      LEAVES.add(leaf);
      return leaf;
    }

    /** A widget's own object bound to its copy's values, its defaults filled in, and placed (docs/widgets.md,
     *  Substitution; Geometry). Null when it's left out. */
    leaf(o, f) {
      const type = o.type;
      if (type !== "image" && type !== "video" && type !== "text" && type !== "shape") return null;
      const S = f.s, oid = cutId(idOf(o));
      const x = num(o.x, 0), y = num(o.y, 0), w = num(o.w, 0), h = num(o.h, 0);
      // a bindable colour: `{$id}` of a colour input takes its value; then anything that isn't a colour falls back
      const paint = (v, fallback) => (refOf(v) !== null ? f.scope.color.get(refOf(v)) : color(v)) ?? fallback;
      const edges = () => {
        const x0 = f.ox + rnd(S * x), y0 = f.oy + rnd(S * y), x1 = f.ox + rnd(S * (x + w)), y1 = f.oy + rnd(S * (y + h));
        return [x0, y0, x1 - x0, y1 - y0];
      };
      const leaf = { id: f.path + oid, type, visible: true, opacity: unit(o.opacity), x: 0, y: 0, w: 0, h: 0,
                     rotate: [0, 90, 180, 270].includes(o.rotate) ? o.rotate : 0, flip: o.flip === true,
                     clip: f.clip, groups: f.groups, top: f.top ?? o.id ?? "",
                     part: f.part ?? (f.depth === this.markDepth + 1 ? o.id ?? "" : undefined) };
      if (type === "image" || type === "video") {
        if (typeof o.src !== "string") return null;
        const raw = spaces(o.src), ref = refOf(raw), src = ref !== null ? f.scope.image.get(ref) ?? "" : mediaPathOk(raw) ? f.folder + raw : "";
        if (!src) return null;  // "", a path outside the folder or not to media, or a reference that isn't an image input
        const [x0, y0, dw, dh] = edges();
        Object.assign(leaf, { x: x0, y: y0, w: Math.max(1, dw), h: Math.max(1, dh), src, smooth: o.smooth === true,
                              fill: ["none", "extend", "mirror", "tile"].includes(o.fill) ? o.fill : "none",
                              keep_aspect: o.keep_aspect === true, tint: o.tint === undefined || o.tint === null ? null : paint(o.tint, null) });
        if (typeof o.pixel === "number") leaf.pixel = o.pixel;
        if (leaf.w > MAX_PLACED[0] || leaf.h > MAX_PLACED[1]) return null;
      } else if (type === "text") {
        // filled in, then cut (a copy's text, unlike a scene's own), and only then read for placeholders
        const text = substituteCut(typeof o.text === "string" ? o.text : "", f.scope.text, MAX_LEAF_TEXT);
        // read leniently, as the player reads it: an outline whose width isn't a number is no outline at all
        const outline = plainObject(o.outline) && !("width" in o.outline && typeof o.outline.width !== "number") ? o.outline : {};
        Object.assign(leaf, { x: f.ox + rnd(S * x), y: f.oy + rnd(S * y), text, live: isLive(text),
                              font: typeof o.font === "string" && fontNameOk(o.font) ? `${f.wid}|${o.font}` : "",
                              size: num(o.size, 48) * S, color: "color" in o ? paint(o.color, FALLBACK.text) : FALLBACK.text,
                              align: ["left", "center", "right"].includes(o.align) ? o.align : "left",
                              valign: ["top", "middle", "bottom"].includes(o.valign) ? o.valign : "top",
                              line_height: num(o.line_height, 1.2), crisp: o.crisp === true,
                              outline: { color: "color" in outline ? paint(outline.color, FALLBACK.outline) : "#000000",
                                         width: num(outline.width, 0) * S } });
        delete leaf.w;
        delete leaf.h;
      } else {
        // read leniently, as the player reads it: a fill or stroke with a key of the wrong kind is left at its default.
        // A colour that isn't text (a number, null) isn't a colour, so it takes the fallback like any other
        const wrong = (v, key, kind) => key in v && typeof v[key] !== kind;
        const stops = o.fill?.stops === undefined ? [] : o.fill.stops;
        let fill = { type: "solid", color: "#ffffff" };  // the player's default, also for a fill it can't read
        if (o.fill === null) fill = null;
        else if (plainObject(o.fill) && o.fill.type === "solid") {
          fill = { type: "solid", color: "color" in o.fill ? paint(o.fill.color, FALLBACK.fill) : "#ffffff" };
        } else if (plainObject(o.fill) && o.fill.type === "linear" && !wrong(o.fill, "angle", "number") && Array.isArray(stops)
                   && stops.every((s) => plainObject(s) && !wrong(s, "at", "number"))) {
          fill = { type: "linear", angle: num(o.fill.angle, 0),
                   stops: stops.map((s) => ({ at: num(s.at, 0), color: "color" in s ? paint(s.color, FALLBACK.fill) : "#ffffff" })) };
        }
        const st = plainObject(o.stroke) && !wrong(o.stroke, "width", "number") ? o.stroke : null;
        const [x0, y0, dw, dh] = edges();
        Object.assign(leaf, { x: x0, y: y0, w: dw, h: dh, shape: ["rect", "ellipse", "line"].includes(o.shape) ? o.shape : "rect",
                              radius: num(o.radius, 0) * S, fill, crisp: o.crisp === true,
                              stroke: st ? { color: "color" in st ? paint(st.color, FALLBACK.stroke) : "#000000",
                                             width: num(st.width, 1) * S } : null });
      }
      return this.made(leaf);
    }
  }

  /** A holder's layers with their copies expanded: [{layer, leaves, marks}], one per layer. `ctx` is {lib (id ->
   *  widget content), folder ("" for the playlist, "widgets/<id>/" for a widget), canvas [w, h], scope (what `{$id}`
   *  means at the top, or null), preview ({copy, scope}: one of the copies, drawn with `scope` as its values as they
   *  are: the widget editor's), markDepth (where the holder's own objects sit: 1 inside the editor's copy)}. A hidden
   *  layer has no leaves; the holder's own objects come out as they are; each leaf a copy made is a fresh object in
   *  canvas pixels, with `src` a media ref, `font` a font ref ("<widget>|<file>", or "" for Segoe UI), `clip`,
   *  `groups`, `live`, its id path, `top` (the id of the holder's object it came from) and `part` (that of the object
   *  inside top's widget it came from). `marks` lists the holder's copies that drew nothing, and why: missing, newer,
   *  cycle, deep or budget. */
  function expandLayers(layers, ctx = {}) {
    return new Expander(ctx || {}).holder(Array.isArray(layers) ? layers : []);
  }

  // --- Live text placement (docs/widgets.md, Live text) ---------------------------------------------------------

  const liveLeaf = (l) => l.type === "text" && (isLeaf(l) ? !!l.live : isLive(l.text));

  // Drawn in place, live text keeps the art between live texts as stills, and the player caps what they may hold:
  // past STILLS_CAP pixels, it's drawn on top instead (docs/widgets.md, Live text). Weighed as the player's scene.rs
  // stills_area weighs it, from the leaves alone: each run of leaves between live texts (in a layer, or in a
  // translucent group holding live text, as the player sorts them into its over list) counts the box it may draw in:
  // the hull of its leaves' clips, or the whole layer or group for one without a clip, cut to that layer or group.
  const PANEL = [0, 0, 1600, 720], STILLS_CAP = 16 * 1600 * 720;
  const hull = (a, b) => [Math.min(a[0], b[0]), Math.min(a[1], b[1]), Math.max(a[2], b[2]), Math.max(a[3], b[3])];
  const someArea = (r) => r[0] < r[2] && r[1] < r[3];
  class Weigh {
    constructor() { this.total = 0; this.run = null; }
    add(reach) { this.run = this.run ? hull(this.run, reach) : reach; }
    flush(rect) {
      const r = this.run && meet(this.run, rect);
      if (r && someArea(r)) this.total += (r[2] - r[0]) * (r[3] - r[1]);
      this.run = null;
    }
    group(leaves, depth, rect, parent) {
      this.flush(parent);
      const inner = new Weigh();
      inner.leaves(leaves, depth, rect);
      inner.flush(rect);
      this.total += inner.total;
    }
    leaves(leaves, depth, rect) {
      for (let k = 0; k < leaves.length;) {
        const g = leaves[k].groups?.[depth];
        if (!g) {
          if (liveLeaf(leaves[k])) this.flush(rect);
          else this.add(leaves[k].clip || rect);
          k++;
          continue;
        }
        let end = k + 1;
        while (end < leaves.length && leaves[end].groups?.[depth]?.id === g.id) end++;
        const members = leaves.slice(k, end), r = meet(g.rect, PANEL);
        k = end;
        if (!someArea(r)) continue;
        if (members.some(liveLeaf)) this.group(members, depth + 1, r, rect);
        else this.add(r);
      }
    }
  }
  /** The pixels the stills of an in-place over list would hold, at most: P's layer from S (a translucent one whole,
   *  as a group), then each later layer (one without live text is one piece of art; a translucent one with live text
   *  a group). Hidden layers have no leaves, so weigh nothing. */
  function stillsArea(layers, p, from) {
    // what the player reads: a holder's own object of a type it doesn't know, or media with no src, isn't there
    const read = (list) => list.filter((l) => isLeaf(l) || l.type === "text" || l.type === "shape"
      || ((l.type === "image" || l.type === "video") && typeof l.src === "string" && l.src !== ""));
    const top = new Weigh(), translucent = (e) => holderOpacity(e.layer) < 1;
    if (translucent(layers[p])) top.group(read(layers[p].leaves), 0, PANEL, PANEL);
    else top.leaves(read(layers[p].leaves.slice(from)), 0, PANEL);
    for (const e of layers.slice(p + 1)) {
      const leaves = read(e.leaves);
      if (!leaves.some(liveLeaf)) for (const l of leaves) top.add(l.clip || PANEL);
      else if (translucent(e)) top.group(leaves, 0, PANEL, PANEL);
      else top.leaves(leaves, 0, PANEL);
    }
    top.flush(PANEL);
    return top.total;
  }

  /** Section 5.7's decision over `stacks` (expandLayers' outputs: the scene's, then the global ones): {onTop, cut
   *  (the id of S's first leaf), live (every live leaf, in drawing order), fold (live leaf -> its layer's opacity
   *  times its groups', for drawing it on top)}. `isAnimated(leaf)` says whether a leaf moves. Live text drawn in
   *  place that would keep more than STILLS_CAP pixels of stills goes on top too, as in the player. */
  function placement(stacks, isAnimated) {
    const layers = stacks.flat(), live = [], fold = new Map();
    let p = null;
    layers.forEach((e, li) => e.leaves.forEach((l, i) => {
      if (!liveLeaf(l)) return;
      live.push(l);
      fold.set(l, (l.groups || []).reduce((k, g) => k * g.opacity, holderOpacity(e.layer)));
      p ||= { li, i, l };
    }));
    if (!p) return { onTop: false, cut: null, live, fold };
    const leaves = layers[p.li].leaves;
    let from = p.i, to = p.i + 1;  // S is leaves[from..to) of P's layer
    if (holderOpacity(layers[p.li].layer) < 1) [from, to] = [0, leaves.length];
    else if (p.l.groups?.length) {
      const g = p.l.groups[0].id, inside = (l) => l.groups?.[0]?.id === g;
      while (from > 0 && inside(leaves[from - 1])) from--;
      while (to < leaves.length && inside(leaves[to])) to++;
    }
    const moving = leaves.slice(from).some((l) => isAnimated(l)) || layers.slice(p.li + 1).some((e) => e.leaves.some((l) => isAnimated(l)));
    return { onTop: moving || stillsArea(layers, p.li, from) > STILLS_CAP, cut: leaves[from].id, live, fold };
  }

  // --- Walks over the library ------------------------------------------------------------------------------------

  /** The widgets a widget's content places directly, hidden or not. */
  function children(json) {
    const out = new Set();
    for (const layer of Array.isArray(json?.layers) ? json.layers : []) {
      for (const o of Array.isArray(layer?.objects) ? layer.objects : []) {
        if (o && o.type === "widget" && typeof o.widget === "string") out.add(o.widget);
      }
    }
    return out;
  }
  /** Every widget `id` places, at any depth up to MAX_DEPTH, as a Set. Each widget is walked once, so a cycle ends
   *  and a wide diamond costs one visit per widget. `memo` (a Map the caller empties when the library changes)
   *  keeps answers between calls. */
  function closure(lib, id, memo) {
    if (memo?.has(id)) return memo.get(id);
    const out = new Set(), seen = new Set([id]);
    let level = [id];
    for (let depth = 0; depth < MAX_DEPTH && level.length; depth++) {
      const next = [];
      for (const w of level) {
        for (const c of children(lib.get(w))) {
          out.add(c);
          if (!seen.has(c)) { seen.add(c); next.push(c); }
        }
      }
      level = next;
    }
    memo?.set(id, out);
    return out;
  }
  /** Whether placing `inner` in `outer` would make a cycle: `outer` is `inner`, or places it at any depth. */
  const contains = (lib, outer, inner, memo) => outer === inner || closure(lib, outer, memo).has(inner);
  /** Where each widget is used: id -> [{holder, count, direct}], each holder a scene id, "global" or
   *  "widget:<id>". `direct` counts the holder's own copies of it; `count` adds the copies of widgets placing it. */
  function uses(lib, doc, memo = new Map()) {
    const out = new Map(), holders = [];
    for (const s of Array.isArray(doc?.scenes) ? doc.scenes : []) holders.push([s?.id, s]);
    holders.push(["global", doc?.global]);
    for (const [id, w] of lib) holders.push([`widget:${id}`, w]);
    const add = (wid, holder, count, direct) => {
      if (!out.has(wid)) out.set(wid, []);
      const list = out.get(wid);
      let e = list.find((x) => x.holder === holder);
      if (!e) list.push(e = { holder, count: 0, direct: 0 });
      e.count += count;
      e.direct += direct;
    };
    for (const [key, h] of holders) {
      const direct = new Map();
      for (const layer of Array.isArray(h?.layers) ? h.layers : []) {
        for (const o of Array.isArray(layer?.objects) ? layer.objects : []) {
          if (o && o.type === "widget" && typeof o.widget === "string") direct.set(o.widget, (direct.get(o.widget) || 0) + 1);
        }
      }
      for (const [wid, n] of direct) {
        add(wid, key, n, n);
        for (const inner of closure(lib, wid, memo)) if (inner !== wid) add(inner, key, n, 0);
      }
    }
    return out;
  }

  Object.assign(WV, {
    MAX_DEPTH, BUDGET, MAX_INPUTS, MAX_PLACED, ID_RE, INPUT_ID_RE,
    rnd, idOk, pathOk, fontNameOk, color, refOf, scan, isLive, plain, substitute, makeSafe, replaceRefs, fitAspect,
    readDef, scopeOf, expandLayers, isLeaf, placement, closure, contains, uses,
  });
})();

WV.loaded.widgets = true;
