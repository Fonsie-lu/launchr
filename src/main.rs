//! launchr — a Wayland application launcher.
//!
//! Renders as a fullscreen `wlr-layer-shell` overlay. The backdrop is a
//! screenshot the launcher takes of the output just before mapping, blurred in
//! process and dimmed with a scrim, so it looks identical on river, sway,
//! Hyprland or any other wlroots compositor without asking any of them for a
//! blur rule. Results are ranked by fuzzy match quality boosted by how often
//! the application has been launched from here before.
//!
//! This file is the startup sequence and nothing else: it decides what runs
//! on which thread and in what order, which is most of what the launcher's
//! latency comes down to.

mod apps;
mod blur;
mod capture;
mod cli;
mod config;
mod matcher;
mod rank;
mod timing;
mod ui;
mod usage;

use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const APP_ID: &str = "ch.bithawk.launchr";

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

/// What the capture worker sends back: one blurred image per output.
type Blurred = Vec<(Option<String>, blur::Image)>;

fn main() -> glib::ExitCode {
    timing::init();
    timing::mark("main");

    // Both overrides happen here, before a single thread exists, and not down
    // beside `gtk::init` where they belong logically. glibc's `setenv` can
    // reallocate `environ`, and both workers below read the environment — the
    // catalog for XDG_DATA_HOME/XDG_DATA_DIRS, the capture for WAYLAND_DISPLAY
    // — so writing to it once they are running is a data race, not a tidiness
    // question. An explicit value in the environment wins over either, and
    // apps launched from here get the original value (or its absence) back —
    // see `LaunchEnv`.
    //
    // GSK_RENDERER: GL/Vulkan context creation dominates the first frame of a
    // process that only lives for a few seconds; the cairo renderer draws this
    // UI just as well and starts sooner.
    //
    // GTK_THEME: `style.css` styles every widget the launcher has, so any
    // theme underneath is parsed only to be painted over. `Empty` is the blank
    // theme GTK ships for its own tests: 28 bytes of CSS against some 150KB
    // for the default one, and nothing for the style cascade to match every
    // widget against. Should a GTK release ever drop it, GTK falls back to
    // the default theme — the same look, just the old startup cost.
    let launch_env = apps::LaunchEnv::pin(&[("GSK_RENDERER", "cairo"), ("GTK_THEME", "Empty")]);

    // `gtk::init` writes to the environment and the locale as well, and both
    // are just as unsafe with the workers running. It unsets the activation
    // token it was started with, so taking that out here first leaves it
    // nothing to remove; the token is handed back to GDK once the window
    // exists. And it would call `setlocale(LC_ALL, "")`, which is done here
    // instead, with GTK told to leave it alone.
    let activation_token = take_activation_token();
    gtk::disable_setlocale();
    // SAFETY: no other thread exists yet, and the argument is a valid C
    // string; the returned locale name is not used.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
    }

    let mut config =
        match cli::parse_args(config::Config::default().with_file_overrides(), std::env::args().skip(1))
        {
            Ok(Some(config)) => config,
            Ok(None) => return glib::ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("launchr: {message}");
                return glib::ExitCode::FAILURE;
            }
        };

    // The item list is GIO and plain file reads, none of it GTK, so it is
    // built on a worker while the main thread brings the toolkit up. Spawned
    // once the arguments are known to be good: `--help` and a rejected flag
    // return before any window exists, and should not pay for a scan nobody
    // reads.
    let catalog_job = {
        let appimages = std::mem::take(&mut config.appimages);
        std::thread::spawn(move || {
            let catalog = apps::Catalog::load(appimages);
            timing::mark("items loaded");
            catalog
        })
    };

    // Grab the backdrop before GTK maps anything, otherwise the launcher ends
    // up in its own screenshot. See `spawn_capture`.
    let capture_job = (config.blur > 0).then(|| spawn_capture(config.blur));

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

    ui::load_css(&config);
    timing::mark("css loaded");

    // `panic = "abort"` is set for the release profile only, so this fallback
    // is unreachable there but live in dev and test builds — where an empty
    // list beats taking the launcher down with the worker.
    let catalog = catalog_job.join().unwrap_or_default();
    timing::mark("items joined");

    let backdrops = capture_job.map_or_else(Vec::new, collect_backdrops);

    let main_loop = glib::MainLoop::new(None, false);
    ui::build_ui(&main_loop, &config, backdrops, catalog, launch_env, activation_token);
    main_loop.run();
    timing::mark("exit");
    glib::ExitCode::SUCCESS
}

/// Remove the activation token the launcher was started with from the
/// environment, the way GDK would, and keep it — `XDG_ACTIVATION_TOKEN`, as
/// the xdg-activation protocol names it, else the older `DESKTOP_STARTUP_ID`.
/// An app launched from here must not inherit it either way; it gets a fresh
/// one from the launch context.
fn take_activation_token() -> Option<String> {
    let token = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|token| !token.is_empty());
    std::env::remove_var("XDG_ACTIVATION_TOKEN");
    std::env::remove_var("DESKTOP_STARTUP_ID");
    token
}

/// Capture every output and blur it on a worker thread. Capture talks to the
/// compositor over its own Wayland connection and the blur is pure CPU work,
/// so neither needs the main thread; the pixels are collected again before the
/// window is presented, which is the point the "screenshot before map"
/// ordering actually has to hold. The result comes back over a channel rather
/// than a join so the wait can time out — see `BACKDROP_WAIT`.
fn spawn_capture(radius: u32) -> Receiver<Blurred> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let deadline = Instant::now() + BACKDROP_WAIT / CAPTURE_SHARE;
    std::thread::spawn(move || {
        let frames = capture::capture_outputs(deadline);
        timing::mark("captured");
        // Sequential across outputs. Blurring them in parallel would need
        // `Mapping` to claim `Sync`, and a hand-written unsafe impl on the
        // mmap wrapper is not worth the few ms it would save on the second
        // monitor.
        let images: Blurred = frames
            .iter()
            .filter_map(|frame| Some((frame.connector.clone(), blur::blurred(frame, radius)?)))
            .collect();
        timing::mark("blurred");
        let _ = sender.send(images);
    });
    receiver
}

/// Wait for the capture worker, or give up on it at `BACKDROP_WAIT` and show
/// the scrim alone.
fn collect_backdrops(receiver: Receiver<Blurred>) -> Vec<ui::Backdrop> {
    let images = receiver.recv_timeout(BACKDROP_WAIT).unwrap_or_else(|_| {
        eprintln!("launchr: backdrop was not ready in time, dimming only");
        Vec::new()
    });
    timing::mark("backdrop collected");
    let backdrops = images
        .into_iter()
        .map(|(connector, image)| ui::Backdrop::new(connector, image))
        .collect();
    timing::mark("textures");
    backdrops
}
