# Text widgets and the global layer (spec)

Text drawn over the art: per playlist piece, plus a **global layer** drawn over every piece. The player
renders it (so text can later come from outside sources while playing); the studio edits it and shows a
close preview.

## Where it lives: `playlist/playlist.json`

```json
{
  "seconds": 30,
  "overlay": [ <widget>, ... ],
  "items": [
    { "file": "city.png", "seconds": 20, "widgets": [ <widget>, ... ], "name": "...", "source": "...", "layout": {...} }
  ]
}
```

- `overlay`: the global layer, drawn over every piece. Optional; missing means none.
- `items[].widgets`: that piece's own text. Optional; missing means none.
- Draw order: the piece's pixels, then its `widgets` in list order, then `overlay` in list order.
- Anything that rewrites playlist.json must keep keys it doesn't know about (`overlay`, `widgets`, and future ones).

## A widget

```json
{
  "id": "w1",
  "type": "text",
  "text": "Hello\nworld",
  "font": "C:\\Windows\\Fonts\\segoeui.ttf",
  "size": 48,
  "color": "#ffffff",
  "opacity": 1.0,
  "x": 100,
  "y": 100,
  "align": "left",
  "valign": "top",
  "line_height": 1.2,
  "outline": { "color": "#000000", "width": 0 },
  "crisp": false
}
```

| key | meaning | default |
|---|---|---|
| `id` | unique within its list; stable, so outside sources can target it later | required |
| `type` | only `"text"` for now; renderers skip types they don't know | required |
| `text` | the text; `\n` starts a new line. `{name}` placeholders are reserved for injected values later: for now they're drawn as written | `""` |
| `font` | absolute path to a `.ttf` or `.otf` file. If it's missing or unreadable, use `C:\Windows\Fonts\segoeui.ttf`, and if that fails, skip the widget | Segoe UI |
| `size` | em size in panel pixels (the 1600x720 space) | 48 |
| `color` | `#rrggbb` | `#ffffff` |
| `opacity` | 0..1, applies to fill and outline | 1 |
| `x`, `y` | the anchor point in panel pixels; see align/valign | 0, 0 |
| `align` | `left`: lines start at x; `center`: lines are centred on x; `right`: lines end at x. Applies per line | `left` |
| `valign` | `top`: the block's top is y; `middle`: its middle is y; `bottom`: its bottom is y | `top` |
| `line_height` | distance between baselines, as a multiple of `size` | 1.2 |
| `outline.width` | outline thickness in panel pixels, drawn around the glyphs under the fill; 0 means none | 0 |
| `outline.color` | `#rrggbb` | `#000000` |
| `crisp` | true: no anti-aliasing (alpha thresholded at 50%), for pixel fonts | false |

### Geometry (both renderers follow this, so the preview matches the player)

- `ascent` and `descent` are the font's horizontal line metrics at `size`. `descent` is a positive
  magnitude. The player gets them from fontdue (`horizontal_line_metrics`); the browser uses
  `measureText().fontBoundingBoxAscent` / `fontBoundingBoxDescent`.
- With n lines, the block height is `ascent + descent + (n - 1) * size * line_height`.
- The block's top is `y` (top), `y - height / 2` (middle) or `y - height` (bottom).
- Line i's baseline is at `top + ascent + i * size * line_height`.
- A line's width is the sum of its glyph advances, with kerning. It starts at `x` (left), `x - width / 2`
  (center) or `x - width` (right).
- The outline is the glyph coverage grown by `outline.width` in every direction (a round dilation),
  painted in `outline.color` before the fill. In the browser that's `strokeText` with
  `lineWidth = 2 * width` and `lineJoin = "round"`, drawn before `fillText`.
- Text can hang off the screen; it's clipped.
- Everything is in upright panel coordinates. The player's 180° rotation happens after compositing.

## The player (player/)

- It reads `overlay` and `items[].widgets` from playlist.json.
- When it prepares a piece's frames, it renders the piece's widgets plus the global layer once into a
  1600x720 RGBA layer. It composites that onto every frame (only the layer's bounding box), then rotates
  and JPEG-encodes as before. Static text costs nothing extra while playing.
- The prepared-frames caches (in-memory and on disk) are keyed on the widgets, the overlay and the font
  files' size and modified time, so editing text re-prepares the piece. Changing the global layer
  re-prepares every piece as it comes up.
- `wvplay render <playlist.json> <item index> <out.png> [--frame N]` writes one composited frame, upright,
  as a PNG, for checking text without the cooler.
- Later, for injected text: the same layer renderer will run while playing, for widgets whose text has
  placeholders. Keep the renderer callable on its own (frame in, layer out); don't bake assumptions
  that it only runs at prepare time.

## The studio

- `GET /api/fonts` lists `[{path, family, style}]` for the `.ttf`/`.otf` files in `C:\Windows\Fonts`,
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts` (fonts installed per user) and the project's `fonts/` folder.
  Names come from Pillow, and the list is cached.
- `GET /api/font?path=...` serves a font file, only if it's in that list.
- A "Text" section edits widgets in one of two layers at a time, **This piece** or **On top of
  everything** (the global layer):
  - Add, select or remove widgets.
  - Edit the properties above: a font picker filterable by name, size, colour, opacity, align, valign,
    line height, outline colour and width, crisp.
  - Click a widget's text on the preview to select it, and drag to move it, with the same edge and
    curve-line snapping as art.
- The preview draws widgets on the screen canvas: the art, then this piece's widgets, then the global
  layer, which always shows. It loads each font with the FontFace API from `/api/font`.
- Saving:
  - The global layer saves to playlist.json's `overlay` as you edit (debounced).
  - This piece's widgets travel with the layout. Add/Update in playlist stores them as the item's
    `widgets`.
  - While editing a piece that's already in the playlist, widget edits also save to that item as you go,
    without recomposing its art. Text changes don't need a video re-encode.
  - The player picks either change up within about a second.
