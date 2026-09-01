#!/usr/bin/env bash
# Install the newest notarized Refinery release into ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/cooperativ-labs/refinery/main/scripts/install-cli.sh | bash
#
# Nothing is installed until the download matches the checksum the release
# published, its payload manifest names this version and target, and the binary
# passes Gatekeeper's signature check.
set -euo pipefail

repository="cooperativ-labs/refinery"
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) target="aarch64-apple-darwin" ;;
    Darwin/x86_64) target="x86_64-apple-darwin" ;;
    *) echo "Refinery publishes builds for Apple Silicon and Intel Macs. Build from source with \`cargo build --release\`." >&2; exit 1 ;;
esac

tag="$(curl -fsSL "https://api.github.com/repos/$repository/releases/latest" |
    sed -nE 's/^[[:space:]]*"tag_name":[[:space:]]*"([^"]+)",?$/\1/p' | head -n 1)"
if [[ ! "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
    echo "Could not determine the newest Refinery release." >&2
    exit 1
fi

version="${BASH_REMATCH[1]}"
archive="refinery-${version}-${target}.zip"
release_base="https://github.com/$repository/releases/download/$tag"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/refinery-install.XXXXXX")"
trap 'rm -rf -- "$work_dir"' EXIT

curl -fL "$release_base/$archive" -o "$work_dir/$archive"
curl -fL "$release_base/checksums.txt" -o "$work_dir/checksums.txt"
awk -v archive="$archive" '$2 == archive { print }' "$work_dir/checksums.txt" > "$work_dir/archive.sha256"
if [[ ! -s "$work_dir/archive.sha256" ]]; then
    echo "The release checksum does not list $archive." >&2
    exit 1
fi
(cd "$work_dir" && shasum -a 256 -c archive.sha256)
ditto -x -k "$work_dir/$archive" "$work_dir/extracted"
/usr/bin/python3 -c 'import json,sys; p=json.load(open(sys.argv[1])); assert p == {"formatVersion":1,"version":sys.argv[2],"target":sys.argv[3],"binaries":["refinery"]}, p' \
    "$work_dir/extracted/refinery-payload.json" "$version" "$target"
codesign --verify --strict "$work_dir/extracted/refinery"
"$work_dir/extracted/refinery" --version | grep -Fx "refinery $version"

mkdir -p "$HOME/.local/bin"
install -m 0755 "$work_dir/extracted/refinery" "$HOME/.local/bin/refinery"
printf 'Installed Refinery %s at %s/refinery\n' "$version" "$HOME/.local/bin"
printf 'If refinery is not on PATH, add $HOME/.local/bin to your shell configuration.\n'
printf 'Run `refinery setup` to finish the first run.\n'
