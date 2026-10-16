#!/usr/bin/env python3
"""Check that relative links and heading anchors in the Markdown files resolve.

    python3 scripts/check-doc-links.py            # every tracked *.md
    python3 scripts/check-doc-links.py docs README.md

Only local links are checked: files, directories and #anchors. External http(s) links are
skipped, so the check is fast and needs no network. Links inside code blocks and inline code
are ignored. Exits 1 and lists each broken link when there are any.
"""
import pathlib
import re
import subprocess
import sys
import unicodedata

ROOT = pathlib.Path(__file__).resolve().parent.parent

INLINE_LINK = re.compile(r"!?\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
REF_DEF = re.compile(r"^\[[^\]]+\]:\s*(\S+)", re.M)
FENCE = re.compile(r"^(```|~~~)")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")


def slug(text: str) -> str:
    """GitHub's heading anchor: lowercase, drop punctuation, spaces to hyphens."""
    text = re.sub(r"`", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)  # keep link text only
    text = unicodedata.normalize("NFKD", text).lower().strip()
    out = []
    for ch in text:
        if ch.isalnum() or ch in "-_":
            out.append(ch)
        elif ch.isspace():
            out.append("-")
    return "".join(out)


def strip_code(text: str) -> str:
    """Blank out fenced blocks and inline code so their contents are not read as links."""
    lines, fenced, kept = text.split("\n"), False, []
    for line in lines:
        if FENCE.match(line.lstrip()):
            fenced = not fenced
            kept.append("")
            continue
        kept.append("" if fenced else re.sub(r"`[^`]*`", "", line))
    return "\n".join(kept)


def anchors_of(path: pathlib.Path) -> set:
    seen, result, fenced = {}, set(), False
    for line in path.read_text().split("\n"):
        if FENCE.match(line.lstrip()):
            fenced = not fenced
            continue
        if fenced:
            continue
        m = HEADING.match(line)
        if m:
            base = slug(m.group(2))
            n = seen.get(base, 0)
            seen[base] = n + 1
            result.add(base if n == 0 else f"{base}-{n}")
    return result


def tracked_markdown() -> list:
    out = subprocess.run(
        ["git", "ls-files", "*.md"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.split("\n")
    # Also files not yet added, so the check is useful while writing.
    out += subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard", "*.md"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout.split("\n")
    # site/ holds the website's own pages, which link with site URLs (/docs/...) that are not
    # files. The site build validates those links itself.
    return sorted(
        {ROOT / p for p in out if p and not p.startswith("site/") and (ROOT / p).exists()}
    )


def select(args: list) -> list:
    if not args:
        return tracked_markdown()
    chosen = []
    for arg in args:
        p = (ROOT / arg) if not pathlib.Path(arg).is_absolute() else pathlib.Path(arg)
        if p.is_dir():
            chosen += sorted(p.rglob("*.md"))
        else:
            chosen.append(p)
    return chosen


def main() -> int:
    anchor_cache, problems = {}, []
    files = select(sys.argv[1:])
    for md in files:
        text = strip_code(md.read_text())
        targets = INLINE_LINK.findall(text) + REF_DEF.findall(text)
        for target in targets:
            if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.I) or target.startswith("//"):
                continue  # http(s):, mailto:, etc.
            path_part, _, fragment = target.partition("#")
            dest = md if not path_part else (md.parent / path_part.split("?")[0]).resolve()
            where = f"{md.relative_to(ROOT)}"
            if not dest.exists():
                problems.append(f"{where}: {target}  (no such file)")
                continue
            if fragment and dest.suffix == ".md":
                if dest not in anchor_cache:
                    anchor_cache[dest] = anchors_of(dest)
                if fragment.lower() not in anchor_cache[dest]:
                    problems.append(f"{where}: {target}  (no such heading in {dest.relative_to(ROOT)})")
    for p in problems:
        print(p)
    print(f"checked {len(files)} files, {len(problems)} broken link(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
