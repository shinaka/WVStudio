# example-temp-badge: a widget with inputs

A label, a pixel-art thermometer and a live temperature on a dark panel, 360 x 120. It's the widget
`docs/widgets.md` walks through. Drag it from the studio's **Widgets** tab onto a scene, or into **Global**,
and fill in its inputs on each copy.

## Inputs

| input | type | default | what it does |
|---|---|---|---|
| Label | text | `CPU` | the small grey text at the top |
| Value | text | `{pc.cpu.temp:d}°C` | the big text under it. Live placeholders work, so a copy can show `{pc.gpu.temp:d}°C` instead |
| Accent | colour | `#ff7a1a` | the colour of the Value text and of the panel's border |
| Icon | image | `media/thermometer-6d3b9a52.png` | the picture on the left, 16 x 16 art drawn at 4 pixels per art pixel. It keeps its shape inside its box, so any picture fits |

A copy stores only the inputs it sets. Leave one at its default and a later change to the widget reaches that
copy.

`{pc.cpu.temp}` needs HWiNFO or Libre Hardware Monitor (see `docs/data.md`); `{pc.gpu.temp}` needs an NVIDIA
card. Until the value arrives, the text shows `--°C`.

## Recolouring the icon per copy

The thermometer is drawn in its own colours (a red column in a grey tube), and it stays that way in every
copy: its **Tint** is **None**. To make an icon take each copy's Accent instead, bind the icon's tint to the
Accent input in the widget editor (the 🔗 next to **Tint** on the image). Then every pixel the picture draws
takes the Accent colour and keeps its own transparency, so a white or black icon on a transparent background
becomes an icon in that colour.

This example doesn't, because Icon is an input: a picture chosen for it in a copy would turn into a
one-colour silhouette, and the thermometer would lose its red column. If you want both, make two widgets,
or give the icon no input and bind its tint.

## Editing it

The examples open read-only in the studio, because they're tracked in git. **Duplicate to edit** makes your
own copy of the folder, under a new id, which you can change freely. The thermometer was drawn with a small
Pillow script; `widget.json` is written as the studio writes one.
