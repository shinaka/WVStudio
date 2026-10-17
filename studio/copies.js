"use strict";
// Reusable widgets (docs/widgets.md): copies. Placing them (the W tool and its menu, a card dropped on the canvas,
// pasting), a copy's properties (#pCopy) and its input rows, the bind buttons by the properties of a widget's own
// objects, an image's Tint and Picture rows, and the keys W, Ctrl+G and Enter. Loaded after studio.js and library.js,
// whose functions it uses. Nothing here runs while widgets are off (widgetsOn in studio.js): the hooks aren't offered
// then, and every listener here checks it.

(() => {
  const SCALES = [0.05, 8];  // the scales the studio offers a copy (docs/widgets.md, A copy)
  const PICTURES = ".png,.apng,.gif,.webp,.jpg,.jpeg,.bmp,.aseprite,.ase,.mp4,.m4v,.mov,.webm,.mkv";
  const round4 = (v) => Math.round(v * 1e4) / 1e4;
  const isObject = (v) => !!v && typeof v === "object" && !Array.isArray(v);
  const spaces = (s) => String(s).replace(/^ +| +$/g, "");  // whole values are compared with U+0020 trimmed

  // --- The library, as copies see it ------------------------------------------------------------------------

  /** A widget as its copies read it (WV.readDef: {problem} when it can't be drawn), or null when it isn't in the
   *  library. */
  const defOf = (id) => (WV.idOk(id) && lib.has(id) ? WV.readDef(id, lib.get(id)) : null);
  /** A widget's container and inputs, one made by a newer studio too (its copies keep their size), or null. */
  function sizeOf(id) {
    if (!WV.idOk(id) || !isObject(lib.get(id))) return null;
    const def = WV.readDef(id, { ...lib.get(id), version: 1 });
    return def.problem ? null : def;
  }
  /** Whether a copy's widget can be drawn, so it has a container to scale (a missing one keeps its stored box). */
  function copyDrawable(o) {
    const def = defOf(o?.widget);
    return !!def && !def.problem;
  }
  /** Sets a copy's scale, within what the studio offers (5% to 800%, at most 4 decimals), and its stored size, which
   *  older tools and a missing widget's box use (docs/widgets.md, A copy). Its top-left stays where it is. */
  function setCopyScale(o, s) {
    const [aw, ah] = artSize(o);
    o.scale = Math.min(SCALES[1], Math.max(SCALES[0], round4(s)));
    o.w = Math.max(1, WV.rnd(o.scale * aw));
    o.h = Math.max(1, WV.rnd(o.scale * ah));
  }
  /** A copy's scale set with its box's centre kept (whole pixels). */
  function scaleAround(o, s) {
    const b = copyBox(o), cx = b.x + b.w / 2, cy = b.y + b.h / 2;
    setCopyScale(o, s);
    o.x = WV.rnd(cx - o.w / 2);
    o.y = WV.rnd(cy - o.h / 2);
  }
  const valuesOf = (o) => (isObject(o.inputs) ? o.inputs : {});

  /** The open widget, read as its copies read it, when one is open and can be drawn; else null. */
  function openDef() {
    const id = holderWidgetId(), def = id === null ? null : defOf(id);
    return def && !def.problem ? def : null;
  }
  /** The values the open widget's editor draws with (its defaults, or the copy it previews), or null. */
  function openScope() {
    const def = openDef();
    return def ? previewScope(holderWidgetId(), def, view.preview) : null;
  }
  /** The values copy `o` of `def` draws with where it is: at the top, or filled in from the open widget's preview
   *  (docs/widgets.md, Passing inputs down). */
  function copyScope(o, def) {
    const outer = openDef();
    return outer ? WV.scopeOf(def, o.inputs, openScope(), outer.folder) : WV.scopeOf(def, o.inputs, null, "");
  }

  /** Refuses (with the message) placing, pasting or swapping widget `id` into the open widget when that would make a
   *  widget contain itself. */
  function refusesCycle(id) {
    const into = holderWidgetId();
    if (into === null || !WV.contains(lib, id, into, closureMemo())) return false;
    message(id === into ? "A widget can't contain itself."
      : `A widget can't contain itself (${widgetName(id)} contains ${widgetName(into)}).`, "warn");
    return true;
  }
  /** Says so, and is true, when the open widget is read-only. */
  function refusesReadOnly() {
    const id = holderWidgetId();
    if (id === null || !isReadOnly(id)) return false;
    message(`${widgetName(id)} is read-only: duplicate it to edit.`, "warn");
    return true;
  }

  /** A new input's id, made from its name (docs/widgets.md, Inputs): lowercase; runs of anything but a-z and 0-9 as
   *  `_`; `i_` in front when it doesn't start with a letter; at most 32; `_2`, `_3`… when an input of widget `wid`
   *  has it, or any copy of it holds a value for it (so an old value can't come back with a new meaning). */
  function inputIdFor(name, wid) {
    const taken = new Set(), w = lib.get(wid);
    for (const i of Array.isArray(w?.inputs) ? w.inputs : []) if (typeof i?.id === "string") taken.add(i.id);
    for (const h of [doc.global, ...doc.scenes, ...lib.values()]) {
      for (const l of layersOf(h)) {
        for (const o of objectsIn(l)) if (o.type === "widget" && o.widget === wid) for (const k of Object.keys(valuesOf(o))) taken.add(k);
      }
    }
    let stem = String(name ?? "").toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "");
    if (!/^[a-z]/.test(stem)) stem = `i_${stem}`;
    stem = stem.slice(0, 32);
    for (let n = 1; ; n++) {
      const tail = n === 1 ? "" : `_${n}`, id = stem.slice(0, 32 - tail.length) + tail;
      if (!taken.has(id) && WV.INPUT_ID_RE.test(id)) return id;
    }
  }

  // --- Menus ---------------------------------------------------------------------------------------------------
  //
  // The bind buttons', the swap's and the input rows' menus share one popup: [{label, title, disabled, pressed, act}].

  const pop = make("div", "popmenu");
  pop.hidden = true;
  pop.setAttribute("role", "menu");
  document.body.append(pop);
  let popFor = null;
  function openPop(opener, items) {
    if (popFor === opener && !pop.hidden) return closePop();
    pop.replaceChildren(...items.map((it) => {
      const b = make("button", "", it.label);
      b.setAttribute("role", "menuitem");
      if (it.title) b.title = it.title;
      b.disabled = !!it.disabled;
      if (it.pressed) b.setAttribute("aria-pressed", "true");
      b.addEventListener("click", () => { closePop(); it.act(); });
      return b;
    }));
    pop.hidden = false;
    popFor = opener;
    placePop();
  }
  function closePop() {
    pop.hidden = true;
    popFor = null;
  }
  /** The popup under its button (over it when there's no room below), again whenever the panel holding the button
   *  changes or scrolls; closed when the button has gone. */
  function placePop() {
    if (pop.hidden || !popFor) return;
    if (!popFor.isConnected || !popFor.getClientRects().length) return closePop();
    const r = popFor.getBoundingClientRect(), w = pop.offsetWidth, h = pop.offsetHeight;
    pop.style.left = `${Math.max(8, Math.min(r.left, innerWidth - w - 8))}px`;
    pop.style.top = `${r.bottom + 2 + h > innerHeight - 8 ? Math.max(8, r.top - h - 2) : r.bottom + 2}px`;
  }
  document.addEventListener("scroll", placePop, true);
  addEventListener("resize", placePop);

  // --- The W tool and its menu (docs/widgets.md, Placing copies) -----------------------------------------------

  const menu = $("widgetMenu"), wmFilter = make("input"), wmList = make("div", "wm-list");
  wmFilter.type = "search";
  wmFilter.placeholder = "Filter widgets";
  wmFilter.spellcheck = false;
  menu.append(wmFilter, wmList);
  const placing = { id: null, thumbs: "" };  // the widget the tool places, and what the menu's pictures were drawn for

  /** The W tool: its menu opens, to pick the widget the next click places. */
  function openWidgetTool() {
    if (!holder()) return message("Add a scene first.", "warn");
    if (refusesReadOnly()) return;
    setTool("widget");
    placing.id = null;
    wmFilter.value = "";
    fillWidgetMenu();
    menu.hidden = false;
    const tools = $("tools").getBoundingClientRect(), b = $("widgetTool").getBoundingClientRect();
    menu.style.left = `${tools.right + 4}px`;
    menu.style.top = `${Math.max(8, Math.min(b.top, innerHeight - menu.offsetHeight - 8))}px`;
    wmFilter.focus();
  }
  function closeWidgetMenu() {
    if (menu.hidden) return;
    menu.hidden = true;
    if (menu.contains(document.activeElement)) document.activeElement.blur();
    if (tool.name === "widget" && !placing.id) setTool("select");
  }
  /** The menu's widgets, by name, those whose names have every word typed in the filter. Inside widget X, those
   *  that hold X are greyed out. */
  function fillWidgetMenu() {
    const words = wmFilter.value.toLowerCase().split(/\s+/).filter(Boolean), into = holderWidgetId(), memo = closureMemo();
    const ids = [...lib.keys()].filter((id) => sizeOf(id) && words.every((w) => widgetName(id).toLowerCase().includes(w)))
      .sort((a, b) => widgetName(a).localeCompare(widgetName(b)));
    const items = ids.map((id) => {
      const b = make("button", "wm-item"), cv = make("canvas");
      [cv.width, cv.height, cv.dataset.widget] = [128, 64, id];
      b.append(cv, make("span", "", widgetName(id)));
      b.title = `${widgetName(id)} (widgets\\${id})`;
      if (into !== null && WV.contains(lib, id, into, memo)) {
        b.disabled = true;
        b.title = id === into ? "A widget can't contain itself" : `A widget can't contain itself (${widgetName(id)} contains ${widgetName(into)})`;
      }
      b.addEventListener("click", () => armWidget(id));
      return b;
    });
    if (!items.length) items.push(make("div", "wm-empty", lib.size ? "No widget's name has those words." : "No widgets yet: make one in the Widgets tab."));
    wmList.replaceChildren(...items);
    drawMenuThumbs();
    if (liveTexts().join("\n") !== live.asked) resolveSoon();  // the live values their pictures show
  }
  // the live text the menu's pictures show, filled in with the rest while it's open
  WV.texts.push(() => (menu.hidden ? [] : [...wmList.querySelectorAll("canvas[data-widget]")]
    .flatMap((cv) => widgetView(cv.dataset.widget, null).flatMap((e) => e.leaves))
    .filter((l) => l.type === "text" && l.live).map((l) => l.text)));
  const thumbFull = document.createElement("canvas");
  /** Each widget in the menu drawn at its defaults, letterboxed (the checks behind it are its canvas's CSS). */
  function drawMenuThumbs() {
    placing.thumbs = `${libEpoch}|${mediaVersion}|${fontsArrived}|${live.version}`;
    for (const cv of wmList.querySelectorAll("canvas[data-widget]")) {
      const id = cv.dataset.widget, def = sizeOf(id), g = cv.getContext("2d");
      g.clearRect(0, 0, cv.width, cv.height);
      if (!def) continue;
      if (thumbFull.width !== def.w || thumbFull.height !== def.h) [thumbFull.width, thumbFull.height] = [def.w, def.h];
      try {
        withCanvasSize(def.w, def.h, () => renderWidget(thumbFull.getContext("2d"), id, 0, null));
      } catch (err) {  // a picture that can't be drawn: the widget can still be placed
        console.error(err);
        continue;
      }
      const k = Math.min(cv.width / def.w, cv.height / def.h), w = def.w * k, h = def.h * k;
      g.imageSmoothingEnabled = true;
      g.imageSmoothingQuality = "high";
      g.drawImage(thumbFull, (cv.width - w) / 2, (cv.height - h) / 2, w, h);
    }
  }
  /** Picks the widget the W tool places: the next click on the canvas puts a copy there. */
  function armWidget(id) {
    placing.id = id;
    closeWidgetMenu();
    setTool("widget");
    stage.classList.add("place");
    message(`Click the canvas to place ${widgetName(id)}. Shift keeps the tool; Esc cancels.`, "muted");
  }
  /** Puts the W tool down: back to the select tool. */
  function cancelPlacing() {
    placing.id = null;
    closeWidgetMenu();
    if (tool.name === "widget") setTool("select");
  }
  wmFilter.addEventListener("input", fillWidgetMenu);
  wmFilter.addEventListener("keydown", (e) => {  // Enter takes the first widget listed
    if (e.key !== "Enter") return;
    e.preventDefault();
    wmList.querySelector(".wm-item:not(:disabled)")?.click();
  });
  $("widgetTool").addEventListener("click", () => {  // after studio.js's own, which has set the tool
    if (!widgetsOn) return;
    if (menu.hidden) openWidgetTool();
    else { placing.id = null; closeWidgetMenu(); }
  });
  // a click outside closes the menus
  document.addEventListener("pointerdown", (e) => {
    if (!pop.hidden && !pop.contains(e.target) && !popFor?.contains(e.target)) closePop();
    if (!menu.hidden && !menu.contains(e.target) && !$("widgetTool").contains(e.target)) closeWidgetMenu();
  }, true);
  // with the W tool, a click on the canvas places the copy (before the editor's own handler, which never sees it)
  $("stageWrap").addEventListener("pointerdown", (e) => {
    if (!widgetsOn || tool.name !== "widget" || e.button !== 0 || tool.space || view.picking) return;
    e.stopPropagation();
    e.preventDefault();
    if (!placing.id) return openWidgetTool();
    const [x, y] = screenPoint(e, true), id = placing.id;
    if (!e.shiftKey) cancelPlacing();
    placeCopy(id, x, y);
  }, true);

  /** Places a copy of widget `id` centred on (`cx`, `cy`), at scale 1, or smaller when its container is bigger than
   *  the canvas, as one undo step, and selects it. */
  function placeCopy(id, cx, cy) {
    if (!holder()) return message("Add a scene first.", "warn");
    if (refusesReadOnly()) return null;
    const def = sizeOf(id);
    if (!def) return message(`There's no widget ${id} in widgets\\.`, "warn");
    if (refusesCycle(id)) return null;
    const s = Math.max(SCALES[0], Math.min(1, Math.floor(Math.min(W / def.w, H / def.h) * 1e4) / 1e4));
    const w = Math.max(1, WV.rnd(s * def.w)), h = Math.max(1, WV.rnd(s * def.h));
    let made = null;
    edit(() => {
      const layer = targetLayer();
      made = layer && newCopy(id, layer.id, null, cx - w / 2, cy - h / 2, s);
      if (!made) return false;
      sel.ids = [made.id];
      sel.layer = layer.id;
    });
    if (made) message("");
    return made;
  }

  /** Puts a new copy of widget `widgetId` into layer `layerId` of the open holder at `index` (null: on top), with its
   *  top-left at (`x`, `y`), made whole, at `scale` (at most 4 decimals), and returns it; null when there's no such
   *  layer. It has no name: it goes by its widget's. Called inside an `edit`, which it doesn't select in. */
  function newCopy(widgetId, layerId, index, x, y, scale) {
    const layer = layerById(layerId);
    if (!layer) return null;
    const def = sizeOf(widgetId), s = Math.min(SCALES[1], Math.max(SCALES[0], round4(num(scale, 1))));
    const o = { id: uid("w", takenIds()), type: "widget", visible: true, locked: false, opacity: 1, x: WV.rnd(num(x, 0)), y: WV.rnd(num(y, 0)),
                w: def ? Math.max(1, WV.rnd(s * def.w)) : 1, h: def ? Math.max(1, WV.rnd(s * def.h)) : 1, scale: s, widget: widgetId, inputs: {} };
    if (!Array.isArray(layer.objects)) layer.objects = [];
    const at = Number.isInteger(index) ? Math.max(0, Math.min(index, layer.objects.length)) : layer.objects.length;
    layer.objects.splice(at, 0, o);
    return o;
  }

  /** Opens a copy's widget, previewing this copy's values; ← Back returns here. */
  function editWidget(o) {
    if (!WV.idOk(o.widget) || !lib.has(o.widget)) return message(`There's no widget ${o.widget} in widgets\\.`, "warn");
    openHolder(`widget:${o.widget}`, { back: true, preview: { holder: sel.scene, copy: o.id } });
  }

  // --- Bind buttons (docs/widgets.md, Inputs and bind buttons) ----------------------------------------------------

  const binds = new Set();  // every bind button made, refreshed with the properties panel
  /** The colour a colour property shows in the open widget: a bound one's input's (as the editor previews it), else
   *  its own; `fallback` for one that isn't a colour. */
  function shownColour(v, fallback) {
    const ref = WV.refOf(v);
    return (ref !== null ? openScope()?.color.get(ref) : WV.color(v)) ?? fallback;
  }
  /** The file image input `ref` shows in the open widget, as a path in its folder: what its editor previews when
   *  that's one of its own files, else its default. "" for none. */
  function boundFile(ref) {
    const input = openDef()?.inputs.find((i) => i.id === ref && i.type === "image");
    if (!input) return "";
    const shown = openScope()?.image.get(ref) ?? "", own = `widgets/${holderWidgetId()}/`;
    if (shown.startsWith(own)) return shown.slice(own.length);
    return WV.pathOk(spaces(input.default)) ? spaces(input.default) : "";
  }
  /** Adds an input of `type` to the open widget, named `name`, defaulting to `value`, and binds it with `set`, as one
   *  undo step. */
  function newInput(type, name, value, set) {
    const wid = holderWidgetId(), def = openDef();
    if (!def || refusesReadOnly()) return;
    if (def.inputs.length >= WV.MAX_INPUTS) return message(`A widget has at most ${WV.MAX_INPUTS} inputs.`, "warn");
    const id = inputIdFor(name, wid);
    edit(() => {
      const w = lib.get(wid);
      w.inputs = [...(Array.isArray(w.inputs) ? w.inputs : []), { id, name, type, default: value }];
      set(`{$${id}}`);
    });
  }

  /** A bind button for a property of the selected object in a widget (or of the widget itself): `kind` is "text"
   *  (a {$} button: its menu puts an input in at the caret, or makes one from the text), "color" (🔗: binds to a
   *  colour input, or makes one; a bound colour shows a chip, whose ✕ writes the colour back in) or "image" (This
   *  file, an image input, or a new one). `get()` and `set(v)` read and write the property; `set` runs inside an
   *  `edit`. opts: {control (the colour control the chip stands in for), fallback (the colour a value that isn't one
   *  shows; null for a tint), name (a function giving the new input's name)}. Returns its element, which follows the
   *  selection by itself: every props hook refreshes it. */
  function bindButton(kind, get, set, opts = {}) {
    const box = make("span", `bind bind-${kind}`), open = make("button", "bind-open");
    open.textContent = kind === "text" ? "{$}" : kind === "color" ? "🔗" : "This file ▾";
    open.title = { text: "Put an input in the text, or make one from it", color: "Bind this colour to a colour input",
                   image: "This file, or an image input" }[kind] || "";
    const chip = make("span", "chip"), dot = make("span", "", "◆"), name = make("span"), unbind = make("button", "", "✕");
    unbind.title = "Unbind: the colour it shows is written in";
    chip.append(dot, name, unbind);
    chip.hidden = true;
    if (kind === "color") box.append(chip);
    box.append(open);
    const fallback = opts.fallback === undefined ? "#ffffff" : opts.fallback;
    const bindTo = (input) => edit(() => set(`{$${input.id}}`));
    unbind.addEventListener("click", () => { if (!refusesReadOnly()) edit(() => set(shownColour(get(), fallback))); });
    open.addEventListener("click", () => {
      const def = openDef();
      if (!def) return;
      const v = get(), ref = WV.refOf(v), items = [];
      if (kind === "text") {
        for (const i of def.inputs.filter((i) => i.type === "text")) items.push({ label: i.name, title: `Put {$${i.id}} in the text`, act: () => insertKey(`$${i.id}`) });
        let refers = false;  // a default is made safe when it's put in, so a reference in it would show as written
        WV.replaceRefs(typeof v === "string" ? v : "", () => { refers = true; return null; });
        items.push({ label: "New input from this text…", disabled: refers,
                     title: refers ? "This text already uses an input" : "A text input whose default is this text, put in its place",
                     act: () => newInput("text", [...String(opts.name?.() || primary()?.name || "Text")].slice(0, 60).join(""), typeof v === "string" ? v : "", set) });
      } else if (kind === "color") {
        for (const i of def.inputs.filter((i) => i.type === "color")) items.push({ label: i.name, title: `{$${i.id}}`, pressed: ref === i.id, act: () => bindTo(i) });
        items.push({ label: "New colour input from this colour", title: "A colour input whose default is this colour, bound here",
                     act: () => newInput("color", opts.name?.() || "Colour", shownColour(v, fallback) ?? "#ffffff", set) });
      } else {
        items.push({ label: "This file", pressed: ref === null, title: "The picture's own file",
                     act: () => { if (ref !== null) edit(() => set(boundFile(ref))); } });
        for (const i of def.inputs.filter((i) => i.type === "image")) items.push({ label: i.name, title: `{$${i.id}}`, pressed: ref === i.id, act: () => bindTo(i) });
        items.push({ label: "New image input from this file", title: "An image input whose default is this file, used here",
                     act: () => newInput("image", opts.name?.() || "Picture", ref !== null ? boundFile(ref) : WV.pathOk(spaces(v ?? "")) ? spaces(v) : "", set) });
      }
      openPop(open, items);
    });
    box.update = () => {
      const v = get(), ref = isWidgetHolder() ? WV.refOf(v) : null;
      const input = ref !== null ? openDef()?.inputs.find((i) => i.id === ref) : null;
      if (kind === "color") {
        chip.hidden = ref === null;
        name.textContent = input?.type === "color" ? input.name : `{$${ref}}`;
        chip.classList.toggle("broken", input?.type !== "color");
        chip.title = input?.type === "color" ? `Bound to the colour input ${input.name} ({$${ref}})` : "Bound to no colour input: it shows the fallback";
        dot.style.color = ref !== null ? shownColour(v, "") : "";
        opts.control?.classList.toggle("bound-hide", ref !== null);
        box.style.display = opts.control?.hidden ? "none" : "";  // a fill that's none has no colour to bind
      } else if (kind === "image") {
        open.textContent = `${ref === null ? "This file" : input?.type === "image" ? input.name : `{$${ref}}`} ▾`;
      }
    };
    binds.add(box);
    return box;
  }

  /** A shape's fill colour as its controls show it: a solid fill's, or a gradient's first stop's (`last`: its last). */
  function fillColour(o, last) {
    const f = o?.fill;
    if (f?.type === "linear") return stopOf(f, last)?.color;
    return last ? undefined : f?.color;
  }
  /** The gradient stop a fill control shows (as gradientStops orders them): the first along it, or the last. */
  function stopOf(f, last) {
    const stops = (Array.isArray(f.stops) ? f.stops : []).filter(isObject), at = (s) => Math.min(1, Math.max(0, num(s.at, 0)));
    if (!stops.length) return null;
    return stops.reduce((a, b) => ((last ? at(b) >= at(a) : at(b) < at(a)) ? b : a));
  }
  function setFillColour(o, last, v) {
    if (o.fill?.type === "linear") { const s = stopOf(o.fill, last); if (s) s.color = v; }
    else if (!last) o.fill = { type: "solid", color: v };
  }

  // --- An image's Tint, Picture and Keep its shape (docs/widgets.md, Images in a box) ------------------------------

  function showMediaRows(p) {
    const on = !!p && isMedia(p);
    $("mTintRow").hidden = !on;
    if (!on) return;
    const tint = WV.color(p.tint), bound = isWidgetHolder() && WV.refOf(p.tint) !== null;
    $("mTintNone").setAttribute("aria-pressed", !tint && !bound);
    set("mTint", tint ?? "#ffffff");
    $("mKeepAspect").checked = p.keep_aspect === true;
    const ref = isWidgetHolder() ? WV.refOf(p.src) : null;
    if (ref === null) return;
    // a picture bound to an input has no file of its own: what the editor shows is the input's
    const input = openDef()?.inputs.find((i) => i.id === ref && i.type === "image"), file = input ? openScope()?.image.get(ref) ?? "" : "";
    const info = file ? getMedia(file)?.info : null;
    $("mInfo").textContent = !input ? `{$${ref}} isn't an image input of this widget: nothing is drawn.`
      : !file ? `${input.name} has no picture: nothing is drawn.`
      : `The picture of the input ${input.name}${info ? `: ${info.width} × ${info.height} file` : ""}.`;
  }
  $("mTint").addEventListener("input", () => {
    const p = primary();
    if (widgetsOn && p && isMedia(p)) edit(() => { find(p.id).obj.tint = $("mTint").value; }, "mTint");
  });
  $("mTintNone").addEventListener("click", () => {
    const p = primary();
    if (widgetsOn && p && isMedia(p)) edit(() => { delete find(p.id).obj.tint; });
  });
  $("mKeepAspect").addEventListener("change", () => {
    const p = primary();
    if (!widgetsOn || !p || !isMedia(p)) return;
    edit(() => { const o = find(p.id).obj; if ($("mKeepAspect").checked) o.keep_aspect = true; else delete o.keep_aspect; });
  });
  // In a widget, a gradient's colour control changes only its own stop: the editor's own handler writes every stop
  // through gradientStops, which would turn the other stop's {$id} white.
  $("pShape").addEventListener("input", (e) => {
    const last = e.target.id === "shColor2";
    if (!widgetsOn || !isWidgetHolder() || (e.target.id !== "shColor" && !last)) return;
    const p = primary();
    if (p?.fill?.type !== "linear") return;
    e.stopPropagation();
    edit(() => setFillColour(find(p.id).obj, last, e.target.value), e.target.id);
  }, true);

  // --- A copy in #pObject: its size sets its scale, and with no name of its own it goes by its widget's ------------

  $("pObject").addEventListener("change", (e) => {
    const id = e.target.id, p = primary();
    if (!widgetsOn || (id !== "oW" && id !== "oH") || p?.type !== "widget" || !copyDrawable(p)) return;
    e.stopPropagation();
    const v = parseFloat(e.target.value), def = defOf(p.widget);
    if (!(v >= 1)) return showProps();
    edit(() => setCopyScale(find(p.id).obj, v / (id === "oW" ? def.w : def.h)));
  }, true);
  $("pObject").addEventListener("input", (e) => {
    const p = primary();
    if (!widgetsOn || e.target.id !== "oName" || p?.type !== "widget") return;
    e.stopPropagation();
    const v = e.target.value;
    edit(() => { const o = find(p.id).obj; if (v) o.name = v; else delete o.name; }, "oName");
  }, true);
  function showCopyObject(p) {
    $("oName").placeholder = p?.type === "widget" ? widgetName(p.widget) : "";
    if (p?.type !== "widget" || !copyDrawable(p)) return;
    const b = copyBox(p);
    set("oW", b.w);
    set("oH", b.h);
  }

  // --- Input rows (docs/widgets.md, A copy's properties) -----------------------------------------------------------

  /** Uploads a picture for an image input: into the open widget's media\, or the playlist's inputs\ (a copy in a
   *  scene or Global). Resolves to its path there, or null. */
  async function uploadPicture(file) {
    if (refusesReadOnly()) return null;
    const id = holderWidgetId();
    message(`Opening ${file.name}…`);
    try {
      const url = id !== null ? `/api/widgets/upload?id=${encodeURIComponent(id)}&name=${encodeURIComponent(file.name)}`
        : `/api/upload?name=${encodeURIComponent(file.name)}&to=inputs`;
      const res = await api(url, { method: "POST", body: file }), info = await res.json();
      if (!res.ok) throw new Error(info.error || res.statusText);
      message("");
      return info.src;
    } catch (err) {
      message(`Couldn't open ${file.name}: ${err.message}`, "warn");
      return null;
    }
  }
  /** A media file's first frame in a small canvas, letterboxed. */
  function drawThumb(cv, ref) {
    const g = cv.getContext("2d"), m = ref ? getMedia(ref) : null;
    g.clearRect(0, 0, cv.width, cv.height);
    const f = m?.ready && m.frames[0];
    if (!f) return;
    const fw = f.videoWidth || f.width, fh = f.videoHeight || f.height;
    if (!(fw > 0 && fh > 0)) return;
    const k = Math.min(cv.width / fw, cv.height / fh);
    g.imageSmoothingEnabled = k < 1;
    g.drawImage(f, (cv.width - fw * k) / 2, (cv.height - fh * k) / 2, fw * k, fh * k);
  }

  /** One input's row: its name, a control for its value, and a note under it. `value` is what's drawn with,
   *  `isDefault` whether that's the widget's default (shown muted), and `onChange(v)` takes a new value: text as
   *  typed (each keystroke), a colour, a picture's path in the open holder ("" for none), `{$id}` from the 🔗 menu,
   *  or null from ↺ (back to the default). opts: {reset: show ↺; pass: a 🔗 menu of the open widget's inputs of this
   *  type, which writes `{$id}` (a copy inside a widget passing an input down); ref: the media ref an image value is
   *  drawn from (else the value in the open holder)}. Text has live-key autocomplete, and with `pass`, `{$` lists the
   *  open widget's text inputs. Choose… and a file dropped on the picture upload into the open widget's media\, or
   *  the playlist's inputs\. The row's update(value, isDefault, ref) shows another value in place (a field being
   *  typed in is left as it is), and note(text) sets the line under it. */
  function inputRow(input, value, isDefault, onChange, opts = {}) {
    const row = make("div", `input-row ${input.type}`), label = make("span", "label", String(input.name ?? input.id));
    label.title = `{$${input.id}}`;
    const field = make("span", "field"), tools = make("span", "tools"), note = make("div", "note small muted");
    row.append(label, field, tools, note);
    let show, ref = opts.ref;
    // a value set by a button (↺, None, the 🔗 menu) shows at once: a field being typed in would keep its own
    const put = (v) => { if (row.contains(document.activeElement)) document.activeElement.blur(); onChange(v); };
    if (input.type === "text") {
      const ta = make("textarea");
      [ta.rows, ta.spellcheck] = [1, false];
      ta.title = "Written like text in a text object: {key} shows a live value";
      ta.addEventListener("input", () => { ta.rows = Math.min(6, ta.value.split("\n").length); onChange(ta.value); });
      acAttach(ta, { get: () => ta.value, set: (v) => onChange(v), inputs: opts.pass ? openWidgetTextInputs : undefined });
      field.append(ta);
      field.style.display = "block";
      show = (v) => {
        if (document.activeElement === ta) return;
        ta.value = String(v ?? "");
        ta.rows = Math.min(6, ta.value.split("\n").length);
      };
    } else if (input.type === "color") {
      const c = make("input"), chip = make("span", "chip");
      c.type = "color";
      c.addEventListener("input", () => onChange(c.value));
      field.append(c, chip);
      show = (v) => {
        const bound = WV.refOf(v), passed = bound !== null && openDef()?.inputs.find((i) => i.id === bound);
        chip.hidden = bound === null;
        chip.textContent = `◆ ${passed ? passed.name : `{$${bound}}`}`;
        c.hidden = bound !== null;
        if (document.activeElement !== c) c.value = WV.color(v) ?? "#ffffff";
      };
    } else {
      const thumb = make("canvas", "thumb"), pick = make("input"), choose = make("button", "", "Choose…"), none = make("button", "", "None");
      const chip = make("span", "chip");
      [thumb.width, thumb.height, thumb.title] = [96, 64, "Drop a picture here to use it"];
      [pick.type, pick.accept, pick.hidden] = ["file", PICTURES, true];
      choose.title = isWidgetHolder() ? "A picture for this input, kept in the widget's media folder" : "A picture for this input, kept in the playlist's inputs folder";
      none.title = "No picture: nothing is drawn";
      const take = async (file) => { if (file) { const src = await uploadPicture(file); if (src !== null) onChange(src); } };
      pick.addEventListener("change", () => { const f = pick.files[0]; pick.value = ""; take(f); });
      choose.addEventListener("click", () => pick.click());
      none.addEventListener("click", () => put(""));
      thumb.addEventListener("dragover", (e) => { if (e.dataTransfer?.types.includes("Files")) e.preventDefault(); });
      thumb.addEventListener("drop", (e) => {  // here, not the canvas: the window's own drop would place it as an image
        e.preventDefault();
        e.stopPropagation();
        document.body.classList.remove("dragover");
        take(e.dataTransfer.files[0]);
      });
      field.append(thumb, chip, choose, none, pick);
      show = (v) => {
        const bound = WV.refOf(v), passed = bound !== null && openDef()?.inputs.find((i) => i.id === bound);
        chip.hidden = bound === null;
        chip.textContent = `◆ ${passed ? passed.name : `{$${bound}}`}`;
        drawThumb(thumb, ref ?? (typeof v === "string" && spaces(v) && bound === null ? mediaRef(spaces(v)) : ""));
      };
      row.redraw = () => show(row.value);
    }
    if (opts.pass) {
      const link = make("button", "", "🔗");
      link.title = "Pass down one of this widget's inputs";
      link.addEventListener("click", () => {
        const mine = (openDef()?.inputs || []).filter((i) => i.type === input.type);
        openPop(link, mine.length ? mine.map((i) => ({ label: i.name, title: `{$${i.id}}`, pressed: WV.refOf(row.value) === i.id, act: () => put(`{$${i.id}}`) }))
          : [{ label: `This widget has no ${{ text: "text", color: "colour", image: "image" }[input.type]} inputs`, disabled: true, act: () => {} }]);
      });
      tools.append(link);
    }
    let reset = null;
    if (opts.reset) {
      reset = make("button", "", "↺");
      reset.title = "Back to the widget's default";
      reset.addEventListener("click", () => put(null));
      tools.append(reset);
    }
    row.update = (v, dflt, r) => {
      row.value = v;
      if (r !== undefined) ref = r;
      row.classList.toggle("is-default", !!dflt);
      if (reset) reset.disabled = !!dflt;
      show(v);
    };
    row.note = (text) => { note.textContent = text || ""; };
    row.update(value, isDefault, opts.ref);
    return row;
  }

  // --- A copy's properties (#pCopy) ----------------------------------------------------------------------------------

  const panel = { key: "", rows: new Map(), notes: "" };
  /** What a copy's row shows for `input`: its own value, or the default when it sets none (a colour or picture
   *  that isn't one counts as none, and a reference only means something inside a widget), and the media ref its
   *  picture is drawn from. */
  function rowState(o, input, scope) {
    const own = valuesOf(o), v = typeof own[input.id] === "string" ? own[input.id] : null, inside = isWidgetHolder();
    let mine = v !== null;
    if (mine && input.type === "color") mine = WV.color(v) !== null || (inside && WV.refOf(v) !== null);
    if (mine && input.type === "image") mine = spaces(v) === "" || WV.pathOk(spaces(v)) || (inside && WV.refOf(v) !== null);
    return { value: mine ? v : input.default, isDefault: !mine, ref: input.type === "image" ? scope.image.get(input.id) ?? "" : undefined };
  }
  /** Whether a value is the input's default, so the copy needn't hold it (and a later change to the default reaches
   *  it). A picture is the default when it's the same file. */
  function isDefaultValue(o, input, v) {
    if (input.type === "text") return v === input.default;
    if (input.type === "color") return WV.color(v) !== null && WV.color(v) === WV.color(input.default);
    const t = spaces(v), d = spaces(input.default), folder = isWidgetHolder() ? `widgets/${holderWidgetId()}/` : "";
    return t === "" ? d === "" : WV.pathOk(t) && folder + t === `widgets/${o.widget}/${d}`;
  }
  /** Sets (or with null, clears) one of a copy's values. Typing in a row is one undo step per pause. */
  function setCopyValue(copyId, input, v) {
    edit(() => {
      const o = find(copyId)?.obj;
      if (!o) return false;
      if (!isObject(o.inputs)) o.inputs = {};
      if (v === null || isDefaultValue(o, input, v)) delete o.inputs[input.id];
      else o.inputs[input.id] = v;
    }, v === null ? "" : `copy:${copyId}:${input.id}`);
  }
  /** Whether a widget, or one it holds, has an image that isn't smooth, whose art pixels an in-between scale makes
   *  uneven. */
  function unevenArt(id) {
    for (const w of [id, ...WV.closure(lib, id, closureMemo())]) {
      for (const l of layersOf(lib.get(w))) if (objectsIn(l).some((x) => isMedia(x) && x.smooth !== true)) return true;
    }
    return false;
  }

  /** Fills #pCopy for the selected copy `o` (docs/widgets.md, A copy's properties): its widget (a menu to swap it),
   *  Edit widget, its scale, and a row per input. The rows are made again only when the inputs change, so a field
   *  being typed in keeps its caret. */
  function showCopyProps(o) {
    const def = defOf(o.widget), ok = !!def && !def.problem;
    $("cpWidgetName").textContent = typeof o.widget === "string" ? (lib.has(o.widget) ? widgetName(o.widget) : o.widget) : "?";
    $("cpEdit").disabled = !WV.idOk(o.widget) || !lib.has(o.widget);
    $("cpScaleRow").hidden = !ok;
    const s = ok ? copyScale(o, def) : 1;
    $("cpScale").textContent = `${+(s * 100).toFixed(2)}%`;
    $("cpSmaller").disabled = s <= SCALES[0];
    $("cpBigger").disabled = s >= SCALES[1];
    $("cpNote").textContent = ok && !Number.isInteger(round4(s)) && unevenArt(o.widget) ? "in-between size: art pixels come out uneven" : "";
    $("cpMessage").textContent = !ok ? (def?.problem === "newer" ? "Made by a newer studio: update this studio to use it."
                                                                  : "This widget isn't in widgets\\. Copy its folder there, or import it.")
      : !def.inputs.length ? "This widget sets its own text and pictures. Edit widget to change them." : "";
    $("cpInputsHead").hidden = !ok || !def.inputs.length;
    if (!ok || !def.inputs.length) {
      $("cpInputs").replaceChildren();
      [panel.key, panel.rows] = ["", new Map()];
      return;
    }
    const scope = copyScope(o, def), key = JSON.stringify([sel.scene, o.id, o.widget, def.inputs, openDef()?.inputs ?? null]);
    if (key !== panel.key) {
      panel.key = key;
      panel.rows = new Map(def.inputs.map((input) => {
        const st = rowState(o, input, scope);
        return [input.id, inputRow(input, st.value, st.isDefault, (v) => setCopyValue(o.id, input, v), { reset: true, pass: isWidgetHolder(), ref: st.ref })];
      }));
      $("cpInputs").replaceChildren(...panel.rows.values());
    } else {
      for (const input of def.inputs) {
        const st = rowState(o, input, scope);
        panel.rows.get(input.id)?.update(st.value, st.isDefault, st.ref);
      }
    }
    refreshCopyNotes(true);
  }
  /** The notes under a copy's rows: what a live text shows now ("shows 52°C"), and "cut off by the widget's edge"
   *  when its text runs past the container. Kept up to date as values, fonts and media arrive. */
  function refreshCopyNotes(force = false) {
    const o = primary();
    if (!widgetsOn || o?.type !== "widget" || $("pCopy").hidden || !panel.rows.size || drag) return;
    const k = [panel.key, docEpoch, libEpoch, fontsArrived, mediaVersion, live.version].join("|");
    if (!force && k === panel.notes) return;
    panel.notes = k;
    const def = defOf(o.widget);
    if (!def || def.problem) return;
    const scope = copyScope(o, def), cut = cutOffInputs(sel.scene, o.id);
    for (const input of def.inputs) {
      const row = panel.rows.get(input.id), notes = [];
      if (!row) continue;
      if (input.type === "text") {
        const t = scope.text.get(input.id) ?? "";
        if (WV.isLive(t) && live.resolved.has(t)) notes.push(`shows ${live.resolved.get(t)}`);
        if (cut.has(input.id)) notes.push("cut off by the widget's edge");
      }
      row.note(notes.join(" · "));
      row.redraw?.();
    }
    if (liveTexts().join("\n") !== live.asked) resolveSoon();
  }
  // a copy's text values are filled in with live values too, for the "shows" notes
  WV.texts.push(() => {
    const o = primary(), def = o?.type === "widget" ? defOf(o.widget) : null;
    if (!def || def.problem || $("pCopy").hidden) return [];
    const scope = copyScope(o, def);
    return def.inputs.filter((i) => i.type === "text").map((i) => scope.text.get(i.id));
  });

  $("cpEdit").addEventListener("click", () => { const p = primary(); if (widgetsOn && p?.type === "widget") editWidget(p); });
  $("cpWidget").addEventListener("click", () => {  // swap: values for inputs with the same ids carry over
    const p = primary();
    if (!widgetsOn || p?.type !== "widget") return;
    const into = holderWidgetId(), memo = closureMemo();
    const ids = [...lib.keys()].filter((id) => sizeOf(id)).sort((a, b) => widgetName(a).localeCompare(widgetName(b)));
    openPop($("cpWidget"), ids.length ? ids.map((id) => {
      const cycle = into !== null && WV.contains(lib, id, into, memo);
      return { label: widgetName(id), pressed: id === p.widget, disabled: cycle,
               title: cycle ? `A widget can't contain itself (${widgetName(id)} contains ${widgetName(into)})` : `widgets\\${id}`,
               act: () => swapCopy(p.id, id) };
    }) : [{ label: "No widgets yet", disabled: true, act: () => {} }]);
  });
  /** Swaps a copy to widget `to`, keeping its scale and its values. A name that was its old widget's goes, so it's
   *  shown under the new one's. */
  function swapCopy(copyId, to) {
    if (refusesReadOnly() || refusesCycle(to)) return;
    edit(() => {
      const o = find(copyId)?.obj;
      if (!o || o.widget === to) return false;
      const was = defOf(o.widget), s = was && !was.problem ? copyScale(o, was) : typeof o.scale === "number" && o.scale > 0 ? o.scale : 1;
      if (o.name === widgetName(o.widget)) delete o.name;
      o.widget = to;
      setCopyScale(o, s);
    });
  }
  /** − and +: the next whole scale down or up (below 1, half and double); 1×: the widget's own size. The copy's
   *  centre stays put. */
  function stepScale(dir) {
    const p = primary();
    if (!widgetsOn || p?.type !== "widget" || !copyDrawable(p)) return;
    edit(() => {
      const o = find(p.id).obj, s = copyScale(o, defOf(o.widget));
      const next = dir === 0 ? 1 : dir > 0 ? (s < 1 - 1e-6 ? Math.min(1, s * 2) : Math.floor(s + 1e-6) + 1)
        : s > 1 + 1e-6 ? Math.ceil(s - 1e-6) - 1 : s / 2;
      scaleAround(o, next);
    });
  }
  $("cpSmaller").addEventListener("click", () => stepScale(-1));
  $("cpBigger").addEventListener("click", () => stepScale(1));
  $("cpOne").addEventListener("click", () => stepScale(0));

  // --- Pasting (docs/widgets.md, Placing copies) ----------------------------------------------------------------------

  /** Where Ctrl+C takes objects from, for the clipboard: {from: "playlist" or "widget:<id>", scope: the values the
   *  widget's editor drew them with, as plain objects}, so a paste elsewhere fills their references in. */
  function clipFrom() {
    const id = holderWidgetId(), def = id === null ? null : defOf(id);
    if (id === null) return { from: "playlist", scope: null };
    const s = def && !def.problem ? previewScope(id, def, view.preview) : null, plain = (m) => Object.fromEntries(m);
    return { from: `widget:${id}`, scope: s && { text: plain(s.text), color: plain(s.color), image: plain(s.image) } };
  }
  const asScope = (s) => ({ text: new Map(Object.entries(isObject(s?.text) ? s.text : {})),
                            color: new Map(Object.entries(isObject(s?.color) ? s.color : {})),
                            image: new Map(Object.entries(isObject(s?.image) ? s.image : {})) });
  /** A media ref's holder and its path there: ["playlist", "sources/x.png"], ["widget:<id>", "media/x.png"]. */
  function placeOf(ref) {
    if (/^(sources|inputs)\//.test(ref)) return ["playlist", ref];
    const m = /^widgets\/([^/]+)\/(.+)$/.exec(ref);
    return m ? [`widget:${m[1]}`, m[2]] : null;
  }
  /** A path written in holder `source` (a widget's id, or null for the playlist) as a media ref, or null. */
  function refIn(path, source) {
    if (typeof path !== "string") return null;
    const p = spaces(path);
    if (source === null) return /^(sources|inputs)\/[^/]+$/.test(p) ? p : null;
    return WV.pathOk(p) ? `widgets/${source}/${p}` : null;
  }

  /** Readies one object copied in `from` (`source`: its widget's id, or null) for holder `to`. Leaving a widget, every
   *  reference to its inputs is filled in from `scope`, what its editor showed: text by the text pass, a bound colour
   *  or tint by the input's colour (or the fallback it showed), a bound picture by the input's file, a nested copy's
   *  values likewise. Each file and font that has to come along is added to `jobs`. False when it drew nothing (its
   *  picture input was empty), so it isn't pasted. */
  function readyFor(o, from, source, to, scope, jobs) {
    if (scope) {
      const paint = (v, fallback) => { const r = WV.refOf(v); return r === null ? v : scope.color.get(r) ?? fallback; };
      if (o.type === "text") {
        if (typeof o.text === "string") o.text = WV.substitute(o.text, scope.text);
        if ("color" in o) o.color = paint(o.color, "#ffffff");
        if (isObject(o.outline) && "color" in o.outline) o.outline.color = paint(o.outline.color, "#000000");
      } else if (o.type === "shape") {
        if (isObject(o.fill) && "color" in o.fill) o.fill.color = paint(o.fill.color, "#ffffff");
        for (const st of Array.isArray(o.fill?.stops) ? o.fill.stops : []) if (isObject(st) && "color" in st) st.color = paint(st.color, "#ffffff");
        if (isObject(o.stroke) && "color" in o.stroke) o.stroke.color = paint(o.stroke.color, "#ffffff");
      } else if (isMedia(o) && "tint" in o) o.tint = paint(o.tint, null);
    }
    const file = (ref, as, put, lost) => {  // where it lands: as it is when it's there already, else copied over
      const own = to === "playlist" ? (as === "input" ? "inputs/" : "sources/") : `widgets/${to.slice(7)}/`;
      if (ref.startsWith(own)) put(to === "playlist" ? ref : ref.slice(own.length));
      else jobs.push({ place: placeOf(ref), as, put, lost });
    };
    if (isMedia(o)) {
      const bound = scope ? WV.refOf(o.src) : null, ref = bound !== null ? scope.image.get(bound) ?? "" : refIn(o.src, source);
      if (ref === "") return false;
      if (ref !== null) file(ref, "object", (p) => { o.src = p; }, () => { o.lost = true; });
    }
    if (o.type === "widget" && isObject(o.inputs)) {
      const inner = defOf(o.widget), types = new Map((inner && !inner.problem ? inner.inputs : []).map((i) => [i.id, i.type]));
      for (const [k, v] of Object.entries(o.inputs)) {
        if (typeof v !== "string") continue;
        const bound = scope ? WV.refOf(v) : null;
        // its widget missing: a reference goes by what it names, a path by its look
        const type = types.get(k) ?? (bound !== null ? (scope.color.has(bound) ? "color" : scope.image.has(bound) ? "image" : "text")
          : refIn(v, source) && /\.[a-z0-9]+$/i.test(v) ? "image" : "text");
        if (type === "text") { if (scope) o.inputs[k] = WV.substitute(v, scope.text); continue; }
        if (type === "color") {
          if (bound === null) continue;
          if (scope.color.has(bound)) o.inputs[k] = scope.color.get(bound);
          else delete o.inputs[k];  // not a colour input: it wasn't set
          continue;
        }
        if (bound !== null && !scope.image.has(bound)) { delete o.inputs[k]; continue; }
        const ref = bound !== null ? scope.image.get(bound) : refIn(v, source);
        if (ref === "") o.inputs[k] = "";
        else if (ref !== null) file(ref, "input", (p) => { o.inputs[k] = p; }, () => { delete o.inputs[k]; });
      }
    }
    if (o.type === "text" && typeof o.font === "string") {  // a scene names a font by its path, a widget by its file name
      const plainFont = !o.font || o.font.toLowerCase() === DEFAULT_FONT.toLowerCase();
      if (source === null && plainFont) o.font = "";
      else if (to === "playlist" && plainFont) o.font = DEFAULT_FONT;
      else {
        const was = o.font, name = was.split(/[\\/]/).pop();
        jobs.push({ place: [from, null], font: was, put: (n) => { o.font = n; },
                    lost: () => { if (source === null) o.font = WV.fontNameOk(name) ? name : ""; } });
      }
    }
    return true;
  }

  /** Pastes copied objects ({wvObjects, from, scope}, from this page or another tab) into the open holder, as one
   *  step, selected. Pasted into another holder, they first take what they need (docs/widgets.md, Placing copies):
   *  references to the widget they leave filled in, then every picture, picture value and font they use copied into
   *  this holder's folder (POST /api/media/copy). A copy of a widget that holds this one is refused; one of a widget
   *  this studio doesn't have pastes anyway, and shows as missing. */
  async function pasteCopied(clip, dx = 12, dy = 12) {
    if (refusesReadOnly()) return;
    const key = sel.scene, to = isWidgetHolder() ? key : "playlist";
    const from = typeof clip.from === "string" && (clip.from === "playlist" || (clip.from.startsWith("widget:") && WV.idOk(clip.from.slice(7))))
      ? clip.from : "playlist";
    const source = from === "playlist" ? null : from.slice(7);
    let list = (Array.isArray(clip.wvObjects) ? clip.wvObjects : []).filter((o) => isObject(o) && typeof o.type === "string" && o.type)
      .map((o) => structuredClone(o));
    for (const o of list) if (o.type === "widget" && refusesCycle(o.widget)) return;
    const jobs = [], any = list.length;
    if (from !== to) list = list.filter((o) => readyFor(o, from, source, to, source !== null ? asScope(clip.scope) : null, jobs));
    if (jobs.length) {
      const groups = new Map(), missing = [];
      for (const j of jobs) {
        const gone = j.place && j.place[0] !== "playlist" && !lib.has(j.place[0].slice(7));  // copied in another studio
        if (gone) missing.push(`widgets\\${j.place[0].slice(7)} isn't here`);
        if (!j.place || gone) { j.lost(); continue; }
        const k = `${j.place[0]}|${j.as || ""}`;
        if (!groups.has(k)) groups.set(k, { from: j.place[0], as: j.as, files: new Set(), fonts: new Set(), jobs: [] });
        const g = groups.get(k);
        if (j.font !== undefined) g.fonts.add(j.font);
        else g.files.add(j.place[1]);
        g.jobs.push(j);
      }
      message("Copying the files it uses…", "muted");
      try {
        for (const g of groups.values()) {
          const out = await postJson("/api/media/copy", { from: g.from, to, files: [...g.files], fonts: [...g.fonts],
                                                          ...(g.as === "input" ? { as: "input" } : {}) });
          for (const j of g.jobs) {
            const got = j.font !== undefined ? out.fonts?.[j.font] : out.files?.[j.place[1]];
            if (typeof got === "string" && got) j.put(got);
            else j.lost();
          }
          for (const m of Array.isArray(out.missing) ? out.missing : []) missing.push(`${m.file}: ${m.why}`);
        }
      } catch (err) {
        return message(`Nothing was pasted: couldn't copy the files it uses (${err.message}).`, "warn");
      }
      if (sel.scene !== key) return message("Nothing was pasted: another scene or widget was opened while its files were copied.", "warn");
      list = list.filter((o) => !o.lost);
      const notes = [...new Set(missing)];
      message(notes.length ? `Pasted without what couldn't be copied: ${notes.slice(0, 3).join("; ")}${notes.length > 3 ? "…" : ""}` : "",
              notes.length ? "warn" : "");
    }
    if (!list.length) return any ? message("Nothing was pasted: the pictures it showed were empty.", "warn") : undefined;
    edit(() => {
      const layer = targetLayer();
      if (!layer) return false;
      if (!Array.isArray(layer.objects)) layer.objects = [];
      const ids = [];
      for (const o of list) {
        o.id = uid(o.type[0], takenIds());
        o.x = num(o.x, 0) + dx;
        o.y = num(o.y, 0) + dy;
        layer.objects.push(o);
        ids.push(o.id);
      }
      sel.ids = ids;
      sel.layer = layer.id;
    });
  }

  // --- Hooks: keys, drops, double-clicks and the properties panel ------------------------------------------------

  let ready = false;
  /** Once widgets are on: the W tool, the Keys help's widget lines, and the bind buttons in their slots. */
  function setUp() {
    if (ready) return;
    ready = true;
    $("widgetTool").hidden = false;
    for (const el of document.querySelectorAll(".widget-keys")) el.hidden = false;
    const p = () => primary();
    const slots = {
      tText: () => bindButton("text", () => p()?.text, (v) => { p().text = v; }),
      tColor: () => bindButton("color", () => p()?.color, (v) => { p().color = v; }, { control: $("tColor") }),
      tOutlineColor: () => bindButton("color", () => p()?.outline?.color,
                                      (v) => { const o = p(); o.outline = { width: 0, ...(isObject(o.outline) ? o.outline : {}), color: v }; },
                                      { control: $("tOutlineColor"), fallback: "#000000" }),
      shColor: () => bindButton("color", () => fillColour(p(), false), (v) => setFillColour(p(), false, v), { control: $("shColor") }),
      shColor2: () => bindButton("color", () => fillColour(p(), true), (v) => setFillColour(p(), true, v), { control: $("shColor2") }),
      shStrokeColor: () => bindButton("color", () => p()?.stroke?.color,
                                      (v) => { const o = p(); o.stroke = { width: 2, ...(isObject(o.stroke) ? o.stroke : {}), color: v }; },
                                      { control: $("shStrokeColor") }),
      mTint: () => bindButton("color", () => p()?.tint, (v) => { const o = p(); if (v === null) delete o.tint; else o.tint = v; },
                              { control: $("mTint"), fallback: null }),
      src: () => bindButton("image", () => p()?.src, (v) => { const o = p(); o.src = v; if (WV.refOf(v) !== null) o.keep_aspect = true; }),
    };
    for (const slot of document.querySelectorAll(".bind-slot[data-bind]")) if (slots[slot.dataset.bind]) slot.replaceChildren(slots[slot.dataset.bind]());
  }

  WV.hooks.props.push((p) => {
    setUp();
    showMediaRows(p);
    showCopyObject(p);
    for (const b of binds) if (b.isConnected) b.update(); else binds.delete(b);
    placePop();
  });

  WV.hooks.key.push((e, typing) => {
    if (e.key === "Escape" && (!pop.hidden || !menu.hidden || tool.name === "widget")) {  // a menu, then the W tool
      if (!pop.hidden) closePop();
      else if (!menu.hidden) closeWidgetMenu();
      else { cancelPlacing(); message(""); }
      e.preventDefault();
      return true;
    }
    if (typing || e.altKey) return false;
    const ctrl = e.ctrlKey || e.metaKey, k = e.key.toLowerCase();
    if (ctrl && !e.shiftKey && k === "g") {  // From selection (library.js's)
      e.preventDefault();
      if (sel.ids.length) WV.fromSelection();
      return true;
    }
    if (ctrl) return false;
    if (k === "w") {
      e.preventDefault();  // the menu's filter takes the focus: the w isn't typed into it
      openWidgetTool();
      return true;
    }
    if (e.key === "Enter" && sel.ids.length === 1 && primary()?.type === "widget") {
      e.preventDefault();
      editWidget(primary());
      return true;
    }
    return false;
  });

  WV.hooks.drop.push((e) => {  // a card from the Widgets tab: a copy centred where it lands
    if (![...(e.dataTransfer?.types || [])].includes("application/x-wv-widget")) return false;
    const id = e.dataTransfer.getData("application/x-wv-widget");
    if (e.target.closest?.("#stageWrap")) {
      const [x, y] = screenPoint(e, true);
      placeCopy(id, x, y);
    }
    return true;
  });

  WV.hooks.dblclick.push((e) => {  // a copy opens its widget
    const hit = objectAt(e);
    if (hit?.obj.type !== "widget") return false;
    editWidget(hit.obj);
    return true;
  });

  // notes that follow what arrives (live values, fonts, media), and the W menu's pictures
  setInterval(() => {
    if (!widgetsOn) return;
    refreshCopyNotes();
    placePop();
    if (!menu.hidden && placing.thumbs !== `${libEpoch}|${mediaVersion}|${fontsArrived}|${live.version}`) drawMenuThumbs();
  }, 500);

  Object.assign(window, { showCopyProps, inputRow, newCopy, bindButton, pasteCopied, clipFrom, setCopyScale, copyDrawable });
})();

WV.loaded.copies = true;
