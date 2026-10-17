#!/bin/sh
# Installs the pastor binary from a GitHub release into ~/.local/bin.
#
#   curl -fsSL https://cacari.co/pastor/install.sh | sh
#
# With `join` first, it then runs `pastor join` with the rest of its
# arguments, as they are, to make this a pull machine of a head. Given no
# token, it asks for one on the terminal with echo off and hands it to
# `pastor join --token-stdin` on a pipe, so `ps` never shows it:
#
#   curl -fsSL https://cacari.co/pastor/install.sh | sudo sh -s -- join
#
# With no terminal (automation, cloud-init), the token goes on stdin, so
# the script comes as an argument instead:
#
#   sudo sh -c "$(curl -fsSL https://cacari.co/pastor/install.sh)" sh join --token-stdin <tokenfile
#
# PASTOR_JOIN_TOKEN works too, but the same user and root can read it in
# /proc/<pid>/environ, and on sudo's command line `ps` shows it to all.
#
# PASTOR_VERSION      the version to install (default: the latest release)
# PASTOR_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
# PASTOR_DOWNLOAD_URL where the tarball and SHA256SUMS are fetched from
#                     (default: the GitHub release for that version; a
#                     mirror, or file:///path for an offline copy)
#
# It needs curl, tar and sha256sum or shasum (and ssh-keygen to join), and
# nothing else: no sudo, no Rust. Run as root, it installs what is missing
# of openssh-client, curl, tar and ca-certificates with apt, dnf or pacman
# first, under the names each gives them. Every download is checked against SHA256SUMS from the same
# release, and the provenance attestation is verified when a logged-in gh
# is around. Completions and the systemd unit are the binary's own job
# afterwards: `pastor completions <shell>` and `pastor setup systemd` (or
# `pastor setup launchd` on macOS).
set -eu

repo=cacarico/pastor
version=${PASTOR_VERSION:-latest}
dir=${PASTOR_INSTALL_DIR:-$HOME/.local/bin}

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

# `join` and what follows are for pastor, so "$@" keeps them as they came:
# never split again, never evaluated.
mode="install"
if [ "${1:-}" = join ]; then
    mode="join"
    shift
fi

tools="curl tar"
[ "$mode" = join ] && tools="$tools ssh-keygen"
missing=
for tool in $tools; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
# Only root installs packages, and only these four.
if [ -n "$missing" ] && [ "$(id -u)" = 0 ]; then
    echo "installing what is missing:$missing" >&2
    if command -v apt-get >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get update -q
        DEBIAN_FRONTEND=noninteractive apt-get install -y -q openssh-client curl tar ca-certificates
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y openssh-clients curl tar ca-certificates
    elif command -v pacman >/dev/null 2>&1; then
        pacman -S --noconfirm --needed openssh curl tar ca-certificates
    else
        fail "missing:$missing, and there is no apt-get, dnf or pacman to install it with"
    fi
fi
for tool in $tools; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is needed"
done
if command -v sha256sum >/dev/null 2>&1; then
    sha256="sha256sum -c -"
elif command -v shasum >/dev/null 2>&1; then
    sha256="shasum -a 256 -c -"
else
    fail "sha256sum or shasum is needed to check the download"
fi

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
    Linux/x86_64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
    Linux/armv7l) target=armv7-unknown-linux-musleabihf ;;
    Linux/riscv64) target=riscv64gc-unknown-linux-musl ;;
    Darwin/arm64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    *) fail "no prebuilt pastor for $os/$arch; build one with: cargo install pastor-cli --locked" ;;
esac
# A 64-bit kernel over a 32-bit userland (common on Raspberry Pi OS) reports
# aarch64 but cannot run a 64-bit binary.
if [ "$target" = aarch64-unknown-linux-musl ] && [ "$(getconf LONG_BIT 2>/dev/null)" = 32 ]; then
    target=armv7-unknown-linux-musleabihf
fi

if [ "$version" = latest ]; then
    # Drafts and prereleases are not "latest", so an rc is never picked here.
    version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
        sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$version" ] || fail "could not find the latest release of $repo"
fi
version=${version#v}
base=${PASTOR_DOWNLOAD_URL:-https://github.com/$repo/releases/download/v$version}
name="pastor-$version-$target"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading $name.tar.gz"
# Redirects (GitHub serves assets from another host) must stay on https.
curl -fsSL --proto-redir '=https' -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL --proto-redir '=https' -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
grep " $name.tar.gz\$" "$tmp/SHA256SUMS" >"$tmp/expected" || fail "SHA256SUMS has no entry for $name.tar.gz"
(cd "$tmp" && $sha256 <expected >/dev/null) || fail "checksum mismatch for $name.tar.gz"

if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$tmp/$name.tar.gz" --repo "$repo" >/dev/null || fail "provenance attestation did not verify"
    echo "provenance attestation verified"
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
# Copy then rename, so a running head keeps its old binary until it restarts
# and a half-written file is never on the PATH.
cp "$tmp/$name/pastor" "$dir/.pastor.new"
chmod 0755 "$dir/.pastor.new"
mv -f "$dir/.pastor.new" "$dir/pastor"
echo "installed $("$dir/pastor" --version) to $dir/pastor"

case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "note: $dir is not on your PATH" >&2 ;;
esac
if [ "$mode" = join ]; then
    # Asked for only when nothing gives a token (an argument, stdin, the
    # variable) and nothing else is asked (a subcommand, help).
    ask=yes
    [ -n "${PASTOR_JOIN_TOKEN:-}" ] && ask=no
    for a in "$@"; do
        case "$a" in
            --token-stdin | -h | --help | create | list | revoke | pjt_*) ask=no ;;
        esac
    done
    # A /dev/tty that opens: the node is there even with no terminal.
    if [ "$ask" = yes ] && (: </dev/tty) 2>/dev/null; then
        trap 'stty echo </dev/tty 2>/dev/null; rm -rf "$tmp"' EXIT
        trap 'exit 130' INT TERM HUP
        stty -echo </dev/tty
        printf 'join token: ' >/dev/tty
        IFS= read -r token </dev/tty || token=
        stty echo </dev/tty
        printf '\n' >/dev/tty
        [ -n "$token" ] || fail "no join token given"
        # printf is a builtin: the token reaches pastor on a pipe, never in
        # an argument or the environment.
        printf '%s\n' "$token" | "$dir/pastor" join --token-stdin "$@"
        exit
    fi
    # The arguments may hold the token: they are passed on, never printed.
    exec "$dir/pastor" join "$@"
fi
echo "next: pastor completions bash|fish|zsh, then pastor setup systemd (setup launchd on macOS)"
