# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Wayland application launcher (GTK4 + `wlr-layer-shell`) in the spirit of fuzzel: type, fuzzy
match, hit Enter. Single binary, no daemon. It is judged on latency between keybind and first
frame, so most of the architecture exists to make startup work overlap rather than queue up.

## Commands

```bash
cargo build --release        # optimized build (lto, codegen-units=1, panic=abort — see Cargo.toml)
cargo test                    # unit tests: matcher, ranking, CLI, config, blur image ops, usage store, launch env
cargo test <name>             # run a single test by name/substring
./install.sh                  # build + install to ~/.local/bin (checks for gtk4/gtk4-layer-shell deps)
LAUNCHR_TIMING=1 cargo run --release -- -b   # print the startup timing breakdown to stderr
```

`.github/workflows/ci.yml` gates every push and pull request on `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` and
`cargo build --release --locked`, so a new warning or an unformatted line fails CI — run
`cargo fmt` and clippy before pushing. `rustfmt.toml` sets `use_small_heuristics = "Max"`.

Edition 2024, `rust-version = "1.88"` (let chains, `slice::as_chunks`). In this edition
`std::env::set_var`/`remove_var` are `unsafe`; the only callers are `LaunchEnv::pin` and
`take_activation_token`, both `unsafe fn`s called from one `unsafe` block at the top of `main` —
keep it that way (see "Startup sequencing").

Manual testing requires a wlroots-based Wayland compositor (river, sway, Hyprland, Wayfire) since
the app uses `wlr-layer-shell` for the window and `wlr-screencopy-v1` for the blur backdrop — it
cannot be exercised under X11. A nested headless wlroots compositor is enough, and keeps the
test off the real desktop (see the orientation note below).

## Architecture

Everything lives flat under `src/`, no submodule tree:

| File | Role |
| --- | --- |
| `src/main.rs` | startup sequence only: env pinning, which work runs on which thread, the backdrop wait |
| `src/cli.rs` | `parse_args` and the `--help` text — the last `Config` layer |
| `src/config.rs` | `Config` (the merged settings) and `~/.config/launchr.json` loading/validation: `Colors`, `AppImageConfig`/`Icon`, clamp ranges. The file is read as a `serde_json` map through `Fields`, one key at a time, so a mistyped value, a bad color or a broken `appimages` entry falls back on its own with a stderr warning, and keys left over at the end are reported as unknown. No serde derive |
| `src/apps.rs` | `Catalog::load` (usage store + GIO desktop entries + AppImages → `Entry`/`Target`), `Target::launch`, `LaunchEnv`, `exec_line` |
| `src/rank.rs` | `rank` — the whole ranking policy — plus `weigh`, `popularity_bonus`, `take_top` |
| `src/ui.rs` | layer-shell window, widget tree, stylesheet rendering, result rows (`Results`/`Row`), key handling, activation |
| `src/capture.rs` | standalone `wlr-screencopy-v1` client over its own Wayland connection (raw `wayland-client`, not GTK's). Also the protocol knowledge the blur needs: `channel_shifts` (8- and 10-bit `wl_shm` formats as bit offsets in a little-endian `u32`) and `orientation` (output transform + `y_invert` → `blur::Orientation`) |
| `src/blur.rs` | pure image ops: downscale + 3-pass separable box blur approximating Gaussian, with the output's orientation folded in (mirrors in the downscale's reads, the transposition in the blur's) |
| `src/matcher.rs` | fuzzy subsequence scorer plus `fold` (lowercase + strip diacritics), no GTK dependency |
| `src/usage.rs` | launch-count store (`$XDG_DATA_HOME/launchr/usage.tsv`), used both for ranking and to record hits |
| `src/timing.rs` | `LAUNCHR_TIMING=1` marks |
| `src/style.css` | Tokyo Night stylesheet *template* — `__TOKEN__` placeholders substituted by `render_style()` in `ui.rs` from `Config::colors`/`font_size`/`dim`, then loaded as a single provider above `GTK_STYLE_PROVIDER_PRIORITY_USER` so it cannot be overridden by the user's GTK theme — one provider, not two, because each one added to a display invalidates the whole style cascade again. It must style every widget on its own: the built-in theme underneath is GTK's blank `Empty` (see below) |

`matcher.rs`, `blur.rs`, `usage.rs`, `config.rs`, `cli.rs`, `rank.rs`, `apps.rs`'s `LaunchEnv`
and `exec_line`, and `ui.rs`'s `Step` are unit-tested in isolation (`#[cfg(test)]` blocks at the
bottom of each file). The two policy decisions are kept GTK-free so they are testable:
`parse_args` takes the argument iterator as a parameter, and `rank` works on `Entry`, which holds
no GIO or GTK type. File-backed code takes its path as a parameter (`config::load_from`,
`Usage::load_from`) so tests never touch the user's files — and never call `set_var`, which
races every other test thread reading the environment. `capture.rs`'s Wayland exchange,
`Catalog::load` and the widget wiring are only exercised by running the app.

The orientation table in `capture::orientation` is pinned by
`blur::tests::every_output_transform_comes_out_upright`, whose fixture is what a headless wlroots
output actually handed over in each of the eight transforms (grim saw the picture upright every
time). To re-derive it: run river headless (`WLR_BACKENDS=headless WLR_RENDERER=pixman river`),
show an asymmetric image with `swaybg`, `wlr-randr --output HEADLESS-1 --transform …`, and dump
the raw frames from a throwaway test that calls `capture::capture_outputs`. The same nested
compositor is how to run the launcher itself headless (`WAYLAND_DISPLAY=wayland-N launchr -b`,
then `grim`).

### Config precedence

`Config` (in `config.rs`) is built in three layers, each overriding the last: built-in defaults
(`Config::default()`) → `~/.config/launchr.json` (`Config::with_file_overrides`) → CLI flags
(`cli::parse_args`, which takes the already-merged config as its starting point; long options
also accept `--flag=value`). Only `lines`, `dim`, and `blur` are settable from both the file and
a flag; `colors`, `font_size`, and `appimages` are file-only, and `width`, `placeholder`, and
`query` are flag-only. AppImages get folded into the same `Entry`/`Target` list as `.desktop`
entries in `Catalog::load`, so search, ranking, and `usage.tsv` popularity tracking treat them
identically — the only branch point is `Target::Desktop` vs. `Target::AppImage` in
`Row::show` (icon lookup: an AppImage's `Icon::Name` or `Icon::File`) and `Target::launch`, where
an AppImage gets a `gio::AppInfo` made on the spot with `create_from_commandline` from
`exec_line` (shell-quoted, `%` doubled — GIO expands field codes, then shell-splits) and is
started with `launch_uris_as_manager_with_fds` (`gio-unix`'s `v2_58` feature) so its stdio is
`/dev/null`, as it always was. Both go through the same `AppLaunchContext`, so both get
`LaunchEnv`'s restored environment and an xdg-activation token.

### Startup sequencing (the part that matters for perf-sensitive changes)

The core design constraint: worker threads start *before* `gtk::init`, and are joined only
right before the window is mapped — the one ordering invariant that must hold is "screenshot
before map" (the launcher must never capture itself).

- `GSK_RENDERER` and `GTK_THEME` are pinned by `LaunchEnv::pin` in the `unsafe` block at the
  top of `main`, before any thread exists. Not a style choice: glibc's `setenv` can reallocate
  `environ`, and both workers below read the environment (`XDG_DATA_DIRS` and the locale
  variables for the catalog, `WAYLAND_DISPLAY` for the capture), so writing to it once they are
  running is a data race. Keep any new `set_var` above the spawns — add it to the `pin` list so
  launched apps get the original value back. `gtk::init` itself writes too, so `main` does
  those writes first: it takes `XDG_ACTIVATION_TOKEN`/`DESKTOP_STARTUP_ID` out of the
  environment (GDK's own `unsetenv` then finds nothing; the token is handed back through
  `gdk::Toplevel::set_startup_id` on map) and calls `setlocale` with `gtk::disable_setlocale()`.
- A worker thread builds the whole item list (`Catalog::load`: `usage.tsv`, `AppInfo::all()`,
  folding, sort) while GTK starts, so none of it is on the main thread. Desktop files are parsed
  once, by GIO: `GenericName=`/`Keywords=` come from the same objects through `gio-unix`'s
  `DesktopAppInfo`, translated and untranslated both (`with_untranslated`), so a French desktop
  still finds Firefox by "browser". `DesktopApp` carries the one `unsafe impl Send`, for the
  single handoff from worker to main thread; nothing else may send GIO objects across threads.
  The worker is spawned once the arguments have parsed, so `--help` and a rejected flag do not
  pay for it.
- With `-b`/`--blur`, a second worker thread opens its own Wayland connection, captures every
  output via `capture::capture_outputs` (`wlr-screencopy-v1`), and blurs each frame
  (`blur::blurred`) — downscaled to a longest edge of 480px, then scaled back up by the renderer
  at draw time. It reports back over a channel rather than a join, so the main thread can time
  out and settle for a dim-only window. Two nested budgets, and they have to keep composing: the
  capture deadline (`BACKDROP_WAIT / CAPTURE_SHARE`, 300ms) covers every compositor exchange
  after the registry is up — neither `blocking_dispatch` nor `roundtrip` has a timeout of its
  own, so `capture.rs` waits through `wait_until`/`dispatch_before` (`poll` with a deadline)
  instead of either — while `BACKDROP_WAIT` (900ms) is the outer bound and must stay larger than
  that deadline plus the connection setup outside it plus the blur, or a capture that succeeded
  gets thrown away after paying for the whole framebuffer readback. Both budgets are counted
  from the moment the worker is spawned (`CaptureJob::give_up_at`), not from when the main
  thread starts waiting. A frame whose offered buffer types include no format `channel_shifts`
  can read is given up on at once (`State::finish`) instead of waiting out the deadline, and
  connector names are only waited for from outputs bound at `wl_output` version 4 or later.
  The blur runs its three horizontal passes back to back, transposes once for the vertical
  ones, and divides by reciprocal multiply (`blur::Divisor`); keep it and the downscale
  rounding, not truncating, or the passes darken the backdrop. Pixels are read as little-endian
  `u32`s and shifted (`Frame::channels`), which costs ~0.15ms over byte loads on a 2560x1440
  frame and is what lets 10-bit outputs be captured at all.
- Multi-output setups pair each captured frame with the output the compositor placed the layer
  surface on, matched by connector name (`DP-1`, `HDMI-A-1`, etc.).
- Screencopy frames arrive in buffer orientation, not as the user sees the output. The
  `wl_output` geometry's transform and the frame's `y_invert` flag become a
  `blur::Orientation`; `downscale` reads blocks mirrored and `box_blur` skips its second
  transpose when a quarter turn is needed, so turning the image upright adds no pass.
- Three env-driven defaults exist purely to avoid paying for things a short-lived process never
  needs, each overridable: no `GtkApplication` (skips session-bus registration), `GSK_RENDERER=cairo`
  (skips GL/Vulkan context setup — GL and Vulkan both measured slower to first paint),
  `GTK_THEME=Empty` (the blank theme GTK ships for its own tests: 28 bytes of CSS instead of
  ~150KB for the default one, and a smaller cascade to match every widget against; were it ever
  dropped, GTK falls back to the default theme, same look, old cost). `~/.config/gtk-4.0/gtk.css`
  is still loaded by `GtkSettings` whatever `GTK_THEME` says, and a theme installer's symlink
  there can be the largest single cost left (~30ms for a 238KB one).

If you touch `main.rs`'s startup path, capture, or blur, re-check timing with
`LAUNCHR_TIMING=1` rather than assuming a change is free — see README "Startup" for the expected
breakdown and reference numbers. The `first frame` mark is the end of the first paint (a
frame clock `after-paint` handler), not a tick callback: ticks run before layout and paint, and
the first paint is some 35ms of software rendering on its own.

### Ranking

`rank` in `rank.rs` is the whole of the policy. Popularity (launch count from `usage.tsv`) and
fuzzy match score are combined, not layered: an empty query shows most-used-first; with a
query, popularity is a log-scaled bonus on top of the fuzzy score (`popularity_bonus`) so
frequent use can win ties but can't bury a much better name match. Only `lines` rows are ever
displayed, so `rank` takes them with a partial sort; entries arrive from `Catalog::load` in
folded name order, which makes the index itself the alphabetical tie break and the comparison a
total order.

Searchable text goes through `matcher::fold` (lowercase plus diacritic stripping, so `cafe`
finds `Café`) once at load time, and each field keeps its `Vec<char>` alongside — `matcher` then
allocates nothing per keystroke. `matcher::score` runs the linear greedy scan first, as both the
exact subsequence test and the reject path for the bulk of the list, and only what survives pays
for `best_chain`, which searches for the best *placement* of the needle rather than the first
occurrence of each character, so a contiguous run further along a name is not missed. Haystacks
longer than `DP_LIMIT` keep the greedy score.

Field weights go through `weigh`, not a plain multiply: `score` goes negative for a poor match in
a long field, and multiplying a negative by a weight below one makes it *larger*, which would put
`Comment=` above `Keywords=` exactly when both matched badly.

Result rows are built once (`Results::new`) and refilled in place by `Results::refresh`, with
unused rows hidden rather than removed, so a row's index and its position in `State::shown` stay
the same number. Each `Row` remembers the entry index it is showing and skips the refill when a
keystroke did not move it, which is what keeps the icon theme lookup out of the per-keystroke
path.

`Comment=`/`Keywords=`/`GenericName=` are folded into the searchable text at
lower weight than the app name (`apps::weight`) but are never displayed.

## Release process

Releases are published from `.github/workflows/release.yml`, triggered by tagging `vX.Y.Z` (or a
manual `workflow_dispatch` with an existing tag, which re-uploads its assets): it checks the tag
against `Cargo.toml`'s version, runs `cargo test --release --locked`, builds `--release
--locked`, packages `target/release/launchr` + `README.md` into a
`launchr-<version>-x86_64-unknown-linux-gnu.tar.gz` with a `.sha256`, and publishes both as a
GitHub release. Bump `version` in `Cargo.toml` (and the `V=` line in the README) before tagging.

Both workflows install GTK through the composite action `.github/actions/setup-gtk`, which
carries the meson/ninja fallback for `gtk4-layer-shell` (only packaged from Ubuntu 24.10 on).
`.github/dependabot.yml` opens weekly update PRs for crates (gtk-rs and wayland grouped) and
for the actions themselves.
