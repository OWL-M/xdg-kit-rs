#!/bin/sh
# Writes fuzz/seeds/<target>/ from seed-package.xml (our own, not shared-mime-info's
# GPL data), using update-mime-database from the default dev shell, and from
# the desktop-entry validation corpus (also ours), and a few mimeapps.list
# files written here. The seeds
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
# Entities: a reference, forward, to an unusable entity (which makes the
# referrer unusable too), nested references, and a namespace by entity.
printf '<!DOCTYPE r [<!ENTITY a "&b;"><!ENTITY b "&#60;&b;">]><r xmlns="&a;"/>' > "$out/xml_root/unusable-loop"
printf '<!DOCTYPE r [<!ENTITY a "&b;&b;"><!ENTITY b "&c;&c;"><!ENTITY c "lol">]><r xmlns="urn:&a;"/>' > "$out/xml_root/nested"
printf '<!DOCTYPE svg [<!ENTITY ns_svg "http://www.w3.org/2000/svg">]><svg xmlns="&ns_svg;"/>' > "$out/xml_root/entity-namespace"
printf '<!DOCTYPE r [<!ENTITY a "&b;$b;"><!ENTITY b "&c;c&;e&a;"\037xmY aurn:x"]a\015\012b">}>>]><r xmlns="&a;"\037xmY a "&b;&b;">+*\377="2"\000\000' > "$out/xml_root/oom-found"
printf '<!DOCTYPE mime-type [<!ENTITY a "&b;"><!ENTITY b "&#60;&b;">]><mime-type xmlns="&a;"><comment>a</comment></mime-type>' > "$out/type_xml/unusable-loop"
# Parameter entities: included text that declares, one including another,
# an ATTLIST default in included text, a conditional section there
# (refused), and standalone with a default in included text referring to
# an entity declared there.
printf '<!DOCTYPE r [<!ENTITY %% p "<!ENTITY ns '"'"'urn:x'"'"'>"> %%p;]><r xmlns="&ns;"/>' > "$out/xml_root/pe-include"
printf '<!DOCTYPE r [<!ENTITY %% q "<!ENTITY ns '"'"'urn:x'"'"'>"><!ENTITY %% p "&#37;q;&#37;q;"> %%p;]><r xmlns="&ns;"/>' > "$out/xml_root/pe-chained"
printf '<!DOCTYPE r SYSTEM "s" [<!ENTITY ns "urn:x"><!ENTITY e "&f;"><!ENTITY %% p "<!ATTLIST r a CDATA '"'"'&#38;e;'"'"'>"> %%p;<!ENTITY f "v">]><r xmlns="&ns;" b="&u;"/>' > "$out/xml_root/pe-attlist"
printf '<!DOCTYPE r [<!ENTITY ns "urn:x"><!ENTITY %% p "<![IGNORE[ x ]]>"> %%p;]><r xmlns="&ns;"/>' > "$out/xml_root/pe-ignore"
printf '<?xml version="1.0" standalone="yes"?><!DOCTYPE r [<!ENTITY ns "urn:x"><!ENTITY %% p "<!ENTITY a '"'"'v'"'"'><!ATTLIST r b CDATA '"'"'&#38;a;'"'"'>"> %%p;]><r xmlns="&ns;"/>' > "$out/xml_root/pe-standalone"
printf '<!DOCTYPE mime-type [<!ENTITY %% p "<!ENTITY ns '"'"'http://www.freedesktop.org/standards/shared-mime-info'"'"'>"> %%p;]><mime-type xmlns="&ns;" type="a/b"><comment>c</comment></mime-type>' > "$out/type_xml/pe-include"
printf '<?xml version="1.0" standalone="yes"?><!DOCTYPE mime-type [<!ENTITY ns "http://www.freedesktop.org/standards/shared-mime-info"><!ENTITY %% q "<!ENTITY a '"'"'v'"'"'>"><!ENTITY %% p "&#37;q;<!ATTLIST mime-type b CDATA '"'"'&#38;a;'"'"'>"> %%p;]><mime-type xmlns="&ns;"><comment>c</comment></mime-type>' > "$out/type_xml/pe-standalone"
# The internal subset read by XML's grammar: every kind of declaration,
# groups nested in a content model, a public literal, and the shape
# freedesktop.org.xml has.
printf '<!DOCTYPE r PUBLIC "-//x//EN" "s" [<!ENTITY ns "urn:x"><!ELEMENT r (a,(b|c)*,d?)+><!ELEMENT a (#PCDATA|b)*><!ATTLIST r a ID #IMPLIED b (x|y) "x" c NOTATION (n) #FIXED "n"><!NOTATION n PUBLIC "-//n//EN"><!ENTITY u SYSTEM "u.gif" NDATA n><!-- c --><?pi x?>]><r xmlns="&ns;"/>' > "$out/xml_root/subset-grammar"
printf '<!DOCTYPE mime-type [<!ENTITY ns "http://www.freedesktop.org/standards/shared-mime-info"><!ELEMENT mime-type (comment+, (acronym,expanded-acronym)? , (icon? | glob | alias)*)><!ATTLIST mime-type type CDATA #REQUIRED><!ELEMENT comment (#PCDATA)><!ATTLIST comment xml:lang CDATA #IMPLIED><!ATTLIST glob weight CDATA "50">]><mime-type xmlns="&ns;" type="a/b"><comment>c</comment></mime-type>' > "$out/type_xml/subset-grammar"
# Attribute defaults: an entity in a namespace declaration, a prefixed
# namespace declaration with a type that collapses spaces, and a field's
# language.
printf '<!DOCTYPE p:r [<!ENTITY ns "urn:x"><!ATTLIST p:r xmlns CDATA "&ns;" xmlns:p NMTOKEN " urn:p ">]><p:r a="1"/>' > "$out/xml_root/attlist-defaults"
printf '<!DOCTYPE mime-type [<!ATTLIST mime-type xmlns CDATA #FIXED "http://www.freedesktop.org/standards/shared-mime-info" xml:lang NMTOKEN " fr "><!ATTLIST comment xml:lang CDATA "de">]><mime-type type="a/b"><comment>c</comment><comment xml:lang="">u</comment><acronym>A</acronym></mime-type>' > "$out/type_xml/attlist-defaults"
printf '*.tar.gz\000a.tar.gz' > "$out/fnmatch/star"
printf 'seed-[0-9]?.*\000seed-4x.txt' > "$out/fnmatch/set"
printf '[!a-c]\\*\000d*' > "$out/fnmatch/escape"
# Whole databases (mime_database): two folders' globs2, aliases, subclasses,
# magic and XMLnamespaces, then a name and data, joined by
# xdg_kit_mime::fuzzing::SEPARATOR (in the order of its FILES). The seed
# folder above: alone; under a second folder that deletes a glob and adds a
# parent loop; and under one whose magic finds XML, for the root element.
sep() { printf '\000--xkt--\000'; }
mkdir -p "$out/mime_database"
seed_folder() {
  cat "$db/globs2"; sep; cat "$db/aliases"; sep; cat "$db/subclasses"; sep
  cat "$db/magic"; sep; cat "$db/XMLnamespaces"
}
{
  seed_folder; sep; sep; sep; sep; sep; sep; printf 'notes.seed'; sep
  printf 'SEED\000\t\n\022\064'
} > "$out/mime_database/one-folder"
{
  printf '0:text/x-seed:__NOGLOBS__\n50:text/x-seed:*.later\n'; sep; sep
  printf 'text/x-seed application/x-loop\napplication/x-loop text/x-seed\n'; sep
  sep; sep
  seed_folder; sep; printf 'x.later'; sep; printf 'S plain text'
} > "$out/mime_database/two-folders"
{
  sep; sep; sep; printf '[50:application/xml]\n>0=\000\005<?xml\n'; sep; sep
  seed_folder; sep; printf 'doc.unknown'; sep
  printf '<?xml version="1.0"?>\n<seed xmlns="urn:seed"/>\n'
} > "$out/mime_database/xml-root"
# Desktop entries: the validation corpus (written for this repository), and
# Exec values from it.
corpus=../crates/xdg-kit-desktop-entry/tests/corpus
mkdir -p "$out/desktop_key_file" "$out/desktop_entry" "$out/desktop_exec"
cp "$corpus"/* "$out/desktop_key_file/"
cp "$corpus"/* "$out/desktop_entry/"
for file in "$corpus"/*; do
  # Numbered per file; the loop runs in the pipeline's subshell.
  grep '^Exec=' "$file" | {
    count=0
    while IFS= read -r line; do
      count=$((count + 1))
      printf '%s' "${line#Exec=}" > "$out/desktop_exec/$(basename "$file").$count"
    done
  }
done
# mimeapps.list files (each read as a plain and a desktop-specific one):
# every group, odd keys, repeated groups and keys, escapes, an entry before
# the first group, a header without `]`, bytes that are not UTF-8.
mkdir -p "$out/mime_apps_list"
printf '# user\n[Default Applications]\ntext/plain=a.desktop;b.desktop;\nimage/*=v.desktop;\n\n[Added Associations]\ntext/plain=a.desktop;\nx-scheme-handler/https=web.desktop;\n\n[Removed Associations]\nText/Plain=c.desktop;\n' > "$out/mime_apps_list/user"
printf '[Default Applications]\r\ntext/html=w.desktop\r\n[Added Associations]\r\ntext/html=w.desktop;\r\n' > "$out/mime_apps_list/crlf"
printf '[Default Applications]\nnot a type=x.desktop;\n*/*=all.desktop;\na/b[de]=l.desktop;\n[X-Other]\nkeep=1' > "$out/mime_apps_list/odd"
printf 'text/x=early.desktop\n[Added Associations]\ntext/plain=a.desktop;\n[Added Associations]\ntext/plain=b.desktop;\ntext/plain=c\\;d.desktop;\\se.desktop;\nimage/png=caf\351.desktop;\n[Removed Associations\n' > "$out/mime_apps_list/repeated"
rm -rf seeds
mv "$out" seeds
