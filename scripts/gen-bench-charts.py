#!/usr/bin/env python3
"""Generate modern benchmark charts for the kzip README from results/benchmark-*.csv.

Outputs PNG charts into docs/benchmarks/ (committed) for embedding in README.md.
Requires: matplotlib.
"""
import csv, os, statistics
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "docs", "benchmarks")
os.makedirs(OUT, exist_ok=True)

# Modern palette
RUST = "#E4572E"   # rust orange
C    = "#5B7DB1"   # steel blue
PAR  = "#2E9E6B"   # green for parallel
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
    rows = []
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            rows.append(r)
    return rows

def median_mibps(rows, impl, workload):
    vals = [float(r["mibps"]) for r in rows if r["impl"] == impl and r["workload"] == workload]
    return statistics.median(vals) if vals else None

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
                    ha="center", va="bottom", fontsize=9, color=TXT, xytext=(0, 3),
                    textcoords="offset points")

# ---------- Chart 1: throughput comparison (C vs Rust) ----------
workloads = [
    ("small",        "Compress small\n(1–64 KiB)"),
    ("large",        "Compress large\n(1 GiB)"),
    ("mixed_serial", "Compress mixed\n(serial)"),
    ("read_full",    "Read full\narchive"),
    ("read_random",  "Read random\nentries"),
]

c_vals, r_vals = [], []
for wl, _ in workloads:
    c_vals.append(median_mibps(load(os.path.join(ROOT,"results","benchmark-small.csv")), "c_libzip", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-large.csv")), "c_libzip", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-mixed.csv")), "c_libzip", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-read.csv")), "c_libzip", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-random.csv")), "c_libzip", wl))
    r_vals.append(median_mibps(load(os.path.join(ROOT,"results","benchmark-small.csv")), "rust_zip_core", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-large.csv")), "rust_zip_core", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-mixed.csv")), "rust_zip_core", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-read.csv")), "rust_zip_core", wl)
                   or median_mibps(load(os.path.join(ROOT,"results","benchmark-random.csv")), "rust_zip_core", wl))

fig, ax = plt.subplots(figsize=(11, 5.5), dpi=150)
x = range(len(workloads))
w = 0.36
b1 = ax.bar([i - w/2 for i in x], c_vals, w, label="C libzip 1.11.4", color=C, zorder=3)
b2 = ax.bar([i + w/2 for i in x], r_vals, w, label="kzip (Rust)", color=RUST, zorder=3)
ax.set_xticks(list(x))
ax.set_xticklabels([lbl for _, lbl in workloads], fontsize=9)
ax.set_title("Throughput: C libzip vs kzip (Rust) — median MiB/s", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "Throughput (MiB/s)")
add_labels(ax, b1)
add_labels(ax, b2)
ax.legend(frameon=False, fontsize=10, loc="upper left")
fig.tight_layout()
fig.savefig(os.path.join(OUT, "benchmark-throughput.png"))
plt.close(fig)

# ---------- Chart 2: parallel compression speedup ----------
mixed = load(os.path.join(ROOT, "results", "benchmark-mixed.csv"))
c_ser  = median_mibps(mixed, "c_libzip", "mixed_serial")
r_ser  = median_mibps(mixed, "rust_zip_core", "mixed_serial")
r_par  = median_mibps(mixed, "rust_zip_core", "mixed_parallel")

fig, ax = plt.subplots(figsize=(8, 5), dpi=150)
labels = ["C libzip\n(serial)", "kzip\n(serial)", "kzip\n(parallel, 24)"]
vals = [c_ser, r_ser, r_par]
colors = [C, RUST, PAR]
bars = ax.bar(labels, vals, color=colors, width=0.55, zorder=3)
ax.set_title("Mixed-corpus compression — parallel speedup", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "Throughput (MiB/s)")
add_labels(ax, bars)
ax.annotate(f"{r_par/r_ser:.1f}× vs serial", xy=(2, r_par), xytext=(1.35, r_par*0.92),
            fontsize=10, color=PAR, fontweight="bold", arrowprops=dict(arrowstyle="->", color=PAR))
fig.tight_layout()
fig.savefig(os.path.join(OUT, "benchmark-parallel.png"))
plt.close(fig)

# ---------- Chart 3: memory footprint ----------
mem = load(os.path.join(ROOT, "results", "benchmark-memory.csv"))
def median_rss(impl, wl):
    vals = [float(r["rss_bytes"]) for r in mem if r["impl"] == impl and r["workload"] == wl and float(r["rss_bytes"]) > 0]
    return statistics.median(vals) if vals else None

c_comp, r_comp = median_rss("c_libzip", "memory_compress"), median_rss("rust_zip_core", "memory_compress")
c_read, r_read = median_rss("c_libzip", "memory_read"), median_rss("rust_zip_core", "memory_read")

fig, ax = plt.subplots(figsize=(8, 5), dpi=150)
labels = ["Compress", "Read"]
x = range(2); w = 0.36
b1 = ax.bar([i - w/2 for i in x], [(c_comp or 0)/2**20, (c_read or 0)/2**20], w, label="C libzip", color=C, zorder=3)
b2 = ax.bar([i + w/2 for i in x], [(r_comp or 0)/2**20, (r_read or 0)/2**20], w, label="kzip (Rust)", color=RUST, zorder=3)
ax.set_xticks(list(x)); ax.set_xticklabels(labels)
ax.set_title("Memory footprint (RSS) — 10k-entry archive", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "RSS (MiB)")
add_labels(ax, b1, "{:.1f}")
add_labels(ax, b2, "{:.1f}")
ax.legend(frameon=False, fontsize=10)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "benchmark-memory.png"))
plt.close(fig)

def median_seconds(rows, impl, workload):
    vals = [float(r["seconds"]) for r in rows if r["impl"] == impl and r["workload"] == workload]
    return statistics.median(vals) if vals else None

# ---------- Chart 4: Modify in-place latency (ms) ----------
mod = load(os.path.join(ROOT, "results", "benchmark-modify.csv"))
c_mp   = median_seconds(mod, "c_libzip",     "modify_inplace")
r_mrw  = median_seconds(mod, "rust_zip_core","modify_rewrite")
r_mp   = median_seconds(mod, "rust_zip_core","modify_inplace")

def _ms(sec):
    return sec * 1000.0 if sec is not None else 0.0

fig, ax = plt.subplots(figsize=(8, 5), dpi=150)
labels = ["C libzip\n(in-place)", "kzip rewrite\n(recompress)", "kzip\n(in-place)"]
vals_ms = [_ms(c_mp), _ms(r_mrw), _ms(r_mp)]
colors = [C, "#9CA3AF", RUST]
bars = ax.bar(labels, vals_ms, color=colors, width=0.55, zorder=3)
ax.set_title("Modify in place (add/delete/rename) — median latency", fontsize=13, fontweight="bold", pad=12)
style_ax(ax, "Latency (ms)")
add_labels(ax, bars, "{:.2f}")
if r_mp and c_mp:
    ax.annotate(f"{_ms(c_mp)/_ms(r_mp):.1f}× faster", xy=(2, _ms(r_mp)), xytext=(1.15, _ms(c_mp)*1.1),
                fontsize=10, color=RUST, fontweight="bold", arrowprops=dict(arrowstyle="->", color=RUST))
fig.tight_layout()
fig.savefig(os.path.join(OUT, "benchmark-modify.png"))
plt.close(fig)

print("Charts written to", OUT)
for f in sorted(os.listdir(OUT)):
    print(" -", f)
