"use strict";
// Reusable widgets (docs/widgets.md): the library. The Widgets tab in the bottom strip and its cards; the window that
// makes a widget (New, From selection, Duplicate, Change folder name); the Delete, Export, Import and conflict windows;
// and the open widget's own properties (#pWidget) with its inputs. Loaded after studio.js, whose functions it uses.
// Nothing here runs while widgets are off (widgetsOn in studio.js).
//
// Names, descriptions, input names, file names and font families come from widget.json files and zips, which can come
// from anywhere: they're only ever shown as text (textContent, title, an option's text, a field's value).

(() => {
  const RESERVED = new Set(["con", "prn", "aux", "nul", ...[1, 2, 3, 4, 5, 6, 7, 8, 9].flatMap((i) => [`com${i}`, `lpt${i}`])]);
  const MAX_INPUTS = 32;
  const KINDS = ["text", "color", "image"];
  const plain = (v) => !!v && typeof v === "object" && !Array.isArray(v);
  const str = (v) => (typeof v === "string" ? v : "");
  const points = (s, n) => [...s].slice(0, n).join("");  // the first n code points: never half an emoji
  const spaces = (s) => s.replace(/^ +| +$/g, "");       // whole values are compared with U+0020 trimmed

  // --- Ids (docs/widgets.md, Ids and folder names) -------------------------------------------------------------

  /** Lowercase, accents removed, each run of anything but a-z and 0-9 one `sep`, and none at the ends. */
  function slug(text, sep) {
    const s = String(text ?? "").toLowerCase().normalize("NFKD").replace(/\p{M}+/gu, "");
    return s.replace(/[^a-z0-9]+/g, sep).split(sep).filter(Boolean).join(sep);
  }
  /** The id a name gives, as the studio makes it: cut to 40, `widget` when empty, `-widget` after a name Windows
   *  reserves, and never an example's (`example-…` folders are the shipped ones). */
  function idFrom(name) {
    const base = (n) => {
      const s = slug(n, "-").slice(0, 40).replace(/-+$/, "");
      return !s ? "widget" : RESERVED.has(s) ? `${s}-widget` : s;
    };
    let stem = base(name);
    while (stem.startsWith("example-")) stem = base(stem.slice("example-".length));
    return stem;
  }
  /** What a new widget's id can't be: the library's ids, Recently deleted's, and the folders listed as problems. */
  function takenIds() {
    return new Set([...lib.keys(), ...libDeleted.map((d) => d?.id), ...libProblems.map((p) => p?.folder)]
      .filter((s) => typeof s === "string").map((s) => s.toLowerCase()));
  }
  /** A new widget's id from its name: `-2`, `-3` and so on while that's taken, the stem cut to stay within 40. */
  function freshId(name, taken = takenIds()) {
    const stem = idFrom(name);
    let id = stem;
    for (let n = 2; taken.has(id); n++) id = stem.slice(0, 40 - `-${n}`.length).replace(/-+$/, "") + `-${n}`;
    return id;
  }
  /** A new input's id from its name (docs/widgets.md, Inputs and bind buttons), as copies.js makes one: lowercase,
   *  each run of anything but a-z and 0-9 one `_`, none at the ends, `i_` before one that doesn't start with a
   *  letter, cut to 32; `_2`, `_3` and so on while `taken` has it. */
  function inputIdFrom(name, taken) {
    let stem = String(name ?? "").toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "");
    if (!/^[a-z]/.test(stem)) stem = `i_${stem}`;
    stem = stem.slice(0, 32);
    let id = stem;
    for (let n = 2; taken.has(id) || !WV.INPUT_ID_RE.test(id); n++) id = stem.slice(0, 32 - `_${n}`.length) + `_${n}`;
    return id;
  }

  // --- Holders and copies --------------------------------------------------------------------------------------

  /** [key, holder] for Global, each scene and each widget; with `editable`, the widgets that aren't read-only only. */
  function holders({ editable = false } = {}) {
    return [["global", doc.global], ...doc.scenes.filter(plain).map((s) => [s.id, s]),
            ...[...lib].filter(([id]) => !editable || !isReadOnly(id)).map(([id, w]) => [`widget:${id}`, w])];
  }
  /** Every copy of widget `id` in `list` (every holder by default): [{key, o}]. */
  function copiesOf(id, list = holders()) {
    const out = [];
    for (const [key, h] of list) {
      for (const l of layersOf(h)) for (const o of objectsIn(l)) if (o.type === "widget" && o.widget === id) out.push({ key, o });
    }
    return out;
  }
  /** How many copies of each widget there are, in the playlist and in widgets: id -> count. */
  function copyCounts() {
    const n = new Map();
    for (const [, h] of holders()) {
      for (const l of layersOf(h)) {
        for (const o of objectsIn(l)) if (o.type === "widget" && typeof o.widget === "string") n.set(o.widget, (n.get(o.widget) || 0) + 1);
      }
    }
    return n;
  }
  /** A copy's name as the page shows it: its own, else its widget's. */
  const copyName = (o) => (typeof o.name === "string" && o.name ? o.name : widgetName(o.widget));
  /** "GPU temp in Desk, Fan in Dashboard": the first few copies of a list, then how many more. */
  function copyList(list) {
    return list.slice(0, 6).map(({ key, o }) => `${copyName(o)} in ${holderName(key)}`).join(", ")
      + (list.length > 6 ? ` and ${list.length - 6} more` : "");
  }
  /** A widget as the player reads it, one from a newer studio too (it's shown, read-only). */
  const defOf = (id) => WV.readDef(id, { ...lib.get(id), version: 1 });

  /** The box around objects as drawn (docs/widgets.md, Making a widget): each text grown by its outline and a tenth
   *  of its size, each shape by half its stroke, rounded outward, then cut to the biggest container from its corner. */
  function contentBox(objs) {
    let l = Infinity, t = Infinity, r = -Infinity, b = -Infinity;
    for (const o of objs) {
      const box = objBox(o);
      const grow = o.type === "text" ? Math.max(0, num(o.outline?.width, 0)) + 0.1 * num(o.size, 48)
        : o.type === "shape" && plain(o.stroke) && num(o.stroke.width, 0) > 0 ? num(o.stroke.width, 0) / 2 : 0;
      l = Math.min(l, num(box.x, 0) - grow);
      t = Math.min(t, num(box.y, 0) - grow);
      r = Math.max(r, num(box.x, 0) + num(box.w, 0) + grow);
      b = Math.max(b, num(box.y, 0) + num(box.h, 0) + grow);
    }
    const x = Math.floor(l), y = Math.floor(t), w = Math.max(1, Math.ceil(r) - x), h = Math.max(1, Math.ceil(b) - y);
    return { x, y, w: Math.min(w, 1600), h: Math.min(h, 720), cut: w > 1600 || h > 720 };
  }

  /** A copy's stored size after its widget's container changed: its scale `s` times the container `def`. A copy
   *  without a scale of its own (an older tool's) gets the one it had, so it stays the size it was. */
  function fitCopy(o, s, def) {
    if (!(typeof o.scale === "number" && o.scale > 0)) s = o.scale = Math.min(16, Math.max(0.01, Math.round(s * 1e4) / 1e4));
    o.w = WV.rnd(s * def.w);
    o.h = WV.rnd(s * def.h);
  }

  /** One undoable change to the open widget, `fn(content, id)`; refused for a read-only one. */
  function editOpen(fn, key = "") {
    const id = holderWidgetId();
    if (id === null || !lib.has(id)) return false;
    if (isReadOnly(id)) {
      message(`${widgetName(id)} is read-only: duplicate it to edit.`, "warn");
      return false;
    }
    return edit(() => fn(lib.get(id), id), key);
  }

  /** Opens a widget in the editor. From a scene or Global, ← Back returns there; from another widget, still to the
   *  scene you started from. */
  function openWidget(id) {
    const key = `widget:${id}`;
    if (sel.scene === key || !lib.has(id)) return;
    if (isWidgetHolder()) openHolder(key);
    else {
      nav.length = 0;
      openHolder(key, { back: true });
    }
  }

  /** Opens a widget's folder in Explorer. */
  async function showFolder(id) {
    try {
      const res = await api("/api/reveal", { method: "POST", headers: { "Content-Type": "application/json" },
                                             body: JSON.stringify({ widget: id }) });
      if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error || res.statusText);
    } catch (err) {
      message(`Couldn't open widgets\\${id}: ${err.message}`, "warn");
    }
  }

  // --- Small menus -----------------------------------------------------------------------------------------------

  let pop = null;
  function closePop() {
    pop?.remove();
    pop = null;
  }
  /** A small menu or list under `anchor` (above it when there's no room), closed by a click elsewhere or Esc. */
  function openPop(anchor, children) {
    closePop();
    pop = make("div", "wc-pop");
    pop.setAttribute("role", "menu");
    pop.append(...children);
    document.body.append(pop);
    const r = anchor.getBoundingClientRect(), w = pop.offsetWidth, h = pop.offsetHeight;
    let top = r.bottom + 2;
    if (top + h > innerHeight - 8) top = Math.max(8, r.top - h - 2);
    pop.style.left = `${Math.max(8, Math.min(r.left, innerWidth - w - 8))}px`;
    pop.style.top = `${top}px`;
  }
  /** A menu of [label, title, run] under `anchor`. */
  function menu(anchor, entries) {
    openPop(anchor, entries.map(([label, title, run]) => {
      const b = make("button", "", label);
      b.setAttribute("role", "menuitem");
      b.title = title;
      b.addEventListener("click", () => { closePop(); run(); });
      return b;
    }));
  }
  document.addEventListener("pointerdown", (e) => { if (pop && !pop.contains(e.target)) closePop(); }, true);
  document.addEventListener("keydown", (e) => { if (pop && e.key === "Escape") closePop(); }, true);
  addEventListener("resize", closePop);

  // --- The Widgets tab (docs/widgets.md, The Widgets tab) ---------------------------------------------------------

  // thumbs: id -> {canvas, key}; live: id -> the live texts its card shows, filled in with the rest (WV.texts)
  const strip = { tab: null, kind: null, key: "", thumbs: new Map(), live: new Map() };
  const cardCv = document.createElement("canvas");  // a widget drawn at its own size, for its card or its picture

  function savedTab() {
    try { return localStorage.getItem("wv.stripTab") === "widgets" ? "widgets" : "scenes"; } catch { return "scenes"; }
  }
  function keepTab(tab) {
    strip.tab = tab;
    try { localStorage.setItem("wv.stripTab", tab); } catch {}
  }
  function setTab(tab) {
    keepTab(tab);
    showWidgetCards();
    thumbsSoon();
  }
  const canMakeFromSelection = () => sel.ids.length > 0 && !!holder() && !isReadOnly(holderWidgetId());

  /** The Scenes | Widgets tabs, and the widget cards (studio.js's showStrip calls this). Opening a widget shows the
   *  Widgets tab, opening a scene or Global the Scenes tab; in between, the tab stays where it was put. */
  function showWidgetCards() {
    const kind = isWidgetHolder() ? "widgets" : "scenes";
    if (strip.kind === null) strip.tab = kind === "widgets" ? "widgets" : savedTab();
    else if (kind !== strip.kind) keepTab(kind);
    strip.kind = kind;
    $("stripTabs").hidden = false;
    for (const b of $("stripTabs").children) b.setAttribute("aria-pressed", b.dataset.tab === strip.tab);
    $("scenes").hidden = strip.tab !== "scenes";
    $("widgetCards").hidden = strip.tab !== "widgets";
    $("wcFromSel").disabled = !canMakeFromSelection();
    $("wcDeleted").hidden = !libDeleted.length;
    $("wcDeleted").textContent = `Recently deleted (${libDeleted.length})`;
    if (strip.tab !== "widgets") return;
    const counts = copyCounts(), current = holderWidgetId();
    $("wcFilter").hidden = lib.size + libProblems.length <= 8;
    const words = $("wcFilter").hidden ? [] : $("wcFilter").value.toLowerCase().split(/\s+/).filter(Boolean);
    const match = (...texts) => words.every((w) => texts.some((t) => t.toLowerCase().includes(w)));
    const shown = [...lib.keys()].map((id) => ({ id, name: widgetName(id) })).filter((e) => match(e.name, e.id))
      .sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) || a.id.localeCompare(b.id));
    const problems = libProblems.filter((p) => plain(p) && match(str(p.folder)));
    const key = JSON.stringify([current, problems, shown.map(({ id, name }) => [id, name, str(lib.get(id)?.description), counts.get(id) || 0,
                                                                              isReadOnly(id), libMeta.get(id)?.readOnly ?? null])]);
    if (key === strip.key) return;
    strip.key = key;
    const empty = shown.length || problems.length ? [] : [make("div", "small muted", lib.size ? "No widget matches." : "No widgets yet: + New makes one.")];
    $("wcList").replaceChildren(...shown.map(({ id }) => widgetCard(id, counts.get(id) || 0, current)), ...problems.map(problemCard), ...empty);
  }

  /** A widget's card: its picture over checks, its name and copy count, ⧉ ⇩ ✕. One made by a newer studio is grey,
   *  with the reason, and opens read-only. */
  function widgetCard(id, count, current) {
    const content = lib.get(id), name = widgetName(id), description = str(content?.description);
    const newer = libMeta.get(id)?.readOnly === "newer" || (typeof content?.version === "number" && content.version > 1);
    const c = make("div", `wcard${id === current ? " current" : ""}${newer ? " problem openable" : ""}`);
    c.dataset.widget = id;
    c.title = description ? `${name}\n${description}` : name;
    if (newer) c.append(make("div", "wc-why", "Made by a newer studio: it opens read-only. Update this studio to edit it."));
    else {
      let t = strip.thumbs.get(id);
      if (!t) {
        t = { canvas: make("canvas"), key: "" };
        [t.canvas.width, t.canvas.height] = [320, 144];
        strip.thumbs.set(id, t);
      }
      c.append(t.canvas);
      c.draggable = true;  // onto the canvas: a copy (copies.js)
      c.addEventListener("dragstart", (e) => {
        e.dataTransfer.setData("application/x-wv-widget", id);
        e.dataTransfer.effectAllowed = "copy";
      });
    }
    if (id.startsWith("example-")) c.append(make("span", "wc-tag", "example"));
    const meta = make("span", "wc-meta"), uses = make("span", "", `${count}×`);
    uses.title = count === 1 ? "Used by 1 copy" : `Used by ${count} copies`;
    const btn = (label, title, run) => {
      const b = make("button", "", label);
      b.title = title;
      b.addEventListener("click", (e) => { e.stopPropagation(); run(); });
      return b;
    };
    meta.append(uses, btn("⧉", "Duplicate this widget", () => openDuplicate(id)),
                btn("⇩", "Export: a .zip to share it", () => openExport(id)),
                btn("✕", "Delete this widget", () => openDelete(id)));
    c.append(make("span", "wc-name", name), meta);
    c.addEventListener("click", (e) => { if (!e.target.closest("button")) openWidget(id); });
    return c;
  }
  /** A folder in widgets\ that isn't a widget: grey, with the reason. */
  function problemCard(p) {
    const folder = str(p.folder), c = make("div", "wcard problem");
    c.title = str(p.problem);
    const meta = make("span", "wc-meta");
    if (WV.idOk(folder)) {  // a folder named like an id can be asked for; one with any other name can't
      const b = make("button", "", "Show folder");
      b.title = `Open widgets\\${folder}`;
      b.addEventListener("click", (e) => { e.stopPropagation(); showFolder(folder); });
      meta.append(b);
    }
    c.append(make("div", "wc-why", str(p.problem)), make("span", "wc-name", folder), meta);
    return c;
  }

  /** Draws the cards whose widget (or one it uses), media, fonts or live values changed (studio.js's renderThumbs
   *  calls this): each widget at its own size, with its defaults, letterboxed into its card. */
  function renderWidgetCards() {
    if (strip.tab !== "widgets") return;
    const asked = new Set([...strip.live.values()].flat());
    for (const [id, t] of strip.thumbs) {
      if (!lib.has(id)) { strip.thumbs.delete(id); strip.live.delete(id); continue; }
      if (!t.canvas.isConnected) continue;  // filtered out
      const key = widgetThumbKey(id);
      if (key === t.key) continue;
      t.key = key;
      const live = new Set(), g = t.canvas.getContext("2d"), cv = drawWidget(id, live);
      strip.live.set(id, [...live]);
      g.clearRect(0, 0, t.canvas.width, t.canvas.height);
      if (!cv) continue;
      const k = Math.min(t.canvas.width / cv.width, t.canvas.height / cv.height);
      const dw = Math.max(1, Math.round(cv.width * k)), dh = Math.max(1, Math.round(cv.height * k));
      g.imageSmoothingEnabled = k < 1;  // pixel art stays crisp when it's made bigger
      g.imageSmoothingQuality = "high";
      g.drawImage(cv, Math.floor((t.canvas.width - dw) / 2), Math.floor((t.canvas.height - dh) / 2), dw, dh);
    }
    if ([...strip.live.values()].flat().some((text) => !asked.has(text))) resolveSoon();  // its live values, for the card
  }
  /** A widget drawn at its own size with its defaults, as its copies are, onto cardCv; null when it can't be. Its live
   *  texts go into `live`. */
  function drawWidget(id, live = null) {
    const def = defOf(id);
    if (def.problem) return null;
    withCanvasSize(def.w, def.h, () => {
      if (cardCv.width !== W || cardCv.height !== H) [cardCv.width, cardCv.height] = [W, H];
      renderWidget(cardCv.getContext("2d"), id, 0, null);
      if (live) for (const e of widgetView(id, null)) for (const l of e.leaves) if (l.type === "text" && l.live) live.add(String(l.text));
    });
    return cardCv;
  }

  /** Recently deleted: each deleted widget, with Restore. */
  function showDeleted(anchor) {
    const rows = libDeleted.filter(plain).map((d) => {
      const row = make("div", "wc-del"), when = num(d.when, 0) > 0 ? new Date(d.when * 1000).toLocaleString() : "";
      const label = make("span", "", `${str(d.name) || str(d.id)}${when ? ` · ${when}` : ""}`);
      label.title = `widgets\\.deleted\\${str(d.trash)}`;
      const b = make("button", "", "Restore");
      b.title = `Bring it back, into widgets\\${str(d.id)}`;
      b.addEventListener("click", () => { closePop(); restoreDeleted(d); });
      row.append(label, b);
      return row;
    });
    openPop(anchor, rows.length ? rows : [make("div", "small muted", "Nothing deleted.")]);
  }
  /** Brings a deleted widget back from Recently deleted (not an undo step: its copies are as they are now). */
  async function restoreDeleted(d) {
    try {
      const out = await postJson("/api/widgets/restore", { trash: d.trash });
      if (lib.has(out.id)) takeInWidget(out.id, out.widget, out.stamp);
      else libAdd(out);
      message(`${widgetName(out.id)} is back, in widgets\\${out.id}.`);
    } catch (err) {
      message(`Couldn't bring ${str(d.name) || str(d.id)} back: ${err.message}`, "warn");
    }
    changed({ save: false });
    afterSaves(() => refreshLibrary());  // its fonts, and the list of deleted ones
  }

  for (const b of $("stripTabs").children) b.addEventListener("click", () => setTab(b.dataset.tab));
  $("wcFilter").addEventListener("input", () => { showWidgetCards(); thumbsSoon(); });
  $("wcNew").addEventListener("click", () => openNew());
  $("wcFromSel").addEventListener("click", () => fromSelection());
  $("wcImport").addEventListener("click", () => $("wcFile").click());
  $("wcFile").addEventListener("change", () => {
    const f = $("wcFile").files[0];
    $("wcFile").value = "";
    if (f) openImport(f);
  });
  $("wcDeleted").addEventListener("click", async () => {
    await refreshDeleted();  // as it is now: an undo or redo may have moved folders since
    showWidgetCards();
    showDeleted($("wcDeleted"));
  });

  // --- Making a widget: New, From selection, Duplicate, Change folder name (docs/widgets.md, Making a widget) -----

  const making = { mode: "new", id: null, from: null, busy: false };
  const PRESETS = ["360x120", "600x100", "240x240", "360x360", "1600x720"];

  /** #widgetDlg: `mode` is "new", "selection" (with `from`: what's selected), "duplicate" or "folder" (Change folder
   *  name), the last two for widget `id`. */
  function openMaking(mode, { id = null, name = "", size = null, from = null, notes = [] } = {}) {
    if ($("widgetDlg").open) return;
    Object.assign(making, { mode, id, from, busy: false });
    $("wgTitle").textContent = { new: "New widget", selection: "New widget from the selection",
                                 duplicate: `Duplicate ${widgetName(id)}`, folder: `Change the folder name of ${widgetName(id)}` }[mode];
    $("wgName").value = name;
    $("wgSize").hidden = mode !== "new" && mode !== "selection";
    $("wgReplaceRow").hidden = mode !== "selection";
    $("wgReplace").checked = true;
    [$("wgW").value, $("wgH").value] = size || [360, 120];
    syncPreset();
    $("wgBgNone").checked = true;
    $("wgNote").replaceChildren(...notes.map(([text, cls]) => make("div", cls, text)));
    $("wgCreate").textContent = { new: "Create", selection: "Create", duplicate: "Duplicate", folder: "Change" }[mode];
    $("wgMsg").textContent = "";
    $("wgCreate").disabled = false;
    showFolderToBe();
    $("widgetDlg").showModal();
    $("wgName").focus();
    $("wgName").select();
  }
  function syncPreset() {
    const size = `${$("wgW").value}x${$("wgH").value}`;
    $("wgPreset").value = PRESETS.includes(size) ? size : "";
  }
  /** The folder the name gives. For Change folder name, null when that's the folder it has now. */
  function folderToBe() {
    const name = $("wgName").value;
    if (making.mode !== "folder") return freshId(name);
    return idFrom(name) === making.id ? null : freshId(name);
  }
  function showFolderToBe() {
    const id = folderToBe();
    $("wgFolder").textContent = `widgets\\${id ?? making.id}`;
    if (making.mode === "folder") {
      $("wgCreate").disabled = making.busy || id === null;
      $("wgMsg").textContent = id === null ? "That's its folder now." : "";
    }
  }
  const say = (text) => { $("wgMsg").textContent = text; };

  function openNew() {
    if (widgetsOn) openMaking("new");
  }
  /** Duplicate…: asks for the copy's name (studio.js's Duplicate to edit calls this too). */
  function openDuplicate(id) {
    if (lib.has(id)) openMaking("duplicate", { id, name: `${widgetName(id)} copy` });
  }
  function openChangeFolder(id) {
    if (!lib.has(id) || isReadOnly(id)) return;
    openMaking("folder", { id, name: widgetName(id), notes: [
      ["Every copy on this PC then uses the new folder, and the old one goes to Recently deleted. Ctrl+Z puts them back.", "muted"],
      ["Copies inside widgets you've already shared with someone keep the old name, and show as missing there.", "warn"]] });
  }

  /** From selection (docs/widgets.md, Making a widget): asks for a name, with the selection's size filled in. Ctrl+G
   *  and the From selection buttons call it. */
  function fromSelection() {
    if (!widgetsOn || $("widgetDlg").open) return;
    const h = holder(), wid = holderWidgetId();
    if (!h) return;
    if (wid !== null && isReadOnly(wid)) return message(`${widgetName(wid)} is read-only: duplicate it to edit.`, "warn");
    const objs = selectedObjects();
    if (!objs.length) return message("Select the objects to make a widget from first.", "warn");
    const box = contentBox(objs), ids = new Set(objs.map((o) => o.id)), layers = layersOf(h);
    const notes = [["The objects move into the widget, and a copy of it takes their place. Their pictures and fonts are copied into its folder.", "muted"]];
    const holding = layers.filter((l) => objectsIn(l).some((o) => ids.has(o.id)));
    if (holding.length > 1 && holding.some((l) => clamp01(l.opacity) < 1)) {
      notes.push(["They're on more than one layer, and one is below 100%: in the copy those layers fade on their own, which can look different.", "warn"]);
    }
    const order = layers.filter((l) => l.visible !== false).flatMap((l) => objectsIn(l).filter((o) => o.visible !== false));
    const first = order.findIndex((o) => ids.has(o.id)), last = order.findLastIndex((o) => ids.has(o.id));
    if (first >= 0 && order.slice(first, last + 1).some((o) => !ids.has(o.id))) {
      notes.push(["Something that isn't selected sits between them: it ends up above or below the copy.", "warn"]);
    }
    if (box.cut) notes.push(["The selection is bigger than a widget can be (1600 × 720): what's past that is cut off.", "warn"]);
    openMaking("selection", { size: [box.w, box.h], from: { key: sel.scene, ids: [...ids], box }, notes });
  }

  async function create() {
    if (making.busy) return;
    const name = $("wgName").value.trim();
    if (!name) return say("Give it a name.");
    let w = 0, h = 0, background = null;
    if (!$("wgSize").hidden) {
      [w, h] = [Number($("wgW").value), Number($("wgH").value)];
      if (!Number.isInteger(w) || !Number.isInteger(h) || w < 1 || w > 1600 || h < 1 || h > 720) {
        return say("The size is 1 to 1600 across and 1 to 720 down, in whole pixels.");
      }
      background = $("wgBgOn").checked ? hex($("wgBgColor").value, "#000000") : null;
    }
    making.busy = true;
    $("wgCreate").disabled = true;
    say("");
    try {
      if (making.mode === "new") await createNew(name, w, h, background);
      else if (making.mode === "selection") await createFromSelection(name, w, h, background, $("wgReplace").checked);
      else if (making.mode === "duplicate") await duplicate(making.id, name);
      else await changeFolder(making.id, name, folderToBe());
    } catch (err) {
      say(err.message);
    } finally {
      making.busy = false;
      $("wgCreate").disabled = false;
    }
  }

  async function createNew(name, w, h, background) {
    const out = await postJson("/api/widgets/new", { name, w, h, background });
    libAdd(out);
    $("widgetDlg").close();
    openWidget(out.id);
    message(`Made ${widgetName(out.id)}, in widgets\\${out.id}.`);
  }

  /** The new widget gets the selected objects, layer by layer, moved to its corner; inside widget A, the inputs of A
   *  they use. Then, as one undo step, a copy of it takes their place. The widget itself isn't part of that step. */
  async function createFromSelection(name, w, h, background, replace) {
    const from = making.from, wid = holderWidgetId();
    if (sel.scene !== from.key) throw new Error("The scene or widget the objects are in isn't open any more.");
    const ids = new Set(from.ids), [cx, cy] = [from.box.x, from.box.y];
    const holding = layersOf(holder()).map((l, i) => [l, i, objectsIn(l).filter((o) => ids.has(o.id))]).filter(([, , mine]) => mine.length);
    if (!holding.length) throw new Error("The selected objects are gone.");
    const top = holding[holding.length - 1][0];  // the layer the copy goes into: its own opacity fades the copy already
    const parts = holding.map(([l, i, mine]) => ({
      id: typeof l.id === "string" && l.id ? l.id : `l${i}`, name: typeof l.name === "string" ? l.name : `Layer ${i + 1}`,
      visible: l.visible !== false, locked: false, opacity: l === top ? 1 : clamp01(l.opacity),
      objects: mine.map((o) => Object.assign(structuredClone(o), { x: num(o.x, 0) - cx, y: num(o.y, 0) - cy })),
    }));
    let inputs = [];
    if (wid !== null) {  // inside widget A: A's inputs they use, which the copy then passes down
      const used = new Set();
      for (const p of parts) for (const o of p.objects) refsIn(o, used);
      inputs = defOf(wid).inputs.filter((i) => used.has(i.id)).map((i) => ({ id: i.id, name: i.name, type: i.type, default: i.default }));
    }
    const out = await postJson("/api/widgets/new", { name, w, h, background, inputs, layers: parts,
                                                     from: wid === null ? "playlist" : `widget:${wid}` });
    libAdd(out);
    $("widgetDlg").close();
    afterSaves(() => refreshLibrary());  // the fonts it was given
    const left = (Array.isArray(out.missing) ? out.missing : []).filter(plain).map((m) => `${str(m.file)} (${str(m.why)})`);
    const notes = left.length ? ` Left out: ${left.join("; ")}` : "";
    if (!replace) return message(`Made ${widgetName(out.id)}, in widgets\\${out.id}.${notes}`, left.length ? "warn" : "");
    let placed = false;
    if (sel.scene === from.key) {
      edit(() => {
        const ls = layersOf(holder()), li = ls.map((l) => objectsIn(l).some((o) => ids.has(o.id))).lastIndexOf(true);
        if (li < 0) return false;
        const layer = ls[li], index = layer.objects.findLastIndex((o) => plain(o) && ids.has(o.id)) + 1;
        const copy = newCopy(out.id, layer.id, index, cx, cy, 1);  // over the topmost selected object, which then goes
        if (!copy) return false;
        if (inputs.length) copy.inputs = Object.fromEntries(inputs.map((i) => [i.id, `{$${i.id}}`]));
        for (const l of ls) if (Array.isArray(l.objects)) l.objects = l.objects.filter((o) => !(plain(o) && ids.has(o.id)));
        sel.ids = [copy.id];
        sel.layer = layer.id;
        placed = true;
      });
    }
    if (placed) message(`Made ${widgetName(out.id)}, and put a copy of it in place of the objects.${notes}`, left.length ? "warn" : "");
    else message(`Made ${widgetName(out.id)}, but couldn't put a copy of it in place of the objects.${notes}`, "warn");
  }
  /** Adds to `out` every input id an object refers to: in its text, its colours and tint, its file, and a copy's values. */
  function refsIn(o, out) {
    const look = (v) => { if (typeof v === "string") WV.replaceRefs(v, (b) => { out.add(b); return null; }); };
    for (const v of [o.text, o.color, o.outline?.color, o.fill?.color, o.stroke?.color, o.src, o.tint]) look(v);
    for (const s of Array.isArray(o.fill?.stops) ? o.fill.stops : []) look(s?.color);
    if (o.type === "widget" && plain(o.inputs)) for (const v of Object.values(o.inputs)) look(v);
  }

  /** Duplicate: a copy of the folder under a new id from `name`, which then opens. */
  async function duplicate(id, name) {
    await flushSave();  // so the copy has every edit
    const out = await postJson("/api/widgets/duplicate", { id, name });
    libAdd(out);
    $("widgetDlg").close();
    openWidget(out.id);
    afterSaves(() => refreshLibrary());  // its fonts
    message(`Made ${widgetName(out.id)}, in widgets\\${out.id}.`);
  }

  /** Change folder name (docs/widgets.md, The widget editor): a duplicate in the new folder, then the old one to
   *  Recently deleted; then, as one undo step that undoes the move too, every copy on this PC points at the new id.
   *  If the old folder can't move (in use), the new one stays, unused, and nothing else changes. */
  async function changeFolder(id, name, folder) {
    if (folder === null) return say("That's its folder now.");
    await flushSave();
    const out = await postJson("/api/widgets/duplicate", { id, name, folder });
    libAdd(out);
    $("widgetDlg").close();
    const action = { action: "delete", id, trash: null }, before = snapshot();
    if (!(await trashFolder(action))) {
      message(`widgets\\${out.id} was made, but the old folder couldn't move, so nothing uses the new one yet. Try again, or delete it.`, "warn");
      return changed({ save: false });
    }
    for (const { o } of copiesOf(id, holders({ editable: true }))) o.widget = out.id;
    pushUndo(before, "", action);
    changed();
    openWidget(out.id);
    afterSaves(() => refreshLibrary());  // its fonts, and Recently deleted
    message(`${widgetName(out.id)} is in widgets\\${out.id} now, and every copy here uses it.`);
  }

  $("wgName").addEventListener("input", showFolderToBe);
  $("wgName").addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); create(); } });
  $("wgPreset").addEventListener("change", () => {
    const [w, h] = $("wgPreset").value.split("x");
    if (w) [$("wgW").value, $("wgH").value] = [w, h];
  });
  for (const id of ["wgW", "wgH"]) $(id).addEventListener("input", syncPreset);
  $("wgBgColor").addEventListener("input", () => { $("wgBgOn").checked = true; });
  $("wgCreate").addEventListener("click", create);
  $("wgCancel").addEventListener("click", () => $("widgetDlg").close());

  // --- Deleting (docs/widgets.md, Undo, saving and deleting) ---------------------------------------------------------

  let deleting = null;
  function openDelete(id) {
    if (!lib.has(id) || $("deleteDlg").open) return;
    deleting = id;
    const name = widgetName(id);
    const uses = (WV.uses(lib, doc, closureMemo()).get(id) || []).filter((u) => u.direct > 0 && u.holder !== `widget:${id}`);
    const n = uses.reduce((a, u) => a + u.direct, 0);
    $("dlTitle").textContent = `Delete ${name}`;
    $("dlText").textContent = n ? `${name} is used ${n === 1 ? "once" : `${n} times`}: ${uses.map((u) => `${holderName(u.holder)} (${u.direct})`).join(", ")}.`
      : `${name} isn't used anywhere.`;
    $("dlKeep").hidden = !n;
    $("dlAll").textContent = n ? "Delete widget and copies" : "Delete widget";
    $("deleteDlg").showModal();
  }
  async function runDelete(copies) {
    const id = deleting;
    $("deleteDlg").close();
    if (!lib.has(id)) return;
    const name = widgetName(id);
    await flushSave();  // so its folder holds every edit, for Ctrl+Z or Restore
    if (!(await deleteWidget(id, { copies }))) return;
    message(`Deleted ${name}: it's in Recently deleted, and Ctrl+Z brings it back.`);
    deletedChanged();
  }
  /** Reads Recently deleted again, after a folder moved there. */
  function deletedChanged() {
    afterSaves(async () => { await refreshDeleted(); showWidgetCards(); });
  }
  $("dlCancel").addEventListener("click", () => $("deleteDlg").close());
  $("dlKeep").addEventListener("click", () => runDelete(false));
  $("dlAll").addEventListener("click", () => runDelete(true));

  // --- Export (docs/widgets.md, Sharing) ---------------------------------------------------------------------------

  let exporting = null;
  /** The fonts the widget and the widgets it uses draw text in that could travel with it: found on this PC, not one of
   *  Windows' own (every PC has those), and not in its fonts\ folder already. [{name, family, style}] */
  function shareableFonts(id) {
    const out = new Map();
    for (const wid of [id, ...WV.closure(lib, id, closureMemo())]) {
      const meta = libMeta.get(wid), own = new Set((meta?.ownFonts || []).map((f) => str(f?.name).toLowerCase()));
      for (const l of layersOf(lib.get(wid))) {
        for (const o of objectsIn(l)) {
          const f = o.type === "text" ? o.font : null;
          if (!WV.fontNameOk(f) || own.has(f.toLowerCase()) || out.has(f.toLowerCase())) continue;
          const found = meta?.resolvedFonts?.[f];
          if (!found?.path || /^[a-z]:\\windows\\fonts\\/i.test(String(found.path))) continue;
          out.set(f.toLowerCase(), { name: f, family: str(found.family), style: str(found.style) });
        }
      }
    }
    return [...out.values()];
  }
  function openExport(id) {
    if (!lib.has(id) || $("exportDlg").open) return;
    exporting = id;
    const name = widgetName(id), used = [...WV.closure(lib, id, closureMemo())].filter((w) => w !== id && lib.has(w)).map(widgetName);
    $("wxTitle").textContent = `Export ${name}`;
    $("wxText").textContent = `Saves ${id}.zip, with a picture of it: ${name}${used.length ? ` and the widget${used.length > 1 ? "s" : ""} it uses (${used.join(", ")})` : ""}. `
      + "Anyone can import it, or copy its folder into their widgets folder."
      + (id.startsWith("example-") ? " It's an example: its picture and fonts go into the zip only." : "");
    const fonts = shareableFonts(id);
    $("wxFontsBox").hidden = !fonts.length;
    $("wxFonts").replaceChildren(...fonts.map((f) => {
      const li = make("li"), label = make("label"), cb = make("input");
      Object.assign(cb, { type: "checkbox", checked: true, value: f.name });
      label.append(cb, `${[f.family, f.style].filter(Boolean).join(" ") || f.name} (${f.name})`);
      li.append(label);
      return li;
    }));
    $("wxNotes").replaceChildren();
    $("wxMsg").textContent = "";
    $("wxGo").disabled = false;
    $("exportDlg").showModal();
  }
  /** The widget's picture for the zip (preview.png): drawn as its copies are, made smaller until it's under 2 MB. */
  function widgetPicture(id) {
    const cv = drawWidget(id);
    if (!cv) return null;
    let url = cv.toDataURL("image/png");
    for (let k = 0.5; url.length > 2.7e6 && k > 0.03; k /= 2) {
      const small = document.createElement("canvas"), g = small.getContext("2d");
      [small.width, small.height] = [Math.max(1, Math.round(cv.width * k)), Math.max(1, Math.round(cv.height * k))];
      g.imageSmoothingQuality = "high";
      g.drawImage(cv, 0, 0, small.width, small.height);
      url = small.toDataURL("image/png");
    }
    return url.length <= 2.7e6 ? url : null;
  }
  async function runExport() {
    const id = exporting;
    if (!lib.has(id) || $("wxGo").disabled) return;
    $("wxGo").disabled = true;
    $("wxMsg").className = "small muted";
    $("wxMsg").textContent = "Exporting…";
    try {
      await flushSave();  // the zip is made from the files
      const fonts = [...$("wxFonts").querySelectorAll("input[type=checkbox]")].filter((c) => c.checked).map((c) => c.value);
      const preview = widgetPicture(id);
      const res = await api("/api/widgets/export", { method: "POST", headers: { "Content-Type": "application/json" },
                                                     body: JSON.stringify({ id, fonts, ...(preview ? { preview } : {}) }) });
      if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error || res.statusText);
      let notes = [];
      try { notes = JSON.parse(res.headers.get("X-Widget-Notes") || "[]"); } catch {}
      const a = make("a"), file = `${id}.zip`;
      a.href = URL.createObjectURL(await res.blob());
      a.download = file;
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 60e3);
      $("wxNotes").replaceChildren(...(Array.isArray(notes) ? notes : []).map((n) => make("li", "", String(n))));
      $("wxMsg").className = "small info";
      $("wxMsg").textContent = `Saved ${file} to your downloads.`;
      afterSaves(() => refreshLibrary());  // the fonts it now carries in its fonts\ folder
    } catch (err) {
      $("wxMsg").className = "small warn";
      $("wxMsg").textContent = `Couldn't export: ${err.message}`;
    } finally {
      $("wxGo").disabled = false;
    }
  }
  $("wxGo").addEventListener("click", runExport);
  $("wxCancel").addEventListener("click", () => $("exportDlg").close());

  // --- Import (docs/widgets.md, Sharing) -----------------------------------------------------------------------

  const importing = { file: null, items: [] };
  /** Reads a shared .zip (nothing is written yet), then shows what's in it. */
  async function openImport(file) {
    if (!widgetsOn || !file || $("importDlg").open) return;
    importing.file = file;
    message(`Reading ${file.name}…`, "muted");
    let out;
    try {
      const res = await api(`/api/widgets/import?mode=check&name=${encodeURIComponent(file.name)}`, { method: "POST", body: file });
      out = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(out.error || res.statusText);
    } catch (err) {
      if (importing.file === file) message(`${file.name} can't be imported: ${err.message}`, "warn");
      return;
    }
    if (importing.file !== file || $("importDlg").open) return;  // another one was dropped meanwhile
    message("");
    importing.items = (Array.isArray(out.widgets) ? out.widgets : []).filter(plain).map(importItem);
    $("wiTitle").textContent = `Import ${file.name}`;
    $("wiList").replaceChildren(...importing.items.map((it) => it.el));
    const notes = [...(Array.isArray(out.errors) ? out.errors : []), ...(Array.isArray(out.notes) ? out.notes : [])];
    $("wiNotes").replaceChildren(...notes.map((n) => make("li", "", String(n))));
    $("wiMsg").textContent = "";
    $("wiGo").disabled = !importing.items.some((it) => it.x.exists !== "same");
    $("importDlg").showModal();
  }
  /** What the import window shows for one widget of the zip, and the choices it offers. */
  function importItem(x, n) {
    const el = make("div", "wi-item"), body = make("div"), list = make("ul", "small"), it = { x, el, choice: null, fonts: [] };
    const pic = str(x.preview).startsWith("data:image/png;base64,") ? Object.assign(make("img", "wi-pic"), { src: x.preview, alt: "" })
      : make("div", "wi-pic");
    const name = str(x.name) || str(x.id), id = str(x.id);
    body.append(make("h3", "", name), make("div", "small muted", `${num(x.w, 0)} × ${num(x.h, 0)} · widgets\\${id}`));
    if (str(x.description)) body.append(make("p", "small", str(x.description)));
    const inputs = (Array.isArray(x.inputs) ? x.inputs : []).filter(plain).map((i) => str(i.name) || str(i.id)).filter(Boolean);
    body.append(make("div", "small", inputs.length ? `Inputs: ${inputs.join(", ")}` : "No inputs: it sets its own text and pictures."));
    const note = (text, cls = "") => list.append(make("li", cls, text));
    const choice = make("div", "row wi-choice");
    if (x.exists === "same") note("You have this one already: it's skipped.", "muted");
    else if (x.exists === "different") {
      note(`You have a different widget in widgets\\${id}: keep both, or replace yours with this one?`);
      for (const [value, label, title] of [["keep-both", "Keep both", "Import it under another folder name, and keep yours"],
                                           ["replace", "Replace", "Yours goes to Recently deleted, and every copy of it uses this one"],
                                           ["skip", "Skip", "Leave it out"]]) {
        const l = make("label"), r = make("input");
        Object.assign(r, { type: "radio", name: `wiChoice${n}`, value, checked: value === "keep-both" });
        r.disabled = value === "replace" && id.startsWith("example-");
        l.title = r.disabled ? "An example isn't replaced: Keep both imports this one under another folder name" : title;
        r.addEventListener("change", () => { if (r.checked) it.choice = value; });
        l.append(r, ` ${label}`);
        choice.append(l);
      }
      it.choice = "keep-both";
    } else note("New: it goes into your widgets.", "muted");
    const needs = (Array.isArray(x.needs) ? x.needs : []).map(String);
    if (needs.length) note(`It uses ${needs.join(", ")}, which ${needs.length > 1 ? "aren't" : "isn't"} in the zip or your widgets: those copies show as missing.`, "warn");
    const cycles = (Array.isArray(x.cycles) ? x.cycles : []).map(String);
    if (cycles.length) note(`With your ${cycles.join(", ")}, it would contain itself: that inner copy won't draw.`, "warn");
    const offers = make("div");
    for (const f of (Array.isArray(x.fonts_missing) ? x.fonts_missing : []).filter(plain)) {
      const family = [str(f.family), str(f.style)].filter(Boolean).join(" ") || str(f.name);
      note(`Its text in ${str(f.name)} shows in Segoe UI until you install ${family}.`, "warn");
      const other = (Array.isArray(f.installed) ? f.installed : []).find((file) => WV.fontNameOk(file));
      if (other) {  // the same family and style, under another file name on this PC
        const l = make("label", "small"), cb = make("input");
        cb.type = "checkbox";
        l.append(cb, ` Use the installed ${family} (${other})`);
        it.fonts.push({ from: str(f.name), to: other, cb });
        offers.append(l);
      }
    }
    for (const t of Array.isArray(x.notes) ? x.notes : []) note(String(t), "muted");
    body.append(list, ...(choice.children.length ? [choice] : []), offers);
    el.append(pic, body);
    return it;
  }
  async function runImport() {
    const file = importing.file;
    if (!file || $("wiGo").disabled) return;
    $("wiGo").disabled = true;
    $("wiMsg").textContent = "";
    const choices = {}, swaps = new Map();  // swaps: the zip's folder -> [{from, to}]
    for (const it of importing.items) {
      if (it.choice) choices[str(it.x.folder)] = it.choice;
      const chosen = it.fonts.filter((f) => f.cb.checked);
      if (chosen.length) swaps.set(str(it.x.folder), chosen);
    }
    let out;
    try {
      await flushSave();  // a widget it replaces goes to Recently deleted as it was last edited
      const res = await api(`/api/widgets/import?mode=import&name=${encodeURIComponent(file.name)}&choices=${encodeURIComponent(JSON.stringify(choices))}`,
                            { method: "POST", body: file });
      out = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(out.error || res.statusText);
    } catch (err) {
      $("wiMsg").textContent = `Couldn't import: ${err.message}`;
      $("wiGo").disabled = false;
      return;
    }
    importing.file = null;
    $("importDlg").close();
    const done = (Array.isArray(out.imported) ? out.imported : []).filter((e) => plain(e) && typeof e.id === "string");
    for (const e of done) {
      if (lib.has(e.id)) takeInWidget(e.id, e.widget, e.stamp);  // Replace: every copy of it uses the new one
      else libAdd(e);
    }
    const fonts = done.filter((e) => swaps.has(str(e.from)) && !isReadOnly(e.id));
    if (fonts.length) {  // Use the installed …: one undoable step
      edit(() => {
        for (const e of fonts) {
          for (const l of layersOf(lib.get(e.id))) {
            for (const o of objectsIn(l)) for (const s of swaps.get(str(e.from))) if (o.type === "text" && o.font === s.from) o.font = s.to;
          }
        }
      });
    }
    const skipped = (Array.isArray(out.skipped) ? out.skipped : []).filter(plain).map((s) => `${str(s.folder) || file.name}: ${str(s.why)}`);
    message(done.length ? `Imported ${done.map((e) => widgetName(e.id)).join(", ")}.${skipped.length ? ` Skipped ${skipped.join(" ")}` : ""}`
      : `Nothing was imported.${skipped.length ? ` ${skipped.join(" ")}` : ""}`, skipped.length ? "warn" : "");
    changed({ save: false });
    afterSaves(() => refreshLibrary());  // their fonts, and Recently deleted
  }
  $("wiGo").addEventListener("click", runImport);
  $("wiCancel").addEventListener("click", () => $("importDlg").close());
  $("importDlg").addEventListener("close", () => { importing.file = null; });
  WV.hooks.drop.push((e) => {  // a .zip dropped anywhere on the page
    const f = e.dataTransfer?.files?.[0];
    if (!f || !/\.zip$/i.test(f.name)) return false;
    openImport(f);
    return true;
  });

  // --- Changed in two places (docs/widgets.md, Undo, saving and deleting) -------------------------------------------

  const asking = [];  // widgets waiting for an answer, the one shown first
  /** A widget changed both here and on disk: Keep mine, or Use the file's version (studio.js's askConflict calls this). */
  function openConflict(id) {
    if (!asking.includes(id)) asking.push(id);
    showConflict();
  }
  function showConflict() {
    if ($("conflictDlg").open || !asking.length) return;
    $("wcfText").textContent = `${widgetName(asking[0])} was changed outside this page (in another tab, or by hand) while you were `
      + "changing it here. Keep your version, or use the file's?";
    $("conflictDlg").showModal();
  }
  function answerConflict(mine) {
    const id = asking.shift();
    $("conflictDlg").close();
    if (id !== undefined) keepConflict(id, mine);
    showConflict();
  }
  $("wcfMine").addEventListener("click", () => answerConflict(true));
  $("wcfFile").addEventListener("click", () => answerConflict(false));
  $("conflictDlg").addEventListener("cancel", (e) => e.preventDefault());  // one or the other: saving it waits for this

  // --- The widget's own properties: #pWidget (docs/widgets.md, The widget editor) -----------------------------------

  const wd = { id: null, rowsKey: "", rows: new Map(), bindKey: "", previewKey: "", usedKey: "", adding: null };
  const EDITING = ["wdName", "wdW", "wdH", "wdFit", "wdBgNone", "wdBgOn", "wdBgColor", "wdDescription", "wdAddText", "wdAddColor",
                   "wdAddImage", "wdChange"];

  /** Fills #pWidget for the open widget `id` (studio.js's fillProps calls this when nothing is selected). Only what
   *  changed is built again, so a field being typed in keeps its place. */
  function showWidgetProps(id) {
    const content = lib.get(id);
    if (!content) return;
    const locked = isReadOnly(id), def = defOf(id);
    if (wd.id !== id) {
      Object.assign(wd, { id, rowsKey: "", bindKey: "", previewKey: "", usedKey: "" });
      endAdding();
    }
    for (const c of EDITING) $(c).disabled = locked;
    set("wdName", str(content.name));
    $("wdFolder").textContent = `widgets\\${id}`;
    $("wdFolder").title = "Its folder name is its id: copies find it by this";
    set("wdW", def.w);
    set("wdH", def.h);
    const bg = content.background, ref = WV.refOf(bg), colour = WV.color(bg);
    $("wdBgNone").checked = ref === null && colour === null;
    $("wdBgOn").checked = !$("wdBgNone").checked;
    const shown = ref !== null ? WV.color(def.inputs.find((i) => i.id === ref && i.type === "color")?.default) ?? "#ffffff" : colour;
    if (shown) set("wdBgColor", shown);
    $("wdBgColor").disabled = locked || ref !== null;
    if (wd.bindKey !== id) {  // the 🔗 that binds the background to a colour input (copies.js's bindButton), which
      wd.bindKey = id;        // follows the background by itself
      $("wdBgBind").replaceChildren(bindButton("color", () => lib.get(id)?.background ?? null,
                                               (v) => { const w = lib.get(id); if (w) w.background = v; },
                                               { control: $("wdBgColor"), fallback: null, name: () => "Background" }));
    }
    $("wdBgBind").firstElementChild?.update?.();
    $("wdBgBind").inert = locked;
    set("wdDescription", str(content.description));
    seg("wdBackdrop", backdrop());
    showInputs(id, def, locked);
    showPreviewChoice(id);
    const cut = [...cutOffInputs(`widget:${id}`)].map((i) => def.inputs.find((x) => x.id === i)?.name || i);
    $("wdCutNote").hidden = !cut.length;
    $("wdCutNote").textContent = cut.length ? `${cut.join(", ")} ${cut.length > 1 ? "are" : "is"} cut off by the widget's edge.` : "";
    showUsedIn(id);
  }

  /** The inputs editor: a row per input with its type, name, {$id} chip, default and ⋮. Rows are built again when
   *  inputs come, go or move; a name or default is updated in place, unless it's being typed in. */
  function showInputs(id, def, locked) {
    $("wdNoInputs").hidden = def.inputs.length > 0;
    const key = JSON.stringify([id, locked, def.inputs.map((i) => [i.id, i.type])]);
    if (key !== wd.rowsKey) {
      wd.rowsKey = key;
      wd.rows = new Map(def.inputs.map((input) => [input.id, inputEditor(input, locked)]));
      $("wdInputs").replaceChildren(...[...wd.rows.values()].map((r) => r.el));
    }
    for (const input of def.inputs) {
      const r = wd.rows.get(input.id);
      if (document.activeElement !== r.name) r.name.value = input.name;
      if (r.shown === input.default) continue;
      if (r.row?.update) r.row.update(input.default, false);  // in place: a field being typed in is left as it is
      else if (r.box.contains(document.activeElement)) continue;
      else {  // edited with the control a copy's value uses (copies.js's inputRow)
        r.row = inputRow(input, input.default, false, (v) => setDefault(input.id, v));
        r.box.replaceChildren(r.row);
      }
      r.shown = input.default;
    }
  }
  function inputEditor(input, locked) {
    const el = make("div", "wd-input"), name = make("input", "wd-iname"), chip = make("button", "wd-chip", `{$${input.id}}`);
    const more = make("button", "wd-more", "⋮"), box = make("div", "wd-default");
    const type = make("span", "wd-type", { text: "T", color: "●", image: "▣" }[input.type]);
    type.title = { text: "A text input", color: "A colour input", image: "A picture input" }[input.type];
    Object.assign(name, { type: "text", spellcheck: false, maxLength: 60, disabled: locked,
                          title: "Its name, as copies show it. Renaming changes only this: references and copies' values use its id." });
    name.addEventListener("input", () => renameInput(input.id, name.value));
    name.addEventListener("change", () => { if (!name.value.trim()) renameInput(input.id, input.id); });
    chip.title = input.type === "text" ? `Copy {$${input.id}}, to put it in a text (or use {$} by the text box)`
      : input.type === "color" ? "Bind a colour to it with the 🔗 after that colour" : "Bind a picture to it with Picture, in an image's properties";
    chip.addEventListener("click", () => copyRef(input));
    more.title = "Move up, move down or delete";
    more.disabled = locked;
    more.addEventListener("click", () => menu(more, [["Move up", "", () => moveInput(input.id, -1)],
                                                     ["Move down", "", () => moveInput(input.id, 1)],
                                                     ["Delete…", "Write its default in wherever the widget uses it, then remove it", () => deleteInput(input.id)]]));
    box.inert = locked;
    el.dataset.input = input.id;
    el.append(type, name, chip, more, box);
    return { el, name, box, shown: undefined };
  }
  /** Input `inputId` as the widget's file has it, or null. */
  const rawInput = (w, inputId) => (Array.isArray(w.inputs) ? w.inputs : []).find((i) => plain(i) && i.id === inputId) || null;
  function renameInput(inputId, name) {
    editOpen((w) => {
      const i = rawInput(w, inputId);
      if (!i) return false;
      i.name = points(name, 60);
    }, `wdInputName|${inputId}`);
  }
  function setDefault(inputId, value) {
    if (typeof value !== "string") return;  // ↺: a widget's own default has nothing to go back to
    editOpen((w) => {
      const i = rawInput(w, inputId);
      if (!i) return false;
      i.default = i.type === "text" ? points(value, 2000) : value;
    }, `wdDefault|${inputId}`);
  }
  function moveInput(inputId, dir) {
    editOpen((w) => {
      const list = Array.isArray(w.inputs) ? w.inputs : [];
      const ok = (x) => plain(x) && typeof x.id === "string" && WV.INPUT_ID_RE.test(x.id) && KINDS.includes(x.type);
      const i = list.findIndex((x) => ok(x) && x.id === inputId);
      let j = i + dir;
      while (j >= 0 && j < list.length && !ok(list[j])) j += dir;
      if (i < 0 || j < 0 || j >= list.length) return false;
      [list[i], list[j]] = [list[j], list[i]];
    });
  }
  /** A text input's reference, copied to paste into a text; for a colour or picture input, how to bind to it. */
  function copyRef(input) {
    const ref = `{$${input.id}}`, write = () => message(`Write ${ref} in a text, where ${input.name} goes.`);
    if (input.type === "color") return message(`To use ${input.name}, click the 🔗 after a colour.`);
    if (input.type === "image") return message(`To use ${input.name}, choose it under Picture in an image's properties.`);
    try {
      navigator.clipboard.writeText(ref).then(() => message(`${ref} copied: paste it into a text, where ${input.name} goes.`), write);
    } catch {
      write();
    }
  }

  /** + Text, + Colour, + Image: asks for the input's name, in the row under the buttons. */
  function askInputName(type) {
    const id = holderWidgetId();
    if (id === null || isReadOnly(id)) return;
    if (defOf(id).inputs.length >= MAX_INPUTS) return message(`A widget can have at most ${MAX_INPUTS} inputs.`, "warn");
    wd.adding = type;
    $("wdNewLabel").textContent = { text: "Text", color: "Colour", image: "Picture" }[type];
    $("wdNewName").placeholder = `Its name, like ${{ text: "Label", color: "Accent", image: "Icon" }[type]}, then Enter`;
    $("wdNewName").value = "";
    $("wdNewRow").hidden = false;
    $("wdNewName").focus();
  }
  function endAdding() {
    wd.adding = null;
    $("wdNewRow").hidden = true;
  }
  /** Adds an input named `name`: its id made from the name (and never one a copy still holds a value for), the type's
   *  blank default. */
  function addInput(type, name) {
    const id = holderWidgetId(), w = id === null ? null : lib.get(id);
    if (!w) return;
    const taken = new Set((Array.isArray(w.inputs) ? w.inputs : []).filter(plain).map((i) => i.id).filter((i) => typeof i === "string"));
    for (const { o } of copiesOf(id)) if (plain(o.inputs)) for (const k of Object.keys(o.inputs)) taken.add(k);
    const input = { id: inputIdFrom(name, taken), name: points(name, 60), type, default: type === "color" ? "#ffffff" : "" };
    editOpen((c) => { c.inputs = [...(Array.isArray(c.inputs) ? c.inputs : []), input]; });
  }
  $("wdNewName").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      const name = $("wdNewName").value.trim(), type = wd.adding;
      endAdding();
      if (name && type) addInput(type, name);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      endAdding();
    }
  });
  $("wdNewName").addEventListener("blur", () => { if (!$("wdNewName").value.trim()) endAdding(); });

  /** Delete…: says which copies set the input, then writes its default in wherever the widget uses it, so the widget
   *  looks the same, and removes it. Copies' values for it stay in their files, unused. */
  function deleteInput(inputId) {
    const id = holderWidgetId();
    if (id === null || isReadOnly(id)) return;
    const input = defOf(id).inputs.find((i) => i.id === inputId);
    if (!input) return;
    const setting = copiesOf(id).filter(({ o }) => plain(o.inputs) && Object.hasOwn(o.inputs, inputId)), n = setting.length;
    const shows = input.type === "text" ? input.default || "nothing" : input.type === "color" ? WV.color(input.default) ?? "#ffffff"
      : WV.pathOk(spaces(input.default)) ? spaces(input.default).split("/").pop() : "no picture";
    const lines = [];
    if (n) lines.push(`${n} cop${n === 1 ? "y sets" : "ies set"} ${input.name}: ${copyList(setting)}. ${n === 1 ? "It'll" : "They'll"} show ${shows}.`);
    lines.push(`Delete the input ${input.name}? Wherever the widget uses it, its default is written in, so it looks the same.`);
    if (!confirm(lines.join("\n\n"))) return;
    editOpen((w) => writeDefaultIn(w, input));
  }
  /** Writes input `input`'s default in wherever widget content `w` uses it, then removes the input (docs/widgets.md,
   *  Inputs and bind buttons). A reference of another kind than the input's showed as written, and still does. */
  function writeDefaultIn(w, input) {
    const id = input.id, d = input.default, isRef = (v) => WV.refOf(v) === id;
    const safe = WV.makeSafe(points(d, 2000)), colour = WV.color(d) ?? "#ffffff", path = WV.pathOk(spaces(d)) ? spaces(d) : "";
    const inText = (v) => (typeof v === "string" ? WV.replaceRefs(v, (b) => (b === id ? safe : null)) : v);
    const kindsOf = new Map();  // a widget its copies are of -> its inputs' types (null when it can't be read)
    const kinds = (wid) => {
      if (!kindsOf.has(wid)) {
        const def = typeof wid === "string" && lib.has(wid) ? defOf(wid) : null;
        kindsOf.set(wid, def && !def.problem ? new Map(def.inputs.map((i) => [i.id, i.type])) : null);
      }
      return kindsOf.get(wid);
    };
    if (input.type === "color" && isRef(w.background)) w.background = colour;
    for (const l of layersOf(w)) {
      for (const o of objectsIn(l)) {
        if (o.type === "widget") {  // a copy inside it, passing the input down
          if (!plain(o.inputs)) continue;
          for (const [k, v] of Object.entries(o.inputs)) {
            if (typeof v !== "string" || (kinds(o.widget)?.get(k) ?? input.type) !== input.type) continue;
            if (input.type === "text") o.inputs[k] = inText(v);
            else if (isRef(v)) o.inputs[k] = input.type === "color" ? colour : path;
          }
        } else if (input.type === "text") {
          if (o.type === "text") o.text = inText(o.text);
        } else if (input.type === "color") {
          if (o.type === "text") {
            if (isRef(o.color)) o.color = colour;
            if (plain(o.outline) && isRef(o.outline.color)) o.outline.color = colour;
          } else if (o.type === "shape") {
            if (plain(o.fill) && isRef(o.fill.color)) o.fill.color = colour;
            for (const s of Array.isArray(o.fill?.stops) ? o.fill.stops : []) if (plain(s) && isRef(s.color)) s.color = colour;
            if (plain(o.stroke) && isRef(o.stroke.color)) o.stroke.color = colour;
          } else if (isMedia(o) && isRef(o.tint)) o.tint = colour;
        } else if (isMedia(o) && isRef(o.src)) o.src = path;  // "" leaves it undrawn, as it was
      }
    }
    w.inputs = (Array.isArray(w.inputs) ? w.inputs : []).filter((i) => !(plain(i) && i.id === id));
  }

  /** Preview with: the defaults, or any copy of this widget, by holder and name (what the editor draws; view only). */
  function showPreviewChoice(id) {
    const copies = copiesOf(id).filter(({ o }) => typeof o.id === "string");
    const current = view.preview ? JSON.stringify({ holder: view.preview.holder, copy: view.preview.copy }) : "";
    const key = JSON.stringify([id, copies.map(({ key, o }) => [key, o.id, copyName(o), holderName(key)]), current]);
    if (key === wd.previewKey) return;
    wd.previewKey = key;
    const el = $("wdPreview");
    el.replaceChildren(new Option("Defaults", ""), ...copies.map(({ key, o }) => new Option(`${copyName(o)} in ${holderName(key)}`,
                                                                                           JSON.stringify({ holder: key, copy: o.id }))));
    el.value = current;
    if (el.selectedIndex < 0) el.value = "";
  }
  /** Used in: each scene, Global and widget with copies, with how many it draws; each opens it. */
  function showUsedIn(id) {
    const uses = (WV.uses(lib, doc, closureMemo()).get(id) || []).filter((u) => u.count > 0 && u.holder !== `widget:${id}`);
    const key = JSON.stringify([id, uses.map((u) => [u.holder, holderName(u.holder), u.count])]);
    if (key === wd.usedKey) return;
    wd.usedKey = key;
    if (!uses.length) return $("wdUsedIn").replaceChildren(make("span", "small muted", "nowhere yet: drag its card onto a scene"));
    $("wdUsedIn").replaceChildren(...uses.flatMap((u, i) => {
      const b = make("button", "link", `${holderName(u.holder)} (${u.count})`);
      b.title = `Open ${holderName(u.holder)}`;
      b.addEventListener("click", () => openHolder(u.holder, { back: true }));
      return i ? [make("span", "muted", "·"), b] : [b];
    }));
  }

  /** Size: the container changes, anchored top-left; every copy keeps its scale, so it grows or shrinks to the right
   *  and down. One step, the copies' stored sizes included. */
  function setSize() {
    const w = Number($("wdW").value), h = Number($("wdH").value), id = holderWidgetId();
    if (id === null || !lib.has(id)) return;
    if (!Number.isInteger(w) || !Number.isInteger(h) || w < 1 || w > 1600 || h < 1 || h > 720) {
      const def = defOf(id);
      [$("wdW").value, $("wdH").value] = [def.w, def.h];
      return message("A widget is 1 to 1600 across and 1 to 720 down, in whole pixels.", "warn");
    }
    editOpen((c, id) => {
      const before = defOf(id), copies = copiesOf(id, holders({ editable: true })).map(({ o }) => [o, copyScale(o, before)]);
      [c.w, c.h] = [w, h];
      const after = defOf(id);
      for (const [o, s] of copies) fitCopy(o, s, after);
    });
    useCanvas();
  }
  /** Fit to content: the container becomes the box around what's drawn, and everything in it moves to (0, 0); every
   *  copy moves the other way by as much times its scale, so at a whole scale nothing moves on any scene. */
  function fitToContent() {
    const id = holderWidgetId(), content = id === null ? null : lib.get(id);
    if (!content || isReadOnly(id)) return;
    const objs = layersOf(content).filter((l) => l.visible !== false).flatMap((l) => objectsIn(l).filter((o) => o.visible !== false));
    if (!objs.length) return message("There's nothing in the widget to fit it to.", "warn");
    const box = contentBox(objs), def = defOf(id);
    if (box.x === 0 && box.y === 0 && box.w === def.w && box.h === def.h) return message("It fits its content already.");
    editOpen((c) => {
      const copies = copiesOf(id, holders({ editable: true })).map(({ o }) => [o, copyScale(o, def)]);
      for (const l of layersOf(c)) for (const o of objectsIn(l)) [o.x, o.y] = [num(o.x, 0) - box.x, num(o.y, 0) - box.y];
      [c.w, c.h] = [box.w, box.h];
      const after = defOf(id);
      for (const [o, s] of copies) {
        [o.x, o.y] = [num(o.x, 0) + WV.rnd(s * box.x), num(o.y, 0) + WV.rnd(s * box.y)];
        fitCopy(o, s, after);
      }
    });
    useCanvas();
    if (box.cut) message("What's in it is bigger than a widget can be (1600 × 720): the widget is now as big as it can be.", "warn");
  }

  $("wdName").addEventListener("input", () => editOpen((c) => { c.name = points($("wdName").value, 80); }, "wdName"));
  $("wdDescription").addEventListener("input", () => editOpen((c) => { c.description = points($("wdDescription").value, 500); }, "wdDescription"));
  $("wdW").addEventListener("change", setSize);
  $("wdH").addEventListener("change", setSize);
  $("wdFit").addEventListener("click", fitToContent);
  $("wdBgNone").addEventListener("change", () => editOpen((c) => { c.background = null; }));
  $("wdBgOn").addEventListener("change", () => editOpen((c) => { c.background = hex($("wdBgColor").value, "#000000"); }));
  $("wdBgColor").addEventListener("input", () => editOpen((c) => { c.background = hex($("wdBgColor").value, "#000000"); }, "wdBgColor"));
  for (const b of $("wdBackdrop").children) b.addEventListener("click", () => seg("wdBackdrop", setBackdrop(b.dataset.value)));
  $("wdAddText").addEventListener("click", () => askInputName("text"));
  $("wdAddColor").addEventListener("click", () => askInputName("color"));
  $("wdAddImage").addEventListener("click", () => askInputName("image"));
  $("wdPreview").addEventListener("change", () => {
    let p = null;
    try { p = $("wdPreview").value ? JSON.parse($("wdPreview").value) : null; } catch {}
    view.preview = p;
    changed({ save: false });
  });
  const forOpen = (run) => () => { const id = holderWidgetId(); if (id !== null) run(id); };
  $("wdReveal").addEventListener("click", forOpen(showFolder));
  $("wdChange").addEventListener("click", forOpen(openChangeFolder));
  $("wdDuplicate").addEventListener("click", forOpen(openDuplicate));
  $("wdExport").addEventListener("click", forOpen(openExport));
  $("wdDelete").addEventListener("click", forOpen(openDelete));

  WV.hooks.props.push((p) => {
    // With no object selected in a widget, its own properties show, over the layer's when a layer is selected: a
    // layer stays selected once an object on it was, and the widget's properties would be out of reach.
    if (isWidgetHolder() && !p && $("pWidget").hidden && lib.has(holderWidgetId())) {
      $("pWidget").hidden = false;
      showWidgetProps(holderWidgetId());
    }
    $("wcFromSel").disabled = !canMakeFromSelection();  // the selection decides whether From selection can be used
  });
  // texts to fill in with live values: the open widget's text defaults, for their rows' "shows …", and the live text
  // the widgets' cards show while the Widgets tab is open
  WV.texts.push(() => {
    const id = holderWidgetId();
    return id === null || !lib.has(id) ? [] : defOf(id).inputs.filter((i) => i.type === "text").map((i) => i.default);
  });
  WV.texts.push(() => (strip.tab === "widgets" ? [...strip.live.values()].flat() : []));

  WV.fromSelection = fromSelection;
  // what studio.js calls: the strip (showStrip, renderThumbs), #pWidget (fillProps), Duplicate to edit
  // (duplicateToEdit) and a widget changed in two places (askConflict)
  Object.assign(window, { showWidgetCards, renderWidgetCards, showWidgetProps, duplicateWidget: openDuplicate, openConflict });
})();

WV.loaded.library = true;
