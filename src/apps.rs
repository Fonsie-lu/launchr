//! What the launcher can launch — every desktop entry GIO lists plus the
//! AppImages from the config file — and how to launch it.
//!
//! [`Catalog::load`] runs on a worker thread while GTK starts, so nothing in
//! here may touch GTK: GIO only.

use crate::config::AppImageConfig;
use crate::matcher;
use crate::usage::Usage;
use gio_unix::DesktopAppInfo;
use gtk4::gio;
use gtk4::gio::prelude::*;
use std::ffi::OsString;
use std::ops::Deref;
use std::process::{Command, Stdio};

/// How much a hit in each searchable field counts, relative to the name.
mod weight {
    pub const NAME: f32 = 1.0;
    pub const GENERIC_NAME: f32 = 0.65;
    pub const EXECUTABLE: f32 = 0.6;
    pub const KEYWORDS: f32 = 0.45;
    pub const COMMENT: f32 = 0.35;
}

/// One searchable field of an entry and how much a hit in it counts.
pub struct Field {
    pub folded: matcher::Folded,
    pub weight: f32,
}

pub fn field(raw: &str, weight: f32) -> Field {
    Field { folded: matcher::Folded::new(raw), weight }
}

/// Everything the ranking needs about one launchable entry. Deliberately free
/// of GTK and GIO types so `rank` can be unit tested.
pub struct Entry {
    pub id: String,
    /// As displayed, with its original case and accents.
    pub name: String,
    /// The name is always the first field, at full weight.
    pub fields: Vec<Field>,
    pub count: u32,
    pub last_used: u64,
}

impl Entry {
    /// Folded name, which is what keeps the list in alphabetical order.
    pub fn sort_key(&self) -> &str {
        &self.fields[0].folded.text
    }
}

/// What activating an entry actually launches. Held in a vec parallel to
/// `Catalog::entries` so the ranking never has to touch a GIO type.
#[derive(Clone)]
pub enum Target {
    Desktop(DesktopApp),
    AppImage(AppImageConfig),
}

/// A desktop entry as GIO parsed it, built on the catalog worker and used on
/// the main thread.
#[derive(Clone)]
pub struct DesktopApp(DesktopAppInfo);

// SAFETY: GObjects are not `Send` only because the bindings cannot know how a
// given one is used. These cross threads exactly once, inside the catalog the
// worker returns: it keeps no reference of its own, so no object is ever used
// from two threads at once, and GObject reference counting is atomic for
// whatever references GIO holds internally. Scoped to this one wrapper so
// everything else in a `Catalog` is still checked by the compiler.
unsafe impl Send for DesktopApp {}

impl Deref for DesktopApp {
    type Target = DesktopAppInfo;

    fn deref(&self) -> &DesktopAppInfo {
        &self.0
    }
}

/// Every launchable entry, in folded-name order, with the usage store its
/// counts came from — which is also where the next launch gets recorded.
#[derive(Default)]
pub struct Catalog {
    pub entries: Vec<Entry>,
    pub targets: Vec<Target>,
    pub usage: Usage,
}

impl Catalog {
    /// Read the usage store and every desktop entry GIO lists, then add the
    /// configured AppImages. The desktop files are parsed exactly once, by GIO:
    /// the keys the `AppInfo` interface leaves out come from the same
    /// `GDesktopAppInfo` objects, not from a second pass over the files.
    pub fn load(appimages: Vec<AppImageConfig>) -> Self {
        let usage = Usage::load();
        let mut items: Vec<(Entry, Target)> = gio::AppInfo::all()
            .into_iter()
            .filter(|info| info.should_show())
            .filter_map(|info| desktop_item(&usage, info))
            .collect();
        items.extend(appimages.into_iter().map(|app| appimage_item(&usage, app)));

        // Compare the folded names in place. `sort_by_key` would have called
        // the key function once per comparison, not once per item, which made
        // this a few tens of thousands of throwaway allocations.
        items.sort_by(|(a, _), (b, _)| a.sort_key().cmp(b.sort_key()));
        let (entries, targets) = items.into_iter().unzip();
        Self { entries, targets, usage }
    }
}

fn desktop_item(usage: &Usage, info: gio::AppInfo) -> Option<(Entry, Target)> {
    // On Unix every entry `AppInfo::all` lists is a `GDesktopAppInfo`.
    let info = info.downcast::<DesktopAppInfo>().ok()?;
    let id = info.id()?.to_string();
    let name = info.name().to_string();

    let mut fields = vec![field(&name, weight::NAME)];
    let generic = with_untranslated(
        info.generic_name().map(|name| words(&name)).unwrap_or_default(),
        info.string("GenericName").map(|raw| words(&raw)),
    );
    if !generic.is_empty() {
        fields.push(field(&generic, weight::GENERIC_NAME));
    }
    if let Some(exec) = info.executable().file_name() {
        fields.push(field(&exec.to_string_lossy(), weight::EXECUTABLE));
    }
    let keywords = with_untranslated(
        info.keywords().iter().map(|word| words(word)).collect::<Vec<_>>().join(" "),
        info.string("Keywords").map(|raw| words(&raw)),
    );
    if !keywords.is_empty() {
        fields.push(field(&keywords, weight::KEYWORDS));
    }
    if let Some(comment) = info.description().filter(|s| !s.is_empty()) {
        fields.push(field(&comment, weight::COMMENT));
    }

    let (count, last_used) = usage.get(&id);
    Some((Entry { id, name, fields, count, last_used }, Target::Desktop(DesktopApp(info))))
}

/// `Keywords=`-style text as plain words: split on `;` and whitespace, joined
/// with single spaces.
fn words(text: &str) -> String {
    text.split(|c: char| c == ';' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The translated value of a key, followed by the untranslated one when that
/// says something else. `GenericName=` and `Keywords=` are mostly written in
/// English and a translation replaces them outright, so searching only one of
/// the two would lose either the user's own language or the English terms.
fn with_untranslated(translated: String, untranslated: Option<String>) -> String {
    match untranslated {
        Some(raw) if !raw.is_empty() && raw != translated => {
            if translated.is_empty() {
                raw
            } else {
                format!("{translated} {raw}")
            }
        }
        _ => translated,
    }
}

fn appimage_item(usage: &Usage, app: AppImageConfig) -> (Entry, Target) {
    let id = format!("appimage:{}", app.path.display());
    let name = app.name.clone();
    let mut fields = vec![field(&name, weight::NAME)];
    if let Some(stem) = app.path.file_stem().and_then(|s| s.to_str()) {
        fields.push(field(stem, weight::EXECUTABLE));
    }
    let (count, last_used) = usage.get(&id);
    (Entry { id, name, fields, count, last_used }, Target::AppImage(app))
}

/// The variables `main` overrides for launchr's own popup, each with the
/// value it had before, so an application launched from here gets the
/// original environment back and still follows the system GTK theme.
pub struct LaunchEnv(Vec<(&'static str, Option<OsString>)>);

impl LaunchEnv {
    /// Set each of `defaults` the environment does not already set, and
    /// remember what was there. Must run before any thread is spawned: glibc's
    /// `setenv` can reallocate `environ` under a thread reading it.
    pub fn pin(defaults: &[(&'static str, &str)]) -> Self {
        let saved = defaults
            .iter()
            .map(|&(name, value)| {
                let original = std::env::var_os(name);
                if original.is_none() {
                    std::env::set_var(name, value);
                }
                (name, original)
            })
            .collect();
        LaunchEnv(saved)
    }

    fn restore_on_context(&self, context: &gio::AppLaunchContext) {
        for (name, original) in &self.0 {
            match original {
                Some(value) => context.setenv(name, value),
                None => context.unsetenv(name),
            }
        }
    }

    fn restore_on_command(&self, command: &mut Command) {
        for (name, original) in &self.0 {
            match original {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
}

impl Target {
    /// Start the target with `env`'s original values restored. `context` is
    /// the display's launch context; only a desktop entry uses it. The error
    /// names what failed to start.
    pub fn launch(&self, env: &LaunchEnv, context: &gio::AppLaunchContext) -> Result<(), String> {
        match self {
            Target::Desktop(info) => {
                env.restore_on_context(context);
                info.launch(&[] as &[gio::File], Some(context)).map_err(|error| {
                    format!("{}: {error}", info.id().unwrap_or_default())
                })
            }
            Target::AppImage(app) => {
                let mut command = Command::new(&app.path);
                command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                env.restore_on_command(&mut command);
                command
                    .spawn()
                    .map(drop)
                    .map_err(|error| format!("{}: {error}", app.path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn keywords_are_plain_words() {
        assert_eq!(words("web;browser; internet;;"), "web browser internet");
        assert_eq!(words(""), "");
    }

    #[test]
    fn untranslated_text_is_kept_beside_a_translation() {
        let both = with_untranslated("navigateur web".into(), Some("web browser".into()));
        assert_eq!(both, "navigateur web web browser");
        // An English desktop, or a file without translations: said once.
        assert_eq!(with_untranslated("web browser".into(), Some("web browser".into())), "web browser");
        assert_eq!(with_untranslated(String::new(), Some("browser".into())), "browser");
        assert_eq!(with_untranslated("browser".into(), None), "browser");
    }

    #[test]
    fn a_launched_command_gets_the_original_environment_back() {
        // What `pin` records: GTK_THEME was unset before launchr set it,
        // GSK_RENDERER was set to something of the user's own.
        let env = LaunchEnv(vec![("GTK_THEME", None), ("GSK_RENDERER", Some("gl".into()))]);
        let mut command = Command::new("true");
        env.restore_on_command(&mut command);
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("GTK_THEME"), None)));
        assert!(envs.contains(&(OsStr::new("GSK_RENDERER"), Some(OsStr::new("gl")))));
    }
}
