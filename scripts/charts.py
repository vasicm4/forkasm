#!/usr/bin/env python3
"""Render BENCHMARKS.md charts from a `forkasm-cli bench-suite` results dir.

    python3 scripts/charts.py bench-results/<date> [out dir, default docs/charts]

Writes one SVG per chart. Needs matplotlib. Colors follow the entity across
every chart (S1 blue, S0 orange, process fork aqua, S2 violet); a component
of an entity (e.g. S1's spawn) shares its hue with a dashed line. Every series
is also direct-labeled, so identity never rests on color alone.
"""

import csv
import sys
from pathlib import Path

import matplotlib

matplotlib.use("svg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.ticker import FuncFormatter  # noqa: E402

SURFACE = "#fcfcfb"
TEXT = "#0b0b0b"
TEXT_2 = "#52514e"
GRID = "#e4e3df"
S1, S0, PROC, S2 = "#2a78d6", "#eb6834", "#1baf7a", "#6250d6"
REF = "#8c8b86"  # external reference points: neutral, not a series hue

plt.rcParams.update({
    "figure.facecolor": SURFACE,
    "axes.facecolor": SURFACE,
    "savefig.facecolor": SURFACE,
    "axes.edgecolor": GRID,
    "axes.labelcolor": TEXT_2,
    "axes.titlecolor": TEXT,
    "axes.titlesize": 12,
    "axes.titleweight": "bold",
    "axes.titlelocation": "left",
    "axes.spines.top": False,
    "axes.spines.right": False,
    "axes.grid": True,
    "axes.axisbelow": True,
    "grid.color": GRID,
    "grid.linewidth": 0.8,
    "xtick.color": TEXT_2,
    "ytick.color": TEXT_2,
    "font.size": 10,
    "font.family": "DejaVu Sans",
    "lines.linewidth": 2,
    "lines.markersize": 6,
    "svg.fonttype": "none",
})


def load(results: Path, name: str) -> list[dict]:
    with open(results / f"{name}.csv") as f:
        return list(csv.DictReader(f))


def us_label(v: float, _pos=None) -> str:
    if v >= 1e6:
        return f"{v / 1e6:g} s"
    if v >= 1e3:
        return f"{v / 1e3:g} ms"
    return f"{v:g} µs"


def time_axis(ax, axis="y"):
    fmt = FuncFormatter(us_label)
    (ax.yaxis if axis == "y" else ax.xaxis).set_major_formatter(fmt)


def end_label(ax, x, y, text, color, dy=0):
    """Direct label at a line's last point, in text ink beside a color chip."""
    ax.annotate(
        text, (x, y), xytext=(8, dy), textcoords="offset points", va="center",
        color=TEXT, fontsize=9,
        bbox=dict(boxstyle="round,pad=0.25", fc=SURFACE, ec=color, lw=1.5),
    )


def title(fig, text: str):
    fig.suptitle(text, x=0.012, ha="left", fontsize=12, fontweight="bold", color=TEXT)


def save(fig, out: Path, name: str):
    fig.tight_layout()
    fig.savefig(out / f"{name}.svg")
    plt.close(fig)
    print(f"wrote {out / name}.svg")


def strategies(results: Path, out: Path):
    rows = load(results, "strategies")
    rows = [r for r in rows if r["fork_med_us"] not in ("", "–")]
    names = [r["strategy"] for r in rows]
    med = [float(r["fork_med_us"]) for r in rows]
    p99 = [float(r["fork_p99_us"]) for r in rows]
    # Published VM-branching numbers (theirs, different hardware).
    refs = [("forkd live branch (published)", 150e3), ("Morph Infinibranch (published)", 250e3)]

    def color(n):
        if n.startswith("S0"):
            return S0
        if n.startswith("S2"):
            return S2
        if n.startswith("process"):
            return PROC
        return S1

    fig, ax = plt.subplots(figsize=(8, 0.42 * (len(rows) + len(refs)) + 1.4))
    y = list(range(len(rows) + len(refs)))[::-1]
    ax.barh(y[: len(rows)], med, color=[color(n) for n in names], height=0.6, edgecolor=SURFACE, linewidth=2)
    ax.barh(y[len(rows):], [v for _, v in refs], color=SURFACE, edgecolor=REF, hatch="///", height=0.6, linewidth=1)
    ax.errorbar(med, y[: len(rows)], xerr=[[0] * len(med), [p - m for p, m in zip(p99, med)]],
                fmt="none", ecolor=TEXT_2, elinewidth=1, capsize=3)
    for yi, v, at in zip(y, med + [v for _, v in refs], p99 + [v for _, v in refs]):
        ax.text(at * 1.2, yi, us_label(round(v, 1 if v < 1e3 else -1)), va="center", fontsize=8.5, color=TEXT)
    ax.set_yticks(y, names + [n for n, _ in refs], fontsize=9)
    ax.set_xscale("log")
    time_axis(ax, "x")
    ax.set_xlim(right=max(p99 + [250e3]) * 6)
    ax.grid(axis="y", visible=False)
    ax.set_xlabel("fork latency (snapshot + spawn), median; whisker to p99; log scale")
    title(fig, "Fork latency by strategy — 64 MiB memory, 128 dirty pages")
    save(fig, out, "strategies")


def size(results: Path, out: Path):
    rows = load(results, "size")
    x = [float(r["memory_MiB"]) for r in rows]
    series = [("S0 full copy", "S0_fork_med_us", S0, "s", 0),
              ("process fork()", "process_fork()_med_us", PROC, "^", 11),
              ("S1 (default)", "S1_fork_med_us", S1, "o", -11)]
    fig, ax = plt.subplots(figsize=(8, 4.6))
    for label, col, c, m, dy in series:
        ys = [float(r[col]) for r in rows]
        ax.plot(x, ys, color=c, marker=m)
        end_label(ax, x[-1], ys[-1], label, c, dy)
    ax.set_xscale("log", base=2)
    ax.set_yscale("log")
    time_axis(ax)
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v / 1024:g} GiB" if v >= 1024 else f"{v:g} MiB"))
    ax.set_xlim(right=x[-1] * 6)
    ax.set_xlabel("sandbox memory (0.78% of pages dirty since the last snapshot)")
    ax.set_ylabel("fork latency, median")
    title(fig, "Fork latency vs memory size")
    save(fig, out, "size")


def dirty(results: Path, out: Path):
    rows = load(results, "dirty")
    x = [float(r["dirty_%"]) for r in rows]
    series = [("S0 fork", "S0_fork_med_us", S0, "s", 0, "-"), ("S1 fork", "S1_fork_med_us", S1, "o", 6, "-"),
              ("S1 spawn only", "S1_spawn_med_us", S1, "^", 0, "--")]
    fig, ax = plt.subplots(figsize=(8, 4.6))
    for label, col, c, m, dy, ls in series:
        ys = [float(r[col]) for r in rows]
        ax.plot(x, ys, color=c, marker=m, ls=ls)
        end_label(ax, x[-1], ys[-1], label, c, dy)
    ax.set_xscale("log")
    ax.set_yscale("log")
    time_axis(ax)
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g}%"))
    ax.set_xlim(right=x[-1] * 8)
    ax.set_xlabel("pages dirtied since the last snapshot (share of 64 MiB, contiguous)")
    ax.set_ylabel("latency, median")
    title(fig, "Fork latency vs dirty pages — S1 is O(dirty), S0 is O(memory)")
    save(fig, out, "dirty")


def depth(results: Path, out: Path):
    rows = load(results, "depth")
    cols = rows[0].keys()
    name_col = next(iter(cols))
    spawn_col = next(c for c in cols if c.startswith("spawn") and c.endswith("med_us"))
    names = [r[name_col] for r in rows]
    spawn = [float(r[spawn_col]) for r in rows]
    fig, ax = plt.subplots(figsize=(8, 0.45 * len(rows) + 1.4))
    y = list(range(len(rows)))[::-1]
    colors = [S1 if n.startswith("default") else "#86b6ef" for n in names]
    ax.barh(y, spawn, color=colors, height=0.6, edgecolor=SURFACE, linewidth=2)
    for yi, v in zip(y, spawn):
        ax.text(v * 1.12, yi, us_label(round(v, 1 if v < 1e3 else -1)), va="center", fontsize=8.5, color=TEXT)
    ax.set_yticks(y, names, fontsize=9)
    ax.set_xscale("log")
    time_axis(ax, "x")
    ax.set_xlim(right=max(spawn) * 4)
    ax.grid(axis="y", visible=False)
    ax.set_xlabel("spawn latency at the end of the chain, median; log scale")
    title(fig, "Fork-of-fork chain: 256 levels × 64 random dirty pages, by compaction setting")
    save(fig, out, "depth")


def memory(results: Path, out: Path):
    rows = [r for r in load(results, "memory") if r["strategy"] == "S1"]
    fig, ax = plt.subplots(figsize=(8, 4.2))
    for dirty_pages, c, m, ls, label in [("0", S1, "o", "-", "idle child"),
                                         ("16", S1, "s", "--", "child that wrote 16 pages")]:
        rs = [r for r in rows if r["dirty_pages_child"] == dirty_pages]
        x = [float(r["N"]) for r in rs]
        ys = [float(r["PSS_child_KiB"]) for r in rs]
        ax.plot(x, ys, color=c, marker=m, ls=ls)
        end_label(ax, x[-1], ys[-1], f"S1, {label}: {ys[-1]:g} KiB", c)
    ax.axhline(64, color=REF, lw=1, ls="--")
    ax.text(1, 61, "SPEC target: < 64 KiB per idle branch", color=TEXT_2, fontsize=8.5, va="top")
    ax.set_xscale("log")
    ax.set_xticks([1, 10, 100, 1000], ["1", "10", "100", "1,000"])
    ax.set_xlim(right=1000 * 40)
    ax.set_ylim(-4, 100)
    ax.set_xlabel("live children of one 64 MiB snapshot")
    ax.set_ylabel("added PSS per child (KiB)")
    title(fig, "Memory per branch, S1 (S0 adds 65,600 KiB per child: a full copy)")
    save(fig, out, "memory")


def main():
    results = Path(sys.argv[1])
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("docs/charts")
    out.mkdir(parents=True, exist_ok=True)
    for chart in (strategies, size, dirty, depth, memory, *EXTRA):
        if (results / f"{chart.__name__}.csv").exists():
            chart(results, out)


def lazy(results: Path, out: Path):
    rows = load(results, "lazy")
    labels = [r["child_writes"] for r in rows]
    x = list(range(len(rows)))
    fig, ax = plt.subplots(figsize=(8, 4.6))
    series = [("S1 fork + writes", "S1_fork_+_writes_med_us", S1, "o", 0),
              ("S2 fork + writes", "S2_fork_+_writes_med_us", S2, "D", 0),
              ("S2 fork only", "S2_fork_med_us", S2, "^", 0)]
    for label, col, c, m, dy in series:
        ys = [float(r[col]) for r in rows]
        ax.plot(x, ys, color=c, marker=m, ls="--" if label.endswith("only") else "-")
        end_label(ax, x[-1], ys[-1], label, c, dy)
    ax.set_xticks(x, [f"{int(v):,}" for v in labels])
    ax.set_yscale("log")
    time_axis(ax)
    ax.set_xlim(-0.3, len(rows) + 0.9)
    ax.set_xlabel("pages the child writes after the fork (64 MiB memory)")
    ax.set_ylabel("latency, median")
    title(fig, "S2 (lazy userfaultfd) forks faster, then pays ~12 µs per first touch")
    save(fig, out, "lazy")


def python(results: Path, out: Path):
    rows = load(results, "python")
    names = [f"{r['strategy']}, {r['threads']} thread{'s' if r['threads'] != '1' else ''}" for r in rows]
    wall = [float(r["wall_med_us"]) for r in rows]
    fig, ax = plt.subplots(figsize=(8, 0.5 * len(rows) + 1.5))
    y = list(range(len(rows)))[::-1]
    ax.barh(y, wall, color=[S2 if n.startswith("S2") else S1 for n in names], height=0.6,
            edgecolor=SURFACE, linewidth=2)
    for yi, v, r in zip(y, wall, rows):
        ax.text(v + max(wall) * 0.02, yi, f"{us_label(round(v, -2))}  ({r['per_branch']} per branch)",
                va="center", fontsize=8.5, color=TEXT)
    ax.set_yticks(y, names, fontsize=9)
    time_axis(ax, "x")
    ax.set_xlim(0, max(wall) * 1.55)
    ax.grid(axis="y", visible=False)
    ax.set_xlabel(f"wall time, median of {rows[0]['repeats']} runs ({rows[0]['branches']} branches each)")
    title(fig, "End to end: beam search over CPython branches (8 × keep 4 × depth 6)")
    save(fig, out, "python")


EXTRA = (lazy, python)

if __name__ == "__main__":
    main()
