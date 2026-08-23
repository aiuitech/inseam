#!/bin/sh
# Installs the prebuilt inseam CLI — no Rust toolchain required.
#
#   curl -fsSL https://docs.inseam.io/install.sh | sh
#
# Detects OS/arch (macOS and Linux, arm64 and x86_64), downloads the release
# tarball from GitHub, verifies its sha256 against the release's
# checksums.txt, and installs the binary. Overrides:
#
#   INSEAM_VERSION      release tag to install (default: latest)
#   INSEAM_INSTALL_DIR  destination directory (default: ~/.local/bin)
#
# This script is served from docs.inseam.io and lives at the repo root;
# release artifacts are built by .github/workflows/release.yml and become
# visible to `latest` only once a maintainer promotes the release
# (`cargo xtask release promote`), so this script never sees an unsigned one.
set -eu

REPO="aiuitech/inseam"
INSTALL_DIR="${INSEAM_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${INSEAM_VERSION:-latest}"

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-gnu" ;;
  *)
    echo "error: unsupported OS '$(uname -s)' — build from source instead:" >&2
    echo "  https://docs.inseam.io/get-started/" >&2
    exit 1
    ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64)  arch="x86_64" ;;
  *)
    echo "error: unsupported architecture '$(uname -m)' — build from source instead:" >&2
    echo "  https://docs.inseam.io/get-started/" >&2
    exit 1
    ;;
esac
asset="inseam-${arch}-${os}.tar.gz"

if [ "$VERSION" = "latest" ]; then
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading ${asset} (${VERSION})..."
curl -fsSL "${base}/${asset}" -o "${tmp}/${asset}"
curl -fsSL "${base}/checksums.txt" -o "${tmp}/checksums.txt"

# Verify against the checksums published with the release.
(
  cd "$tmp"
  line="$(grep "  ${asset}\$" checksums.txt)" || {
    echo "error: ${asset} missing from checksums.txt" >&2
    exit 1
  }
  if command -v sha256sum >/dev/null 2>&1; then
    echo "$line" | sha256sum -c - >/dev/null
  else
    echo "$line" | shasum -a 256 -c - >/dev/null
  fi
)

tar -xzf "${tmp}/${asset}" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "${tmp}/inseam" "${INSTALL_DIR}/inseam"

echo "installed inseam to ${INSTALL_DIR}/inseam"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "note: ${INSTALL_DIR} is not on your PATH; add it with e.g.:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
echo "get started: https://docs.inseam.io/get-started/"
