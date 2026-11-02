#!/usr/bin/env bash
# Installs the newest cooperalianca-library release:
#   curl -fsSL https://raw.githubusercontent.com/imabee-glacier/cooperalianca-library/main/install/cooperalianca-library.sh | bash
# COOPERALIANCA_INSTALL_DIR sets the target (default ~/.local/bin); COOPERALIANCA_VERSION pins a version (e.g. 0.1.0).
set -euo pipefail

repo=${COOPERALIANCA_REPO:-imabee-glacier/cooperalianca-library}
github=${COOPERALIANCA_GITHUB_URL:-https://github.com}
api=${COOPERALIANCA_API_URL:-https://api.github.com}
dir=${COOPERALIANCA_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) triple=x86_64-unknown-linux-gnu ;;
  *) echo "cooperalianca-library: no prebuilt binary for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

if [ -n "${COOPERALIANCA_VERSION:-}" ]; then
  version=${COOPERALIANCA_VERSION#v}
else
  version=$(curl -fsSL "$api/repos/$repo/releases?per_page=100" \
    | grep -o '"tag_name": *"cooperalianca-library-v[0-9]*\.[0-9]*\.[0-9]*"' \
    | sed 's/.*cooperalianca-library-v//; s/"$//' | sort -V | tail -n 1)
  [ -n "$version" ] || { echo "cooperalianca-library: no release found for $repo" >&2; exit 1; }
fi
tag=cooperalianca-library-v$version
asset=cooperalianca-library-$version-$triple.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Installing cooperalianca-library $version to $dir"
curl -fsSL -o "$tmp/$asset" "$github/$repo/releases/download/$tag/$asset"
curl -fsSL -o "$tmp/$asset.sha256" "$github/$repo/releases/download/$tag/$asset.sha256"
(cd "$tmp" && sha256sum -c "$asset.sha256" > /dev/null)

mkdir -p "$dir"
tar -xzf "$tmp/$asset" -C "$dir" ./cooperalianca
echo "Installed $("$dir/cooperalianca" --version)"
case ":$PATH:" in *":$dir:"*) ;; *) echo "Add $dir to your PATH to run cooperalianca." ;; esac
