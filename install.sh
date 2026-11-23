#!/bin/sh
# =============================================================================
#  install.sh - install iztop from its latest GitHub release.
#
#    sh -c "$(curl -fsSL https://raw.githubusercontent.com/GladTechInfo/iztop/main/install.sh)"
#
#  Picks the release file for this system, checks it against the release's
#  SHA256SUMS, and installs it:
#    Debian, Ubuntu, Proxmox VE          the .deb, with apt   -> /usr/bin/iztop
#    Fedora, RHEL and rebuilds, openSUSE the .rpm, with dnf, yum or zypper
#    Arch, Alpine and anything else      the static binary    -> /usr/local/bin/iztop
#  Uses sudo (or doas) when not run as root, and prints each command first.
#
#  Options (after `sh -s --` when piped:  curl -fsSL URL | sh -s -- --uninstall):
#    --version X.Y.Z   that release instead of the latest
#    --method M        deb, rpm or bin instead of the one detected
#    --bindir DIR      where the static binary goes (default /usr/local/bin);
#                      a directory you can write to needs no sudo
#    --uninstall       remove iztop, however this script installed it
#  Needs: Linux on x86-64, curl or wget, sha256sum.
# =============================================================================
set -eu

# ------------------------------------------------------------------ config ---
#  setting      value
NAME=iztop
REPO=GladTechInfo/iztop
RUN_HINT='sudo iztop'

VERSION=''
METHOD=''
BINDIR=/usr/local/bin
ACTION=install

# ----------------------------------------------------------------- helpers ---
# say: a message on stderr, under the tool's name.
say() { printf '\033[33m%s-install\033[0m: %s\n' "$NAME" "$*" >&2; }
# die: a message, then stop.
die() { say "$*"; exit 1; }
# run: print a command, then run it (with $SUDO in front when given).
run() { printf '\033[1m+ %s\033[0m\n' "$*" >&2; "$@"; }
# have: is a command available?
have() { command -v "$1" >/dev/null 2>&1; }

# fetch URL FILE: download with curl or wget.
fetch() {
  if have curl; then curl -fsSL -o "$2" "$1"
  else wget -q -O "$2" "$1"
  fi
}

# need_root: set SUDO to the command that gives root, or to nothing as root.
need_root() {
  if [ "$(id -u)" = 0 ]; then SUDO=''
  elif have sudo; then SUDO=sudo
  elif have doas; then SUDO=doas
  else die 'needs root: run it as root, or install sudo or doas'
  fi
}

# ----------------------------------------------------------------- options ---
while [ $# -gt 0 ]; do
  case $1 in
    --version)   VERSION=${2:?--version needs a value}; VERSION=${VERSION#v}; shift 2 ;;
    --method)    METHOD=${2:?--method needs deb, rpm or bin}; shift 2 ;;
    --bindir)    BINDIR=${2:?--bindir needs a directory}; shift 2 ;;
    --uninstall) ACTION=uninstall; shift ;;
    -h|--help)   printf 'options: --version X.Y.Z, --method deb|rpm|bin, --bindir DIR, --uninstall\n'; exit 0 ;;
    *)           die "unknown option: $1 (try --help)" ;;
  esac
done
case $METHOD in ''|deb|rpm|bin) ;; *) die "--method must be deb, rpm or bin, not $METHOD" ;; esac

# --------------------------------------------------------------- uninstall ---
# Removes the package if one is installed, else the static binary.
if [ $ACTION = uninstall ]; then
  need_root
  if have dpkg && dpkg -s "$NAME" >/dev/null 2>&1; then
    run $SUDO apt-get remove -y "$NAME"
  elif have rpm && rpm -q "$NAME" >/dev/null 2>&1; then
    if   have dnf;    then run $SUDO dnf remove -y "$NAME"
    elif have zypper; then run $SUDO zypper --non-interactive remove "$NAME"
    else                   run $SUDO rpm -e "$NAME"
    fi
  elif [ -e "$BINDIR/$NAME" ]; then
    [ -w "$BINDIR" ] && SUDO=''
    run $SUDO rm -f "$BINDIR/$NAME"
  else
    die "$NAME is not installed (no package, nothing in $BINDIR)"
  fi
  say 'removed'
  exit 0
fi

# ------------------------------------------------------------------ checks ---
[ "$(uname -s)" = Linux ] || die "runs on Linux only (this is $(uname -s))"
case $(uname -m) in
  x86_64|amd64) ;;
  *) die "release files are for x86-64 only (this is $(uname -m)); build from source instead" ;;
esac
have curl || have wget || die 'needs curl or wget'
have sha256sum || die 'needs sha256sum (coreutils)'

# ------------------------------------------------------------- the method ---
# Arch first: it may have dpkg or rpm installed as tools, never as its manager.
if [ -z "$METHOD" ]; then
  if   have pacman;                                         then METHOD=bin
  elif have dpkg && have apt-get;                           then METHOD=deb
  elif have rpm && { have dnf || have yum || have zypper; }; then METHOD=rpm
  else                                                           METHOD=bin
  fi
fi

# ------------------------------------------------------------- the release ---
# SHA256SUMS first: from releases/latest/download it also names the latest
# version (a plain redirect, so no GitHub API and no API rate limit).
# A world-readable directory, so apt's unprivileged downloader can read the .deb.
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT INT TERM
chmod 755 "$dir"
if [ -n "$VERSION" ]; then
  base="https://github.com/$REPO/releases/download/v$VERSION"
  fetch "$base/SHA256SUMS" "$dir/SHA256SUMS" || die "no release v$VERSION (or no SHA256SUMS in it)"
else
  fetch "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$dir/SHA256SUMS" ||
    die 'cannot download the latest release from GitHub (offline?)'
  # The static binary's line, "<sum>  NAME-X.Y.Z-x86_64-linux", gives X.Y.Z.
  while read -r _sum f; do
    f=${f#\*}
    case $f in "$NAME"-*-x86_64-linux) VERSION=${f#"$NAME"-}; VERSION=${VERSION%-x86_64-linux} ;; esac
  done < "$dir/SHA256SUMS"
  [ -n "$VERSION" ] || die 'cannot tell the latest version from its SHA256SUMS'
  base="https://github.com/$REPO/releases/download/v$VERSION"
fi

case $METHOD in
  deb) FILE="${NAME}_${VERSION}-1_amd64.deb" ;;
  rpm) FILE="${NAME}-${VERSION}-1.x86_64.rpm" ;;
  bin) FILE="${NAME}-${VERSION}-x86_64-linux" ;;
esac

# Already this version? (The static binary prints "NAME X.Y.Z" for --version.)
if have "$NAME" && "$NAME" --version 2>/dev/null | grep -q " $VERSION\$"; then
  say "$NAME $VERSION is already installed ($(command -v "$NAME"))"
  exit 0
fi

say "installing $NAME $VERSION from $FILE"

# ---------------------------------------------------- download and verify ---
# Plain shell rather than awk: openSUSE's container images have no awk.
fetch "$base/$FILE" "$dir/$FILE" || die "cannot download $base/$FILE"
chmod 644 "$dir/$FILE"

want=''
while read -r sum f; do
  [ "${f#\*}" = "$FILE" ] && want=$sum
done < "$dir/SHA256SUMS"
# shellcheck disable=SC2046  # split "<sum>  <file>" into $1 and $2
set -- $(sha256sum "$dir/$FILE")
got=$1
[ -n "$want" ] || die "$FILE is not listed in SHA256SUMS"
[ "$want" = "$got" ] || die "checksum mismatch for $FILE (expected $want, got $got) - not installed"
say 'checksum OK'

# ----------------------------------------------------------------- install ---
case $METHOD in
  deb)
    need_root
    run $SUDO apt-get install -y "$dir/$FILE" ;;
  rpm)
    need_root
    if   have dnf;    then run $SUDO dnf install -y "$dir/$FILE"
    elif have zypper; then run $SUDO zypper --non-interactive install --allow-unsigned-rpm "$dir/$FILE"
    else                   run $SUDO yum install -y "$dir/$FILE"
    fi ;;
  bin)
    # sudo only when this user cannot create or write the directory.
    if [ ! -d "$BINDIR" ] && ! mkdir -p "$BINDIR" 2>/dev/null; then
      need_root
      run $SUDO mkdir -p "$BINDIR"
    fi
    if [ -w "$BINDIR" ]; then SUDO=''; else need_root; fi
    run $SUDO install -m 0755 "$dir/$FILE" "$BINDIR/$NAME"
    if have pacman; then
      say 'pacman does not track this copy; packaging/aur in the repository builds a pacman package instead'
    fi
    case ":$PATH:" in *":$BINDIR:"*) ;; *) say "$BINDIR is not on your PATH" ;; esac ;;
esac

say "done: $("$NAME" --version 2>/dev/null || echo "$NAME $VERSION")"
say "start it with:  $RUN_HINT"
say "remove it with: curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | sh -s -- --uninstall"
