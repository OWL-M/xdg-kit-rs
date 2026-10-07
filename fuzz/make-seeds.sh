#!/bin/sh
# Writes fuzz/seeds/<target>/ from seed-package.xml (our own, not shared-mime-info's
# GPL data), using update-mime-database from the default dev shell. The seeds
# are generated, not committed (fuzz/.gitignore): run
# `nix develop -c fuzz/make-seeds.sh` before fuzzing; checks.fuzz runs it
# itself. The old seeds are replaced only once all new ones are written.
set -eu
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/mime/packages"
cp seed-package.xml "$work/mime/packages/seed.xml"
XDG_DATA_DIRS="$work" XDG_DATA_HOME="$work" update-mime-database "$work/mime"
db="$work/mime"
out="$work/seeds"
mkdir -p "$out/mime_cache" "$out/magic" "$out/treemagic" "$out/globs2" "$out/type_xml" "$out/xml_root" "$out/fnmatch"
cp "$db/mime.cache" "$out/mime_cache/seed"
# magic: two-byte length, the rules after the 12-byte header, data they match.
len=$(($(wc -c < "$db/magic") - 12))
printf "\\$(printf %03o $((len / 256)))\\$(printf %03o $((len % 256)))" > "$out/magic/seed"
tail -c +13 "$db/magic" >> "$out/magic/seed"
printf 'SEED\000\t\n\022\064\004\003\002\001 seed data' >> "$out/magic/seed"
cp "$db/treemagic" "$out/treemagic/seed"
cp "$db/globs2" "$out/globs2/globs2"
cp "$db/aliases" "$out/globs2/aliases"
cp "$db/XMLnamespaces" "$out/globs2/namespaces"
cp "$db/subclasses" "$out/globs2/subclasses"
cp "$db/icons" "$out/globs2/icons"
cp "$db/generic-icons" "$out/globs2/generic-icons"
cp "$db/types" "$out/globs2/types"
cp "$db/text/x-seed.xml" "$out/type_xml/seed"
# What update-mime-database never writes: CDATA, character references, an
# empty xml:lang, markup inside a value.
printf '<?xml version="1.0"?>\n<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="a/b"><comment><![CDATA[x<y]]> &#x41;&#66;&amp;</comment><comment xml:lang="">empty</comment><acronym>A<b>B</b>C</acronym></mime-type>' > "$out/type_xml/handwritten"
printf '<?xml version="1.0"?>\n<!-- c --><s:seed xmlns:s="urn:seed" a="1">' > "$out/xml_root/prefixed"
printf '<!DOCTYPE x><x xmlns="urn:any"/>' > "$out/xml_root/default"
printf '\357\273\277<?pi data?><u:x/>' > "$out/xml_root/unbound"
printf '*.tar.gz\000a.tar.gz' > "$out/fnmatch/star"
printf 'seed-[0-9]?.*\000seed-4x.txt' > "$out/fnmatch/set"
printf '[!a-c]\\*\000d*' > "$out/fnmatch/escape"
rm -rf seeds
mv "$out" seeds
