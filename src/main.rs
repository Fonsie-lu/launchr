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
use usage::Usage;

const APP_ID: &str = "ch.bithawk.launchr";
const NAMESPACE: &str = "launchr";
const STYLE: &str = include_str!("style.css");

/// Radius `-b` picks when it is given no number of its own.
const DEFAULT_BLUR: u32 = 32;

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
            let elapsed = START.get_or_init(Instant::now).elapsed();
            eprintln!("launchr: {:>7.1}ms  {label}", elapsed.as_secs_f64() * 1000.0);
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

/// What activating an `Item` actually launches.
#[derive(Clone)]
enum Target {
    Desktop(gio::AppInfo),
    AppImage(AppImageConfig),
}

/// One launchable entry plus the precomputed text we match against.
struct Item {
    target: Target,
    id: String,
    name: String,
    /// Lowercased searchable text with a per-field weight.
    fields: Vec<(String, f32)>,
    count: u32,
    last_used: u64,
}

struct State {
    items: Vec<Item>,
    /// Indices into `items`, in the order currently displayed.
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
    let config = match parse_args(Config::default().with_file_overrides()) {
        Ok(Some(config)) => config,
        Ok(None) => return glib::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("launchr: {message}");
            return glib::ExitCode::FAILURE;
        }
    };

    // Reading the desktop files is a few hundred small reads that never touch
    // GTK, so it runs alongside bringing the toolkit up.
    let scan_job = std::thread::spawn(scan_desktop_files);

    // Grab the backdrop before GTK maps anything, otherwise the launcher ends
    // up in its own screenshot. Capture talks to the compositor over its own
    // Wayland connection and the blur is pure CPU work, so both go on a worker
    // thread as well; the pixels are collected again before the window is
    // presented, which is the point the ordering actually has to hold.
    let blur_radius = config.blur;
    let capture_job = (blur_radius > 0).then(|| {
        std::thread::spawn(move || {
            let frames = capture::capture_outputs();
            timing::mark("captured");
            let images: Vec<_> = frames
                .iter()
                .filter_map(|frame| {
                    Some((frame.connector.clone(), blur::blurred(frame, blur_radius)?))
                })
                .collect();
            timing::mark("blurred");
            images
        })
    });

    // A plain gtk::init beats gtk::Application here: GApplication registers
    // itself on the session bus before it will emit `activate`, and that
    // round trip costs more than everything this launcher does with GTK.
    glib::set_prgname(Some(APP_ID));
    glib::set_application_name("launchr");
    gdk::set_allowed_backends("wayland");
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
    if gtk::init().is_err() {
        eprintln!("launchr: cannot open a display");
        return glib::ExitCode::FAILURE;
    }
    timing::mark("gtk init");

    load_css(&config.colors, config.font_size, config.dim);
    timing::mark("css loaded");

    let backdrops = capture_job.map_or_else(Vec::new, |job| {
        let images = job.join().unwrap_or_default();
        timing::mark("capture joined");
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

    let extras = scan_job.join().unwrap_or_default();
    timing::mark("desktop files scanned");

    let main_loop = glib::MainLoop::new(None, false);
    build_ui(
        &main_loop,
        &Rc::new(config),
        &Rc::new(backdrops),
        extras,
        LaunchEnv { original_gsk_renderer, original_gtk_theme },
    );
    main_loop.run();
    timing::mark("exit");
    glib::ExitCode::SUCCESS
}

/// `base` is the built-in defaults with `~/.config/launchr.json` already
/// applied; any flag here overrides it further.
fn parse_args(base: Config) -> Result<Option<Config>, String> {
    let mut config = base;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        // `-b` is the one option whose value is optional, so the value is
        // pulled out here rather than through a closure over the iterator.
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
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
                config.lines = value()?.parse().map_err(|_| "--lines expects a number")?;
                config.lines = config.lines.clamp(1, 50);
            }
            "-w" | "--width" => {
                config.width = value()?.parse().map_err(|_| "--width expects a number")?;
                config.width = config.width.clamp(240, 3000);
            }
            "-p" | "--prompt" => config.placeholder = value()?,
            "-q" | "--query" => config.query = value()?,
            "-b" | "--blur" => {
                drop(value);
                // A bare -b means "blur, you pick the radius"; a number after
                // it sets one. Anything else is the next option, left alone.
                let radius = match args.peek().and_then(|next| next.parse::<u32>().ok()) {
                    Some(radius) => {
                        args.next();
                        radius
                    }
                    None => DEFAULT_BLUR,
                };
                config.blur = radius.min(200);
            }
            "--no-blur" => config.blur = 0,
            "-d" | "--dim" => {
                config.dim = value()?.parse().map_err(|_| "--dim expects a number")?;
                config.dim = config.dim.clamp(0.0, 1.0);
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
fn render_style(colors: &Colors, font_size: u32) -> String {
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
}

fn load_css(colors: &Colors, font_size: u32, dim: f64) {
    let Some(display) = gdk::Display::default() else { return };
    // Above PRIORITY_USER, not PRIORITY_APPLICATION: a user GTK theme in
    // ~/.config/gtk-4.0/gtk.css sits at 800 and would otherwise repaint the
    // list and its selection in the theme's own colours.
    let base = gtk::STYLE_PROVIDER_PRIORITY_USER + 1;
    let bg_rgb = config::rgb_triplet(&colors.background);
    for (css, priority) in [
        (render_style(colors, font_size), base),
        (
            format!(".dim {{ background-color: rgba({bg_rgb}, {dim}); }}"),
            base + 1,
        ),
    ] {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&css);
        gtk::style_context_add_provider_for_display(&display, &provider, priority);
    }
}

fn build_ui(
    main_loop: &glib::MainLoop,
    config: &Rc<Config>,
    backdrops: &Rc<Vec<Backdrop>>,
    extras: HashMap<String, Extra>,
    launch_env: LaunchEnv,
) {
    let usage = Usage::load();
    timing::mark("usage loaded");
    let items = load_items(&usage, &extras, &config.appimages);
    timing::mark("items loaded");
    let state = Rc::new(RefCell::new(State {
        items,
        shown: Vec::new(),
        usage,
        launch_env,
    }));
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
            if backdrops.len() < 2 {
                return;
            }
            let Some(surface) = window.surface() else { return };
            let Some(monitor) = WidgetExt::display(window).monitor_at_surface(&surface) else {
                return;
            };
            let connector = monitor.connector();
            let connector = connector.as_ref().map(|c| c.as_str());
            if let Some(found) =
                backdrops.iter().find(|b| b.connector.as_deref() == connector)
            {
                backdrop.set_paintable(Some(&found.texture));
            }
        }
    });

    entry.connect_changed({
        let state = state.clone();
        let selected = selected.clone();
        let list = list.clone();
        let config = config.clone();
        move |entry| refresh(&state, &selected, &list, &entry.text(), config.lines)
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
        let window = window.clone();
        move |_, key, _, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::Escape => window.close(),
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    activate(&state, selected.get(), &window);
                }
                gdk::Key::Down | gdk::Key::Tab => move_selection(&state, &selected, &list, 1),
                gdk::Key::Up | gdk::Key::ISO_Left_Tab => move_selection(&state, &selected, &list, -1),
                gdk::Key::n | gdk::Key::j if ctrl => move_selection(&state, &selected, &list, 1),
                gdk::Key::p | gdk::Key::k if ctrl => move_selection(&state, &selected, &list, -1),
                gdk::Key::Page_Down => move_selection(&state, &selected, &list, 5),
                gdk::Key::Page_Up => move_selection(&state, &selected, &list, -5),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);

    refresh(&state, &selected, &list, &config.query, config.lines);
    timing::mark("rows filled");

    // Hold the height of a full result list whatever is actually in it, so
    // the panel keeps its size and the input stays put instead of drifting up
    // the screen as a query narrows things down. Measured from a real row so
    // it follows the stylesheet rather than a number repeated here.
    if let Some(row) = list.row_at_index(0) {
        let (_, natural, _, _) = row.measure(gtk::Orientation::Vertical, -1);
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
    if !config.query.is_empty() {
        entry.set_text(&config.query);
        entry.set_position(-1);
    }
}

/// Collect every desktop entry the user is allowed to see, plus the
/// configured AppImages.
fn load_items(usage: &Usage, extras: &HashMap<String, Extra>, appimages: &[AppImageConfig]) -> Vec<Item> {
    let mut items: Vec<Item> = gio::AppInfo::all()
        .into_iter()
        .filter(|info| info.should_show())
        .filter_map(|info| {
            let id = info.id()?.to_string();
            let name = info.name().to_string();
            let extra = extras.get(&id);
            let generic = extra.and_then(|e| e.generic_name.clone());
            let comment = info.description().map(|s| s.to_string()).filter(|s| !s.is_empty());

            let mut fields = vec![(name.to_lowercase(), 1.0)];
            if let Some(generic) = &generic {
                fields.push((generic.to_lowercase(), 0.65));
            }
            if let Some(exec) = info.executable().file_name() {
                fields.push((exec.to_string_lossy().to_lowercase(), 0.6));
            }
            if let Some(keywords) = extra.map(|e| &e.keywords).filter(|k| !k.is_empty()) {
                fields.push((keywords.to_lowercase(), 0.45));
            }
            if let Some(comment) = &comment {
                fields.push((comment.to_lowercase(), 0.35));
            }

            let (count, last_used) = usage.get(&id);
            Some(Item { target: Target::Desktop(info), id, name, fields, count, last_used })
        })
        .collect();

    for appimage in appimages {
        let id = format!("appimage:{}", appimage.path.display());
        let name = appimage.name.clone();
        let mut fields = vec![(name.to_lowercase(), 1.0)];
        if let Some(stem) = appimage.path.file_stem().and_then(|s| s.to_str()) {
            fields.push((stem.to_lowercase(), 0.6));
        }
        let (count, last_used) = usage.get(&id);
        items.push(Item {
            target: Target::AppImage(appimage.clone()),
            id,
            name,
            fields,
            count,
            last_used,
        });
    }

    items.sort_by_key(|item| item.name.to_lowercase());
    items
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

fn parse_desktop_file(path: &Path) -> Option<Extra> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut extra = Extra::default();
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
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

/// Rebuild the visible rows for `query`.
fn refresh(
    state: &Rc<RefCell<State>>,
    selected: &Selected,
    list: &gtk::ListBox,
    query: &str,
    lines: usize,
) {
    let mut state = state.borrow_mut();

    let needle = query.trim().to_lowercase();
    let needle_chars: Vec<char> = needle.chars().collect();

    let mut ranked: Vec<(usize, i32)> = if needle.is_empty() {
        // No query: pure popularity order, ties broken by recency then name.
        (0..state.items.len()).map(|i| (i, 0)).collect()
    } else {
        state
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let best = item
                    .fields
                    .iter()
                    .filter_map(|(text, weight)| {
                        matcher::score(&needle, &needle_chars, text)
                            .map(|s| (s as f32 * weight) as i32)
                    })
                    .max()?;
                Some((i, best + popularity_bonus(item.count)))
            })
            .collect()
    };

    if needle.is_empty() {
        ranked.sort_by(|&(a, _), &(b, _)| {
            let (a, b) = (&state.items[a], &state.items[b]);
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_used.cmp(&a.last_used))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    } else {
        ranked.sort_by(|&(ai, asc), &(bi, bsc)| {
            let (a, b) = (&state.items[ai], &state.items[bi]);
            bsc.cmp(&asc)
                .then_with(|| b.count.cmp(&a.count))
                .then_with(|| a.name.len().cmp(&b.name.len()))
        });
    }

    state.shown = ranked.into_iter().take(lines).map(|(i, _)| i).collect();
    selected.set(0);

    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for &index in &state.shown {
        list.append(&build_row(&state.items[index]));
    }
    if let Some(row) = list.row_at_index(0) {
        list.select_row(Some(&row));
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

fn build_row(item: &Item) -> gtk::ListBoxRow {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .css_classes(["row-body"])
        .build();

    let icon = match &item.target {
        Target::Desktop(info) => match info.icon() {
            Some(gicon) => gtk::Image::from_gicon(&gicon),
            None => gtk::Image::from_icon_name("application-x-executable"),
        },
        Target::AppImage(app) => match &app.icon {
            Some(name) => gtk::Image::from_icon_name(name),
            None => gtk::Image::from_icon_name("application-x-executable"),
        },
    };
    icon.set_pixel_size(40);
    row.append(&icon);

    row.append(&label(&item.name, "name"));

    gtk::ListBoxRow::builder().child(&row).css_classes(["result"]).build()
}

/// `max_width_chars(1)` lets the label shrink below its natural size, which is
/// what makes ellipsizing actually kick in inside a fixed width panel.
fn label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
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
    delta: i32,
) {
    let len = state.borrow().shown.len();
    if len == 0 {
        return;
    }
    // Wrap around, like fuzzel does.
    let next = (selected.get() as i32 + delta).rem_euclid(len as i32);
    selected.set(next as usize);
    if let Some(row) = list.row_at_index(next) {
        list.select_row(Some(&row));
    }
}

fn activate(state: &Rc<RefCell<State>>, position: usize, window: &gtk::Window) {
    let target = {
        let state = state.borrow();
        state
            .shown
            .get(position)
            .map(|&index| (state.items[index].target.clone(), state.items[index].id.clone()))
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
    if launched {
        state.borrow_mut().usage.record(&id);
    }
    window.close();
}
