# Widgets: reusable pieces (spec)

A **widget** is a small canvas of its own, made in the studio's editor like a scene: images, videos, text,
shapes, live values and other widgets, on layers. It has a name, a fixed size (its **container**) and, if it
wants, **inputs**. It lives in its own folder, `widgets\<id>\`, so it can be copied to another PC or shared
online. Scenes, the global layer and other widgets use it through **copies**: objects that say "draw widget X
here, at this scale, with these inputs". Copies are linked: editing the widget changes every copy, everywhere.

A widget doesn't need inputs. Its text can be fixed (`CPU`), live (`{pc.cpu.temp:d}°C`) or both, exactly as
text in a scene, and then the widget is simply reusable content. Inputs only mark what each copy may change.
Every input has a default, set in the widget, so a copy sets only what it wants to change.

Scenes and their objects are in [scenes.md](scenes.md), text in [text.md](text.md), and live values in
[data.md](data.md).

## 1. Words

| word | means |
|---|---|
| widget | a reusable piece, defined once in `widgets\<id>\widget.json` |
| copy | one use of a widget in a scene, the global layer or another widget: an object with `"type": "widget"` |
| container | the widget's own canvas, `w` × `h` pixels. A copy is the container times its scale |
| input | a named value (text, colour or image) that a widget declares, with a default. Each copy may set its own |
| Widgets tab | the library of widgets, next to the scenes in the bottom strip |
| Edit widget | opens a widget in the editor. **← Back** returns to where you were |

## 2. The widgets folder

### 2.1 Where it is

- `widgets\` sits **next to the playlist's folder**, as `plugins\` does: `CoolerVideo\playlist\playlist.json`
  uses `CoolerVideo\widgets\`.
- A playlist read from a folder of images, or from a single file, has no widgets.
- Everything that plays from that folder shares the library. There is one playlist, so that's one library.

### 2.2 Layout

```
widgets\
  temp-badge\
    widget.json                      the widget (required)
    media\thermometer-6d3b9a52.png   its images and videos
    fonts\Orbitron-Bold.ttf          fonts that travel with it (optional)
    preview.png                      its picture, written when it's exported (optional)
    README.md                        notes for people you share it with (optional)
  claude-usage\
    widget.json
  .deleted\                          widgets you deleted, kept so they can be restored
  .import-3f9a\                      an import in progress (the studio's own)
```

- Only `widget.json` is required.
- Folders whose names start with `.` are the studio's own. They are never widgets.
- The player and the studio read `widget.json`, the files it names, and (the studio, to share) `preview.png`
  and `README.md`. Anything else in the folder is left alone.

### 2.3 Ids and folder names

- **The folder name is the widget's id.** `widget.json` doesn't repeat it, so a folder copied under a new name
  is simply another widget.
- An id is 1 to 40 characters: `a`–`z` and `0`–`9`, with `-` inside: `^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$`.
  Windows' reserved names (`con`, `prn`, `aux`, `nul`, `com1`–`com9`, `lpt1`–`lpt9`) aren't ids.
- **An id never changes by itself.** Copies refer to it, including copies in widgets you've shared. Renaming a
  widget changes its `name` only. To give it another folder name, use **Change folder name…** (section 6.3):
  it makes the new folder and points every copy on this PC at it.
- The studio makes the id from the name when a widget is made (New, From selection, Duplicate, an import
  that keeps both):
  - lowercase; accents removed; each run of anything but `a`–`z` and `0`–`9` becomes one `-`; `-` trimmed from
    the ends; cut to 40;
  - empty gives `widget`; a reserved name gets `-widget` added;
  - if that id is taken, by a folder in `widgets\` or by a deleted widget in `widgets\.deleted\`, `-2`, `-3`
    and so on are added, cutting the stem so the id stays within 40.
- A folder whose name isn't an id is ignored by the player. The studio lists it as a problem:
  "The folder name `CPU Badge` isn't a widget id: rename it to `cpu-badge`."
- **The match is exact, letter case included.** Windows would open `widgets\CPU-Badge\` for the id
  `cpu-badge`, but neither renderer does: both list `widgets\` and use only a folder whose name is exactly the
  id. So a copy of `cpu-badge` draws nothing on the cooler and in the studio alike until the folder is renamed.

### 2.4 `widget.json`

```json
{
  "version": 1,
  "name": "Temp badge",
  "description": "A label, an icon and a live temperature.",
  "w": 360,
  "h": 120,
  "background": null,
  "fonts": { "bahnschrift.ttf": { "family": "Bahnschrift", "style": "Regular" } },
  "inputs": [
    { "id": "label",  "name": "Label",  "type": "text",  "default": "CPU" },
    { "id": "value",  "name": "Value",  "type": "text",  "default": "{pc.cpu.temp:d}°C" },
    { "id": "accent", "name": "Accent", "type": "color", "default": "#ff7a1a" },
    { "id": "icon",   "name": "Icon",   "type": "image", "default": "media/thermometer-6d3b9a52.png" }
  ],
  "layers": [
    { "id": "l1", "name": "Back", "visible": true, "locked": false, "opacity": 1.0, "objects": [
      { "id": "s1", "type": "shape", "name": "Panel", "x": 0, "y": 0, "w": 360, "h": 120, "shape": "rect",
        "radius": 18, "fill": { "type": "solid", "color": "#101418" },
        "stroke": { "color": "{$accent}", "width": 3 }, "crisp": false },
      { "id": "i1", "type": "image", "name": "Icon", "x": 20, "y": 28, "w": 64, "h": 64,
        "src": "{$icon}", "smooth": false, "fill": "none", "pixel": 4, "keep_aspect": true }
    ] },
    { "id": "l2", "name": "Text", "visible": true, "locked": false, "opacity": 1.0, "objects": [
      { "id": "t1", "type": "text", "name": "Label", "text": "{$label}", "font": "bahnschrift.ttf",
        "size": 30, "color": "#9aa4b2", "x": 104, "y": 22 },
      { "id": "t2", "type": "text", "name": "Value", "text": "{$value}", "font": "bahnschrift.ttf",
        "size": 48, "color": "{$accent}", "x": 104, "y": 54 }
    ] }
  ]
}
```

This widget ships as `widgets\example-temp-badge\` (section 2.6), with every key written out.

| key | meaning | default |
|---|---|---|
| `version` | the file format, 1. A higher number means a newer studio made it: the player skips the widget, and the studio shows it as a problem and never saves over it | 1 |
| `name` | shown in the library and the layers panel; any text, at most 80 characters | the id |
| `description` | shown on the card's tooltip and when importing; at most 500 characters | `""` |
| `w`, `h` | the container in pixels: whole numbers, `w` 1–1600 and `h` 1–720 | 360, 120 |
| `background` | `null` (transparent), a colour, or `"{$id}"` naming a colour input | `null` |
| `fonts` | the family and style of each font file its texts name, `{file: {family, style}}`. The studio keeps it up to date when it saves, so another PC can find the same font under another file name (section 2.5) | `{}` |
| `inputs` | at most 32, in the order a copy's properties show them (section 4) | `[]` |
| `layers` | exactly as a scene's layers ([scenes.md](scenes.md)): bottom first, each with its objects bottom first, ids unique within the widget. Objects are image, video, text, shape and `widget` (a copy of another widget) | `[]` |

- Coordinates in `layers` are container pixels: (0, 0) is the container's top-left.
- A layer's opacity works as in a scene: the layer is a group (section 5.5).
- An image or video may have `"keep_aspect": true` and a `"tint"` (section 5.3).
- **A colour** is `#rrggbb` or `#rgb`, hex digits in either case, with spaces (U+0020) around it ignored. Inside
  a widget, every colour is read that way and anything else is the renderer's fallback: white for text,
  fills, gradient stops and strokes, black for an outline, and none for a background or a tint. The studio
  writes lowercase `#rrggbb`.
- Unknown keys are kept when the studio rewrites the file. It's UTF-8 without a byte-order mark, written
  with `indent=2` like playlist.json.
- At most 2000 objects in all; the studio refuses to save more.

### 2.5 Paths, media and fonts

**Paths in `widget.json` are relative to the widget's folder.** A path is 1 to 4 parts joined by `/`; each part
matches `^[A-Za-z0-9_](?:[A-Za-z0-9_.-]{0,98}[A-Za-z0-9_-])?$` and isn't a reserved Windows name (with or
without an extension). So there's no drive, no leading `/`, no `\`, no `..`, and no part ending in `.` (which
Windows would quietly drop, making `media./a.png` the same file as `media/a.png`). The player leaves out an
object whose path breaks the rule, and logs it. The studio never writes one, and won't save a `src` whose
first part is `widgets`, in any letter case.

**Media.** The studio keeps a widget's images and videos in `media\`, named `<stem>-<hash>.<ext>`:

- `<stem>` is the uploaded file's name, lowercased, each run of other characters as `-`, at most 40
  characters, `media` when empty;
- `<hash>` is the first 8 hex digits of the SHA-1 of the bytes;
- `<ext>` is the lowercased extension.

The same file always gets the same name, so nothing is stored twice, and the names still mean something to
someone browsing the folder. A hand-made widget may keep its art anywhere in its folder, as long as the paths
follow the rule.

**Media types** are the ones the player draws: `.png .apng .gif .webp .jpg .jpeg .bmp` and `.mp4 .m4v .mov .webm
.mkv`. (The player draws BMP from this version on. Before, the studio took BMP files that the cooler couldn't
show.)

- An **Aseprite** file (`.aseprite`, `.ase`) added to a widget, by upload, From selection or paste, is turned
  into a PNG (one frame) or an animated PNG with your Aseprite, and the widget keeps only that. The player
  can't read Aseprite files, and the PC you share it with may not have Aseprite. Without Aseprite the studio
  says so and adds nothing.
- An `.avi` is refused in a widget ("Widgets take MP4, MOV, WebM or MKV video: convert this one first").
  Scenes still take it.
- A video is always read as the container its extension names: a WebM renamed `.mp4` doesn't play (section 7).

**Fonts are file names, not paths:** `"font": "bahnschrift.ttf"`. An empty font means Segoe UI. A name is
looked up in this order, and the first file that exists is used:

1. the widget's `fonts\` folder;
2. `C:\Windows\Fonts`;
3. `%LOCALAPPDATA%\Microsoft\Windows\Fonts` (fonts installed for one user);
4. `CoolerVideo\fonts` (the folder holding `widgets\`, then `fonts`).

If none has it, the text is drawn in Segoe UI, as for any missing font, and the studio's font picker says
"(not on this PC: shown in Segoe UI)". So a widget folder copied to another PC keeps working, and a font you
bundled in `fonts\` wins over one of the same name on that PC.

**A font name** is one file name, the same rule in the player, the studio and import: at most 100 characters
before an extension `.ttf` or `.otf` (either case); none of `\ / : * ? " < > |` or control characters; not
starting or ending with a space or a dot; and a stem (before the first dot) that isn't a reserved Windows name
(`con.ttf`, `NUL.otf`). Spaces inside are fine (`Orbitron Bold.ttf`). Anything else counts as missing.

**Families travel too.** The same font often has another file name on another PC (`Orbitron[wght].ttf`,
`Orbitron-VariableFont_wght.ttf`). `fonts` in `widget.json` (section 2.4) remembers each file's family and
style, so the studio can name the font that's missing ("Orbitron Bold") and offer one installed under another
name (section 6.8).

### 2.6 Git and the examples

`.gitignore` keeps your own widgets out of git, and the examples in:

```
# your own widgets (the examples are shipped)
/widgets/*
!/widgets/example-*/
```

Two examples ship, as `plugins\example-weather` does:

- `widgets\example-temp-badge\`: the widget in section 2.4, with a 16 × 16 pixel-art thermometer at `pixel` 4.
- `widgets\example-claude-usage\`: no inputs, two texts with their own live values (section 6.2).

Each has a README.md saying what its inputs do. **The studio opens them read-only**, with **Duplicate to
edit**: they're tracked in git, so changing them in place would collide with the next `git pull`. Their copies
are placed and filled in like any others. The studio never writes into a folder named `example-…`: no saves,
no uploads, no tidy, no `preview.png`. (A hand edit is still possible, as with the example plugin.)

## 3. A copy: the `widget` object

```json
{ "id": "wq3x9", "type": "widget", "name": "GPU temp", "visible": true, "locked": false, "opacity": 1.0,
  "x": 40, "y": 520, "w": 540, "h": 180, "scale": 1.5,
  "widget": "temp-badge",
  "inputs": { "label": "GPU", "value": "{pc.gpu.temp:d}°C", "accent": "#3ddc84" } }
```

| key | meaning | default |
|---|---|---|
| `widget` | the widget's id | required: without it the copy draws nothing |
| `x`, `y` | where the container's top-left goes, in the holder's pixels. The studio keeps them whole | 0, 0 |
| `scale` | the copy's size as a multiple of the container, the same across and down | see below |
| `inputs` | `{input id: value}`, only the inputs this copy sets (section 4.3) | `{}` |
| `opacity` | 0..1. The copy is drawn **as a whole** at this opacity, like a layer (section 5.5) | 1 |
| `visible` | a hidden copy draws nothing | true |
| `w`, `h` | the copy's size in pixels, written by the studio for older tools and for a missing widget's box. **Renderers ignore them** | |
| `name`, `locked` | for the editor, as on any object. A new copy has no `name`: the editor shows its widget's current name until you give the copy one | |

- **`scale` is what counts.** The copy is `scale × w` by `scale × h` of the container. When the widget's
  container later grows (the author adds a part on the right), every copy grows with it, and its content stays
  the size it was. The studio updates the stored `w` and `h` of every copy in the same step.
- If `scale` is missing or isn't a positive number, it's `w ÷` the container's width when `w` is positive,
  else 1. Then it's clamped to 0.01–16. The studio offers 5%–800% and writes at most 4 decimals.
- **Copies don't turn or flip.** Text and shapes can't ([scenes.md](scenes.md)), so a turned copy couldn't be
  drawn faithfully. `rotate` and `flip` on a copy are ignored, and the studio doesn't write them. Media inside a
  widget still turn.
- **Ids:** the studio gives copies `w…` ids, unique within their holder (a scene, the global layer, or a
  widget).
- A copy is the only way to use a widget's content. You can't move or hide one part in one copy: edit the
  widget, or give it an input.

## 4. Inputs

### 4.1 Declaring an input

```json
{ "id": "label", "name": "Label", "type": "text", "default": "CPU" }
```

| key | meaning |
|---|---|
| `id` | what references use: `^[a-z][a-z0-9_]{0,31}$`, unique within the widget. **It never changes** once made, like a widget's id |
| `name` | what the studio shows: any text, at most 60 characters. Renaming changes only this |
| `type` | `text`, `color` or `image` |
| `default` | **every input has one.** text: up to 2000 characters (longer is cut), written like text in a text object (live placeholders and `{{ }}` work). color: a colour (section 2.4). image: a path in the widget's folder (`media/chip-1a2b3c4d.png`), or `""` for no image |

- **Inputs are optional.** A widget's text, colours and images can all be fixed, live, or mixed, exactly as in
  a scene. A widget with no inputs is simply reusable content.
- A missing or invalid default counts as `""` for text and image, and `#ffffff` for a colour.
- An image input may name any widget media file (section 2.5): a still, an animation or a video. With
  `keep_aspect` it fits the box by the file's own size, a video's included.
- **Characters** are Unicode code points wherever this file counts or cuts them, so nothing is cut in the
  middle of an emoji.

### 4.2 Referring to an input

There is one way to write a reference: **`{$id}`**.

- **In text:** anywhere in a text object's `text`, any number of times: `Temp: {$value}`,
  `{$label}\n{$value}`. Only text inputs can be used in text.
- **In a colour:** the whole value is `"{$id}"`, naming a colour input. These can be bound: text `color` and
  `outline.color`; shape `fill.color` (solid), each gradient stop's `color`, and `stroke.color`; an image's or
  video's `tint` (section 5.3), which is how a copy recolours an icon that's a picture; the widget's
  `background`.
- **In an image or video:** the whole `src` is `"{$id}"`, naming an image input.
- **In a copy inside a widget:** a value can refer to the enclosing widget's inputs (section 4.5).

Nothing else can be bound: not sizes, positions, opacity or visibility.

Why `{$id}`: every live key starts with `a`–`z`, `0`–`9`, `_` or `-` (built-ins, sources and plugin ids), never
`$`, so a reference can't be taken for a live value. And `{{ }}` keep their meaning: `{{$label}}` shows a
literal `{$label}`.

### 4.3 A copy's values

`inputs` on a copy maps input ids to strings. A copy stores only what it sets; the rest take the widget's
defaults. The studio removes a value that equals the default, so a later change to the default reaches that
copy.

- **text:** written like text in a text object, at most 2000 characters (longer is cut). `{pc.gpu.temp:d}°C`
  makes that copy's text live.
- **color:** a colour (section 2.4: `#rrggbb` or `#rgb`, either case). Anything else counts as not set, and
  the default applies.
- **image:** a path relative to the folder of the file the copy is written in, or `""` for no image.
  - In `playlist.json` it's a file in the playlist's **`inputs\`** folder: `"inputs/9c1e2a7b3f10.png"`. It isn't
    in `sources\`, because an older studio would delete it there (section 7).
  - In a widget's `widget.json`, it's a file in that widget's folder, normally `media\`.
  - A path that breaks the rule of section 2.5 counts as not set.
- Values for ids the widget doesn't declare (any more) are ignored and kept. The studio never gives a new
  input an id that copies still hold values for, so an old value can't come back with a new meaning.
- **At the top level** (a copy in a scene or the global layer) there's no widget around the copy, so a colour
  or image value that is `{$id}` counts as not set, and a text value's references show as written.

### 4.4 Substitution

Inputs are filled in **when the file is read, before anything else looks at the text**: before the text is
checked for live placeholders, and before `{{` and `}}` in static text become plain braces. So a copy whose
Value is `{pc.gpu.temp}` makes that text live in that copy only, and a copy that types plain `GPU` into a text
input makes nothing live. The player and the studio follow these rules exactly.

**The text pass** reads text left to right:

1. `{{` and `}}` are copied as they are, so the live parser still sees them as escaped braces.
2. `{$` starts a reference. Its body runs to the next `{`, `}` or line break (U+000A only, as for live
   placeholders). If that next character is `}`:
   - a body that is the id of one of the widget's **text** inputs is replaced by that input's value, made
     safe (below);
   - any other body (an unknown id, a colour or image input, `{$Label}`, `{$label:d}`, `{$}`) becomes
     `{{$body}}`, which shows as the `{$body}` that was written, and never makes the text live. A typo is
     visible on the canvas, and the studio underlines it in the text box.

   Otherwise the `{` is copied.
3. Anything else is copied.

**A value is made safe before it's inserted,** in two steps:

1. The text pass runs over it with no inputs at all, so a `{$…}` inside it becomes `{{$…}}`.
2. It's written out again in a canonical form: read for live placeholders as [data.md](data.md) reads them,
   each placeholder is written as `{` + its body + `}`, and every other brace, `{` or `}`, is written doubled.

The result is never read again for references. So a value can't inject another reference, can't turn into a
live key `$x` that shows `--`, and can't join with the text around it: a value `x{` before `{pc.cpu.temp}` stays
`x{` followed by a live temperature, instead of turning the pair into an escaped `{{`. Its live placeholders and
its `{{ }}` keep their meaning, and it shows exactly what it showed on its own.

**A reference can build a placeholder.** `{pc.{$sensor}.temp:d}°C` with Sensor `gpu` reads
`{pc.gpu.temp:d}°C`, which is live. One widget with a "Sensor" input can then show the CPU or the GPU.

**Colours:** a bindable colour whose whole value is `{$id}` of a colour input takes that input's value. Any
other `{$…}` value isn't a colour, so the fallback of section 2.4 applies: white for text, fills, stops and
strokes, black for an outline, and no background or tint. Every colour of an expanded object is then written
as lowercase `#rrggbb`, so both renderers draw the same colour whatever their own parsers accept.

**Whole values** (a colour, a `src`, a copy's colour or image value) are compared after trimming spaces
(U+0020) from both ends, and nothing else: not tabs, not U+FEFF.

**Images:** a `src` that is `{$id}` of an image input takes that input's file. If the value is `""`, or the id
isn't an image input, the object is left out (nothing is drawn), as it is for an empty `src`.

**In a scene's own text** `{$id}` means nothing special: scenes have no inputs, so it reads as a live key
`$id` that is never found, and shows `--`. The studio doesn't offer inputs outside widgets.

### 4.5 Passing inputs down

A copy inside widget A fills in its own inputs in A's terms, so a widget passes its inputs down:

- A text value goes through the text pass with A's text inputs: `"{$label} temp"` on an inner copy shows the
  outer copy's Label.
- A colour or image value whose whole value is `{$id}` takes A's colour or image input `id`, as this copy of A
  has it. An image chosen in a scene stays that file all the way down.
- Any other image value is a path in A's folder.

So a "Dashboard" widget can have one Accent input, and pass it to the Accent of each "Temp badge" inside it.

### 4.6 Examples

Both renderers must give exactly these results. "Shows" is what the live text then draws. They're also the
test vectors in `player/tests/widgets/inputs.json`.

| text | text inputs | result |
|---|---|---|
| `{$label}: {$value}` | label `CPU`, value `{pc.cpu.temp:d}°C` | `CPU: {pc.cpu.temp:d}°C` (live) |
| `{{$label}}` | label `CPU` | `{{$label}}`: shows `{$label}` |
| `{{{$label}}}` | label `CPU` | `{{CPU}}`: shows `{CPU}` |
| `{$nope}` | none | `{{$nope}}`: shows `{$nope}`, not live |
| `{$Label}`, `{$ label}`, `{$label:d}`, `{$}` | label `CPU` | `{{$Label}}`, `{{$ label}}`, `{{$label:d}}`, `{{$}}`: each shows as written, not live |
| `{$accent}` | (accent is a colour input) | `{{$accent}}`: shows `{$accent}` |
| `{$a}{$a}` | a `{$b}` | `{{$b}}{{$b}}`: shows `{$b}{$b}` |
| `{$v}` | v `100% {{ok}}` | `100% {{ok}}`: shows `100% {ok}` |
| `{pc.{$sensor}.temp:d}°C` | sensor `gpu` | `{pc.gpu.temp:d}°C` (live) |
| `{$a` | a `x` | `{$a`: shows `{$a` |
| `x}{$a}` | a `1` | `x}1` |
| `{$a}{pc.cpu.temp}` | a `x{` | `x{{{pc.cpu.temp}`: shows `x{` and the temperature (live) |
| `{$a}}` | a `5}` | `5}}}`: shows `5}}` |
| `{$v}` | v `{ }` | `{{ }}`: shows `{ }`, not live |
| `{$a` U+000D `}` | a `x` | `{{$a` U+000D `}}`: shows as written (only U+000A ends a body) |

## 5. Drawing

Both renderers follow this section exactly, so the studio's preview matches the cooler: the same
nearest-neighbour rule for images, the same crisp rules, the same whole pixels.

### 5.1 A copy becomes ordinary objects

When a playlist is read, each copy is **expanded** into ordinary image, video, text and shape objects,
called **leaves**, put in the copy's place in its layer's list. Then everything works on plain objects: the
timeline, animation, live text, the player's caches and its watch on files. A scene without copies reads and
draws exactly as before.

A copy's leaves come out in this order: the background (if any), then the widget's layers bottom first, each
layer's objects bottom first. A copy inside a widget is expanded in its place, the same way. Hidden layers and
objects inside a widget aren't expanded.

Each leaf carries:

- its **clip**: the whole-pixel rectangle it may draw in (section 5.4);
- its **groups**: the copies and widget layers below 100% opacity that it sits in, outermost first
  (section 5.5);
- an id `<copy id>/<object id>` (and deeper, `<copy id>/<inner copy id>/<object id>`), for logs.

A copy is skipped (draws nothing) when it's hidden, at opacity 0, its widget is missing or unreadable, it would
make a cycle, it's too deep, or the budget has run out (section 5.10).

### 5.2 Geometry at scale: whole pixels

`rnd(v)` is `floor(v + 0.5)`, in 64-bit floating point, in both renderers: in Rust `(v + 0.5).floor()`, in
JavaScript `Math.floor(v + 0.5)` (not `Math.round`, which differs for a few doubles near halves).

A copy in a holder drawn at scale `s` with origin `(ox, oy)` (whole pixels; `s` = 1 and origin (0, 0) in a
scene or the global layer):

- its scale `S = s × scale`;
- its **box**: left `L = ox + rnd(s × x)`, top `T = oy + rnd(s × y)`, width `Wp = max(1, rnd(S × w))`,
  height `Hp = max(1, rnd(S × h))`, with `w` × `h` the container. The box covers `[L, L+Wp) × [T, T+Hp)`.
- Its contents are drawn at scale `S` with origin `(L, T)`.

Each object inside, once its defaults are filled in, is placed like this:

| kind | changes | keeps |
|---|---|---|
| image, video | edges `x0 = ox + rnd(s × x)`, `y0 = oy + rnd(s × y)`, `x1 = ox + rnd(s × (x + w))`, `y1 = oy + rnd(s × (y + h))`; then `x = x0`, `y = y0`, `w = max(1, x1 − x0)`, `h = max(1, y1 − y0)` | `rotate`, `flip`, `smooth`, `fill`, `opacity`, `keep_aspect`, `tint` |
| rect, ellipse, line | the same four edges, with `w = x1 − x0`, `h = y1 − y0` (a line keeps its signs); `radius × s`; `stroke.width × s` | `shape`, fill (a gradient follows the box), `crisp`, `opacity` |
| text | anchor `x = ox + rnd(s × x)`, `y = oy + rnd(s × y)`; `size × s`; `outline.width × s` | `font`, colours, `align`, `valign`, `line_height`, `crisp`, `opacity` |

(Here `s`, `ox` and `oy` are the copy's: `S`, `L`, `T`.)

- The expressions are evaluated in exactly that order (`x + w` first, then the product, then `rnd`, then the
  sum), never with a fused multiply-add, so the two renderers get the same numbers.
- **Every input number is the JSON's own, as a 64-bit float,** read exactly (correctly rounded) by both
  renderers. Only results are stored in narrower fields. A text's anchor, in particular, comes from the file's
  `x` and `y` as 64-bit numbers, never from a 32-bit copy: `x` 0.7 in a 5× copy is `rnd(3.5)` = 4 (a 32-bit 0.7
  would give 3).
- **Edges, not sizes, are rounded.** Two images that touch in the widget touch in every copy, at any scale:
  the right edge of one and the left edge of the next come from the same `rnd(s × (x + w))`.
- **Every copy at the same scale looks the same, pixel for pixel,** wherever it is: everything inside is
  rounded relative to the copy's whole-pixel origin.
- **Images are resampled once, from the file, at their final size.** The nearest-neighbour rule of
  [scenes.md](scenes.md) then holds exactly in every copy, and pixel art in a 2× copy is as clean as pixel art
  placed at 2× in a scene. At a scale that isn't whole, art pixels come out uneven, as for any in-between size;
  the studio says so.
- **Text and shapes are drawn again at the new size,** never enlarged as pictures, so they stay sharp.
- At scale 1 with whole numbers (what the studio writes), every placed value is the authored one plus the
  copy's origin, so a 1× copy looks exactly like the widget in its editor.
- The text limits (size 2000, outline 500) apply to the scaled values.

### 5.3 Images in a box: `keep_aspect` and `tint`

**`keep_aspect`.** A copy's image rarely has the shape the author drew. With `"keep_aspect": true` on an image
or video, the file is scaled to fit **inside** its placed box, keeping its shape, and centred. It's worked out
when the file is opened for drawing (in the player, while preparing), from the file's own size:

- `bw × bh` is the placed box (section 5.2); `fw × fh` is the file's own size in pixels, after turning
  (`rotate` 90 or 270 swaps them). For a video it's the size ffmpeg reports for its video stream, with width
  and height swapped when the file says it's turned a quarter (a phone video's rotation, which ffmpeg applies
  when it decodes).
- `q = min(bw / fw, bh / fh)`; the drawn size is `vw = max(1, rnd(fw × q))`, `vh = max(1, rnd(fh × q))`;
- it's drawn at `x + floor((bw − vw) / 2)`, `y + floor((bh − vh) / 2)`, at `vw × vh`. **That box replaces the
  placed one** for everything about drawing: the one resample, and the anchor of an `extend`, `mirror` or
  `tile` pattern. The clip stays the copy's.
- A file that can't be opened draws nothing, as anywhere. The studio draws its usual loading box until it
  knows the file's size.

Binding an image to an image input turns it on in the studio. It means the same in a scene (an image pasted
out of a widget keeps it), but the studio offers the tick box only in widgets.

**`tint`.** An image or video may have `"tint"`: a colour, `"{$id}"` of a colour input, or `null` (the default:
drawn as it is). With a tint, **every pixel the file draws takes the tint's colour and keeps its own
transparency**: a white or black icon on a transparent background becomes an icon in that colour. The tint is
applied to the final pixels, after the one resample. A tint that isn't a colour (section 2.4) means no tint.

So one icon widget can be a different colour in each copy: bind the icon's tint to an Accent input. (A
picture with its own colours, like a photo, just becomes a silhouette; for those, use an image input.) The
studio offers **Tint** for any image or video, in scenes too.

### 5.4 The clip and the fill area

- **A copy draws only inside its box.** What you see in the widget editor is the container, so anything
  hanging off it there is cut off in every copy too. To show more, make the container bigger.
- A copy inside a widget draws only inside the intersection of its box and its holder's clip.
- Clip edges are whole pixels, so nothing needs soft clipping.
- `extend`, `mirror` and `tile` repeat an image outward to cover its canvas ([scenes.md](scenes.md)). Inside a
  widget the canvas is the clip, so they fill the container, not the panel. Their pattern stays anchored to
  the object.

### 5.5 Opacity: copies and widget layers are groups

- **A copy is drawn as a whole at its opacity,** like a layer ([scenes.md](scenes.md)): its background and
  layers are drawn together onto a transparent buffer, which is then laid over the layer at the copy's
  opacity. Where a faded copy's parts overlap, they don't show through each other: it looks like the widget,
  faded.
- **A widget layer below 100% is a group** inside its copy, exactly as a scene layer is.
- **An object's own opacity** multiplies within its layer, as in scenes.
- **A copy or widget layer at 100% isn't a separate buffer:** its objects are drawn straight into what holds
  it, each inside its clip. That gives the same picture (blending is associative) and costs nothing extra.

### 5.6 Background

- A widget's `background`, when it's a colour, fills the copy's box. It's drawn first, inside the copy, so the
  copy's opacity applies to it too.
- `null` is transparent. The widget editor shows a checkerboard (or black or white, your choice) behind a
  transparent widget. That's only in the editor: copies and thumbnails don't show it.

### 5.7 Live text

Live text inside copies follows the scene rules ([scenes.md](scenes.md), "Live text"), which this feature makes
exact. The player decides once per scene:

1. Take the scene's layers, then the global layers, in drawing order, with every copy expanded. Find the
   first live text, P.
2. Find S, where P's group starts: P's top-level layer if that layer is below 100%; otherwise the outermost
   copy or widget layer below 100% around P; otherwise P itself.
3. If nothing animated is drawn from S upward (S's own content included), every live text is drawn **in
   place**: exactly where it sits in the stack, inside its groups and clips, as if it were static text.
4. Otherwise every live text is drawn **on top** of everything, in drawing order, at its opacity times the
   opacities of its layer and its groups, clipped to its copy.

So two copies of a gauge in one layer each show their value over their own panel, and a faded copy's live text
fades with it. The studio's preview follows the same rule, so it shows what the cooler shows, and the layers
panel marks such text `live` or `live, on top`.

Live text is redrawn only when its shown text changes. While playing, the frames hold everything below S.
What's from S upward is kept as stills, groups and live texts, put together again when a text changes, then
laid over each frame: one blend per frame.

**Over an animation, values update less often.** A change of a live text means decoding, blending and encoding
every frame of the scene's cycle again: for a 60-second video, 1800 frames. The player keeps that work to about
2% of one processor core: after a redraw that took `c` seconds of work, the next one starts no sooner than
`max(0.5 s, 50 × c)` after the last one started, and shows every change made meanwhile. Over a still scene
that's twice a second at most; over a short GIF a few seconds; over a long video it can be minutes. The studio
says so in the scene's note ("Live values here redraw M frames each time they change, so the cooler updates
them less often"), and warns when a copy's animation stretches a scene's loop, or every scene's through Global,
to the 60 s cap.

### 5.8 Animation and the timeline

A widget has no timeline of its own. Its animated images and videos loop on their own timing from the scene's
time 0, and count towards the scene's cycle (the least common multiple of its loops, capped at 60 s). Every copy
of an animated widget is in step with the others. The widget editor plays them on the studio's clock.

### 5.9 The global layer

Copies work in the global layer exactly as in scenes, inputs and all, drawn above every scene's layers.
Editing a widget used in the global layer prepares every scene again, as any edit to the global layer does.

### 5.10 Limits

| limit | value | past it |
|---|---|---|
| nesting | 8 copies deep (a copy in a scene is 1 deep) | the deeper copy is skipped |
| work | 4000 steps per scene, and 4000 for the global layers (see below) | everything that needs a step is skipped from there on |
| placed media | an image or video placed bigger than 6400 × 2880 | it's left out |
| container | 1–1600 × 1–720 | clamped |
| scale | 0.01–16 (the studio offers 0.05–8) | clamped |
| inputs | 32 per widget | the rest are ignored |
| objects | 2000 per `widget.json` | the studio refuses to save more |
| file | 1 MiB per `widget.json` | the studio refuses to save more, and lists a bigger one as a problem; the player doesn't read it. Its copies draw nothing, on the cooler and in the studio (section 5.11) |
| text | a copy's text value: 2000 characters, counted again once the values passed down to it are filled in, so it stays within 2000 at any depth. A text inside a widget: 4000 characters once its values are filled in, before it's read for live placeholders | the rest is cut, by whole characters (never half an emoji), and what's left is read as usual: a placeholder cut part way shows as written. Texts in a scene's own objects aren't cut |
| ids | 64 characters of a copy's id, and of a layer's or an object's id inside a widget | the rest is ignored: two copies whose ids start with the same 64 characters can be drawn as one group (section 5.5) |

**Counting work.** The budget decides which leaves exist, so both renderers count it exactly alike. Steps are
taken in drawing order (the holder's layers bottom first, each layer's objects bottom first, into each copy as
it's reached):

- **1 step for every copy reached**, at any depth, before anything else is checked: hidden, at opacity 0,
  missing, a cycle or too deep, it still costs its step. A copy is reached when it sits in a layer that is
  visible and above opacity 0 (in a scene, the global layers or a widget).
- **1 step for a background** that's drawn.
- **1 step for every other object inside a widget** that is visible, in a layer that is visible and above
  opacity 0.
- **Nothing else costs anything:** not a hidden object other than a copy, not anything in a hidden layer or a
  layer at opacity 0 (its copies aren't reached), and not a scene's or the global layers' own objects that
  aren't copies.

When a step is needed and none is left, that item and everything after it that needs a step is skipped,
including the rest of every copy being expanded. Leaves already made stay, so a copy can be cut part way. A
scene's own objects that aren't copies are still drawn.

A widget that contains a copy of itself, directly or through others, is a cycle: the inner copy is skipped. The
studio refuses to make one, so this only happens with files edited by hand, or with an import that meets one of
your widgets (the import window says so first, section 6.8). The player logs each of these once per read.

### 5.11 Missing and broken widgets

- **The player:** a copy whose widget folder is missing (or named in other letter case, section 2.3), or whose
  `widget.json` is missing, not JSON, not an object, over 1 MiB, or from a newer studio, draws nothing. The
  player logs it once per read: `widget temp-badge: no widgets\temp-badge\widget.json`. It watches the
  missing file and the `widgets\` folder itself, so putting the folder in place, or renaming it, makes it read
  the playlist again.
- **The studio:** the copy shows as a red dashed box at its stored `x`, `y`, `w`, `h`, labelled with the reason
  ("Missing widget: temp-badge", "Made by a newer studio: temp-badge"), on the editor's overlay only (never in
  thumbnails or exports). It can still be selected, moved, deleted or swapped to another widget.

### 5.12 A worked example

Widget `badge`: container 10 × 10, background `#102030`, no inputs, one layer `l1` at opacity 0.5 holding:

- `r`: rect at 1, 1, 3 × 3, radius 2, opacity 0.8, white fill, black stroke 2;
- `i`: image `media/a.png` at 3, 0, 5 × 5;
- `t`: text `Hi {$x}` at 2, 3, size 10, outline width 1.

Two copies in a scene layer at opacity 1: `c` at 10, 20, scale 1.5, opacity 0.5; and `d` at −3, 0, scale 1.5.

`c`'s box is `[10, 20, 25, 35]` (`rnd(1.5 × 10)` = 15). `d`'s is `[−3, 0, 12, 15]`.

| leaf | x | y | w | h | other | groups | clip |
|---|---|---|---|---|---|---|---|
| `c/background` | 10 | 20 | 15 | 15 | crisp rect, fill `#102030` | `c` 0.5 | 10, 20, 25, 35 |
| `c/r` | 12 | 22 | 4 | 4 | radius 3, stroke 3, opacity 0.8 | `c` 0.5, `c/l1` 0.5 | 10, 20, 25, 35 |
| `c/i` | 15 | 20 | 7 | 8 | `…\widgets\badge\media\a.png` | `c` 0.5, `c/l1` 0.5 | 10, 20, 25, 35 |
| `c/t` | 13 | 25 | | | size 15, outline 1.5, text `Hi {{$x}}` (shows `Hi {$x}`) | `c` 0.5, `c/l1` 0.5 | 10, 20, 25, 35 |
| `d/background` | −3 | 0 | 15 | 15 | | | −3, 0, 12, 15 |
| `d/r` | −1 | 2 | 4 | 4 | | `d/l1` 0.5 | −3, 0, 12, 15 |
| `d/i` | 2 | 0 | 7 | 8 | | `d/l1` 0.5 | −3, 0, 12, 15 |
| `d/t` | 0 | 5 | | | size 15 | `d/l1` 0.5 | −3, 0, 12, 15 |

For `c/i`: `x0 = 10 + rnd(4.5) = 15`, `x1 = 10 + rnd(1.5 × 8) = 22`, `y0 = 20 + rnd(0) = 20`,
`y1 = 20 + rnd(7.5) = 28`. `d`'s parts have the same sizes as `c`'s and sit at the same places in its box.
Each group's buffer covers its clip inside the panel: `c` and `c/l1` cover `[10, 20, 25, 35]`; `d/l1` covers
`[0, 0, 12, 15]`. This example is also a test fixture, `player/tests/widgets/expand/basic`.

The fixture adds three things the table above doesn't exercise, each after what's shown:

- In `badge`'s layer `l1`, last: `n`, a crisp rect at −1, 0, 2 × 2. In `c`: `x0 = 10 + rnd(−1.5) = 9` (half up:
  `floor(−1.0)`; rounding half away from zero would give 8), `x1 = 10 + rnd(1.5) = 12`, so `c/n` is 9, 20,
  3 × 3, clipped at 10. In `d`: `d/n` is −4, 0, 3 × 3.
- A second widget `dot`, 2 × 2, no background, one layer at 1 holding `u`, text `.` at 0.7, 0, size 2.
- In the scene layer, after `d`: `e`, a copy of `dot` at 2.5, 60, scale 1, whose box is `[3, 60, 5, 62]`
  (`rnd(2.5)` = 3) and whose `e/u` is anchored at 4, 60 (`3 + rnd(0.7)`), size 2; and `f`, a copy of `dot` at
  0, 40, scale 5, whose box is `[0, 40, 10, 50]` and whose `f/u` is anchored at 4, 40 (`rnd(5 × 0.7)` =
  `rnd(3.5)` = 4), size 10.

### 5.13 The studio's preview

The studio expands copies with the same rules and the same arithmetic, and draws:

- the same pixels for images (one nearest-neighbour resample to the final box, `keep_aspect`, `tint`), crisp
  shapes (the pixel-centre rule on the placed geometry), clips, fills and the fill area;
- groups through buffers blended at the group's opacity, which can differ from the player's integer blend by
  a level or two where something is translucent, as layer opacity already does; a tinted pixel that's partly
  transparent can differ by as much (the browser stores colours premultiplied);
- text laid out at the scaled size, with the small anti-aliasing differences text already has;
- live text in place or on top by the rule of section 5.7.

## 6. In the studio

### 6.1 The Widgets tab

The bottom strip has two tabs on its left: **Scenes** and **Widgets**. The Widgets tab holds one card per
widget, sorted by name:

- a picture of the widget (letterboxed over a checkerboard), its name, and how many copies use it ("3×");
- buttons ⧉ Duplicate, ⇩ Export and ✕ Delete;
- a widget with a problem (a bad folder name, a broken `widget.json`, one over 1 MiB, one from a newer studio)
  shows a grey card with the reason and **Show folder**;
- an example (`example-…`) is marked "example", and opens read-only.

After the cards: **+ New**, **From selection**, **Import…**, and **Recently deleted** (when there are some).
With more than 8 widgets a filter box appears. Clicking a card opens the widget in the editor. Dragging a card
onto the canvas places a copy.

Folders copied into `widgets\` by hand appear when the studio's window next gets focus.

### 6.2 Making a widget

**+ New** opens a small window:

- **Name** (the folder it will get is shown under it: `widgets\temp-badge`);
- **Size:** presets Label 360 × 120, Wide 600 × 100, Square 240 × 240, Gauge 360 × 360, Whole screen
  1600 × 720, or W × H;
- **Background:** Transparent, or a colour.

**Create** makes the folder and opens the widget.

**From selection** (also **Ctrl+G**, and a button in the object's properties) makes a widget from the selected
objects. The window has the selection's size filled in, and **Replace the selection with a copy** ticked.
**Create** then, as one undo step, removes the objects and puts a copy exactly where they were. Their images
are copied into the widget's `media\`, and their fonts into its `fonts\`, except fonts in `C:\Windows\Fonts`,
which the widget names without a copy. The widget gets one layer per layer that held selected objects, with the
same names and opacities (except the layer that receives the copy, which is set to 100%: its scene layer
already fades it). The page warns when the result may look different: when objects came from more than one
layer and one of them is below 100%, or when something unselected sat between them.

Made **inside a widget**, the new widget declares every input the selected objects use, with the same id,
name, type and default, and the copy that replaces them passes each one down (`{$id}`). So nothing changes on
screen, and the outer widget's copies still fill them in.

Example: a Claude usage widget, no inputs (it ships as `widgets\example-claude-usage\`).

1. **+ New**, "Claude usage", 520 × 140.
2. Two texts with their own live values, written in the widget:
   `Claude {claude.session:d}% · resets in {claude.session_resets_in}` and `this week {claude.week:d}%`.
3. **← Back**, open **Global**, drag it in. It's on every scene, and its copies have nothing to fill in.

### 6.3 The widget editor

- The canvas becomes the container, at a zoom that fits. The panel's guides, camera hole and curve lines are
  gone: they belong to the panel. The border is drawn. Snapping uses the container's edges and centre lines.
- The title reads `Widget: Temp badge · 360 × 120 · used by 3 copies`, with **← Back to Desk** (the place you
  came from).
- Everything else is the scene editor: layers, the tools, images, video, text with live values, shapes, copies
  of other widgets, undo.
- The editor draws the widget exactly as a 1× copy at the container's top-left, with the default inputs, or
  with the inputs of the copy you opened it from ("Previewing with GPU temp in Desk · use defaults"). A copy
  that sits inside another widget previews with the values it gets in a copy of that widget left at its
  defaults ("Previewing with Badge in Dashboard (Dashboard's defaults)").
- The media fit buttons read **Fit inside** and **Fill**, for the container.
- The eyedropper sets the widget's background.
- **Read-only widgets** (an example, or one made by a newer studio) open for viewing: the tools and properties
  are greyed out, and a bar says why, with **Duplicate to edit**.

With nothing selected, the properties show the widget itself:

```
WIDGET
Name         [Temp badge                ]
Folder       widgets\temp-badge          [Show folder] [Change…]
Size         W [360]  H [120]            [Fit to content]
Background   (•) None  ( ) [■]  🔗
Description  [A label, an icon and a live temperature.]
Backdrop     Checks | Black | White        (only in the editor)
INPUTS                        [+ Text] [+ Colour] [+ Image]
  T  Label    {$label}    [CPU               ]   ⋮
  T  Value    {$value}    [{pc.cpu.temp:d}°C ]   ⋮
  ●  Accent   {$accent}   [■]                    ⋮
  ▣  Icon     {$icon}     [▣] [Choose…]          ⋮
Preview with [Defaults ▾]
USED IN      Desk (2) · Global (1) · Dashboard (1)
[Duplicate…] [Export…] [Delete…]
```

- **Size:** changing W or H keeps the content where it is, anchored top-left. Copies keep their scale, so they
  grow or shrink to the right and down.
- **Fit to content** makes the container the content's box and moves the content to (0, 0). Every copy moves
  by the same amount times its scale, so at a whole scale nothing moves on any scene. At an in-between scale a
  part can move by one pixel, because the copy's place and the part's place inside it are then rounded
  separately.
- **Used in:** each scene, Global and widget with copies, with a count. Click one to open it.
- **Change folder name…** asks for the new name (showing the folder it gives, `widgets\gpu-badge`), then, as one
  undo step: makes the new folder as a duplicate, points every copy on this PC at it (in the playlist and in
  your other widgets), and moves the old folder to Recently deleted. It warns that copies inside widgets you've
  already shared elsewhere keep the old id. **Ctrl+Z** brings the old folder back and points the copies at it
  again; the new folder stays, unused, as after Duplicate.

### 6.4 Inputs and bind buttons

- **+ Text, + Colour, + Image** ask for a name ("Label"), then add the input with that name, an id made from it
  (`label`), and the type's default. The id is shown as `{$label}`. The widget's properties show only while
  nothing is selected, so clicking a text input's id copies it, to paste into a text; the `{$}` button by a
  text's box puts it in at the caret.
- **Renaming** an input changes its name only. Its id, and so every reference and every copy's value, stays.
- **⋮:** Move up, Move down, Delete. **Delete** writes the default in wherever the widget uses the input, so
  the widget itself looks the same, then removes it. Copies that set it lose their value: the confirm lists them
  ("2 copies set Label: GPU temp in Desk, Fan in Dashboard. They'll show CPU."). Their values are kept in the
  file but ignored, and no new input takes that id while any copy still holds a value for it.
- The default is edited with the same control a copy uses (section 6.6).

While a widget is open, the property controls get **bind buttons**:

- **Text:** a `{$}` button by the text box. Its menu lists the text inputs (inserting `{$id}` at the caret) and
  **New input from this text…**, which makes a text input whose default is the object's text (so nothing
  changes on screen) and sets the text to `{$id}`. It's offered only when the text holds no `{$…}` reference
  (otherwise it's greyed out: "This text already uses an input"). Typing `{$` in the text box lists the inputs.
- **Colours:** a 🔗 after each colour control (text colour and outline, shape fill, gradient colours, stroke,
  an image's tint, and the background). Its menu lists the colour inputs and **New colour input from this
  colour**. A bound colour shows a chip `◆ Accent ✕`; ✕ unbinds it, writing the input's current colour in.
- **Images:** a row **Picture: [This file ▾]** listing the image inputs and **New image input from this file**.
  Binding ticks **Keep its shape inside the box** (`keep_aspect`).
- **Tint** (any image or video, in scenes too): **None** or a colour, with its 🔗 in a widget.
- Objects that use an input show a small `in` badge in the layers panel.

### 6.5 Placing copies

- **Drag a card** onto the canvas: the copy is centred where you drop it, at scale 1, or smaller if the
  container is bigger than the canvas.
- **The W tool** (key **W**) opens a menu of widgets with a filter box. Pick one, then click the canvas to
  place it there. Shift keeps the tool; Esc cancels.
- **Copy, paste and duplicate** work as for any object. Pasting between a scene and a widget copies the images
  and fonts it needs into the right folder (fonts as From selection does). A copy of a widget this studio
  doesn't have pastes anyway and shows as missing.
- **Parts copied out of a widget keep their look.** Pasted into a scene, Global or another widget, every
  reference to the first widget's inputs is filled in with what its editor was showing: text gets the value
  (made safe), a bound colour or tint gets the colour, a bound picture gets the file (copied along). So nothing
  picks up another widget's inputs by accident, and nothing shows `--` or `{$label}`. A part whose picture
  input was empty drew nothing, and isn't pasted.
- **A widget can't contain itself.** Placing, pasting or swapping a widget into one it's part of (directly or
  through others) is refused: "A widget can't contain itself (Dashboard contains Temp badge)."
- Drag a copy's corner to resize it. It keeps its shape, and snaps to whole scales (×1, ×2, …) near them.

### 6.6 A copy's properties

The object section shows Name, Opacity, X, Y, W and H. Typing W or H sets the scale, keeping the shape. Then:

```
WIDGET COPY
▣ Temp badge ▾                    [Edit widget]
Scale   [−] 150% [+]   [1×]
        in-between size: art pixels come out uneven
INPUTS
Label   [GPU                    ] ↺
Value   [{pc.gpu.temp:d}°C      ] ↺     shows 52°C
Accent  [■]                       ↺
Icon    [▣ thumb] [Choose…] [None] ↺
```

- The widget's name is a menu: pick another widget to **swap** this copy to it. Values for inputs with the
  same ids carry over. The copy's name follows the widget unless you've renamed the copy.
- Each input row shows the value the copy draws with. One the copy doesn't set is shown muted; editing it sets
  it; **↺** goes back to the default.
- Text has autocomplete for live keys, and shows what it reads now ("shows 52°C"). Inside a widget, `{$` lists
  that widget's inputs, to pass them down. When the text doesn't fit, the row says "cut off by the widget's
  edge": the container always clips (section 5.4), so a long value or a growing one (`100°C`, `--`) loses its
  end.
- **A widget with no inputs** shows, in place of the rows: "This widget sets its own text and pictures. Edit
  widget to change them." with **Edit widget**.
- **Choose…** uploads a file: into the playlist's `inputs\` for a copy in a scene or Global, or into the open
  widget's `media\` for a copy inside a widget. A file dropped on the thumbnail does the same.
- **Edit widget**, a double-click on the copy, or Enter, opens the widget, previewing this copy's inputs.

### 6.7 Undo, saving and deleting

- **One undo history covers everything:** scene edits, widget edits, and edits that change both (changing a
  container's size, making a widget from a selection). Undo and redo each open the scene or widget where that
  step was made. Files a step uses (images, input pictures) are kept while the page could still undo back to
  them, however long ago that was.
- Edits save as you make them. Each widget is its own file, so editing a widget doesn't rewrite the playlist.
  The player picks the change up and prepares again only the scenes that use the widget. If a program is
  reading the file just as it's saved (the player, say), the studio tries again for a moment.
- **Delete…** on a widget asks first, listing where it's used: "Temp badge is used 3 times: Desk (2),
  Dashboard (1)." **Delete widget and copies** removes the copies too; **Keep the copies** leaves them, shown as
  missing until the widget comes back. Either way its folder moves to `widgets\.deleted\`, and **Ctrl+Z**
  brings back the widget, folder and all, and any copies the delete removed.
- **New, Duplicate and Import aren't undone** by Ctrl+Z: a widget made by them stays in the library until you
  delete it. Undoing **From selection** brings back the objects and leaves the new widget, unused.
- **Recently deleted** in the Widgets tab lists deleted widgets, with **Restore**. Nothing is ever deleted from
  `widgets\.deleted\` by the studio.
- Deleting, restoring, replacing and changing a folder name move folders. If Windows won't let the studio move
  one (a program, an Explorer preview or the player preparing a video, has a file in it open), it tries for
  about 2 seconds, then says "Temp badge is in use (a program has its files open): try again", and nothing has
  changed.
- If a widget's file changes outside this page (another tab, a hand edit), the page takes in the new version
  when its window gets focus. If you had unsaved changes to that widget, it asks: **Keep mine** or **Use the
  file's version**.

### 6.8 Sharing: export and import

- **Export…** saves `temp-badge.zip`: the widget's folder and every widget it uses. The window lists the fonts
  it uses that aren't Windows' own, each ticked to include ("only if its licence lets you share it"). Included
  fonts are also copied into the widget's `fonts\`, so the folder carries them from then on. A picture of the
  widget goes in as `preview.png` (into the zip only, for an example).
- **Import…** (or drop a `.zip` anywhere on the page) shows, for each widget in it: the picture, name, size,
  inputs and description; whether you have it already ("the same" is skipped; a different one offers **Keep
  both** or **Replace**); any widget it needs that isn't included; whether it would make a cycle with one of
  yours (its inner copy then won't draw); and any font you don't have, by family ("its text shows in Segoe UI
  until you install Orbitron Bold"). **Import** puts them in the library. **Replace** moves your old one to
  Recently deleted, and every copy of it uses the new one.
- In a zip, each top-level folder holding a `widget.json` is a widget. A `widget.json` at the top of the zip,
  not in a folder, is one widget whose id comes from the zip's file name (`temp-badge.zip` gives
  `temp-badge`).
- **A font under another name.** When you have the missing font's family and style under another file name,
  the import window and the font picker offer **Use the installed Orbitron Bold (Orbitron[wght].ttf)**, which
  points the widget's texts at that file in one undoable step.
- **Without the studio:** copy the widget's folder into `widgets\`. The folder name is the id.
- **Widgets are data:** unlike plugins, they can't run code. Their files are still checked before anything
  uses them: every name, type and size; that each image or video really is what its name says (a video is only
  ever read as its own container type); and that each font is a well-formed font file. A zip may not hold
  Aseprite files or `.avi` videos.

## 7. Compatibility

- **playlist.json stays `"version": 2`**, and nothing is added at its top level: the library is the folder. (An
  older studio treats any other version as version 1, and would drop every scene.)
- **An older player** skips objects of an unknown type: it plays every scene without its copies, and never reads
  `widgets\`. It also ignores `tint` and `keep_aspect`. **This studio says when your player is older:** the one
  running ("The player running is older than this studio: widgets won't show on the cooler until it's rebuilt
  and restarted"), or the one built for Play and Export PNG ("The player was built before widgets: rebuild it
  (`cargo build --release` in `player\`) to see them on the cooler and in Export PNG").
- **An older studio** draws nothing for a copy, keeps it when saving, and can still move or delete it by its
  stored box. It never looks in `widgets\` or `playlist\inputs\`, so its tidy can't delete anything a copy
  uses. Resizing a copy there changes only its stored `w` and `h`, which this studio and player ignore: the
  copy keeps its scale.
- **This player** draws existing scenes as before, except:
  - live text is now placed exactly (section 5.7), and **live text in a hidden layer no longer shows on the
    cooler**, matching the studio (before, it did);
  - an object at a negative half pixel moves (positions now round half up, as the studio's preview does);
  - `.bmp` images now draw (the studio accepted them before; the cooler showed nothing);
  - a video whose file says it's turned (a phone video) is read at its turned size (before, its frames were
    read at the unturned size);
  - a video whose extension doesn't match its contents (a WebM named `.mp4`) no longer plays: rename it;
  - over an animation, live values update at most as often as section 5.7 allows.

  It prepares every scene again once after the upgrade.
- **A newer `widget.json`** (`version` above 1) is skipped by this player and shown read-only by this studio,
  which never saves over it.
