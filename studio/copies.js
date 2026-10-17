"use strict";
// Reusable widgets (docs/widgets.md): copies. Placing them (the W tool, dropping a card, pasting), a copy's
// properties (#pCopy) and its input rows, the bind buttons, and From selection's last step. Loaded after studio.js,
// whose functions it uses. Nothing here runs while widgets are off (widgetsOn in studio.js).

/** Fills #pCopy for the selected copy `o`. Not built yet: does nothing. */
function showCopyProps(o) {}

/** One input's row, for a copy's value or a widget's default: `value` is what's drawn with, `isDefault` whether
 *  that's the widget's default, and `onChange` takes a new value. Returns the row's element. Not built yet: an empty
 *  element. */
function inputRow(input, value, isDefault, onChange) {
  return make("div");
}

/** Puts a new copy of widget `widgetId` into layer `layerId` of the open holder at `index`, at (`x`, `y`) and
 *  `scale`, and returns it. Called inside an `edit`. Not built yet: adds nothing, and returns null. */
function newCopy(widgetId, layerId, index, x, y, scale) {
  return null;
}

/** A bind button for a property of the selected object: `kind` is "text", "color" or "image"; `get` and `set` read
 *  and write the bound property. Returns its element. Not built yet: an empty element. */
function bindButton(kind, get, set) {
  return make("span");
}

WV.loaded.copies = true;
