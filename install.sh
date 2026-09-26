#!/bin/sh
# Installs the pastor binary from a GitHub release into ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/cacarico/pastor/main/install.sh | sh
#
# PASTOR_VERSION      the version to install (default: the latest release)
# PASTOR_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
# PASTOR_DOWNLOAD_URL where the tarball and SHA256SUMS are fetched from
#                     (default: the GitHub release for that version; a
#                     mirror, or file:///path for an offline copy)
#
# It needs curl, tar and sha256sum or shasum, and nothing else: no sudo,
# no Rust. Every download is checked against SHA256SUMS from the same
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

for tool in curl tar; do
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
echo "next: pastor completions bash|fish|zsh, then pastor setup systemd (setup launchd on macOS)"
