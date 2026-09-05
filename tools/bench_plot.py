#!/usr/bin/env -S uv run --no-project --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["matplotlib"]
# ///
"""Turn bench logs (`<db>/bench-log.jsonl`) into a performance report.

Panels: throughput (rate, batch-latency percentiles with stalls marked), per-insert traffic
(differenced between census records) and shape/memory. Plotted against tree size or wall
clock, switched in the page; frontier deepenings are drawn as verticals.

A run is `LABEL=PATH`, `LABEL=PATH,PATH,...` (stitched) or `LABEL=DIR` (the database's own
log, else every `log_*.csv` in it -- the `bench_loop.py` layout). A bare path uses its stem as
the label. Legacy CSV logs load with whatever columns they have.

Output by extension: `.html` (default, self-contained) or `.png` (matplotlib).

    uv run --no-project tools/bench_plot.py bigdb -o report.html
    uv run --no-project tools/bench_plot.py a=bench_out/a b=bench_out/b -o ab.html
    uv run --no-project tools/bench_plot.py growth=bigdb --min-leaves 2000000 -o growth.png
    uv run --no-project tools/bench_plot.py --run 60 -o fresh.html   # bench a fresh tree first
"""

import argparse
import csv
import datetime
import glob
import html
import json
import os
import shutil
import subprocess
import sys

# Palette: the categorical slots and the single-hue ordinal ramp, both validated for light and
# dark surfaces (adjacent CVD dE 9.1 light / 8.4 dark; ordinal monotone in L, light end clear of
# the surface). Slots are assigned in fixed order and never cycled -- a panel that would need a
# fifth metric is split instead. Three light-mode slots sit below 3:1 against the surface, so the
# relief rule applies: every series carries a direct endpoint label and every panel has a table
# view, both shipped below.
CATEGORICAL = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100"]
CATEGORICAL_DARK = ["#3987e5", "#d95926", "#199e70", "#c98500"]
ORDINAL = ["#86b6ef", "#3987e5", "#1c5cab", "#0d366b"]
ORDINAL_DARK = ["#184f95", "#256abf", "#3987e5", "#86b6ef"]

MAX_POINTS = 700  # per series; the eye cannot use more and the file gets large fast


class Run:
    """One label's rows, stitched across files and across the runs inside a file: `elapsed` is
    continuous, `inserted` cumulative, `leaves` absolute (falls back to `inserted` for logs
    without it). `census` holds the periodic traffic records, which are per-phase totals, so a
    reader differences consecutive ones."""

    FIELDS = ("elapsed", "inserted", "leaves", "rss", "batch_secs", "batch_entries",
              "sorted_runs", "frontier_depth")

    def __init__(self, label):
        self.label = label
        self.meta = []
        for f in self.FIELDS:
            setattr(self, f, [])
        self.census = []  # one dict per census record, `leaves`/`elapsed`/`run_index` added
        self.run_starts = []  # row index where each run begins: rate windows stay inside one
        self._latency = None  # four panel series and a table read it; bucketing is not cheap

    def add_file(self, path):
        """One log file: JSON lines (`bench-log.jsonl`) or a pre-round-24 CSV."""
        with open(path) as fh:
            lines = fh.read().splitlines()
        segments = [] if lines and lines[0].startswith("{") else None
        if segments is None:
            self._add_csv(lines)
            return
        # A `run` record opens a run. A truncated last line (a run killed mid-write) is skipped
        # rather than guessed at, which is the whole reason for one object a line.
        for line in lines:
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            kind = record.get("kind")
            if kind == "run":
                self.meta.append(" ".join(f"{k}={v}" for k, v in record.items() if k != "kind"))
                segments.append([])
            elif kind in ("batch", "census") and segments:
                segments[-1].append(record)
        for records in segments:
            self._add_segment(records)

    def _add_csv(self, lines):
        rows = [line for line in lines
                if not line.startswith("#") or self.meta.append(line.strip()[2:])]
        self._add_segment([dict(row, kind="batch") for row in csv.DictReader(rows)])

    def _add_segment(self, records):
        """One run's records, stitched onto whatever came before."""
        t_offset = self.elapsed[-1] if self.elapsed else 0.0
        n_offset = self.inserted[-1] if self.inserted else 0
        self.run_starts.append(len(self.elapsed))
        first_ts = None
        for row in records:
            if not row.get("total_inserted"):
                continue  # a CSV run killed mid-write leaves a partial last row
            ts = float(row["timestamp"])
            first_ts = ts if first_ts is None else first_ts
            elapsed = float(row["elapsed_secs"]) if "elapsed_secs" in row else ts - first_ts
            inserted = n_offset + int(row["total_inserted"])
            leaves = int(row["leaf_count"]) if row.get("leaf_count") else inserted
            if row["kind"] == "census":
                # Tagged with its run: the counts are totals since the run's census phase
                # opened, so differencing across a restart would read as negative traffic.
                self.census.append(dict(row, leaves=leaves, elapsed=t_offset + elapsed,
                                        run_index=len(self.run_starts) - 1))
                continue
            self.elapsed.append(t_offset + elapsed)
            self.inserted.append(inserted)
            self.leaves.append(leaves)
            self.rss.append(int(row["rss_bytes"]) if row.get("rss_bytes") else None)
            self.batch_secs.append(float(row["batch_secs"]) if row.get("batch_secs") else None)
            self.batch_entries.append(int(row["batch_entries"]) if row.get("batch_entries")
                                      else None)
            self.sorted_runs.append(int(row["sorted_runs"]) if row.get("sorted_runs") is not None
                                    and row.get("sorted_runs") != "" else None)
            self.frontier_depth.append(int(row["frontier_depth"])
                                       if row.get("frontier_depth") is not None
                                       and row.get("frontier_depth") != "" else None)

    # -- derived series ------------------------------------------------------------------

    def segments(self):
        bounds = self.run_starts + [len(self.elapsed)]
        return list(zip(bounds, bounds[1:]))

    def rates(self, window_entries):
        """(leaves, elapsed, entries/s) per row, the rate taken over the last `window_entries`
        inserts of the same run."""
        out_leaves, out_elapsed, out_rates = [], [], []
        for start, end in self.segments():
            left = start
            for i in range(start, end):
                while self.inserted[i] - self.inserted[left] > window_entries and left < i:
                    left += 1
                dn = self.inserted[i] - self.inserted[left]
                dt = self.elapsed[i] - self.elapsed[left]
                if dn >= window_entries and dt > 0:
                    out_leaves.append(self.leaves[i])
                    out_elapsed.append(self.elapsed[i])
                    out_rates.append(dn / dt)
        return out_leaves, out_elapsed, out_rates

    def latency(self, min_batches=250, max_buckets=150):
        """Batch-duration percentiles per non-overlapping bucket of batches.

        Non-overlapping because a sliding window would reuse the same slow batch in fifty
        consecutive p99s and draw one stall as a plateau. Buckets are sized in *batches*, not
        inserts, and never hold fewer than `min_batches`: a bucket of fifty samples has no p99 --
        `int(0.99 * 50)` is the last index, so the p99 line would be the max line drawn twice.
        Each bucket is then an independent sample of the distribution at that tree size, which is
        the thing a windowed mean rate cannot show."""
        if self._latency is not None:
            return self._latency
        usable = sum(1 for t in self.batch_secs if t is not None)
        if not usable:
            self._latency = []
            return self._latency
        per = max(min_batches, -(-usable // max_buckets))
        out = []
        for start, end in self.segments():
            rows = [i for i in range(start, end) if self.batch_secs[i] is not None]
            for k in range(0, len(rows), per):
                chunk = rows[k:k + per]
                # A trailing sliver is dropped rather than plotted: its percentiles are drawn
                # from too few samples to sit on the same axis as the buckets before it.
                if len(chunk) < per // 2:
                    continue
                times = sorted(self.batch_secs[i] for i in chunk)
                last = chunk[-1]
                out.append({
                    "leaves": self.leaves[last], "elapsed": self.elapsed[last],
                    "n": len(times), "p50": pct(times, 0.50), "p90": pct(times, 0.90),
                    "p99": pct(times, 0.99), "max": times[-1],
                    # Share of the bucket's insert time spent in its slowest 1% of batches: the
                    # one number that says whether the tail is a curiosity or the cost.
                    "tail_share": tail_share(times),
                })
        self._latency = out
        return out

    def stalls(self, limit=60, factor=5.0):
        """The outlier batches: slower than `factor` x the run's median, worst first."""
        times = sorted(t for t in self.batch_secs if t is not None)
        if not times:
            return []
        threshold = pct(times, 0.50) * factor
        rows = [{"leaves": self.leaves[i], "elapsed": self.elapsed[i], "secs": t,
                 "sorted_runs": self.sorted_runs[i], "frontier_depth": self.frontier_depth[i]}
                for i, t in enumerate(self.batch_secs) if t is not None and t >= threshold]
        rows.sort(key=lambda r: -r["secs"])
        return rows[:limit]

    def latency_by_sorted_runs(self):
        """Median batch duration bucketed by the LSM shape at that batch. Not a time series --
        it is the correlation the read-cost model predicts, read straight off the log."""
        buckets = {}
        for runs, secs in zip(self.sorted_runs, self.batch_secs):
            if runs is None or secs is None:
                continue
            buckets.setdefault(runs, []).append(secs)
        return [(k, len(v), pct(sorted(v), 0.50)) for k, v in sorted(buckets.items())]

    def transitions(self):
        """Where the frontier deepened: the tree-shape steps, at batch resolution. `levels_depth`
        is census-only, so it is looked up from the nearest census record at or after."""
        out, previous = [], None
        for i, depth in enumerate(self.frontier_depth):
            if depth is None or depth == previous:
                continue
            if previous is not None:
                levels = next((int(c["levels_depth"]) for c in self.census
                               if c.get("levels_depth") and c["elapsed"] >= self.elapsed[i]), None)
                out.append({"leaves": self.leaves[i], "elapsed": self.elapsed[i],
                            "from": previous, "to": depth, "levels": levels})
            previous = depth
        return out

    def per_insert(self, field, window_entries):
        """(leaves, elapsed, per-insert value) per census record, over the last `window_entries`
        inserts of the same run. A census record holds totals since its run's phase opened, so a
        *pair* of them differences into the traffic over just the inserts between -- which is why
        the window slides here exactly as it does in `rates`, rather than taking consecutive
        records and hoping the cadence matches."""
        # A summed series tolerates fields a log predates (history_puts, from the hashchains
        # format): they count as zero, so old and new logs plot on one axis.
        fields = [f for f in field.split("+") if all(f in c for c in self.census[:1])]
        if not fields:
            return [], [], []
        out_leaves, out_elapsed, out_values = [], [], []
        left = 0
        for i, later in enumerate(self.census):
            while left < i and (
                    self.census[left]["run_index"] != later["run_index"]
                    or int(later["entries"]) - int(self.census[left]["entries"]) > window_entries):
                left += 1
            earlier = self.census[left]
            entries = int(later["entries"]) - int(earlier["entries"])
            if earlier["run_index"] != later["run_index"] or entries < window_entries:
                continue
            delta = sum(int(later[f]) - int(earlier[f]) for f in fields)
            out_leaves.append(later["leaves"])
            out_elapsed.append(later["elapsed"])
            out_values.append(delta / entries)
        return out_leaves, out_elapsed, out_values

    def census_at(self, fraction, window_entries):
        """Every per-insert ratio at one point through the run, for the table view."""
        rows = {}
        for name, field in CENSUS_FIELDS:
            n, _e, v = self.per_insert(field, window_entries)
            if v:
                i = min(len(v) - 1, int(fraction * len(v)))
                rows[name] = (n[i], v[i])
        return rows

    def series(self, field):
        """(leaves, elapsed, value) for a per-batch field, skipping rows that lack it."""
        out = ([], [], [])
        for i, value in enumerate(getattr(self, field)):
            if value is None:
                continue
            out[0].append(self.leaves[i])
            out[1].append(self.elapsed[i])
            out[2].append(value)
        return out

    def trim(self, min_leaves):
        self.census = [c for c in self.census if c["leaves"] >= min_leaves]
        keep = [i for i, n in enumerate(self.leaves) if n >= min_leaves]
        if not keep:
            for f in self.FIELDS:
                setattr(self, f, [])
            self.run_starts = []
            return
        first = keep[0]
        self._latency = None
        self.run_starts = sorted({max(s, first) for s in self.run_starts if s <= keep[-1]}
                                 | {first})
        self.run_starts = [s - first for s in self.run_starts]
        for f in self.FIELDS:
            setattr(self, f, getattr(self, f)[first:])


# name, log field(s) -- summed when joined by '+'
CENSUS_FIELDS = [
    ("puts", "leaf_puts+interior_puts+history_puts"),
    ("leaf puts", "leaf_puts"),
    ("interior puts", "interior_puts"),
    ("history puts", "history_puts"),
    ("subtree loads", "subtree_loads"),
    ("bytes staged", "bytes_staged"),
    ("write batches", "write_batches"),
    ("data blocks", "data_blocks_read"),
    ("index blocks", "index_blocks_read"),
    ("seeks", "seeks"),
    ("leaves read", "leaves_read"),
]


def pct(sorted_values, p):
    return sorted_values[min(len(sorted_values) - 1, int(p * len(sorted_values)))]


def tail_share(sorted_values):
    total = sum(sorted_values)
    if not total:
        return 0.0
    cut = pct(sorted_values, 0.99)
    return sum(v for v in sorted_values if v >= cut) / total


def thin(n, cap=MAX_POINTS):
    """Indices of at most `cap` evenly spread samples, always keeping the last."""
    if n <= cap:
        return list(range(n))
    step = n / cap
    return sorted({int(i * step) for i in range(cap)} | {n - 1})


def load_run(spec):
    label, _, path = spec.rpartition("=") if "=" in spec else ("", "", spec)
    if not label:
        label = os.path.splitext(os.path.basename(os.path.normpath(path)))[0]
    paths = []
    for part in path.split(","):
        if os.path.isdir(part):
            # A database directory carries its own log; a log directory is the older
            # one-file-per-run layout.
            own = os.path.join(part, "bench-log.jsonl")
            if os.path.exists(own):
                paths.append(own)
            else:
                paths.extend(sorted(glob.glob(os.path.join(part, "log_*.csv"))))
        elif os.path.exists(part):
            paths.append(part)
        else:
            sys.exit(f"no such log: {part}")
    if not paths:
        sys.exit(f"no logs for {label}")
    run = Run(label)
    for p in paths:
        run.add_file(p)
    return run


# ---------------------------------------------------------------------------------------
# Panels
#
# A panel is a title, a y-unit and a handful of series, each carrying both x-arrays so the page
# can switch between tree size and wall clock without a round trip. Two rules shape the list:
# every panel has ONE y-scale (two measures of different magnitude get two panels, never a
# secondary axis), and a panel never holds more than four series.
# ---------------------------------------------------------------------------------------


class Panel:
    def __init__(self, key, title, ylabel, note="", ramp="categorical", ylog=False, group=None):
        self.d = {"key": key, "title": title, "ylabel": ylabel, "note": note, "ramp": ramp,
                  "ylog": ylog, "group": group, "x": {}, "series": [], "points": []}

    def add(self, label, leaves, elapsed, values, slot, style="line"):
        if not values:
            return self
        keep = thin(len(values))
        xs = ([leaves[i] for i in keep], [round(elapsed[i], 2) for i in keep])
        ref = self._xref(xs)
        self.d["series"].append({"label": label, "xr": ref, "slot": slot, "style": style,
                                 "y": [trim_float(values[i]) for i in keep]})
        return self

    def scatter(self, label, leaves, elapsed, values, tips):
        self.d["points"] = [{"x": [n, round(e, 2)], "y": trim_float(v), "tip": t}
                            for n, e, v, t in zip(leaves, elapsed, values, tips)]
        self.d["points_label"] = label
        return self

    def _xref(self, xs):
        key = str(xs[0])
        if key not in self.d["x"]:
            self.d["x"][key] = {"ref": f"x{len(self.d['x'])}", "leaves": xs[0], "elapsed": xs[1]}
        return self.d["x"][key]["ref"]

    def finish(self):
        self.d["x"] = list(self.d["x"].values())
        return self.d if self.d["series"] else None


def trim_float(v):
    """Six significant figures is more than any of these measures carries, and it roughly halves
    the embedded JSON."""
    return float(f"{v:.6g}")


def build_panels(runs, window_entries):
    """The report's sections. Multi-metric panels facet by run rather than overlaying, so colour
    means the same thing in every panel: within a panel it identifies the metric, and a run is
    named in the panel title. Single-metric panels overlay the runs, and there colour identifies
    the run."""
    multi = len(runs) > 1
    sections = []

    def overlay(panel, getter):
        for slot, run in enumerate(runs):
            panel.add(run.label, *getter(run), slot % 4)
        return panel.finish()

    def facet(key, title, ylabel, metrics, note="", ramp="categorical", ylog=False, after=None):
        """One panel per run rather than one panel with every run's every metric. The facets
        share a y-scale (`group`): small multiples on independent scales invite exactly the
        comparison they cannot support."""
        out = []
        for run in runs:
            name = f"{title} — {run.label}" if multi else title
            panel = Panel(f"{key}:{run.label}", name, ylabel, note, ramp, ylog, group=key)
            for slot, (label, getter) in enumerate(metrics):
                panel.add(label, *getter(run), slot)
            if after:
                after(panel, run)
            built = panel.finish()
            if built:
                out.append(built)
        return out

    # -- throughput ----------------------------------------------------------------------
    rate = overlay(
        Panel("rate", "Insert rate", "Inserts per second",
              f"Windowed over {window_entries:,} inserts."),
        lambda r: r.rates(window_entries))

    def latency_points(panel, run):
        rows = run.stalls()
        panel.scatter("stall", [r["leaves"] for r in rows], [r["elapsed"] for r in rows],
                      [r["secs"] * 1e3 for r in rows],
                      [f"{r['secs'] * 1e3:,.0f} ms, {r['sorted_runs']} sorted runs"
                       for r in rows])

    bucket = next((r.latency()[0]["n"] for r in runs if r.latency()), 0)
    latency = facet(
        "latency", "Batch latency", "Milliseconds",
        [(name, (lambda k: lambda r: bucketed(r, k))(key))
         for name, key in (("p50", "p50"), ("p90", "p90"), ("p99", "p99"), ("max", "max"))],
        note=(f"Percentiles over non-overlapping buckets of {bucket:,} batches; dots are "
              "individual batches above 5x the run's median. A windowed mean rate averages this "
              "whole spread into one point."),
        ramp="ordinal", ylog=True, after=latency_points)
    if any(latency):
        sections.append({"title": "Throughput", "panels": [p for p in [rate] if p] + latency})
    elif rate:
        sections.append({"title": "Throughput", "panels": [rate]})

    # -- per-insert traffic --------------------------------------------------------------
    def ratio(field):
        return lambda r: r.per_insert(field, window_entries)

    # Log y on the two panels a frontier deepening spikes: rewriting the tree top costs a window
    # ~20 puts and ~1.7 KB an insert against a steady state of 2 and 160, and on a linear axis
    # that one window flattens the whole rest of the run into a hairline.
    traffic = facet(
        "write", "Write cost per insert", "Operations per insert",
        [("puts", ratio("leaf_puts+interior_puts+history_puts")),
         ("leaf puts", ratio("leaf_puts")), ("interior puts", ratio("interior_puts")),
         ("history puts", ratio("history_puts")), ("subtree loads", ratio("subtree_loads"))],
        note="The write-path headline: REVIEW.md tracks puts/insert 11.301 -> 2.556 -> 1.910. "
             "The spikes are frontier deepenings rewriting the tree top.", ylog=True)
    traffic += facet(
        "read", "Read cost per insert", "Operations per insert",
        [("data blocks", ratio("data_blocks_read")), ("index blocks", ratio("index_blocks_read")),
         ("seeks", ratio("seeks"))],
        note=("A subtree scan costs about one data block and one seek per sorted run whatever "
              "range it covers. Index blocks are plotted separately, not folded in: while they "
              "stay cache-resident this line sits at zero, and that is worth being able to see."))
    traffic.append(overlay(
        Panel("bytes", "Bytes staged per insert", "Bytes per insert",
              note="Spikes are frontier deepenings, as on the write-cost panel.", ylog=True),
        ratio("bytes_staged")))
    traffic.append(overlay(
        Panel("leaves_read", "Leaves read per insert", "Leaves per insert",
              note=("Structural, not the read headline: a leaf row is 66 bytes and a block holds "
                    "62 of them, so scans covering 3 and 30 leaves cost about the same blocks.")),
        ratio("leaves_read")))
    traffic = [p for p in traffic if p]
    if traffic:
        sections.append({"title": "RocksDB traffic per insert", "panels": traffic})

    # -- shape and memory ----------------------------------------------------------------
    shape = [
        overlay(Panel("sorted_runs", "Sorted runs", "Sorted runs",
                      note="The LSM shape the read path pays for."),
                lambda r: r.series("sorted_runs")),
        overlay(Panel("frontier", "Frontier depth", "Depth"),
                lambda r: r.series("frontier_depth")),
        overlay(Panel("rss", "Resident set", "GB",
                      note="Process RSS, not the tree's own accounting."),
                lambda r: (lambda n, e, v: (n, e, [b / 1e9 for b in v]))(*r.series("rss"))),
    ]
    shape = [p for p in shape if p]
    if shape:
        sections.append({"title": "Shape and memory", "panels": shape})
    return sections


def bucketed(run, key):
    rows = run.latency()
    return ([r["leaves"] for r in rows], [r["elapsed"] for r in rows],
            [r[key] * 1e3 for r in rows])


def build_markers(runs):
    """Frontier-deepening events, drawn as verticals across every panel."""
    out = []
    for run in runs:
        for t in run.transitions():
            label = f"frontier {t['from']}→{t['to']}"
            if len(runs) > 1:
                label = f"{run.label}: {label}"
            out.append({"x": [t["leaves"], round(t["elapsed"], 2)], "label": label})
    return out


# ---------------------------------------------------------------------------------------
# Tables -- the table view every panel needs, plus the aggregates that are not time series
# ---------------------------------------------------------------------------------------


def build_tables(runs, window_entries):
    tables = []

    rows = []
    for run in runs:
        if not run.elapsed:
            continue
        times = sorted(t for t in run.batch_secs if t is not None)
        _l, _e, rates = run.rates(window_entries)
        rows.append([run.label, f"{len(run.elapsed):,}",
                     f"{run.leaves[0]:,} → {run.leaves[-1]:,}",
                     f"{run.inserted[-1]:,}", clock(run.elapsed[-1]),
                     f"{sum(rates) / len(rates):,.0f}/s" if rates else "-",
                     f"{max(b for b in run.rss if b) / 1e9:.2f} GB"
                     if any(run.rss) else "-"])
    tables.append({"title": "Runs", "columns": ["run", "rows", "leaves start → end",
                                                "inserted", "elapsed", "mean rate", "peak RSS"],
                   "rows": rows})

    rows = []
    for run in runs:
        times = sorted(t for t in run.batch_secs if t is not None)
        if not times:
            continue
        rows.append([run.label, f"{len(times):,}"]
                    + [f"{pct(times, p) * 1e3:,.1f}" for p in (0.5, 0.9, 0.99, 0.999)]
                    + [f"{times[-1] * 1e3:,.1f}",
                       f"{pct(times, 0.99) / pct(times, 0.5):.1f}x",
                       f"{tail_share(times) * 100:.1f}%"])
    if rows:
        tables.append({
            "title": "Batch latency",
            "columns": ["run", "batches", "p50 ms", "p90 ms", "p99 ms", "p99.9 ms", "max ms",
                        "p99/p50", "time in slowest 1%"],
            "rows": rows,
            "note": "The last column is the share of all insert time spent in the slowest 1% of "
                    "batches -- what a windowed mean rate spreads invisibly across its window."})

    rows = []
    for run in runs:
        transitions = run.transitions()
        for stall in run.stalls(limit=12):
            near = min(transitions, key=lambda t: abs(t["elapsed"] - stall["elapsed"]),
                       default=None)
            close = near and abs(near["elapsed"] - stall["elapsed"]) < 2.0
            rows.append([run.label, f"{stall['secs'] * 1e3:,.1f}",
                         f"{stall['leaves']:,}", clock(stall["elapsed"]),
                         str(stall["sorted_runs"]),
                         f"frontier {near['from']}→{near['to']}" if close else ""])
    if rows:
        tables.append({"title": "Slowest batches",
                       "columns": ["run", "ms", "leaves", "at", "sorted runs", "coincides with"],
                       "rows": rows,
                       "note": "The last column is filled when the batch lands within two "
                               "seconds of a frontier deepening."})

    rows = []
    for run in runs:
        for t in run.transitions():
            rows.append([run.label, f"{t['leaves']:,}", clock(t["elapsed"]),
                         f"{t['from']} → {t['to']}",
                         str(t["levels"]) if t["levels"] is not None else ""])
    if rows:
        tables.append({"title": "Tree-shape transitions",
                       "columns": ["run", "leaves", "at", "frontier depth", "levels depth"],
                       "rows": rows})

    rows = []
    for run in runs:
        for runs_count, n, median in run.latency_by_sorted_runs():
            rows.append([run.label, str(runs_count), f"{n:,}", f"{median * 1e3:.2f}"])
    if rows:
        tables.append({"title": "Batch latency by sorted runs",
                       "columns": ["run", "sorted runs", "batches", "median ms"],
                       "rows": rows,
                       "note": "Not a time series: the cost model says a scan pays a block and a "
                               "seek per sorted run, and this is that claim measured directly."})

    rows = []
    for run in runs:
        early, late = run.census_at(0.05, window_entries), run.census_at(0.95, window_entries)
        for name, _field in CENSUS_FIELDS:
            if name in early and name in late:
                rows.append([run.label, name, f"{early[name][0]:,}", f"{early[name][1]:,.3f}",
                             f"{late[name][0]:,}", f"{late[name][1]:,.3f}"])
    if rows:
        tables.append({"title": "Per-insert traffic, early against late",
                       "columns": ["run", "metric", "leaves (early)", "per insert",
                                   "leaves (late)", "per insert"],
                       "rows": rows})
    return tables


# ---------------------------------------------------------------------------------------
# Renderers
# ---------------------------------------------------------------------------------------


def clock(x):
    if x < 60:
        return f"{x:.0f}s" if x == int(x) else f"{x:.1f}s"
    m, s = divmod(int(x), 60)
    h, m = divmod(m, 60)
    return f"{h}h {m:02d}m" if h else f"{m}m {s:02d}s"


def millions(x, _pos=None):
    v = x / 1e6
    return f"{v:,.0f}M" if v == int(v) else f"{v:,.1f}M"


def render_html(report, path):
    template = HTML_TEMPLATE
    template = template.replace("__TITLE__", html.escape(report["title"]))
    template = template.replace("__DATA__", json.dumps(report, separators=(",", ":")))
    with open(path, "w") as fh:
        fh.write(template)


def render_png(report, path):
    """The flat chart: the same panels, stacked, against tree size."""
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import FuncFormatter

    panels = [p for section in report["sections"] for p in section["panels"]]
    fig, axes = plt.subplots(len(panels), 1, figsize=(11, 3.6 * len(panels)), squeeze=False)
    for ax, panel in zip(axes[:, 0], panels):
        ramp = ORDINAL if panel["ramp"] == "ordinal" else CATEGORICAL
        xs = {x["ref"]: x["leaves"] for x in panel["x"]}
        for series in panel["series"]:
            ax.plot(xs[series["xr"]], series["y"], label=series["label"],
                    color=ramp[series["slot"] % len(ramp)], linewidth=1.6,
                    drawstyle="steps-post" if series["style"] == "step" else "default")
        if panel["points"]:
            ax.plot([p["x"][0] for p in panel["points"]], [p["y"] for p in panel["points"]],
                    "o", markersize=3.5, color="#d03b3b", label=panel.get("points_label"),
                    linestyle="none")
        for marker in report["markers"]:
            ax.axvline(marker["x"][0], color="#c3c2b7", linewidth=0.8, zorder=0)
        if panel["ylog"]:
            ax.set_yscale("log")
        ax.set_title(panel["title"], fontsize=11, loc="left")
        ax.set_xlabel("Leaves in tree")
        ax.set_ylabel(panel["ylabel"])
        ax.xaxis.set_major_formatter(FuncFormatter(millions))
        ax.grid(True, alpha=0.25, linewidth=0.6)
        if len(panel["series"]) + bool(panel["points"]) > 1:
            ax.legend(fontsize=8, frameon=False)
    fig.tight_layout()
    fig.savefig(path, dpi=140)


def summary(report):
    """The same report, in the terminal."""
    for table in report["tables"]:
        if not table["rows"]:
            continue
        widths = [max(len(str(r[i])) for r in [table["columns"]] + table["rows"])
                  for i in range(len(table["columns"]))]
        print(f"\n{table['title']}")
        print("  " + "  ".join(c.ljust(w) for c, w in zip(table["columns"], widths)))
        print("  " + "  ".join("-" * w for w in widths))
        for row in table["rows"]:
            print("  " + "  ".join(str(c).ljust(w) for c, w in zip(row, widths)))


def run_bench(seconds, db):
    """Bench a fresh database; the log lands inside it."""
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    subprocess.run(["cargo", "build", "--release", "--bin", "bench"], cwd=root, check=True)
    shutil.rmtree(db, ignore_errors=True)
    subprocess.run([os.path.join(root, "target", "release", "bench"), "-t", str(seconds), db],
                   check=True)


HTML_TEMPLATE = r"""<!DOCTYPE html>
<meta charset="utf-8">
<title>__TITLE__</title>
<style>
  :root {
    color-scheme: light;
    --surface-1: #fcfcfb; --plane: #f9f9f7;
    --text-primary: #0b0b0b; --text-secondary: #52514e; --muted: #898781;
    --grid: #e1e0d9; --axis: #c3c2b7; --border: rgba(11,11,11,0.10);
    --critical: #d03b3b;
    --s0: #2a78d6; --s1: #eb6834; --s2: #1baf7a; --s3: #eda100;
    --o0: #86b6ef; --o1: #3987e5; --o2: #1c5cab; --o3: #0d366b;
  }
  @media (prefers-color-scheme: dark) {
    :root:where(:not([data-theme="light"])) {
      color-scheme: dark;
      --surface-1: #1a1a19; --plane: #0d0d0d;
      --text-primary: #ffffff; --text-secondary: #c3c2b7; --muted: #898781;
      --grid: #2c2c2a; --axis: #383835; --border: rgba(255,255,255,0.10);
      --s0: #3987e5; --s1: #d95926; --s2: #199e70; --s3: #c98500;
      --o0: #184f95; --o1: #256abf; --o2: #3987e5; --o3: #86b6ef;
    }
  }
  :root[data-theme="dark"] {
    color-scheme: dark;
    --surface-1: #1a1a19; --plane: #0d0d0d;
    --text-primary: #ffffff; --text-secondary: #c3c2b7; --muted: #898781;
    --grid: #2c2c2a; --axis: #383835; --border: rgba(255,255,255,0.10);
    --s0: #3987e5; --s1: #d95926; --s2: #199e70; --s3: #c98500;
    --o0: #184f95; --o1: #256abf; --o2: #3987e5; --o3: #86b6ef;
  }
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--plane); color: var(--text-primary);
         font: 14px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
  .wrap { max-width: 1080px; margin: 0 auto; padding: 28px 20px 80px; }
  h1 { font-size: 20px; font-weight: 600; margin: 0 0 4px; }
  h2 { font-size: 12px; font-weight: 600; letter-spacing: 0.08em; text-transform: uppercase;
       color: var(--muted); margin: 36px 0 12px; }
  .sub { color: var(--text-secondary); font-size: 13px; margin: 0 0 20px; }
  .sub code { font-size: 12px; color: var(--muted); }
  /* One filter row above everything it scopes -- never per-chart controls. */
  .controls { display: flex; flex-wrap: wrap; gap: 8px; align-items: center;
              padding: 10px 12px; background: var(--surface-1);
              border: 1px solid var(--border); border-radius: 10px;
              position: sticky; top: 0; z-index: 20; }
  .controls .spacer { flex: 1; }
  .controls label { color: var(--text-secondary); font-size: 12px; }
  button, select { font: inherit; font-size: 12px; color: var(--text-primary);
                   background: var(--plane); border: 1px solid var(--border);
                   border-radius: 7px; padding: 4px 10px; cursor: pointer; }
  button[aria-pressed="true"] { background: var(--text-primary); color: var(--surface-1);
                                border-color: var(--text-primary); }
  .card { background: var(--surface-1); border: 1px solid var(--border); border-radius: 10px;
          padding: 14px 16px 8px; margin-bottom: 14px; }
  .card h3 { font-size: 14px; font-weight: 600; margin: 0 0 2px; }
  .note { color: var(--text-secondary); font-size: 12px; margin: 0 0 8px; max-width: 76ch; }
  .legend { display: flex; flex-wrap: wrap; gap: 4px 14px; margin: 6px 0 2px; }
  .legend button { border: none; background: none; padding: 2px 0; display: flex; gap: 6px;
                   align-items: center; color: var(--text-secondary); font-size: 12px; }
  .legend button[aria-pressed="false"] { opacity: 0.35; }
  .swatch { width: 10px; height: 10px; border-radius: 3px; flex: none; }
  /* A zoom drag must not paint the axis labels blue on its way across. */
  svg { display: block; width: 100%; touch-action: none; user-select: none;
        -webkit-user-select: none; }
  .tick { fill: var(--muted); font-size: 10px; font-variant-numeric: tabular-nums; }
  .axis-title { fill: var(--muted); font-size: 10px; }
  .endlabel { fill: var(--text-secondary); font-size: 10px; }
  .gridline { stroke: var(--grid); stroke-width: 1; shape-rendering: crispEdges; }
  .axisline { stroke: var(--axis); stroke-width: 1; shape-rendering: crispEdges; }
  .marker { stroke: var(--axis); stroke-width: 1; }
  .marker-label { fill: var(--muted); font-size: 9px; }
  .cursor { stroke: var(--text-secondary); stroke-width: 1; opacity: 0.6; }
  .tip { position: fixed; pointer-events: none; z-index: 40; background: var(--surface-1);
         border: 1px solid var(--border); border-radius: 8px; padding: 8px 10px;
         font-size: 12px; box-shadow: 0 6px 24px rgba(0,0,0,0.16); display: none;
         min-width: 170px; }
  .tip .h { color: var(--muted); margin-bottom: 4px; }
  .tip .r { display: flex; gap: 10px; align-items: center; justify-content: space-between; }
  .tip .r span:last-child { font-variant-numeric: tabular-nums; color: var(--text-primary); }
  .tip .r span:first-child { display: flex; gap: 6px; align-items: center;
                             color: var(--text-secondary); }
  details { margin-top: 6px; }
  summary { cursor: pointer; color: var(--muted); font-size: 12px; padding: 4px 0; }
  table { border-collapse: collapse; width: 100%; font-size: 12px; margin: 6px 0 10px; }
  th, td { text-align: left; padding: 5px 10px 5px 0; border-bottom: 1px solid var(--grid);
           white-space: nowrap; }
  th { color: var(--muted); font-weight: 600; }
  td { font-variant-numeric: tabular-nums; color: var(--text-secondary); }
  td:first-child, th:first-child { color: var(--text-primary); }
  .scroll { overflow-x: auto; }
</style>
<div class="wrap">
  <h1 id="title"></h1>
  <p class="sub" id="sub"></p>
  <div class="controls">
    <label>x-axis</label>
    <button id="x-leaves" aria-pressed="true">Tree size</button>
    <button id="x-elapsed" aria-pressed="false">Elapsed</button>
    <button id="markers" aria-pressed="true">Shape transitions</button>
    <span class="spacer"></span>
    <span id="zoomnote" class="sub" style="margin:0;font-size:12px"></span>
    <button id="reset">Reset zoom</button>
    <button id="theme">Theme</button>
  </div>
  <div id="sections"></div>
  <h2>Tables</h2>
  <div id="tables"></div>
</div>
<div class="tip" id="tip"></div>
<script>
const REPORT = __DATA__;
const PANELS = REPORT.sections.flatMap(s => s.panels);
const NS = "http://www.w3.org/2000/svg";
const PAD = {l: 58, r: 96, t: 22, b: 26};
const H = 168;

let xKey = "leaves";
let domain = null;          // [min,max] in current xKey, null = full
let showMarkers = true;
const hidden = new Set();   // "panelKey/seriesLabel"

const el = (tag, attrs = {}, parent) => {
  const n = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
  if (parent) parent.appendChild(n);
  return n;
};
const h = (tag, attrs = {}, parent) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "text") n.textContent = v; else n.setAttribute(k, v);
  }
  if (parent) parent.appendChild(n);
  return n;
};
const color = (panel, slot) =>
  `var(--${panel.ramp === "ordinal" ? "o" : "s"}${slot % 4})`;

// -- formatting -------------------------------------------------------------------------
function fmtLeaves(v) {
  if (Math.abs(v) >= 1e9) return (v / 1e9).toFixed(v % 1e9 ? 2 : 0) + "B";
  if (Math.abs(v) >= 1e6) return (v / 1e6).toFixed(v % 1e6 ? 1 : 0) + "M";
  if (Math.abs(v) >= 1e3) return (v / 1e3).toFixed(0) + "K";
  return String(Math.round(v));
}
function fmtClock(s) {
  if (s < 60) return (Math.round(s * 10) / 10) + "s";
  const m = Math.floor(s / 60), sec = Math.round(s % 60);
  if (m < 60) return m + "m " + String(sec).padStart(2, "0") + "s";
  return Math.floor(m / 60) + "h " + String(m % 60).padStart(2, "0") + "m";
}
const fmtX = v => xKey === "leaves" ? fmtLeaves(v) : fmtClock(v);
function fmtY(v) {
  const a = Math.abs(v);
  if (a === 0) return "0";
  if (a >= 1e4) return v.toLocaleString(undefined, {maximumFractionDigits: 0});
  // Round to a sensible step, then let String() drop the trailing zeros a fixed
  // precision would leave behind ("50.00" on a millisecond axis).
  if (a >= 10) return String(Math.round(v * 10) / 10);
  if (a >= 1) return String(Math.round(v * 100) / 100);
  return v.toPrecision(2);
}

// -- scales -----------------------------------------------------------------------------
function niceTicks(lo, hi, count) {
  if (!(hi > lo)) return [lo];
  const raw = (hi - lo) / count;
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const step = [1, 2, 2.5, 5, 10].find(m => raw <= m * mag) * mag;
  const out = [];
  for (let v = Math.ceil(lo / step) * step; v <= hi + step * 1e-9; v += step) out.push(v);
  return out;
}
function logTicks(lo, hi) {
  const out = [];
  for (let e = Math.floor(Math.log10(lo)); e <= Math.ceil(Math.log10(hi)); e++)
    for (const m of [1, 2, 5]) {
      const v = m * Math.pow(10, e);
      if (v >= lo && v <= hi) out.push(v);
    }
  return out.length > 1 ? out : [lo, hi];
}

function fullDomain() {
  let lo = Infinity, hi = -Infinity;
  for (const s of REPORT.sections) for (const p of s.panels) for (const x of p.x) {
    const a = x[xKey];
    if (a.length) { lo = Math.min(lo, a[0]); hi = Math.max(hi, a[a.length - 1]); }
  }
  return lo < hi ? [lo, hi] : [0, 1];
}
const activeDomain = () => domain || fullDomain();

// -- panel rendering --------------------------------------------------------------------
function drawPanel(panel, svg) {
  const W = svg.clientWidth || 800;
  svg.setAttribute("viewBox", `0 0 ${W} ${H + PAD.t + PAD.b}`);
  svg.setAttribute("height", H + PAD.t + PAD.b);
  while (svg.firstChild) svg.removeChild(svg.firstChild);

  const [x0, x1] = activeDomain();
  const xs = Object.fromEntries(panel.x.map(x => [x.ref, x[xKey]]));
  const visible = panel.series.filter(s => !hidden.has(panel.key + "/" + s.label));

  // y-domain from what is visible inside the current x-window only: zooming into a quiet
  // stretch should rescale to it, not stay squashed by a stall off-screen. Faceted panels
  // (`group`) pool their peers' data, so one run's stall rescales every facet and the
  // small multiples stay comparable by eye.
  let lo = Infinity, hi = -Infinity;
  for (const peer of panel.group ? PANELS.filter(p => p.group === panel.group) : [panel]) {
    const pxs = Object.fromEntries(peer.x.map(x => [x.ref, x[xKey]]));
    for (const s of peer.series) {
      if (hidden.has(peer.key + "/" + s.label)) continue;
      const ax = pxs[s.xr];
      for (let i = 0; i < s.y.length; i++)
        if (ax[i] >= x0 && ax[i] <= x1) { lo = Math.min(lo, s.y[i]); hi = Math.max(hi, s.y[i]); }
    }
    for (const p of peer.points || [])
      if (p.x[xKey === "leaves" ? 0 : 1] >= x0 && p.x[xKey === "leaves" ? 0 : 1] <= x1)
        { lo = Math.min(lo, p.y); hi = Math.max(hi, p.y); }
  }
  if (!isFinite(lo)) { lo = 0; hi = 1; }
  if (lo === hi) { lo -= 0.5; hi += 0.5; }

  const log = panel.ylog && lo > 0;
  if (!log) { if (lo > 0 && lo / hi > 0.4) lo = 0; hi += (hi - lo) * 0.08; }
  else { lo /= 1.3; hi *= 1.3; }

  const px = v => PAD.l + (v - x0) / (x1 - x0) * (W - PAD.l - PAD.r);
  const py = v => log
    ? PAD.t + H - (Math.log10(Math.max(v, lo)) - Math.log10(lo)) /
        (Math.log10(hi) - Math.log10(lo)) * H
    : PAD.t + H - (v - lo) / (hi - lo) * H;

  const ticks = log ? logTicks(lo, hi) : niceTicks(lo, hi, 4);
  for (const t of ticks) {
    const y = py(t);
    if (y < PAD.t - 1 || y > PAD.t + H + 1) continue;
    el("line", {class: "gridline", x1: PAD.l, x2: W - PAD.r, y1: y, y2: y}, svg);
    el("text", {class: "tick", x: PAD.l - 7, y: y + 3, "text-anchor": "end"}, svg)
      .textContent = fmtY(t);
  }
  el("line", {class: "axisline", x1: PAD.l, x2: W - PAD.r,
              y1: PAD.t + H, y2: PAD.t + H}, svg);
  for (const t of niceTicks(x0, x1, 6)) {
    const x = px(t);
    if (x < PAD.l - 1 || x > W - PAD.r + 1) continue;
    el("text", {class: "tick", x: x, y: PAD.t + H + 15, "text-anchor": "middle"}, svg)
      .textContent = fmtX(t);
  }
  el("text", {class: "axis-title", x: 0, y: 11}, svg).textContent = panel.ylabel;

  if (showMarkers) {
    // Thinned at draw time, not in the data: the early frontier steps sit inside the first
    // percent of a 300 M-leaf run and would pile into one smear, but they separate as soon as
    // the reader zooms in. Lines need less room than labels, so the two thin independently.
    let lastLine = -1e9, lastLabel = -1e9, row = 0;
    for (const m of REPORT.markers) {
      const v = m.x[xKey === "leaves" ? 0 : 1], x = px(v);
      if (x < PAD.l || x > W - PAD.r || x - lastLine < 26) continue;
      lastLine = x;
      el("line", {class: "marker", x1: x, x2: x, y1: PAD.t, y2: PAD.t + H,
                  "stroke-dasharray": "2 3"}, svg);
      if (panel.first && x - lastLabel > 78) {
        lastLabel = x;
        el("text", {class: "marker-label", x: x + 3, y: PAD.t + 9 + (row++ % 2) * 10}, svg)
          .textContent = m.label;
      }
    }
  }

  // Series, then a direct endpoint label each: the relief rule for the light-mode slots that
  // sit below 3:1, and it means identity never rests on hue alone.
  const ends = [];
  for (const s of visible) {
    const ax = xs[s.xr];
    let d = "", pen = false;
    for (let i = 0; i < s.y.length; i++) {
      if (ax[i] < x0 || ax[i] > x1) { pen = false; continue; }
      const X = px(ax[i]), Y = py(s.y[i]);
      if (!pen) { d += `M${X.toFixed(1)} ${Y.toFixed(1)}`; pen = true; }
      else if (s.style === "step") d += `H${X.toFixed(1)}V${Y.toFixed(1)}`;
      else d += `L${X.toFixed(1)} ${Y.toFixed(1)}`;
    }
    if (!d) continue;
    el("path", {d, fill: "none", stroke: color(panel, s.slot), "stroke-width": 2,
                "stroke-linejoin": "round", "stroke-linecap": "round"}, svg);
    for (let i = s.y.length - 1; i >= 0; i--)
      if (ax[i] >= x0 && ax[i] <= x1) { ends.push({y: py(s.y[i]), label: s.label,
                                                   c: color(panel, s.slot)}); break; }
  }
  // Push apart, then slide the whole stack back inside the plot. Clamping each label to the
  // floor instead would pile every one of them on the same pixel, which is what three flat
  // series near zero do.
  ends.sort((a, b) => a.y - b.y);
  for (let i = 1; i < ends.length; i++)
    if (ends[i].y - ends[i - 1].y < 12) ends[i].y = ends[i - 1].y + 12;
  const overflow = ends.length ? ends[ends.length - 1].y - (PAD.t + H) : 0;
  if (overflow > 0) for (const e of ends) e.y -= overflow;
  for (const e of visible.length > 1 ? ends : []) {
    el("circle", {cx: W - PAD.r + 8, cy: e.y, r: 3, fill: e.c}, svg);
    el("text", {class: "endlabel", x: W - PAD.r + 15, y: e.y + 3}, svg).textContent = e.label;
  }

  for (const p of panel.points || []) {
    const v = p.x[xKey === "leaves" ? 0 : 1];
    if (v < x0 || v > x1) continue;
    // A 2px surface ring rather than a stroke border, so overlapping stalls stay countable.
    el("circle", {cx: px(v), cy: py(p.y), r: 3.5, fill: "var(--critical)",
                  stroke: "var(--surface-1)", "stroke-width": 2}, svg);
  }

  const cursor = el("line", {class: "cursor", y1: PAD.t, y2: PAD.t + H,
                             visibility: "hidden"}, svg);
  svg._state = {panel, xs, px, py, x0, x1, W, cursor, visible};
}

// -- interaction ------------------------------------------------------------------------
const tip = document.getElementById("tip");

function nearest(arr, v) {
  let lo = 0, hi = arr.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid] < v) lo = mid + 1; else hi = mid;
  }
  if (lo > 0 && Math.abs(arr[lo - 1] - v) < Math.abs(arr[lo] - v)) lo--;
  return lo;
}

function onMove(ev, svg) {
  const st = svg._state;
  if (!st) return;
  const rect = svg.getBoundingClientRect();
  const scale = st.W / rect.width;
  const sx = (ev.clientX - rect.left) * scale;
  if (sx < PAD.l || sx > st.W - PAD.r) return onLeave();
  const value = st.x0 + (sx - PAD.l) / (st.W - PAD.l - PAD.r) * (st.x1 - st.x0);
  for (const other of document.querySelectorAll("svg")) {
    const os = other._state;
    if (!os) continue;
    const ox = os.px(value);
    os.cursor.setAttribute("x1", ox);
    os.cursor.setAttribute("x2", ox);
    os.cursor.setAttribute("visibility",
      ox >= PAD.l && ox <= os.W - PAD.r ? "visible" : "hidden");
  }
  let rows = "";
  let shown = null;
  for (const s of st.visible) {
    const ax = st.xs[s.xr];
    if (!ax.length) continue;
    const i = nearest(ax, value);
    if (shown === null) shown = ax[i];
    rows += `<div class="r"><span><span class="swatch" style="background:` +
      `${color(st.panel, s.slot)}"></span>${s.label}</span><span>${fmtY(s.y[i])}</span></div>`;
  }
  if (shown === null) return;
  tip.innerHTML = `<div class="h">${xKey === "leaves" ? fmtLeaves(shown) + " leaves"
    : fmtClock(shown)}</div>` + rows;
  tip.style.display = "block";
  const w = tip.offsetWidth, hgt = tip.offsetHeight;
  tip.style.left = Math.min(ev.clientX + 14, window.innerWidth - w - 8) + "px";
  tip.style.top = Math.max(8, Math.min(ev.clientY - hgt / 2,
                                       window.innerHeight - hgt - 8)) + "px";
}
function onLeave() {
  tip.style.display = "none";
  for (const svg of document.querySelectorAll("svg"))
    if (svg._state) svg._state.cursor.setAttribute("visibility", "hidden");
}

function attachDrag(svg) {
  let from = null, band = null;
  const valueAt = ev => {
    const st = svg._state, rect = svg.getBoundingClientRect();
    const sx = (ev.clientX - rect.left) * (st.W / rect.width);
    return st.x0 + (Math.max(PAD.l, Math.min(sx, st.W - PAD.r)) - PAD.l) /
      (st.W - PAD.l - PAD.r) * (st.x1 - st.x0);
  };
  svg.addEventListener("pointerdown", ev => {
    from = valueAt(ev);
    band = el("rect", {y: PAD.t, height: H, fill: "var(--text-secondary)",
                       opacity: 0.12}, svg);
    svg.setPointerCapture(ev.pointerId);
  });
  svg.addEventListener("pointermove", ev => {
    onMove(ev, svg);
    if (from === null) return;
    const st = svg._state, a = st.px(from), b = st.px(valueAt(ev));
    band.setAttribute("x", Math.min(a, b));
    band.setAttribute("width", Math.abs(b - a));
  });
  svg.addEventListener("pointerup", ev => {
    if (from === null) return;
    const to = valueAt(ev);
    const [lo, hi] = [Math.min(from, to), Math.max(from, to)];
    from = null;
    band.remove();
    // A click is not a zoom: below a few pixels of travel it is the reader pointing at a value.
    if (hi - lo > (activeDomain()[1] - activeDomain()[0]) * 0.005) { domain = [lo, hi]; }
    renderAll();
  });
  svg.addEventListener("dblclick", () => { domain = null; renderAll(); });
  svg.addEventListener("pointerleave", onLeave);
}

// -- build ------------------------------------------------------------------------------
function panelTable(panel) {
  const [x0, x1] = activeDomain();
  const rows = [];
  const ref = panel.x[0];
  if (!ref) return "";
  const axis = ref[xKey];
  const step = Math.max(1, Math.ceil(axis.length / 40));
  let head = `<tr><th>${xKey === "leaves" ? "leaves" : "elapsed"}</th>` +
    panel.series.map(s => `<th>${s.label}</th>`).join("") + "</tr>";
  for (let i = 0; i < axis.length; i += step) {
    if (axis[i] < x0 || axis[i] > x1) continue;
    rows.push(`<tr><td>${xKey === "leaves" ? Math.round(axis[i]).toLocaleString()
      : fmtClock(axis[i])}</td>` + panel.series.map(s => {
        const a = Object.fromEntries(panel.x.map(x => [x.ref, x[xKey]]))[s.xr];
        const j = nearest(a, axis[i]);
        return `<td>${fmtY(s.y[j])}</td>`;
      }).join("") + "</tr>");
  }
  return `<div class="scroll"><table>${head}${rows.join("")}</table></div>`;
}

function build() {
  document.getElementById("title").textContent = REPORT.title;
  document.getElementById("sub").innerHTML = REPORT.subtitle;
  const host = document.getElementById("sections");
  let first = true;
  for (const section of REPORT.sections) {
    h("h2", {text: section.title}, host);
    for (const panel of section.panels) {
      panel.first = first; first = false;
      const card = h("div", {class: "card"}, host);
      h("h3", {text: panel.title}, card);
      if (panel.note) h("p", {class: "note", text: panel.note}, card);
      if (panel.series.length + (panel.points || []).length > 1) {
        const legend = h("div", {class: "legend"}, card);
        for (const s of panel.series) {
          const b = h("button", {"aria-pressed": "true"}, legend);
          h("span", {class: "swatch", style: `background:${color(panel, s.slot)}`}, b);
          h("span", {text: s.label}, b);
          b.onclick = () => {
            const key = panel.key + "/" + s.label;
            hidden.has(key) ? hidden.delete(key) : hidden.add(key);
            b.setAttribute("aria-pressed", String(!hidden.has(key)));
            renderAll();
          };
        }
        if ((panel.points || []).length) {
          const b = h("button", {"aria-pressed": "true", disabled: "true"}, legend);
          h("span", {class: "swatch", style: "background:var(--critical)"}, b);
          h("span", {text: panel.points_label + " (" + panel.points.length + ")"}, b);
        }
      }
      const svg = el("svg", {}, card);
      attachDrag(svg);
      panel._svg = svg;
      const det = h("details", {}, card);
      h("summary", {text: "Table view"}, det);
      panel._table = h("div", {}, det);
    }
  }

  const thost = document.getElementById("tables");
  for (const t of REPORT.tables) {
    if (!t.rows.length) continue;
    const card = h("div", {class: "card"}, thost);
    h("h3", {text: t.title}, card);
    if (t.note) h("p", {class: "note", text: t.note}, card);
    const scroll = h("div", {class: "scroll"}, card);
    scroll.innerHTML = "<table><tr>" + t.columns.map(c => `<th>${c}</th>`).join("") +
      "</tr>" + t.rows.map(r => "<tr>" + r.map(c => `<td>${c}</td>`).join("") + "</tr>")
      .join("") + "</table>";
  }
}

function renderAll() {
  for (const s of REPORT.sections) for (const p of s.panels) {
    drawPanel(p, p._svg);
    if (p._table.parentElement.open) p._table.innerHTML = panelTable(p);
  }
  const [a, b] = activeDomain();
  document.getElementById("zoomnote").textContent =
    domain ? `${fmtX(a)} – ${fmtX(b)}` : "drag to zoom · double-click to reset";
}

document.getElementById("x-leaves").onclick = () => setX("leaves");
document.getElementById("x-elapsed").onclick = () => setX("elapsed");
function setX(k) {
  if (k === xKey) return;
  xKey = k; domain = null;
  document.getElementById("x-leaves").setAttribute("aria-pressed", String(k === "leaves"));
  document.getElementById("x-elapsed").setAttribute("aria-pressed", String(k === "elapsed"));
  renderAll();
}
// A log without `frontier_depth` (the pre-round-24 CSVs) has nothing to toggle.
const markerButton = document.getElementById("markers");
if (!REPORT.markers.length) markerButton.remove();
else markerButton.onclick = ev => {
  showMarkers = !showMarkers;
  ev.currentTarget.setAttribute("aria-pressed", String(showMarkers));
  renderAll();
};
document.getElementById("reset").onclick = () => { domain = null; renderAll(); };
document.getElementById("theme").onclick = () => {
  const now = document.documentElement.getAttribute("data-theme");
  const next = now === "dark" ? "light" : now === "light" ? null : "dark";
  if (next) document.documentElement.setAttribute("data-theme", next);
  else document.documentElement.removeAttribute("data-theme");
};
document.addEventListener("toggle", ev => {
  if (ev.target.tagName === "DETAILS" && ev.target.open) renderAll();
}, true);

build();
renderAll();
new ResizeObserver(() => renderAll()).observe(document.getElementById("sections"));
</script>
"""


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("runs", nargs="*",
                    help="LABEL=PATH[,PATH..] or LABEL=DIR; bare path uses its stem")
    ap.add_argument("-o", "--output", default="bench-report.html",
                    help="output path; .html for the report, .png for the flat chart")
    ap.add_argument("--window-entries", type=int, default=500_000,
                    help="inserts each sample spans (default 500,000)")
    ap.add_argument("--min-leaves", type=int, default=0,
                    help="drop rows while the tree is smaller than this")
    ap.add_argument("--run", type=float, metavar="SECONDS",
                    help="first bench a fresh tree for this long (into tempdb/plot) and "
                         "include it")
    ap.add_argument("--quiet", action="store_true", help="skip the terminal tables")
    args = ap.parse_args()

    runs = [load_run(spec) for spec in args.runs]
    if args.run:
        run_bench(args.run, "tempdb/plot")
        runs.append(load_run("fresh=tempdb/plot"))
    if not runs:
        ap.error("nothing to plot: give a log, a directory, or --run")
    for run in runs:
        run.trim(args.min_leaves)
    if not any(run.elapsed for run in runs):
        sys.exit("no rows left after --min-leaves")

    report = {
        "title": "Bench report: " + ", ".join(r.label for r in runs),
        "subtitle": " &middot; ".join(
            [html.escape(m) for r in runs for m in r.meta]
            + [f"generated {datetime.datetime.now():%Y-%m-%d %H:%M}"]),
        "sections": build_panels(runs, args.window_entries),
        "markers": build_markers(runs),
        "tables": build_tables(runs, args.window_entries),
    }

    if not args.quiet:
        summary(report)
    if args.output.endswith(".png"):
        render_png(report, args.output)
    else:
        render_html(report, args.output)
    print(f"\nreport -> {args.output}")


if __name__ == "__main__":
    main()
