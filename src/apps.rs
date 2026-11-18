//! What the launcher can launch — every desktop entry GIO lists plus the
//! AppImages from the config file — and how to launch it.
//!
//! [`Catalog::load`] runs on a worker thread while GTK starts, so nothing in
//! here may touch GTK: GIO only.

use crate::config::AppImageConfig;
use crate::matcher;
use crate::usage::Usage;
use gio_unix::DesktopAppInfo;
use gio_unix::prelude::DesktopAppInfoExtManual;
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use std::ffi::OsString;
use std::fs::File;
use std::ops::Deref;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

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
    // `None` for a desktop file without an `Exec=` line, which GIO still
    // lists when it is D-Bus activatable.
    if let Some(exec) = info.executable().as_deref().and_then(Path::file_name) {
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
    /// remember what was there.
    ///
    /// # Safety
    ///
    /// No other thread may exist yet: glibc's `setenv` can reallocate
    /// `environ` under a thread reading it.
    pub unsafe fn pin(defaults: &[(&'static str, &str)]) -> Self {
        let saved = defaults
            .iter()
            .map(|&(name, value)| {
                let original = std::env::var_os(name);
                if original.is_none() {
                    // SAFETY: the caller guarantees this thread is the only one.
                    unsafe { std::env::set_var(name, value) };
                }
                (name, original)
            })
            .collect();
        LaunchEnv(saved)
    }

    fn restore_on(&self, context: &gio::AppLaunchContext) {
        for (name, original) in &self.0 {
            match original {
                Some(value) => context.setenv(name, value),
                None => context.unsetenv(name),
            }
        }
    }
}

/// `path` as the `Exec=` line of a desktop entry that runs it with no
/// arguments. GIO expands field codes in that line first and then splits it
/// the way a shell would, so the path is shell quoted and any `%` in it
/// doubled — a space or a `%` in a file name must not change what is run.
fn exec_line(path: &Path) -> OsString {
    let quoted = glib::shell_quote(path);
    let mut line = Vec::with_capacity(quoted.len() + 2);
    for &byte in quoted.as_bytes() {
        line.push(byte);
        if byte == b'%' {
            line.push(b'%');
        }
    }
    OsString::from_vec(line)
}

impl Target {
    /// Start the target with `env`'s original values restored. `context` is
    /// the display's launch context. The error names what failed to start.
    ///
    /// An AppImage goes through GIO too, as a desktop entry made up on the
    /// spot, rather than through `std::process::Command`: that way it gets
    /// the same environment handling and, from the launch context, the
    /// activation token a compositor with focus stealing prevention wants to
    /// see before it hands the new window the keyboard.
    pub fn launch(&self, env: &LaunchEnv, context: &gio::AppLaunchContext) -> Result<(), String> {
        env.restore_on(context);
        match self {
            Target::Desktop(info) => info
                .launch(&[] as &[gio::File], Some(context))
                .map_err(|error| format!("{}: {error}", info.id().unwrap_or_default())),
            Target::AppImage(app) => launch_appimage(app, context)
                .map_err(|error| format!("{}: {error}", app.path.display())),
        }
    }
}

fn launch_appimage(app: &AppImageConfig, context: &gio::AppLaunchContext) -> Result<(), String> {
    let info = gio::AppInfo::create_from_commandline(
        exec_line(&app.path),
        Some(&app.name),
        gio::AppInfoCreateFlags::SUPPORTS_STARTUP_NOTIFICATION,
    )
    .map_err(|error| error.to_string())?
    // On Unix every `AppInfo` GIO makes is a `GDesktopAppInfo`.
    .downcast::<DesktopAppInfo>()
    .map_err(|_| "not a desktop app info".to_owned())?;
    // Not the launcher's own stdin/stdout/stderr: the AppImage outlives the
    // launcher, and must not hold open a pipe it was started with — a
    // script reading launchr's output would wait for the AppImage to exit.
    let null =
        File::options().read(true).write(true).open("/dev/null").map_err(|e| e.to_string())?;
    info.launch_uris_as_manager_with_fds(
        &[],
        Some(context),
        // What GIO's plain `launch` uses.
        glib::SpawnFlags::SEARCH_PATH,
        None,
        None,
        Some(&null),
        Some(&null),
        Some(&null),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            with_untranslated("web browser".into(), Some("web browser".into())),
            "web browser"
        );
        assert_eq!(with_untranslated(String::new(), Some("browser".into())), "browser");
        assert_eq!(with_untranslated("browser".into(), None), "browser");
    }

    #[test]
    fn a_launched_app_gets_the_original_environment_back() {
        // What `pin` records: GTK_THEME was unset before launchr set it,
        // GSK_RENDERER was set to something of the user's own.
        let env = LaunchEnv(vec![("GTK_THEME", None), ("GSK_RENDERER", Some("gl".into()))]);
        let context = gio::AppLaunchContext::new();
        env.restore_on(&context);
        let environment = context.environment();
        assert!(environment.iter().any(|var| var == "GSK_RENDERER=gl"));
        assert!(!environment.iter().any(|var| var.as_bytes().starts_with(b"GTK_THEME=")));
    }

    #[test]
    fn an_exec_line_keeps_an_awkward_path_intact() {
        let line = exec_line(Path::new("/opt/My App's 100%.AppImage"));
        assert_eq!(line, r"'/opt/My App'\''s 100%%.AppImage'");
    }
}
