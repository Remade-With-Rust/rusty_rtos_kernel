#!/usr/bin/env python3
"""The unsafe census: every fence in the source is in UNSAFE.md.

Hardening gate H-22 (static analysis beyond the default linter), and the
"unsafe census is a release gate" clause of the mission plan's security
doctrine. Two halves, one rule each:

1. The COMPILER guarantees every `unsafe` is fenced: the workspace denies
   `unsafe_code`, so an `unsafe` block, fn or impl compiles only inside an
   item or statement carrying `#[expect(unsafe_code, reason = "...")]`.
2. THIS script guarantees every fence is inventoried: the function (or
   other item) each fence belongs to must be named, in backticks, in
   UNSAFE.md. A new fence nobody wrote up fails CI, so the inventory cannot
   drift from the code.

A fence on an item belongs to that item; a fence on a statement (an
`unsafe` block inside a function) belongs to the enclosing function. A
crate whose lib.rs says `#![forbid(unsafe_code)]` must have no fences at
all. Stdlib only, so CI needs nothing but Python.

    python3 tools/unsafe_census.py            # from the repository root
"""

from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
FENCE = re.compile(r"#\[(?:expect|allow)\(\s*unsafe_code")
EXPORT = re.compile(r'export_name\s*=\s*"([A-Za-z_][A-Za-z0-9_]*)"')
ITEM = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|unsafe\s+|extern\s+\"C\"\s+)*"
    r"(?:fn|static|struct|enum|trait|mod)\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)"
)
IMPL = re.compile(r"^\s*(?:unsafe\s+)?impl\b[^{]*?\bfor\s+([A-Za-z_][A-Za-z0-9_]*)")
GLOBAL_ASM = re.compile(r"^\s*(?:core::arch::)?global_asm!")
FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def fences(path: pathlib.Path) -> list[tuple[int, str]]:
    """(line, owner) for every unsafe_code fence in a source file."""
    lines = path.read_text(encoding="utf-8").splitlines()
    found = []
    for i, line in enumerate(lines):
        if line.lstrip().startswith("//"):
            continue
        # An attribute may span lines (`#[expect(` / `unsafe_code,` /
        # `reason = ...` / `)]`). Join it before matching: the first version
        # of this script matched one line at a time and silently skipped
        # every fence rustfmt had wrapped.
        text, end = line, i
        if re.match(r"^\s*#\[(?:expect|allow)\(", line) and ")]" not in line:
            while end + 1 < len(lines) and ")]" not in lines[end]:
                end += 1
                text += " " + lines[end].strip()
        if not FENCE.search(text):
            continue
        # Skip the attributes and comments between the fence and what it is
        # on, remembering an `export_name`, which is how a handler is named.
        j = end + 1
        exported = None
        while j < len(lines) and lines[j].lstrip().startswith(("#[", "//")):
            m = EXPORT.search(lines[j])
            if m:
                exported = m.group(1)
            j += 1
        nxt = lines[j] if j < len(lines) else ""
        owner = None
        if exported:
            owner = exported
        elif m := ITEM.match(nxt):
            owner = m.group(1)
        elif m := IMPL.match(nxt):
            owner = m.group(1)
        elif GLOBAL_ASM.match(nxt):
            owner = "global_asm!"
        else:
            # A statement: the enclosing function owns it.
            # From the line ABOVE the fence, past attributes and comments:
            # a fence's own `reason = "... a fn pointer"` is not a function.
            for k in range(i - 1, -1, -1):
                if lines[k].lstrip().startswith(("#[", "//")):
                    continue
                m = FN.search(lines[k])
                if m:
                    owner = m.group(1)
                    break
        found.append((i + 1, owner or "?"))
    return found


def sections(inventory: str) -> dict[str, set[str]]:
    """The names each `## \\`crate\\`` section of UNSAFE.md mentions.

    Per crate, so a row about one port's `new_task_context` cannot vouch for
    another port's. Backticks are paired per LINE: one stray backtick
    anywhere would otherwise shift every pair after it. Every word inside a
    span counts, so a row naming `backend::thaw` covers `thaw`.
    """
    named: dict[str, set[str]] = {}
    current: set[str] | None = None
    for line in inventory.splitlines():
        head = re.match(r"^##\s+`([^`]+)`", line)
        if head:
            current = named.setdefault(head.group(1), set())
            continue
        if line.startswith("## "):
            current = None
            continue
        if current is None:
            continue
        for tick in re.findall(r"`([^`]+)`", line):
            current.add(tick)
            current.update(re.findall(r"[A-Za-z_][A-Za-z0-9_!]*", tick))
    return named


def main() -> int:
    md = ROOT / "UNSAFE.md"
    named = sections(md.read_text(encoding="utf-8") if md.exists() else "")
    failures = []
    total = 0
    crates = sorted(ROOT.glob("crates/*/src/lib.rs"))
    for lib in crates:
        crate = lib.parent.parent
        name = crate.name
        forbids = "#![forbid(unsafe_code)]" in lib.read_text(encoding="utf-8")
        for src in sorted(crate.glob("src/**/*.rs")):
            for line, owner in fences(src):
                total += 1
                where = f"{src.relative_to(ROOT).as_posix()}:{line}"
                if forbids:
                    failures.append(f"{where}: a fence in a crate that forbids unsafe_code")
                elif owner == "?":
                    failures.append(f"{where}: a fence this script cannot attribute to an item")
                elif name not in named:
                    failures.append(f"{where}: `{owner}` is fenced, and UNSAFE.md has no `## \\`{name}\\`` section")
                elif owner not in named[name]:
                    failures.append(f"{where}: `{owner}` is fenced but not in UNSAFE.md's `{name}` section")
    print(f"unsafe census: {total} fence(s) across {len(crates)} crate(s)")
    for f in failures:
        print(f"  FAIL  {f}")
    if failures:
        print("Every unsafe_code fence must be written up in UNSAFE.md: what it does and why it is sound.")
        return 1
    print("  ok    every fence is inventoried in UNSAFE.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
