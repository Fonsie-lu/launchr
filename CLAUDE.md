# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Wayland application launcher (GTK4 + `wlr-layer-shell`) in the spirit of fuzzel: type, fuzzy
match, hit Enter. Single binary, no daemon. It is judged on latency between keybind and first
frame, so most of the architecture exists to make startup work overlap rather than queue up.

## Commands

```bash
cargo build --release        # optimized build (lto, codegen-units=1, panic=abort — see Cargo.toml)
cargo test                    # unit tests: matcher ordering, blur image ops, usage store round trip
cargo test <name>             # run a single test by name/substring
./install.sh                  # build + install to ~/.local/bin (checks for gtk4/gtk4-layer-shell deps)
LAUNCHR_TIMING=1 cargo run --release -- -b   # print the startup timing breakdown to stderr
```

`.github/workflows/ci.yml` gates every push and pull request on
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` and
`cargo build --release --locked`, so a new warning fails CI — run clippy before pushing.

Manual testing requires a wlroots-based Wayland session (river, sway, Hyprland, Wayfire) since
the app uses `wlr-layer-shell` for the window and `wlr-screencopy-v1` for the blur backdrop —
it cannot be exercised meaningfully in a headless/X11 environment.

## Architecture

Everything lives under `src/`, six files, no submodule tree:

| File | Role |
| --- | --- |
| `src/main.rs` | entry point: CLI parsing, layer-shell window/widget tree, key handling, ranking/search glue, `.desktop` file scan |
| `src/capture.rs` | standalone `wlr-screencopy-v1` client over its own Wayland connection (raw `wayland-client`, not GTK's) |
| `src/blur.rs` | pure image ops: downscale + 3-pass separable box blur approximating Gaussian |
| `src/matcher.rs` | fuzzy subsequence scorer plus `fold` (lowercase + strip diacritics), no GTK dependency |
| `src/usage.rs` | launch-count store (`$XDG_DATA_HOME/launchr/usage.tsv`), used both for ranking and to record hits |
| `src/config.rs` | `~/.config/launchr.json` loading/validation: `Colors`, `AppImageConfig`, per-field fallback to defaults with a stderr warning on anything invalid |
| `src/style.css` | Tokyo Night stylesheet *template* — `__TOKEN__` placeholders substituted by `render_style()` in `main.rs` from `Config::colors`/`font_size`/`dim`, then loaded as a single provider above `GTK_STYLE_PROVIDER_PRIORITY_USER` so it cannot be overridden by the user's GTK theme — one provider, not two, because each one added to a display invalidates the whole style cascade again |

`matcher.rs`, `blur.rs`, `usage.rs`, and `config.rs` are self-contained and unit-tested in
isolation (`#[cfg(test)]` blocks at the bottom of each file). `main.rs` keeps its two policy
decisions GTK-free so they are testable too: `parse_args` takes the argument iterator as a
parameter, and `rank` works on `Entry`, which holds no GIO or GTK type — the tests at the
bottom of `main.rs` cover both. `capture.rs` and the widget wiring are only exercised by
running the app.

### Config precedence

`Config` (in `main.rs`) is built in three layers, each overriding the last: built-in defaults
(`Config::default()`) → `~/.config/launchr.json` (`Config::with_file_overrides`, backed by
`config::load()`) → CLI flags (`parse_args`, which now takes the already-merged config as its
starting point instead of `Config::default()`). Only `lines`, `dim`, and `blur` are settable from
both the file and a flag; `colors`, `font_size`, and `appimages` are file-only, and `width`,
`placeholder`, and `query` are flag-only. AppImages get folded into the same `Item`/`Target` list
as `.desktop` entries in `load_items`, so search, ranking, and `usage.tsv` popularity tracking
treat them identically — the only branch point is `Target::Desktop` vs. `Target::AppImage` in
`build_row` (icon lookup) and `activate` (`AppInfo::launch` vs. a plain `std::process::Command`).

### Startup sequencing (the part that matters for perf-sensitive changes)

The core design constraint: worker threads start *before* `gtk::init`, and are joined only
right before the window is mapped — the one ordering invariant that must hold is "screenshot
before map" (the launcher must never capture itself).

- `GSK_RENDERER` and `GTK_THEME` are pinned as the first statements of `main`, before any
  thread exists. Not a style choice: glibc's `setenv` can reallocate `environ`, and both workers
  below read the environment (`XDG_DATA_DIRS` for the scan, `WAYLAND_DISPLAY` for the capture),
  so writing to it once they are running is a data race. Keep any new `set_var` above the
  spawns.
- A worker thread reads `.desktop` files (`scan_desktop_files` in `main.rs`) while GTK starts.
  It is spawned once the arguments have parsed, so `--help` and a rejected flag do not pay for
  a scan that is thrown away.
- With `-b`/`--blur`, a second worker thread opens its own Wayland connection, captures every
  output via `capture::capture_outputs` (`wlr-screencopy-v1`), and blurs each frame
  (`blur::blurred`) — downscaled to a longest edge of 480px, then scaled back up by the renderer
  at draw time. It reports back over a channel rather than a join, so the main thread can time
  out and settle for a dim-only window. Two nested budgets, and they have to keep composing:
  `capture::DEADLINE` (300ms) covers every compositor exchange after the registry is up —
  neither `blocking_dispatch` nor `roundtrip` has a timeout of its own, so `capture.rs` waits
  through `wait_until`/`dispatch_before` (`poll` with a deadline) instead of either — while
  `BACKDROP_WAIT` (900ms) is the outer bound and must stay larger than `DEADLINE` plus the
  connection setup outside it plus the blur, or a capture that succeeded gets thrown away after
  paying for the whole framebuffer readback.
- The item list is built *before* the backdrop is collected, so `AppInfo::all()` — which
  reparses every desktop file the scan thread already read, and is the main thread's largest
  remaining cost — overlaps the capture rather than queueing behind it.
- Multi-output setups pair each captured frame with the output the compositor placed the layer
  surface on, matched by connector name (`DP-1`, `HDMI-A-1`, etc.).
- Three env-driven defaults exist purely to avoid paying for things a short-lived process never
  needs, each overridable: no `GtkApplication` (skips session-bus registration), `GSK_RENDERER=cairo`
  (skips GL/Vulkan context setup), `GTK_THEME=Adwaita` (skips parsing a large user theme that gets
  painted over anyway regardless).

If you touch `main.rs`'s startup path, capture, or blur, re-check timing with
`LAUNCHR_TIMING=1` rather than assuming a change is free — see README "Startup" for the expected
breakdown and reference numbers.

### Ranking

`rank` in `main.rs` is the whole of the policy. Popularity (launch count from `usage.tsv`) and
fuzzy match score are combined, not layered: an empty query shows most-used-first; with a
query, popularity is a log-scaled bonus on top of the fuzzy score (`popularity_bonus`) so
frequent use can win ties but can't bury a much better name match. Only `lines` rows are ever
displayed, so `rank` takes them with a partial sort; entries arrive from `load_items` in folded
name order, which makes the index itself the alphabetical tie break and the comparison a total
order.

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

Result rows are built once (`build_rows`) and refilled in place by `refresh`, with unused rows
hidden rather than removed, so a row's index and its position in `State::shown` stay the same
number. Each `Row` remembers the entry index it is showing and skips the refill when a keystroke
did not move it, which is what keeps the icon theme lookup out of the per-keystroke path.

`Comment=`/`Keywords=`/`GenericName=` are folded into the searchable text at
lower weight than the app name but are never displayed — GIO's Rust bindings don't expose
`GDesktopAppInfo`, so those fields are read directly from the `.desktop` files in
`application_dirs()`/`parse_desktop_file()` rather than via `AppInfo`.

## Release process

Releases are published from `.github/workflows/release.yml`, triggered by tagging `vX.Y.Z` (or a
manual `workflow_dispatch` with an existing tag): it runs `cargo test --release`, builds
`--release --locked`, packages `target/release/launchr` + `README.md` into a
`launchr-<version>-x86_64-unknown-linux-gnu.tar.gz` with a `.sha256`, and publishes both as a
GitHub release.

**That workflow is described here but does not exist in the repository** — only `ci.yml` does.
Write it before cutting a release. `gtk4-layer-shell` is only packaged from Ubuntu 24.10 on, so
it needs the same meson/ninja fallback `ci.yml` already carries.
