#!/bin/sh
# Installs the xcheckfs binary from a GitHub release.
#
#   curl -fsSL https://github.com/replicix/xcheckfs/releases/latest/download/install.sh | sh
#
# Environment:
#   XCHECKFS_VERSION      release to install, e.g. 0.1.0 (default: latest)
#   XCHECKFS_INSTALL_DIR  target directory (default: /usr/local/bin when
#                         writable or running as root, else ~/.local/bin)
#   XCHECKFS_BASE_URL     download from this URL (a mirror) instead of GitHub
#
# While the repository is private, downloads need an authenticated GitHub
# CLI (`gh auth login`); it is used automatically when available.
set -eu

REPO=replicix/xcheckfs

die() {
    echo "xcheckfs install: $*" >&2
    exit 1
}

[ "$(uname -s)" = Linux ] || die "xcheckfs runs on Linux only (it needs the Linux FUSE kernel interface)"
case "$(uname -m)" in
    x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *) die "no prebuilt binary for $(uname -m); build from source with cargo" ;;
esac

version=${XCHECKFS_VERSION:-latest}
version=${version#v}
asset=xcheckfs-$target.tar.xz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -z "${XCHECKFS_BASE_URL:-}" ] && command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    if [ "$version" = latest ]; then tag=; else tag=v$version; fi
    # shellcheck disable=SC2086 # empty $tag selects the latest release
    gh release download $tag -R "$REPO" -D "$tmp" -p "$asset" -p SHA256SUMS ||
        die "download failed (is the release published?)"
else
    if [ -n "${XCHECKFS_BASE_URL:-}" ]; then
        base=$XCHECKFS_BASE_URL
    elif [ "$version" = latest ]; then
        base=https://github.com/$REPO/releases/latest/download
    else
        base=https://github.com/$REPO/releases/download/v$version
    fi
    for f in "$asset" SHA256SUMS; do
        curl -fsSL -o "$tmp/$f" "$base/$f" ||
            die "download of $f failed (a private repository needs 'gh auth login')"
    done
fi

(
    cd "$tmp"
    grep " $asset\$" SHA256SUMS >asset.sum || die "$asset is not listed in SHA256SUMS"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c asset.sum >/dev/null
    else
        shasum -a 256 -c asset.sum >/dev/null
    fi
) || die "checksum verification failed"

tar -C "$tmp" -xJf "$tmp/$asset"

dir=${XCHECKFS_INSTALL_DIR:-}
if [ -z "$dir" ]; then
    if [ "$(id -u)" = 0 ] || [ -w /usr/local/bin ]; then
        dir=/usr/local/bin
    else
        dir=$HOME/.local/bin
    fi
fi
mkdir -p "$dir"
install -m 755 "$tmp/xcheckfs-$target/xcheckfs" "$dir/xcheckfs"
echo "installed $("$dir/xcheckfs" --version) to $dir/xcheckfs"

case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "note: $dir is not in PATH" ;;
esac
if ! command -v fusermount3 >/dev/null 2>&1; then
    echo "note: fusermount3 not found; non-root mounts need the fuse3 package"
fi
