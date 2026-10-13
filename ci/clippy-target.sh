#!/bin/sh
# Lints each crate's library and tests for one target, with its default
# features and with all of them, each crate on its own so that no other
# crate's features fill in for a missing one. Run it in the dev shell
# `cross`: `nix develop .#cross -c ci/clippy-target.sh <target>`.
set -eu
target=$1
case " $XDG_KIT_CROSS_TARGETS $XDG_KIT_CROSS_SOURCE_TARGETS " in
*" $target "*) ;;
*)
    echo "$target is not a target of the dev shell cross" >&2
    exit 1
    ;;
esac
# Targets with no prebuilt standard library build it from source.
case " $XDG_KIT_CROSS_SOURCE_TARGETS " in
*" $target "*)
    export RUSTC_BOOTSTRAP=1
    set -- -Zbuild-std=std
    ;;
*)
    set --
    ;;
esac
for dir in crates/*/; do
    crate=$(basename "$dir")
    # Default features, then all of them.
    for features in "" --all-features; do
        for kind in --lib --tests; do
            echo "== $target $crate $kind ${features:-default features}"
            cargo clippy "$@" --target "$target" -p "$crate" $kind $features \
                --locked -- -D warnings
        done
    done
done
