#!/usr/bin/env python3
"""Regenerate every image in docs/media from real runs of Isma.

    cargo build --release && python3 docs/media/generate.py

- Terminal images are recordings of real commands, colors included: each
  command runs in a pseudo-terminal and its ANSI output is turned into SVG.
- The benchmark chart reads bench/results.json, written by bench/run.sh.
- The banner, badges and pipeline diagram are drawn here as plain SVG.

Animations are CSS-only and switch off under `prefers-reduced-motion`.
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
ISMA = ROOT / "target" / "release" / "isma"

MONO = "'JetBrains Mono','Fira Code','DejaVu Sans Mono',Menlo,Consolas,monospace"
SANS = "system-ui,-apple-system,'Segoe UI',Helvetica,Arial,sans-serif"
CW, LH, FS = 8.43, 19, 14  # terminal: char width, line height, font size

GREEN = "#39ff78"
TERM_BG, TERM_BAR, TERM_EDGE, TERM_FG = "#0b0f0d", "#141b17", "#22302a", "#d7dae0"


def esc(s: str) -> str:
    return html.escape(s, quote=True)


# ------------------------------------------------------------------ capture

def capture(cmd: str, cwd: Path, env_extra: dict | None = None) -> str:
    """Run `cmd` in a pseudo-terminal (so programs print colors) and return its output."""
    env = {**os.environ, "TERM": "xterm-256color", "COLUMNS": "110"}
    env["PATH"] = f"{ISMA.parent}:{env['PATH']}"  # the freshly built isma, unless overridden
    env.update(env_extra or {})
    env.pop("NO_COLOR", None)
    r = subprocess.run(["script", "-qec", cmd, "/dev/null"], cwd=cwd, env=env, capture_output=True, text=True, timeout=600)
    return r.stdout.replace("\r\n", "\n").rstrip("\n")


# ------------------------------------------------------------------ ANSI -> styled runs

BASIC = {
    30: "#3b4048", 31: "#ff6b6b", 32: "#5af78e", 33: "#f4d35e", 34: "#57c7ff", 35: "#ff6ac1", 36: "#9aedfe", 37: "#d7dae0",
    90: "#6b7280", 91: "#ff8787", 92: "#7dfca5", 93: "#ffe17d", 94: "#82d8ff", 95: "#ff92d0", 96: "#b8f3ff", 97: "#ffffff",
}


def xterm256(n: int) -> str:
    if n < 16:
        return BASIC[(30 + n) if n < 8 else (90 + n - 8)]
    if n < 232:
        n -= 16
        levels = [0, 95, 135, 175, 215, 255]
        r, g, b = levels[n // 36], levels[(n // 6) % 6], levels[n % 6]
        return f"#{r:02x}{g:02x}{b:02x}"
    v = 8 + (n - 232) * 10
    return f"#{v:02x}{v:02x}{v:02x}"


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
                            style["fg"] = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5])
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
            if st["fg"]:
                attrs.append(f'fill="{st["fg"]}"')
            if st["bold"]:
                attrs.append('font-weight="700"')
            if st["dim"]:
                attrs.append('opacity="0.6"')
            parts.append(f"<tspan {' '.join(attrs)}>{esc(chunk)}</tspan>")
            col += len(chunk)
    return f'<text y="{y:.1f}" xml:space="preserve">{"".join(parts)}</text>', col


# ------------------------------------------------------------------ terminal window

# Fail-safe animation: everything is visible by default; the keyframes only
# hide a line while it waits for its turn (fill-mode `both`). Where animation
# doesn't run (some previewers, image proxies), the full output still shows.
TERM_CSS = """
.r{animation:show .01s linear both}
.c{animation:blink 1.05s steps(1) infinite}
@keyframes show{from{opacity:0}to{opacity:1}}
@keyframes type{from{clip-path:inset(0 100% 0 0)}to{clip-path:inset(0 0 0 0)}}
@keyframes blink{50%{opacity:0}}
@media (prefers-reduced-motion:reduce){.r,.t,.c{animation:none!important}}
"""


def terminal_svg(title: str, script: list[tuple[str, str]], *, line_step: float = 0.035, min_cols: int = 72) -> str:
    """`script` is a list of ("cmd", command) / ("out", ansi_text) entries."""
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
    pad, bar = 20, 36
    w = round(pad * 2 + cols * CW)
    h = round(bar + pad + len(rows) * LH + pad - 4)
    body, t = [], 0.4
    for i, (kind, payload) in enumerate(rows):
        y = bar + pad + i * LH + FS - 2
        if kind == "cmd":
            typing = max(0.3, 0.035 * len(payload))
            text, _ = runs_svg([(payload, {"fg": TERM_FG, "bold": False, "dim": False})], pad + 2 * CW, y)
            body.append(f'<g class="r" style="animation-delay:{t:.2f}s"><text x="{pad}" y="{y:.1f}" fill="{GREEN}" font-weight="700">$</text></g>')
            body.append(f'<g class="t" style="animation:type {typing:.2f}s steps({max(1, len(payload))}) {t + 0.15:.2f}s both">{text}</g>')
            t += typing + 0.45
        elif kind == "out":
            text, _ = runs_svg(payload, pad, y)
            body.append(f'<g class="r" style="animation-delay:{t:.2f}s">{text}</g>')
            t += line_step
        else:
            body.append(
                f'<g class="r" style="animation-delay:{t + 0.2:.2f}s"><text x="{pad}" y="{y:.1f}" fill="{GREEN}" font-weight="700">$</text>'
                f'<rect class="c" x="{pad + 2 * CW:.1f}" y="{y - FS + 3:.1f}" width="{CW:.1f}" height="{FS + 2}" fill="{GREEN}"/></g>'
            )
    dots = "".join(f'<circle cx="{20 + i * 18}" cy="{bar / 2}" r="6" fill="{c}"/>' for i, c in enumerate(["#ff5f56", "#ffbd2e", "#27c93f"]))
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-label="{esc(title)}">
<title>{esc(title)}</title>
<style>{TERM_CSS}</style>
<rect x="0.5" y="0.5" width="{w - 1}" height="{h - 1}" rx="10" fill="{TERM_BG}" stroke="{TERM_EDGE}"/>
<path d="M10 0.5h{w - 20}a9.5 9.5 0 0 1 9.5 9.5v{bar - 10}h-{w - 1}v-{bar - 10}a9.5 9.5 0 0 1 9.5-9.5z" fill="{TERM_BAR}"/>
{dots}
<text x="{w / 2}" y="{bar / 2 + 4}" text-anchor="middle" font-family="{SANS}" font-size="12" fill="#7d8a83">{esc(title)}</text>
<g font-family="{MONO}" font-size="{FS}" fill="{TERM_FG}">
{chr(10).join(body)}
</g>
</svg>
"""


# ------------------------------------------------------------------ hero banner

LOGO = [
    "██╗███████╗███╗   ███╗ █████╗ ",
    "██║██╔════╝████╗ ████║██╔══██╗",
    "██║███████╗██╔████╔██║███████║",
    "██║╚════██║██║╚██╔╝██║██╔══██║",
    "██║███████║██║ ╚═╝ ██║██║  ██║",
    "╚═╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝",
]
# Box-drawing "shadow" glyphs as line segments inside a unit cell.
SHADOW = {
    "═": [((0, .5), (1, .5))],
    "║": [((.5, 0), (.5, 1))],
    "╗": [((0, .5), (.5, .5)), ((.5, .5), (.5, 1))],
    "╔": [((1, .5), (.5, .5)), ((.5, .5), (.5, 1))],
    "╚": [((.5, 0), (.5, .5)), ((.5, .5), (1, .5))],
    "╝": [((.5, 0), (.5, .5)), ((.5, .5), (0, .5))],
}


def hero_svg() -> str:
    """The ISMA block logo drawn as vector cells, so it looks the same in every font."""
    W, H = 1000, 300
    cw, ch = 13, 24
    lw = len(LOGO[0]) * cw
    ox, oy = (W - lw) / 2, 44
    blocks, shadows = [], []
    for r, line in enumerate(LOGO):
        for c, g in enumerate(line):
            x, y = ox + c * cw, oy + r * ch
            if g == "█":
                blocks.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{cw + .3:.1f}" height="{ch + .3:.1f}"/>')
            for (x1, y1), (x2, y2) in SHADOW.get(g, []):
                shadows.append(f"M{x + x1 * cw:.1f} {y + y1 * ch:.1f}L{x + x2 * cw:.1f} {y + y2 * ch:.1f}")
    scan = "".join(f'<rect x="0" y="{y}" width="{W}" height="1" fill="#39ff78" opacity=".035"/>' for y in range(0, H, 4))
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Isma: Python's syntax, Kotlin's strictness, Rust's speed">
<title>Isma: Python's syntax, Kotlin's strictness, Rust's speed</title>
<defs>
<linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#8dffb4"/><stop offset=".55" stop-color="#39ff78"/><stop offset="1" stop-color="#00a944"/></linearGradient>
<radialGradient id="glow" cx=".5" cy=".38" r=".55"><stop offset="0" stop-color="#00ff6a" stop-opacity=".22"/><stop offset="1" stop-color="#00ff6a" stop-opacity="0"/></radialGradient>
<filter id="blur" x="-20%" y="-20%" width="140%" height="140%"><feGaussianBlur stdDeviation="7"/></filter>
<clipPath id="card"><rect width="{W}" height="{H}" rx="16"/></clipPath>
</defs>
<style>
.c{{animation:blink 1.05s steps(1) infinite}}
.sweep{{animation:sweep 6s linear infinite}}
@keyframes blink{{50%{{opacity:0}}}}
@keyframes sweep{{from{{transform:translateY(-60px)}}to{{transform:translateY({H}px)}}}}
@media (prefers-reduced-motion:reduce){{.c,.sweep{{animation:none}}.sweep{{display:none}}}}
</style>
<g clip-path="url(#card)">
<rect width="{W}" height="{H}" fill="#070b09"/>
<rect width="{W}" height="{H}" fill="url(#glow)"/>
{scan}
<rect class="sweep" x="0" y="0" width="{W}" height="60" fill="#39ff78" opacity=".05"/>
<g fill="url(#g)" filter="url(#blur)" opacity=".55">{"".join(blocks)}</g>
<path d="{" ".join(shadows)}" stroke="#0f7a3a" stroke-width="3" fill="none" stroke-linecap="square"/>
<g fill="url(#g)">{"".join(blocks)}</g>
<text x="{W / 2}" y="228" text-anchor="middle" font-family="{SANS}" font-size="25" font-weight="600" fill="#e8fff0">Python's syntax <tspan fill="#39ff78">·</tspan> Kotlin's strictness <tspan fill="#39ff78">·</tspan> Rust's speed</text>
<text x="{W / 2 - 92}" y="266" font-family="{MONO}" font-size="16" fill="#8fa898" textLength="164" lengthAdjust="spacingAndGlyphs"><tspan fill="#39ff78" font-weight="700">$</tspan> isma hello.isma</text>
<rect class="c" x="{W / 2 + 80}" y="253" width="9" height="17" fill="#39ff78"/>
</g>
<rect x=".5" y=".5" width="{W - 1}" height="{H - 1}" rx="16" fill="none" stroke="#1d3a28"/>
</svg>
"""


# ------------------------------------------------------------------ badges

def badge_svg(label: str, value: str, color: str = "#16a34a") -> str:
    lw, vw = round(len(label) * 6.6 + 14), round(len(value) * 6.6 + 14)
    w = lw + vw
    font = "Verdana,'DejaVu Sans',Geneva,sans-serif"
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="20" role="img" aria-label="{esc(label)}: {esc(value)}">
<title>{esc(label)}: {esc(value)}</title>
<clipPath id="r"><rect width="{w}" height="20" rx="4"/></clipPath>
<g clip-path="url(#r)"><rect width="{lw}" height="20" fill="#24292f"/><rect x="{lw}" width="{vw}" height="20" fill="{color}"/></g>
<g fill="#fff" font-family="{font}" font-size="11" text-anchor="middle">
<text x="{lw / 2}" y="14">{esc(label)}</text><text x="{lw + vw / 2}" y="14" font-weight="700">{esc(value)}</text>
</g>
</svg>
"""


# ------------------------------------------------------------------ benchmark chart

# Categorical slots 1-3 of the validated reference palette (light / dark),
# checked with the palette validator against GitHub's white and #0d1117 pages.
SERIES = [("Isma", "#2a78d6", "#3987e5"), ("Rust", "#eb6834", "#d95926"), ("Python", "#1baf7a", "#199e70")]


def bar_path(x: float, y: float, w: float, h: float, r: float = 4) -> str:
    """Horizontal bar: square at the baseline, 4px rounded data end."""
    r = min(r, w / 2, h / 2)
    return f"M{x:.1f} {y:.1f}h{w - r:.1f}a{r} {r} 0 0 1 {r} {r}v{h - 2 * r:.1f}a{r} {r} 0 0 1 -{r} {r}h-{w - r:.1f}z"


def benchmark_svg(results: dict) -> str:
    W = 760
    label_w, x0, max_w = 64, 76, 560
    panels = [
        ("fib", "fib(35): 30 million recursive function calls"),
        ("sieve", "Sieve of Eratosthenes up to 10 million (List[Bool])"),
        ("strings", "Building a 15 MB string from 2 million f-strings"),
        ("particles", "Moving a Vec2 particle 5 million steps (pos = pos.add(vel))"),
        ("trees", "Binary trees: building and walking 3 million enum nodes"),
    ]
    out, y = [], 0
    out.append(f'<text class="p" x="0" y="20" font-size="17" font-weight="600">Run time in milliseconds (lower is better)</text>')
    out.append(f'<text class="s" x="0" y="42" font-size="12.5">Best of 3 runs, same machine ({esc(results.get("cpu", ""))}), {esc(results.get("date", ""))}. Rust built with rustc -O.</text>')
    lx = 0
    for i, (name, _, _) in enumerate(SERIES):
        out.append(f'<rect class="b{i}" x="{lx}" y="58" width="12" height="12" rx="3"/>')
        out.append(f'<text class="s" x="{lx + 18}" y="69" font-size="12.5">{name}</text>')
        lx += 18 + len(name) * 7.5 + 22
    y = 96
    for key, heading in panels:
        vals = [results[key][n.lower()] for n, _, _ in SERIES]
        isma, rust, py = vals
        out.append(f'<text class="p" x="0" y="{y}" font-size="13.5" font-weight="600">{esc(heading)}</text>')
        vs_py = f"{py / isma:.0f}×" if py / isma >= 10 else f"{py / isma:.1f}×"
        vs_rust = f"Rust is {isma / rust:.1f}× faster than Isma" if rust < isma else f"Isma is {rust / isma:.1f}× faster than Rust"
        out.append(f'<text class="s" x="0" y="{y + 18}" font-size="12">Isma is {vs_py} faster than Python; {vs_rust}</text>')
        top = y + 32
        scale = max_w / max(vals)
        for j, ((name, _, _), v) in enumerate(zip(SERIES, vals)):
            by = top + j * 30
            bw = max(3, v * scale)
            out.append(f'<text class="p" x="{label_w}" y="{by + 15}" font-size="12.5" text-anchor="end">{name}</text>')
            out.append(f'<path class="b{j}" d="{bar_path(x0, by, bw, 20)}"><title>{name}: {v} ms</title></path>')
            out.append(f'<text class="s" x="{x0 + bw + 8:.1f}" y="{by + 15}" font-size="12.5">{v:,} ms</text>')
        out.append(f'<line class="axis" x1="{x0 - .5}" y1="{top - 4}" x2="{x0 - .5}" y2="{top + 3 * 30 - 6}"/>')
        y = top + 3 * 30 + 30
    H = y - 8
    css_light = "".join(f".b{i}{{fill:{l}}}" for i, (_, l, _) in enumerate(SERIES))
    css_dark = "".join(f".b{i}{{fill:{d}}}" for i, (_, _, d) in enumerate(SERIES))
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Benchmark: Isma vs Rust vs Python run times">
<title>Benchmark: Isma vs Rust vs Python run times</title>
<style>
text{{font-family:{SANS}}}
.p{{fill:#0b0b0b}} .s{{fill:#52514e}} .axis{{stroke:#c3c2b7;stroke-width:1}}
{css_light}
@media (prefers-color-scheme:dark){{.p{{fill:#f0efec}} .s{{fill:#c3c2b7}} .axis{{stroke:#383835}} {css_dark}}}
</style>
{chr(10).join(out)}
</svg>
"""


# ------------------------------------------------------------------ pipeline diagram

def pipeline_svg() -> str:
    W, H = 1140, 262
    bw, bh, gap, x0, y1 = 120, 58, 20, 20, 92
    stages = [
        ("your .isma file", "source code"),
        ("Lexer", "→ tokens"),
        ("Parser", "→ syntax tree"),
        ("Type checker", "strict types + lints"),
        ("MIR + ARC", "Perceus ref counting"),
        ("Codegen", "→ LLVM IR"),
        ("clang -O3", "+ small C runtime"),
        ("native binary", "runs on its own"),
    ]
    xs = [x0 + i * (bw + gap) for i in range(len(stages))]
    parts = []

    def box(x, y, title, sub, cls="box", w=bw):
        parts.append(f'<rect class="{cls}" x="{x}" y="{y}" width="{w}" height="{bh}" rx="9"/>')
        parts.append(f'<text class="t" x="{x + w / 2}" y="{y + 25}" text-anchor="middle" font-size="13.5" font-weight="600">{esc(title)}</text>')
        parts.append(f'<text class="m" x="{x + w / 2}" y="{y + 43}" text-anchor="middle" font-size="11.5">{esc(sub)}</text>')

    def arrow(x1, y1_, x2, y2, label=None):
        parts.append(f'<path class="a" d="M{x1} {y1_}L{x2} {y2}" marker-end="url(#ah)"/>')
        if label:
            parts.append(f'<text class="m" x="{(x1 + x2) / 2 + 8}" y="{(y1_ + y2) / 2 + 4}" font-size="11.5">{esc(label)}</text>')

    for i, (t, s) in enumerate(stages):
        box(xs[i], y1, t, s, "box hot" if i == len(stages) - 1 else "box")
        if i:
            arrow(xs[i - 1] + bw + 2, y1 + bh / 2, xs[i] - 4, y1 + bh / 2)
    # imported files (yours and the standard library's) are lexed + parsed with your file
    box(xs[1] + (bw + gap) / 2 - 25, 8, "imported .isma files", "yours + the stdlib", w=170)
    arrow(xs[1] + (bw + gap) / 2 + bw / 2, 8 + bh + 2, xs[1] + (bw + gap) / 2 + bw / 2, y1 - 4, "import")
    # the same checker powers the editor
    box(xs[3], 192, "isma lsp", "language server")
    arrow(xs[3] + bw / 2, y1 + bh + 2, xs[3] + bw / 2, 188)
    box(xs[4] + 30, 192, "VS Code", "squiggles · hover · completion", "box blue", 200)
    arrow(xs[3] + bw + 2, 192 + bh / 2, xs[4] + 26, 192 + bh / 2)
    # the runtime half of memory management
    box(xs[6] - 15, 192, "C runtime", "allocator · cycle collector", "box", 150)
    arrow(xs[6] + bw / 2, 192 - 4, xs[6] + bw / 2, y1 + bh + 2)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="How Isma compiles: lexer, parser, type checker, MIR with reference counting, LLVM codegen, clang with the C runtime, native binary; the checker also powers the VS Code language server">
<title>How Isma compiles a program</title>
<defs><marker id="ah" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0L10 5L0 10z" class="ah"/></marker></defs>
<style>
text{{font-family:{SANS}}}
.box{{fill:#f6f8fa;stroke:#d0d7de}} .hot{{fill:#dafbe1;stroke:#1a7f37}} .blue{{fill:#ddf4ff;stroke:#0969da}}
.t{{fill:#1f2328}} .m{{fill:#59636e}} .a{{stroke:#1a7f37;stroke-width:1.6;fill:none}} .ah{{fill:#1a7f37}}
@media (prefers-color-scheme:dark){{
.box{{fill:#161b22;stroke:#30363d}} .hot{{fill:#0f2a19;stroke:#3fb950}} .blue{{fill:#0c2340;stroke:#4493f8}}
.t{{fill:#e6edf3}} .m{{fill:#9198a1}} .a{{stroke:#3fb950}} .ah{{fill:#3fb950}}}}
</style>
{chr(10).join(parts)}
</svg>
"""


# ------------------------------------------------------------------ scenes

HELLO = 'fn main():\n    val name = "Isma"\n    print(f"Hello from {name}!")\n'
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
        env = {"ISMA_CACHE_DIR": str(work / "cache")}
        (work / "hello.isma").write_text(HELLO)
        (work / "fib.isma").write_text((ROOT / "examples" / "fib.isma").read_text())
        (work / "oops.isma").write_text(OOPS)
        (work / "crash.isma").write_text(CRASH)
        (work / "clock.isma").write_text((ROOT / "examples" / "clock.isma").read_text())

        def scene(*cmds):
            return [x for c in cmds for x in (("cmd", c), ("out", capture(c, work, env)))]

        write("terminal-run.svg", terminal_svg("isma: run a script, or build a native binary",
              scene("isma hello.isma", "isma build fib.isma -o fib", "./fib", "file -b fib | cut -d, -f1-2")))
        write("terminal-errors.svg", terminal_svg("isma: errors that explain themselves",
              scene("isma check oops.isma", "isma crash.isma"), line_step=0.02))
        write("terminal-time.svg", terminal_svg("isma: the time module", scene("isma clock.isma")))

        # The installer, run against a throwaway home directory.
        home = work / "home"
        home.mkdir()
        cargo_bin = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "bin"
        inst_env = {"HOME": str(home), "SHELL": "/bin/bash", "ISMA_CACHE_DIR": str(home / ".cache" / "isma"),
                    "PATH": f"{cargo_bin}:/usr/bin:/bin",  # a machine without isma yet
                    "CARGO_HOME": os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
                    "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup"))}
        inst = capture(f"{ROOT}/install.sh -y --no-anim", ROOT, inst_env)
        write("terminal-install.svg", terminal_svg("./install.sh", [("cmd", "./install.sh -y"), ("out", inst)], line_step=0.06))

    results = json.loads((ROOT / "bench" / "results.json").read_text())
    write("benchmark.svg", benchmark_svg(results))
    write("hero.svg", hero_svg())
    write("pipeline.svg", pipeline_svg())

    cases = sum(len(list((ROOT / "tests" / d).glob("*.isma"))) for d in ["run", "fail", "panic", "warn", "arc"])
    write("badge-native.svg", badge_svg("compiles to", "native code"))
    write("badge-llvm.svg", badge_svg("optimizer", "LLVM -O3", "#2563eb"))
    write("badge-tests.svg", badge_svg("test programs", f"{cases} passing"))
    write("badge-editor.svg", badge_svg("editor", "VS Code", "#0e7490"))
    write("badge-fun.svg", badge_svg("built for", "fun", "#c026d3"))


if __name__ == "__main__":
    main()
