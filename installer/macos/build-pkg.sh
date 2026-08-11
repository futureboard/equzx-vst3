#!/usr/bin/env bash
#
# EQUZX — macOS installer.
#
# Builds a .pkg that drops the VST3 and CLAP bundles into the system plug-in
# folders. Both are directory bundles and must keep their `.vst3` / `.clap`
# suffix and internal layout exactly as `cargo xtask bundle` produced them —
# a host finds a plug-in by that suffix, and a bundle that has been renamed or
# flattened is simply invisible.
#
#   installer/macos/build-pkg.sh [bundled-dir] [output-dir]
#
# Signing is opt-in through the environment, so a local build needs no identity:
#   CODESIGN_IDENTITY    "Developer ID Application: ..." — signs the bundles
#   PRODUCTSIGN_IDENTITY "Developer ID Installer: ..."   — signs the .pkg
#   NOTARY_PROFILE       a `notarytool` keychain profile — notarises and staples

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bundled="${1:-$repo_root/target/bundled}"
out_dir="${2:-$repo_root/target/installer}"

version="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$repo_root/version.json")"
identifier="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bundleId"])' "$repo_root/version.json")"

vst3="$bundled/EQUZX.vst3"
clap="$bundled/EQUZX.clap"
[[ -d "$vst3" ]] || { echo "missing $vst3 — run: cargo xtask bundle equzx --release" >&2; exit 1; }
[[ -d "$clap" ]] || { echo "missing $clap — run: cargo xtask bundle equzx --release" >&2; exit 1; }

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

payload="$staging/payload"
mkdir -p "$payload/Library/Audio/Plug-Ins/VST3" "$payload/Library/Audio/Plug-Ins/CLAP"
# -R, not -r: these are bundles, and symlinks inside them have to stay symlinks.
cp -R "$vst3" "$payload/Library/Audio/Plug-Ins/VST3/EQUZX.vst3"
cp -R "$clap" "$payload/Library/Audio/Plug-Ins/CLAP/EQUZX.clap"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  echo "signing bundles as $CODESIGN_IDENTITY"
  for bundle in "$payload/Library/Audio/Plug-Ins/VST3/EQUZX.vst3" \
                "$payload/Library/Audio/Plug-Ins/CLAP/EQUZX.clap"; do
    codesign --force --timestamp --options runtime \
      --sign "$CODESIGN_IDENTITY" "$bundle"
  done
else
  echo "no CODESIGN_IDENTITY set — building an unsigned package"
fi

mkdir -p "$out_dir"
component="$staging/EQUZX-component.pkg"
product="$out_dir/EQUZX-$version-macos.pkg"

pkgbuild \
  --root "$payload" \
  --identifier "$identifier" \
  --version "$version" \
  --install-location / \
  "$component"

# A distribution wrapper is what gives the installer a title, a readable
# welcome pane, and the architecture requirement.
cat >"$staging/distribution.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>EQUZX $version</title>
    <organization>digital.futureboard</organization>
    <options customize="never" require-scripts="false" hostArchitectures="x86_64,arm64"/>
    <domains enable_localSystem="true"/>
    <choices-outline>
        <line choice="default"/>
    </choices-outline>
    <choice id="default" title="EQUZX">
        <pkg-ref id="$identifier"/>
    </choice>
    <pkg-ref id="$identifier" version="$version" onConclusion="none">EQUZX-component.pkg</pkg-ref>
</installer-gui-script>
XML

productbuild \
  --distribution "$staging/distribution.xml" \
  --package-path "$staging" \
  --version "$version" \
  "$product"

if [[ -n "${PRODUCTSIGN_IDENTITY:-}" ]]; then
  echo "signing package as $PRODUCTSIGN_IDENTITY"
  productsign --sign "$PRODUCTSIGN_IDENTITY" "$product" "$product.signed"
  mv "$product.signed" "$product"
fi

if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  echo "notarising"
  xcrun notarytool submit "$product" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$product"
fi

echo "built $product"
