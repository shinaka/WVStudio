# Scenes: layers and objects (spec)

Each playlist entry is a **scene**: the panel's 1600x720 canvas (a widget has its own size) with a
background colour and a stack of **layers**, each holding **objects** (images, videos, text, shapes and
copies of widgets). The player draws scenes from the original files.
The studio is an editor for them, in the spirit of Photoshop or Figma. This replaces the one piece file
plus text model.

## `playlist/playlist.json`, version 2

```json
{
  "version": 2,
  "seconds": 30,
  "global": { "layers": [ <layer> ] },
  "scenes": [
    { "id": "s1", "name": "city", "seconds": 30, "background": "#000000", "layers": [ <layer> ] }
  ]
}
```

- `global` is drawn over every scene: its layers sit above all of the scene's.
- **Lists are in drawing order:** bottom first, top last. That applies to `layers` and to each layer's
  `objects`. The studio's layers panel shows them reversed, top at the top.
- Keep unknown keys when rewriting the file.
- `seconds` is how long the scene shows before the next one. A playlist with only one scene loops it forever.

### Layer

```json
{ "id": "l1", "name": "Art", "visible": true, "locked": false, "opacity": 1.0, "objects": [ <object> ] }
```

A layer's opacity applies to the layer **as a whole**, like a Photoshop group. Its objects are first
drawn together onto a transparent buffer, which is then blended onto the scene at that opacity. So where
objects overlap, they don't show through each other. A hidden layer isn't drawn.

### Object: the common keys

```json
{ "id": "o1", "type": "image", "name": "city", "visible": true, "locked": false, "opacity": 1.0,
  "x": 0, "y": 0, "w": 1600, "h": 720, "rotate": 0, "flip": false }
```

- `x`, `y`, `w`, `h`:
  - For image, video and shape objects: the box in panel pixels, as drawn after rotation. For a
    90°/270° rotation, `w`/`h` are the box's on-screen size.
  - For text: `x`/`y` are the anchor (text.md), and `w`/`h` are ignored.
  - For a widget copy: `x`/`y` are the container's top-left, and `w`/`h` are only for older tools; its
    size comes from `scale` (widgets.md).
  - A position that isn't a whole number is rounded half up (`floor(v + 0.5)`), in the player and the
    studio alike, so −1.5 becomes −1.
- `rotate` is 0, 90, 180 or 270 (clockwise), and `flip` mirrors left to right after rotating. Text
  ignores both for now.
- `opacity` is 0..1, and multiplies within the layer's buffer.
- `locked` is only for the editor: a locked object can't be selected on the canvas. Renderers ignore it.

### Object types

- **image / video**
  - Keys: `"src": "sources/<hash>.<ext>"` (relative to the playlist folder), `"smooth": false`,
    `"fill": "none" | "extend" | "mirror" | "tile"`, and `"pixel": 3` (the editor only; see below).
  - **Drawing:** the source is first turned and flipped, then scaled to `w`x`h`.
    - With `smooth` false, scaling is nearest neighbour. Screen pixel `u` (0..w-1) shows source pixel
      `floor((2u + 1) * srcw / (2w))`, and the same for rows. That's exactly today's rule, so the
      preview and the player match pixel for pixel.
    - With `smooth` true, it's a good-quality resample (Lanczos in the player); the preview may differ
      slightly.
  - **Fill:** `extend`, `mirror` and `tile` repeat the object outward to cover the **whole canvas**, as
    today's fill modes do, still within its layer. `none` draws only its box.
  - **Animation:** animated images (GIF, APNG, WebP) and videos loop on their own timing. Video is read at
    30 fps or less, and only its first 60 s are used.
  - **Files:** images are PNG, APNG, GIF, WebP, JPG and BMP; videos MP4, M4V, MOV, WebM, MKV and AVI. A
    video is always read as the container its extension names, so a WebM renamed `.mp4` doesn't play.
  - `pixel` is how many source pixels make one art pixel. Only the editor uses it, to offer whole-number
    sizes; it's 1 for photos and video.
  - `"keep_aspect": true` fits the file inside its box, keeping its shape, centred. `"tint": "#rrggbb"`
    gives every pixel the file draws that colour, keeping its transparency; `null` draws it as it is.
    Both are described in widgets.md ("Images in a box"). The studio offers Tint everywhere and the
    keep-aspect tick box inside widgets.
- **text:** the text keys from text.md (text, font, size, color, align, valign, line_height, outline
  and crisp, with `x`/`y` as the anchor), plus the common keys. Its `opacity` replaces the one in text.md.
  Live placeholders work as described in data.md.
- **shape**
  - `"shape": "rect" | "ellipse" | "line"`, plus `"radius": 0` (corner radius, for rects).
  - `"fill"` is one of:
    - `{"type": "solid", "color": "#rrggbb"}`;
    - `{"type": "linear", "angle": 90, "stops": [{"at": 0, "color": "#..."}, {"at": 1, "color": "#..."}]}`;
    - `null`.
  - `"stroke": {"color": "#rrggbb", "width": 2}` or `null`.
  - `"crisp": false`.
  - **Geometry:** a line runs from (x, y) to (x+w, y+h), so `w`/`h` can be negative for a line. The
    stroke is centred on the outline. `angle` is in degrees: 0 runs left to right, 90 top to bottom.
  - **Crisp:** a pixel is painted exactly when its centre is inside the fill or stroke region, with no
    anti-aliasing. Both renderers must follow that rule, so crisp shapes match exactly.
  - Shapes ignore `rotate` and `flip`.
- **widget:** a copy of a widget from the `widgets\` folder next to the playlist's, drawn here at a
  scale, with its own values for the widget's inputs.
  - Keys: `"widget": "<id>"`, `"scale": 1.5`, `"inputs": {"label": "GPU"}`, plus the common keys.
  - `scale` is what counts: the copy is the widget's container times `scale`. `w` and `h` are written
    for older tools, and renderers ignore them. Copies don't turn or flip (`rotate` and `flip` are
    ignored), because text and shapes can't.
  - The whole format, the inputs and the drawing rules are in widgets.md.

### Drawing a frame at time t

1. Start from the scene's background colour.
2. For each visible layer, bottom to top: draw its visible objects, bottom to top, into a transparent
   buffer at time t, each at its own opacity. Then blend the buffer over the frame at the layer's
   opacity.
3. Draw the global layers the same way, on top.

A copy of a widget is drawn as its objects, in place in its layer's list, each clipped to the copy's box.
A copy below 100% opacity, and a widget layer below 100%, is a group like a scene layer: drawn whole onto
a buffer, then blended at that opacity. At 100% its objects are drawn straight into the layer, which
gives the same picture.

### Timeline

- **No animation:** a scene with no animated objects is a still.
- **With animation:** the scene's cycle is the least common multiple of its animated objects' loop
  lengths, capped at 60 s. Frames are made at every moment any object's frame changes within the cycle,
  with at least 20 ms between frames. One animated object therefore gives exactly its own frames, as
  today.
- **Caching:** the player prepares the cycle once and keeps it in its caches as now, keyed on the scene's
  JSON (with the global layer) and the stamps of the files it uses.

### Live text (data.md)

The player decides once per scene where its live text goes, copies expanded:

1. Take the scene's layers, then the global layers, in drawing order. Find the first live text, P.
2. Find S, where P's group starts: P's top-level layer if that layer is below 100%; otherwise the
   outermost copy or widget layer below 100% around P; otherwise P itself.
3. If nothing animated is drawn from S upward (S's own content included), every live text is drawn **in
   place**: exactly where it sits in the stack, inside its groups and clips, as if it were static text.
   Translucent groups are kept whole, so a faded copy's value fades with its panel.
4. **Otherwise** every live text is drawn on top of everything, in drawing order, at its opacity times the
   opacities of its layer and its groups, clipped to its copy. The layers panel marks such text
   `live, on top`.

- Live text in a hidden layer isn't drawn.
- **Cost:** it's only redrawn when its shown text changes. While playing, the frames hold everything below
  S; what's from S upward is kept as stills, groups and live texts, put together again when a text
  changes, then laid over each frame.
- **Over an animation, values update less often.** A change means every frame of the scene's cycle is
  blended and encoded again. The player keeps that to about 2% of one core: after a redraw that took `c`
  seconds, the next starts no sooner than `max(0.5 s, 50 × c)` after the last one started. Over a still
  that's twice a second at most; over a long video it can be minutes. The studio says so in the scene's
  note.

## Moving from version 1

The studio converts the file the first time it reads one, keeping a copy as `playlist.v1.json`. The player
reads both versions the same way.

- **Each item becomes a scene**, named after its file's stem, with the same seconds and a black background:
  - Layer "Art" holds one image or video object. Its `src` is the item's `source`, and its box comes from
    `layout`: width, height, x, y, rotate, flip, smooth, fill, plus the layout's background colour.
  - Pieces without a kept original instead use the piece file itself, copied into `sources/`, at 0,0
    1600x720.
  - Layer "Text" holds the item's `widgets`, as text objects.
- **The old `overlay`** becomes the global layer's single layer, "Text".

## The player

- **Drawing:** it reads version 2, and version 1 through the conversion above, then draws scenes as
  specified.
  - Images are drawn in the existing parallel encode path, then rotated 180° and JPEG-encoded as now.
  - It keeps the background re-prepare, the settle wait and live data.
- **Render command:** `wvplay render <playlist.json> <scene index> <out.png> [--time MS]` writes the scene
  at that time, upright.
- **Media:** each media object is decoded once per file and size, and shared between objects.

## The studio

- **Editor layout:**
  - **Left:** a toolbar with Move/Select (V), Text (T), Image/Video (I: a file picker, or drop a file onto
    the canvas), Shape (R, with a menu for rect, ellipse and line), Widget (W: pick a widget, then click
    where its copy goes), and Hand and zoom (H, and Space to drag).
  - **Centre:** the canvas, with guides, the camera hole, snapping and corner handles as now.
  - **Right, top: Layers panel.** Layers with their objects nested under them, top first. Drag to reorder
    objects, or to move them between layers. Eye and lock toggles, double-click to rename, Add and Delete
    layer, and badges for animated and live objects.
  - **Right, bottom: Properties panel** for the selection:
    - **Any object:** opacity, x/y/w/h, rotate and flip.
    - **Images and video:** whole-number scale steps based on `pixel`, smooth, fill, fit buttons, and the
      background colour from the eyedropper.
    - **Text:** the text options, with autocomplete.
    - **Shapes:** fill (solid or gradient), stroke, radius, crisp.
    - **A widget copy:** the widget, its scale, and a row per input (widgets.md).
    - **A layer:** opacity and name.
    - **Nothing selected:** the scene's name, seconds and background.
- **The playlist:** a strip of scene thumbnails with their seconds. **Global** is pinned first. Click a
  scene to edit it; add, duplicate, delete and reorder scenes. There are Play/Stop and Start with Windows
  controls, and the Data sources window as now. A **Widgets** tab beside **Scenes** holds the widget
  library: a card per widget to open or drag onto the canvas, plus New, From selection and Import
  (widgets.md).
- **Saving:** edits save as you make them (debounced), and the player picks them up. There's no Add or
  Update step any more.
  - Media uploads go into `sources/` (named by content hash, as now) and are tidied when no object uses
    them.
  - Export files (PNG, GIF and MP4 of a scene) can stay as a secondary action, done by the player:
    `wvplay render`, or a new export command if it's simple.
- **Preview:** the canvas draws the scene as the spec says. It uses the same nearest-neighbour rule for
  images, so pixel art matches the player exactly, and its crisp shapes follow the pixel-centre rule.
- **Editing basics:** undo and redo (Ctrl+Z / Ctrl+Y) over the scene document; delete; copy, paste and
  duplicate (Ctrl+D); arrow-key nudges.
