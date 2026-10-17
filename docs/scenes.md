# Scenes: layers and objects (spec)

Each playlist entry is a **scene**: a 1600x720 canvas with a background colour and a stack of **layers**,
each holding **objects** (images, videos, text, shapes). The player draws scenes from the original files.
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
  - `pixel` is how many source pixels make one art pixel. Only the editor uses it, to offer whole-number
    sizes; it's 1 for photos and video.
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

### Drawing a frame at time t

1. Start from the scene's background colour.
2. For each visible layer, bottom to top: draw its visible objects, bottom to top, into a transparent
   buffer at time t, each at its own opacity. Then blend the buffer over the frame at the layer's
   opacity.
3. Draw the global layers the same way, on top.

### Timeline

- **No animation:** a scene with no animated objects is a still.
- **With animation:** the scene's cycle is the least common multiple of its animated objects' loop
  lengths, capped at 60 s. Frames are made at every moment any object's frame changes within the cycle,
  with at least 20 ms between frames. One animated object therefore gives exactly its own frames, as
  today.
- **Caching:** the player prepares the cycle once and keeps it in its caches as now, keyed on the scene's
  JSON (with the global layer) and the stamps of the files it uses.

### Live text (data.md)

- **Where it's drawn:** a live text object is drawn at its true place in the stack when nothing above it,
  in the scene or the global layer, is animated. In that case the player keeps whatever is above as a
  still RGBA image and blends it over the redrawn text.
- **Otherwise** it's drawn on top of everything. The studio shows a note on such objects.
- **Cost:** it's only redrawn when its shown text changes, as now.

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
    the canvas), Shape (R, with a menu for rect, ellipse and line), and Hand and zoom (H, and Space
    to drag).
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
    - **A layer:** opacity and name.
    - **Nothing selected:** the scene's name, seconds and background.
- **The playlist:** a strip of scene thumbnails with their seconds. **Global** is pinned first. Click a
  scene to edit it; add, duplicate, delete and reorder scenes. There are Play/Stop and Start with Windows
  controls, and the Data sources window as now.
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
