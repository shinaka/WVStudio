# Widget test fixtures

Shared vectors and fixtures for widgets (docs/widgets.md). The player's Rust tests and the studio's headless
page check both read them, and both must reproduce them exactly. Neither changes them: when one disagrees,
the spec's text decides, and a file here changes only when the spec says it's wrong.

**The expected values were worked out by hand from the spec** (docs/widgets.md, sections 4 and 5: inputs,
substitution, geometry, clips, groups and limits), not by running an implementation. Python only made and
named the tiny media files, wrote the hand values out as JSON, and evaluated `floor(v + 0.5)` for `rnd.json`.

`note` keys in any file are for people. Readers ignore them.

## Vectors

| file | rows | checks | read by |
|---|---|---|---|
| `inputs.json` | `{text, inputs, result}` | the text pass, `substitute` | widget.rs, `WV.substitute` |
| `safe.json` | `{value, safe}` | a value made safe, `make_safe` | widget.rs, `WV.makeSafe` |
| `colors.json` | `{value, color}` | the colour rule: `#rrggbb` or null | widget.rs `color`, `WV.color`, studio.py `check_color` |
| `aspect.json` | `{box, native, rotate, result}` | `keep_aspect`'s fitted box | scene.rs `fit_aspect`, `WV.fitAspect` |
| `refs.json` | `{text, id, value, result}` | Delete input's rewrite | the page check only |
| `live.json` | `{text, live, plain}` | data.rs `is_live`, and `fill` with no values, as the code is today | data.rs, `WV.isLive`, `WV.plain` |
| `rnd.json` | `{v, rnd}` | `floor(v + 0.5)` in 64-bit floats | widget.rs `rnd`, `WV.rnd` |

- **inputs.json:** `inputs` holds a copy's text values as written in a file. Cut each to 2000 code points
  first, as a copy's values are, then pass them to `substitute` as the text inputs. A colour or image input
  isn't listed: it isn't a text input.
- **aspect.json:** `box` is the placed box `[x, y, w, h]`; `native` is the file's own `[w, h]`; `rotate` is the
  object's, and 90 or 270 swaps `native` before fitting. `result` is `[x, y, w, h]`.
- **refs.json:** the call is `WV.replaceRefs(text, b => b === id ? WV.makeSafe(value) : null)`.
- **live.json** and **safe.json:** data.rs trims a key with Rust's `str::trim` (Unicode White_Space), so
  `{U+0085}` is a blank key and `{U+FEFF}` isn't. JavaScript's `trim` does the opposite for both, so a port
  must trim as Rust does.
- **rnd.json:** a row with `a` and `b` is a product the renderers compute from two JSON numbers. Both
  `rnd(v)` and `rnd(a * b)` must give `rnd`, and `a * b` is exactly `v`. A JSON parser that isn't correctly
  rounded gets some of them wrong.

## `expand/<case>/`

Each case is a playlist and its widgets, as they'd sit on disk: `playlist/playlist.json` (with its
`sources/` and `inputs/` files), and `widgets/<id>/widget.json` with its `media/`. Widget media are named
`<stem>-<first 8 hex digits of the SHA-1>.<ext>`.

| case | covers |
|---|---|
| `basic` | the worked example of docs/widgets.md, plus a negative half (`c/n` at 9, `d/n` at −4), a positive half (`e` at 2.5) and a 64-bit text anchor (`f/u` at 4) |
| `inputs` | text, colour and image inputs and their defaults; values that aren't colours; fallbacks inside the widget; a bound tint; unknown and wrong-type references; values made safe; a reference that builds a placeholder; image paths from the playlist and from the widget; `""`; `{$x}` values at the top level |
| `nested` | a copy inside a widget, with values passed down by `{$id}` and inline; nested placement, clips and group ids |
| `limits` | scenes `cycle` (A in A, A → B → A), `deep` (9 deep), `budget` (the work count, cut part way through a copy), `sizes` (placed media over 6400 × 2880, `scale` from `w`, clamping); and `bytes`, added later by hand in the same way: a copy's text value cut to 2000 again after its own text pass, at the top level and passed down; a text inside a copy cut to 4000 before the live parse, the cut falling inside a placeholder and inside a `}}`; copy, layer and object ids cut to 64. Each cut counts code points: an emoji before it counts as one |
| `bounds` | the same cuts from other sides, added later in the same way as `bytes`: scene `values` (a copy's text value cut to 2000 again after its own text pass, where the pass made 800,000 characters, and an emoji value cut there; a text inside a copy cut to 4000 before the live parse: after an emoji, between doubled braces, part way through a placeholder, so nothing is live) and scene `ids` (copy, layer and object ids cut to 64 in leaf and group ids, right after an emoji; two copies whose ids become the same; a scene's own object keeps its id) |
| `missing` | copies that draw nothing: no folder, a folder `CPU-Badge` for the id `cpu-badge`, ids that aren't ids (`../x`, `Gauge`, `con`, none at all), a `widget.json` that isn't JSON or isn't an object, `version: 2`, a folder without `widget.json`, a `widget.json` one byte over 1 MiB (`heavy`: a widget that would draw, written on one line so line endings can't change its size); the rest of a widget around a missing inner copy still draws, in a scene and in the global layer |
| `paths` | `src` values left out (`../x.png`, `C:/x.png`, `/x.png`, `\\host\x.png`, `media\x.png`, `media./x.png`, five parts, `media/CON.png`) and kept (four parts, spaces around it); an image input whose default isn't a path, set by a copy; fonts that aren't one file name fall back to Segoe UI, `Orbitron Bold.ttf` is kept |
| `fills` | tile, mirror and extend inside a small container, at 1, 1.5 and nested at 2: each leaf keeps its own placed box (one wholly outside the container too) and gets the container as its clip, which is the fill area; a translucent copy hanging off the panel's top-left |
| `aspect` | `keep_aspect` and `tint` carried on the leaves: bound tints, `#ABC` and spaces made `#rrggbb`, tints that aren't colours (`blue`, `{$nope}`, an image input, a number) and a `keep_aspect` that isn't `true` dropped; a scene image's own. Also colours that aren't strings (a number, `null`, `false`, a list, an object) in a fill, a gradient stop, a stroke, a text and its outline: each is its fallback, and the stroke or gradient it's in stays |
| `live` | where live text is drawn: two gauge copies in one layer (in place, cut at the first value), a copy at 0.5 (cut at the copy), a scene layer at 0.5 (cut at the layer), a GIF above (on top), a hidden layer's live text (left out), and a scene whose only live text is the global clock |

The last five came later, worked out by hand from the spec in the same way and checked against an independent
reference.

`expected.json` lists, for each scene and for the global layers, every object drawn, in drawing order, with
copies expanded. A top-level object that isn't a copy comes out as it is, with no clip and no groups.

```json
{ "scenes": [ { "id": "s1", "placement": { "on_top": false, "cut": null }, "leaves": [ <leaf> ] } ], "global": { "leaves": [ <leaf> ] } }
```

A leaf's keys depend on its type:

- **every leaf:** `id` (`<copy id>/<object id>`, deeper for nested copies), `type` (`image`, `text` or
  `shape`; these cases have no videos), `opacity` (its own), `clip` (`[left, top, right, bottom]`, right and
  bottom exclusive, or null), `groups` (`[[id, opacity, rect]]`, outermost first).
- **image:** `x`, `y`, `w`, `h` (the placed box), `src` (relative to the case folder, with `/`),
  `keep_aspect`, `tint` (`#rrggbb` or null).
- **text:** `x`, `y` (the anchor; no `w` or `h`), `text` (as drawn: substituted, and static text's `{{ }}`
  made plain), `live`, `size`, `outline_width`, `color`, `outline_color`, `font` (the file name;
  `segoeui.ttf` for Segoe UI, which an empty font means).
- **shape:** `x`, `y`, `w`, `h`, `shape`, `radius`, `color` (a solid fill's colour, or null for no fill),
  `stroke_width` and `stroke_color` (null for no stroke), `crisp`.

Colours are as the leaf holds them: lowercase `#rrggbb`, with fallbacks written out. Compare numbers as
numbers (4 and 4.0 are equal); opacities to 6 decimals. `outline_color`, `stroke_color`, `shape` and `crisp` go
beyond the spec's list, so that a fallback in an outline or a stroke, and the background's crisp rect, are
checked too.

Each scene's `placement` says where its live text is drawn (docs/widgets.md, Live text): `on_top`, and `cut`,
the id of the first leaf of S, where the first live text's group starts. A scene without live text has
`{"on_top": false, "cut": null}`.

## `parity/`

- `playlist/` and `widgets/`: the pixel fixture. Two scenes (`media`, `groups`) and the global layer, with no
  text, no smooth scaling and only crisp shapes. It covers nearest media at scales 1, 1.5, 2 and 3, at odd,
  half and negative offsets; nested copies; copies and widget layers at 0.5; clips; tile, mirror and extend in
  a container; copies hanging off the panel's edges; `keep_aspect` with a tiled fill; `tint` on an opaque and
  a half-transparent PNG; a `.bmp`; and the same in the global layer.
- `tolerance.json`: the rects where a translucent blend or a partly transparent tinted pixel is drawn. There
  the player's and the page's pixels may differ by at most 2 per channel. Everywhere else they must be equal.
- `text/`: a second fixture, with text, for placement only. Scene `inplace` has live text with nothing
  animated above it, so it's drawn in place. Scene `ontop` adds a GIF above, so it's drawn on top. The live
  values use a key no source has, with a fallback, so the text doesn't change between renders.

A harness copies a fixture to its scratch folder before running `wvplay` on it, and adds the `data.json` it
needs there. Nothing writes into this folder.
