#!/bin/sh
# Install babysit by downloading the latest release binary.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/yusukeshib/babysit/main/install.sh | sh
#
# Environment overrides:
#   BABYSIT_INSTALL_DIR  install location (default: $HOME/.local/bin)
#   BABYSIT_VERSION      version tag like v0.2.4 (default: latest release)

set -eu

REPO="yusukeshib/babysit"
INSTALL_DIR="${BABYSIT_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Darwin) os="darwin" ;;
  Linux)  os="linux" ;;
  *) echo "babysit: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64)   arch="x86_64" ;;
  arm64|aarch64)  arch="aarch64" ;;
  *) echo "babysit: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset="babysit-${arch}-${os}"

version="${BABYSIT_VERSION:-}"
if [ -z "$version" ]; then
  echo "Fetching latest release..."
  version=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n 1)
  if [ -z "$version" ]; then
    echo "babysit: could not determine latest version" >&2
    exit 1
  fi
fi

base_url="https://github.com/${REPO}/releases/download/${version}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading babysit ${version} (${arch}-${os})..."
curl -fsSL "${base_url}/${asset}"        -o "${tmp}/${asset}"
curl -fsSL "${base_url}/${asset}.sha256" -o "${tmp}/${asset}.sha256"

expected=$(awk '{print $1}' "${tmp}/${asset}.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "${tmp}/${asset}" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')
else
  echo "babysit: no sha256sum / shasum available, skipping checksum verification" >&2
  actual="$expected"
fi

if [ "$actual" != "$expected" ]; then
  echo "babysit: checksum mismatch (expected $expected, got $actual)" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
chmod +x "${tmp}/${asset}"
mv "${tmp}/${asset}" "${INSTALL_DIR}/babysit"

echo "Installed babysit ${version} -> ${INSTALL_DIR}/babysit"

case ":$PATH:" in
  *:"$INSTALL_DIR":*) ;;
  *)
    echo
    echo "Note: ${INSTALL_DIR} is not in your PATH. Add it to your shell profile, e.g.:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
