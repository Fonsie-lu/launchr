//! `~/.config/launchr.json` — optional overrides for colors, sizing, and a
//! list of AppImages to launch directly (they have no `.desktop` file, so GIO
//! never finds them on its own). Everything in the file is optional; a
//! missing file, a missing field, or invalid JSON just falls back to the
//! built-in defaults rather than failing the launcher, and a single bad
//! field (an unparsable color, an AppImage that doesn't exist) is skipped
//! with a warning on stderr rather than rejecting the whole file.

use serde::Deserialize;
use std::path::PathBuf;

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

#[derive(Default)]
pub struct FileOverrides {
    pub colors: Colors,
    pub font_size: Option<u32>,
    pub lines: Option<usize>,
    pub dim: Option<f64>,
    pub blur: Option<u32>,
    pub appimages: Vec<AppImageConfig>,
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

pub fn load() -> FileOverrides {
    let Some(path) = config_path() else { return FileOverrides::default() };
    let text = match std::fs::read_to_string(&path) {
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
        font_size: file.font_size.map(|v| v.clamp(8, 60)),
        lines: file.lines.map(|v| v.clamp(1, 50)),
        dim: file.dim.map(|v| v.clamp(0.0, 1.0)),
        blur: file.blur.map(|v| v.min(200)),
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
    fn appimage_entry_needs_a_real_file() {
        let missing = FileAppImage {
            name: "Ghost".to_owned(),
            path: "/nonexistent/path/does-not-exist.AppImage".to_owned(),
            icon: None,
        };
        assert!(missing.into_config().is_none());

        let dir = std::env::temp_dir();
        let path = dir.join("launchr-config-test.AppImage");
        std::fs::write(&path, b"").unwrap();
        let real = FileAppImage { name: "Real".to_owned(), path: path.display().to_string(), icon: None };
        let config = real.into_config().expect("existing file should be accepted");
        assert_eq!(config.name, "Real");
        std::fs::remove_file(&path).ok();
    }
}
