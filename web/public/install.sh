#!/bin/sh
# Installs SwarmOps: `swarmops` and `so` from a GitHub release, checked against the release's
# SHA256SUMS, into ~/.local/bin (/usr/local/bin as root) or the directory --dir names. Run it
# again to update; --uninstall removes the two binaries and nothing else.
#
#   curl -fsSL https://get.swarmops.cc/install.sh | bash
#   curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --version 0.1.0 --dir /opt/swarmops/bin
#   curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --uninstall
#
# POSIX sh, not Python or fish: it runs on users' machines through `curl … | bash`, and must
# also run where only sh is certain; bash, dash and busybox ash run it. It sends no telemetry;
# it writes only the install directory and a temporary directory, and edits no shell startup
# file. Every step is a function and the last line calls them, so a download cut short runs
# nothing.
# scripts/test-install.py tests it offline; site/install.sh is the copy get.swarmops.cc serves.

set -eu

# Where the releases are, and the pages the messages point to.
REPO_URL='https://github.com/KeiaiLab/SwarmOps'
RELEASES_URL="$REPO_URL/releases"
SOURCE_URL="$REPO_URL#install"
QUICK_START_URL="$REPO_URL#quick-start"
WEBSITE_URL='https://swarmops.cc'

# Test seams (--help): scripts/test-install.py points them at a fake release on 127.0.0.1.
DOWNLOAD_BASE=${SWARMOPS_DOWNLOAD_BASE:-$RELEASES_URL/download}
LATEST_URL=${SWARMOPS_LATEST_URL:-$RELEASES_URL/latest}

# The release binaries need glibc 2.34 or newer; before 2.39, ld.so prints one line about a
# weak symbol version at each of their starts (README, Install).
GLIBC_MIN=2.34
GLIBC_QUIET=2.39

# A release version, as the tags name it without their `v`: 0.1.0, 0.2.0-rc.1.
VERSION_RE='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

# What an archive installs, and how each line the installer prints on stderr starts.
BINARIES='swarmops so'
PREFIX='swarmops-install:'

NEWLINE='
'

# Tools answer in the C locale, which the parsing below expects; pattern ranges are ASCII.
export LC_ALL=C

# Prints a message on stderr.
say() {
  printf '%s %s\n' "$PREFIX" "$*" >&2
}

# Prints a message on stderr and stops with status 1.
die() {
  say "$*"
  exit 1
}

# Prints a usage error on stderr and stops with status 2.
usage_error() {
  say "$*; see --help."
  exit 2
}

usage() {
  cat <<EOF
Installs SwarmOps: swarmops and so, from a GitHub release, checked against its SHA256SUMS.

Usage: curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- [options]
       bash install.sh [options]

Options:
  --version X.Y.Z  The release to install. Default: SWARMOPS_VERSION, else the latest.
  --dir DIR        Where to put swarmops and so. Default: SWARMOPS_INSTALL_DIR, else
                   /usr/local/bin as root, else \$HOME/.local/bin.
  --uninstall      Remove swarmops and so from DIR. Stores are never touched.
  -h, --help       Print this help.

Environment:
  SWARMOPS_VERSION        As --version.
  SWARMOPS_INSTALL_DIR    As --dir.
  SWARMOPS_DOWNLOAD_BASE  Test seam: where release files are downloaded from. Default:
                          $RELEASES_URL/download
  SWARMOPS_LATEST_URL     Test seam: the URL that redirects to the latest release. Default:
                          $RELEASES_URL/latest
  Both seams take https:// URLs, or http://127.0.0.1 for a local test server.

Run it again to update: it replaces both binaries in place.

Linux only: x86_64 or aarch64, with glibc $GLIBC_MIN or newer. Needs curl or wget, tar, and
sha256sum or shasum. It writes only DIR and a temporary directory, and edits no shell startup
file; it needs root only for a DIR that only root can write.
EOF
}

# Reads the options; --version and --dir win over their environment variables.
parse_args() {
  opt_version=
  opt_dir=
  opt_uninstall=
  while [ "$#" -gt 0 ]; do
    case $1 in
      --version)
        need_value "$@"
        opt_version=$2
        shift 2
        ;;
      --version=*)
        opt_version=${1#*=}
        shift
        ;;
      --dir)
        need_value "$@"
        opt_dir=$2
        shift 2
        ;;
      --dir=*)
        opt_dir=${1#*=}
        shift
        ;;
      --uninstall)
        opt_uninstall=1
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage_error "Unknown option '$1'"
        ;;
    esac
  done
}

# Stops with a usage error when option $1 has no value after it.
need_value() {
  if [ "$#" -lt 2 ]; then
    usage_error "$1 needs a value"
  fi
}

# The install directory: --dir, else SWARMOPS_INSTALL_DIR, else /usr/local/bin for root (the
# FHS place for programs installed outside the package manager), else ~/.local/bin (XDG).
pick_dir() {
  dir=${opt_dir:-${SWARMOPS_INSTALL_DIR:-}}
  if [ -n "$dir" ]; then
    return 0
  fi
  if [ "$(id -u)" -eq 0 ]; then
    dir=/usr/local/bin
    return 0
  fi
  if [ -z "${HOME:-}" ]; then
    die "HOME is not set: name the install directory with --dir."
  fi
  dir=$HOME/.local/bin
}

# 1. The system: the releases hold Linux builds only.
check_os() {
  os=$(uname -s)
  if [ "$os" = Linux ]; then
    return 0
  fi
  say 'SwarmOps runs on Linux today; macOS and Windows are next.'
  die "This system is $os. The releases: $RELEASES_URL"
}

# 2. The target: the release build for this machine's architecture.
pick_target() {
  machine=$(uname -m)
  case $machine in
    x86_64 | amd64)
      target=x86_64-unknown-linux-gnu
      ;;
    aarch64 | arm64)
      target=aarch64-unknown-linux-gnu
      ;;
    *)
      die "No release build runs on $machine, only on x86_64 and aarch64." \
        "To build from source: $SOURCE_URL"
      ;;
  esac
}

# 3. The C library: glibc 2.34 or newer. `ldd --version` names it on its first line
# ("ldd (GNU libc) 2.39"), and musl's names musl; `getconf GNU_LIBC_VERSION` ("glibc 2.39")
# answers where ldd is missing.
check_libc() {
  glibc=

  # 1. ldd: musl is refused at once; glibc's version ends its first line.
  if command -v ldd >/dev/null 2>&1; then
    out=$(ldd --version 2>&1) || :
    case $out in
      *musl*)
        die "This system's C library is musl; the release binaries need glibc $GLIBC_MIN" \
          "or newer. To build from source: $SOURCE_URL"
        ;;
    esac
    glibc=$(printf '%s\n' "$out" |
      sed -n '1s/.* \([0-9][0-9]*\.[0-9][0-9]*\)[.0-9]*$/\1/p')
  fi

  # 2. getconf, when ldd gave no version.
  if [ -z "$glibc" ] && command -v getconf >/dev/null 2>&1; then
    out=$(getconf GNU_LIBC_VERSION 2>/dev/null) || :
    glibc=$(printf '%s\n' "$out" |
      sed -n 's/^glibc \([0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')
  fi

  # 3. The version against the floor, and the ld.so line that glibc before 2.39 prints.
  if [ -z "$glibc" ]; then
    die "Cannot tell this system's glibc version (ldd --version, getconf GNU_LIBC_VERSION);" \
      "the release binaries need glibc $GLIBC_MIN or newer. To build from source: $SOURCE_URL"
  fi
  if older "$glibc" "$GLIBC_MIN"; then
    die "This system has glibc $glibc; the release binaries need $GLIBC_MIN or newer." \
      "To build from source: $SOURCE_URL"
  fi
  if older "$glibc" "$GLIBC_QUIET"; then
    say "On glibc $glibc, each start of swarmops and so prints one ld.so line on stderr," \
      "\"weak version 'GLIBC_$GLIBC_QUIET' not found\"; exit codes, stdout and the MCP" \
      "protocol are unaffected."
  fi
}

# Succeeds when version $1 is older than version $2, both X.Y.
older() {
  if [ "${1%%.*}" -ne "${2%%.*}" ]; then
    [ "${1%%.*}" -lt "${2%%.*}" ]
    return
  fi
  [ "${1#*.}" -lt "${2#*.}" ]
}

# 4. The downloader: curl, else wget.
pick_downloader() {
  if command -v curl >/dev/null 2>&1; then
    downloader=curl
  elif command -v wget >/dev/null 2>&1; then
    downloader=wget
  else
    die "Neither curl nor wget is installed: install one, then run this again."
  fi
}

# The protocol curl may use for URL $1: HTTPS, or plain HTTP to 127.0.0.1, where the offline
# test serves its fake release; a URL with a user part could name another host, so it is
# refused there. wget gets the same check.
url_proto() {
  case $1 in
    https://*)
      proto='=https'
      ;;
    http://127.0.0.1[:/]*)
      case $1 in
        *@*)
          die "Refusing $1: a local test URL has no user part."
          ;;
      esac
      proto='=http'
      ;;
    *)
      die "Refusing $1: downloads use https:// only."
      ;;
  esac
}

# Downloads URL $1 into file $2.
fetch() {
  url_proto "$1"
  case $downloader in
    curl)
      curl -fsSL --proto "$proto" --tlsv1.2 -o "$2" "$1"
      ;;
    wget)
      wget -q -O "$2" "$1"
      ;;
  esac
}

# 5. The version: --version, else SWARMOPS_VERSION, else the latest release; a leading v is
# dropped (v0.1.0 is 0.1.0).
pick_version() {
  version=${opt_version:-${SWARMOPS_VERSION:-}}
  if [ -z "$version" ]; then
    latest_version
  fi
  version=${version#v}
  if ! valid_version "$version"; then
    die "'$version' is not a release version: X.Y.Z or X.Y.Z-pre, such as 0.1.0."
  fi
}

# The latest release's tag: the last part of the URL that releases/latest redirects to
# (…/releases/tag/v0.1.0). A redirect needs no API call, so no API rate limit applies.
latest_version() {
  url_proto "$LATEST_URL"
  final=
  case $downloader in
    curl)
      final=$(curl -fsSLI --proto "$proto" --tlsv1.2 -o /dev/null \
        -w '%{url_effective}' "$LATEST_URL") || final=
      ;;
    wget)
      final=$(wget_final "$LATEST_URL") || final=
      ;;
  esac
  case $final in
    */releases/tag/?*)
      version=${final##*/}
      ;;
    *)
      die "Cannot find the latest release: $LATEST_URL led to '$final'."
      ;;
  esac
}

# The URL that wget's redirects from $1 end at: the last Location header it prints (-S),
# or $1 itself.
wget_final() {
  headers=$(wget -q -S --spider "$1" 2>&1) || return 1
  location=$(printf '%s\n' "$headers" |
    tr -d '\r' |
    sed -n 's/^ *[Ll]ocation: *//p' |
    tail -n 1)
  printf '%s\n' "${location:-$1}"
}

# Succeeds when $1 is a release version: one line that VERSION_RE matches.
valid_version() {
  case $1 in
    *"$NEWLINE"*)
      return 1
      ;;
  esac
  printf '%s\n' "$1" | grep -Eq "$VERSION_RE"
}

# 6. The download: the archive and SHA256SUMS, into a temporary directory that the EXIT trap
# removes however the installer ends; INT and TERM end it through that trap too.
download() {
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  work=$(mktemp -d "${TMPDIR:-/tmp}/swarmops-install.XXXXXX") ||
    die "Cannot make a temporary directory."

  archive=swarmops-$version-$target.tar.gz
  release_url=$DOWNLOAD_BASE/v$version
  say "Installing SwarmOps $version ($target) into $dir."
  say "Downloading $release_url/$archive"
  fetch "$release_url/$archive" "$work/$archive" ||
    die "Cannot download $release_url/$archive: is $version a release? $RELEASES_URL"
  fetch "$release_url/SHA256SUMS" "$work/SHA256SUMS" ||
    die "Cannot download $release_url/SHA256SUMS."
}

# Removes what the installer leaves behind, however it ends: its temporary directory, and a
# binary copied into DIR but not yet renamed into place.
cleanup() {
  if [ -n "$staged" ]; then
    rm -f -- "$staged"
  fi
  if [ -n "$work" ]; then
    rm -rf -- "$work"
  fi
}

# 7. The check: the archive against its line in SHA256SUMS, by sha256sum, or by shasum where
# sha256sum is missing. Without a line, or on a mismatch, nothing is installed.
verify() {
  awk -v name="$archive" '$2 == name || $2 == "*" name' "$work/SHA256SUMS" \
    >"$work/archive.sha256" || die "Cannot read SHA256SUMS."
  if [ ! -s "$work/archive.sha256" ]; then
    die "SHA256SUMS of v$version has no line for $archive: nothing was installed."
  fi
  if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
    die "Neither sha256sum nor shasum is installed, so the download cannot be checked."
  fi
  if ! (cd "$work" && sums_match archive.sha256) >/dev/null 2>&1; then
    die "$archive does not match its line in SHA256SUMS: nothing was installed."
  fi
  say "$archive matches SHA256SUMS."
}

# Checks the files that checksum list $1 names, with sha256sum, else shasum.
sums_match() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$1"
  else
    shasum -a 256 -c "$1"
  fi
}

# 8. The binaries: only swarmops and so, from the archive's one directory.
extract() {
  top=swarmops-$version-$target
  set --
  for name in $BINARIES; do
    set -- "$@" "$top/$name"
  done
  tar -xzf "$work/$archive" -C "$work" "$@" ||
    die "$archive does not hold $*."
  for name in $BINARIES; do
    if [ ! -f "$work/$top/$name" ]; then
      die "$archive does not hold $top/$name."
    fi
  done
}

# 9. The install: each binary copied to a temporary name in DIR, made executable, then renamed
# over its own name, so that DIR never holds half a binary. The version it replaces, if any, is
# kept for the report.
install_bins() {
  mkdir -p -- "$dir" || die "Cannot create $dir."
  dir=$(CDPATH='' cd -- "$dir" && pwd) || die "Cannot enter $dir."
  for name in $BINARIES; do
    if [ -d "$dir/$name" ]; then
      die "$dir/$name is a directory: nothing was installed."
    fi
  done
  previous=$(installed_version "$dir/swarmops")

  for name in $BINARIES; do
    staged=$(mktemp "$dir/.$name.XXXXXX") || die "Cannot write in $dir."
    cp "$work/$top/$name" "$staged" || die "Cannot write $staged."
    chmod 0755 "$staged" || die "Cannot make $staged executable."
    mv -f "$staged" "$dir/$name" || die "Cannot replace $dir/$name."
    staged=
  done
}

# 10. The report, on stdout: the version now installed, the line that puts DIR on PATH if it
# is not there, and where to start.
report() {
  printed=$("$dir/swarmops" --version) ||
    die "Installed, but '$dir/swarmops --version' failed."
  printf '%s\n' "$printed"
  if [ -n "$previous" ]; then
    printf 'Replaced %s.\n' "$previous"
  fi
  printf 'Installed swarmops and so (its short form) in %s.\n' "$dir"
  path_hint
  printf 'Quick start: %s\n' "$QUICK_START_URL"
  printf 'Website: %s\n' "$WEBSITE_URL"
}

# The line that puts DIR on PATH, unless PATH has it: for fish, or for sh and its kin.
path_hint() {
  case ":$PATH:" in
    *":$dir:"* | *":$dir/:"*)
      return 0
      ;;
  esac
  case ${SHELL:-} in
    */fish)
      printf '%s is not on PATH. Add this line to ~/.config/fish/config.fish:\n' "$dir"
      printf '  fish_add_path "%s"\n' "$(quoted fish "$dir")"
      ;;
    *)
      printf '%s is not on PATH. Add this line to your shell startup file' "$dir"
      printf ' (~/.profile, ~/.bashrc or ~/.zshrc):\n'
      printf '  export PATH="%s:$PATH"\n' "$(quoted sh "$dir")"
      ;;
  esac
}

# Text $2 escaped for double quotes in shell $1 (sh or fish): a backslash before each \, " and
# $, and for sh before each backquote too.
quoted() {
  case $1 in
    fish)
      specials='\"$'
      ;;
    *)
      specials='\"$`'
      ;;
  esac
  printf '%s\n' "$2" | sed "s/[$specials]/\\\\&/g"
}

# The first line `swarmops --version` prints for binary $1 ("swarmops 0.1.0"), or nothing when
# $1 is missing or is another program.
installed_version() {
  if [ ! -x "$1" ] || [ -d "$1" ]; then
    return 0
  fi
  line=$("$1" --version 2>/dev/null | head -n 1) || line=
  case $line in
    'swarmops '?*)
      printf '%s\n' "$line"
      ;;
  esac
}

# --uninstall: removes swarmops and so from DIR when they are SwarmOps' own; another program
# by either name is left alone. Stores, and everything else, stay.
uninstall() {
  removed=
  for name in $BINARIES; do
    if [ -z "$(installed_version "$dir/$name")" ]; then
      continue
    fi
    rm -f -- "$dir/$name" || die "Cannot remove $dir/$name."
    removed="$removed $name"
  done
  if [ -z "$removed" ]; then
    die "$dir holds no SwarmOps binary: nothing was removed."
  fi
  printf 'Removed%s from %s. Stores are left as they are.\n' "$removed" "$dir"
}

main() {
  work=
  staged=
  previous=
  parse_args "$@"
  pick_dir
  if [ -n "$opt_uninstall" ]; then
    uninstall
    return 0
  fi

  check_os
  pick_target
  check_libc
  pick_downloader
  pick_version
  download
  verify
  extract
  install_bins
  report
}

main "$@"
