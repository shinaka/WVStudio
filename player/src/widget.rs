//! Widgets (docs/widgets.md): reusable pieces, each in its own folder `widgets\<id>\` beside the
//! playlist's folder, and the copies that place them in scenes, the global layers and other widgets.
//!
//! A copy is expanded as the playlist is read: its widget's objects become ordinary objects in the copy's
//! place, each with the clip and the groups it draws in, so everything after the read works on plain
//! objects. Nothing is read yet: a copy draws nothing, as an object of an unknown type didn't.

// stubs until the reader is written
#![allow(dead_code, unused_variables)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::Value;

use crate::playlist::{Group, Object, Rect, Stamp};
use crate::text::DEFAULT_FONT;

/// Copies nested deeper than this are skipped; a copy in a scene is 1 deep.
pub const MAX_DEPTH: usize = 8;
/// Steps of work for each scene's copies, and for the global layers' (docs/widgets.md, Limits).
pub const BUDGET: usize = 4000;
/// Inputs a widget may declare; the rest are ignored.
pub const MAX_INPUTS: usize = 32;
/// An image or video placed bigger than this is left out.
pub const MAX_PLACED: (i64, i64) = (6400, 2880);

/// The widgets folder for a playlist whose folder is `base`, beside it as plugins\ is. `base` is made
/// absolute first (std::path::absolute), so `wvplay play playlist.json` run inside playlist\ finds ..\widgets.
pub fn widgets_for(base: &Path) -> PathBuf {
    let base = if base.as_os_str().is_empty() { Path::new(".") } else { base };
    let base = std::path::absolute(base).unwrap_or_else(|_| base.to_path_buf());
    base.parent().unwrap_or(base.as_path()).join("widgets")
}

/// floor(v + 0.5): the one rounding rule for everything a copy places (docs/widgets.md).
pub fn rnd(v: f64) -> i64 {
    (v + 0.5).floor() as i64
}

/// The text pass (docs/widgets.md, Substitution): `{$id}` of a text input replaced by its value, made safe.
pub fn substitute(text: &str, input: &dyn Fn(&str) -> Option<String>) -> String {
    text.to_string()
}

/// A value made safe: the text pass with no inputs, then written out in the canonical form.
pub fn make_safe(value: &str) -> String {
    value.to_string()
}

/// A colour (docs/widgets.md, widget.json) as lowercase `#rrggbb`, or None. Never panics on any string.
pub fn color(value: &str) -> Option<String> {
    None
}

/// Whether a path in `widget.json` stays inside the widget's folder (docs/widgets.md, Paths).
pub fn path_ok(path: &str) -> bool {
    false
}

/// Whether a font name is one file name the renderers look up (docs/widgets.md, A font name).
pub fn font_name_ok(name: &str) -> bool {
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Text,
    Color,
    Image,
}

/// A value a widget declares, which each copy may set (docs/widgets.md, Inputs).
#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    pub id: String,
    pub kind: InputKind,
    pub default: String,
}

/// A widget as read. Its layers stay JSON: they're expanded once per copy, with that copy's values.
#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub id: String,
    pub folder: PathBuf,
    pub w: u32,
    pub h: u32,
    pub background: Option<String>,
    pub inputs: Vec<Input>,
    pub layers: Vec<Value>,
}

/// The input values one copy draws with: text, `#rrggbb`, and absolute image paths (empty: no image).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scope {
    pub text: HashMap<String, String>,
    pub color: HashMap<String, String>,
    pub image: HashMap<String, PathBuf>,
}

/// The widgets one read of the playlist uses, each read once, and every file whose change matters, with the
/// stamp it had just before it was read.
pub struct Library {
    folder: PathBuf,
    names: Option<HashSet<String>>,
    defs: HashMap<String, Result<Rc<Def>, String>>,
    pub watch: Vec<(PathBuf, Option<Stamp>)>,
    logged: HashSet<String>,
}

impl Library {
    /// The widgets in `folder`, which isn't looked at until a copy asks for one.
    pub fn new(folder: PathBuf) -> Library {
        Library { folder, names: None, defs: HashMap::new(), watch: Vec::new(), logged: HashSet::new() }
    }

    /// The widget `id`, read the first time it's asked for, or why it can't be drawn.
    pub fn get(&mut self, id: &str) -> Result<Rc<Def>, String> {
        Err(format!("widget {id}: widgets aren't read yet"))
    }

    /// The font file a text in `def` names (docs/widgets.md, Fonts).
    pub fn font(&mut self, def: &Def, name: &str) -> PathBuf {
        PathBuf::from(DEFAULT_FONT)
    }
}

/// Where a holder's objects are placed: docs/widgets.md, Geometry. The top frame is the playlist's.
#[derive(Clone, Debug)]
pub struct Frame {
    pub folder: PathBuf,
    pub scope: Rc<Scope>,
    pub s: f64,
    pub ox: i64,
    pub oy: i64,
    pub clip: Option<Rect>,
    pub groups: Vec<Group>,
    pub stack: Vec<String>,
    pub depth: usize,
    pub path: String,
}

impl Frame {
    /// A scene's or the global layers' own: the playlist's folder, no inputs, at scale 1 from the panel's
    /// top-left, with no clip.
    pub fn top(folder: &Path) -> Frame {
        Frame { folder: folder.to_path_buf(), scope: Rc::default(), s: 1.0, ox: 0, oy: 0, clip: None, groups: Vec::new(),
                stack: Vec::new(), depth: 0, path: String::new() }
    }
}

/// Expands the copies of one stack of layers (a scene's, or the global ones) within one budget.
pub struct Expander<'a> {
    lib: &'a mut Library,
    ffmpeg: &'a Path,
    budget: usize,
    /// Names the scene or the global layers in log lines.
    holder: String,
}

impl<'a> Expander<'a> {
    pub fn new(lib: &'a mut Library, ffmpeg: &'a Path, holder: &str) -> Expander<'a> {
        Expander { lib, ffmpeg, budget: BUDGET, holder: holder.to_string() }
    }
}

impl Expander<'_> {
    /// Adds the objects the copy `value` (an object with "type": "widget") draws, placed in `frame`, to
    /// `out`. Not yet: the copy is dropped.
    pub fn copy(&mut self, value: Value, frame: &Frame, out: &mut Vec<Object>) {}
}
