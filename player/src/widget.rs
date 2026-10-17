//! Widgets (docs/widgets.md): reusable pieces, each in its own folder `widgets\<id>\` beside the
//! playlist's folder, and the copies that place them in scenes, the global layers and other widgets.
//!
//! A copy is expanded as the playlist is read: its widget's objects become ordinary objects in the copy's
//! place, each with the clip and the groups it draws in, so everything after the read works on plain
//! objects. Reading opens no media and runs no ffmpeg: `keep_aspect` and `tint` stay on the leaves, for
//! when their files are opened.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::Value;

use crate::art::{HEIGHT, WIDTH};
use crate::playlist::{self, Group, Kind, Object, Paint, Rect, Shape, ShapeKind, Stamp};
use crate::text::DEFAULT_FONT;

/// Copies nested deeper than this are skipped; a copy in a scene is 1 deep.
pub const MAX_DEPTH: usize = 8;
/// Steps of work for each scene's copies, and for the global layers' (docs/widgets.md, Limits).
pub const BUDGET: usize = 4000;
/// Inputs a widget may declare; the rest are ignored.
pub const MAX_INPUTS: usize = 32;
/// An image or video placed bigger than this is left out.
pub const MAX_PLACED: (i64, i64) = (6400, 2880);

/// Characters (code points) kept of a text input's value or default, before and after its own text pass.
const MAX_TEXT: usize = 2000;
/// Characters kept of a text inside a copy, once its values are in.
const MAX_LEAF_TEXT: usize = 4000;
/// Characters kept of a copy's, a widget layer's or a widget object's id.
const MAX_ID: usize = 64;
/// Bytes of a `widget.json` read: a bigger one draws nothing, as the studio won't save one.
const MAX_JSON: u64 = 1 << 20;
/// Windows' reserved names, which no id, path part or font stem may be.
const RESERVED: [&str; 22] = ["con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
                              "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9"];
const PANEL: Rect = [0, 0, WIDTH as i64, HEIGHT as i64];

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

/// One piece of text as the text pass reads it: copied as it is, or a reference's body.
enum Token<'a> {
    Copy(&'a str),
    Ref(&'a str),
}

/// The text pass's reading (docs/widgets.md, Substitution): `{{` and `}}` are copied whole, `{$` starts a
/// reference whose body runs to the next `{`, `}` or line break and counts only when that's a `}`. It stops
/// early when `token` says so.
fn tokens<'a>(text: &'a str, mut token: impl FnMut(Token<'a>) -> bool) {
    let mut rest = text;
    while !rest.is_empty() {
        let at = rest.find(['{', '}']).unwrap_or(rest.len());
        let (next, after) = if at > 0 {
            (Token::Copy(&rest[..at]), &rest[at..])
        } else if rest.starts_with("{{") || rest.starts_with("}}") {
            (Token::Copy(&rest[..2]), &rest[2..])
        } else if let Some(after) = rest.strip_prefix("{$")
            && let Some(end) = after.find(['{', '}', '\n'])
            && after[end..].starts_with('}')
        {
            (Token::Ref(&after[..end]), &after[end + 1..])
        } else {
            (Token::Copy(&rest[..1]), &rest[1..])
        };
        if !token(next) {
            return;
        }
        rest = after;
    }
}

/// Text written out up to a number of characters (code points), never stopping inside one. The rest is
/// dropped.
struct Capped {
    text: String,
    room: usize,
}

impl Capped {
    /// Adds `s`, or as much of it as there's room for: false once it's full.
    fn push(&mut self, s: &str) -> bool {
        let mut n = 0;
        for (at, _) in s.char_indices() {
            if n == self.room {
                self.text.push_str(&s[..at]);
                self.room = 0;
                return false;
            }
            n += 1;
        }
        self.text.push_str(s);
        self.room -= n;
        true
    }
}

/// The text pass (docs/widgets.md, Substitution): `{$id}` of a text input replaced by its value, made safe.
pub fn substitute(text: &str, input: &dyn Fn(&str) -> Option<String>) -> String {
    substitute_cut(text, input, usize::MAX)
}

/// `substitute`, cut to its first `max` characters (code points). It stops once it has them, so a text
/// that would grow huge is never built whole.
fn substitute_cut(text: &str, input: &dyn Fn(&str) -> Option<String>, max: usize) -> String {
    let mut out = Capped { text: String::with_capacity(text.len().min(max)), room: max };
    tokens(text, |token| match token {
        Token::Copy(s) => out.push(s),
        Token::Ref(body) => match input(body) {
            Some(value) => out.push(&make_safe(&value)),
            // shown as written, and never live
            None => out.push("{{$") && out.push(body) && out.push("}}"),
        },
    });
    out.text
}

/// A value made safe: the text pass with no inputs, then written out in the canonical form.
pub fn make_safe(value: &str) -> String {
    let unbound = substitute(value, &|_| None);
    let mut out = String::with_capacity(unbound.len() + 4);
    scan(&unbound, |part| match part {
        Part::Text(s) => {
            for c in s.chars() {
                if matches!(c, '{' | '}') {
                    out.push(c);
                }
                out.push(c);
            }
        }
        Part::Hole(body) => {
            out.push('{');
            out.push_str(body);
            out.push('}');
        }
    });
    out
}

/// A piece of text as the live parser reads it: as written (each `{{` or `}}` as its one brace), or a
/// placeholder's whole body.
enum Part<'a> {
    Text(&'a str),
    Hole(&'a str),
}

/// data.rs's `scan`, giving each placeholder's body as written. It must split text exactly as that does
/// (the test `scan_matches_the_live_parser` checks it).
fn scan<'a>(text: &'a str, mut part: impl FnMut(Part<'a>)) {
    let mut rest = text;
    while let Some(at) = rest.find(['{', '}']) {
        part(Part::Text(&rest[..at]));
        let tail = &rest[at..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            part(Part::Text(&tail[..1]));
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('{') {
            let end = tail[1..].find(['{', '}', '\n']).filter(|&e| tail[1 + e..].starts_with('}'));
            if let Some(end) = end {
                let body = &tail[1..1 + end];
                let spec = body.split_once('|').map_or(body, |(spec, _)| spec);
                let key = spec.split_once(':').map_or(spec, |(key, _)| key);
                if !key.trim().is_empty() {
                    part(Part::Hole(body));
                    rest = &tail[end + 2..];
                    continue;
                }
            }
        }
        part(Part::Text(&tail[..1]));
        rest = &tail[1..];
    }
    part(Part::Text(rest));
}

/// A colour (docs/widgets.md, widget.json) as lowercase `#rrggbb`, or None. Never panics on any string.
pub fn color(value: &str) -> Option<String> {
    let hex = value.trim_matches(' ').strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    match hex.len() {
        6 => Some(format!("#{hex}")),
        3 => Some(hex.chars().fold(String::from("#"), |mut out, c| {
            out.push(c);
            out.push(c);
            out
        })),
        _ => None,
    }
}

/// The input a whole value `{$id}` names (spaces around it ignored), or None for any other value.
fn reference(value: &str) -> Option<&str> {
    value.trim_matches(' ').strip_prefix("{$")?.strip_suffix('}')
}

/// Whether a name's stem (before its first dot) is one of Windows' reserved names, in any case.
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default();
    RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// Whether `id` names a widget: docs/widgets.md, Ids and folder names.
fn id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    let end = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= 40
        && end(&b[0])
        && end(&b[b.len() - 1])
        && b.iter().all(|c| end(c) || *c == b'-')
        && !RESERVED.contains(&id)
}

/// Whether `id` can be an input's id: `^[a-z][a-z0-9_]{0,31}$`.
fn input_id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Whether a path in `widget.json` stays inside the widget's folder (docs/widgets.md, Paths).
pub fn path_ok(path: &str) -> bool {
    let part_ok = |part: &str| {
        let inner = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-';
        match part.as_bytes() {
            [] => false,
            [one] => one.is_ascii_alphanumeric() || *one == b'_',
            [first, middle @ .., last] => {
                (first.is_ascii_alphanumeric() || *first == b'_')
                    && middle.len() <= 98
                    && middle.iter().all(|c| inner(c) || *c == b'.')
                    && inner(last)
            }
        }
    };
    let parts = path.split('/').count();
    (1..=4).contains(&parts) && path.split('/').all(|part| part_ok(part) && !reserved(part))
}

/// Whether a font name is one file name the renderers look up (docs/widgets.md, A font name).
pub fn font_name_ok(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".ttf") && !lower.ends_with(".otf") {
        return false;
    }
    let before = name[..name.len() - 4].chars().count();
    (1..=100).contains(&before)
        && !name.chars().any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c < ' ')
        && !name.starts_with([' ', '.'])
        && !reserved(name)
}

/// The first `n` characters (code points) of `s`, so nothing is cut in the middle of one.
fn cut(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((at, _)) => s[..at].to_string(),
        None => s.to_string(),
    }
}

/// Where two rects overlap (it may be empty).
fn meet(a: Rect, b: Rect) -> Rect {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])]
}

fn empty(r: Rect) -> bool {
    r[2] <= r[0] || r[3] <= r[1]
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
    /// As written; `""` when it's missing or isn't a string.
    pub default: String,
}

/// A widget as read. Its objects stay JSON: they're expanded once per copy, with that copy's values.
#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub id: String,
    pub folder: PathBuf,
    pub w: u32,
    pub h: u32,
    /// As written: a colour, or `{$id}` of a colour input.
    pub background: Option<String>,
    pub inputs: Vec<Input>,
    pub layers: Vec<DefLayer>,
}

/// One of a widget's layers, as every copy walks it: visible, above opacity 0, and holding only what costs
/// a step (docs/widgets.md, Counting work). Whatever else the file holds draws nothing and costs nothing,
/// so it's left out once, when the widget is read, rather than passed over again in every copy.
#[derive(Clone, Debug, PartialEq)]
pub struct DefLayer {
    /// As written (its first 64 characters), or `l<index>` (its place among the file's layers) when it has
    /// none.
    pub id: String,
    /// Above 0, at most 1.
    pub opacity: f64,
    /// Its copies, hidden or not, and its other objects that aren't hidden, bottom first.
    pub objects: Vec<Value>,
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
    /// The folder's entries, exactly as stored: listed once, the first time a widget is asked for.
    names: Option<HashSet<String>>,
    defs: HashMap<String, Result<Rc<Def>, String>>,
    /// Each (widget, font name)'s file, looked up once.
    fonts: HashMap<(String, String), PathBuf>,
    pub watch: Vec<(PathBuf, Option<Stamp>)>,
    /// What's been logged, so each line shows once per read.
    logged: HashSet<String>,
}

impl Library {
    /// The widgets in `folder`, which isn't looked at until a copy asks for one.
    pub fn new(folder: PathBuf) -> Library {
        Library { folder, names: None, defs: HashMap::new(), fonts: HashMap::new(), watch: Vec::new(), logged: HashSet::new() }
    }

    /// Logs `line`, unless this read already has.
    fn log(&mut self, line: String) {
        if !self.logged.contains(&line) {
            crate::player::log(&line);
            self.logged.insert(line);
        }
    }

    /// The widget `id`, read the first time it's asked for, or why it can't be drawn.
    pub fn get(&mut self, id: &str) -> Result<Rc<Def>, String> {
        if !id_ok(id) {
            return Err(format!("widget {id:?}: that isn't a widget id, so its copies draw nothing"));
        }
        if let Some(def) = self.defs.get(id) {
            return def.clone();
        }
        if self.names.is_none() {
            // stamped first, so a folder made, removed or renamed from now on is noticed
            self.watch.push((self.folder.clone(), playlist::stamp(&self.folder)));
            let names = std::fs::read_dir(&self.folder)
                .map(|entries| entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
                .unwrap_or_default();
            self.names = Some(names);
        }
        // watched whether or not it's there, and stamped before it's read, so a save landing meanwhile counts
        let file = self.folder.join(id).join("widget.json");
        self.watch.push((file.clone(), playlist::stamp(&file)));
        // only a folder named exactly the id, whatever Windows would open
        let def = if self.names.as_ref().is_some_and(|names| names.contains(id)) {
            self.read(id, &file).map(Rc::new)
        } else {
            Err(format!("widget {id}: no widgets\\{id}\\widget.json"))
        };
        self.defs.insert(id.to_string(), def.clone());
        def
    }

    /// `widget.json`, read leniently: what makes no sense is left at its default, and logged.
    fn read(&mut self, id: &str, file: &Path) -> Result<Def, String> {
        // never more than one byte past the cap, whatever the file's size
        let mut bytes = Vec::new();
        std::fs::File::open(file).and_then(|f| f.take(MAX_JSON + 1).read_to_end(&mut bytes)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("widget {id}: no widgets\\{id}\\widget.json"),
            _ => format!("widget {id}: couldn't read widget.json ({e})"),
        })?;
        if bytes.len() as u64 > MAX_JSON {
            return Err(format!("widget {id}: widget.json is too big (more than 1 MiB)"));
        }
        let json: Value = serde_json::from_slice(&bytes).map_err(|e| format!("widget {id}: widget.json isn't valid JSON ({e})"))?;
        let Value::Object(keys) = json else {
            return Err(format!("widget {id}: widget.json isn't a widget (it isn't a JSON object)"));
        };
        if let Some(version) = keys.get("version").filter(|v| v.as_f64().is_some_and(|v| v > 1.0)) {
            return Err(format!("widget {id}: widget.json was made by a newer studio (version {version})"));
        }
        let size = |key: &str, max: i64, default: u32| keys.get(key).and_then(Value::as_f64).map_or(default, |v| rnd(v).clamp(1, max) as u32);
        let mut inputs: Vec<Input> = Vec::new();
        for (i, entry) in keys.get("inputs").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let n = i + 1;
            if inputs.len() == MAX_INPUTS {
                self.log(format!("widget {id}: ignoring input {n} and those after it: a widget has at most {MAX_INPUTS} inputs"));
                break;
            }
            let input_id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
            let kind = match entry.get("type").and_then(Value::as_str) {
                Some("text") => Some(InputKind::Text),
                Some("color") => Some(InputKind::Color),
                Some("image") => Some(InputKind::Image),
                _ => None,
            };
            let problem = if !entry.is_object() {
                Some("it isn't an object".to_string())
            } else if !input_id_ok(input_id) {
                Some(format!("{} isn't an input id", entry.get("id").unwrap_or(&Value::Null)))
            } else if inputs.iter().any(|input| input.id == input_id) {
                Some(format!("{input_id:?} is used twice"))
            } else if kind.is_none() {
                Some(format!("its type {} isn't text, color or image", entry.get("type").unwrap_or(&Value::Null)))
            } else {
                None
            };
            match (problem, kind) {
                (None, Some(kind)) => {
                    let default = entry.get("default").and_then(Value::as_str).unwrap_or_default().to_string();
                    inputs.push(Input { id: input_id.to_string(), kind, default });
                }
                (problem, _) => self.log(format!("widget {id}: ignoring input {n}: {}", problem.unwrap_or_default())),
            }
        }
        Ok(Def {
            id: id.to_string(),
            folder: self.folder.join(id),
            w: size("w", WIDTH as i64, 360),
            h: size("h", HEIGHT as i64, 120),
            background: keys.get("background").and_then(Value::as_str).map(str::to_string),
            inputs,
            layers: layers(keys.get("layers")),
        })
    }

    /// The font file a text in `def` names (docs/widgets.md, Fonts): the first that exists of the widget's
    /// own `fonts\`, Windows' fonts, the user's fonts and the project's `fonts\`. Every path tried is watched,
    /// so a font put in place later is noticed. None found: the first, which the font loader can't open, so
    /// it falls back to Segoe UI as for any missing font.
    pub fn font(&mut self, def: &Def, name: &str) -> PathBuf {
        if name.is_empty() || !font_name_ok(name) {
            return PathBuf::from(DEFAULT_FONT);
        }
        let key = (def.id.clone(), name.to_string());
        if let Some(found) = self.fonts.get(&key) {
            return found.clone();
        }
        let mut tries = vec![def.folder.join("fonts").join(name), Path::new(r"C:\Windows\Fonts").join(name)];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            tries.push(Path::new(&local).join(r"Microsoft\Windows\Fonts").join(name));
        }
        if let Some(root) = self.folder.parent() {
            tries.push(root.join("fonts").join(name));
        }
        let mut found = None;
        for path in &tries {
            let stamp = playlist::stamp(path);
            self.watch.push((path.clone(), stamp));
            if stamp.is_some() {
                found = Some(path.clone());
                break;
            }
        }
        let found = found.unwrap_or_else(|| tries[0].clone());
        self.fonts.insert(key, found.clone());
        found
    }
}

/// A widget's `layers` as every copy walks them (`DefLayer`). Each is read as a scene's layers are: a key
/// that makes no sense is left at its default.
fn layers(list: Option<&Value>) -> Vec<DefLayer> {
    let mut kept = Vec::new();
    for (i, layer) in list.and_then(Value::as_array).into_iter().flatten().enumerate() {
        let Some(keys) = layer.as_object() else { continue };
        let opacity = keys.get("opacity").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0);
        if keys.get("visible").and_then(Value::as_bool) == Some(false) || opacity <= 0.0 {
            continue;
        }
        // a copy costs its step even when it's hidden; nothing else hidden, and nothing that isn't an
        // object, costs anything
        let objects: Vec<Value> = keys
            .get("objects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|o| o.is_object() && (is_copy(o) || o.get("visible").and_then(Value::as_bool) != Some(false)))
            .cloned()
            .collect();
        if objects.is_empty() {
            continue;
        }
        let id = keys.get("id").and_then(Value::as_str).filter(|l| !l.is_empty()).map_or_else(|| format!("l{i}"), |l| cut(l, MAX_ID));
        kept.push(DefLayer { id, opacity, objects });
    }
    kept
}

/// Whether an object is a copy of a widget.
fn is_copy(object: &Value) -> bool {
    object.get("type").and_then(Value::as_str) == Some("widget")
}

/// Where a holder's objects are placed: docs/widgets.md, Geometry. The top frame is the playlist's.
#[derive(Clone, Debug)]
pub struct Frame {
    /// What paths in the holder are relative to.
    pub folder: PathBuf,
    /// The holder's own input values (none at the top).
    pub scope: Rc<Scope>,
    pub s: f64,
    pub ox: i64,
    pub oy: i64,
    pub clip: Option<Rect>,
    pub groups: Vec<Group>,
    /// The widgets being expanded around it, outermost first.
    pub stack: Vec<String>,
    pub depth: usize,
    /// Its leaves' id prefix: `c/`, `c/w9/`.
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

/// The input values a copy draws with (docs/widgets.md, A copy's values and Passing inputs down): what it
/// sets, in its holder's terms, and the widget's defaults for the rest.
fn values(copy: &Value, frame: &Frame, def: &Def) -> Scope {
    let set_here = copy.get("inputs").and_then(Value::as_object);
    let mut scope = Scope::default();
    for input in &def.inputs {
        let set = set_here.and_then(|v| v.get(&input.id)).and_then(Value::as_str);
        match input.kind {
            InputKind::Text => {
                // cut again after its own text pass, so a value is never longer at any depth
                let text = match set {
                    Some(v) => substitute_cut(&cut(v, MAX_TEXT), &|id| frame.scope.text.get(id).cloned(), MAX_TEXT),
                    None => cut(&input.default, MAX_TEXT),
                };
                scope.text.insert(input.id.clone(), text);
            }
            InputKind::Color => {
                let set = set.and_then(|v| match reference(v) {
                    Some(id) => frame.scope.color.get(id).cloned(),
                    None => color(v),
                });
                let value = set.or_else(|| color(&input.default)).unwrap_or_else(|| "#ffffff".into());
                scope.color.insert(input.id.clone(), value);
            }
            InputKind::Image => {
                // a file, "" for none, or the holder's own image input; anything else isn't set
                let set = set.and_then(|v| {
                    let v = v.trim_matches(' ');
                    match reference(v) {
                        Some(id) => frame.scope.image.get(id).cloned(),
                        None if v.is_empty() => Some(PathBuf::new()),
                        None => path_ok(v).then(|| frame.folder.join(v)),
                    }
                });
                let value = set.unwrap_or_else(|| {
                    let d = input.default.trim_matches(' ');
                    if path_ok(d) { def.folder.join(d) } else { PathBuf::new() }
                });
                scope.image.insert(input.id.clone(), value);
            }
        }
    }
    scope
}

/// Expands the copies of one stack of layers (a scene's, or the global ones) within one budget.
pub struct Expander<'a> {
    lib: &'a mut Library,
    budget: usize,
    /// Names the scene or the global layers in log lines.
    holder: String,
}

impl<'a> Expander<'a> {
    pub fn new(lib: &'a mut Library, holder: &str) -> Expander<'a> {
        Expander { lib, budget: BUDGET, holder: holder.to_string() }
    }
}

impl Expander<'_> {
    /// One step of the budget (docs/widgets.md, Counting work), or false once it's spent.
    fn step(&mut self) -> bool {
        if self.budget == 0 {
            self.lib.log(format!("{}: its widgets take more than {BUDGET} steps, skipping the rest", self.holder));
            return false;
        }
        self.budget -= 1;
        true
    }

    /// Adds the objects the copy `value` (an object with "type": "widget") draws, placed in `frame`, to
    /// `out`: docs/widgets.md, sections 4 and 5.
    pub fn copy(&mut self, value: Value, frame: &Frame, out: &mut Vec<Object>) {
        self.expand(&value, frame, out);
    }

    /// `copy`, for a copy that stays where it is: one inside a widget is expanded again in every copy of
    /// that widget.
    fn expand(&mut self, value: &Value, frame: &Frame, out: &mut Vec<Object>) {
        // every copy reached costs its step, whether it draws or not
        if !self.step() {
            return;
        }
        let num = |key: &str| value.get(key).and_then(Value::as_f64);
        let id = cut(value.get("id").and_then(Value::as_str).unwrap_or_default(), MAX_ID);
        let opacity = num("opacity").unwrap_or(1.0).clamp(0.0, 1.0);
        if value.get("visible").and_then(Value::as_bool) == Some(false) || opacity <= 0.0 {
            return;
        }
        // what holds the copy, for log lines
        let holder = || frame.stack.last().map_or_else(|| self.holder.clone(), |w| format!("widget {w}"));
        let Some(widget) = value.get("widget").and_then(Value::as_str) else {
            let line = format!("{}: the copy {id} names no widget, so it draws nothing", holder());
            self.lib.log(line);
            return;
        };
        if frame.stack.iter().any(|w| w == widget) {
            let line = format!("{}: skipping the copy of {widget} inside itself", holder());
            self.lib.log(line);
            return;
        }
        if frame.depth >= MAX_DEPTH {
            let line = format!("{}: more than {MAX_DEPTH} copies deep, skipping {id}", holder());
            self.lib.log(line);
            return;
        }
        let def = match self.lib.get(widget) {
            Ok(def) => def,
            Err(e) => {
                self.lib.log(e);
                return;
            }
        };

        // the scale, then the box in whole pixels
        let scale = num("scale")
            .filter(|s| *s > 0.0)
            .unwrap_or_else(|| match num("w") {
                Some(w) if w > 0.0 => w / def.w as f64,
                _ => 1.0,
            })
            .clamp(0.01, 16.0);
        let s = frame.s * scale;
        let left = frame.ox.saturating_add(rnd(frame.s * num("x").unwrap_or(0.0)));
        let top = frame.oy.saturating_add(rnd(frame.s * num("y").unwrap_or(0.0)));
        let (width, height) = (rnd(s * def.w as f64).max(1), rnd(s * def.h as f64).max(1));
        let place = [left, top, left.saturating_add(width), top.saturating_add(height)];
        let clip = frame.clip.map_or(place, |c| meet(place, c));
        if empty(clip) {
            return;
        }

        let scope = Rc::new(values(value, frame, &def));
        let path = format!("{}{id}/", frame.path);
        let mut groups = frame.groups.clone();
        if opacity < 1.0 {
            groups.push(Group { id: format!("{}{id}", frame.path), opacity: opacity as f32, rect: meet(clip, PANEL) });
        }

        let background = def.background.as_deref().and_then(|b| match reference(b) {
            Some(input) => scope.color.get(input).cloned(),
            None => color(b),
        });
        if let Some(fill) = background {
            if !self.step() {
                return;
            }
            let shape = Shape { shape: ShapeKind::Rect, radius: 0.0, fill: Some(Paint::Solid { color: fill }), stroke: None, crisp: true };
            out.push(Object {
                id: format!("{path}background"),
                visible: true,
                opacity: 1.0,
                x: left as f64,
                y: top as f64,
                w: width as f64,
                h: height as f64,
                rotate: 0,
                flip: false,
                kind: Kind::Shape(shape),
                clip: Some(clip),
                groups: groups.clone(),
            });
        }

        // only the visible layers above opacity 0, each holding only what costs a step (DefLayer)
        for layer in &def.layers {
            let mut layer_groups = groups.clone();
            if layer.opacity < 1.0 {
                layer_groups.push(Group { id: format!("{path}{}", layer.id), opacity: layer.opacity as f32, rect: meet(clip, PANEL) });
            }
            for object in &layer.objects {
                if is_copy(object) {
                    // hidden or not: it takes its own step
                    let mut stack = frame.stack.clone();
                    stack.push(def.id.clone());
                    let inner = Frame { folder: def.folder.clone(), scope: scope.clone(), s, ox: left, oy: top, clip: Some(clip),
                                        groups: layer_groups.clone(), stack, depth: frame.depth + 1, path: path.clone() };
                    self.expand(object, &inner, out);
                } else {
                    if !self.step() {
                        return;
                    }
                    if let Some(mut leaf) = self.leaf(object, &def, &scope, s, (left, top)) {
                        leaf.id = format!("{path}{}", leaf.id);
                        if let Kind::Text(t) = &mut leaf.kind {
                            t.id = leaf.id.clone();
                            t.clip = Some(clip);
                        }
                        leaf.clip = Some(clip);
                        leaf.groups = layer_groups.clone();
                        out.push(leaf);
                    }
                }
            }
        }
    }

    /// One object of `def` (not a copy) with the copy's values filled in, read as a scene's objects are, then
    /// placed at scale `s` from `origin`; or None when it draws nothing. The caller adds its clip and groups.
    fn leaf(&mut self, object: &Value, def: &Def, scope: &Scope, s: f64, (ox, oy): (i64, i64)) -> Option<Object> {
        let mut value = object.clone();
        let id = cut(value.get("id").and_then(Value::as_str).unwrap_or_default(), MAX_ID);
        // a colour that's `{$id}` of a colour input takes its value. One that isn't a string (a number, null)
        // isn't a colour either: it becomes "", so reading keeps its stroke or gradient, and it takes the
        // fallback below like any other
        let bind = |field: Option<&mut Value>| {
            let Some(field) = field else { return };
            match field.as_str() {
                Some(v) => {
                    if let Some(c) = reference(v).and_then(|input| scope.color.get(input)) {
                        *field = Value::String(c.clone());
                    }
                }
                None => *field = Value::String(String::new()),
            }
        };
        let mut src = None;
        match value.get("type").and_then(Value::as_str).unwrap_or_default() {
            "image" | "video" => {
                let raw = value.get("src").and_then(Value::as_str).unwrap_or_default().trim_matches(' ');
                src = Some(match reference(raw) {
                    // an image input's file; "" (or a reference to anything else) draws nothing
                    Some(input) => scope.image.get(input).filter(|p| !p.as_os_str().is_empty())?.clone(),
                    None if raw.is_empty() => return None,
                    None if path_ok(raw) => def.folder.join(raw),
                    None => {
                        self.lib.log(format!("widget {}: leaving out {id}: its src {raw:?} isn't a path inside the widget's folder", def.id));
                        return None;
                    }
                });
                bind(value.get_mut("tint"));
            }
            "text" => {
                // cut before the live parse reads it, however many values it holds
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    value["text"] = Value::String(substitute_cut(text, &|input| scope.text.get(input).cloned(), MAX_LEAF_TEXT));
                }
                bind(value.get_mut("color"));
                bind(value.get_mut("outline").and_then(|o| o.get_mut("color")));
            }
            "shape" => {
                if let Some(fill) = value.get_mut("fill") {
                    bind(fill.get_mut("color"));
                    for stop in fill.get_mut("stops").and_then(Value::as_array_mut).into_iter().flatten() {
                        bind(stop.get_mut("color"));
                    }
                }
                bind(value.get_mut("stroke").and_then(|o| o.get_mut("color")));
            }
            _ => {}
        }
        // the JSON's own numbers, read as 64-bit floats: only the results go in narrower fields
        let size = value.get("size").and_then(Value::as_f64);
        let outline = value.get("outline").and_then(|o| o.get("width")).and_then(Value::as_f64);
        let font = value.get("font").and_then(Value::as_str).unwrap_or_default().to_string();

        // a key that makes no sense is logged once per read, not once per copy
        let lib = &mut *self.lib;
        let mut o = playlist::object_to(value, &def.folder, &mut |line| lib.log(format!("widget {}: {line}", def.id)))?;
        let (x, y, w, h) = (o.x, o.y, o.w, o.h);
        // edges, not sizes, are rounded, so parts that touch in the widget touch in every copy
        let (x0, y0) = (ox.saturating_add(rnd(s * x)), oy.saturating_add(rnd(s * y)));
        let (x1, y1) = (ox.saturating_add(rnd(s * (x + w))), oy.saturating_add(rnd(s * (y + h))));
        let fallback = |c: &mut String, or: &str| *c = color(c).unwrap_or_else(|| or.to_string());
        match &mut o.kind {
            Kind::Media(m) => {
                m.src = src?;
                let (pw, ph) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
                if pw > MAX_PLACED.0 || ph > MAX_PLACED.1 {
                    self.lib.log(format!("widget {}: leaving out {id}: placed at {pw} × {ph}, more than {} × {}", def.id, MAX_PLACED.0, MAX_PLACED.1));
                    return None;
                }
                (o.x, o.y, o.w, o.h) = (x0 as f64, y0 as f64, pw as f64, ph as f64);
            }
            Kind::Shape(sh) => {
                match &mut sh.fill {
                    Some(Paint::Solid { color }) => fallback(color, "#ffffff"),
                    Some(Paint::Linear { stops, .. }) => stops.iter_mut().for_each(|stop| fallback(&mut stop.color, "#ffffff")),
                    None => {}
                }
                if let Some(stroke) = &mut sh.stroke {
                    fallback(&mut stroke.color, "#ffffff");
                    stroke.width *= s;
                }
                sh.radius *= s;
                (o.x, o.y, o.w, o.h) = (x0 as f64, y0 as f64, x1.saturating_sub(x0) as f64, y1.saturating_sub(y0) as f64);
            }
            Kind::Text(t) => {
                fallback(&mut t.color, "#ffffff");
                fallback(&mut t.outline.color, "#000000");
                t.font = self.lib.font(def, &font);
                t.size = (size.unwrap_or(t.size as f64) * s) as f32;
                t.outline.width = (outline.unwrap_or(t.outline.width as f64) * s) as f32;
                (t.x, t.y) = (x0 as f32, y0 as f32);
                (o.x, o.y) = (x0 as f64, y0 as f64);
            }
        }
        o.id = id;
        Some(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vectors(name: &str) -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("widgets").join(name);
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
    }

    /// A scratch folder of its own for one test, removed afterwards.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wvplay-widget-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("playlist")).unwrap();
            std::fs::create_dir_all(dir.join("widgets")).unwrap();
            Scratch { dir }
        }

        fn write(&self, path: &str, text: &str) -> PathBuf {
            let path = self.dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn rnd_vectors() {
        for row in vectors("rnd.json") {
            let want = row["rnd"].as_i64().unwrap();
            assert_eq!(rnd(row["v"].as_f64().unwrap()), want, "{row}");
            if let (Some(a), Some(b)) = (row["a"].as_f64(), row["b"].as_f64()) {
                assert_eq!(a * b, row["v"].as_f64().unwrap(), "{row}: a parser that isn't correctly rounded");
                assert_eq!(rnd(a * b), want, "{row}");
            }
        }
    }

    #[test]
    fn substitute_vectors() {
        for row in vectors("inputs.json") {
            let inputs: HashMap<String, String> =
                row["inputs"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), cut(v.as_str().unwrap(), MAX_TEXT))).collect();
            let got = substitute(row["text"].as_str().unwrap(), &|id| inputs.get(id).cloned());
            assert_eq!(got, row["result"].as_str().unwrap(), "{row}");
        }
    }

    /// Stopping once it has `max` characters gives exactly the whole text pass cut to `max`.
    #[test]
    fn substitute_cut_is_a_cut() {
        let mut rows: Vec<(String, HashMap<String, String>)> = vectors("inputs.json")
            .iter()
            .map(|row| {
                let inputs = row["inputs"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), cut(v.as_str().unwrap(), MAX_TEXT)));
                (row["text"].as_str().unwrap().to_string(), inputs.collect())
            })
            .collect();
        rows.push(("😀{$a}é{$}{{$a}}{$b}x".into(), [("a".to_string(), "{😀}{$c}".to_string())].into_iter().collect()));
        for (text, inputs) in &rows {
            let input = |id: &str| inputs.get(id).cloned();
            let whole = substitute(text, &input);
            let n = whole.chars().count();
            // every cut of a short text; of a long one, the ends and every 97th
            for max in (0..=n + 1).filter(|m| n < 300 || *m < 40 || m % 97 == 0 || m + 40 > n) {
                assert_eq!(substitute_cut(text, &input, max), cut(&whole, max), "{text:?} cut to {max}");
            }
        }
    }

    #[test]
    fn make_safe_vectors() {
        for row in vectors("safe.json") {
            assert_eq!(make_safe(row["value"].as_str().unwrap()), row["safe"].as_str().unwrap(), "{row}");
        }
    }

    /// The copy of the live parser splits text exactly as data.rs's does.
    #[test]
    fn scan_matches_the_live_parser() {
        let mut texts: Vec<String> = Vec::new();
        for (file, key) in [("live.json", "text"), ("safe.json", "value"), ("safe.json", "safe"), ("inputs.json", "result")] {
            texts.extend(vectors(file).iter().filter_map(|r| r[key].as_str().map(str::to_string)));
        }
        for text in &texts {
            let mut live = false;
            let mut plain = String::new();
            scan(text, |p| match p {
                Part::Text(s) => plain.push_str(s),
                Part::Hole(body) => {
                    live = true;
                    plain.push_str(body.split_once('|').map_or("--", |(_, fallback)| fallback));
                }
            });
            assert_eq!(live, crate::data::is_live(text), "{text:?}");
            assert_eq!(plain, crate::data::fill(text, |_| None, chrono::Utc::now()), "{text:?}");
        }
    }

    /// The live parser reads the shared vectors as they say, a text a limit cut part way included.
    #[test]
    fn live_vectors() {
        for row in vectors("live.json") {
            let text = row["text"].as_str().unwrap();
            assert_eq!(crate::data::is_live(text), row["live"].as_bool().unwrap(), "{row}");
            assert_eq!(crate::data::fill(text, |_| None, chrono::Utc::now()), row["plain"].as_str().unwrap(), "{row}");
        }
    }

    #[test]
    fn color_vectors() {
        for row in vectors("colors.json") {
            assert_eq!(color(row["value"].as_str().unwrap()).as_deref(), row["color"].as_str(), "{row}");
        }
        // never panics, whatever the bytes
        let alphabet = ["é", "a", "F", "0", "#", " ", "\u{feff}", "{", "$", "}"];
        for n in 0..=4 {
            let mut idx = vec![0usize; n];
            loop {
                let s: String = idx.iter().map(|&i| alphabet[i]).collect();
                let _ = color(&s);
                let mut k = 0;
                while k < n && idx[k] + 1 == alphabet.len() {
                    idx[k] = 0;
                    k += 1;
                }
                if k == n {
                    break;
                }
                idx[k] += 1;
            }
        }
    }

    #[test]
    fn paths_and_font_names() {
        for good in ["media/a.png", "a", "_", "x.png", "a/b/c/d.png", "media/thermometer-5d41402a.png", "A_b-c.d.e"] {
            assert!(path_ok(good), "{good}");
        }
        let long = "a".repeat(101);
        for bad in ["", "../x.png", "C:/x.png", "/x.png", r"\\host\x.png", r"media\x.png", "media./a.png", "media/a.", "a/b/c/d/e.png",
                    "con", "CON.png", "media/nul.txt", "Lpt1.x.y", "-a", ".a", "a b.png", "médi/a.png", "a//b", "a/", long.as_str()] {
            assert!(!path_ok(bad), "{bad}");
        }
        assert!(path_ok(&"a".repeat(100)));
        for good in ["Orbitron Bold.ttf", "bahnschrift.ttf", "x.OTF", "Orbitron[wght].ttf", "con1.ttf", "a.b.ttf", "é.ttf"] {
            assert!(font_name_ok(good), "{good}");
        }
        let long = format!("{}.ttf", "é".repeat(101));
        for bad in ["con.ttf", "NUL.otf", " x.ttf", "x.ttf.", "x.ttf ", ".x.ttf", ".ttf", "x.woff", "fonts/x.ttf", r"C:\x.ttf",
                    "a:b.ttf", "a\u{1}.ttf", "con.x.ttf", long.as_str(), ""] {
            assert!(!font_name_ok(bad), "{bad}");
        }
        assert!(font_name_ok(&format!("{}.ttf", "é".repeat(100))));
    }

    #[test]
    fn ids() {
        let (forty, forty_one) = ("a".repeat(40), "a".repeat(41));
        for good in ["a", "temp-badge", "a1", "0", "com0", forty.as_str()] {
            assert!(id_ok(good), "{good}");
        }
        for bad in ["", "-a", "a-", "A", "Gauge", "../x", "con", "lpt9", "a_b", "a.b", "é", forty_one.as_str()] {
            assert!(!id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn library_reads_leniently() {
        let scratch = Scratch::new("library");
        let widgets = scratch.dir.join("widgets");
        scratch.write("widgets/ok/widget.json", r##"{ "version": 1, "w": 2000.4, "h": -5, "background": 7,
            "inputs": [ { "id": "label", "type": "text", "default": "CPU" }, { "id": "Label", "type": "text" },
                        { "id": "label", "type": "color" }, { "id": "n", "type": "number" }, "junk",
                        { "id": "c", "type": "color", "default": 5 } ],
            "layers": "nope" }"##);
        scratch.write("widgets/plain/widget.json", r#"{ "w": 10.5 }"#);
        scratch.write("widgets/bad/widget.json", "{ not json");
        scratch.write("widgets/list/widget.json", "[]");
        scratch.write("widgets/newer/widget.json", r#"{ "version": 2, "w": 10 }"#);
        scratch.write("widgets/CPU-Badge/widget.json", r#"{ "w": 10 }"#);
        std::fs::create_dir_all(widgets.join("empty")).unwrap();
        let many: Vec<String> = (0..40).map(|i| format!(r#"{{ "id": "i{i}", "type": "text" }}"#)).collect();
        scratch.write("widgets/many/widget.json", &format!(r#"{{ "inputs": [{}] }}"#, many.join(",")));
        // 1 MiB is the most read, as the studio saves; a byte more and the widget draws nothing
        let padded = |n: usize| {
            let head = r#"{ "w": 10, "description": ""#;
            format!("{head}{}\" }}", "x".repeat(n - head.len() - 3))
        };
        scratch.write("widgets/full/widget.json", &padded(1 << 20));
        scratch.write("widgets/big/widget.json", &padded((1 << 20) + 1));

        let mut lib = Library::new(widgets.clone());
        let ok = lib.get("ok").unwrap();
        assert_eq!((ok.w, ok.h, ok.background.clone(), ok.layers.len()), (1600, 1, None, 0));
        assert_eq!(ok.folder, widgets.join("ok"));
        let inputs: Vec<(&str, InputKind, &str)> = ok.inputs.iter().map(|i| (i.id.as_str(), i.kind, i.default.as_str())).collect();
        assert_eq!(inputs, [("label", InputKind::Text, "CPU"), ("c", InputKind::Color, "")]);
        let plain = lib.get("plain").unwrap();
        assert_eq!((plain.w, plain.h), (11, 120)); // half up; missing h is 120
        assert!(lib.get("bad").unwrap_err().contains("isn't valid JSON"));
        assert!(lib.get("list").unwrap_err().contains("isn't a JSON object"));
        assert!(lib.get("newer").unwrap_err().contains("newer studio (version 2)"));
        assert_eq!(lib.get("empty").unwrap_err(), r"widget empty: no widgets\empty\widget.json");
        assert_eq!(lib.get("gone").unwrap_err(), r"widget gone: no widgets\gone\widget.json");
        // a folder in other letter case isn't the widget, though Windows would open it
        assert!(lib.get("cpu-badge").unwrap_err().contains("no widgets"));
        assert!(lib.get("../x").unwrap_err().contains("isn't a widget id"));
        assert!(lib.get("con").is_err());
        assert_eq!(lib.get("many").unwrap().inputs.len(), MAX_INPUTS);
        assert_eq!(std::fs::metadata(widgets.join(r"full\widget.json")).unwrap().len(), 1 << 20);
        assert_eq!(lib.get("full").unwrap().w, 10);
        assert_eq!(lib.get("big").unwrap_err(), "widget big: widget.json is too big (more than 1 MiB)");
        // read once per read of the playlist
        assert!(Rc::ptr_eq(&lib.get("ok").unwrap(), &ok));

        // the folder, and every widget.json asked for, stamped, whether it's there or not
        let watched: HashMap<PathBuf, Option<Stamp>> = lib.watch.iter().cloned().collect();
        assert_eq!(watched[&widgets], playlist::stamp(&widgets));
        assert!(watched[&widgets].is_some());
        assert_eq!(watched[&widgets.join(r"gone\widget.json")], None);
        assert!(watched[&widgets.join(r"ok\widget.json")].is_some());
        assert!(!watched.contains_key(&widgets.join(r"..\x\widget.json")));
        assert!(watched.contains_key(&widgets.join(r"cpu-badge\widget.json")));
        // one too big is watched like any other, so making it smaller is noticed
        assert!(watched[&widgets.join(r"big\widget.json")].is_some_and(|(_, size)| size == (1 << 20) + 1));

        // no widgets folder at all: missing, and the folder is watched for being made
        let mut lib = Library::new(scratch.dir.join("nowhere"));
        assert!(lib.get("ok").is_err());
        assert_eq!(lib.watch[0], (scratch.dir.join("nowhere"), None));
    }

    /// What draws nothing and costs no step is left out once, when the widget is read, so a copy passes
    /// over none of it. A layer without an id keeps `l<index>`, counted among all the file's layers.
    #[test]
    fn layers_keep_only_what_costs_a_step() {
        let scratch = Scratch::new("layers");
        scratch.write("widgets/w/widget.json", r#"{ "layers": [
            { "id": "hidden", "visible": false, "objects": [ { "id": "a", "type": "shape" } ] },
            { "id": "clear", "opacity": 0, "objects": [ { "id": "b", "type": "widget", "widget": "x" } ] },
            "junk",
            { "opacity": 0.5, "visible": "no", "objects": [ 5, null, "x", { "id": "h", "type": "shape", "visible": false },
                { "id": "hc", "type": "widget", "widget": "x", "visible": false }, { "id": "s", "type": "shape", "visible": 3 } ] },
            { "id": "empty", "objects": [ { "id": "c", "type": "text", "visible": false } ] },
            { "id": "", "opacity": "x", "objects": [ { "id": "u", "type": "sticker" } ] },
            { "id": "list", "objects": "x" } ] }"#);
        let mut lib = Library::new(scratch.dir.join("widgets"));
        let def = lib.get("w").unwrap();
        let kept: Vec<(&str, f64, Vec<&str>)> =
            def.layers.iter().map(|l| (l.id.as_str(), l.opacity, l.objects.iter().map(|o| o["id"].as_str().unwrap()).collect())).collect();
        // a hidden copy still costs its step, and an object of a type nobody draws costs one too
        assert_eq!(kept, [("l3", 0.5, vec!["hc", "s"]), ("l5", 1.0, vec!["u"])]);
    }

    /// A key that makes no sense in a widget's object is logged once per read, however many copies read it.
    #[test]
    fn ignored_keys_are_logged_once() {
        let scratch = Scratch::new("logged");
        scratch.write("widgets/w/widget.json",
                      r#"{ "layers": [ { "objects": [ { "id": "i", "type": "image", "src": "a.png", "keep_aspect": "yes" } ] } ] }"#);
        let mut lib = Library::new(scratch.dir.join("widgets"));
        let top = Frame::top(&scratch.dir.join("playlist"));
        let mut out = Vec::new();
        let mut expander = Expander::new(&mut lib, "scene s1");
        for id in ["a", "b"] {
            expander.copy(serde_json::json!({ "id": id, "type": "widget", "widget": "w" }), &top, &mut out);
        }
        assert_eq!(out.len(), 2);
        // through the library's own log, which says each line once per read
        assert!(lib.logged.contains(r#"widget w: ignoring a image's keep_aspect: "yes" doesn't make sense there"#), "{:?}", lib.logged);
    }

    #[test]
    fn font_lookup() {
        let scratch = Scratch::new("fonts");
        let widgets = scratch.dir.join("widgets");
        scratch.write("widgets/w/widget.json", "{}");
        let mut lib = Library::new(widgets.clone());
        let def = lib.get("w").unwrap();
        let start = lib.watch.len();
        let windows = Path::new(r"C:\Windows\Fonts\arial.ttf");
        assert!(windows.exists(), "the test needs Arial");
        // Windows' own, after the widget's folder was tried
        assert_eq!(lib.font(&def, "arial.ttf"), windows);
        let tried: Vec<PathBuf> = lib.watch[start..].iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(tried, [widgets.join(r"w\fonts\arial.ttf"), windows.to_path_buf()]);
        // looked up once per read
        lib.font(&def, "arial.ttf");
        assert_eq!(lib.watch.len(), start + 2);

        // missing everywhere: every place is tried and watched, and the first is used (so Segoe UI draws)
        let name = format!("wvplay-test-{}.ttf", std::process::id());
        let start = lib.watch.len();
        assert_eq!(lib.font(&def, &name), widgets.join(r"w\fonts").join(&name));
        let mut want = vec![widgets.join(r"w\fonts").join(&name), Path::new(r"C:\Windows\Fonts").join(&name)];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            want.push(Path::new(&local).join(r"Microsoft\Windows\Fonts").join(&name));
        }
        want.push(scratch.dir.join("fonts").join(&name));
        let tried: Vec<(PathBuf, Option<Stamp>)> = lib.watch[start..].to_vec();
        assert_eq!(tried, want.iter().map(|p| (p.clone(), None)).collect::<Vec<_>>());

        // the project's fonts folder, then a bundled one beating Windows' own
        let project = scratch.write(&format!("fonts/{name}"), "x");
        let mut lib = Library::new(widgets.clone());
        let def = lib.get("w").unwrap();
        assert_eq!(lib.font(&def, &name), project);
        let bundled = scratch.write(r"widgets/w/fonts/arial.ttf", "x");
        assert_eq!(lib.font(&def, "arial.ttf"), bundled);
        assert_eq!(lib.watch.last().unwrap().0, bundled);

        // names that aren't one file name are Segoe UI, and no path is built from them
        let start = lib.watch.len();
        for name in ["", "fonts/arial.ttf", r"..\arial.ttf", "con.ttf", " arial.ttf", "arial.ttf.", r"C:\Windows\Fonts\arial.ttf"] {
            assert_eq!(lib.font(&def, name), Path::new(DEFAULT_FONT), "{name}");
        }
        assert_eq!(lib.watch.len(), start);
    }

    /// `wvplay play playlist.json` run inside playlist\ finds the widgets beside it, not inside it.
    #[test]
    fn widgets_beside_a_relative_playlist() {
        let scratch = Scratch::new("relative");
        let here = std::env::current_dir().unwrap();
        std::env::set_current_dir(scratch.dir.join("playlist")).unwrap();
        let found = (widgets_for(Path::new("")), widgets_for(Path::new(".")));
        std::env::set_current_dir(here).unwrap();
        let want = std::path::absolute(scratch.dir.join("widgets")).unwrap();
        assert_eq!(found.0, want);
        assert_eq!(found.1, want);
        assert_eq!(widgets_for(Path::new(r"C:\x\playlist")), Path::new(r"C:\x\widgets"));
    }
}
