#!/usr/bin/env python3
"""Interleaved fixed-work A/B harness for the bench binary (DESIGN.md §12).

Each variant inserts exactly ``--entries`` entries from one seed into a fresh copy of the
reference database with the page cache dropped (``--keep-cache`` to skip); variants run
A,B,A,B,... so drift cannot favour one. Medians and spreads are reported, and every variant
must end at the same root hash.
"""

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time

PATTERNS = {
    "throughput": r"^Throughput:\s+([0-9.]+)",
    "insert_secs": r"^Insert time:\s+([0-9.]+)",
    "init_secs": r"^Init time:\s+([0-9.]+)",
    "peak_rss_gb": r"^Peak RSS:\s+([0-9.]+)",
    "leaves": r"^Leaves:\s+(\S+)",
    "frontier_depth": r"^Frontier depth:\s+(\d+)",
    "sorted_runs": r"^Sorted runs:\s+(\d+)",
    "puts_per_insert": r"puts/insert:\s+([0-9.]+)",
    "leaf_puts_per_insert": r"puts/insert:\s+[0-9.]+\s+\(([0-9.]+) leaf",
    "interior_puts_per_insert": r"puts/insert:.*\+ ([0-9.]+) interior",
    "bytes_per_insert": r"bytes staged/insert:\s+([0-9.]+)",
    "batches_per_insert": r"write batches/insert:\s+([0-9.]+)",
    "subtree_loads_per_insert": r"subtree loads/insert:\s+([0-9.]+)",
    "leaves_read_per_insert": r"leaves read/insert:\s+([0-9.]+)",
    "leaves_read_per_load": r"leaves read/load:\s+([0-9.]+)",
    "blocks_read_per_insert": r"blocks read/insert:\s+([0-9.]+)",
    "data_blocks_per_insert": r"blocks read/insert:\s+[0-9.]+\s+\(([0-9.]+) data",
    "index_blocks_per_insert": r"blocks read/insert:.*\+ ([0-9.]+) index",
    "seeks_per_insert": r"seeks/insert:\s+([0-9.]+)",
    "levels_depth": r"^Levels held:\s+0\.\.=(\d+)",
    "p50_ms": r"^\s+p50:\s+([0-9.]+) ms",
    "p99_ms": r"^\s+p99:\s+([0-9.]+) ms",
    "root_hash": r"^Root hash:\s+([0-9a-f]+)",
}


def checkpoint_copy(src, dst):
    """Copy a RocksDB directory, hardlinking the immutable SSTs (as RocksDB's Checkpoint
    does). Hardlinks share the page-cache inode, so dropping the copy's cache drops the
    reference's too, which a cold-cache protocol wants anyway."""
    os.makedirs(dst)
    for name in os.listdir(src):
        s, d = os.path.join(src, name), os.path.join(dst, name)
        if name.endswith(".sst"):
            os.link(s, d)
        else:
            shutil.copy2(s, d)


def drop_page_cache(path):
    """Evict every file under `path` from the page cache. Needs no privileges."""
    dropped = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            full = os.path.join(root, name)
            try:
                fd = os.open(full, os.O_RDONLY)
            except OSError:
                continue
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
                dropped += 1
            except OSError:
                pass
            finally:
                os.close(fd)
    return dropped


def parse_output(text):
    out = {}
    for key, pattern in PATTERNS.items():
        match = re.search(pattern, text, re.MULTILINE)
        if match:
            value = match.group(1)
            try:
                out[key] = float(value)
            except ValueError:
                out[key] = value
    return out


def run_variant(variant, args, scratch, run_index):
    """One measurement: fresh DB copy, cold cache, fixed work."""
    if os.path.exists(scratch):
        shutil.rmtree(scratch)
    copy_start = time.time()
    ref = variant.get("ref") or args.ref
    if args.link_copy:
        checkpoint_copy(ref, scratch)
    else:
        shutil.copytree(ref, scratch)
    copy_secs = time.time() - copy_start

    if not args.keep_cache:
        drop_page_cache(scratch)

    cmd = [
        variant["binary"],
        "-n", str(args.entries),
        "-w", str(args.window),
        "-c", str(args.batch),
        "-t", str(args.timeout),
        "--seed", str(args.seed),
    ] + variant["extra"] + [scratch]

    env = dict(os.environ, RUST_LOG=args.log_level)
    started = time.time()
    proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
    wall = time.time() - started

    combined = proc.stdout + proc.stderr
    parsed = parse_output(combined)
    parsed["wall_secs"] = wall
    parsed["copy_secs"] = copy_secs
    parsed["exit_code"] = proc.returncode
    parsed["run_index"] = run_index
    if proc.returncode != 0:
        parsed["stderr_tail"] = combined[-3000:]
    return parsed


def summarize(label, runs):
    ok = [r for r in runs if r.get("exit_code") == 0 and "throughput" in r]
    if not ok:
        return {"label": label, "runs": len(runs), "ok": 0}
    def med(key):
        vals = [r[key] for r in ok if key in r]
        return statistics.median(vals) if vals else None
    throughputs = sorted(r["throughput"] for r in ok)
    hashes = {r.get("root_hash") for r in ok}
    return {
        "label": label,
        "runs": len(runs),
        "ok": len(ok),
        "throughput_median": statistics.median(throughputs),
        "throughput_all": throughputs,
        "throughput_spread_pct": (
            100.0 * (throughputs[-1] - throughputs[0]) / throughputs[0] if throughputs[0] else None
        ),
        "insert_secs_median": med("insert_secs"),
        "puts_per_insert": med("puts_per_insert"),
        "leaf_puts_per_insert": med("leaf_puts_per_insert"),
        "interior_puts_per_insert": med("interior_puts_per_insert"),
        "bytes_per_insert": med("bytes_per_insert"),
        "batches_per_insert": med("batches_per_insert"),
        "subtree_loads_per_insert": med("subtree_loads_per_insert"),
        "leaves_read_per_insert": med("leaves_read_per_insert"),
        "blocks_read_per_insert": med("blocks_read_per_insert"),
        "seeks_per_insert": med("seeks_per_insert"),
        "levels_depth": med("levels_depth"),
        "p50_ms": med("p50_ms"),
        "p99_ms": med("p99_ms"),
        "peak_rss_gb": med("peak_rss_gb"),
        "sorted_runs": med("sorted_runs"),
        "init_secs": med("init_secs"),
        "root_hashes": sorted(h for h in hashes if h),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ref", required=True,
                    help="default reference database directory (never modified); a variant may "
                         "override it with LABEL=BINARY@ITS_OWN_REF")
    ap.add_argument("--entries", type=int, default=2_000_000)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--window", type=int, default=100_000)
    ap.add_argument("--batch", type=int, default=10_000)
    ap.add_argument("--timeout", type=int, default=7200, help="safety deadline, seconds")
    # Fixed so every variant inserts the same keys. Must not be the seed a reference
    # database was built with, or the run is a pure update workload.
    ap.add_argument("--seed", type=int, default=0x6e657720)  # "new "
    ap.add_argument("--scratch", default="/workspaces/project/ab_scratch")
    ap.add_argument("--out", default=None, help="write raw results as JSON here")
    ap.add_argument("--keep-cache", action="store_true", help="do not drop the page cache")
    ap.add_argument("--link-copy", action="store_true",
                    help="hardlink SSTs instead of copying them (for references too large "
                         "to copy; see checkpoint_copy)")
    ap.add_argument("--log-level", default="error")
    ap.add_argument("variants", nargs="+",
                    help="LABEL=BINARY[@REF][:extra bench args]  e.g. base=target/release/bench")
    args = ap.parse_args()

    variants = []
    for spec in args.variants:
        label, _, rest = spec.partition("=")
        binary, _, extra = rest.partition(":")
        # A format-changing variant names its own reference with `@`; it must have been
        # built from the same seed, entry count and configuration.
        binary, _, own_ref = binary.partition("@")
        variants.append({"label": label, "binary": binary, "ref": own_ref or None,
                         "extra": extra.split() if extra else []})

    for v in variants:
        if not os.path.exists(v["binary"]):
            sys.exit(f"no such binary: {v['binary']}")
        if v["ref"] and not os.path.isdir(v["ref"]):
            sys.exit(f"no such reference database: {v['ref']}")
    if not os.path.isdir(args.ref):
        sys.exit(f"no such reference database: {args.ref}")

    results = {v["label"]: [] for v in variants}
    print(f"reference={args.ref}  entries={args.entries:,}  reps={args.reps}  "
          f"cache={'warm' if args.keep_cache else 'dropped'}", flush=True)

    for rep in range(args.reps):
        for v in variants:  # interleaved, so drift cannot favour one variant
            res = run_variant(v, args, args.scratch, rep)
            results[v["label"]].append(res)
            status = "ok" if res.get("exit_code") == 0 else f"EXIT {res.get('exit_code')}"
            print(f"  rep {rep} {v['label']:<22} {status:>6}  "
                  f"{res.get('throughput', 0):>10,.0f} entries/s  "
                  f"puts/ins {res.get('puts_per_insert', float('nan')):.3f}  "
                  f"blocks/ins {res.get('blocks_read_per_insert', float('nan')):.3f}  "
                  f"leaves-read/ins {res.get('leaves_read_per_insert', float('nan')):.1f}",
                  flush=True)
            if res.get("exit_code") != 0:
                print(res.get("stderr_tail", "")[-1500:], flush=True)

    if os.path.exists(args.scratch):
        shutil.rmtree(args.scratch)

    summaries = [summarize(v["label"], results[v["label"]]) for v in variants]

    print("\n=== summary (median of %d) ===" % args.reps)
    header = (f"{'variant':<24}{'entries/s':>12}{'spread':>8}{'puts/ins':>10}"
              f"{'blks/ins':>10}{'lvs-rd/ins':>12}{'p99 ms':>9}{'RSS GB':>8}{'runs':>6}")
    print(header)
    print("-" * len(header))
    baseline = summaries[0]
    for s in summaries:
        if not s.get("ok"):
            print(f"{s['label']:<24}  ALL RUNS FAILED")
            continue
        delta = ""
        if s is not baseline and baseline.get("throughput_median"):
            pct = 100.0 * (s["throughput_median"] / baseline["throughput_median"] - 1)
            delta = f"  ({pct:+.1f}%)"
        print(f"{s['label']:<24}{s['throughput_median']:>12,.0f}"
              f"{s['throughput_spread_pct'] or 0:>7.1f}%"
              f"{s['puts_per_insert'] or 0:>10.3f}"
              f"{s['blocks_read_per_insert'] or 0:>10.3f}"
              f"{s['leaves_read_per_insert'] or 0:>12.1f}"
              f"{s['p99_ms'] or 0:>9.1f}"
              f"{s['peak_rss_gb'] or 0:>8.2f}"
              f"{s['sorted_runs'] or 0:>6.0f}{delta}")

    # The correctness gate.
    all_hashes = {h for s in summaries for h in s.get("root_hashes", [])}
    print()
    if len(all_hashes) == 1:
        print(f"root hash agrees across all variants and runs: {next(iter(all_hashes))[:16]}...")
    else:
        print("!!! ROOT HASH MISMATCH - a variant computed a different tree:")
        for s in summaries:
            print(f"    {s['label']:<24} {s.get('root_hashes')}")

    if args.out:
        os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
        with open(args.out, "w") as fh:
            json.dump({"config": vars(args), "runs": results, "summaries": summaries}, fh, indent=2)
        print(f"\nraw results -> {args.out}")

    return 0 if len(all_hashes) <= 1 else 1


if __name__ == "__main__":
    sys.exit(main())
