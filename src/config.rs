//! The launcher's settings, built in three layers: the built-in defaults
//! ([`Config::default`]), then `~/.config/launchr.json`
//! ([`Config::with_file_overrides`]), then the command line (`cli::parse_args`).
//!
//! The file holds optional overrides for colors, sizing, and a list of
//! AppImages to launch directly (they have no `.desktop` file, so GIO never
//! finds them on its own). Everything in it is optional; a missing file, a
//! missing field, or invalid JSON just falls back to the built-in defaults
//! rather than failing the launcher, and a single bad field (an unparsable
//! color, a number given as a string, an AppImage that doesn't exist) is
//! skipped with a warning on stderr rather than rejecting the whole file. So
//! is a key nothing reads, which is nearly always a typo.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
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
    pub icon: Option<Icon>,
}

/// An AppImage's icon: a name looked up in the icon theme, or an image file.
#[derive(Clone, Debug, PartialEq)]
pub enum Icon {
    Name(String),
    File(PathBuf),
}

impl Icon {
    /// A theme icon name never contains a slash, so anything that does is a
    /// path, with a leading `~/` expanded.
    fn parse(value: String) -> Self {
        if value.contains('/') { Icon::File(expand_tilde(&value)) } else { Icon::Name(value) }
    }
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

/// One JSON object of the file, read a key at a time. Each `take` removes
/// what it reads, so whatever is left at `finish` is a key nothing knows.
struct Fields {
    map: Map<String, Value>,
    /// Where the object sits in the file, for the warnings: empty at the top
    /// level, else `colors.` or `appimages[2].`.
    prefix: String,
}

impl Fields {
    fn new(value: Value, name: String) -> Option<Self> {
        let map = serde_json::from_value(value)
            .inspect_err(|err| eprintln!("launchr: config: ignoring {name}: {err}"))
            .ok()?;
        Some(Fields { map, prefix: format!("{name}.") })
    }

    /// The value at `key`, or `None` — with a warning when there was a value
    /// but not one of the right type. `null` counts as absent.
    fn take<T: DeserializeOwned>(&mut self, key: &str) -> Option<T> {
        let value = self.map.remove(key).filter(|value| !value.is_null())?;
        serde_json::from_value(value)
            .inspect_err(|err| eprintln!("launchr: config: ignoring {}{key}: {err}", self.prefix))
            .ok()
    }

    fn nested(&mut self, key: &str) -> Option<Fields> {
        let value = self.map.remove(key).filter(|value| !value.is_null())?;
        Fields::new(value, format!("{}{key}", self.prefix))
    }

    fn finish(self) {
        for key in self.map.keys() {
            eprintln!("launchr: config: ignoring unknown setting {}{key}", self.prefix);
        }
    }
}

fn read_colors(fields: Option<Fields>) -> Colors {
    let mut colors = Colors::default();
    let Some(mut fields) = fields else { return colors };
    let slots = [
        ("background", &mut colors.background),
        ("panel", &mut colors.panel),
        ("foreground", &mut colors.foreground),
        ("selection", &mut colors.selection),
        ("accent", &mut colors.accent),
        ("muted", &mut colors.muted),
    ];
    for (key, slot) in slots {
        let Some(value) = fields.take::<String>(key) else { continue };
        if parse_hex(&value).is_some() {
            *slot = value;
        } else {
            eprintln!("launchr: config: ignoring colors.{key}: {value:?} is not a #rrggbb color");
        }
    }
    fields.finish();
    colors
}

fn read_appimages(list: Option<Vec<Value>>) -> Vec<AppImageConfig> {
    let entries = list.unwrap_or_default().into_iter().enumerate();
    entries
        .filter_map(|(i, value)| read_appimage(Fields::new(value, format!("appimages[{i}]"))?))
        .collect()
}

fn read_appimage(mut fields: Fields) -> Option<AppImageConfig> {
    let name = fields.take::<String>("name");
    let path = fields.take::<String>("path");
    let icon = fields.take::<String>("icon").map(Icon::parse);
    let entry = fields.prefix.trim_end_matches('.').to_owned();
    fields.finish();

    let (Some(name), Some(path)) = (name, path) else {
        eprintln!("launchr: config: ignoring {entry}: it needs both a name and a path");
        return None;
    };
    let path = expand_tilde(&path);
    let problem = match std::fs::metadata(&path) {
        _ if name.trim().is_empty() => Some("its name is empty"),
        Ok(meta) if !meta.is_file() => Some("not a file"),
        // Caught here rather than when it fails to start, which would close
        // the launcher on a launch that never happened. Asked of the kernel
        // rather than read off the mode bits: an execute bit for someone
        // else's user or group is no use to this one.
        Ok(_) if rustix::fs::access(&path, rustix::fs::Access::EXEC_OK).is_err() => {
            Some("not executable (chmod +x)")
        }
        Ok(_) => None,
        Err(_) => Some("no such file"),
    };
    if let Some(problem) = problem {
        eprintln!("launchr: config: ignoring {entry} {path:?}: {problem}");
        return None;
    }
    Some(AppImageConfig { name, path, icon })
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
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let dir =
        xdg.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
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
/// [`parse_hex`] (or the built-in defaults), so the fallback here never
/// actually triggers.
pub fn rgb_triplet(hex: &str) -> String {
    let (r, g, b) = parse_hex(hex).unwrap_or((0, 0, 0));
    format!("{r}, {g}, {b}")
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
    let map = match serde_json::from_str::<Map<String, Value>>(&text) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("launchr: cannot parse {}: {err}", path.display());
            return FileOverrides::default();
        }
    };

    let mut file = Fields { map, prefix: String::new() };
    let overrides = FileOverrides {
        colors: read_colors(file.nested("colors")),
        font_size: file
            .take::<u32>("font_size")
            .map(|v| v.clamp(*FONT_SIZE.start(), *FONT_SIZE.end())),
        lines: file.take::<usize>("lines").map(|v| v.clamp(*LINES.start(), *LINES.end())),
        dim: file.take::<f64>("dim").map(|v| v.clamp(*DIM.start(), *DIM.end())),
        blur: file.take::<u32>("blur").map(|v| v.min(MAX_BLUR)),
        appimages: read_appimages(file.take("appimages")),
    };
    file.finish();
    overrides
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
    fn a_mistyped_field_spoils_only_itself() {
        let path = write_config(
            "mistyped",
            r##"{ "lines": "6", "dim": 0.5, "blur": true, "font-size": 30,
                  "colors": { "accent": 5, "panel": "#000000", "accnet": "#ffffff" },
                  "appimages": [ { "name": "No Path" }, 3 ] }"##,
        );
        let overrides = load_from(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(overrides.dim, Some(0.5));
        assert_eq!(overrides.colors.panel, "#000000");
        assert_eq!(overrides.lines, None);
        assert_eq!(overrides.blur, None);
        assert_eq!(overrides.font_size, None);
        assert_eq!(overrides.colors.accent, Colors::default().accent);
        assert!(overrides.appimages.is_empty());
    }

    fn appimage_entry(path: &Path, icon: &str) -> Fields {
        let entry = serde_json::json!({ "name": "Real", "path": path, "icon": icon });
        Fields::new(entry, "appimages[0]".to_owned()).unwrap()
    }

    #[test]
    fn appimage_entry_needs_a_real_executable_file() {
        let missing = Path::new("/nonexistent/path/does-not-exist.AppImage");
        assert!(read_appimage(appimage_entry(missing, "app")).is_none());

        let path = write_file("appimage", "AppImage", "");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let unrunnable = read_appimage(appimage_entry(&path, "app"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runnable = read_appimage(appimage_entry(&path, "app"));
        std::fs::remove_file(&path).ok();

        assert!(unrunnable.is_none(), "a file without an execute bit cannot be launched");
        let config = runnable.expect("an existing executable should be accepted");
        assert_eq!(config.name, "Real");
        assert_eq!(config.icon, Some(Icon::Name("app".to_owned())));
    }

    #[test]
    fn an_icon_with_a_slash_is_a_file() {
        assert_eq!(Icon::parse("firefox".into()), Icon::Name("firefox".into()));
        assert_eq!(Icon::parse("/opt/app.png".into()), Icon::File("/opt/app.png".into()));
    }
}
