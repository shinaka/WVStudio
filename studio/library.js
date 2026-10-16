"use strict";
// Reusable widgets (docs/widgets.md): the Widgets tab in the bottom strip; the New, Delete, Export, Import and
// conflict windows; and the open widget's own properties (#pWidget) with its inputs. Loaded after studio.js, whose
// functions it uses. Nothing here runs while widgets are off (widgetsOn in studio.js).

/** Makes a widget from the selected objects and puts a copy of it where they were (docs/widgets.md, Making a
 *  widget). Ctrl+G and the From selection buttons call it. Not built yet: does nothing. */
WV.fromSelection = function fromSelection() {};

/** Fills #pWidget: the open widget's own properties, shown when nothing is selected. Not built yet: does nothing. */
function showWidgetProps(id) {}

WV.loaded.library = true;
