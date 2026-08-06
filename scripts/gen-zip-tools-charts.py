#!/usr/bin/env python3
"""Generate charts for the kzip-vs-third-party zip-tools benchmark.

Reads results/benchmark-zip-tools.csv and writes PNGs into docs/benchmarks/
for embedding in README.md and results/zip-tools-benchmark.md.
Requires: matplotlib.
"""
import csv, os, statistics
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "docs", "benchmarks")
os.makedirs(OUT, exist_ok=True)

RUST = "#E4572E"   # rust orange (kzip)
C    = "#5B7DB1"   # steel blue
PAR  = "#2E9E6B"   # green
GREY = "#9CA3AF"
GRID = "#E5E7EB"
TXT  = "#1F2937"

plt.rcParams.update({
    "font.family": "DejaVu Sans",
    "text.color": TXT,
    "axes.edgecolor": "#D1D5DB",
    "axes.labelcolor": TXT,
    "xtick.color": TXT,
    "ytick.color": TXT,
    "axes.titlecolor": TXT,
    "figure.facecolor": "white",
    "axes.facecolor": "white",
})

def load(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))

def median(rows, key):
    return statistics.median(float(r[key]) for r in rows)

def style_ax(ax, ylabel):
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.grid(axis="y", color=GRID, linewidth=1)
    ax.set_axisbelow(True)
    ax.set_ylabel(ylabel, fontsize=11)
    ax.tick_params(labelsize=10)

def add_labels(ax, bars, fmt="{:.0f}"):
    for b in bars:
        h = b.get_height()
        ax.annotate(fmt.format(h), (b.get_x() + b.get_width()/2, h),
                    ha="center", va="bottom", fontsize=8.5, color=TXT, xytext=(0, 3),
                    textcoords="offset points")

def canonical(tool):
    # Merge "Info-ZIP 3.0 (zip)" and "Info-ZIP 3.0 (unzip)" into one tool entry.
    return tool.split(" (")[0] if tool.startswith("Info-ZIP") else tool

rows = load(os.path.join(ROOT, "results", "benchmark-zip-tools.csv"))
tools = []
for r in rows:
    name = canonical(r["tool"])
    if name not in [t[0] for t in tools]:
        tools.append([name, r["format"], None, None])
for r in rows:
    name = canonical(r["tool"])
    for t in tools:
        if t[0] == name:
            if r["operation"] == "compress":
                t[2] = float(r["mibps"])
            else:
                t[3] = float(r["mibps"])

short = {
    "kzip (Rust zip_core)": "kzip",
    "7-Zip 26.02 (7za)": "7-Zip",
    "Info-ZIP 3.0": "Info-ZIP",
    "Zstandard 1.5.7": "zstd",
    "LZ4 1.10.0": "lz4",
}
names = [short[t[0]] for t in tools]
formats = [t[1] for t in tools]
comp = [t[2] for t in tools]
extr = [t[3] for t in tools]
colors = [RUST if t[0].startswith("kzip") else (C if t[1] == "ZIP" else GREY) for t in tools]

# ---------- Chart A: compress throughput (MiB/s) ----------
fig, ax = plt.subplots(figsize=(9, 5), dpi=150)
bars = ax.bar(names, comp, color=colors, width=0.6, zorder=3)
ax.set_title("Zip / compression tools — median compress throughput", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "Compress throughput (MiB/s)")
add_labels(ax, bars)
ax.annotate("ZIP format\n(fair vs kzip)", xy=(0, comp[0]*0.86), xytext=(0.8, comp[0]*0.72),
            fontsize=9, color=TXT, arrowprops=dict(arrowstyle="->", color=TXT))
fig.tight_layout()
fig.savefig(os.path.join(OUT, "zip-tools-compress.png"))
plt.close(fig)

# ---------- Chart B: extract throughput (MiB/s) ----------
fig, ax = plt.subplots(figsize=(9, 5), dpi=150)
bars = ax.bar(names, extr, color=colors, width=0.6, zorder=3)
ax.set_title("Zip / compression tools — median extract throughput", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "Extract throughput (MiB/s)")
add_labels(ax, bars)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "zip-tools-extract.png"))
plt.close(fig)

# ---------- Chart C: compression ratio (compressed/uncompressed) ----------
ratios = []
for t in tools:
    match = [r for r in rows if canonical(r["tool"]) == t[0]]
    ratios.append(statistics.median(float(r["ratio"]) for r in match))
fig, ax = plt.subplots(figsize=(9, 5), dpi=150)
bars = ax.bar(names, ratios, color=colors, width=0.6, zorder=3)
ax.set_title("Compression ratio (compressed / uncompressed) — lower is better", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "Ratio (c / u)")
add_labels(ax, bars, "{:.3f}")
fig.tight_layout()
fig.savefig(os.path.join(OUT, "zip-tools-ratio.png"))
plt.close(fig)

print("Charts written to", OUT)
for f in sorted(os.listdir(OUT)):
    if f.startswith("zip-tools"):
        print(" -", f)
