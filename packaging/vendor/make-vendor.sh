#!/bin/sh
# Regenerate the vendored build inputs of the Ubuntu PPA source package.
# Run from the repository root whenever Cargo.lock or the toolchain changes.
#
#   packaging/vendor/make-vendor.sh [TOOLCHAIN_DIR]
#
# TOOLCHAIN_DIR holds the official rustc/cargo/rust-std component tarballs
# from https://static.rust-lang.org/dist/ (they are downloaded when absent).
set -eu
RUST_VERSION=1.97.1
TRIPLE=x86_64-unknown-linux-gnu
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DEST=packaging/vendor
SRC=${1:-}

for c in rustc cargo rust-std; do
  f="$c-$RUST_VERSION-$TRIPLE.tar.xz"
  if [ -n "$SRC" ] && [ -f "$SRC/$f" ]; then
    cp "$SRC/$f" "$DEST/$f"
  elif [ ! -f "$DEST/$f" ]; then
    curl -fsSLo "$DEST/$f" "https://static.rust-lang.org/dist/$f"
  fi
done

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cargo vendor --locked --versioned-dirs "$WORK/vendor" >/dev/null

# Crates the three Linux programs build with; everything else (Windows,
# macOS and wasm crates, dev-dependencies) becomes an empty stub so the
# source package does not carry hundreds of MB that are never compiled.
cargo tree --locked --target "$TRIPLE" -e normal,build --prefix none --format '{p}' \
  -p diskrescue -p diskrescue-tui -p diskrescue-gui \
  | sed -n 's/^\([A-Za-z0-9_-]*\) v\([^ ]*\).*/\1-\2/p' | sort -u > "$WORK/keep"

python3 - "$WORK/vendor" "$WORK/keep" <<'PY'
import json, os, shutil, sys
vendor, keep_file = sys.argv[1], sys.argv[2]
keep = set(open(keep_file).read().split())
stubbed = 0
for d in sorted(os.listdir(vendor)):
    if d in keep:
        continue
    path = os.path.join(vendor, d)
    manifest = open(os.path.join(path, "Cargo.toml")).read()
    checksum = json.load(open(os.path.join(path, ".cargo-checksum.json")))
    shutil.rmtree(path)
    os.makedirs(os.path.join(path, "src"))
    open(os.path.join(path, "Cargo.toml"), "w").write(manifest)
    open(os.path.join(path, "src", "lib.rs"), "w").write("")
    json.dump({"files": {}, "package": checksum.get("package")}, open(os.path.join(path, ".cargo-checksum.json"), "w"))
    stubbed += 1
print(f"kept {len(keep)} crates, stubbed {stubbed}")
PY

tar -C "$WORK" --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -cJf "$DEST/crates-$VERSION.tar.xz" vendor
(cd "$DEST" && sha256sum rustc-*.tar.xz cargo-*.tar.xz rust-std-*.tar.xz "crates-$VERSION.tar.xz" > SHA256SUMS)
ls -la "$DEST"
