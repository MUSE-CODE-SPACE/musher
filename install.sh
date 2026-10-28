#!/bin/sh
# Install the latest musher release binary.  Usage: curl -fsSL .../install.sh | sh
#   MUSHER_VERSION=v0.3.0      pin a release (default: latest)
#   MUSHER_INSTALL_DIR=/path   where to put the binary (default: ~/.local/bin)
set -e
repo="MUSE-CODE-SPACE/musher"
dir="${MUSHER_INSTALL_DIR:-$HOME/.local/bin}"
version="${MUSHER_VERSION:-latest}"

os=$(uname -s); arch=$(uname -m)
case "$os" in Darwin) os=apple-darwin ;; Linux) os=unknown-linux-musl ;; *) echo "unsupported OS: $os"; exit 1 ;; esac
case "$arch" in arm64|aarch64) arch=aarch64 ;; x86_64|amd64) arch=x86_64 ;; *) echo "unsupported arch: $arch"; exit 1 ;; esac
target="$arch-$os"

if [ "$version" = latest ]; then
  # The newest release that actually has a binary for this platform (a build job can lag or fail).
  url=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=10" | grep -o "https://[^\"]*/musher-$target.tar.gz" | head -1)
  [ -n "$url" ] || url="https://github.com/$repo/releases/latest/download/musher-$target.tar.gz"
else url="https://github.com/$repo/releases/download/$version/musher-$target.tar.gz"; fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $url"
curl -fsSL "$url" | tar -xz -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/musher" "$dir/musher"
echo "installed $dir/musher"
case ":$PATH:" in *":$dir:"*) ;; *) echo "add to PATH: export PATH=\"$dir:\$PATH\"" ;; esac
"$dir/musher" --version
