#!/usr/bin/env bash
# Build and install launchr. Works on any distro with a Rust toolchain and the
# GTK4 development files; nothing here is specific to one compositor.
# Re-exec under bash when started with a POSIX shell (sh install.sh): the
# script needs pipefail, which dash and ash do not have.
if [ -z "${BASH_VERSION:-}" ]; then
    exec bash "$0" "$@"
fi
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BUILD=1
UNINSTALL=0

say() { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m::\033[0m %s\n' "$*" >&2; }
die() {
	printf '\033[1;31m::\033[0m %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<EOF
Build and install launchr.

Usage: ./install.sh [options]

  --prefix DIR   install under DIR (default \$PREFIX, else ~/.local)
                 the binary lands in DIR/bin/launchr
  --no-build     skip cargo build, install the existing target/release/launchr
  --uninstall    remove the installed binary
  -h, --help     this text

Examples:
  ./install.sh                      # ~/.local/bin/launchr
  sudo ./install.sh --prefix /usr/local
  PREFIX=/opt/launchr ./install.sh
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--prefix)
		[ $# -ge 2 ] || die "--prefix needs a directory"
		PREFIX="$2"
		shift 2
		;;
	--prefix=*)
		PREFIX="${1#*=}"
		shift
		;;
	--no-build)
		BUILD=0
		shift
		;;
	--uninstall)
		UNINSTALL=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		usage >&2
		exit 1
		;;
	esac
done

cd "$(dirname "$(readlink -f "$0")")"
BINDIR="$PREFIX/bin"
TARGET="$BINDIR/launchr"

if [ "$UNINSTALL" -eq 1 ]; then
	if [ -e "$TARGET" ]; then
		rm -f "$TARGET" && say "removed $TARGET"
	else
		say "nothing to remove at $TARGET"
	fi
	DATA="${XDG_DATA_HOME:-$HOME/.local/share}/launchr"
	if [ -d "$DATA" ]; then
		say "launch counts kept in $DATA (delete it by hand to reset)"
	fi
	exit 0
fi

# --- dependencies ---------------------------------------------------------
missing=()
for tool in cargo pkg-config; do
	command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done

pkgs=()
if command -v pkg-config >/dev/null 2>&1; then
	pkg-config --exists gtk4 || pkgs+=("gtk4")
	pkg-config --exists gtk4-layer-shell-0 || pkgs+=("gtk4-layer-shell")
fi

if [ ${#missing[@]} -gt 0 ] || [ ${#pkgs[@]} -gt 0 ]; then
	warn "missing: ${missing[*]-} ${pkgs[*]-}"
	echo
	echo "Install them first, e.g.:"
	echo "  Arch          sudo pacman -S --needed rust gtk4 gtk4-layer-shell pkgconf"
	echo "  Fedora        sudo dnf install cargo gtk4-devel gtk4-layer-shell-devel pkgconf-pkg-config"
	echo "  Debian 13+    sudo apt install cargo libgtk-4-dev libgtk4-layer-shell-dev pkg-config"
	echo "  openSUSE      sudo zypper install cargo gtk4-devel gtk4-layer-shell-devel pkg-config"
	echo "  Alpine        doas apk add cargo gtk4.0-dev gtk4-layer-shell-dev pkgconf"
	echo "  Void          sudo xbps-install cargo gtk4-devel gtk4-layer-shell-devel pkg-config"
	echo
	die "dependencies missing"
fi

# --- build ----------------------------------------------------------------
if [ "$BUILD" -eq 1 ]; then
	say "building (release)"
	cargo build --release
fi
[ -x target/release/launchr ] || die "target/release/launchr not found — drop --no-build"

# --- install --------------------------------------------------------------
if ! mkdir -p "$BINDIR" 2>/dev/null; then
	die "cannot create $BINDIR — rerun with sudo, or pick a --prefix you own"
fi
if [ ! -w "$BINDIR" ]; then
	die "$BINDIR is not writable — rerun with sudo, or pick a --prefix you own"
fi

install -Dm755 target/release/launchr "$TARGET"
say "installed $TARGET"

case ":$PATH:" in
*":$BINDIR:"*) ;;
*) warn "$BINDIR is not on your PATH — bind the launcher by its full path, or add the directory to PATH" ;;
esac

cat <<EOF

Bind it in your compositor. Note that a keybind is usually run through a bare
/bin/sh, which reads no shell rc file, so if $BINDIR is only added to PATH by
your .bashrc or .zshrc, spell the path out in full.

  river     riverctl map normal Super D spawn $TARGET
  sway/i3   bindsym \$mod+d exec $TARGET
  niri      binds { Mod+D { spawn "$TARGET"; } }
  Hyprland  bind = SUPER, D, exec, $TARGET

No compositor blur rule is needed: launchr screenshots the output through
wlr-screencopy-v1 and blurs its own backdrop.
EOF
