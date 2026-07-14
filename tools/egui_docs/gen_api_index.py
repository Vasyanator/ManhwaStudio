#!/usr/bin/env python3
"""Generate `egui-docs/api/` from rustdoc JSON.

Purpose:
The `egui-docs/api/` tree is the *machine-extracted* half of the egui reference:
a grep-friendly index of the real public API of the egui-family crates the
project depends on. It exists so that an agent can answer "does this API
exist in our version?" by grepping a file instead of recalling a stale
version of egui from training data.

Nothing in the output is written by hand: every signature, doc line, and
source location comes from rustdoc JSON produced from the exact crate
sources in the local cargo registry.

Usage:
    tools/egui_docs/build.sh          # builds the JSON, then runs this

    python3 tools/egui_docs/gen_api_index.py \
        --json-dir target/doc --out egui-docs/api --stamp egui-docs/VERSION

Notes:
Targets rustdoc JSON `format_version` 60 (nightly ~2026-07). rustdoc JSON is
an unstable format; `--format-version` guards against silently misreading a
future schema.
"""

from __future__ import annotations

import argparse
import json
import logging
import re
import sys
from pathlib import Path
from typing import Any

log = logging.getLogger("gen_api_index")

# rustdoc JSON schema this generator was written against. A mismatch means the
# nightly toolchain moved; re-read the schema before trusting the output.
EXPECTED_FORMAT_VERSION = 60

# Crates indexed, in the order they appear in the generated README.
CRATES = ["egui", "eframe", "epaint", "emath", "ecolor", "egui_extras"]

# Item kinds that get a full entry (with signature + members).
TOPLEVEL_KINDS = {
    "struct",
    "enum",
    "trait",
    "function",
    "type_alias",
    "constant",
    "macro",
}

# Additionally listed in symbols.txt, though they get no full entry. Modules must be
# here: `egui` re-exports the `epaint` and `atomics` modules, so `egui::epaint::Vertex`
# is a path callers legitimately write. Omitting modules would make a grep for it come
# up empty and — under this tree's "absent means it does not exist" rule — produce a
# confident false negative.
SYMBOL_ONLY_KINDS = {"module"}


class RustdocCrate:
    """One crate's rustdoc JSON, with helpers to resolve ids and render types.

    `index` maps id -> item (only items local to this crate carry `inner`),
    `paths` maps id -> {crate_id, path, kind} for every item the crate can
    name, including foreign ones.
    """

    def __init__(self, name: str, data: dict[str, Any]) -> None:
        self.name = name
        self.version: str = data.get("crate_version") or "?"
        self.format_version: int = data["format_version"]
        # rustdoc emits ids as JSON object keys (strings) but references them
        # as ints; normalize to int so lookups from either side work.
        self.index: dict[int, Any] = {int(k): v for k, v in data["index"].items()}
        self.paths: dict[int, Any] = {int(k): v for k, v in data["paths"].items()}
        self.root: int = int(data["root"])
        # Source-file prefix to strip, so citations read
        # `egui-0.35.0/src/containers/panel.rs:238`.
        self._src_prefix = re.compile(r"^.*/(?=[a-z_0-9]+-\d[^/]*/)")
        # rustdoc's `paths` records where an item is *defined*
        # (`egui::containers::panel::Panel`), but callers write the *re-exported*
        # path (`egui::Panel`). Indexing by the definition path would make a
        # grep for `egui::Panel` come up empty and tell an agent the type does
        # not exist — the exact failure this whole tree exists to prevent. So
        # walk the public module tree and record the shortest reachable path.
        self.public_paths: dict[int, list[str]] = {}
        # Every public path an item is reachable by, not just the shortest. See `_record`.
        self.alias_paths: dict[int, list[list[str]]] = {}
        # Re-exports of items owned by another crate, collected during the walk and
        # resolved later by the Registry (which has every crate's JSON loaded).
        # (public path in this crate, definition path in the owning crate)
        self.foreign_reexports: list[tuple[list[str], tuple[str, ...]]] = []
        # `pub use other_crate::module::*;` — (module path here, module path there)
        self.foreign_globs: list[tuple[tuple[str, ...], tuple[str, ...]]] = []
        self._walk_public_tree()
        # Definition path -> id, so another crate can find items we own.
        self.by_def_path: dict[tuple[str, ...], int] = {
            tuple(entry["path"]): item_id
            for item_id, entry in self.paths.items()
            if entry.get("crate_id") == 0
        }

    def _walk_public_tree(self) -> None:
        """Populate `public_paths` with the shortest publicly reachable path per item."""
        root_item = self.index.get(self.root)
        if not root_item:
            return
        # (module_id, path_to_module). BFS, so shorter paths are seen first.
        queue: list[tuple[int, list[str]]] = [(self.root, [self.name])]
        seen_modules: set[tuple[int, tuple[str, ...]]] = set()

        while queue:
            mod_id, mod_path = queue.pop(0)
            key = (mod_id, tuple(mod_path))
            if key in seen_modules:
                continue
            seen_modules.add(key)

            mod_item = self.index.get(mod_id)
            if not mod_item:
                continue
            inner = mod_item.get("inner") or {}
            if "module" not in inner:
                continue

            for child_id in inner["module"].get("items", []):
                child_id = int(child_id)
                child = self.index.get(child_id)
                if not child or not is_public(child):
                    continue
                child_inner = child.get("inner") or {}

                if "use" in child_inner:
                    use = child_inner["use"]
                    target = use.get("id")
                    if target is None:
                        continue  # rustdoc could not resolve it; nothing to point at
                    target = int(target)
                    if target not in self.index:
                        # Cross-crate re-export: `egui` re-exports most of epaint
                        # and emath (Color32, Rect, Stroke, CornerRadius, …), and
                        # those are the symbols the codebase uses most. The item
                        # body lives in the other crate's JSON, so record the
                        # foreign reference and let the Registry stitch it up.
                        entry = self.paths.get(target)
                        if entry and not use.get("is_glob"):
                            self.foreign_reexports.append(
                                (mod_path + [use["name"]], tuple(entry["path"]))
                            )
                        elif entry and use.get("is_glob"):
                            self.foreign_globs.append((tuple(mod_path), tuple(entry["path"])))
                        continue
                    if use.get("is_glob"):
                        # `pub use foo::*;` — the target module's children become
                        # reachable at *this* module's path, not under its own name.
                        queue.append((target, mod_path))
                    else:
                        self._record(target, mod_path + [use["name"]])
                        # A re-exported module is itself a namespace: recurse so
                        # its children get the short path too.
                        tgt = self.index.get(target)
                        if tgt and "module" in (tgt.get("inner") or {}):
                            queue.append((target, mod_path + [use["name"]]))
                    continue

                name = child.get("name")
                if not name:
                    continue
                child_path = mod_path + [name]
                self._record(child_id, child_path)
                if "module" in child_inner:
                    queue.append((child_id, child_path))

    def _record(self, item_id: int, path: list[str]) -> None:
        """Record a public path for an item.

        `public_paths` keeps the shortest (then lexicographically smallest) one — that
        is the canonical path the per-crate pages group by. `alias_paths` keeps *every*
        public path, because a caller may legitimately write a longer one
        (`egui::epaint::Vertex` as well as `egui::Vertex`), and `symbols.txt` is read
        as "absent means it does not exist". A missing alias would be a false negative.
        """
        self.alias_paths.setdefault(item_id, []).append(path)
        prev = self.public_paths.get(item_id)
        if prev is None or (len(path), path) < (len(prev), prev):
            self.public_paths[item_id] = path

    def own_items(
        self, kinds: set[str] = TOPLEVEL_KINDS
    ) -> list[tuple[str, int, dict[str, Any], list[str]]]:
        """Public items this crate defines, of the requested kinds: (kind, id, item, path)."""
        out: list[tuple[str, int, dict[str, Any], list[str]]] = []
        for item_id, path in self.public_paths.items():
            item = self.index.get(item_id)
            if not item:
                continue
            entry = self.paths.get(item_id)
            kind = entry.get("kind") if entry else kind_of(item)
            if kind not in kinds:
                continue
            out.append((kind, item_id, item, path))
        return out


    def span(self, item: dict[str, Any]) -> str | None:
        """Return `crate-ver/src/f.rs:LINE` for an item, or None if rustdoc has no span."""
        sp = item.get("span")
        if not sp:
            return None
        filename = self._src_prefix.sub("", sp["filename"])
        return f"{filename}:{sp['begin'][0]}"

    def path_of(self, item_id: int) -> str | None:
        """Fully qualified path of an item id (`egui::containers::panel::Panel`)."""
        entry = self.paths.get(item_id)
        if not entry:
            return None
        return "::".join(entry["path"])

    # -- docs --------------------------------------------------------------

    @staticmethod
    def summary(item: dict[str, Any], limit: int = 170) -> str:
        """First paragraph of the doc comment, flattened to one line."""
        docs = item.get("docs") or ""
        para = docs.split("\n\n", 1)[0].strip()
        para = " ".join(para.split())
        if len(para) > limit:
            para = para[: limit - 1].rstrip() + "…"
        return para

    @staticmethod
    def deprecated(item: dict[str, Any]) -> str:
        """Marker text if the item is `#[deprecated]`, else empty."""
        dep = item.get("deprecation")
        if not dep:
            return ""
        note = dep.get("note") or ""
        note = " ".join(note.split())
        return f" **DEPRECATED**{': ' + note if note else ''}"

    # -- type rendering ----------------------------------------------------

    def ty(self, t: Any) -> str:  # noqa: C901 - a type printer is inherently a big match
        """Render a rustdoc JSON type node back to Rust-ish source text.

        Best-effort and lossy by design: the goal is a signature a human or
        agent can recognize and grep for, not a re-parseable AST.
        """
        if t is None:
            return "()"
        if isinstance(t, str):
            return t
        if not isinstance(t, dict) or len(t) != 1:
            return "?"

        (kind, v), = t.items()

        if kind == "primitive":
            return v
        if kind == "generic":
            return v
        if kind == "resolved_path":
            return self._path_with_args(v)
        if kind == "borrowed_ref":
            mut = "mut " if v.get("is_mutable") else ""
            lt = f"{v['lifetime']} " if v.get("lifetime") else ""
            return f"&{lt}{mut}{self.ty(v['type'])}"
        if kind == "raw_pointer":
            mut = "mut" if v.get("is_mutable") else "const"
            return f"*{mut} {self.ty(v['type'])}"
        if kind == "tuple":
            inner = ", ".join(self.ty(x) for x in v)
            return f"({inner})"
        if kind == "slice":
            return f"[{self.ty(v)}]"
        if kind == "array":
            return f"[{self.ty(v['type'])}; {v.get('len', '_')}]"
        if kind == "impl_trait":
            return "impl " + " + ".join(self._bound(b) for b in v)
        if kind == "dyn_trait":
            traits = " + ".join(self._path_with_args(x["trait"]) for x in v.get("traits", []))
            lt = v.get("lifetime")
            if lt:
                traits = f"{traits} + {lt}"
            return f"dyn {traits}"
        if kind == "qualified_path":
            self_ty = self.ty(v["self_type"])
            trait = v.get("trait")
            name = v["name"]
            if trait:
                return f"<{self_ty} as {self._path_with_args(trait)}>::{name}"
            return f"{self_ty}::{name}"
        if kind == "function_pointer":
            sig = v.get("sig", {})
            args = ", ".join(self.ty(a[1]) for a in sig.get("inputs", []))
            out = sig.get("output")
            ret = f" -> {self.ty(out)}" if out else ""
            return f"fn({args}){ret}"
        if kind == "infer":
            return "_"
        if kind == "pat":
            return self.ty(v.get("type"))
        return "?"

    def _path_with_args(self, p: dict[str, Any]) -> str:
        """Render a resolved path plus its generic args (`Into<Id>`, `Vec<Shape>`)."""
        name = p.get("path") or self.path_of(p.get("id", -1)) or "?"
        # rustdoc gives the full path for foreign items; keep only the tail so
        # signatures stay readable (`Id`, not `egui::id::Id`).
        name = name.split("::")[-1]
        return name + self._args(p.get("args"))

    def _args(self, args: Any) -> str:
        if not args:
            return ""
        if "angle_bracketed" in args:
            ab = args["angle_bracketed"]
            parts: list[str] = []
            for a in ab.get("args", []):
                if not isinstance(a, dict):
                    continue
                if "type" in a:
                    parts.append(self.ty(a["type"]))
                elif "lifetime" in a:
                    parts.append(a["lifetime"])
                elif "const" in a:
                    parts.append(str(a["const"].get("expr", "_")))
            for c in ab.get("constraints", []):
                binding = c.get("binding", {})
                if "equality" in binding:
                    eq = binding["equality"]
                    val = self.ty(eq["type"]) if "type" in eq else "?"
                    parts.append(f"{c['name']} = {val}")
            return f"<{', '.join(parts)}>" if parts else ""
        if "parenthesized" in args:
            pz = args["parenthesized"]
            inputs = ", ".join(self.ty(x) for x in pz.get("inputs", []))
            out = pz.get("output")
            ret = f" -> {self.ty(out)}" if out else ""
            return f"({inputs}){ret}"
        return ""

    def _bound(self, b: Any) -> str:
        if not isinstance(b, dict):
            return "?"
        if "trait_bound" in b:
            return self._path_with_args(b["trait_bound"]["trait"])
        if "outlives" in b:
            return b["outlives"]
        return "?"

    # -- signatures --------------------------------------------------------

    def fn_signature(self, item: dict[str, Any], qualifier: str = "") -> str:
        """Render a function/method item as `fn name(args) -> Ret`.

        `qualifier` is prepended to the name (e.g. `Panel::`) so the line is
        greppable as the call site would be written.
        """
        fn = item["inner"]["function"]
        sig = fn["sig"]
        header = fn.get("header") or {}

        args: list[str] = []
        for arg_name, arg_ty in sig.get("inputs", []):
            rendered = self.ty(arg_ty)
            # `self`, `&self`, `&mut self` come through as a normal input named
            # "self"; print them bare, the way they are written in Rust.
            if arg_name == "self":
                args.append(rendered.replace("Self", "self") if rendered != "Self" else "self")
            else:
                args.append(f"{arg_name}: {rendered}")
        if sig.get("is_c_variadic"):
            args.append("...")

        out = sig.get("output")
        ret = f" -> {self.ty(out)}" if out else ""

        prefix = ""
        if header.get("is_const"):
            prefix += "const "
        if header.get("is_unsafe"):
            prefix += "unsafe "
        if header.get("is_async"):
            prefix += "async "

        generics = self._generic_params(fn.get("generics"))
        return f"{prefix}fn {qualifier}{item['name']}{generics}({', '.join(args)}){ret}"

    def _generic_params(self, generics: Any) -> str:
        """Render declared generic params, skipping desugared `impl Trait` ones."""
        if not generics:
            return ""
        names: list[str] = []
        for p in generics.get("params", []):
            name = p.get("name", "")
            # rustdoc represents argument-position `impl Trait` as a synthetic
            # generic param literally named "impl Into<Id>"; it is already
            # rendered inline in the argument list, so drop it here.
            if name.startswith("impl ") or name in ("Self",):
                continue
            kind = p.get("kind") or {}
            if "lifetime" in kind:
                continue
            names.append(name)
        return f"<{', '.join(names)}>" if names else ""


# One entry of the final index: the crate that OWNS the item (whose source the
# citations point into), the item itself, and the path a caller actually writes.
IndexEntry = tuple[str, "RustdocCrate", int, dict[str, Any], list[str]]


class Registry:
    """All loaded crates, able to resolve cross-crate re-exports.

    `egui` re-exports most of `epaint` and `emath` (`Color32`, `Rect`, `Stroke`,
    `CornerRadius`, `Shape`, `Mesh`, `pos2`, …) — the very symbols the codebase
    uses most. Those items are *defined* in another crate's rustdoc JSON, so a
    per-crate walk alone would drop them and `symbols.txt` would wrongly claim
    `egui::Color32` does not exist. This class stitches them back together.
    """

    def __init__(self, crates: list[RustdocCrate]) -> None:
        self.crates = crates
        self.by_name = {c.name: c for c in crates}

    def _owner(self, def_path: tuple[str, ...]) -> tuple[RustdocCrate, int] | None:
        """Find the crate and item id that define `def_path` (`('epaint','Color32')`)."""
        if not def_path:
            return None
        owner = self.by_name.get(def_path[0])
        if not owner:
            return None  # a crate we do not index (e.g. `std`); out of scope
        item_id = owner.by_def_path.get(def_path)
        if item_id is None:
            return None
        return owner, item_id

    def entries_for(
        self, crate: RustdocCrate, kinds: set[str] = TOPLEVEL_KINDS
    ) -> list[IndexEntry]:
        """Every public item reachable under `crate`'s public paths, own or re-exported.

        `kinds` selects what to include: the per-crate pages want documentable items
        only, while `symbols.txt` additionally wants modules (see `SYMBOL_ONLY_KINDS`).
        """
        out: list[IndexEntry] = [
            (kind, crate, item_id, item, path)
            for kind, item_id, item, path in crate.own_items(kinds)
        ]
        seen_paths = {tuple(e[4]) for e in out}
        unresolved = 0

        for public_path, def_path in crate.foreign_reexports:
            resolved = self._owner(def_path)
            if not resolved:
                unresolved += 1
                continue
            owner, item_id = resolved
            item = owner.index.get(item_id)
            if not item:
                unresolved += 1
                continue
            kind = owner.paths[item_id].get("kind") or kind_of(item)
            if kind not in kinds:
                continue
            key = tuple(public_path)
            if key in seen_paths:
                continue
            seen_paths.add(key)
            out.append((kind, owner, item_id, item, public_path))

            # A re-exported module is a namespace callers can path through: `egui`
            # does `pub use epaint;`, so `egui::epaint::Vertex` is a real path. Walking
            # the module's rustdoc children does not work — a crate root's children are
            # `use` items, not the types themselves. Remap the owner's own public paths
            # instead: every `epaint::X` it exposes is reachable here as `egui::epaint::X`.
            if kind == "module":
                for owned_id, aliases in owner.alias_paths.items():
                    owned = owner.index.get(owned_id)
                    if not owned:
                        continue
                    oentry = owner.paths.get(owned_id)
                    okind = (oentry.get("kind") if oentry else kind_of(owned)) or "?"
                    if okind not in kinds:
                        continue
                    for alias in aliases:
                        if tuple(alias[: len(def_path)]) != def_path:
                            continue
                        ckey = tuple(public_path + alias[len(def_path) :])
                        if ckey in seen_paths:
                            continue
                        seen_paths.add(ckey)
                        out.append((okind, owner, owned_id, owned, list(ckey)))

        # `pub use other::module::*;` — lift the target module's public children
        # into the re-exporting module's namespace.
        for here, there in crate.foreign_globs:
            resolved = self._owner(there)
            if not resolved:
                unresolved += 1
                continue
            owner, mod_id = resolved
            mod_item = owner.index.get(mod_id)
            if not mod_item or "module" not in (mod_item.get("inner") or {}):
                continue
            for child_id in mod_item["inner"]["module"].get("items", []):
                child_id = int(child_id)
                child = owner.index.get(child_id)
                if not child or not is_public(child) or not child.get("name"):
                    continue
                entry = owner.paths.get(child_id)
                kind = (entry.get("kind") if entry else kind_of(child)) or "?"
                if kind not in TOPLEVEL_KINDS:
                    continue
                key = tuple(list(here) + [child["name"]])
                if key in seen_paths:
                    continue
                seen_paths.add(key)
                out.append((kind, owner, child_id, child, list(key)))

        if unresolved:
            log.info(
                "%s: %d re-export(s) point outside the indexed crate set (expected for "
                "std/ahash/serde types); skipped",
                crate.name,
                unresolved,
            )
        return out

    # -- locations ---------------------------------------------------------

def is_public(item: dict[str, Any]) -> bool:
    """True if rustdoc marked the item `pub` (rustdoc already strips private ones)."""
    vis = item.get("visibility")
    return vis == "public" or vis == "default" or isinstance(vis, dict)


def kind_of(item: dict[str, Any]) -> str:
    """Item kind as spelled in rustdoc's `inner` tag (fallback when `paths` lacks the id)."""
    inner = item.get("inner") or {}
    return next(iter(inner), "?")


def render_crate(crate: RustdocCrate, entries: list[IndexEntry]) -> str:
    """Render one crate's full public-API index as markdown, keyed by public path."""
    lines: list[str] = []
    push = lines.append

    push(f"# API index: `{crate.name}` {crate.version}")
    push("")
    push(
        "GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. "
        "Extracted from rustdoc JSON of the exact crate source in the local cargo registry, "
        "so every signature and line number below is real."
    )
    push("")
    push(
        "**If a name is not in this file, it does not exist in our version of the crate.** "
        "Grep here before writing egui code from memory."
    )
    push("")
    push(
        "Items are listed under the path callers actually write (the public re-export, "
        "e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. "
        "Citations point into the crate that owns the item, so a type `egui` re-exports "
        "from `epaint` cites `epaint-0.35.0/src/…`."
    )
    push("")

    by_module: dict[str, list[IndexEntry]] = {}
    for entry in entries:
        module = "::".join(entry[4][:-1]) or crate.name
        by_module.setdefault(module, []).append(entry)

    for module in sorted(by_module, key=lambda m: (m.count("::"), m)):
        push(f"## `{module}`")
        push("")
        for kind, owner, item_id, item, path in sorted(
            by_module[module], key=lambda e: (e[0], e[4][-1])
        ):
            render_item(owner, kind, item_id, item, path, push)
        push("")

    return "\n".join(lines) + "\n"


def render_item(
    crate: RustdocCrate,
    kind: str,
    item_id: int,
    item: dict[str, Any],
    path: list[str],
    push: Any,
) -> None:
    """Render one top-level item plus its inherent methods / variants / fields."""
    name = path[-1]
    loc = crate.span(item)
    loc_s = f" — `{loc}`" if loc else ""
    doc = crate.summary(item)
    dep = crate.deprecated(item)

    if kind == "function":
        push(f"### `{name}`{loc_s}")
        push("")
        push("```rust")
        push(crate.fn_signature(item))
        push("```")
        if doc or dep:
            push("")
            push(f"{doc}{dep}")
        push("")
        return

    push(f"### `{name}` ({kind}){loc_s}")
    push("")
    if doc or dep:
        push(f"{doc}{dep}")
        push("")

    inner = item["inner"]

    if kind == "enum":
        variant_ids = inner["enum"].get("variants", [])
        if variant_ids:
            push("Variants:")
            push("")
            for vid in variant_ids:
                v = crate.index.get(int(vid))
                if not v:
                    continue
                vdoc = crate.summary(v, 100)
                push(f"- `{name}::{v['name']}`" + (f" — {vdoc}" if vdoc else ""))
            push("")

    if kind == "struct":
        skind = inner["struct"].get("kind")
        if isinstance(skind, dict) and "plain" in skind:
            field_ids = skind["plain"].get("fields", [])
            fields: list[str] = []
            for fid in field_ids:
                f = crate.index.get(int(fid))
                if not f or not is_public(f):
                    continue
                fty = crate.ty(f["inner"]["struct_field"])
                fdoc = crate.summary(f, 90)
                fields.append(f"- `{f['name']}: {fty}`" + (f" — {fdoc}" if fdoc else ""))
            if fields:
                push("Public fields:")
                push("")
                lines_out = fields
                for line in lines_out:
                    push(line)
                push("")

    # Inherent methods (impls with no trait), and the list of traits implemented.
    impl_ids = inner.get(kind, {}).get("impls", []) if kind in ("struct", "enum") else []
    methods: list[str] = []
    traits: list[str] = []
    for iid in impl_ids:
        imp_item = crate.index.get(int(iid))
        if not imp_item:
            continue
        imp = imp_item["inner"]["impl"]
        if imp.get("blanket_impl") or imp.get("is_synthetic"):
            continue
        trait_ref = imp.get("trait")
        if trait_ref is not None:
            traits.append(crate._path_with_args(trait_ref))
            continue
        for mid in imp.get("items", []):
            m = crate.index.get(int(mid))
            if not m or not is_public(m):
                continue
            m_inner = m.get("inner") or {}
            if "function" not in m_inner:
                continue
            sig = crate.fn_signature(m, qualifier="")
            mloc = crate.span(m)
            mdoc = crate.summary(m, 110)
            mdep = crate.deprecated(m)
            entry = f"- `{sig}`"
            if mloc:
                entry += f" — `{mloc}`"
            if mdoc:
                entry += f"\n  {mdoc}"
            if mdep:
                entry += f"\n  {mdep.strip()}"
            methods.append(entry)

    if kind == "trait":
        push("Required/provided items:")
        push("")
        for mid in inner["trait"].get("items", []):
            m = crate.index.get(int(mid))
            if not m:
                continue
            m_inner = m.get("inner") or {}
            if "function" not in m_inner:
                continue
            sig = crate.fn_signature(m)
            mloc = crate.span(m)
            mdoc = crate.summary(m, 110)
            entry = f"- `{sig}`" + (f" — `{mloc}`" if mloc else "")
            if mdoc:
                entry += f"\n  {mdoc}"
            push(entry)
        push("")

    if methods:
        push("Methods:")
        push("")
        for m in sorted(methods):
            push(m)
        push("")

    if traits:
        uniq = sorted(set(traits))
        push(f"Implements: {', '.join(f'`{t}`' for t in uniq)}")
        push("")


def render_symbols(per_crate: dict[str, list[IndexEntry]]) -> str:
    """Flat, one-symbol-per-line list across all crates — the fastest grep target."""
    out: list[str] = [
        "# Flat symbol list — every public item of the egui-family crates we depend on,",
        "# listed under the path a caller writes (re-exports resolved).",
        "# GENERATED by tools/egui_docs/gen_api_index.py. Do not edit.",
        "# Format: <path>\t<kind>\t<source location>",
        "#",
        "# A name absent from this file DOES NOT EXIST in the version we build against.",
        "# Grep here before writing egui code from memory.",
        "",
    ]
    rows: list[str] = []
    for crate_name, entries in per_crate.items():
        for kind, owner, item_id, item, path_parts in entries:
            path = "::".join(path_parts)
            loc = owner.span(item) or "-"
            dep_flag = "deprecated-" if item.get("deprecation") else ""
            rows.append(f"{path}\t{dep_flag}{kind}\t{loc}")

            # Alternative public paths to the same item, e.g. `egui::text::LayoutJob`
            # beside `egui::LayoutJob`. Callers write either; a grep must find both.
            if owner.name == crate_name:
                for alias in owner.alias_paths.get(item_id, []):
                    if alias == path_parts:
                        continue
                    rows.append(f"{'::'.join(alias)}\t{dep_flag}{kind}-alias\t{loc}")

            inner = item.get("inner") or {}

            # Enum variants, so `grep 'StrokeKind::Outside'` hits.
            if kind == "enum":
                for vid in inner["enum"].get("variants", []):
                    v = owner.index.get(int(vid))
                    if v:
                        rows.append(f"{path}::{v['name']}\tvariant\t{owner.span(v) or '-'}")

            # Trait methods, so `grep 'App::ui'` hits.
            if kind == "trait":
                for mid in inner["trait"].get("items", []):
                    m = owner.index.get(int(mid))
                    if m and "function" in (m.get("inner") or {}):
                        rows.append(f"{path}::{m['name']}\ttrait-method\t{owner.span(m) or '-'}")

            # Inherent methods, so `grep 'Panel::top'` hits.
            impl_ids = inner.get(kind, {}).get("impls", []) if kind in ("struct", "enum") else []
            for iid in impl_ids:
                imp_item = owner.index.get(int(iid))
                if not imp_item:
                    continue
                imp = imp_item["inner"]["impl"]
                if imp.get("trait") is not None or imp.get("blanket_impl"):
                    continue
                for mid in imp.get("items", []):
                    m = owner.index.get(int(mid))
                    if not m or not is_public(m):
                        continue
                    if "function" not in (m.get("inner") or {}):
                        continue
                    mloc = owner.span(m) or "-"
                    dep = "deprecated-method" if m.get("deprecation") else "method"
                    rows.append(f"{path}::{m['name']}\t{dep}\t{mloc}")

    out.extend(sorted(set(rows)))
    return "\n".join(out) + "\n"


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")

    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--json-dir", type=Path, default=Path("target/doc"))
    ap.add_argument("--out", type=Path, default=Path("egui-docs/api"))
    ap.add_argument("--stamp", type=Path, default=Path("egui-docs/VERSION"))
    ap.add_argument(
        "--format-version",
        type=int,
        default=EXPECTED_FORMAT_VERSION,
        help="rustdoc JSON schema version this generator understands",
    )
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)

    loaded: list[RustdocCrate] = []
    for name in CRATES:
        path = args.json_dir / f"{name}.json"
        if not path.is_file():
            log.error(
                "Missing rustdoc JSON.\n"
                "  Expected: %s\n"
                "  Cause: the JSON was never built, or the build failed.\n"
                "  Fix: run tools/egui_docs/build.sh (needs the nightly toolchain).",
                path,
            )
            return 1
        data = json.loads(path.read_text(encoding="utf-8"))
        crate = RustdocCrate(name, data)
        if crate.format_version != args.format_version:
            log.error(
                "rustdoc JSON schema mismatch for crate %s.\n"
                "  Found format_version: %d\n"
                "  Generator understands: %d\n"
                "  Cause: the nightly toolchain changed the unstable rustdoc JSON schema.\n"
                "  Fix: re-read the schema, update gen_api_index.py, then bump "
                "EXPECTED_FORMAT_VERSION.",
                name,
                crate.format_version,
                args.format_version,
            )
            return 1
        loaded.append(crate)
        log.info("loaded %s %s (%d items)", name, crate.version, len(crate.index))

    registry = Registry(loaded)
    # Pages document items; symbols.txt additionally lists modules, so that a path a
    # caller can legitimately write (`egui::epaint::Vertex`) is never reported absent.
    per_crate: dict[str, list[IndexEntry]] = {
        crate.name: registry.entries_for(crate) for crate in loaded
    }
    symbol_kinds = TOPLEVEL_KINDS | SYMBOL_ONLY_KINDS
    per_crate_symbols: dict[str, list[IndexEntry]] = {
        crate.name: registry.entries_for(crate, symbol_kinds) for crate in loaded
    }

    for crate in loaded:
        entries = per_crate[crate.name]
        target = args.out / f"{crate.name}.md"
        target.write_text(render_crate(crate, entries), encoding="utf-8")
        log.info("wrote %s (%d public items)", target, len(entries))

    symbols = args.out / "symbols.txt"
    symbols.write_text(render_symbols(per_crate_symbols), encoding="utf-8")
    log.info("wrote %s", symbols)

    readme = args.out / "README.md"
    readme.write_text(render_api_readme(loaded, per_crate), encoding="utf-8")
    log.info("wrote %s", readme)

    versions = {c.name: c.version for c in loaded}
    args.stamp.write_text(render_stamp(versions), encoding="utf-8")
    log.info("wrote %s", args.stamp)
    return 0


def render_api_readme(crates: list[RustdocCrate], per_crate: dict[str, list[IndexEntry]]) -> str:
    """Entry point for the generated half of the docs."""
    rows = "\n".join(
        f"| `{c.name}` | {c.version} | [`{c.name}.md`]({c.name}.md) "
        f"| {len(per_crate[c.name])} |"
        for c in crates
    )
    return f"""# `egui-docs/api/` — generated API index

GENERATED DIRECTORY — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`.

Everything here is extracted from rustdoc JSON built from the exact crate
sources in the local cargo registry. No part of it is written from memory, so
it is the authoritative answer to "does this API exist in our version?".

## Rule

**A name that does not appear in `symbols.txt` does not exist in the version we
depend on.** Grep before you write.

```bash
grep -n 'SidePanel'      egui-docs/api/symbols.txt   # no hits -> it does not exist
grep -n 'Panel::top'     egui-docs/api/symbols.txt   # -> egui::Panel::top  method  egui-0.35.0/src/containers/panel.rs:238
grep -rn 'fn rect_stroke' egui-docs/api/epaint.md    # -> exact 0.35 signature
```

## Contents

| Crate | Version | Index | Items |
|---|---|---|---|
{rows}

- `symbols.txt` — flat `path <TAB> kind <TAB> source location` list across all
  crates above. One line per public item and per inherent method. This is the
  fastest existence check.
- `<crate>.md` — per-crate index grouped by module: signatures, public fields,
  enum variants, inherent methods, implemented traits, and the first line of
  each doc comment, each with a `file:line` citation into the crate source.

## What this does NOT cover

Prose, rationale, project conventions, and migration traps live in the
hand-written pages one level up (`egui-docs/00-version-map.md` and friends).
This directory is a dictionary, not a guide.
"""


def render_stamp(versions: dict[str, str]) -> str:
    """The version stamp that `check_sync.py` compares against Cargo.lock."""
    lines = [
        "# Versions the egui-docs/ tree was generated from.",
        "# GENERATED by tools/egui_docs/gen_api_index.py. Do not edit by hand.",
        "# tools/egui_docs/check_sync.py fails if these drift from Cargo.lock.",
        "",
    ]
    lines.extend(f"{name}={version}" for name, version in sorted(versions.items()))
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    sys.exit(main())
