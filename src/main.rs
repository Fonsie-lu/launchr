//! launchr — a Wayland application launcher.
//!
//! Renders as a fullscreen `wlr-layer-shell` overlay. The backdrop is a
//! screenshot the launcher takes of the output just before mapping, blurred in
//! process and dimmed with a scrim, so it looks identical on river, sway,
//! Hyprland or any other wlroots compositor without asking any of them for a
//! blur rule. Results are ranked by fuzzy match quality boosted by how often
//! the application has been launched from here before.

mod blur;
mod capture;
mod config;
mod matcher;
mod usage;

use config::{AppImageConfig, Colors};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use usage::Usage;

const APP_ID: &str = "ch.bithawk.launchr";
const NAMESPACE: &str = "launchr";
const STYLE: &str = include_str!("style.css");

/// Radius `-b` picks when it is given no number of its own.
const DEFAULT_BLUR: u32 = 32;

/// Shown for anything without an icon of its own.
const FALLBACK_ICON: &str = "application-x-executable";

/// How long the main thread waits on the capture worker before giving up and
/// showing a dim-only window.
///
/// The single budget for the whole backdrop: the worker is handed a deadline
/// derived from it rather than carrying a constant of its own, so there are
/// never two numbers to keep in step.
const BACKDROP_WAIT: Duration = Duration::from_millis(900);

/// The share of `BACKDROP_WAIT` the compositor exchanges may spend. What is
/// left pays for the downscale and the three blur passes, which run after the
/// last frame arrives: there is no point capturing a frame the main thread
/// will have given up on by the time it has been blurred.
const CAPTURE_SHARE: u32 = 3;

/// Wall clock since process start, printed only with LAUNCHR_TIMING=1 in the
/// environment. Startup latency is the whole point of a launcher, so the
/// breakdown stays in the binary rather than living in a scratch patch.
mod timing {
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    static ON: OnceLock<bool> = OnceLock::new();

    pub fn init() {
        START.get_or_init(Instant::now);
        ON.get_or_init(|| std::env::var_os("LAUNCHR_TIMING").is_some());
    }

    pub fn mark(label: &str) {
        if *ON.get_or_init(|| false) {
            if let Some(start) = START.get() {
                eprintln!("launchr: {:7.1}ms  {label}", start.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
}

struct Config {
    lines: usize,
    width: i32,
    placeholder: String,
    query: String,
    /// Blur radius in screen pixels; 0 means no screenshot is taken at all.
    /// Off by default: the capture is a full compositor round trip plus a
    /// readback of the whole framebuffer, which is most of the launcher's
    /// startup on a slow machine.
    blur: u32,
    /// Opacity of the scrim painted over the blurred backdrop.
    dim: f64,
    /// Font size, in pixels, of a result row's name. The search entry scales
    /// with it (see `render_style`) so the "large input, small results"
    /// proportion holds at any size.
    font_size: u32,
    colors: Colors,
    /// Directly launchable AppImages, from `~/.config/launchr.json` — they
    /// have no `.desktop` file, so GIO never surfaces them on its own.
    appimages: Vec<AppImageConfig>,
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
    /// Applies `~/.config/launchr.json` on top of the built-in defaults.
    /// Called before `parse_args`, so a CLI flag still wins over the file.
    fn with_file_overrides(mut self) -> Self {
        let file = config::load();
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

/// What activating an entry actually launches. Held in a vec parallel to
/// `State::entries` so the ranking never has to touch a GTK type.
#[derive(Clone)]
enum Target {
    Desktop(gio::AppInfo),
    AppImage(AppImageConfig),
}

/// One searchable field of an entry and how much a hit in it counts.
struct Field {
    folded: matcher::Folded,
    weight: f32,
}

fn field(raw: &str, weight: f32) -> Field {
    Field { folded: matcher::Folded::new(raw), weight }
}

/// Everything the ranking needs about one launchable entry. Deliberately free
/// of GTK and GIO types so `rank` can be unit tested.
struct Entry {
    id: String,
    /// As displayed, with its original case and accents.
    name: String,
    /// The name is always the first field, at full weight.
    fields: Vec<Field>,
    count: u32,
    last_used: u64,
}

impl Entry {
    /// Folded name, which is what keeps the list in alphabetical order.
    fn sort_key(&self) -> &str {
        &self.fields[0].folded.text
    }
}

struct State {
    entries: Vec<Entry>,
    targets: Vec<Target>,
    /// Indices into `entries`, in the order currently displayed.
    shown: Vec<usize>,
    usage: Usage,
    launch_env: LaunchEnv,
}

/// The environment as it was before `main` pinned `GSK_RENDERER`/`GTK_THEME`
/// for launchr's own popup. Restored on the launch context in `activate` so
/// launched apps still see the system GTK theme instead of launchr's.
struct LaunchEnv {
    original_gsk_renderer: Option<String>,
    original_gtk_theme: Option<String>,
}

/// Highlighted row. Kept outside `State` because GTK fires `row-selected`
/// synchronously from inside code that already holds the `State` borrow.
type Selected = Rc<Cell<usize>>;

fn main() -> glib::ExitCode {
    timing::init();
    timing::mark("main");

    // Both overrides happen here, before a single thread exists, and not down
    // beside `gtk::init` where they belong logically. glibc's `setenv` can
    // reallocate `environ`, and both workers below read the environment — the
    // scan for XDG_DATA_HOME/XDG_DATA_DIRS, the capture for WAYLAND_DISPLAY —
    // so writing to it once they are running is a data race, not a tidiness
    // question.
    //
    // GL/Vulkan context creation dominates the first frame of a process that
    // only lives for a few seconds; the cairo renderer draws this UI just as
    // well and starts sooner. An explicit GSK_RENDERER still wins. These two
    // overrides are for launchr's own popup only — apps launched from it get
    // the pre-override value (or its absence) back via the launch context in
    // `activate`, so they still pick up the system GTK theme.
    let original_gsk_renderer = std::env::var("GSK_RENDERER").ok();
    if original_gsk_renderer.is_none() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    // The launcher paints over the user's GTK theme anyway, and parsing one is
    // easily the most expensive thing GTK does here — a big theme is a quarter
    // of a megabyte of CSS. Pin the built-in theme, which is a compiled-in
    // resource. An explicit GTK_THEME still wins.
    let original_gtk_theme = std::env::var("GTK_THEME").ok();
    if original_gtk_theme.is_none() {
        std::env::set_var("GTK_THEME", "Adwaita");
    }

    let config = match parse_args(Config::default().with_file_overrides(), std::env::args().skip(1))
    {
        Ok(Some(config)) => config,
        Ok(None) => return glib::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("launchr: {message}");
            return glib::ExitCode::FAILURE;
        }
    };

    // Reading the desktop files is a few hundred small reads that never touch
    // GTK, so it runs alongside bringing the toolkit up. Spawned once the
    // arguments are known to be good: `--help` and a rejected flag return
    // before any window exists, and should not pay for a scan nobody reads.
    let scan_job = std::thread::spawn(scan_desktop_files);

    // Grab the backdrop before GTK maps anything, otherwise the launcher ends
    // up in its own screenshot. Capture talks to the compositor over its own
    // Wayland connection and the blur is pure CPU work, so both go on a worker
    // thread as well; the pixels are collected again before the window is
    // presented, which is the point the ordering actually has to hold. The
    // result comes back over a channel rather than a join so the wait can time
    // out — see `BACKDROP_WAIT`.
    let blur_radius = config.blur;
    let capture_job = (blur_radius > 0).then(|| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let deadline = std::time::Instant::now() + BACKDROP_WAIT / CAPTURE_SHARE;
        std::thread::spawn(move || {
            let frames = capture::capture_outputs(deadline);
            timing::mark("captured");
            // Sequential across outputs. Blurring them in parallel would need
            // `Mapping` to claim `Sync`, and a hand-written unsafe impl on the
            // mmap wrapper is not worth the few ms it would save on the second
            // monitor.
            let images: Vec<_> = frames
                .iter()
                .filter_map(|frame| {
                    Some((frame.connector.clone(), blur::blurred(frame, blur_radius)?))
                })
                .collect();
            timing::mark("blurred");
            let _ = sender.send(images);
        });
        receiver
    });

    // A plain gtk::init beats gtk::Application here: GApplication registers
    // itself on the session bus before it will emit `activate`, and that
    // round trip costs more than everything this launcher does with GTK.
    glib::set_prgname(Some(APP_ID));
    glib::set_application_name("launchr");
    gdk::set_allowed_backends("wayland");
    if gtk::init().is_err() {
        eprintln!("launchr: cannot open a display");
        return glib::ExitCode::FAILURE;
    }
    timing::mark("gtk init");

    load_css(&config.colors, config.font_size, config.dim);
    timing::mark("css loaded");

    // Building the item list runs before the backdrop is collected rather than
    // after, so `AppInfo::all()` — which reparses every desktop file the scan
    // thread already read, and is the most expensive thing left on the main
    // thread — overlaps the capture instead of queueing behind it.
    let usage = Usage::load();
    timing::mark("usage loaded");
    // `panic = "abort"` is set for the release profile only, so this fallback
    // is unreachable there but live in dev and test builds — where losing the
    // two extra searchable keys beats taking the launcher down with the
    // worker.
    let extras = scan_job.join().unwrap_or_default();
    timing::mark("desktop files scanned");
    let (entries, targets) = load_items(&usage, &extras, &config.appimages);
    timing::mark("items loaded");

    let backdrops = capture_job.map_or_else(Vec::new, |receiver| {
        let images = receiver.recv_timeout(BACKDROP_WAIT).unwrap_or_else(|_| {
            eprintln!("launchr: backdrop was not ready in time, dimming only");
            Vec::new()
        });
        timing::mark("backdrop collected");
        let out: Vec<Backdrop> = images
            .iter()
            .map(|(connector, image)| Backdrop {
                connector: connector.clone(),
                texture: texture(image),
            })
            .collect();
        timing::mark("textures");
        out
    });

    let main_loop = glib::MainLoop::new(None, false);
    build_ui(
        &main_loop,
        &Rc::new(config),
        &Rc::new(backdrops),
        State {
            entries,
            targets,
            shown: Vec::new(),
            usage,
            launch_env: LaunchEnv { original_gsk_renderer, original_gtk_theme },
        },
    );
    main_loop.run();
    timing::mark("exit");
    glib::ExitCode::SUCCESS
}

fn flag_value(flag: &str, next: Option<String>) -> Result<String, String> {
    next.ok_or_else(|| format!("{flag} needs a value"))
}

fn flag_number<T: std::str::FromStr>(flag: &str, next: Option<String>) -> Result<T, String> {
    flag_value(flag, next)?
        .parse()
        .map_err(|_| format!("{flag} expects a number"))
}

/// `base` is the built-in defaults with `~/.config/launchr.json` already
/// applied; any flag here overrides it further.
fn parse_args<I>(base: Config, args: I) -> Result<Option<Config>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = base;
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "launchr — Wayland application launcher\n\n\
                     Usage: launchr [options]\n\n\
                     Options:\n  \
                     -l, --lines N     result rows to show (default 6)\n  \
                     -w, --width N     panel width in pixels (default 680)\n  \
                     -p, --prompt TEXT placeholder text (default: none)\n  \
                     -q, --query TEXT  start with the input prefilled\n  \
                     -b, --blur [N]    blur the desktop behind the launcher,\n  \
                     \x20                 radius N in pixels (default 32, off\n  \
                     \x20                 unless asked for; costs a screenshot)\n  \
                     -d, --dim F       backdrop dim, 0.0 to 1.0 (default 0.75)\n  \
                     \x20   --no-blur     no blur; the default\n  \
                     -h, --help        show this help\n\n\
                     Colors, font size, list length, dim, blur and a list of\n\
                     directly launchable AppImages can also be set in\n\
                     ~/.config/launchr.json; flags above override it."
                );
                return Ok(None);
            }
            "-l" | "--lines" => {
                let lines = flag_number::<usize>(&arg, args.next())?;
                config.lines = lines.clamp(*config::LINES.start(), *config::LINES.end());
            }
            "-w" | "--width" => {
                config.width = flag_number::<i32>(&arg, args.next())?.clamp(240, 3000);
            }
            "-p" | "--prompt" => config.placeholder = flag_value(&arg, args.next())?,
            "-q" | "--query" => config.query = flag_value(&arg, args.next())?,
            "-b" | "--blur" => {
                // A bare -b means "blur, you pick the radius"; a number after
                // it sets one. Anything else is the next option, left alone.
                let radius = match args.peek().and_then(|next| next.parse::<u32>().ok()) {
                    Some(radius) => {
                        args.next();
                        radius
                    }
                    None => DEFAULT_BLUR,
                };
                config.blur = radius.min(config::MAX_BLUR);
            }
            "--no-blur" => config.blur = 0,
            "-d" | "--dim" => {
                // `"nan".parse::<f64>()` succeeds and `f64::clamp` hands NaN
                // straight back, which would reach the stylesheet as
                // `rgba(r, g, b, NaN)` and take the whole scrim out with it.
                let dim = flag_number::<f64>(&arg, args.next())?;
                if !dim.is_finite() {
                    return Err(format!("{arg} expects a number"));
                }
                config.dim = dim.clamp(*config::DIM.start(), *config::DIM.end());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some(config))
}

/// One output's blurred screenshot, tagged with the connector it came from.
struct Backdrop {
    connector: Option<String>,
    texture: gdk::MemoryTexture,
}

fn texture(image: &blur::Image) -> gdk::MemoryTexture {
    gdk::MemoryTexture::new(
        image.width as i32,
        image.height as i32,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from(&image.rgb),
        (image.width * 3) as usize,
    )
}

/// Fills in `style.css`'s color and size placeholders. Plain string
/// substitution rather than GTK's `@define-color` cascade, so the result is
/// ordinary CSS regardless of provider load order.
fn render_style(colors: &Colors, font_size: u32, dim: f64) -> String {
    // Keeps the original theme's ratio between the search entry and a result
    // row's name (39px / 21px) at any font size.
    let entry_size = font_size + 18;
    STYLE
        .replace("__BG_RGB__", &config::rgb_triplet(&colors.background))
        .replace("__PANEL_RGB__", &config::rgb_triplet(&colors.panel))
        .replace("__FOREGROUND__", &colors.foreground)
        .replace("__SELECTION_RGB__", &config::rgb_triplet(&colors.selection))
        .replace("__SELECTION__", &colors.selection)
        .replace("__ACCENT__", &colors.accent)
        .replace("__MUTED__", &colors.muted)
        .replace("__ENTRY_SIZE__", &entry_size.to_string())
        .replace("__NAME_SIZE__", &font_size.to_string())
        .replace("__DIM__", &dim.to_string())
}

fn load_css(colors: &Colors, font_size: u32, dim: f64) {
    let Some(display) = gdk::Display::default() else { return };
    // Above PRIORITY_USER, not PRIORITY_APPLICATION: a user GTK theme in
    // ~/.config/gtk-4.0/gtk.css sits at 800 and would otherwise repaint the
    // list and its selection in the theme's own colours.
    //
    // One provider, not two: the dim used to arrive as a second provider
    // layered on top, and every provider added to a display invalidates the
    // whole style cascade again. It is a token in the template instead.
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&render_style(colors, font_size, dim));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
    );
}

/// One result row. The widgets are built once and refilled as the query
/// changes, so a keystroke costs a label and an icon update instead of tearing
/// down and rebuilding the whole list.
struct Row {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    name: gtk::Label,
    /// Index into `State::entries` currently displayed. Entry indices are
    /// fixed for the life of the process, so this is enough to skip a refill —
    /// and the icon lookup inside it — for a row a keystroke did not move.
    shown: Cell<Option<usize>>,
}

impl Row {
    /// Display `entry`. Nothing outside these two methods touches `shown`, so
    /// it cannot drift out of step with what the widgets are holding.
    fn show(&self, index: usize, entry: &Entry, target: &Target) {
        // Narrowing a query usually leaves the top rows where they were, and
        // the icon lookup below is the expensive half of this method.
        if self.shown.replace(Some(index)) != Some(index) {
            self.name.set_label(&entry.name);
            match target {
                Target::Desktop(info) => match info.icon() {
                    Some(gicon) => self.icon.set_from_gicon(&gicon),
                    None => self.icon.set_icon_name(Some(FALLBACK_ICON)),
                },
                Target::AppImage(app) => {
                    self.icon.set_icon_name(Some(app.icon.as_deref().unwrap_or(FALLBACK_ICON)))
                }
            }
        }
        self.row.set_visible(true);
    }

    fn hide(&self) {
        self.shown.set(None);
        self.row.set_visible(false);
    }
}

fn build_ui(
    main_loop: &glib::MainLoop,
    config: &Rc<Config>,
    backdrops: &Rc<Vec<Backdrop>>,
    state: State,
) {
    let state = Rc::new(RefCell::new(state));
    let selected: Selected = Rc::new(Cell::new(0));

    let window = gtk::Window::builder().css_classes(["launchr"]).build();
    // No GtkApplication to hold the process open, so the loop ends with the
    // window: closing it on Escape, on activation or on a click outside is
    // what quits the launcher.
    window.connect_close_request({
        let main_loop = main_loop.clone();
        move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        }
    });

    // Fullscreen overlay so the dim/blur covers the whole output.
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some(NAMESPACE));
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);

    let entry = gtk::Entry::builder()
        .placeholder_text(&config.placeholder)
        .text(&config.query)
        .css_classes(["search"])
        .has_frame(false)
        .hexpand(true)
        .build();

    let list = gtk::ListBox::builder()
        .css_classes(["results"])
        .selection_mode(gtk::SelectionMode::Browse)
        .show_separators(false)
        .activate_on_single_click(true)
        .build();
    let rows = Rc::new(build_rows(&list, config.lines));

    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["panel"])
        .width_request(config.width)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    panel.append(&entry);
    panel.append(&list);

    let dim = gtk::Box::builder()
        .css_classes(["dim"])
        .hexpand(true)
        .vexpand(true)
        .build();
    dim.append(&panel);

    let backdrop = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Fill)
        .can_shrink(true)
        .build();
    if let Some(first) = backdrops.first() {
        backdrop.set_paintable(Some(&first.texture));
    }

    let stack = gtk::Overlay::builder().child(&backdrop).build();
    stack.add_overlay(&dim);
    window.set_child(Some(&stack));

    // The compositor picks which output the layer surface lands on, so the
    // matching screenshot can only be chosen once the surface exists.
    window.connect_map({
        let backdrops = backdrops.clone();
        let backdrop = backdrop.clone();
        move |window| {
            // A single output skips the round trip that delivers connector
            // names, so unnamed shots are the one case where whatever was
            // captured is by definition the right one. Everything else has to
            // be matched, including a lone shot left over from a multi-output
            // capture where the other output failed.
            if backdrops.iter().all(|b| b.connector.is_none()) {
                return;
            }
            let Some(surface) = window.surface() else { return };
            let Some(monitor) = WidgetExt::display(window).monitor_at_surface(&surface) else {
                return;
            };
            // No name from GDK is not the same as no match: without one there
            // is nothing to pair against, so leave the first shot in place
            // rather than blanking a backdrop that may well be correct.
            let Some(connector) = monitor.connector() else { return };
            match backdrops.iter().find(|b| b.connector.as_deref() == Some(connector.as_str())) {
                Some(found) => backdrop.set_paintable(Some(&found.texture)),
                // This output is named and nothing was captured for it. No
                // backdrop beats another monitor's desktop behind the panel.
                None => backdrop.set_paintable(None::<&gdk::Texture>),
            }
        }
    });

    list.connect_row_selected({
        let selected = selected.clone();
        move |_, row| {
            if let Some(row) = row {
                selected.set(row.index().max(0) as usize);
            }
        }
    });

    list.connect_row_activated({
        let state = state.clone();
        let window = window.clone();
        move |_, row| activate(&state, row.index().max(0) as usize, &window)
    });

    entry.connect_activate({
        let state = state.clone();
        let selected = selected.clone();
        let window = window.clone();
        move |_| activate(&state, selected.get(), &window)
    });

    let click = gtk::GestureClick::new();
    click.connect_pressed({
        let window = window.clone();
        let panel = panel.clone();
        move |gesture, _, x, y| {
            let Some(widget) = gesture.widget() else { return };
            let origin = gtk::graphene::Point::new(x as f32, y as f32);
            // Anything outside the panel counts as "dismiss".
            if let Some(point) = widget.compute_point(&panel, &origin) {
                let inside = point.x() >= 0.0
                    && point.y() >= 0.0
                    && point.x() <= panel.width() as f32
                    && point.y() <= panel.height() as f32;
                if !inside {
                    window.close();
                }
            }
        }
    });
    dim.add_controller(click);

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let state = state.clone();
        let selected = selected.clone();
        let list = list.clone();
        let rows = rows.clone();
        let window = window.clone();
        move |_, key, _, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::Escape => window.close(),
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    activate(&state, selected.get(), &window);
                }
                gdk::Key::Down | gdk::Key::Tab => {
                    move_selection(&state, &selected, &list, &rows, 1)
                }
                gdk::Key::Up | gdk::Key::ISO_Left_Tab => {
                    move_selection(&state, &selected, &list, &rows, -1)
                }
                gdk::Key::n | gdk::Key::j if ctrl => {
                    move_selection(&state, &selected, &list, &rows, 1)
                }
                gdk::Key::p | gdk::Key::k if ctrl => {
                    move_selection(&state, &selected, &list, &rows, -1)
                }
                gdk::Key::Page_Down => move_selection(&state, &selected, &list, &rows, 5),
                gdk::Key::Page_Up => move_selection(&state, &selected, &list, &rows, -5),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);

    // Fill the rows for the starting query before wiring up `changed`: the
    // entry already carries `config.query`, so connecting first would have the
    // signal repeat this same pass for nothing.
    refresh(&state, &selected, &list, &rows, &config.query);
    timing::mark("rows filled");
    entry.connect_changed({
        let state = state.clone();
        let selected = selected.clone();
        let list = list.clone();
        let rows = rows.clone();
        move |entry| refresh(&state, &selected, &list, &rows, &entry.text())
    });

    // Hold the height of a full result list whatever is actually in it, so
    // the panel keeps its size and the input stays put instead of drifting up
    // the screen as a query narrows things down. Measured from a real row so
    // it follows the stylesheet rather than a number repeated here.
    if let Some(first) = rows.first() {
        let (_, natural, _, _) = first.row.measure(gtk::Orientation::Vertical, -1);
        if natural > 0 {
            list.set_height_request(natural * config.lines as i32);
        }
    }
    window.add_tick_callback(|_, _| {
        timing::mark("first frame");
        glib::ControlFlow::Break
    });
    window.present();
    timing::mark("presented");
    entry.grab_focus();
    entry.set_position(-1);
}

/// Build the fixed set of result rows and hand back the handles `refresh`
/// writes into. Rows beyond the current result count are hidden rather than
/// removed, which keeps a row's index and its position in `State::shown` the
/// same number.
fn build_rows(list: &gtk::ListBox, lines: usize) -> Vec<Row> {
    (0..lines)
        .map(|_| {
            let icon = gtk::Image::new();
            icon.set_pixel_size(40);
            let name = label("name");

            let body = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(14)
                .css_classes(["row-body"])
                .build();
            body.append(&icon);
            body.append(&name);

            let row = gtk::ListBoxRow::builder().child(&body).css_classes(["result"]).build();
            list.append(&row);
            Row { row, icon, name, shown: Cell::new(None) }
        })
        .collect()
}

/// Collect every desktop entry the user is allowed to see, plus the
/// configured AppImages, as two index-aligned vecs.
fn load_items(
    usage: &Usage,
    extras: &HashMap<String, Extra>,
    appimages: &[AppImageConfig],
) -> (Vec<Entry>, Vec<Target>) {
    let mut items: Vec<(Entry, Target)> = gio::AppInfo::all()
        .into_iter()
        .filter(|info| info.should_show())
        .filter_map(|info| {
            let id = info.id()?.to_string();
            let name = info.name().to_string();
            let extra = extras.get(&id);

            let mut fields = vec![field(&name, 1.0)];
            if let Some(generic) = extra.and_then(|e| e.generic_name.as_deref()) {
                fields.push(field(generic, 0.65));
            }
            if let Some(exec) = info.executable().file_name() {
                fields.push(field(&exec.to_string_lossy(), 0.6));
            }
            if let Some(keywords) = extra.map(|e| &e.keywords).filter(|k| !k.is_empty()) {
                fields.push(field(keywords, 0.45));
            }
            if let Some(comment) = info.description().filter(|s| !s.is_empty()) {
                fields.push(field(&comment, 0.35));
            }

            let (count, last_used) = usage.get(&id);
            let entry = Entry { id, name, fields, count, last_used };
            Some((entry, Target::Desktop(info)))
        })
        .collect();

    for appimage in appimages {
        let id = format!("appimage:{}", appimage.path.display());
        let name = appimage.name.clone();
        let mut fields = vec![field(&name, 1.0)];
        if let Some(stem) = appimage.path.file_stem().and_then(|s| s.to_str()) {
            fields.push(field(stem, 0.6));
        }
        let (count, last_used) = usage.get(&id);
        let entry = Entry { id, name, fields, count, last_used };
        items.push((entry, Target::AppImage(appimage.clone())));
    }

    // Compare the folded names in place. `sort_by_key` would have called the
    // key function once per comparison, not once per item, which made this a
    // few tens of thousands of throwaway allocations on the startup path.
    items.sort_by(|(a, _), (b, _)| a.sort_key().cmp(b.sort_key()));
    items.into_iter().unzip()
}

#[derive(Default)]
struct Extra {
    generic_name: Option<String>,
    keywords: String,
}

/// GIO exposes no `GDesktopAppInfo` bindings in Rust, so pick up the two extra
/// searchable keys straight from the desktop files.
fn scan_desktop_files() -> HashMap<String, Extra> {
    let mut out = HashMap::new();
    for dir in application_dirs() {
        collect_desktop_files(&dir, "", &mut out);
    }
    out
}

fn application_dirs() -> Vec<PathBuf> {
    let home_data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));

    let system = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());

    // Later directories must not override earlier (higher priority) ones.
    home_data
        .into_iter()
        .chain(system.split(':').filter(|p| !p.is_empty()).map(PathBuf::from))
        .map(|p| p.join("applications"))
        .collect()
}

fn collect_desktop_files(dir: &Path, prefix: &str, out: &mut HashMap<String, Extra>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if path.is_dir() {
            collect_desktop_files(&path, &format!("{prefix}{name}-"), out);
        } else if name.ends_with(".desktop") {
            let id = format!("{prefix}{name}");
            if out.contains_key(&id) {
                continue;
            }
            if let Some(extra) = parse_desktop_file(&path) {
                out.insert(id, extra);
            }
        }
    }
}

/// Read the `[Desktop Entry]` group and stop. Everything after it is actions
/// and other groups this does not search, and a desktop file is read here only
/// to be reparsed by GIO a moment later, so the less of it that is touched the
/// better.
fn parse_desktop_file(path: &Path) -> Option<Extra> {
    // One read and borrowed lines. A `BufReader` would allocate a `String` per
    // line and an 8KiB buffer per file for no gain — these files are smaller
    // than that buffer, so stopping early saves parsing, never a read. Failing
    // the whole file on unreadable bytes is deliberate: a half-parsed entry
    // would shadow a readable copy of the same id in a lower priority
    // directory.
    let text = std::fs::read_to_string(path).ok()?;
    let mut extra = Extra::default();
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_entry {
                break;
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        // Ignore locale-suffixed variants; the plain keys match well enough.
        if let Some(value) = line.strip_prefix("GenericName=") {
            extra.generic_name = Some(value.to_owned()).filter(|v| !v.is_empty());
        } else if let Some(value) = line.strip_prefix("Keywords=") {
            extra.keywords = value.replace(';', " ").trim().to_owned();
        }
    }
    Some(extra)
}

/// Apply a field's weight to the score it produced.
///
/// Scaling cannot simply multiply: `score` goes negative for a poor match in a
/// long field, and multiplying a negative by a weight below one makes it
/// *larger*, so the comment (0.35) would outrank the keywords (0.45) exactly
/// when both matched badly. Dividing on that side keeps a lower weight worth
/// less whatever the sign.
fn weigh(score: i32, weight: f32) -> i32 {
    let score = score as f32;
    (if score >= 0.0 { score * weight } else { score / weight }) as i32
}

/// Keep the `lines` best of `ranked` in order and drop the rest. Generic so
/// each caller's comparison inlines — this runs over the whole entry list on
/// the startup path, where a trait object's indirect call blocks that.
fn take_top<F>(ranked: &mut Vec<(usize, i32)>, lines: usize, mut compare: F)
where
    F: FnMut(&(usize, i32), &(usize, i32)) -> std::cmp::Ordering,
{
    // Only `lines` rows are ever displayed, so pull them out with a partial
    // sort and order those. On an empty query that is the difference between
    // ordering six entries and ordering a few thousand.
    if ranked.len() > lines {
        ranked.select_nth_unstable_by(lines, &mut compare);
        ranked.truncate(lines);
    }
    ranked.sort_by(compare);
}

/// Pick the entries to show for `query`, best first, at most `lines` of them.
///
/// GTK-free on purpose: this is the whole of the ranking policy and it is
/// covered by the tests at the bottom of this file.
fn rank(entries: &[Entry], query: &str, lines: usize) -> Vec<usize> {
    let needle = matcher::Folded::new(query.trim());

    // In both branches below `entries` is in folded-name order, so the index is
    // the alphabetical tie break and no name has to be folded again here. It
    // also makes each comparison a total order, which is what lets `take_top`
    // produce a stable, deterministic list.
    if needle.is_empty() {
        let mut ranked: Vec<(usize, i32)> = (0..entries.len()).map(|i| (i, 0)).collect();
        take_top(&mut ranked, lines, |&(ai, _), &(bi, _)| {
            let (a, b) = (&entries[ai], &entries[bi]);
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_used.cmp(&a.last_used))
                .then_with(|| ai.cmp(&bi))
        });
        return ranked.into_iter().map(|(i, _)| i).collect();
    }

    let mut ranked: Vec<(usize, i32)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            let best = entry
                .fields
                .iter()
                .filter_map(|f| matcher::score(&needle, &f.folded).map(|s| weigh(s, f.weight)))
                .max()?;
            Some((i, best + popularity_bonus(entry.count)))
        })
        .collect();
    take_top(&mut ranked, lines, |&(ai, asc), &(bi, bsc)| {
        let (a, b) = (&entries[ai], &entries[bi]);
        bsc.cmp(&asc)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| ai.cmp(&bi))
    });
    ranked.into_iter().map(|(i, _)| i).collect()
}

/// Rebuild the visible rows for `query`.
/// `rows` was built with one row per displayable line, so it is the row count
/// as well as the widgets — there is no separate `lines` to fall out of step
/// with it.
fn refresh(
    state: &Rc<RefCell<State>>,
    selected: &Selected,
    list: &gtk::ListBox,
    rows: &[Row],
    query: &str,
) {
    let mut state = state.borrow_mut();
    state.shown = rank(&state.entries, query, rows.len());
    selected.set(0);

    for (position, row) in rows.iter().enumerate() {
        match state.shown.get(position) {
            Some(&index) => row.show(index, &state.entries[index], &state.targets[index]),
            None => row.hide(),
        }
    }
    if state.shown.is_empty() {
        list.select_row(None::<&gtk::ListBoxRow>);
    } else {
        list.select_row(Some(&rows[0].row));
    }
}

/// Frequently launched apps get a boost, but on a log curve so a good name
/// match on a fresh app can still win.
fn popularity_bonus(count: u32) -> i32 {
    if count == 0 {
        0
    } else {
        (((count + 1) as f32).ln() * 40.0) as i32
    }
}

/// `max_width_chars(1)` lets the label shrink below its natural size, which is
/// what makes ellipsizing actually kick in inside a fixed width panel.
fn label(class: &str) -> gtk::Label {
    gtk::Label::builder()
        .css_classes([class])
        .xalign(0.0)
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .max_width_chars(1)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build()
}

fn move_selection(
    state: &Rc<RefCell<State>>,
    selected: &Selected,
    list: &gtk::ListBox,
    rows: &[Row],
    delta: i32,
) {
    let len = state.borrow().shown.len();
    if len == 0 {
        return;
    }
    // Wrap around, like fuzzel does.
    let next = (selected.get() as i32 + delta).rem_euclid(len as i32) as usize;
    selected.set(next);
    if let Some(row) = rows.get(next) {
        list.select_row(Some(&row.row));
    }
}

fn activate(state: &Rc<RefCell<State>>, position: usize, window: &gtk::Window) {
    let target = {
        let state = state.borrow();
        state
            .shown
            .get(position)
            .map(|&index| (state.targets[index].clone(), state.entries[index].id.clone()))
    };
    let Some((target, id)) = target else { return };

    // `main` pinned GSK_RENDERER/GTK_THEME in the process environment for
    // launchr's own popup; undo that just for this launch so the app we
    // start still follows the system GTK theme instead of launchr's.
    let launched = match target {
        Target::Desktop(info) => {
            let context = WidgetExt::display(window).app_launch_context();
            {
                let state = state.borrow();
                match &state.launch_env.original_gsk_renderer {
                    Some(value) => context.setenv("GSK_RENDERER", value),
                    None => context.unsetenv("GSK_RENDERER"),
                }
                match &state.launch_env.original_gtk_theme {
                    Some(value) => context.setenv("GTK_THEME", value),
                    None => context.unsetenv("GTK_THEME"),
                }
            }
            match info.launch(&[] as &[gio::File], Some(&context)) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("launchr: failed to launch {id}: {error}");
                    false
                }
            }
        }
        Target::AppImage(app) => {
            let mut command = std::process::Command::new(&app.path);
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            {
                let state = state.borrow();
                match &state.launch_env.original_gsk_renderer {
                    Some(value) => {
                        command.env("GSK_RENDERER", value);
                    }
                    None => {
                        command.env_remove("GSK_RENDERER");
                    }
                }
                match &state.launch_env.original_gtk_theme {
                    Some(value) => {
                        command.env("GTK_THEME", value);
                    }
                    None => {
                        command.env_remove("GTK_THEME");
                    }
                }
            }
            match command.spawn() {
                Ok(_) => true,
                Err(error) => {
                    eprintln!("launchr: failed to launch {}: {error}", app.path.display());
                    false
                }
            }
        }
    };
    // Close first: recording the launch flushes the store to disk, and there
    // is no reason for the launcher to stay on screen while that happens.
    window.close();
    if launched {
        state.borrow_mut().usage.record(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry as the ranking sees it: no GIO, no GTK.
    fn entry(name: &str, count: u32, last_used: u64) -> Entry {
        Entry {
            id: format!("{name}.desktop"),
            name: name.to_owned(),
            fields: vec![field(name, 1.0)],
            count,
            last_used,
        }
    }

    /// `load_items` leaves the list in folded-name order and `rank` relies on
    /// it, so the fixtures here do the same.
    fn sorted(mut entries: Vec<Entry>) -> Vec<Entry> {
        entries.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        entries
    }

    fn names(entries: &[Entry], shown: &[usize]) -> Vec<String> {
        shown.iter().map(|&i| entries[i].name.clone()).collect()
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn parse(list: &[&str]) -> Config {
        parse_args(Config::default(), args(list))
            .expect("should parse")
            .expect("should not be --help")
    }

    #[test]
    fn flags_override_the_defaults() {
        let config = parse(&["-l", "9", "-w", "900", "-p", "run:", "-q", "fire", "-d", "0.5"]);
        assert_eq!(config.lines, 9);
        assert_eq!(config.width, 900);
        assert_eq!(config.placeholder, "run:");
        assert_eq!(config.query, "fire");
        assert_eq!(config.dim, 0.5);
    }

    #[test]
    fn flags_override_the_file_layer_they_are_given() {
        let base = Config { lines: 3, blur: 12, ..Config::default() };
        let config = parse_args(base, args(&["-l", "7"])).unwrap().unwrap();
        assert_eq!(config.lines, 7);
        // Untouched by a flag, so the value handed in survives.
        assert_eq!(config.blur, 12);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let config = parse(&["-l", "500", "-w", "10", "-d", "4", "-b", "9000"]);
        assert_eq!(config.lines, 50);
        assert_eq!(config.width, 240);
        assert_eq!(config.dim, 1.0);
        assert_eq!(config.blur, 200);
    }

    #[test]
    fn blur_takes_an_optional_radius() {
        assert_eq!(parse(&["-b"]).blur, DEFAULT_BLUR);
        assert_eq!(parse(&["-b", "12"]).blur, 12);
        // A following option must not be eaten as the radius.
        let config = parse(&["-b", "-l", "4"]);
        assert_eq!(config.blur, DEFAULT_BLUR);
        assert_eq!(config.lines, 4);
        // Last flag wins, in both directions.
        assert_eq!(parse(&["-b", "20", "--no-blur"]).blur, 0);
        assert_eq!(parse(&["--no-blur", "-b", "20"]).blur, 20);
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse_args(Config::default(), args(&["--nope"])).is_err());
        assert!(parse_args(Config::default(), args(&["-l"])).is_err());
        assert!(parse_args(Config::default(), args(&["-l", "many"])).is_err());
        // Parses as a float and survives `clamp`, so it needs its own check —
        // it would otherwise reach the stylesheet as `rgba(..., NaN)`.
        assert!(parse_args(Config::default(), args(&["-d", "nan"])).is_err());
        assert!(parse_args(Config::default(), args(&["-d", "inf"])).is_err());
        assert!(parse(&["-d", "0.25"]).dim.is_finite());
    }

    #[test]
    fn a_lower_weight_is_worth_less_whatever_the_sign() {
        // Multiplying would have made the smaller weight the larger number.
        assert!(weigh(-31, 0.45) > weigh(-31, 0.35));
        assert!(weigh(80, 0.45) > weigh(80, 0.35));
        assert_eq!(weigh(0, 0.45), 0);
    }

    #[test]
    fn a_poor_name_match_still_outranks_a_poor_comment_match() {
        // Both score badly enough to go negative, which is where the weighting
        // used to invert and put the description first.
        let mut described = entry("Aardvark Nine", 0, 0);
        described
            .fields
            .push(field("a tool for organising the zebra photographs you keep", 0.35));
        let entries = sorted(vec![described, entry("Zebra", 0, 0)]);
        assert_eq!(names(&entries, &rank(&entries, "zebra", 6))[0], "Zebra");
    }

    #[test]
    fn help_asks_for_no_window() {
        assert!(parse_args(Config::default(), args(&["-h"])).unwrap().is_none());
    }

    #[test]
    fn popularity_is_a_bonus_on_a_log_curve() {
        assert_eq!(popularity_bonus(0), 0);
        assert!(popularity_bonus(1) > 0);
        assert!(popularity_bonus(10) > popularity_bonus(1));
        // Ten times the launches is worth well under ten times the bonus.
        assert!(popularity_bonus(100) < popularity_bonus(10) * 2);
    }

    #[test]
    fn an_empty_query_is_most_used_first() {
        let entries = sorted(vec![
            entry("Alacritty", 0, 0),
            entry("Firefox", 9, 100),
            entry("Gimp", 3, 400),
        ]);
        assert_eq!(names(&entries, &rank(&entries, "", 6)), ["Firefox", "Gimp", "Alacritty"]);
    }

    #[test]
    fn equally_used_entries_fall_back_to_recency_then_name() {
        let entries = sorted(vec![
            entry("Zathura", 2, 500),
            entry("Alacritty", 2, 500),
            entry("Gimp", 2, 900),
        ]);
        assert_eq!(names(&entries, &rank(&entries, "", 6)), ["Gimp", "Alacritty", "Zathura"]);
    }

    #[test]
    fn the_result_list_is_capped_at_lines() {
        let entries = sorted((0..200).map(|i| entry(&format!("app{i:03}"), i, 0)).collect());
        let shown = rank(&entries, "", 6);
        assert_eq!(shown.len(), 6);
        assert_eq!(names(&entries, &shown)[0], "app199");
        assert_eq!(rank(&entries, "app", 4).len(), 4);
    }

    #[test]
    fn a_query_ranks_by_match_quality_not_popularity_alone() {
        let entries = sorted(vec![entry("Firefox", 0, 0), entry("Files", 40, 900)]);
        // "firef" is a much better match than anything in "files", so the
        // popularity bonus must not be able to bury it.
        assert_eq!(names(&entries, &rank(&entries, "firef", 6)), ["Firefox"]);
    }

    #[test]
    fn popularity_breaks_a_tie_between_equal_matches() {
        // Both are a clean prefix match for "term", so nothing but the tie
        // breaks separates them and the shorter name comes first.
        let cold = sorted(vec![entry("Termite", 0, 0), entry("Terminal", 0, 0)]);
        assert_eq!(names(&cold, &rank(&cold, "term", 6)), ["Termite", "Terminal"]);

        // Launch history is enough to turn that around.
        let warm = sorted(vec![entry("Termite", 0, 0), entry("Terminal", 25, 900)]);
        assert_eq!(names(&warm, &rank(&warm, "term", 6)), ["Terminal", "Termite"]);
    }

    #[test]
    fn a_query_that_matches_nothing_shows_nothing() {
        let entries = sorted(vec![entry("Firefox", 5, 0), entry("Gimp", 5, 0)]);
        assert!(rank(&entries, "zzzz", 6).is_empty());
    }

    #[test]
    fn rank_folds_the_query_before_matching() {
        let entries = sorted(vec![entry("Café Player", 0, 0), entry("Gimp", 0, 0)]);
        assert_eq!(names(&entries, &rank(&entries, "  CAFE  ", 6)), ["Café Player"]);
    }
}
