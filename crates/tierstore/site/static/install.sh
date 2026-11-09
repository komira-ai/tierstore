#!/bin/sh
# Installs tierstore from the release files on tierstore.dev:
#
#   curl -fsSL https://tierstore.dev/install.sh | sh
#   curl -fsSL https://tierstore.dev/install.sh | sh -s -- --version X.Y.Z --prefix "$HOME/bin"
#
# What it does: finds the platform (Linux x86_64, Linux aarch64, macOS arm64), reads the newest version from
# <base-url>/releases/latest.json (or takes --version), downloads that platform's tarball and the version's SHA256SUMS
# from <base-url>/releases/<version>/, refuses unless the tarball's SHA-256 is the one SHA256SUMS lists, and places the
# binary `tierstore` in the prefix. It does not start the service, write a configuration or create an account.
#
# The digest check catches a damaged or truncated download. It does not prove who built the file, because SHA256SUMS
# comes from the same address as the tarball; the provenance bundles of a release do (https://tierstore.dev/docs/).
#
# Flags (it reads no environment variable of its own):
#   --version X.Y.Z   install this version instead of the newest
#   --prefix DIR      the directory for the binary (default: $HOME/.local/bin)
#   --system          the prefix is /usr/local/bin unless --prefix says otherwise, and sudo is used when the
#                     directory is not writable by this user (and only then)
#   --base-url URL    where releases/ is (default: https://tierstore.dev); for tests and mirrors
#   --help            this text
#
# POSIX sh. It needs curl, tar, install, mktemp and one of sha256sum and shasum.

set -eu

BASE_URL=https://tierstore.dev
VERSION=
PREFIX=
SYSTEM=0

say() { printf '%s\n' "$*"; }
die() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
usage: install.sh [--version X.Y.Z] [--prefix DIR] [--system] [--base-url URL]

  --version X.Y.Z   install this version instead of the newest
  --prefix DIR      the directory for the binary (default: $HOME/.local/bin)
  --system          prefix /usr/local/bin, with sudo when that directory is not writable by you
  --base-url URL    where releases/ is (default: https://tierstore.dev)
  --help            this text
USAGE
}

while [ $# -gt 0 ]; do
  case $1 in
    --version | --prefix | --base-url)
      [ $# -ge 2 ] || die "$1 needs a value"
      case $1 in
        --version) VERSION=$2 ;;
        --prefix) PREFIX=$2 ;;
        --base-url) BASE_URL=$2 ;;
      esac
      shift 2
      ;;
    --system)
      SYSTEM=1
      shift
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *) die "unknown argument $1 (try --help)" ;;
  esac
done

# --- platform -------------------------------------------------------------------------------------------------------

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS/$ARCH" in
  Linux/x86_64 | Linux/amd64) PLATFORM=linux-x86_64 ;;
  Linux/aarch64 | Linux/arm64) PLATFORM=linux-aarch64 ;;
  Darwin/arm64) PLATFORM=macos-arm64 ;;
  *)
    die "no tierstore build for $OS $ARCH. Builds exist for Linux x86_64, Linux aarch64 and macOS arm64 (Apple silicon).
Other ways to install: 'brew install komira-ai/tap/tierstore' (macOS arm64, Linux), or 'sudo apt install tierstore'
on Debian and Ubuntu (amd64, arm64) from https://packages.komira.ai/apt; https://tierstore.dev/docs/ has the rest."
    ;;
esac

if [ -z "$PREFIX" ]; then
  if [ "$SYSTEM" = 1 ]; then
    PREFIX=/usr/local/bin
  else
    PREFIX=${HOME:?HOME is not set; give --prefix}/.local/bin
  fi
fi

command -v curl >/dev/null 2>&1 || die "curl is needed and was not found"
command -v tar >/dev/null 2>&1 || die "tar is needed and was not found"
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  die "sha256sum or shasum is needed to check the download and neither was found"
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/tierstore-install.XXXXXX") || die "cannot make a temporary directory"
cleanup() {
  rm -f "$TMP/tierstore" "$TMP/SHA256SUMS" "$TMP/latest.json" "$TMP"/*.tar.gz
  rmdir "$TMP" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

fetch() { curl -fsSL --retry 2 -o "$2" "$1" || die "download failed: $1"; }

# --- version --------------------------------------------------------------------------------------------------------

if [ -z "$VERSION" ]; then
  fetch "$BASE_URL/releases/latest.json" "$TMP/latest.json"
  VERSION=$(tr -d '\n' <"$TMP/latest.json" | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  [ -n "$VERSION" ] || die "$BASE_URL/releases/latest.json names no version"
fi
case $VERSION in
  [0-9]*) ;;
  *) die "'$VERSION' is not a version (expected X.Y.Z)" ;;
esac
case $VERSION in
  *[!0-9A-Za-z.+-]*) die "'$VERSION' is not a version (expected X.Y.Z)" ;;
esac

# --- download and check ---------------------------------------------------------------------------------------------

FILE=tierstore-$VERSION-$PLATFORM.tar.gz
DIR_URL=$BASE_URL/releases/$VERSION
say "Installing tierstore $VERSION for $PLATFORM"
fetch "$DIR_URL/SHA256SUMS" "$TMP/SHA256SUMS"
fetch "$DIR_URL/$FILE" "$TMP/$FILE"

WANT=$(awk -v f="$FILE" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' "$TMP/SHA256SUMS")
[ -n "$WANT" ] || die "$DIR_URL/SHA256SUMS lists no $FILE; nothing was installed"
GOT=$(sha256_of "$TMP/$FILE")
if [ "$GOT" != "$WANT" ]; then
  die "checksum mismatch for $FILE: SHA256SUMS says $WANT, the download is $GOT; nothing was installed"
fi
say "Checked $FILE: sha256 $GOT"

tar -xzf "$TMP/$FILE" -C "$TMP" tierstore || die "$FILE holds no 'tierstore' file; nothing was installed"

# --- place ----------------------------------------------------------------------------------------------------------

SUDO=
writable() { [ -d "$1" ] && [ -w "$1" ]; }
if ! writable "$PREFIX"; then
  if [ ! -e "$PREFIX" ] && mkdir -p "$PREFIX" 2>/dev/null; then
    :
  elif [ "$SYSTEM" = 1 ] && [ "$(id -u)" != 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "$PREFIX is not writable and sudo was not found; run as root or give --prefix"
    SUDO=sudo
    say "$PREFIX is not writable by you; using sudo to place the binary"
  else
    die "$PREFIX is not writable; give another --prefix, or --system to use sudo"
  fi
fi
$SUDO mkdir -p "$PREFIX"
$SUDO install -m 0755 "$TMP/tierstore" "$PREFIX/tierstore" || die "cannot place $PREFIX/tierstore"

say "Installed $PREFIX/tierstore ($("$PREFIX/tierstore" --version 2>/dev/null || say 'version not reported'))."
case ":${PATH:-}:" in
  *":$PREFIX:"*) ;;
  *) say "Note: $PREFIX is not on your PATH; add it, or run $PREFIX/tierstore by its path." ;;
esac
say "tierstore is not running: this script starts nothing and writes no configuration."
say "To start it: write a configuration file, then enable the service (or run 'tierstore --config FILE')."
say "Linux hosts: packaging/ in the repository has a systemd unit and an example file."
say "Guide: https://tierstore.dev/docs/"
