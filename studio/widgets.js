"use strict";
// Reusable widgets (docs/widgets.md): the page's model, `WV`. Pure functions, no DOM, shared by the drawing in
// studio.js and the panels in library.js and copies.js. Loaded first, before studio.js.

const WV = {
  // studio.js offers these events to each function in turn before handling them itself (offerHooks), while widgets
  // are on: key(e, typing), drop(e), dblclick(e) return true when they've handled the event; props(p) are all called
  // at the end of showProps, with the primary object or null. library.js and copies.js add their own.
  hooks: { drop: [], key: [], dblclick: [], props: [] },
  fromSelection() {},  // From selection: library.js fills it in; Ctrl+G and #pObject's button call it
  loaded: {},          // each of the three scripts sets its name here as its last line
};
window.WV = WV;

WV.loaded.widgets = true;
