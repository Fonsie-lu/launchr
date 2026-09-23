//! The layer-shell window: widget tree, stylesheet, key handling, and the
//! glue between the result rows and the ranking.

use crate::apps::{Catalog, Entry, LaunchEnv, Target};
use crate::blur;
use crate::config::{self, Colors, Config};
use crate::rank;
use crate::timing;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const NAMESPACE: &str = "launchr";
const STYLE: &str = include_str!("style.css");

/// Shown for anything without an icon of its own.
const FALLBACK_ICON: &str = "application-x-executable";

/// One output's blurred screenshot, tagged with the connector it came from.
pub struct Backdrop {
    connector: Option<String>,
    texture: gdk::MemoryTexture,
}

impl Backdrop {
    pub fn new(connector: Option<String>, image: blur::Image) -> Self {
        let stride = image.width as usize * 3;
        let texture = gdk::MemoryTexture::new(
            image.width as i32,
            image.height as i32,
            gdk::MemoryFormat::R8g8b8,
            // Handed over rather than copied: the pixels are not needed again.
            &glib::Bytes::from_owned(image.rgb),
            stride,
        );
        Self { connector, texture }
    }
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

pub fn load_css(config: &Config) {
    let Some(display) = gdk::Display::default() else { return };
    // Above PRIORITY_USER, not PRIORITY_APPLICATION: a user GTK theme in
    // ~/.config/gtk-4.0/gtk.css sits at 800 and would otherwise repaint the
    // list and its selection in the theme's own colours.
    //
    // One provider, not two: the dim used to arrive as a second provider
    // layered on top, and every provider added to a display invalidates the
    // whole style cascade again. It is a token in the template instead.
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&render_style(&config.colors, config.font_size, config.dim));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
    );
}

/// What the result rows are showing. Only ever borrowed for the length of one
/// handler.
struct State {
    catalog: Catalog,
    /// Indices into `catalog.entries`, in the order currently displayed.
    shown: Vec<usize>,
}

/// One result row. The widgets are built once and refilled as the query
/// changes, so a keystroke costs a label and an icon update instead of tearing
/// down and rebuilding the whole list.
struct Row {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    name: gtk::Label,
    /// Index into `Catalog::entries` currently displayed. Entry indices are
    /// fixed for the life of the process, so this is enough to skip a refill —
    /// and the icon lookup inside it — for a row a keystroke did not move.
    shown: Cell<Option<usize>>,
}

impl Row {
    fn new() -> Self {
        // Never empty, so an unfilled row measures the same as a filled one —
        // see `Results::reserve_full_height`.
        let icon = gtk::Image::from_icon_name(FALLBACK_ICON);
        icon.set_pixel_size(40);
        // `max_width_chars(1)` lets the label shrink below its natural size,
        // which is what makes ellipsizing actually kick in inside a fixed
        // width panel.
        let name = gtk::Label::builder()
            .css_classes(["name"])
            .xalign(0.0)
            .hexpand(true)
            .halign(gtk::Align::Fill)
            .max_width_chars(1)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(14)
            .css_classes(["row-body"])
            .build();
        body.append(&icon);
        body.append(&name);

        let row = gtk::ListBoxRow::builder().child(&body).css_classes(["result"]).build();
        Row { row, icon, name, shown: Cell::new(None) }
    }

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

/// The result list: a fixed set of rows, one per displayable line, and which
/// of them is highlighted. Rows beyond the current result count are hidden
/// rather than removed, which keeps a row's index and its position in
/// `State::shown` the same number.
struct Results {
    list: gtk::ListBox,
    rows: Vec<Row>,
    /// Highlighted row. Kept outside `State` because GTK fires `row-selected`
    /// synchronously from inside code that already holds the `State` borrow.
    selected: Cell<usize>,
}

impl Results {
    fn new(lines: usize) -> Self {
        let list = gtk::ListBox::builder()
            .css_classes(["results"])
            .selection_mode(gtk::SelectionMode::Browse)
            .show_separators(false)
            .activate_on_single_click(true)
            .build();
        let rows: Vec<Row> = (0..lines).map(|_| Row::new()).collect();
        for row in &rows {
            list.append(&row.row);
        }
        Results { list, rows, selected: Cell::new(0) }
    }

    /// Rank for `query` and refill the rows. `rows` was built with one row per
    /// displayable line, so it is the row count as well as the widgets — there
    /// is no separate `lines` to fall out of step with it.
    fn refresh(&self, state: &RefCell<State>, query: &str) {
        let mut state = state.borrow_mut();
        let State { catalog, shown } = &mut *state;
        *shown = rank::rank(&catalog.entries, query, self.rows.len());
        self.selected.set(0);

        for (position, row) in self.rows.iter().enumerate() {
            match shown.get(position) {
                Some(&index) => row.show(index, &catalog.entries[index], &catalog.targets[index]),
                None => row.hide(),
            }
        }
        if shown.is_empty() {
            self.list.select_row(None::<&gtk::ListBoxRow>);
        } else {
            self.list.select_row(Some(&self.rows[0].row));
        }
    }

    fn move_selection(&self, state: &RefCell<State>, delta: i32) {
        let len = state.borrow().shown.len();
        if len == 0 {
            return;
        }
        // Wrap around, like fuzzel does.
        let next = (self.selected.get() as i32 + delta).rem_euclid(len as i32) as usize;
        self.selected.set(next);
        if let Some(row) = self.rows.get(next) {
            self.list.select_row(Some(&row.row));
        }
    }

    /// Hold the height of a full result list whatever is actually in it, so
    /// the panel keeps its size and the input stays put instead of drifting up
    /// the screen as a query narrows things down. Measured from a real row so
    /// it follows the stylesheet rather than a number repeated here — and
    /// before the first `refresh`, which may hide every row, and a hidden
    /// widget measures as nothing.
    fn reserve_full_height(&self) {
        if let Some(first) = self.rows.first() {
            let (_, natural, _, _) = first.row.measure(gtk::Orientation::Vertical, -1);
            if natural > 0 {
                self.list.set_height_request(natural * self.rows.len() as i32);
            }
        }
    }
}

/// Launch the entry at `position` in the displayed list and record the hit.
fn activate(state: &RefCell<State>, env: &LaunchEnv, position: usize, window: &gtk::Window) {
    // Cloned out so no borrow is held while GIO launches.
    let picked = {
        let state = state.borrow();
        state.shown.get(position).map(|&index| {
            (state.catalog.targets[index].clone(), state.catalog.entries[index].id.clone())
        })
    };
    let Some((target, id)) = picked else { return };

    let context = WidgetExt::display(window).app_launch_context();
    let launched = match target.launch(env, context.upcast_ref()) {
        Ok(()) => true,
        Err(message) => {
            eprintln!("launchr: failed to launch {message}");
            false
        }
    };
    // Close first: recording the launch flushes the store to disk, and there
    // is no reason for the launcher to stay on screen while that happens.
    window.close();
    if launched {
        state.borrow_mut().catalog.usage.record(&id);
    }
}

/// Pick the screenshot of the output the compositor put `window` on. Only
/// known once the surface exists, so this runs on map.
fn match_backdrop(window: &gtk::Window, picture: &gtk::Picture, backdrops: &[Backdrop]) {
    // A single output skips the round trip that delivers connector names, so
    // unnamed shots are the one case where whatever was captured is by
    // definition the right one. Everything else has to be matched, including
    // a lone shot left over from a multi-output capture where the other
    // output failed.
    if backdrops.iter().all(|b| b.connector.is_none()) {
        return;
    }
    let Some(surface) = window.surface() else { return };
    let Some(monitor) = WidgetExt::display(window).monitor_at_surface(&surface) else {
        return;
    };
    // No name from GDK is not the same as no match: without one there is
    // nothing to pair against, so leave the first shot in place rather than
    // blanking a backdrop that may well be correct.
    let Some(connector) = monitor.connector() else { return };
    match backdrops.iter().find(|b| b.connector.as_deref() == Some(connector.as_str())) {
        Some(found) => picture.set_paintable(Some(&found.texture)),
        // This output is named and nothing was captured for it. No backdrop
        // beats another monitor's desktop behind the panel.
        None => picture.set_paintable(None::<&gdk::Texture>),
    }
}

/// Mark the end of the first paint. Not a tick callback: ticks run in the
/// frame clock's update phase, before that frame's layout and paint, which are
/// most of what the first frame costs.
fn mark_first_frame(window: &gtk::Window) {
    window.connect_realize(|window| {
        let Some(clock) = window.frame_clock() else { return };
        let painted = Cell::new(false);
        clock.connect_after_paint(move |_| {
            if !painted.replace(true) {
                timing::mark("first frame");
            }
        });
    });
}

pub fn build_ui(
    main_loop: &glib::MainLoop,
    config: &Config,
    backdrops: Vec<Backdrop>,
    catalog: Catalog,
    env: LaunchEnv,
    activation_token: Option<String>,
) {
    let state = Rc::new(RefCell::new(State { catalog, shown: Vec::new() }));
    let env = Rc::new(env);

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

    let results = Rc::new(Results::new(config.lines));

    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["panel"])
        .width_request(config.width)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    panel.append(&entry);
    panel.append(&results.list);

    let dim = gtk::Box::builder()
        .css_classes(["dim"])
        .hexpand(true)
        .vexpand(true)
        .build();
    dim.append(&panel);

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Fill)
        .can_shrink(true)
        .build();
    if let Some(first) = backdrops.first() {
        picture.set_paintable(Some(&first.texture));
    }

    let stack = gtk::Overlay::builder().child(&picture).build();
    stack.add_overlay(&dim);
    window.set_child(Some(&stack));

    window.connect_map(move |window| match_backdrop(window, &picture, &backdrops));
    // `main` took the token out of the environment before GDK could; this is
    // where GDK would have used it, once the surface exists.
    if let Some(token) = activation_token {
        window.connect_map(move |window| {
            if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
                toplevel.set_startup_id(&token);
            }
        });
    }

    // Weak: the list owns this handler, and `Results` owns the list.
    results.list.connect_row_selected({
        let results = Rc::downgrade(&results);
        move |_, row| {
            if let (Some(row), Some(results)) = (row, results.upgrade()) {
                results.selected.set(row.index().max(0) as usize);
            }
        }
    });

    results.list.connect_row_activated({
        let state = state.clone();
        let env = env.clone();
        let window = window.clone();
        move |_, row| activate(&state, &env, row.index().max(0) as usize, &window)
    });

    entry.connect_activate({
        let state = state.clone();
        let env = env.clone();
        let results = results.clone();
        let window = window.clone();
        move |_| activate(&state, &env, results.selected.get(), &window)
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
        let results = results.clone();
        let window = window.clone();
        move |_, key, _, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            let step = match key {
                gdk::Key::Escape => {
                    window.close();
                    return glib::Propagation::Stop;
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    activate(&state, &env, results.selected.get(), &window);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Down | gdk::Key::Tab => 1,
                gdk::Key::n | gdk::Key::j if ctrl => 1,
                gdk::Key::Up | gdk::Key::ISO_Left_Tab => -1,
                gdk::Key::p | gdk::Key::k if ctrl => -1,
                gdk::Key::Page_Down => 5,
                gdk::Key::Page_Up => -5,
                _ => return glib::Propagation::Proceed,
            };
            results.move_selection(&state, step);
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);

    results.reserve_full_height();
    // Fill the rows for the starting query before wiring up `changed`: the
    // entry already carries `config.query`, so connecting first would have the
    // signal repeat this same pass for nothing.
    results.refresh(&state, &config.query);
    timing::mark("rows filled");
    entry.connect_changed({
        let state = state.clone();
        let results = results.clone();
        move |entry| results.refresh(&state, &entry.text())
    });

    if timing::enabled() {
        mark_first_frame(&window);
    }
    window.present();
    timing::mark("presented");
    entry.grab_focus();
    entry.set_position(-1);
}
