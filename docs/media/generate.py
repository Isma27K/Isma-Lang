#!/usr/bin/env python3
"""Regenerate every image in docs/media from real runs of Tarn.

    cargo build --release && python3 docs/media/generate.py

- Terminal images are recordings of real commands: each command runs in a
  pseudo-terminal and its ANSI output is turned into SVG, recolored for a
  white page.
- The benchmark chart reads bench/results.json, written by bench/run.sh.
- The banner, mark, badges, pipeline diagram and the editor icon are drawn
  here.

The look is a white room: white cards, hairline borders, near-black text,
grays for everything secondary, and nothing louder than that.
"""

import html
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "docs" / "media"
TARN = ROOT / "target" / "release" / "tarn"

MONO = "'JetBrains Mono','SF Mono','Fira Code','DejaVu Sans Mono',Menlo,Consolas,monospace"
SANS = "Inter,'SF Pro Text','Helvetica Neue',Helvetica,Arial,system-ui,sans-serif"
CW, LH, FS = 8.1, 20, 13.5  # terminal: char width, line height, font size

# The palette.
INK = "#111111"      # text, and the one thing that should stand out
GRAY = "#6e6e6e"     # secondary text
FAINT = "#a8a8a8"    # tertiary text, prompts
LINE = "#e6e6e6"     # hairlines
PAPER = "#ffffff"


def esc(s: str) -> str:
    return html.escape(s, quote=True)


# ------------------------------------------------------------------ capture

def capture(cmd: str, cwd: Path, env_extra: dict | None = None) -> str:
    """Run `cmd` in a pseudo-terminal (so programs print colors) and return its output."""
    env = {**os.environ, "TERM": "xterm-256color", "COLUMNS": "110"}
    env["PATH"] = f"{TARN.parent}:{env['PATH']}"  # the freshly built tarn, unless overridden
    env.update(env_extra or {})
    env.pop("NO_COLOR", None)
    r = subprocess.run(["script", "-qec", cmd, "/dev/null"], cwd=cwd, env=env, capture_output=True, timeout=600)
    # (Decoded by hand: text mode would turn a lone `\r`, which redraws a line, into a newline.)
    return r.stdout.decode("utf-8", "replace").replace("\r\n", "\n").rstrip("\n")


# ------------------------------------------------------------------ ANSI -> styled runs

# Terminal colors, deepened so they read on white.
BASIC = {
    30: "#3a3a3a", 31: "#c4362c", 32: "#2f7d4f", 33: "#a86a12", 34: "#2f5fa8", 35: "#8e4a9e", 36: "#2a7a7a", 37: "#6e6e6e",
    90: "#8a8a8a", 91: "#c4362c", 92: "#2f7d4f", 93: "#a86a12", 94: "#2f5fa8", 95: "#8e4a9e", 96: "#2a7a7a", 97: INK,
}


def xterm256(n: int) -> str:
    """A 256-color code, for a white background: grays are inverted (a light
    gray meant to stand out on black becomes a dark one), colors darkened."""
    if n < 16:
        return BASIC[(30 + n) if n < 8 else (90 + n - 8)]
    if n < 232:
        n -= 16
        levels = [0, 95, 135, 175, 215, 255]
        r, g, b = (levels[n // 36] * 0.55, levels[(n // 6) % 6] * 0.55, levels[n % 6] * 0.55)
        return "#%02x%02x%02x" % (int(r), int(g), int(b))
    v = 8 + (n - 232) * 10
    w = max(0x2a, min(0xb8, 255 - v + 10))
    return f"#{w:02x}{w:02x}{w:02x}"


CSI = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")


def parse_ansi(text: str) -> list[list[tuple[str, dict]]]:
    """Split ANSI text into lines of (text, style) runs. Styles persist across lines."""
    style = {"fg": None, "bold": False, "dim": False}
    lines = []
    for raw in text.split("\n"):
        if "\r" in raw:
            raw = raw.split("\r")[-1]  # carriage return: later text overwrites the line
        runs, pos = [], 0
        for m in CSI.finditer(raw):
            if m.start() > pos:
                runs.append((raw[pos:m.start()], dict(style)))
            pos = m.end()
            if m.group(2) != "m":
                continue
            codes = [int(c) if c.isdigit() else 0 for c in (m.group(1) or "0").split(";")]
            i = 0
            while i < len(codes):
                c = codes[i]
                if c == 0:
                    style = {"fg": None, "bold": False, "dim": False}
                elif c == 1:
                    style["bold"] = True
                elif c == 2:
                    style["dim"] = True
                elif c == 22:
                    style["bold"] = style["dim"] = False
                elif c in BASIC:
                    style["fg"] = BASIC[c]
                elif c == 39:
                    style["fg"] = None
                elif c in (38, 48) and i + 1 < len(codes):
                    if codes[i + 1] == 5 and i + 2 < len(codes):
                        if c == 38:
                            style["fg"] = xterm256(codes[i + 2])
                        i += 2
                    elif codes[i + 1] == 2 and i + 4 < len(codes):
                        if c == 38:
                            style["fg"] = "#%02x%02x%02x" % tuple(int(x * 0.55) for x in codes[i + 2:i + 5])
                        i += 4
                i += 1
        if pos < len(raw):
            runs.append((raw[pos:], dict(style)))
        lines.append(runs)
    return lines


def runs_svg(runs, x0: float, y: float) -> tuple[str, int]:
    """One terminal line as <tspan>s. Non-ASCII glyphs get their own x so a
    fallback font can never shift the columns after them."""
    parts, col = [], 0
    for text, st in runs:
        for chunk in re.findall(r"[\x20-\x7e]+|[^\x20-\x7e]", text):
            if not chunk.strip():
                col += len(chunk)
                continue
            attrs = [f'x="{x0 + col * CW:.1f}"']
            if st["dim"]:
                attrs.append(f'fill="{FAINT}"')
            elif st["fg"]:
                attrs.append(f'fill="{st["fg"]}"')
            if st["bold"]:
                attrs.append('font-weight="600"')
            parts.append(f"<tspan {' '.join(attrs)}>{esc(chunk)}</tspan>")
            col += len(chunk)
    return f'<text y="{y:.1f}" xml:space="preserve">{"".join(parts)}</text>', col


# ------------------------------------------------------------------ terminal window

# Fail-safe animation: everything is visible by default; the keyframes only
# hide a line while it waits for its turn (fill-mode `both`). Where animation
# doesn't run (some previewers, image proxies), the full output still shows.
TERM_CSS = """
.r{animation:show .01s linear both}
.c{animation:blink 1.1s steps(1) infinite}
@keyframes show{from{opacity:0}to{opacity:1}}
@keyframes type{from{clip-path:inset(0 100% 0 0)}to{clip-path:inset(0 0 0 0)}}
@keyframes blink{50%{opacity:0}}
@media (prefers-reduced-motion:reduce){.r,.t,.c{animation:none!important}}
"""


def terminal_svg(title: str, script: list[tuple[str, str]], *, line_step: float = 0.035, min_cols: int = 72) -> str:
    """A white terminal window. `script` is a list of ("cmd", command) / ("out", ansi_text) entries."""
    rows = []  # (kind, payload)
    for kind, payload in script:
        if kind == "cmd":
            rows.append(("cmd", payload))
        elif payload.strip():
            lines = parse_ansi(payload)
            while lines and not "".join(t for t, _ in lines[-1]).strip():
                lines.pop()  # no trailing blank lines before the next prompt
            rows.extend(("out", runs) for runs in lines)
    rows.append(("prompt", ""))
    cols = max([min_cols] + [len(p) + 2 for k, p in rows if k == "cmd"] + [sum(len(t) for t, _ in p) for k, p in rows if k == "out"])
    pad, bar, m = 28, 40, 10  # padding, title bar, margin for the shadow
    w = round(pad * 2 + cols * CW)
    h = round(bar + pad + len(rows) * LH + pad - 6)
    body, t = [], 0.4
    for i, (kind, payload) in enumerate(rows):
        y = bar + pad + i * LH + FS - 2
        if kind == "cmd":
            typing = max(0.3, 0.035 * len(payload))
            text, _ = runs_svg([(payload, {"fg": INK, "bold": False, "dim": False})], pad + 2 * CW, y)
            body.append(f'<g class="r" style="animation-delay:{t:.2f}s"><text x="{pad}" y="{y:.1f}" fill="{FAINT}">$</text></g>')
            body.append(f'<g class="t" style="animation:type {typing:.2f}s steps({max(1, len(payload))}) {t + 0.15:.2f}s both">{text}</g>')
            t += typing + 0.45
        elif kind == "out":
            text, _ = runs_svg(payload, pad, y)
            body.append(f'<g class="r" style="animation-delay:{t:.2f}s">{text}</g>')
            t += line_step
        else:
            body.append(
                f'<g class="r" style="animation-delay:{t + 0.2:.2f}s"><text x="{pad}" y="{y:.1f}" fill="{FAINT}">$</text>'
                f'<rect class="c" x="{pad + 2 * CW:.1f}" y="{y - FS + 2:.1f}" width="1.6" height="{FS + 3}" fill="{INK}"/></g>'
            )
    dots = "".join(f'<circle cx="{m + 22 + i * 16}" cy="{m + bar / 2}" r="4.5" fill="{LINE}"/>' for i in range(3))
    W, H = w + 2 * m, h + 2 * m
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="{esc(title)}">
<title>{esc(title)}</title>
<defs><filter id="s" x="-10%" y="-10%" width="120%" height="130%"><feDropShadow dx="0" dy="2" stdDeviation="4" flood-color="#000" flood-opacity=".05"/></filter></defs>
<style>{TERM_CSS}</style>
<rect x="{m}" y="{m}" width="{w}" height="{h}" rx="14" fill="{PAPER}" stroke="{LINE}" filter="url(#s)"/>
<line x1="{m}" y1="{m + bar}" x2="{m + w}" y2="{m + bar}" stroke="#f0f0ef"/>
{dots}
<text x="{m + w / 2}" y="{m + bar / 2 + 4}" text-anchor="middle" font-family="{SANS}" font-size="12" fill="{FAINT}">{esc(title)}</text>
<g transform="translate({m} {m})" font-family="{MONO}" font-size="{FS}" fill="#2b2b2b">
{chr(10).join(body)}
</g>
</svg>
"""


# ------------------------------------------------------------------ the mark

def mark(cx: float, cy: float, r: float, uid: str = "m", color: str = INK) -> str:
    """A still lake: a disc above a thin horizon, and its faint reflection below."""
    gap_top, gap_bot = cy + 0.14 * r, cy + 0.30 * r
    return (
        f'<clipPath id="{uid}"><circle cx="{cx}" cy="{cy}" r="{r}"/></clipPath>'
        f'<g clip-path="url(#{uid})">'
        f'<rect x="{cx - r}" y="{cy - r}" width="{2 * r}" height="{gap_top - (cy - r):.2f}" fill="{color}"/>'
        f'<rect x="{cx - r}" y="{gap_bot:.2f}" width="{2 * r}" height="{cy + r - gap_bot:.2f}" fill="{color}" opacity=".16"/>'
        f"</g>"
    )


def mark_svg(size: int = 64, color: str = INK) -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 64 64" role="img" aria-label="Tarn">
<title>Tarn</title>
{mark(32, 32, 26, color=color)}
</svg>
"""


def icon_png(path: Path, size: int = 128) -> None:
    """The editor icon: the mark on a white tile, drawn big and scaled down for smooth edges."""
    from PIL import Image, ImageChops, ImageDraw

    s = 8 * size
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    inset, radius = s * 0.02, s * 0.22
    d.rounded_rectangle([inset, inset, s - inset, s - inset], radius=radius, fill=(255, 255, 255, 255), outline=(226, 226, 226, 255), width=max(1, s // 128))
    cx = cy = s / 2
    r = s * 0.27
    gap_top, gap_bot = cy + 0.14 * r, cy + 0.30 * r
    disc = Image.new("L", (s, s), 0)
    ImageDraw.Draw(disc).ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    top = Image.new("L", (s, s), 0)
    ImageDraw.Draw(top).rectangle([0, 0, s, gap_top], fill=255)
    bottom = Image.new("L", (s, s), 0)
    ImageDraw.Draw(bottom).rectangle([0, gap_bot, s, s], fill=int(255 * 0.16))
    ink = Image.new("RGBA", (s, s), (17, 17, 17, 255))
    img.paste(ink, (0, 0), ImageChops.multiply(disc, top))
    img.paste(ink, (0, 0), ImageChops.multiply(disc, bottom))
    img.resize((size, size), Image.LANCZOS).save(path)


# ------------------------------------------------------------------ hero banner

def hero_svg() -> str:
    W, H = 1000, 340
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Tarn: Python's syntax, Kotlin's strictness, native speed">
<title>Tarn: Python's syntax, Kotlin's strictness, native speed</title>
<rect x=".5" y=".5" width="{W - 1}" height="{H - 1}" rx="20" fill="{PAPER}" stroke="{LINE}"/>
{mark(W / 2, 92, 22, "hm")}
<text x="{W / 2}" y="196" text-anchor="middle" font-family="{SANS}" font-size="64" font-weight="300" letter-spacing="10" fill="{INK}">tarn</text>
<text x="{W / 2}" y="244" text-anchor="middle" font-family="{SANS}" font-size="17" fill="{GRAY}" letter-spacing=".3">Python's syntax<tspan fill="{FAINT}">   ·   </tspan>Kotlin's strictness<tspan fill="{FAINT}">   ·   </tspan>native speed</text>
<text x="{W / 2}" y="292" text-anchor="middle" font-family="{MONO}" font-size="13.5" fill="{FAINT}">$ <tspan fill="{GRAY}">tarn hello.tr</tspan></text>
</svg>
"""


# ------------------------------------------------------------------ badges

def badge_svg(label: str, value: str) -> str:
    """A quiet pill: gray label, dark value, hairline border."""
    lw, vw = round(len(label) * 6.3 + 20), round(len(value) * 6.6 + 18)
    w, h = lw + vw, 24
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" role="img" aria-label="{esc(label)}: {esc(value)}">
<title>{esc(label)}: {esc(value)}</title>
<rect x=".5" y=".5" width="{w - 1}" height="{h - 1}" rx="{h / 2 - .5}" fill="{PAPER}" stroke="{LINE}"/>
<line x1="{lw}" y1="6" x2="{lw}" y2="{h - 6}" stroke="{LINE}"/>
<g font-family="{SANS}" font-size="11.5" text-anchor="middle">
<text x="{lw / 2 + 2}" y="16" fill="{GRAY}">{esc(label)}</text><text x="{lw + vw / 2 - 2}" y="16" fill="{INK}" font-weight="500">{esc(value)}</text>
</g>
</svg>
"""


# ------------------------------------------------------------------ benchmark chart

# Tarn in ink; the others in grays.
SERIES = [("Tarn", INK), ("Rust", "#a9a9a9"), ("Python", "#dcdcdb")]


def bar_path(x: float, y: float, w: float, h: float, r: float = 3) -> str:
    """Horizontal bar: square at the baseline, rounded data end."""
    r = min(r, w / 2, h / 2)
    return f"M{x:.1f} {y:.1f}h{w - r:.1f}a{r} {r} 0 0 1 {r} {r}v{h - 2 * r:.1f}a{r} {r} 0 0 1 -{r} {r}h-{w - r:.1f}z"


def benchmark_svg(results: dict) -> str:
    W = 760
    pad, label_w, max_w = 36, 108, 520
    x0 = pad + label_w
    panels = [
        ("fib", "fib(35): 30 million recursive function calls"),
        ("sieve", "Sieve of Eratosthenes up to 10 million"),
        ("strings", "Building a 15 MB string from 2 million f-strings"),
        ("particles", "Moving a Vec2 particle 5 million steps"),
        ("trees", "Binary trees: building and walking 3 million enum nodes"),
        ("maps", "Hash maps: 4 million counting updates"),
    ]
    out = []
    out.append(f'<text x="{pad}" y="56" font-size="18" font-weight="500" fill="{INK}">Run time, in milliseconds</text>')
    out.append(f'<text x="{pad}" y="80" font-size="12.5" fill="{GRAY}">Lower is better. Best of 3 runs on one machine ({esc(results.get("cpu", ""))}), Rust built with rustc -O.</text>')
    lx = pad
    for name, color in SERIES:
        out.append(f'<rect x="{lx}" y="98" width="10" height="10" rx="2" fill="{color}"/>')
        out.append(f'<text x="{lx + 16}" y="107" font-size="12.5" fill="{GRAY}">{name}</text>')
        lx += 16 + len(name) * 7.2 + 26
    y = 150
    for key, heading in panels:
        if key not in results:
            continue
        vals = [results[key][n.lower()] for n, _ in SERIES]
        tarn, rust, py = vals
        out.append(f'<line x1="{pad}" y1="{y - 22}" x2="{W - pad}" y2="{y - 22}" stroke="#f0f0ef"/>')
        out.append(f'<text x="{pad}" y="{y}" font-size="13.5" font-weight="500" fill="{INK}">{esc(heading)}</text>')
        vs_py = f"{py / tarn:.0f}×" if py / tarn >= 10 else f"{py / tarn:.1f}×"
        vs_rust = f"Rust is {tarn / rust:.1f}× faster" if rust < tarn else f"{rust / tarn:.1f}× faster than Rust"
        out.append(f'<text x="{pad}" y="{y + 19}" font-size="12" fill="{FAINT}">{vs_py} faster than Python · {vs_rust}</text>')
        top = y + 34
        scale = max_w / max(vals)
        for j, ((name, color), v) in enumerate(zip(SERIES, vals)):
            by = top + j * 24
            bw = max(2, v * scale)
            out.append(f'<text x="{x0 - 14}" y="{by + 12}" font-size="12" text-anchor="end" fill="{GRAY}">{name}</text>')
            out.append(f'<path d="{bar_path(x0, by + 1, bw, 14)}" fill="{color}"><title>{name}: {v} ms</title></path>')
            out.append(f'<text x="{x0 + bw + 8:.1f}" y="{by + 12}" font-size="12" fill="{GRAY}">{v:,} ms</text>')
        y = top + 3 * 24 + 48
    H = y - 20
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Benchmark: Tarn vs Rust vs Python run times">
<title>Benchmark: Tarn vs Rust vs Python run times</title>
<rect x=".5" y=".5" width="{W - 1}" height="{H - 1}" rx="18" fill="{PAPER}" stroke="{LINE}"/>
<g font-family="{SANS}">
{chr(10).join(out)}
</g>
</svg>
"""


# ------------------------------------------------------------------ pipeline diagram

def pipeline_svg() -> str:
    W, H = 1180, 300
    bw, bh, gap, x0, y1 = 122, 60, 20, 30, 112
    stages = [
        ("your .tr file", "source code"),
        ("Lexer", "tokens"),
        ("Parser", "syntax tree"),
        ("Type checker", "strict types, lints"),
        ("MIR + ARC", "reference counting"),
        ("Codegen", "LLVM IR"),
        ("clang -O3", "+ a small C runtime"),
        ("native binary", "runs on its own"),
    ]
    xs = [x0 + i * (bw + gap) for i in range(len(stages))]
    parts = []

    def box(x, y, title, sub, strong=False, w=bw):
        fill, stroke, t, s = (INK, INK, PAPER, "#bdbdbd") if strong else (PAPER, LINE, INK, GRAY)
        parts.append(f'<rect x="{x}" y="{y}" width="{w}" height="{bh}" rx="10" fill="{fill}" stroke="{stroke}"/>')
        parts.append(f'<text x="{x + w / 2}" y="{y + 26}" text-anchor="middle" font-size="13.5" font-weight="500" fill="{t}">{esc(title)}</text>')
        parts.append(f'<text x="{x + w / 2}" y="{y + 44}" text-anchor="middle" font-size="11.5" fill="{s}">{esc(sub)}</text>')

    def arrow(x1, y1_, x2, y2, label=None):
        parts.append(f'<path d="M{x1} {y1_}L{x2} {y2}" stroke="#c4c4c4" stroke-width="1.2" fill="none" marker-end="url(#ah)"/>')
        if label:
            parts.append(f'<text x="{(x1 + x2) / 2 + 8}" y="{(y1_ + y2) / 2 + 4}" font-size="11.5" fill="{FAINT}">{esc(label)}</text>')

    for i, (t, s) in enumerate(stages):
        box(xs[i], y1, t, s, strong=i == len(stages) - 1)
        if i:
            arrow(xs[i - 1] + bw + 2, y1 + bh / 2, xs[i] - 4, y1 + bh / 2)
    # imported files (yours and the standard library's) are lexed + parsed with your file
    mid = xs[1] + (bw + gap) / 2 + bw / 2
    box(mid - 85, 24, "imported .tr files", "yours and the stdlib", w=170)
    arrow(mid, 24 + bh + 2, mid, y1 - 4, "import")
    # the same checker powers the editor
    box(xs[3], 214, "tarn lsp", "language server")
    arrow(xs[3] + bw / 2, y1 + bh + 2, xs[3] + bw / 2, 210)
    box(xs[4] + 30, 214, "VS Code", "squiggles, hover, completion", w=200)
    arrow(xs[3] + bw + 2, 214 + bh / 2, xs[4] + 26, 214 + bh / 2)
    # the runtime half of memory management
    box(xs[6] - 15, 214, "C runtime", "allocator, cycle collector", w=152)
    arrow(xs[6] + bw / 2, 214 - 4, xs[6] + bw / 2, y1 + bh + 2)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="How Tarn compiles: lexer, parser, type checker, MIR with reference counting, LLVM codegen, clang with the C runtime, native binary; the checker also powers the VS Code language server">
<title>How Tarn compiles a program</title>
<defs><marker id="ah" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M0 0L10 5L0 10z" fill="#c4c4c4"/></marker></defs>
<rect x=".5" y=".5" width="{W - 1}" height="{H - 1}" rx="18" fill="{PAPER}" stroke="{LINE}"/>
<g font-family="{SANS}">
{chr(10).join(parts)}
</g>
</svg>
"""


# ------------------------------------------------------------------ scenes

HELLO = 'fn main():\n    val name = "Tarn"\n    print(f"Hello from {name}.")\n'
OOPS = """fn average(scores: List[Int]) -> Float:
    var total = 0
    for s in scores:
        total += s
    return total / scores.len()

fn main():
    val scores = [90, 72, 85]
    if scores.len():
        print(average(scores))
"""
CRASH = """fn main():
    val xs = [10, 20, 30]
    var i = 0
    while i <= xs.len():
        print(xs[i])
        i += 1
"""


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cargo", "build", "--release", "-q"], cwd=ROOT, check=True)
    def write(name: str, text: str) -> None:
        (OUT / name).write_text(text)
        print(f"  wrote docs/media/{name}")

    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        env = {"TARN_CACHE_DIR": str(work / "cache")}
        (work / "hello.tr").write_text(HELLO)
        (work / "fib.tr").write_text((ROOT / "examples" / "fib.tr").read_text())
        (work / "oops.tr").write_text(OOPS)
        (work / "crash.tr").write_text(CRASH)
        (work / "clock.tr").write_text((ROOT / "examples" / "clock.tr").read_text())

        def scene(*cmds):
            return [x for c in cmds for x in (("cmd", c), ("out", capture(c, work, env)))]

        write("terminal-run.svg", terminal_svg("run a script, or build a native binary",
              scene("tarn hello.tr", "tarn build fib.tr -o fib", "./fib", "file -b fib | cut -d, -f1-2")))
        write("terminal-errors.svg", terminal_svg("errors that explain themselves",
              scene("tarn check oops.tr", "tarn crash.tr"), line_step=0.02))
        write("terminal-time.svg", terminal_svg("the time module", scene("tarn clock.tr")))

        # The installer, run against a throwaway home directory.
        home = work / "home"
        home.mkdir()
        cargo_bin = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "bin"
        inst_env = {"HOME": str(home), "SHELL": "/bin/bash", "TARN_CACHE_DIR": str(home / ".cache" / "tarn"),
                    "PATH": f"{cargo_bin}:/usr/bin:/bin",  # a machine without tarn yet
                    "CARGO_HOME": os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
                    "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup"))}
        inst = capture(f"{ROOT}/install.sh -y --no-anim", ROOT, inst_env)
        write("terminal-install.svg", terminal_svg("install.sh", [("cmd", "./install.sh -y"), ("out", inst)], line_step=0.06))

    results = json.loads((ROOT / "bench" / "results.json").read_text())
    write("benchmark.svg", benchmark_svg(results))
    write("hero.svg", hero_svg())
    write("pipeline.svg", pipeline_svg())
    write("mark.svg", mark_svg())

    cases = sum(len(list((ROOT / "tests" / d).glob("*.tr"))) for d in ["run", "fail", "panic", "warn", "arc"])
    write("badge-native.svg", badge_svg("compiles to", "native code"))
    write("badge-llvm.svg", badge_svg("optimizer", "LLVM -O3"))
    write("badge-tests.svg", badge_svg("test programs", f"{cases} passing"))
    for old in ["badge-editor.svg", "badge-fun.svg"]:
        (OUT / old).unlink(missing_ok=True)
    icon_png(OUT / "vscode-icon.png")
    print("  wrote docs/media/vscode-icon.png")


if __name__ == "__main__":
    main()
