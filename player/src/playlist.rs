//! What to play: one file, every image in a folder, or a playlist file.
//!
//! A playlist is scenes made of layers and objects (docs/scenes.md), with files relative to the playlist's
//! own folder. Version 1 files (one piece plus its text per item, docs/text.md) are turned into scenes as
//! they're read; nothing is written back.
//!
//! ```json
//! { "version": 2, "seconds": 30, "global": { "layers": [ ... ] },
//!   "scenes": [ { "id": "s1", "name": "city", "seconds": 20, "background": "#000000", "layers": [ ... ] } ] }
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::art::{HEIGHT, WIDTH};
use crate::text::DEFAULT_FONT;
use crate::widget::{self, Expander, Frame, Library};

const IMAGE_TYPES: [&str; 7] = ["png", "gif", "webp", "jpg", "jpeg", "bmp", "apng"];

/// left, top, right, bottom in panel pixels; right and bottom are exclusive.
pub type Rect = [i64; 4];
/// A file's modified time and size, as `stamp` gives it.
pub type Stamp = (SystemTime, u64);

/// The whole file, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub seconds: f64,
    /// Drawn over every scene, above its own layers.
    pub global: Vec<Layer>,
    pub scenes: Vec<Scene>,
    /// Every file whose change means reading again, with the stamp it had when the read looked at it,
    /// sorted: the target, the widgets' files (docs/widgets.md), and every media file and font drawn.
    pub watch: Vec<(PathBuf, Option<Stamp>)>,
}

/// One entry: a 1600x720 canvas with a background colour and a stack of layers, bottom first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Scene {
    pub id: String,
    pub name: String,
    pub seconds: f64,
    /// `#rrggbb`
    pub background: String,
    pub layers: Vec<Layer>,
}

impl Scene {
    /// The media files it draws, for stamps and fingerprints.
    pub fn media(&self) -> impl Iterator<Item = &Path> {
        self.layers.iter().flat_map(|l| l.objects.iter()).filter_map(|o| match &o.kind {
            Kind::Media(m) => Some(m.src.as_path()),
            _ => None,
        })
    }

    /// The fonts its text uses.
    pub fn fonts(&self) -> impl Iterator<Item = &Path> {
        self.layers.iter().flat_map(|l| l.objects.iter()).filter_map(|o| match &o.kind {
            Kind::Text(w) => Some(w.font.as_path()),
            _ => None,
        })
    }
}

/// A group of objects, blended onto the scene as a whole at its opacity.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Layer {
    pub id: String,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub objects: Vec<Object>,
}

impl Layer {
    /// Text with placeholders (docs/data.md), drawn while playing, with the layer's opacity folded in.
    pub fn live(&self) -> impl Iterator<Item = Text> + '_ {
        self.objects.iter().filter(|o| o.visible).filter_map(move |o| match &o.kind {
            Kind::Text(w) if w.live => Some(Text { opacity: w.opacity * self.opacity, ..w.clone() }),
            _ => None,
        })
    }
}

/// An image, video, text or shape. The keys every type has, plus its own.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Object {
    pub id: String,
    pub visible: bool,
    /// 0..1, within the layer's buffer.
    pub opacity: f32,
    /// The box in panel pixels as drawn (after rotation); for text, `x`/`y` are the anchor.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// 0, 90, 180 or 270, clockwise; `flip` mirrors left to right afterwards. Text and shapes ignore both.
    pub rotate: u16,
    pub flip: bool,
    pub kind: Kind,
    /// Where it may draw, when it's part of a widget's copy (docs/widgets.md): the copy's box, cut by any
    /// copy around it. None elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<Rect>,
    /// The translucent copies and widget layers it sits in, outermost first. Empty elsewhere.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
}

/// A copy or widget layer below 100% that an object sits in: its objects are drawn together into a buffer
/// covering `rect`, which is laid over what holds it at `opacity` (docs/widgets.md, Opacity).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    /// Its copy's or widget layer's id path: `c`, `c/l1`, `c/w9`.
    pub id: String,
    pub opacity: f32,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Kind {
    Media(Media),
    Text(Text),
    Shape(Shape),
}

/// An image, animation or video file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Media {
    /// Absolute once read (relative to the playlist's folder in the file).
    pub src: PathBuf,
    /// Lanczos instead of nearest neighbour.
    pub smooth: bool,
    pub fill: Fill,
    /// The file fitted inside its box, keeping its shape, and centred, when it's opened (docs/widgets.md).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub keep_aspect: bool,
    /// `#rrggbb`: every pixel the file draws takes this colour, keeping its own transparency
    /// (docs/widgets.md). A tint that isn't a colour is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tint: Option<String>,
}

impl Default for Media {
    fn default() -> Media {
        Media { src: PathBuf::new(), smooth: false, fill: Fill::None, keep_aspect: false, tint: None }
    }
}

/// How an image reaches past its box: not at all, its edge pixels stretched, or repeated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fill {
    #[default]
    None,
    Extend,
    Mirror,
    Tile,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shape {
    pub shape: ShapeKind,
    /// Corner radius, for rects.
    pub radius: f64,
    pub fill: Option<Paint>,
    pub stroke: Option<Stroke>,
    /// No anti-aliasing: a pixel is painted exactly when its centre is inside.
    pub crisp: bool,
}

impl Default for Shape {
    fn default() -> Shape {
        Shape { shape: ShapeKind::Rect, radius: 0.0, fill: Some(Paint::Solid { color: "#ffffff".into() }), stroke: None, crisp: false }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShapeKind {
    #[default]
    Rect,
    Ellipse,
    /// From (x, y) to (x + w, y + h); only its stroke is drawn.
    Line,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Paint {
    Solid { color: String },
    /// `angle` in degrees: 0 runs left to right, 90 top to bottom, across the object's box.
    Linear { #[serde(default)] angle: f64, #[serde(default)] stops: Vec<Stop> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stop {
    pub at: f64,
    pub color: String,
}

impl Default for Stop {
    fn default() -> Stop {
        Stop { at: 0.0, color: "#ffffff".into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stroke {
    pub color: String,
    /// Centred on the outline.
    pub width: f64,
}

impl Default for Stroke {
    fn default() -> Stroke {
        Stroke { color: "#000000".into(), width: 1.0 }
    }
}

/// A piece of text over the art. Keys left out take the defaults in docs/text.md.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Text {
    pub id: String,
    /// Only "text" is drawn; other types are skipped.
    #[serde(rename = "type")]
    pub kind: String,
    /// `\n` starts a new line.
    pub text: String,
    /// A .ttf or .otf file. If it's missing or unreadable, Segoe UI is used instead.
    pub font: PathBuf,
    /// Em size in panel pixels.
    pub size: f32,
    /// `#rrggbb`
    pub color: String,
    /// 0..1, for the fill and the outline.
    pub opacity: f32,
    /// The anchor point in upright panel pixels; `align` and `valign` say how the text sits on it.
    pub x: f32,
    pub y: f32,
    pub align: Align,
    pub valign: Valign,
    /// Distance between baselines, as a multiple of `size`.
    pub line_height: f32,
    pub outline: Outline,
    /// No anti-aliasing (alpha thresholded at 50%), for pixel fonts.
    pub crisp: bool,
    /// Its text has placeholders, filled in while playing. Not part of the file (or the cache keys).
    #[serde(skip)]
    pub live: bool,
    /// Its object's `clip`, for the text renderer. Not part of the file.
    #[serde(skip)]
    pub clip: Option<Rect>,
}

impl Default for Text {
    fn default() -> Text {
        Text {
            id: String::new(),
            kind: String::new(),
            text: String::new(),
            font: PathBuf::from(DEFAULT_FONT),
            size: 48.0,
            color: "#ffffff".into(),
            opacity: 1.0,
            x: 0.0,
            y: 0.0,
            align: Align::Left,
            valign: Valign::Top,
            line_height: 1.2,
            outline: Outline::default(),
            crisp: false,
            live: false,
            clip: None,
        }
    }
}

/// Where each line sits against `x`: starting at it, centred on it, or ending at it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// Where the block of lines sits against `y`: its top, middle or bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Valign {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Outline {
    /// `#rrggbb`
    pub color: String,
    /// Thickness in panel pixels around the glyphs; 0 means none.
    pub width: f32,
}

impl Default for Outline {
    fn default() -> Outline {
        Outline { color: "#000000".into(), width: 0.0 }
    }
}

/// The keys every object has, as they are in the file.
#[derive(Deserialize)]
#[serde(default)]
struct Common {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    visible: bool,
    opacity: f32,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    rotate: u16,
    flip: bool,
}

impl Default for Common {
    fn default() -> Common {
        Common { id: String::new(), kind: String::new(), visible: true, opacity: 1.0, x: 0.0, y: 0.0, w: 0.0, h: 0.0,
                 rotate: 0, flip: false }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct LayerKeys {
    id: String,
    name: String,
    visible: bool,
    opacity: f32,
    objects: Vec<Value>,
}

impl Default for LayerKeys {
    fn default() -> LayerKeys {
        LayerKeys { id: String::new(), name: String::new(), visible: true, opacity: 1.0, objects: Vec::new() }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct SceneKeys {
    id: String,
    name: String,
    seconds: Option<f64>,
    background: String,
    layers: Vec<Value>,
}

impl Default for SceneKeys {
    fn default() -> SceneKeys {
        SceneKeys { id: String::new(), name: String::new(), seconds: None, background: "#000000".into(), layers: Vec::new() }
    }
}

/// Media files, when read while playing: the pieces' sizes come from them.
pub fn read(target: &Path, default_seconds: f64, ffmpeg: &Path) -> Result<Playlist, Box<dyn Error>> {
    // stamped before it's read, so a save that lands during the read is seen as a change next time
    let before = stamp(target);
    let mut playlist = if is_json(target) {
        parse(&std::fs::read(target)?, target.parent().unwrap_or(Path::new(".")), default_seconds, ffmpeg)?
    } else {
        let mut paths = if target.is_dir() { images(target)? } else { vec![target.to_path_buf()] };
        paths.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        let scenes = paths.into_iter().enumerate().map(|(i, p)| piece(&p, i, default_seconds, ffmpeg)).collect();
        let mut playlist = Playlist { seconds: default_seconds, global: Vec::new(), scenes, watch: Vec::new() };
        watch_files(&mut playlist);
        playlist
    };
    if playlist.scenes.is_empty() {
        return Err(format!("nothing to play in {}", target.display()).into());
    }
    playlist.watch.insert(0, (target.to_path_buf(), before));
    sort_watch(&mut playlist.watch);
    Ok(playlist)
}

fn is_json(target: &Path) -> bool {
    target.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) && !target.is_dir()
}

/// The images and videos in a folder target.
fn images(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(std::fs::read_dir(folder)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && is_image(p)).collect())
}

/// Adds every media file and font the playlist draws to its watch list, stamped now, then sorts the list.
fn watch_files(playlist: &mut Playlist) {
    let mut files: Vec<&Path> = playlist
        .scenes
        .iter()
        .flat_map(|s| &s.layers)
        .chain(&playlist.global)
        .flat_map(|l| &l.objects)
        .filter_map(|o| match &o.kind {
            Kind::Media(m) => Some(m.src.as_path()),
            Kind::Text(t) => Some(t.font.as_path()),
            Kind::Shape(_) => None,
        })
        .collect();
    files.sort();
    files.dedup();
    let stamped: Vec<(PathBuf, Option<Stamp>)> = files.into_iter().map(|p| (p.to_path_buf(), stamp(p))).collect();
    playlist.watch.extend(stamped);
    sort_watch(&mut playlist.watch);
}

/// Sorted by path, each path once, with the stamp it was first given.
fn sort_watch(watch: &mut Vec<(PathBuf, Option<Stamp>)>) {
    watch.sort_by(|a, b| a.0.cmp(&b.0));
    watch.dedup_by(|later, first| later.0 == first.0);
}

/// A playlist file's scenes, with paths relative to `base`. Version 1 files are converted as they're read.
/// Copies of widgets (docs/widgets.md) are expanded into the objects they draw, within a budget for the
/// global layers and one for each scene.
pub fn parse(json: &[u8], base: &Path, default_seconds: f64, ffmpeg: &Path) -> Result<Playlist, Box<dyn Error>> {
    let file: Value = serde_json::from_slice(json)?;
    let version = file.get("version").and_then(Value::as_u64).unwrap_or(if file.get("scenes").is_some() { 2 } else { 1 });
    let fallback = file.get("seconds").and_then(Value::as_f64).unwrap_or(default_seconds);
    let file = if version >= 2 { file } else { convert(file, base, ffmpeg) };
    let mut library = Library::new(widget::widgets_for(base));
    let mut expander = Expander::new(&mut library, "the global layer");
    let global = layers(file.get("global").and_then(|g| g.get("layers")), base, &mut expander);
    let scenes = file
        .get("scenes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(i, value)| {
            let keys: SceneKeys = lenient(value, "scene")?;
            let id = if keys.id.is_empty() { format!("s{i}") } else { keys.id };
            let mut expander = Expander::new(&mut library, &format!("scene {id}"));
            Some(Scene {
                id,
                name: keys.name,
                seconds: keys.seconds.unwrap_or(fallback).max(1.0),
                background: keys.background,
                layers: layers(Some(&Value::Array(keys.layers)), base, &mut expander),
            })
        })
        .collect();
    let mut playlist = Playlist { seconds: fallback, global, scenes, watch: library.watch };
    watch_files(&mut playlist);
    Ok(playlist)
}

/// Version 1 (items with a file, a layout and their text `widgets`, plus an overlay) as version 2, in memory. See
/// docs/scenes.md, "Moving from version 1".
fn convert(file: Value, base: &Path, ffmpeg: &Path) -> Value {
    let get = |v: &Value, key: &str| v.get(key).cloned().unwrap_or(Value::Null);
    let layer = |name: &str, objects: Vec<Value>| serde_json::json!({ "id": name.to_lowercase(), "name": name, "objects": objects });
    let scenes: Vec<Value> = get(&file, "items")
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let file = PathBuf::from(item.get("file")?.as_str()?);
            let layout = get(&item, "layout");
            let source = item.get("source").and_then(Value::as_str).map(|s| base.join(s)).filter(|s| s.is_file());
            let mut art = serde_json::json!({ "id": "art", "type": "image" });
            let mut background = "#000000".to_string();
            match source {
                // the kept original, laid out as the studio showed it
                Some(src) => {
                    let (sw, sh) = crate::media::dimensions(&src, ffmpeg).unwrap_or((WIDTH, HEIGHT));
                    let rotate = get(&layout, "rotate").as_u64().unwrap_or(0) % 360;
                    let (tw, th) = if rotate % 180 == 90 { (sh, sw) } else { (sw, sh) };
                    let factor = get(&layout, "factor").as_f64().filter(|f| *f >= 1.0).unwrap_or(1.0);
                    let scale = get(&layout, "scale").as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
                    let w = (tw as f64 / factor * scale).round().max(1.0);
                    let h = (th as f64 / factor * scale).round().max(1.0);
                    let fill_key = get(&layout, "fill");
                    let fill = match fill_key.as_str() {
                        Some(f @ ("extend" | "mirror" | "tile")) => f,
                        _ => "none",
                    };
                    if let Some(bg) = get(&layout, "bg").as_str() {
                        background = bg.to_string();
                    }
                    art["src"] = Value::String(item["source"].as_str().unwrap_or_default().to_string());
                    art["x"] = get(&layout, "x").as_f64().unwrap_or(0.0).into();
                    art["y"] = get(&layout, "y").as_f64().unwrap_or(0.0).into();
                    art["w"] = w.into();
                    art["h"] = h.into();
                    art["rotate"] = rotate.into();
                    art["flip"] = get(&layout, "flip").as_bool().unwrap_or(false).into();
                    art["smooth"] = get(&layout, "smooth").as_bool().unwrap_or(false).into();
                    art["fill"] = fill.into();
                }
                // no original kept: the finished piece itself, as it fits the screen
                None => {
                    let dims = crate::media::dimensions(&base.join(&file), ffmpeg).unwrap_or((WIDTH, HEIGHT));
                    let (x, y, w, h) = fitted(dims);
                    art["src"] = Value::String(file.to_string_lossy().into_owned());
                    art["x"] = x.into();
                    art["y"] = y.into();
                    art["w"] = w.into();
                    art["h"] = h.into();
                    art["smooth"] = (w < dims.0).into(); // shrunk: resampled, as before
                }
            }
            let widgets = get(&item, "widgets").as_array().cloned().unwrap_or_default();
            let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mut scene = serde_json::json!({ "id": format!("s{i}"), "name": stem, "background": background,
                                                 "layers": [layer("Art", vec![art]), layer("Text", widgets)] });
            if let Some(seconds) = item.get("seconds") {
                scene["seconds"] = seconds.clone();
            }
            Some(scene)
        })
        .collect();
    let overlay = get(&file, "overlay").as_array().cloned().unwrap_or_default();
    serde_json::json!({ "version": 2, "seconds": get(&file, "seconds"), "global": { "layers": [layer("Text", overlay)] }, "scenes": scenes })
}

/// A file on its own (or in a folder) as a scene: fitted to the screen as pieces always were.
fn piece(path: &Path, index: usize, seconds: f64, ffmpeg: &Path) -> Scene {
    let dims = crate::media::dimensions(path, ffmpeg).unwrap_or((WIDTH, HEIGHT));
    let (x, y, w, h) = fitted(dims);
    let smooth = w < dims.0; // shrunk: resampled, as before
    let art = Object {
        id: "art".into(),
        visible: true,
        opacity: 1.0,
        x: x as f64,
        y: y as f64,
        w: w as f64,
        h: h as f64,
        rotate: 0,
        flip: false,
        kind: Kind::Media(Media { src: path.to_path_buf(), smooth, fill: Fill::None, keep_aspect: false, tint: None }),
        clip: None,
        groups: Vec::new(),
    };
    Scene {
        id: format!("f{index}"),
        name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        seconds,
        background: "#000000".into(),
        layers: vec![Layer { id: "art".into(), name: "Art".into(), visible: true, opacity: 1.0, objects: vec![art] }],
    }
}

/// Where a picture of this size goes on the screen: smaller art at the biggest whole-number scale that
/// fits (so pixels stay crisp), bigger art shrunk to fit, both centred.
pub fn fitted((w, h): (u32, u32)) -> (i64, i64, u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let (nw, nh) = if (w, h) == (WIDTH * 3 / 2, HEIGHT * 3 / 2) {
        (WIDTH, HEIGHT) // made for the glass's own 2400x1080
    } else {
        let s = (WIDTH / w).min(HEIGHT / h);
        if s >= 1 {
            (w * s, h * s)
        } else {
            let k = (WIDTH as f64 / w as f64).min(HEIGHT as f64 / h as f64);
            (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1))
        }
    };
    (((WIDTH - nw) / 2) as i64, ((HEIGHT - nh) / 2) as i64, nw, nh)
}

/// The layers in `list`, each copy of a widget in them expanded by `expander`.
fn layers(list: Option<&Value>, base: &Path, expander: &mut Expander) -> Vec<Layer> {
    list.and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(i, value)| {
            let keys: LayerKeys = lenient(value, "layer")?;
            let opacity = keys.opacity.clamp(0.0, 1.0);
            let mut objects = Vec::new();
            for value in keys.objects {
                if value.get("type").and_then(Value::as_str) == Some("widget") {
                    // a copy in a hidden layer isn't expanded, and costs nothing
                    if keys.visible && opacity > 0.0 {
                        expander.copy(value, &Frame::top(base), &mut objects);
                    }
                } else {
                    objects.extend(object(value, base));
                }
            }
            Some(Layer {
                id: if keys.id.is_empty() { format!("l{i}") } else { keys.id },
                name: keys.name,
                visible: keys.visible,
                opacity,
                objects,
            })
        })
        .collect()
}

/// An object, or None for one of a type this player doesn't know (skipped, rather than stopping the
/// whole playlist from playing).
pub(crate) fn object(value: Value, base: &Path) -> Option<Object> {
    object_to(value, base, &mut |line| crate::player::log(line))
}

/// `object`, telling `note` instead of the log about each key it ignores. It's how a widget's objects are
/// read, once a copy's values are in: they're read again for every copy, but logged once per read.
pub(crate) fn object_to(value: Value, base: &Path, note: &mut dyn FnMut(String)) -> Option<Object> {
    let common: Common = lenient_to(value.clone(), "object", note)?;
    let kind = match common.kind.as_str() {
        "image" | "video" => {
            let mut media: Media = lenient_to(value, "image", note)?;
            if media.src.as_os_str().is_empty() {
                return None;
            }
            media.src = base.join(&media.src);
            media.tint = media.tint.as_deref().and_then(widget::color);
            Kind::Media(media)
        }
        "text" => {
            let mut w: Text = lenient_to(value, "text", note)?;
            // a relative font is next to the playlist, like the pieces; no font at all means the default
            w.font = if w.font.as_os_str().is_empty() { PathBuf::from(DEFAULT_FONT) } else { base.join(&w.font) };
            w.live = crate::data::is_live(&w.text);
            if !w.live {
                w.text = crate::data::fill(&w.text, |_| None, chrono::Utc::now()); // `{{` and `}}` as plain braces
            }
            Kind::Text(w)
        }
        "shape" => Kind::Shape(lenient_to(value, "shape", note)?),
        _ => return None,
    };
    Some(Object {
        id: common.id,
        visible: common.visible,
        opacity: common.opacity.clamp(0.0, 1.0),
        x: common.x,
        y: common.y,
        w: common.w,
        h: common.h,
        rotate: if [0, 90, 180, 270].contains(&common.rotate) { common.rotate } else { 0 },
        flip: common.flip,
        kind,
        clip: None,
        groups: Vec::new(),
    })
}

/// `T` from a JSON object, with any value of the wrong kind (a size of "big", an align of "centre") left
/// at its default, as the studio's preview does.
fn lenient<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Option<T> {
    lenient_to(value, what, &mut |line| crate::player::log(line))
}

/// `lenient`, telling `note` instead of the log about each value it leaves at its default.
fn lenient_to<T: serde::de::DeserializeOwned>(value: Value, what: &str, note: &mut dyn FnMut(String)) -> Option<T> {
    let Value::Object(mut keys) = value else { return None };
    keys.retain(|key, value| {
        let alone = Value::Object([(key.clone(), value.clone())].into_iter().collect());
        let ok = serde_json::from_value::<T>(alone).is_ok();
        if !ok {
            note(format!("ignoring a {what}'s {key}: {value} doesn't make sense there"));
        }
        ok
    });
    serde_json::from_value(Value::Object(keys)).ok()
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| IMAGE_TYPES.iter().chain(&crate::art::VIDEO_TYPES).any(|t| e.eq_ignore_ascii_case(t)))
}

/// The paths of `playlist.watch` (and a folder target's images, as they are now), each with its stamp
/// now: it differs from `playlist.watch` exactly when the playlist, the folder's contents, a widget, a
/// media file or a font changed since the read. Only stats: the list was made once, by the read.
pub fn fingerprint(target: &Path, playlist: &Playlist) -> Vec<(PathBuf, Option<Stamp>)> {
    let mut paths: Vec<PathBuf> = playlist.watch.iter().map(|(p, _)| p.clone()).collect();
    paths.push(target.to_path_buf());
    if !target.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) && target.is_dir() {
        paths.extend(images(target).unwrap_or_default());
    }
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|p| {
            let s = stamp(&p);
            (p, s)
        })
        .collect()
}

/// A file's modified time and size, to notice when it's been replaced.
pub fn stamp(path: &Path) -> Option<Stamp> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(json: &str) -> Playlist {
        parse(json.as_bytes(), Path::new(r"C:\pl"), 30.0, Path::new("ffmpeg")).unwrap()
    }

    #[test]
    fn text_defaults() {
        let w: Text = serde_json::from_str(r#"{ "id": "w1", "type": "text" }"#).unwrap();
        assert_eq!(w, Text { id: "w1".into(), kind: "text".into(), ..Text::default() });
        assert_eq!(w.font, PathBuf::from(r"C:\Windows\Fonts\segoeui.ttf"));
        assert_eq!((w.size, w.opacity, w.x, w.y, w.line_height), (48.0, 1.0, 0.0, 0.0, 1.2));
        assert_eq!((w.color.as_str(), w.align, w.valign, w.crisp), ("#ffffff", Align::Left, Valign::Top, false));
        assert_eq!(w.outline, Outline { color: "#000000".into(), width: 0.0 });
        // half an outline keeps the other half's default
        let w: Text = serde_json::from_str(r#"{ "type": "text", "outline": { "width": 3 } }"#).unwrap();
        assert_eq!(w.outline, Outline { color: "#000000".into(), width: 3.0 });
    }

    #[test]
    fn version_2_objects() {
        let p = read_json(r##"{ "version": 2, "seconds": 10,
            "global": { "layers": [ { "id": "g", "name": "Top", "opacity": 0.5, "objects": [
                { "id": "clock", "type": "text", "text": "{time}", "opacity": 0.5 } ] } ] },
            "scenes": [ { "id": "a", "name": "one", "background": "#102030", "layers": [
                { "id": "l1", "name": "Art", "objects": [
                    { "id": "i", "type": "image", "src": "sources/x.png", "x": 1, "y": 2, "w": 30, "h": 40, "rotate": 90, "flip": true, "fill": "mirror", "smooth": true },
                    { "id": "v", "type": "video", "src": "sources/v.mp4", "rotate": 45 },
                    { "id": "s", "type": "shape", "shape": "ellipse", "w": 10, "h": 10, "fill": { "type": "linear", "angle": 90, "stops": [ { "at": 0, "color": "#000000" }, { "at": 1, "color": "#ffffff" } ] }, "stroke": { "color": "#ff0000", "width": 2 }, "crisp": true },
                    { "id": "n", "type": "shape", "fill": null },
                    { "id": "t", "type": "text", "text": "hi", "font": "fonts/p.ttf", "x": 5, "size": "big" },
                    { "id": "u", "type": "sticker" },
                    { "id": "e", "type": "image" },
                    "junk"
                ] },
                { "id": "l2", "visible": false, "objects": [] }
            ] }, { "layers": [] } ] }"##);
        assert_eq!(p.seconds, 10.0);
        assert_eq!(p.scenes.len(), 2);
        let s = &p.scenes[0];
        assert_eq!((s.id.as_str(), s.name.as_str(), s.seconds, s.background.as_str()), ("a", "one", 10.0, "#102030"));
        assert_eq!(s.layers.len(), 2);
        assert!(!s.layers[1].visible);
        let ids: Vec<&str> = s.layers[0].objects.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["i", "v", "s", "n", "t"]); // unknown types, an image without a file, and junk are skipped
        let i = &s.layers[0].objects[0];
        assert_eq!((i.x, i.y, i.w, i.h, i.rotate, i.flip), (1.0, 2.0, 30.0, 40.0, 90, true));
        assert_eq!(i.kind, Kind::Media(Media { src: PathBuf::from(r"C:\pl\sources/x.png"), smooth: true, fill: Fill::Mirror, keep_aspect: false, tint: None }));
        assert_eq!(s.layers[0].objects[1].rotate, 0); // 45 isn't a quarter turn
        let Kind::Shape(sh) = &s.layers[0].objects[2].kind else { panic!() };
        assert_eq!((sh.shape, sh.crisp), (ShapeKind::Ellipse, true));
        assert!(matches!(&sh.fill, Some(Paint::Linear { angle, stops }) if *angle == 90.0 && stops.len() == 2));
        assert_eq!(sh.stroke, Some(Stroke { color: "#ff0000".into(), width: 2.0 }));
        let Kind::Shape(n) = &s.layers[0].objects[3].kind else { panic!() };
        assert_eq!((n.fill.clone(), n.stroke.clone(), n.shape), (None, None, ShapeKind::Rect));
        let Kind::Text(t) = &s.layers[0].objects[4].kind else { panic!() };
        assert_eq!((t.text.as_str(), t.x, t.size), ("hi", 5.0, 48.0));
        assert_eq!(t.font, PathBuf::from(r"C:\pl\fonts/p.ttf"));
        // the second scene gets an id, the fallback seconds and a black background
        assert_eq!((p.scenes[1].id.as_str(), p.scenes[1].seconds, p.scenes[1].background.as_str()), ("s1", 10.0, "#000000"));
        // the global layer's live text carries the layer's opacity
        assert_eq!(p.global.len(), 1);
        let live: Vec<Text> = p.global[0].live().collect();
        assert_eq!((live.len(), live[0].opacity), (1, 0.25));
    }

    #[test]
    fn version_1_converts() {
        let dir = std::env::temp_dir().join(format!("wvplay-v1-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sources")).unwrap();
        image::RgbaImage::new(100, 50).save(dir.join("sources").join("ab.png")).unwrap();
        image::RgbaImage::new(1600, 720).save(dir.join("piece.png")).unwrap();
        let json = r##"{ "seconds": 12,
            "overlay": [ { "id": "g1", "type": "text", "text": "{time}" } ],
            "items": [
                { "file": "city.png", "seconds": 5, "source": "sources/ab.png",
                  "layout": { "factor": 2, "scale": 3, "x": -7, "y": 8, "rotate": 90, "flip": true, "smooth": true, "fill": "extend", "bg": "#123456" },
                  "widgets": [ { "id": "w1", "type": "text", "text": "hello" } ] },
                { "file": "piece.png" },
                { "file": "gone.gif", "source": "sources/missing.png", "layout": { "fill": "solid" } }
            ] }"##;
        let p = parse(json.as_bytes(), &dir, 30.0, Path::new("ffmpeg")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(p.seconds, 12.0);
        assert_eq!(p.scenes.len(), 3);
        let s = &p.scenes[0];
        assert_eq!((s.id.as_str(), s.name.as_str(), s.seconds, s.background.as_str()), ("s0", "city", 5.0, "#123456"));
        let names: Vec<&str> = s.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Art", "Text"]);
        let art = &s.layers[0].objects[0];
        // turned 90: 50x100 file pixels, 2 per art pixel, at 3x: 75x150
        assert_eq!((art.x, art.y, art.w, art.h, art.rotate, art.flip), (-7.0, 8.0, 75.0, 150.0, 90, true));
        assert_eq!(art.kind, Kind::Media(Media { src: dir.join("sources/ab.png"), smooth: true, fill: Fill::Extend, keep_aspect: false, tint: None }));
        let Kind::Text(w) = &s.layers[1].objects[0].kind else { panic!() };
        assert_eq!(w.text, "hello");
        // no original kept: the piece itself, filling the screen; a missing original likewise
        let piece = &p.scenes[1].layers[0].objects[0];
        assert_eq!((piece.x, piece.y, piece.w, piece.h), (0.0, 0.0, 1600.0, 720.0));
        assert_eq!(p.scenes[1].seconds, 12.0);
        assert!(matches!(&piece.kind, Kind::Media(m) if m.src == dir.join("piece.png") && m.fill == Fill::None));
        assert!(matches!(&p.scenes[2].layers[0].objects[0].kind, Kind::Media(m) if m.src == dir.join("gone.gif")));
        // the overlay is the global layer's one layer
        assert_eq!(p.global.len(), 1);
        assert_eq!(p.global[0].name, "Text");
        assert_eq!(p.global[0].live().count(), 1);
    }

    #[test]
    fn version_1_real_playlist() {
        let real = Path::new(r"C:\Git\CoolerVideo\playlist\playlist.json");
        if !real.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("wvplay-real-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sources")).unwrap();
        std::fs::copy(real, dir.join("playlist.json")).unwrap();
        for f in std::fs::read_dir(real.parent().unwrap().join("sources")).unwrap().flatten() {
            std::fs::copy(f.path(), dir.join("sources").join(f.file_name())).unwrap();
        }
        let p = read(&dir.join("playlist.json"), 30.0, Path::new("ffmpeg")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!p.scenes.is_empty());
        let s = &p.scenes[0];
        assert_eq!(s.name, "city");
        assert_eq!(s.layers.len(), 2);
        let art = &s.layers[0].objects[0];
        // 1600x720 at 1.118: 1789x805 at -76,-72, edges stretched
        assert_eq!((art.x, art.y, art.w, art.h), (-76.0, -72.0, 1789.0, 805.0));
        assert!(matches!(&art.kind, Kind::Media(m) if m.fill == Fill::Extend && !m.smooth));
        assert_eq!(p.global[0].live().count(), 1);
    }

    #[test]
    fn live_texts() {
        let p = read_json(r#"{ "version": 2, "global": { "layers": [ { "objects": [ { "id": "o", "type": "text", "text": "{time}" } ] } ] },
            "scenes": [ { "layers": [ { "objects": [ { "id": "l", "type": "text", "text": "CPU {pc.cpu.load:d}%" },
                                                    { "id": "s", "type": "text", "text": "{{not live}}" } ] } ] } ] }"#);
        let ids: Vec<String> = p.scenes[0].layers[0].live().map(|w| w.id).collect();
        assert_eq!(ids, ["l"]);
        let Kind::Text(s) = &p.scenes[0].layers[0].objects[1].kind else { panic!() };
        assert_eq!(s.text, "{not live}"); // static text has its escapes made plain
        assert_eq!(p.global[0].live().count(), 1);
    }

    #[test]
    fn fits_pieces() {
        assert_eq!(fitted((1600, 720)), (0, 0, 1600, 720));
        assert_eq!(fitted((160, 72)), (0, 0, 1600, 720));
        assert_eq!(fitted((480, 254)), (320, 106, 960, 508)); // 2x, centred
        assert_eq!(fitted((2400, 1080)), (0, 0, 1600, 720));
        assert_eq!(fitted((3200, 720)), (0, 180, 1600, 360));
    }
}

/// Reading widgets and expanding copies (docs/widgets.md). Owned by work package P1.
#[cfg(test)]
mod expand_tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    /// A scratch root of its own for one test, with `playlist\` and `widgets\` in it, removed afterwards.
    struct Root {
        dir: PathBuf,
    }

    impl Root {
        fn new(name: &str) -> Root {
            let dir = std::env::temp_dir().join(format!("wvplay-expand-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("playlist")).unwrap();
            std::fs::create_dir_all(dir.join("widgets")).unwrap();
            Root { dir }
        }

        fn write(&self, path: &str, text: &str) -> PathBuf {
            let path = self.dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }

        /// A widget whose one layer holds `objects`, with `keys` (each followed by a comma) besides.
        fn widget(&self, id: &str, keys: &str, objects: &str) {
            self.write(&format!("widgets/{id}/widget.json"),
                       &format!(r#"{{ "version": 1, {keys} "layers": [ {{ "id": "l1", "objects": [{objects}] }} ] }}"#));
        }

        fn target(&self) -> PathBuf {
            self.dir.join(r"playlist\playlist.json")
        }

        /// A playlist of one scene whose one layer holds `objects`, written, then read.
        fn read(&self, objects: &str) -> Playlist {
            self.write("playlist/playlist.json",
                       &format!(r#"{{ "version": 2, "scenes": [ {{ "id": "s1", "layers": [ {{ "id": "l1", "objects": [{objects}] }} ] }} ] }}"#));
            self.again()
        }

        fn again(&self) -> Playlist {
            read(&self.target(), 30.0, Path::new("ffmpeg")).unwrap()
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn leaves(p: &Playlist) -> &[Object] {
        &p.scenes[0].layers[0].objects
    }

    fn ids(p: &Playlist) -> Vec<&str> {
        leaves(p).iter().map(|o| o.id.as_str()).collect()
    }

    fn text(o: &Object) -> &Text {
        match &o.kind {
            Kind::Text(t) => t,
            _ => panic!("{} isn't text", o.id),
        }
    }

    fn shape(o: &Object) -> &Shape {
        match &o.kind {
            Kind::Shape(s) => s,
            _ => panic!("{} isn't a shape", o.id),
        }
    }

    fn media(o: &Object) -> &Media {
        match &o.kind {
            Kind::Media(m) => m,
            _ => panic!("{} isn't media", o.id),
        }
    }

    #[test]
    fn copy_expands_in_place() {
        let root = Root::new("in-place");
        root.widget("w", r##""w": 10, "h": 10, "background": "#102030","##, r#"{ "id": "r", "type": "shape", "w": 2, "h": 2 }"#);
        let p = root.read(r#"{ "id": "a", "type": "shape" }, { "id": "c", "type": "widget", "widget": "w", "x": 5, "y": 6 },
                             { "id": "b", "type": "shape" }"#);
        assert_eq!(ids(&p), ["a", "c/background", "c/r", "b"]);
        let l = leaves(&p);
        // the scene's own objects as they were; the copy's parts in its box, clipped to it
        assert_eq!((l[0].clip, l[0].groups.len(), l[3].clip), (None, 0, None));
        assert_eq!((l[1].x, l[1].y, l[1].w, l[1].h, l[1].clip), (5.0, 6.0, 10.0, 10.0, Some([5, 6, 15, 16])));
        assert_eq!(shape(&l[1]), &Shape { shape: ShapeKind::Rect, radius: 0.0, fill: Some(Paint::Solid { color: "#102030".into() }), stroke: None, crisp: true });
        assert_eq!((l[2].x, l[2].y, l[2].w, l[2].h, l[2].clip), (5.0, 6.0, 2.0, 2.0, Some([5, 6, 15, 16])));
    }

    #[test]
    fn transform_after_defaults() {
        let root = Root::new("defaults");
        root.widget("w", r#""w": 10, "h": 10,"#,
                    r##"{ "id": "t", "type": "text", "text": "x", "x": 1, "y": 2 },
                        { "id": "o", "type": "text", "text": "y", "outline": { "color": "#ffffff" } },
                        { "id": "s", "type": "shape", "w": 3, "h": 4, "stroke": {} },
                        { "id": "e", "type": "shape", "shape": "ellipse", "radius": 2, "stroke": { "width": 1.5 } }"##);
        let p = root.read(r#"{ "id": "c", "type": "widget", "widget": "w", "x": 10, "y": 20, "scale": 2 }"#);
        let t = text(&leaves(&p)[0]);
        // the default size, then scaled
        assert_eq!((t.size, t.x, t.y, t.outline.width), (96.0, 12.0, 24.0, 0.0));
        assert_eq!((leaves(&p)[0].x, leaves(&p)[0].y), (12.0, 24.0));
        assert_eq!(t.clip, Some([10, 20, 30, 40]));
        assert_eq!(t.id, "c/t");
        assert_eq!(text(&leaves(&p)[1]).outline, Outline { color: "#ffffff".into(), width: 0.0 });
        let s = shape(&leaves(&p)[2]);
        assert_eq!((s.radius, s.stroke.clone()), (0.0, Some(Stroke { color: "#000000".into(), width: 2.0 })));
        assert_eq!((leaves(&p)[2].w, leaves(&p)[2].h), (6.0, 8.0));
        let e = shape(&leaves(&p)[3]);
        assert_eq!((e.radius, e.stroke.as_ref().unwrap().width, e.fill.clone()), (4.0, 3.0, Some(Paint::Solid { color: "#ffffff".into() })));
    }

    #[test]
    fn inputs_make_text_live() {
        let root = Root::new("live");
        root.widget("g", r#""inputs": [ { "id": "v", "name": "Value", "type": "text", "default": "plain" } ],"#,
                    r#"{ "id": "t", "type": "text", "text": "{$v}" }"#);
        let p = root.read(r#"{ "id": "a", "type": "widget", "widget": "g", "inputs": { "v": "{pc.gpu.temp}" } },
                             { "id": "b", "type": "widget", "widget": "g", "inputs": { "v": "GPU" } },
                             { "id": "c", "type": "widget", "widget": "g", "inputs": { "v": 5 } }"#);
        let shown: Vec<(&str, bool)> = leaves(&p).iter().map(|o| (text(o).text.as_str(), text(o).live)).collect();
        // live in the one copy only; a value that isn't a string isn't set
        assert_eq!(shown, [("{pc.gpu.temp}", true), ("GPU", false), ("plain", false)]);
    }

    #[test]
    fn values_are_made_safe() {
        let root = Root::new("safe");
        root.widget("g", r#""inputs": [ { "id": "v", "type": "text" } ],"#,
                    r#"{ "id": "t", "type": "text", "text": "{$v}{pc.cpu.temp}" }, { "id": "u", "type": "text", "text": "[{$v}]" }"#);
        let long = format!("{}😀", "a".repeat(1999));
        let p = root.read(&format!(r#"{{ "id": "a", "type": "widget", "widget": "g", "inputs": {{ "v": "{{$x}}" }} }},
                                      {{ "id": "b", "type": "widget", "widget": "g", "inputs": {{ "v": "x{{" }} }},
                                      {{ "id": "c", "type": "widget", "widget": "g", "inputs": {{ "v": "5}}" }} }},
                                      {{ "id": "d", "type": "widget", "widget": "g", "inputs": {{ "v": "{long}x" }} }}"#));
        let shown: Vec<(&str, bool)> = leaves(&p).iter().map(|o| (text(o).text.as_str(), text(o).live)).collect();
        assert_eq!(&shown[..6], [
            // a reference in a value shows as written, and makes nothing live
            ("{{$x}}{pc.cpu.temp}", true), ("[{$x}]", false),
            // a value can't join with the text around it
            ("x{{{pc.cpu.temp}", true), ("[x{]", false),
            ("5}}{pc.cpu.temp}", true), ("[5}]", false),
        ]);
        // cut to 2000 characters, never in the middle of one
        assert_eq!(shown[7].0, format!("[{long}]"));
    }

    #[test]
    fn reference_builds_placeholder() {
        let root = Root::new("placeholder");
        root.widget("g", r#""inputs": [ { "id": "sensor", "type": "text", "default": "cpu" } ],"#,
                    r#"{ "id": "t", "type": "text", "text": "{pc.{$sensor}.temp:d}°C" }"#);
        let p = root.read(r#"{ "id": "a", "type": "widget", "widget": "g", "inputs": { "sensor": "gpu" } },
                             { "id": "b", "type": "widget", "widget": "g" }"#);
        let shown: Vec<(&str, bool)> = leaves(&p).iter().map(|o| (text(o).text.as_str(), text(o).live)).collect();
        assert_eq!(shown, [("{pc.gpu.temp:d}°C", true), ("{pc.cpu.temp:d}°C", true)]);
    }

    fn stamp_of<'a>(p: &'a Playlist, path: &Path) -> Option<&'a Option<Stamp>> {
        p.watch.iter().find(|(q, _)| q == path).map(|(_, s)| s)
    }

    #[test]
    fn missing_widget_is_watched() {
        let root = Root::new("missing");
        root.write("widgets/other/widget.json", "{}");
        let p = root.read(r#"{ "id": "c", "type": "widget", "widget": "gone" }"#);
        let (target, widgets) = (root.target(), root.dir.join("widgets"));
        assert!(leaves(&p).is_empty());
        assert_eq!(stamp_of(&p, &widgets.join(r"gone\widget.json")), Some(&None));
        assert!(stamp_of(&p, &widgets).is_some_and(Option::is_some));
        assert_eq!(fingerprint(&target, &p), p.watch);

        // the widget put in place
        std::thread::sleep(Duration::from_millis(30));
        root.widget("gone", "", r#"{ "id": "t", "type": "text", "text": "x", "font": "arial.ttf" }"#);
        assert_ne!(fingerprint(&target, &p), p.watch);
        let p = root.again();
        assert_eq!(ids(&p), ["c/t"]);
        assert_eq!(fingerprint(&target, &p), p.watch);

        // another folder in widgets\ renamed
        std::thread::sleep(Duration::from_millis(30));
        std::fs::rename(widgets.join("other"), widgets.join("other2")).unwrap();
        assert_ne!(fingerprint(&target, &p), p.watch);
        let p = root.again();
        assert_eq!(fingerprint(&target, &p), p.watch);

        // a font dropped into the widget's fonts\ later, where the lookup tried first
        assert_eq!(stamp_of(&p, &widgets.join(r"gone\fonts\arial.ttf")), Some(&None));
        assert_eq!(text(&leaves(&p)[0]).font, Path::new(r"C:\Windows\Fonts\arial.ttf"));
        root.write("widgets/gone/fonts/arial.ttf", "x");
        assert_ne!(fingerprint(&target, &p), p.watch);
        assert_eq!(text(&leaves(&root.again())[0]).font, widgets.join(r"gone\fonts\arial.ttf"));
    }

    #[test]
    fn stamps_before_reading() {
        let root = Root::new("stamps");
        root.widget("w", "", r#"{ "id": "r", "type": "shape" }"#);
        let p = root.read(r#"{ "id": "c", "type": "widget", "widget": "w" }"#);
        let target = root.target();
        let file = root.dir.join(r"widgets\w\widget.json");
        assert_eq!(stamp_of(&p, &file), Some(&stamp(&file)));
        // a save of the widget that the read didn't see: the stamp it took before reading tells
        root.widget("w", r#""w": 99,"#, r#"{ "id": "r", "type": "shape" }"#);
        let next = fingerprint(&target, &p);
        assert_ne!(next, p.watch);
        // (stamping only after the read, as the playlist used to be, would have taken this as seen)
        assert_eq!(fingerprint(&target, &p), next);

        // the playlist itself is stamped before it's read too
        let before = stamp(&target);
        assert_eq!(stamp_of(&root.again(), &target), Some(&before));
    }

    #[test]
    fn fingerprint_only_stats() {
        let root = Root::new("stats");
        let objects: Vec<String> = (0..100).map(|i| format!(r#"{{ "id": "t{i}", "type": "text", "text": "x", "font": "arial.ttf" }}"#)).collect();
        root.widget("w", "", &objects.join(","));
        let copies: Vec<String> = (0..5).map(|i| format!(r#"{{ "id": "c{i}", "type": "widget", "widget": "w" }}"#)).collect();
        let p = root.read(&copies.join(","));
        assert_eq!(leaves(&p).len(), 500);
        let target = root.target();
        let widgets = root.dir.join("widgets");
        let paths: Vec<PathBuf> = p.watch.iter().map(|(p, _)| p.clone()).collect();
        let mut want = vec![target.clone(), widgets.clone(), widgets.join(r"w\fonts\arial.ttf"), widgets.join(r"w\widget.json"),
                            PathBuf::from(r"C:\Windows\Fonts\arial.ttf")];
        want.sort();
        assert_eq!(paths, want);
        let found = fingerprint(&target, &p);
        assert_eq!(found, p.watch);
        // it walks no leaves: the list was made by the read
        let bare = Playlist { scenes: Vec::new(), global: Vec::new(), ..p.clone() };
        assert_eq!(fingerprint(&target, &bare), found);
    }

    /// Inside a widget, a colour that isn't a string isn't a colour: it's the fallback, and the stroke or
    /// gradient it's in stays (docs/widgets.md, widget.json).
    #[test]
    fn colours_that_arent_strings() {
        let root = Root::new("not-strings");
        root.widget("w", r#""w": 10, "h": 10,"#,
                    r##"{ "id": "a", "type": "shape", "stroke": { "color": 5, "width": 2 },
                          "fill": { "type": "linear", "stops": [ { "at": 0, "color": null }, { "at": 1, "color": "#ABC" } ] } },
                        { "id": "b", "type": "shape", "fill": { "type": "solid", "color": false }, "stroke": { "color": null } },
                        { "id": "t", "type": "text", "text": "x", "color": [], "outline": { "color": {}, "width": 1 } },
                        { "id": "i", "type": "image", "src": "a.png", "tint": 7 }"##);
        let p = root.read(r#"{ "id": "c", "type": "widget", "widget": "w" }"#);
        let l = leaves(&p);
        assert_eq!(ids(&p), ["c/a", "c/b", "c/t", "c/i"]);
        let white = |at: f64| Stop { at, color: "#ffffff".into() };
        assert_eq!(shape(&l[0]).stroke, Some(Stroke { color: "#ffffff".into(), width: 2.0 }));
        assert_eq!(shape(&l[0]).fill, Some(Paint::Linear { angle: 0.0, stops: vec![white(0.0), Stop { at: 1.0, color: "#aabbcc".into() }] }));
        assert_eq!(shape(&l[1]).fill, Some(Paint::Solid { color: "#ffffff".into() }));
        assert_eq!(shape(&l[1]).stroke, Some(Stroke { color: "#ffffff".into(), width: 1.0 }));
        assert_eq!((text(&l[2]).color.as_str(), &text(&l[2]).outline), ("#ffffff", &Outline { color: "#000000".into(), width: 1.0 }));
        assert_eq!(media(&l[3]).tint, None);

        // a scene's own objects are read as they always were
        let p = root.read(r#"{ "id": "s", "type": "shape", "stroke": { "color": 5, "width": 2 } }"#);
        assert_eq!(shape(&leaves(&p)[0]).stroke, None);
    }

    #[test]
    fn scene_tint_is_read() {
        let root = Root::new("tint");
        let p = root.read(r##"{ "id": "a", "type": "image", "src": "a.png", "tint": "#ABC", "keep_aspect": true },
                              { "id": "b", "type": "video", "src": "b.mp4", "tint": "aé123" },
                              { "id": "c", "type": "image", "src": "c.png", "tint": null, "keep_aspect": "yes" }"##);
        let read: Vec<(Option<&str>, bool)> = leaves(&p).iter().map(|o| (media(o).tint.as_deref(), media(o).keep_aspect)).collect();
        assert_eq!(read, [(Some("#aabbcc"), true), (None, false), (None, false)]);
        let json = serde_json::to_string(&leaves(&p)[0].kind).unwrap();
        assert!(json.ends_with(r##""fill":"none","keep_aspect":true,"tint":"#aabbcc"}"##), "{json}");
    }

    /// Whatever a hand-edited or downloaded widget.json holds, reading it never panics, and stays bounded.
    #[test]
    fn hostile_widgets_dont_panic() {
        let root = Root::new("hostile");
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        let bodies = [
            deep.as_str(),
            "",
            "\u{feff}{}",
            "null",
            r#"{ "version": "2", "w": "wide", "h": [], "background": {}, "inputs": {}, "layers": {} }"#,
            r#"{ "version": 1e308, "w": 1e308 }"#,
            r#"{ "w": -1e308, "h": 1e308, "background": "aé123", "inputs": [ { "id": "a", "type": "color", "default": "é" },
                 { "id": "b", "type": "image", "default": "\\\\host\\x.png" }, { "id": "t", "type": "text", "default": "{$t}" } ],
               "layers": [ 5, "x", null, { "opacity": "x", "visible": 3, "objects": [ 1, null, "x", [],
                 { "id": "big", "type": "image", "src": "a.png", "x": 1e308, "y": -1e308, "w": 1e308, "h": 1e308 },
                 { "id": "s", "type": "shape", "x": -1e308, "w": 1e308, "radius": 1e308, "stroke": { "width": 1e308, "color": "{$a}" },
                   "fill": { "type": "linear", "stops": [ { "color": "{$a}" }, 5, { "at": "x" } ] } },
                 { "id": "t", "type": "text", "text": "{$t}{$", "size": 1e308, "x": 1e308, "color": "{$b}", "outline": "x", "font": "é:.ttf" },
                 { "id": "v", "type": "video", "src": "{$b}", "tint": "{$a}" },
                 { "id": "w", "type": "widget", "widget": "evil", "scale": 1e308, "x": -1e308, "inputs": { "a": 5, "t": ["x"] } },
                 { "id": "w2", "type": "widget", "widget": "evil", "scale": -1e-308, "w": 1e308 } ] } ] }"#,
        ];
        for (i, body) in bodies.iter().enumerate() {
            root.write("widgets/evil/widget.json", body);
            let p = root.read(r##"{ "id": "c", "type": "widget", "widget": "evil", "x": 1e308, "scale": 1e308, "inputs": { "a": "#fff" } },
                                  { "id": "d", "type": "widget", "widget": "evil", "x": -9007199254740993, "y": 1e-308, "inputs": [] }"##);
            assert!(leaves(&p).len() <= widget::BUDGET, "body {i}");
        }
        // a wide tree of copies, 60 × 60 × 60: the budget caps the work
        for (outer, inner) in [("fan1", "fan2"), ("fan2", "fan3"), ("fan3", "leaf")] {
            let many: Vec<String> = (0..60).map(|i| format!(r#"{{ "id": "w{i}", "type": "widget", "widget": "{inner}" }}"#)).collect();
            root.widget(outer, r#""w": 4, "h": 4,"#, &format!(r#"{{ "id": "r", "type": "shape" }}, {}"#, many.join(",")));
        }
        root.widget("leaf", r#""w": 4, "h": 4,"#, r#"{ "id": "r", "type": "shape" }"#);
        let started = std::time::Instant::now();
        let p = root.read(r#"{ "id": "top", "type": "widget", "widget": "fan1" }, { "id": "after", "type": "shape" }"#);
        assert!(leaves(&p).len() > 1000 && leaves(&p).len() < widget::BUDGET);
        assert_eq!(leaves(&p).last().unwrap().id, "after");
        assert!(started.elapsed() < Duration::from_secs(10));

        // what costs no step is passed over once, not again in every copy: 20,000 hidden shapes and 20,000
        // hidden layers under 3,999 copies
        let hidden = vec![r#"{ "type": "shape", "visible": false }"#; 20_000].join(",");
        let layers = vec![r#"{ "visible": false, "objects": [ { "type": "shape" } ] }"#; 20_000].join(",");
        root.write("widgets/free/widget.json", &format!(r#"{{ "w": 4, "h": 4, "layers": [ {{ "objects": [{hidden}] }}, {layers} ] }}"#));
        let copies: Vec<String> = (0..3999).map(|i| format!(r#"{{ "id": "c{i}", "type": "widget", "widget": "free" }}"#)).collect();
        let started = std::time::Instant::now();
        let p = root.read(&copies.join(","));
        assert!(leaves(&p).is_empty());
        assert!(started.elapsed() < Duration::from_secs(5), "read in {:?}", started.elapsed());
    }

    /// However values and ids grow through nested copies, each value stays within 2000 characters, each
    /// text inside a copy within 4000, and each part of an id within 64 (docs/widgets.md, Limits).
    #[test]
    fn expansion_is_bounded() {
        let root = Root::new("bounded");
        // each widget shows its value 500 times over and passes it down 500 times over, three deep: whole,
        // the deepest text would be 2000 × 500³ characters
        let refs = "{$t}".repeat(500);
        let inputs = json!([{ "id": "t", "type": "text" }]);
        for (id, inner) in [("n1", Some("n2")), ("n2", Some("n3")), ("n3", None)] {
            let mut objects = vec![json!({ "id": "t", "type": "text", "text": refs })];
            if let Some(inner) = inner {
                objects.push(json!({ "id": "w", "type": "widget", "widget": inner, "inputs": { "t": refs } }));
            }
            root.write(&format!("widgets/{id}/widget.json"), &json!({ "inputs": inputs, "layers": [{ "objects": objects }] }).to_string());
        }
        let started = std::time::Instant::now();
        let p = root.read(&json!({ "id": "c", "type": "widget", "widget": "n1", "inputs": { "t": "x".repeat(2000) } }).to_string());
        assert_eq!(ids(&p), ["c/t", "c/w/t", "c/w/w/t"]);
        for o in leaves(&p) {
            assert_eq!((text(o).text.as_str(), text(o).id.as_str()), ("x".repeat(4000).as_str(), o.id.as_str()));
        }
        assert!(started.elapsed() < Duration::from_secs(2), "read in {:?}", started.elapsed());

        // ids of 100,000 characters, nested, each cut to 64 before it builds a path or a group
        let huge = |c: char| c.to_string().repeat(100_000);
        let unit = json!({ "type": "shape", "w": 1, "h": 1 });
        let mut inner = unit.clone();
        inner["id"] = json!(huge('p'));
        root.write("widgets/i2/widget.json", &json!({ "layers": [{ "id": huge('M'), "objects": [inner] }] }).to_string());
        let mut part = unit.clone();
        part["id"] = json!(huge('o'));
        let copy = json!({ "id": huge('w'), "type": "widget", "widget": "i2", "opacity": 0.5 });
        root.write("widgets/i1/widget.json", &json!({ "layers": [{ "id": huge('L'), "opacity": 0.5, "objects": [part, copy] }] }).to_string());
        let p = root.read(&json!({ "id": huge('c'), "type": "widget", "widget": "i1", "opacity": 0.5 }).to_string());
        let n = |c: char| c.to_string().repeat(64);
        assert_eq!(ids(&p), [format!("{}/{}", n('c'), n('o')), format!("{}/{}/{}", n('c'), n('w'), n('p'))]);
        let groups: Vec<&str> = leaves(&p)[1].groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(groups, [n('c'), format!("{}/{}", n('c'), n('L')), format!("{}/{}", n('c'), n('w'))]);
    }

    /// The leaf form of player/tests/widgets/README.md.
    fn leaf_form(o: &Object, case: &Path) -> Value {
        let rel = |p: &Path| match p.strip_prefix(case) {
            Ok(r) => r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"),
            Err(_) => p.display().to_string(),
        };
        let groups: Vec<Value> = o.groups.iter().map(|g| json!([g.id, g.opacity, g.rect])).collect();
        let mut leaf = json!({ "id": o.id, "opacity": o.opacity, "clip": o.clip, "groups": groups });
        let more = match &o.kind {
            Kind::Media(m) => json!({ "type": "image", "x": o.x, "y": o.y, "w": o.w, "h": o.h, "src": rel(&m.src),
                                      "keep_aspect": m.keep_aspect, "tint": m.tint }),
            Kind::Text(t) => {
                assert_eq!((t.x, t.y), (o.x as f32, o.y as f32), "{}: the text's anchor is its object's", o.id);
                let font = t.font.file_name().map(|f| f.to_string_lossy().into_owned());
                json!({ "type": "text", "x": o.x, "y": o.y, "text": t.text, "live": t.live, "size": t.size,
                        "outline_width": t.outline.width, "color": t.color, "outline_color": t.outline.color, "font": font })
            }
            Kind::Shape(s) => {
                let color = match &s.fill {
                    Some(Paint::Solid { color }) => Some(color.clone()),
                    _ => None,
                };
                json!({ "type": "shape", "x": o.x, "y": o.y, "w": o.w, "h": o.h, "shape": s.shape, "radius": s.radius, "color": color,
                        "stroke_width": s.stroke.as_ref().map(|s| s.width), "stroke_color": s.stroke.as_ref().map(|s| s.color.clone()),
                        "crisp": s.crisp })
            }
        };
        leaf.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
        leaf
    }

    /// What's drawn of a holder's layers (decision 2 of the build: hidden layers, layers at opacity 0 and
    /// hidden objects at the top level aren't listed).
    fn drawn(layers: &[Layer], case: &Path) -> Vec<Value> {
        layers.iter().filter(|l| l.visible && l.opacity > 0.0).flat_map(|l| &l.objects).filter(|o| o.visible).map(|o| leaf_form(o, case)).collect()
    }

    /// Numbers as numbers, opacities to 6 decimals.
    fn same(key: &str, a: &Value, b: &Value) -> bool {
        let six = |v: f64| (v * 1e6).round();
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => {
                let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
                if key == "opacity" { six(x) == six(y) } else { x == y }
            }
            (Value::Array(x), Value::Array(y)) if key == "groups" => {
                x.len() == y.len()
                    && x.iter().zip(y).all(|(g, h)| g[0] == h[0] && same("opacity", &g[1], &h[1]) && same("rect", &g[2], &h[2]))
            }
            (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(key, x, y)),
            _ => a == b,
        }
    }

    /// Every `expand/<case>` gives exactly the leaves its `expected.json` lists (its `placement` is the
    /// live text work's).
    #[test]
    fn fixtures_match() {
        let cases = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"tests\widgets\expand");
        let mut problems = Vec::new();
        let mut checked = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&cases).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for case in entries {
            let Ok(file) = std::fs::read(case.join("expected.json")) else { continue };
            let name = case.file_name().unwrap().to_string_lossy().into_owned();
            let expected: Value = serde_json::from_slice(&file).unwrap();
            let p = read(&case.join(r"playlist\playlist.json"), 30.0, Path::new("ffmpeg")).unwrap();
            let mut got: Vec<(String, Vec<Value>)> = p.scenes.iter().map(|s| (s.id.clone(), drawn(&s.layers, &case))).collect();
            got.push(("global".into(), drawn(&p.global, &case)));
            let mut want: Vec<(String, Vec<Value>)> = expected["scenes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| (s["id"].as_str().unwrap().to_string(), s["leaves"].as_array().unwrap().clone()))
                .collect();
            want.push(("global".into(), expected["global"]["leaves"].as_array().cloned().unwrap_or_default()));
            let scenes = |list: &[(String, Vec<Value>)]| list.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
            if scenes(&got) != scenes(&want) {
                problems.push(format!("{name}: scenes {:?}, expected {:?}", scenes(&got), scenes(&want)));
                continue;
            }
            for ((holder, mine), (_, theirs)) in got.iter().zip(&want) {
                let id_list = |leaves: &[Value]| leaves.iter().map(|l| l["id"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
                if id_list(mine) != id_list(theirs) {
                    problems.push(format!("{name}/{holder}: leaves {:?},\n    expected {:?}", id_list(mine), id_list(theirs)));
                    continue;
                }
                for (m, t) in mine.iter().zip(theirs) {
                    let (m, t) = (m.as_object().unwrap(), t.as_object().unwrap());
                    for key in m.keys().chain(t.keys().filter(|k| !m.contains_key(*k) && *k != "note")) {
                        let (a, b) = (m.get(key).unwrap_or(&Value::Null), t.get(key));
                        if b.is_none() || !same(key, a, b.unwrap()) {
                            problems.push(format!("{name}/{holder} {}: {key} is {a}, expected {}", m["id"], b.map_or("nothing".into(), Value::to_string)));
                        }
                    }
                }
            }
            checked.push(name);
        }
        assert!(problems.is_empty(), "{} differences:\n{}", problems.len(), problems.join("\n"));
        for case in ["basic", "inputs", "nested", "limits", "missing", "paths", "fills", "aspect"] {
            assert!(checked.iter().any(|c| c == case), "no expand/{case}");
        }
    }

    /// A scene without copies is the same JSON as before widgets (so the same cache keys, but for
    /// scene::VERSION): no clip, groups, keep_aspect or tint is written for it.
    #[test]
    fn no_widgets_same_json() {
        let json = br##"{ "version": 2, "global": { "layers": [ { "id": "g", "name": "Top", "opacity": 0.5, "objects": [
                { "id": "clock", "type": "text", "text": "{time}", "x": 10, "y": 20 } ] } ] },
            "scenes": [ { "id": "a", "name": "one", "seconds": 5, "background": "#102030", "layers": [
                { "id": "l1", "name": "Art", "objects": [
                    { "id": "i", "type": "image", "src": "sources/x.png", "x": 1.5, "y": -2, "w": 30, "h": 40, "rotate": 90, "flip": true,
                      "fill": "tile", "smooth": true, "opacity": 0.75, "locked": true, "name": "Pic" },
                    { "id": "v", "type": "video", "src": "sources/v.mp4" },
                    { "id": "s", "type": "shape", "x": 2, "y": 3, "w": 10, "h": 5, "radius": 4, "crisp": true,
                      "fill": { "type": "linear", "angle": 45, "stops": [ { "at": 0, "color": "#000000" }, { "at": 1, "color": "#ffffff" } ] },
                      "stroke": { "color": "#ff0000", "width": 2 } },
                    { "id": "n", "type": "shape", "shape": "line", "fill": null, "stroke": { "width": 3 } },
                    { "id": "t", "type": "text", "text": "a {{b}}", "font": "fonts/p.ttf", "size": 30, "color": "#abcdef", "x": 5, "y": 6,
                      "align": "center", "valign": "middle", "line_height": 1.5, "outline": { "color": "#00ff00", "width": 2 }, "crisp": true,
                      "opacity": 0.5 } ] },
                { "id": "l2", "name": "Hidden", "visible": false, "objects": [ { "id": "e", "type": "sticker" } ] } ] } ] }"##;
        let p = parse(json, Path::new(r"C:\pl"), 30.0, Path::new("ffmpeg")).unwrap();
        let scene = concat!(
            r##"{"id":"a","name":"one","seconds":5.0,"background":"#102030","layers":[{"id":"l1","name":"Art","visible":true,"opacity":1.0,"objects":["##,
            r##"{"id":"i","visible":true,"opacity":0.75,"x":1.5,"y":-2.0,"w":30.0,"h":40.0,"rotate":90,"flip":true,"##,
            r##""kind":{"src":"C:\\pl\\sources/x.png","smooth":true,"fill":"tile"}},"##,
            r##"{"id":"v","visible":true,"opacity":1.0,"x":0.0,"y":0.0,"w":0.0,"h":0.0,"rotate":0,"flip":false,"##,
            r##""kind":{"src":"C:\\pl\\sources/v.mp4","smooth":false,"fill":"none"}},"##,
            r##"{"id":"s","visible":true,"opacity":1.0,"x":2.0,"y":3.0,"w":10.0,"h":5.0,"rotate":0,"flip":false,"##,
            r##""kind":{"shape":"rect","radius":4.0,"fill":{"type":"linear","angle":45.0,"stops":[{"at":0.0,"color":"#000000"},"##,
            r##"{"at":1.0,"color":"#ffffff"}]},"stroke":{"color":"#ff0000","width":2.0},"crisp":true}},"##,
            r##"{"id":"n","visible":true,"opacity":1.0,"x":0.0,"y":0.0,"w":0.0,"h":0.0,"rotate":0,"flip":false,"##,
            r##""kind":{"shape":"line","radius":0.0,"fill":null,"stroke":{"color":"#000000","width":3.0},"crisp":false}},"##,
            r##"{"id":"t","visible":true,"opacity":0.5,"x":5.0,"y":6.0,"w":0.0,"h":0.0,"rotate":0,"flip":false,"##,
            r##""kind":{"id":"t","type":"text","text":"a {b}","font":"C:\\pl\\fonts/p.ttf","size":30.0,"color":"#abcdef","opacity":0.5,"##,
            r##""x":5.0,"y":6.0,"align":"center","valign":"middle","line_height":1.5,"outline":{"color":"#00ff00","width":2.0},"crisp":true}}]},"##,
            r##"{"id":"l2","name":"Hidden","visible":false,"opacity":1.0,"objects":[]}]}"##,
        );
        assert_eq!(serde_json::to_string(&p.scenes[0]).unwrap(), scene);
        let global = concat!(
            r##"[{"id":"g","name":"Top","visible":true,"opacity":0.5,"objects":["##,
            r##"{"id":"clock","visible":true,"opacity":1.0,"x":10.0,"y":20.0,"w":0.0,"h":0.0,"rotate":0,"flip":false,"##,
            r##""kind":{"id":"clock","type":"text","text":"{time}","font":"C:\\Windows\\Fonts\\segoeui.ttf","size":48.0,"color":"#ffffff","##,
            r##""opacity":1.0,"x":10.0,"y":20.0,"align":"left","valign":"top","line_height":1.2,"outline":{"color":"#000000","width":0.0},"##,
            r##""crisp":false}}]}]"##,
        );
        assert_eq!(serde_json::to_string(&p.global).unwrap(), global);
    }
}

/// Live text in drawing order (docs/widgets.md, Live text). Owned by work package P3.
#[cfg(test)]
mod live_tests {}
