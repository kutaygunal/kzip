#!/usr/bin/env python3
"""Render benchmark JSON as a README-friendly static SVG.

An optional fourth argument can still generate an animated SVG for local
experiments, but the static chart is the canonical documentation artifact.
"""

from __future__ import annotations

import html
import json
import sys
from datetime import datetime, timezone
from pathlib import Path


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
    width = 1400
    height = 790
    panel_width = 650
    panel_height = 548
    panel_y = 166
    panel_gap = 36
    panel_xs = {"write": 32, "read": 32 + panel_width + panel_gap}
    chart_offset = 194
    chart_width = 330
    maximum = max(
        result["throughput_mib_s"]
        for row in rows
        for result in (row["libzip-c"], row["kzip-rust"])
    )
    domain = max(maximum, 1.0) * 1.12
    wins = sum(
        row["kzip-rust"]["throughput_mib_s"] > row["libzip-c"]["throughput_mib_s"]
        for row in rows
    )
    best_speedup = max(
        row["kzip-rust"]["throughput_mib_s"] / row["libzip-c"]["throughput_mib_s"]
        for row in rows
        if row["libzip-c"]["throughput_mib_s"] > 0
    )
    peak_throughput = max(row["kzip-rust"]["throughput_mib_s"] for row in rows)
    read_rows = [row for row in rows if row["operation"] == "read"]
    read_wins = sum(
        row["kzip-rust"]["throughput_mib_s"] > row["libzip-c"]["throughput_mib_s"]
        for row in read_rows
    )
    generated_at = document.get("generated_at")
    try:
        latest = datetime.fromtimestamp(float(generated_at), tz=timezone.utc).strftime("%Y-%m-%d")
    except (TypeError, ValueError, OSError):
        latest = str(generated_at)
    motion = "" if animated else "aria-label=\"Static benchmark chart\""
    title = "kzip Rust vs libzip C - benchmark snapshot"
    subtitle = (
        f"Median throughput in MiB/s  |  {len(rows)} workloads  |  "
        f"{document['samples']} samples  |  generated {latest}"
    )
    by_operation = {
        operation: [row for row in rows if row["operation"] == operation]
        for operation in ("write", "read")
    }
    workload_order = ("many-small", "mixed-8m", "single-16m", "text-8m", "tiny-mixed")
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
        '<style>text{font-family:Inter,Segoe UI,Arial,sans-serif;fill:#edf5ff} .muted{fill:#92a6bf;font-size:13px} .label{font-size:12px;font-weight:600} .value{font-size:12px;font-weight:700} .section{font-size:18px;font-weight:750;letter-spacing:.08em} .method{fill:#8fa6c0;font-size:11px;font-weight:700;letter-spacing:.12em} .kpi-label{fill:#8fa6c0;font-size:11px;font-weight:700;letter-spacing:.1em} .kpi-value{font-size:26px;font-weight:800} .grid{stroke:#2b3b51;stroke-width:1} .divider{stroke:#26364b;stroke-width:1} .panel{fill:#121d2d;stroke:#26364b;stroke-width:1} .card{fill:#162438;stroke:#2a4059;stroke-width:1} .bar-c{fill:#ffb45b} .bar-r{fill:#5ed9f5} .track{fill:#1d2b3e} .winner{fill:#74e4bd;font-size:11px;font-weight:700}',
        f'{animation_css}</style>',
        '<rect width="100%" height="100%" fill="#0b1320"/>',
        '<circle cx="38" cy="39" r="8" fill="#5ed9f5"/><circle cx="38" cy="39" r="3" fill="#0b1320"/>',
        f'<text x="60" y="45" font-size="25" font-weight="800">{esc(title)}</text>',
        f'<text x="60" y="72" class="muted">{esc(subtitle)}</text>',
        '<rect x="1078" y="28" width="14" height="14" rx="4" class="bar-c"/><text x="1101" y="40" class="muted">libzip C</text>',
        '<rect x="1190" y="28" width="14" height="14" rx="4" class="bar-r"/><text x="1213" y="40" class="muted">kzip Rust</text>',
    ]
    cards = (
        (32, "RUST WINS", f"{wins} / {len(rows)}", "workloads faster"),
        (322, "BEST SPEEDUP", f"{best_speedup:.1f}x", "versus libzip C"),
        (612, "PEAK THROUGHPUT", f"{peak_throughput:,.0f}", "MiB/s from kzip Rust"),
        (902, "READ RESULT", f"{read_wins} / {len(read_rows)}", "read workloads faster"),
    )
    for x, label, value, detail in cards:
        parts.append(f'<rect x="{x}" y="96" width="258" height="56" rx="12" class="card"/>')
        parts.append(f'<text x="{x + 18}" y="116" class="kpi-label">{label}</text>')
        parts.append(f'<text x="{x + 18}" y="143" class="kpi-value">{value}</text>')
        parts.append(f'<text x="{x + 148}" y="138" class="muted">{detail}</text>')

    ticks = 4
    for operation, panel_x in panel_xs.items():
        parts.append(f'<rect x="{panel_x}" y="{panel_y}" width="{panel_width}" height="{panel_height}" rx="16" class="panel"/>')
        parts.append(f'<text x="{panel_x + 24}" y="{panel_y + 34}" class="section">{operation.upper()}</text>')
        parts.append(f'<text x="{panel_x + 24}" y="{panel_y + 54}" class="muted">Median throughput - higher is better</text>')
        chart_left = panel_x + chart_offset
        for tick in range(ticks + 1):
            value = domain * tick / ticks
            x = chart_left + chart_width * tick / ticks
            parts.append(f'<line x1="{x:.1f}" y1="{panel_y + 72}" x2="{x:.1f}" y2="{panel_y + panel_height - 26}" class="grid"/>')
            parts.append(f'<text x="{x:.1f}" y="{panel_y + 68}" text-anchor="middle" class="muted">{value:.0f}</text>')

        row_map = {
            (row["method"], row["workload"]): row
            for row in by_operation[operation]
        }
        y = panel_y + 88
        animation_index = 0
        for method in ("store", "deflate"):
            parts.append(f'<text x="{panel_x + 24}" y="{y + 4}" class="method">{method.upper()}</text>')
            y += 14
            for workload in workload_order:
                row = row_map[(method, workload)]
                parts.append(f'<text x="{panel_x + 24}" y="{y + 18}" class="label">{esc(workload)}</text>')
                parts.append(f'<rect x="{chart_left}" y="{y + 3}" width="{chart_width}" height="9" rx="4" class="track"/>')
                parts.append(f'<rect x="{chart_left}" y="{y + 16}" width="{chart_width}" height="9" rx="4" class="track"/>')
                for lane, engine in enumerate(("libzip-c", "kzip-rust")):
                    value = row[engine]["throughput_mib_s"]
                    bar_y = y + 3 + lane * 13
                    bar_width = chart_width * value / domain
                    cls = "bar-c" if engine == "libzip-c" else "bar-r"
                    anim_cls = " animated" if animated else ""
                    delay = (animation_index * 0.035 + lane * 0.02) if animated else 0
                    delay_attr = f' style="animation-delay:{delay:.3f}s"' if animated else ""
                    parts.append(
                        f'<rect x="{chart_left}" y="{bar_y}" width="{bar_width:.2f}" height="9" rx="4" class="{cls}{anim_cls}" '
                        f'data-target-width="{bar_width:.2f}"{delay_attr}/>'
                    )
                    value_x = chart_left + bar_width + 8
                    parts.append(f'<text x="{value_x:.1f}" y="{bar_y + 8}" class="value">{value:,.1f}</text>')
                if row["kzip-rust"]["throughput_mib_s"] > row["libzip-c"]["throughput_mib_s"]:
                    parts.append(f'<text x="{panel_x + panel_width - 28}" y="{y + 19}" text-anchor="end" class="winner">↑</text>')
                y += 37
                animation_index += 1
            if method == "store":
                parts.append(f'<line x1="{panel_x + 24}" y1="{y + 5}" x2="{panel_x + panel_width - 24}" y2="{y + 5}" class="divider"/>')
                y += 18

    parts.append(f'<text x="32" y="754" class="muted">Benchmark uses deterministic workloads, checksum validation, {document["warmups"]} warmup rounds, and {document["samples"]} measured samples. See the raw JSON for p95 latency and workload details.</text>')
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
