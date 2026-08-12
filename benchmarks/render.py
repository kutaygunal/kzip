#!/usr/bin/env python3
"""Render benchmark JSON as a README-friendly static SVG.

An optional fourth argument can still generate the animated SVG used for local
experiments, but the static chart is the canonical documentation artifact.
"""

from __future__ import annotations

import html
import json
import sys
from pathlib import Path


COLORS = {"libzip-c": "#f59e0b", "kzip-rust": "#38bdf8"}
ORDER = {"write": 0, "read": 1}
METHOD_ORDER = {"store": 0, "deflate": 1}


def load(path: Path) -> dict:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def ordered_rows(document: dict) -> list[dict]:
    rows = {}
    for result in document["results"]:
        key = (result["operation"], result["method"], result["workload"])
        rows.setdefault(key, {})[result["engine"]] = result
    return [
        {"operation": key[0], "method": key[1], "workload": key[2], **values}
        for key, values in sorted(
            rows.items(),
            key=lambda item: (
                ORDER[item[0][0]],
                METHOD_ORDER[item[0][1]],
                item[0][2],
            ),
        )
    ]


def esc(value: object) -> str:
    return html.escape(str(value), quote=True)


def make_svg(document: dict, animated: bool) -> str:
    rows = ordered_rows(document)
    width = 1240
    left = 250
    chart_width = 720
    right = 210
    row_height = 28
    top = 100
    height = top + len(rows) * row_height + 46
    maximum = max(
        result["throughput_mib_s"]
        for row in rows
        for result in (row["libzip-c"], row["kzip-rust"])
    )
    domain = max(maximum, 1.0) * 1.12
    latest = esc(document["generated_at"])
    motion = "" if animated else "aria-label=\"Static benchmark chart\""
    title = "kzip Rust vs libzip C — median throughput"
    animation_css = (
        '@keyframes grow{from{transform:scaleX(0)}to{transform:scaleX(1)}} '
        '.animated{transform-box:fill-box;transform-origin:left center;'
        'animation:grow .9s cubic-bezier(.2,.8,.2,1) both} '
        '@media(prefers-reduced-motion:reduce){.animated{animation:none!important}}'
        if animated
        else ""
    )
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" {motion}>',
        f"<title>{esc(title)}</title>",
        '<style>text{font-family:ui-monospace,SFMono-Regular,Consolas,monospace;fill:#dbeafe} .muted{fill:#94a3b8;font-size:12px} .label{font-size:12px} .value{font-size:11px} .grid{stroke:#334155;stroke-width:1} .bar-c{fill:#f59e0b} .bar-r{fill:#38bdf8} ',
        f'{animation_css}</style>',
        '<rect width="100%" height="100%" fill="#0f172a"/>',
        f'<text x="32" y="32" font-size="20" font-weight="600">{esc(title)}</text>',
        f'<text x="32" y="54" class="muted">MiB/s • median of {document["samples"]} samples after {document["warmups"]} warmup(s) • generated {latest}</text>',
        '<rect x="32" y="68" width="12" height="12" rx="2" class="bar-c"/><text x="50" y="78" class="muted">original libzip C</text>',
        '<rect x="190" y="68" width="12" height="12" rx="2" class="bar-r"/><text x="208" y="78" class="muted">kzip Rust C ABI</text>',
    ]
    ticks = 5
    for tick in range(ticks + 1):
        value = domain * tick / ticks
        x = left + chart_width * tick / ticks
        parts.append(f'<line x1="{x:.1f}" y1="{top - 12}" x2="{x:.1f}" y2="{height - 34}" class="grid"/>')
        parts.append(f'<text x="{x:.1f}" y="{top - 18}" text-anchor="middle" class="muted">{value:.0f}</text>')

    for index, row in enumerate(rows):
        y = top + index * row_height
        label = f'{row["operation"]} / {row["method"]} / {row["workload"]}'
        parts.append(f'<text x="32" y="{y + 12}" class="label">{esc(label)}</text>')
        for lane, engine in enumerate(("libzip-c", "kzip-rust")):
            value = row[engine]["throughput_mib_s"]
            bar_y = y + lane * 10
            bar_width = chart_width * value / domain
            cls = "bar-c" if engine == "libzip-c" else "bar-r"
            anim_cls = " animated" if animated else ""
            delay = (index * 0.035 + lane * 0.02) if animated else 0
            delay_attr = f' style="animation-delay:{delay:.3f}s"' if animated else ""
            parts.append(
                f'<rect x="{left}" y="{bar_y}" width="{bar_width:.2f}" height="7" rx="2" class="{cls}{anim_cls}" '
                f'data-target-width="{bar_width:.2f}"{delay_attr}/>'
            )
            value_x = left + bar_width + 7
            parts.append(f'<text x="{value_x:.1f}" y="{bar_y + 7}" class="value">{value:.1f}</text>')
    parts.append(f'<text x="32" y="{height - 12}" class="muted">Higher is better. See benchmark JSON for p95, samples, workload sizes, and checksums.</text>')
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main() -> int:
    if len(sys.argv) not in (3, 4):
        print(
            "usage: render.py <benchmark-json> <static-svg> [animated-svg]",
            file=sys.stderr,
        )
        return 2
    document = load(Path(sys.argv[1]))
    Path(sys.argv[2]).write_text(make_svg(document, False), encoding="utf-8")
    if len(sys.argv) == 4:
        Path(sys.argv[3]).write_text(make_svg(document, True), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
