#!/bin/sh
set -eu

REPO="${GAMBI_REPO:-victorarias/gambi}"
VERSION=""
BIN_DIR=""

usage() {
  cat <<'EOF'
Install gambi from GitHub releases.

Usage:
  install.sh [--version <tag>] [--bin-dir <path>] [--repo <owner/name>]

Examples:
  curl -fsSL https://raw.githubusercontent.com/victorarias/gambi/main/install.sh | sh
  curl -fsSL https://raw.githubusercontent.com/victorarias/gambi/main/install.sh | sh -s -- --version v0.1.0
  curl -fsSL https://raw.githubusercontent.com/victorarias/gambi/main/install.sh | sh -s -- --bin-dir "$HOME/.local/bin"
EOF
}

fail() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

download_file() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$out" "$url"
  else
    fail "either curl or wget is required"
  fi
}

download_stdout() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$url"
  else
    fail "either curl or wget is required"
  fi
}

calc_sha256() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required"
  fi
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || fail "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --bin-dir)
      [ "$#" -ge 2 ] || fail "--bin-dir requires a value"
      BIN_DIR="$2"
      shift 2
      ;;
    --repo)
      [ "$#" -ge 2 ] || fail "--repo requires a value"
      REPO="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

need_cmd uname
need_cmd tar
need_cmd awk
need_cmd mktemp
need_cmd install

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin) os_target="apple-darwin" ;;
  Linux) os_target="unknown-linux-gnu" ;;
  *) fail "unsupported operating system: $os (supported: macOS, Linux)" ;;
esac

case "$arch" in
  x86_64|amd64) arch_target="x86_64" ;;
  arm64|aarch64) arch_target="aarch64" ;;
  *) fail "unsupported architecture: $arch (supported: x86_64, arm64/aarch64)" ;;
esac

target="${arch_target}-${os_target}"

if [ -z "$VERSION" ]; then
  api_url="https://api.github.com/repos/${REPO}/releases/latest"
  VERSION="$(download_stdout "$api_url" | awk -F '"' '/"tag_name"[[:space:]]*:/ {print $4; exit}')"
  [ -n "$VERSION" ] || fail "failed to resolve latest release tag from ${api_url}"
fi

asset="gambi-${target}.tar.gz"
base_url="https://github.com/${REPO}/releases/download/${VERSION}"

tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t gambi-installer)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT INT TERM

download_file "${base_url}/${asset}" "${tmpdir}/${asset}"
download_file "${base_url}/checksums.txt" "${tmpdir}/checksums.txt"

expected_sha="$(awk -v f="$asset" '$2 == f {print $1; exit}' "${tmpdir}/checksums.txt")"
[ -n "$expected_sha" ] || fail "checksum entry not found for ${asset}"

actual_sha="$(calc_sha256 "${tmpdir}/${asset}")"
[ "$expected_sha" = "$actual_sha" ] || fail "checksum mismatch for ${asset}"

tar -xzf "${tmpdir}/${asset}" -C "$tmpdir"
[ -f "${tmpdir}/gambi" ] || fail "release archive does not contain gambi binary"

if [ -z "$BIN_DIR" ]; then
  if [ -w "/usr/local/bin" ]; then
    BIN_DIR="/usr/local/bin"
  else
    BIN_DIR="${HOME}/.local/bin"
  fi
fi

mkdir -p "$BIN_DIR"

if [ -w "$BIN_DIR" ]; then
  install -m 755 "${tmpdir}/gambi" "${BIN_DIR}/gambi"
else
  command -v sudo >/dev/null 2>&1 || fail "no write access to ${BIN_DIR} and sudo is unavailable"
  sudo install -m 755 "${tmpdir}/gambi" "${BIN_DIR}/gambi"
fi

echo "gambi ${VERSION} installed to ${BIN_DIR}/gambi"
if ! command -v gambi >/dev/null 2>&1; then
  echo "Add ${BIN_DIR} to PATH to run gambi directly."
fi
