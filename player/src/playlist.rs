//! What to play: one file, every image in a folder, or a playlist file.
//!
//! A playlist is JSON, with files relative to the playlist's own folder. Text drawn over the pieces
//! (docs/widgets.md) is `widgets` per item, plus `overlay`, drawn over every item:
//!
//! ```json
//! { "seconds": 30, "overlay": [ ... ], "items": [ { "file": "out/cat.png", "seconds": 20, "widgets": [ ... ] }, { "file": "out/rain.gif" } ] }
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::text::DEFAULT_FONT;

const IMAGE_TYPES: [&str; 7] = ["png", "gif", "webp", "jpg", "jpeg", "bmp", "apng"];

/// One piece. Its prepared frames depend on the file, the text over it and the art options.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub seconds: f64,
    /// This piece's own text.
    pub widgets: Vec<Widget>,
    /// The global layer, drawn over every piece (so the same in every item).
    pub overlay: Vec<Widget>,
}

impl Item {
    /// A piece with no text over it.
    fn new(path: PathBuf, seconds: f64) -> Item {
        Item { path, seconds, widgets: Vec::new(), overlay: Vec::new() }
    }

    /// Everything drawn over the piece, in drawing order: its own widgets, then the global layer.
    pub fn text(&self) -> impl Iterator<Item = &Widget> + Clone {
        self.widgets.iter().chain(&self.overlay)
    }
}

/// A piece of text over the art. Keys left out take the defaults in docs/widgets.md.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Widget {
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
}

impl Default for Widget {
    fn default() -> Widget {
        Widget {
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

#[derive(Deserialize)]
struct File {
    seconds: Option<f64>,
    overlay: Option<Vec<serde_json::Value>>,
    items: Vec<FileItem>,
}

#[derive(Deserialize)]
struct FileItem {
    file: PathBuf,
    seconds: Option<f64>,
    widgets: Option<Vec<serde_json::Value>>,
}

pub fn read(target: &Path, default_seconds: f64) -> Result<Vec<Item>, Box<dyn Error>> {
    let items = if target.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(target)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && is_image(p))
            .collect();
        paths.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        paths.into_iter().map(|path| Item::new(path, default_seconds)).collect()
    } else if target.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        parse(&std::fs::read(target)?, target.parent().unwrap_or(Path::new(".")), default_seconds)?
    } else {
        vec![Item::new(target.to_path_buf(), default_seconds)]
    };
    if items.is_empty() {
        return Err(format!("nothing to play in {}", target.display()).into());
    }
    Ok(items)
}

/// A playlist file's items, with paths relative to `base`.
fn parse(json: &[u8], base: &Path, default_seconds: f64) -> Result<Vec<Item>, Box<dyn Error>> {
    let file: File = serde_json::from_slice(json)?;
    let fallback = file.seconds.unwrap_or(default_seconds);
    let overlay = widgets(file.overlay, base);
    Ok(file.items
        .into_iter()
        .map(|i| Item {
            path: base.join(i.file),
            seconds: i.seconds.unwrap_or(fallback).max(1.0),
            widgets: widgets(i.widgets, base),
            overlay: overlay.clone(),
        })
        .collect())
}

/// The text widgets, which are all this player can draw. Anything else is left out, rather than
/// stopping the whole playlist from playing.
fn widgets(list: Option<Vec<serde_json::Value>>, base: &Path) -> Vec<Widget> {
    list.unwrap_or_default()
        .into_iter()
        .filter(|value| value.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(widget)
        .map(|mut w| {
            // a relative font is next to the playlist, like the pieces; no font at all means the default
            w.font = if w.font.as_os_str().is_empty() { PathBuf::from(DEFAULT_FONT) } else { base.join(&w.font) };
            w
        })
        .collect()
}

/// A widget, with any value of the wrong kind (a size of "big", an align of "centre") left at its
/// default, as the studio's preview does.
fn widget(value: serde_json::Value) -> Option<Widget> {
    let serde_json::Value::Object(mut keys) = value else { return None };
    keys.retain(|key, value| {
        let alone = serde_json::Value::Object([(key.clone(), value.clone())].into_iter().collect());
        let ok = serde_json::from_value::<Widget>(alone).is_ok();
        if !ok {
            crate::player::log(format_args!("ignoring a widget's {key}: {value} doesn't make sense there"));
        }
        ok
    });
    serde_json::from_value(serde_json::Value::Object(keys)).ok()
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| IMAGE_TYPES.iter().chain(&crate::art::VIDEO_TYPES).any(|t| e.eq_ignore_ascii_case(t)))
}

/// Changes whenever the playlist, the folder's contents, any listed file or any font the text uses
/// changes.
pub fn fingerprint(target: &Path, items: &[Item]) -> Vec<(PathBuf, Option<(SystemTime, u64)>)> {
    let mut paths = vec![target.to_path_buf()];
    if target.is_dir() {
        if let Ok(entries) = std::fs::read_dir(target) {
            paths.extend(entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| is_image(p)));
        }
    }
    paths.extend(items.iter().map(|i| i.path.clone()));
    paths.extend(items.iter().flat_map(|i| i.text()).map(|w| w.font.clone()));
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
pub fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_defaults() {
        let w: Widget = serde_json::from_str(r#"{ "id": "w1", "type": "text" }"#).unwrap();
        assert_eq!(w, Widget { id: "w1".into(), kind: "text".into(), ..Widget::default() });
        assert_eq!(w.font, PathBuf::from(r"C:\Windows\Fonts\segoeui.ttf"));
        assert_eq!((w.size, w.opacity, w.x, w.y, w.line_height), (48.0, 1.0, 0.0, 0.0, 1.2));
        assert_eq!((w.color.as_str(), w.align, w.valign, w.crisp), ("#ffffff", Align::Left, Valign::Top, false));
        assert_eq!(w.outline, Outline { color: "#000000".into(), width: 0.0 });
        // half an outline keeps the other half's default
        let w: Widget = serde_json::from_str(r#"{ "type": "text", "outline": { "width": 3 } }"#).unwrap();
        assert_eq!(w.outline, Outline { color: "#000000".into(), width: 3.0 });
    }

    #[test]
    fn widget_values() {
        let w: Widget = serde_json::from_str(
            r##"{ "id": "a", "type": "text", "text": "Hi\nthere", "size": 20, "color": "#ff0000", "opacity": 0.5,
                 "x": 10, "y": 20.5, "align": "right", "valign": "middle", "line_height": 1.5,
                 "outline": { "color": "#00ff00", "width": 2 }, "crisp": true, "future": "ignored" }"##,
        )
        .unwrap();
        assert_eq!((w.text.as_str(), w.size, w.color.as_str(), w.opacity), ("Hi\nthere", 20.0, "#ff0000", 0.5));
        assert_eq!((w.x, w.y, w.align, w.valign, w.line_height, w.crisp), (10.0, 20.5, Align::Right, Valign::Middle, 1.5, true));
        assert_eq!(w.outline, Outline { color: "#00ff00".into(), width: 2.0 });
    }

    #[test]
    fn playlist_text() {
        let json = br#"{
            "seconds": 10,
            "overlay": [ { "id": "clock", "type": "text", "text": "12:00" } ],
            "items": [
                { "file": "a.png", "widgets": [
                    { "id": "w1", "type": "text", "font": "fonts/pixel.ttf" },
                    { "id": "w2", "type": "image" },
                    { "id": "w3", "type": "text", "size": "big", "align": "centre", "text": "kept" },
                    "not a widget",
                    { "id": "w4", "type": "text", "font": "" },
                    { "id": "w5", "type": "text", "font": "D:\\Fonts\\x.otf" }
                ] },
                { "file": "b.gif", "seconds": 3, "name": "kept", "layout": {} }
            ]
        }"#;
        let base = Path::new(r"C:\pl");
        let items = parse(json, base, 30.0).unwrap();
        assert_eq!(items.len(), 2);
        // unknown types and things that aren't widgets are left out; values that make no sense are defaults
        let ids: Vec<&str> = items[0].widgets.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["w1", "w3", "w4", "w5"]);
        let w3 = &items[0].widgets[1];
        assert_eq!((w3.size, w3.align, w3.text.as_str()), (48.0, Align::Left, "kept"));
        assert_eq!(items[0].widgets[0].font, base.join("fonts/pixel.ttf"));
        assert_eq!(items[0].widgets[2].font, PathBuf::from(DEFAULT_FONT));
        assert_eq!(items[0].widgets[3].font, PathBuf::from(r"D:\Fonts\x.otf"));
        assert!(items[1].widgets.is_empty());
        for item in &items {
            assert_eq!(item.overlay.len(), 1);
            assert_eq!(item.overlay[0].text, "12:00");
        }
        let order: Vec<&str> = items[0].text().map(|w| w.id.as_str()).collect();
        assert_eq!(order, ["w1", "w3", "w4", "w5", "clock"]);
        assert_eq!((items[0].seconds, items[1].seconds), (10.0, 3.0));
    }

    #[test]
    fn playlist_without_text() {
        let items = parse(br#"{ "items": [ { "file": "a.png" } ] }"#, Path::new("."), 30.0).unwrap();
        assert!(items[0].widgets.is_empty() && items[0].overlay.is_empty());
        let plain = items.clone();
        let items = parse(br#"{ "overlay": null, "items": [ { "file": "a.png", "widgets": null } ] }"#, Path::new("."), 30.0).unwrap();
        assert_eq!(items, plain);
        // what the studio writes for a piece without text
        let items = parse(br#"{ "overlay": [], "items": [ { "file": "a.png", "widgets": [] } ] }"#, Path::new("."), 30.0).unwrap();
        assert_eq!(items, plain);
        assert_eq!(crate::text::key(items[0].text()), None);
    }
}
