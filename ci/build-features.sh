#!/bin/sh
# Builds each crate on its own with its default features and with none, so
# that no other crate's features fill in for a missing one. Run it in the
# dev shell: `nix develop -c ci/build-features.sh`.
set -eu
for dir in crates/*/; do
    crate=$(basename "$dir")
    # Default features, then none.
    for features in "" --no-default-features; do
        echo "== $crate ${features:-default features}"
        cargo build -p "$crate" $features --locked
    done
done
