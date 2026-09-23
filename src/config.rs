//! The launcher's settings, built in three layers: the built-in defaults
//! ([`Config::default`]), then `~/.config/launchr.json`
//! ([`Config::with_file_overrides`]), then the command line (`cli::parse_args`).
//!
//! The file holds optional overrides for colors, sizing, and a list of
//! AppImages to launch directly (they have no `.desktop` file, so GIO never
//! finds them on its own). Everything in it is optional; a missing file, a
//! missing field, or invalid JSON just falls back to the built-in defaults
//! rather than failing the launcher, and a single bad field (an unparsable
//! color, an AppImage that doesn't exist) is skipped with a warning on stderr
//! rather than rejecting the whole file.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct Config {
    pub lines: usize,
    pub width: i32,
    pub placeholder: String,
    pub query: String,
    /// Blur radius in screen pixels; 0 means no screenshot is taken at all.
    /// Off by default: the capture is a full compositor round trip plus a
    /// readback of the whole framebuffer, which is most of the launcher's
    /// startup on a slow machine.
    pub blur: u32,
    /// Opacity of the scrim painted over the blurred backdrop.
    pub dim: f64,
    /// Font size, in pixels, of a result row's name. The search entry scales
    /// with it (see `ui::render_style`) so the "large input, small results"
    /// proportion holds at any size.
    pub font_size: u32,
    pub colors: Colors,
    /// Directly launchable AppImages, from `~/.config/launchr.json` — they
    /// have no `.desktop` file, so GIO never surfaces them on its own.
    pub appimages: Vec<AppImageConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lines: 6,
            width: 680,
            placeholder: String::new(),
            query: String::new(),
            blur: 0,
            dim: 0.75,
            font_size: 21,
            colors: Colors::default(),
            appimages: Vec::new(),
        }
    }
}

impl Config {
    /// Applies `~/.config/launchr.json` on top of `self`. Called before
    /// `cli::parse_args`, so a CLI flag still wins over the file.
    pub fn with_file_overrides(self) -> Self {
        self.overridden_by(load())
    }

    fn overridden_by(mut self, file: FileOverrides) -> Self {
        self.colors = file.colors;
        self.appimages = file.appimages;
        if let Some(v) = file.font_size {
            self.font_size = v;
        }
        if let Some(v) = file.lines {
            self.lines = v;
        }
        if let Some(v) = file.dim {
            self.dim = v;
        }
        if let Some(v) = file.blur {
            self.blur = v;
        }
        self
    }
}

#[derive(Clone)]
pub struct Colors {
    pub background: String,
    pub panel: String,
    pub foreground: String,
    pub selection: String,
    pub accent: String,
    pub muted: String,
}

impl Default for Colors {
    /// Tokyo Night, matching the values `style.css` shipped with before this
    /// became configurable.
    fn default() -> Self {
        Self {
            background: "#10111e".to_owned(),
            panel: "#1a1b26".to_owned(),
            foreground: "#c0caf5".to_owned(),
            selection: "#283457".to_owned(),
            accent: "#7aa2f7".to_owned(),
            muted: "#565f89".to_owned(),
        }
    }
}

#[derive(Clone)]
pub struct AppImageConfig {
    pub name: String,
    pub path: PathBuf,
    pub icon: Option<String>,
}

/// What the file sets, already validated and clamped. `colors` is complete:
/// a field the file leaves out or gets wrong holds its default.
#[derive(Default)]
struct FileOverrides {
    colors: Colors,
    font_size: Option<u32>,
    lines: Option<usize>,
    dim: Option<f64>,
    blur: Option<u32>,
    appimages: Vec<AppImageConfig>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileColors {
    background: Option<String>,
    panel: Option<String>,
    foreground: Option<String>,
    selection: Option<String>,
    accent: Option<String>,
    muted: Option<String>,
}

#[derive(Deserialize)]
struct FileAppImage {
    name: String,
    path: String,
    icon: Option<String>,
}

impl FileAppImage {
    fn into_config(self) -> Option<AppImageConfig> {
        let path = expand_tilde(&self.path);
        if self.name.trim().is_empty() {
            eprintln!("launchr: ignoring appimages entry with an empty name ({path:?})");
            return None;
        }
        if !path.is_file() {
            eprintln!("launchr: ignoring appimages entry {path:?}: no such file");
            return None;
        }
        Some(AppImageConfig { name: self.name, path, icon: self.icon })
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    colors: FileColors,
    font_size: Option<u32>,
    lines: Option<usize>,
    dim: Option<f64>,
    /// Same convention as `-b`/`--blur`: 0 or absent means off, otherwise the
    /// blur radius in pixels.
    blur: Option<u32>,
    appimages: Vec<FileAppImage>,
}

/// Accepted ranges, shared with `cli::parse_args` so a value set in the file
/// and the same value set by a flag are clamped identically.
pub const FONT_SIZE: std::ops::RangeInclusive<u32> = 8..=60;
pub const LINES: std::ops::RangeInclusive<usize> = 1..=50;
pub const DIM: std::ops::RangeInclusive<f64> = 0.0..=1.0;
pub const MAX_BLUR: u32 = 200;
/// Flag-only, but kept beside the others so every accepted range is in one
/// place.
pub const WIDTH: std::ops::RangeInclusive<i32> = 240..=3000;

fn config_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let dir = xdg.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(dir.join("launchr.json"))
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/").zip(std::env::var_os("HOME")) {
        Some((rest, home)) => PathBuf::from(home).join(rest),
        None => PathBuf::from(path),
    }
}

fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// "r, g, b" for a hex color, for use inside an `rgba(...)` declaration.
/// Callers only ever pass colors that have already been through
/// [`valid_hex`] (or the built-in defaults), so the fallback here never
/// actually triggers.
pub fn rgb_triplet(hex: &str) -> String {
    let (r, g, b) = parse_hex(hex).unwrap_or((0, 0, 0));
    format!("{r}, {g}, {b}")
}

fn valid_hex(value: String, field: &str) -> Option<String> {
    if parse_hex(&value).is_some() {
        Some(value)
    } else {
        eprintln!("launchr: ignoring invalid color for {field}: {value:?}");
        None
    }
}

fn load() -> FileOverrides {
    match config_path() {
        Some(path) => load_from(&path),
        None => FileOverrides::default(),
    }
}

/// The body of [`load`], split out so the tests can point it at a file of
/// their own instead of the caller's home directory.
fn load_from(path: &Path) -> FileOverrides {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return FileOverrides::default(),
        Err(err) => {
            eprintln!("launchr: cannot read {}: {err}", path.display());
            return FileOverrides::default();
        }
    };
    let file: FileConfig = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("launchr: cannot parse {}: {err}", path.display());
            return FileOverrides::default();
        }
    };

    let mut colors = Colors::default();
    if let Some(v) = file.colors.background.and_then(|v| valid_hex(v, "background")) {
        colors.background = v;
    }
    if let Some(v) = file.colors.panel.and_then(|v| valid_hex(v, "panel")) {
        colors.panel = v;
    }
    if let Some(v) = file.colors.foreground.and_then(|v| valid_hex(v, "foreground")) {
        colors.foreground = v;
    }
    if let Some(v) = file.colors.selection.and_then(|v| valid_hex(v, "selection")) {
        colors.selection = v;
    }
    if let Some(v) = file.colors.accent.and_then(|v| valid_hex(v, "accent")) {
        colors.accent = v;
    }
    if let Some(v) = file.colors.muted.and_then(|v| valid_hex(v, "muted")) {
        colors.muted = v;
    }

    let appimages = file.appimages.into_iter().filter_map(FileAppImage::into_config).collect();

    FileOverrides {
        colors,
        font_size: file.font_size.map(|v| v.clamp(*FONT_SIZE.start(), *FONT_SIZE.end())),
        lines: file.lines.map(|v| v.clamp(*LINES.start(), *LINES.end())),
        dim: file.dim.map(|v| v.clamp(*DIM.start(), *DIM.end())),
        blur: file.blur.map(|v| v.min(MAX_BLUR)),
        appimages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_hex() {
        assert_eq!(parse_hex("#7aa2f7"), Some((0x7a, 0xa2, 0xf7)));
        assert_eq!(parse_hex("#FFFFFF"), Some((255, 255, 255)));
    }

    #[test]
    fn rejects_bad_hex() {
        assert_eq!(parse_hex("7aa2f7"), None); // missing #
        assert_eq!(parse_hex("#7aa2f"), None); // too short
        assert_eq!(parse_hex("#7aa2f77"), None); // too long
        assert_eq!(parse_hex("#zzzzzz"), None); // not hex digits
    }

    #[test]
    fn rgb_triplet_formats_and_falls_back() {
        assert_eq!(rgb_triplet("#7aa2f7"), "122, 162, 247");
        assert_eq!(rgb_triplet("nonsense"), "0, 0, 0");
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let overrides = load_from(Path::new("/nonexistent/launchr.json"));
        assert!(overrides.appimages.is_empty());
        assert_eq!(overrides.lines, None);
        assert_eq!(overrides.colors.accent, Colors::default().accent);
    }

    fn write_file(name: &str, extension: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("launchr-config-{name}-{}.{extension}", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn write_config(name: &str, body: &str) -> PathBuf {
        write_file(name, "json", body)
    }

    #[test]
    fn unparsable_json_falls_back_to_the_defaults() {
        let path = write_config("broken", "{ not json");
        let overrides = load_from(&path);
        // Removed before the assertions so a failure does not leak the file.
        std::fs::remove_file(&path).ok();
        assert_eq!(overrides.font_size, None);
        assert_eq!(overrides.colors.background, Colors::default().background);
    }

    #[test]
    fn file_values_are_read_and_clamped() {
        let path = write_config(
            "clamped",
            r##"{ "font_size": 400, "lines": 0, "dim": -1, "blur": 9000,
                  "colors": { "accent": "#ff0000", "muted": "not a color" } }"##,
        );
        let overrides = load_from(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(overrides.font_size, Some(60));
        assert_eq!(overrides.lines, Some(1));
        assert_eq!(overrides.dim, Some(0.0));
        assert_eq!(overrides.blur, Some(200));
        assert_eq!(overrides.colors.accent, "#ff0000");
        // One bad field is skipped; the rest of the file still applies.
        assert_eq!(overrides.colors.muted, Colors::default().muted);
    }

    #[test]
    fn the_file_layer_overrides_only_what_it_sets() {
        let file = FileOverrides { lines: Some(3), blur: Some(12), ..FileOverrides::default() };
        let config = Config::default().overridden_by(file);
        assert_eq!(config.lines, 3);
        assert_eq!(config.blur, 12);
        // Left out of the file, so the built-in default survives.
        assert_eq!(config.dim, Config::default().dim);
        assert_eq!(config.font_size, Config::default().font_size);
    }

    #[test]
    fn appimage_entry_needs_a_real_file() {
        let missing = FileAppImage {
            name: "Ghost".to_owned(),
            path: "/nonexistent/path/does-not-exist.AppImage".to_owned(),
            icon: None,
        };
        assert!(missing.into_config().is_none());

        let path = write_file("appimage", "AppImage", "");
        let real = FileAppImage { name: "Real".to_owned(), path: path.display().to_string(), icon: None };
        let config = real.into_config().expect("existing file should be accepted");
        std::fs::remove_file(&path).ok();
        assert_eq!(config.name, "Real");
    }
}
