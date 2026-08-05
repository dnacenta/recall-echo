#!/usr/bin/env sh
# recall-echo installer — downloads the latest release binary for this platform.
#
#   curl -fsSL https://raw.githubusercontent.com/dnacenta/recall-echo/main/install.sh | sh
#
# Honest about what it does: fetches one tarball from GitHub Releases, extracts
# one binary, and puts it on your PATH. No daemons started, no config written,
# nothing touched outside the install directory. Run `recall-echo init` after.
set -eu

REPO="dnacenta/recall-echo"
BIN="recall-echo"
INSTALL_DIR="${RECALL_ECHO_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }
say() { printf '  %s\n' "$1"; }

# --- platform -------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) err "unsupported OS: $os (recall-echo ships Linux and macOS binaries; build from source with \`cargo install recall-echo --locked\`)" ;;
esac

case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) err "unsupported architecture: $arch" ;;
esac

target="${arch_part}-${os_part}"
asset="${BIN}-${target}.tar.gz"

# --- version --------------------------------------------------------------
version="${RECALL_ECHO_VERSION:-}"
if [ -z "$version" ]; then
    version="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
    [ -n "$version" ] || err "could not determine the latest release (set RECALL_ECHO_VERSION to pin one)"
fi

url="https://github.com/${REPO}/releases/download/${version}/${asset}"

printf '\n\033[1mrecall-echo\033[0m %s — %s\n\n' "$version" "$target"

# --- download -------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset"
curl -fsSL "$url" -o "$tmp/$asset" || err "download failed: $url"

say "extracting"
tar -xzf "$tmp/$asset" -C "$tmp" || err "could not extract $asset"

extracted="$(find "$tmp" -type f -name "$BIN" -perm -u+x | head -1)"
[ -n "$extracted" ] || err "no $BIN binary inside $asset"

# --- install --------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
mv "$extracted" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"
say "installed to $INSTALL_DIR/$BIN"

# --- PATH check -----------------------------------------------------------
case ":$PATH:" in
    *":$INSTALL_DIR:"*) on_path=1 ;;
    *) on_path=0 ;;
esac

printf '\n'
if [ "$on_path" -eq 0 ]; then
    printf '  \033[33m%s is not on your PATH.\033[0m Add this to your shell profile:\n\n' "$INSTALL_DIR"
    printf '    export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
fi

printf '  Next:  \033[1m%s init\033[0m\n\n' "$BIN"
