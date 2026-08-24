#!/usr/bin/env sh
# Install keke by downloading a prebuilt binary from the latest GitHub release.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/milisp/keke-agent/main/scripts/install.sh | sh
#
# Env vars:
#   KEKE_VERSION      release tag to install (default: latest)
#   KEKE_INSTALL_DIR  where to place the binary (default: $HOME/.local/bin)

set -eu

REPO="milisp/keke-agent"
INSTALL_DIR="${KEKE_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${KEKE_VERSION:-latest}"

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin) plat="apple-darwin" ;;
    Linux) plat="unknown-linux-gnu" ;;
    *) die "unsupported OS: $os (build from source instead: cargo install --path crates/keke-cli)" ;;
  esac
  case "$arch" in
    x86_64|amd64) cpu="x86_64" ;;
    arm64|aarch64) cpu="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
  esac
  printf '%s-%s\n' "$cpu" "$plat"
}

main() {
  need_cmd curl
  need_cmd tar
  need_cmd mkdir

  target="$(detect_target)"

  if [ "$VERSION" = "latest" ]; then
    api_url="https://api.github.com/repos/${REPO}/releases/latest"
  else
    api_url="https://api.github.com/repos/${REPO}/releases/tags/${VERSION}"
  fi

  tag="$(curl -fsSL "$api_url" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
  [ -n "${tag:-}" ] || die "could not resolve a release tag from $api_url"

  asset="keke-${tag}-${target}.tar.gz"
  url="https://github.com/${REPO}/releases/download/${tag}/${asset}"

  say "downloading ${asset} (${tag})..."
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  if ! curl -fsSL -o "$tmp/$asset" "$url"; then
    die "no prebuilt binary for ${target} at ${tag} (${url}). Build from source instead: cargo install --path crates/keke-cli"
  fi

  tar -xzf "$tmp/$asset" -C "$tmp"
  mkdir -p "$INSTALL_DIR"
  mv "$tmp/keke" "$INSTALL_DIR/keke"
  chmod +x "$INSTALL_DIR/keke"

  say "installed keke ${tag} to ${INSTALL_DIR}/keke"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "note: add ${INSTALL_DIR} to your PATH to run 'keke' directly" ;;
  esac
}

main "$@"
