#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "Usage: $0 <tag> <checksums.txt> [repo]" >&2
  exit 1
fi

tag="$1"
checksums_file="$2"
repo="${3:-victorarias/gambi}"
version="${tag#v}"

if [[ ! -f "$checksums_file" ]]; then
  echo "checksums file not found: $checksums_file" >&2
  exit 1
fi

lookup_sha() {
  local asset="$1"
  local sha
  sha="$(awk -v f="$asset" '$2 == f { print $1; exit }' "$checksums_file")"
  if [[ -z "$sha" ]]; then
    echo "missing checksum for asset: $asset" >&2
    exit 1
  fi
  printf "%s" "$sha"
}

sha_x86_64_macos="$(lookup_sha "gambi-x86_64-apple-darwin.tar.gz")"
sha_aarch64_macos="$(lookup_sha "gambi-aarch64-apple-darwin.tar.gz")"
sha_x86_64_linux="$(lookup_sha "gambi-x86_64-unknown-linux-gnu.tar.gz")"
sha_aarch64_linux="$(lookup_sha "gambi-aarch64-unknown-linux-gnu.tar.gz")"

cat <<EOF
class Gambi < Formula
  desc "Local MCP aggregator with execute-first workflow and OAuth refresh"
  homepage "https://github.com/${repo}"
  version "${version}"
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/${repo}/releases/download/${tag}/gambi-aarch64-apple-darwin.tar.gz"
      sha256 "${sha_aarch64_macos}"
    else
      url "https://github.com/${repo}/releases/download/${tag}/gambi-x86_64-apple-darwin.tar.gz"
      sha256 "${sha_x86_64_macos}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/${repo}/releases/download/${tag}/gambi-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "${sha_aarch64_linux}"
    else
      url "https://github.com/${repo}/releases/download/${tag}/gambi-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${sha_x86_64_linux}"
    end
  end

  def install
    bin.install "gambi"
    prefix.install "LICENSE"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/gambi --version")
  end
end
EOF
