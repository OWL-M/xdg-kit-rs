#!/usr/bin/env python3
"""Dumps a menu as GNOME's libgnome-menu (GMenu 3.0) resolves it.

The reference side of the crate's tree comparison: one line per item, in
display order, in the format the Rust side prints too. With --menutest it
prints the spec suite's flat `<menu path>/<TAB><id><TAB><file>` format.

    gmenu-dump.py [--menutest] [--no-display] [--show-empty] <menu file>
"""

import sys

import gi

gi.require_version("GMenu", "3.0")
from gi.repository import GMenu  # noqa: E402


def directory_path(directory):
    names = []
    while directory is not None and directory.get_parent() is not None:
        names.append(directory.get_menu_id())
        directory = directory.get_parent()
    return "/".join(reversed(names))


def walk(directory, out, menutest):
    here = directory_path(directory)
    shown = (here + "/") if here else "/"
    it = directory.iter()
    while True:
        kind = it.next()
        if kind == GMenu.TreeItemType.INVALID:
            break
        if kind == GMenu.TreeItemType.DIRECTORY:
            sub = it.get_directory()
            if not menutest:
                out.append(f"menu {directory_path(sub)} label={sub.get_name()}")
            walk(sub, out, menutest)
        elif kind == GMenu.TreeItemType.ENTRY:
            entry = it.get_entry()
            if menutest:
                out.append(f"{shown}\t{entry.get_desktop_file_id()}\t{entry.get_desktop_file_path()}")
            else:
                name = entry.get_app_info().get_name()
                out.append(f"entry {here} {entry.get_desktop_file_id()} label={name}")
        elif kind == GMenu.TreeItemType.SEPARATOR:
            if not menutest:
                out.append(f"separator {here}")
        elif kind == GMenu.TreeItemType.HEADER:
            header = it.get_header()
            if not menutest:
                out.append(f"header {here} label={header.get_directory().get_name()}")
        elif kind == GMenu.TreeItemType.ALIAS:
            alias = it.get_alias()
            label = alias.get_directory().get_name()
            if alias.get_aliased_item_type() == GMenu.TreeItemType.ENTRY:
                entry = alias.get_aliased_entry()
                if menutest:
                    out.append(f"{shown}\t{entry.get_desktop_file_id()}\t{entry.get_desktop_file_path()}")
                else:
                    out.append(f"alias {here} label={label} -> entry {entry.get_desktop_file_id()}")
            else:
                sub = alias.get_aliased_directory()
                if not menutest:
                    out.append(f"alias {here} label={label} -> menu {directory_path(sub)}")
                walk(sub, out, menutest)


def main():
    args = sys.argv[1:]
    menutest = "--menutest" in args
    flags = GMenu.TreeFlags.NONE
    if "--no-display" in args:
        flags |= GMenu.TreeFlags.INCLUDE_NODISPLAY
    if "--show-empty" in args:
        flags |= GMenu.TreeFlags.SHOW_EMPTY
    paths = [arg for arg in args if not arg.startswith("--")]
    if paths:
        tree = GMenu.Tree.new_for_path(paths[0], flags)
    else:
        tree = GMenu.Tree.new("applications.menu", flags)
    tree.load_sync()
    out = []
    walk(tree.get_root_directory(), out, menutest)
    print("\n".join(out))


main()
