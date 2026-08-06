#!/bin/sh
# wherry installer — downloads the latest release binary for your platform.
#
#   curl -fsSL https://raw.githubusercontent.com/magicabdel/wherry/main/install.sh | sh
#   wget -qO-  https://raw.githubusercontent.com/magicabdel/wherry/main/install.sh | sh
#
# Override the install directory with WHERRY_INSTALL_DIR (default: ~/.local/bin).
set -eu

REPO="magicabdel/wherry"
INSTALL_DIR="${WHERRY_INSTALL_DIR:-$HOME/.local/bin}"

err() {
  echo "error: $1" >&2
  exit 1
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    err "need curl or wget to download wherry"
  fi
}

main() {
  case "$(uname -s)" in
    Linux) os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) err "unsupported OS: $(uname -s)" ;;
  esac

  case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) err "unsupported architecture: $(uname -m)" ;;
  esac

  target="${arch}-${os}"
  url="https://github.com/${REPO}/releases/latest/download/wherry-${target}.tar.gz"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  echo "Downloading wherry (${target})..."
  download "$url" "$tmp/wherry.tar.gz"
  tar -xzf "$tmp/wherry.tar.gz" -C "$tmp"

  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$tmp/wherry" "$INSTALL_DIR/wherry"
  echo "Installed wherry to $INSTALL_DIR/wherry"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "note: $INSTALL_DIR is not on your PATH — add it to your shell profile." ;;
  esac
}

main "$@"
