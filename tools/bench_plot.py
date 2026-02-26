#!/usr/bin/env python3
"""Run jellyfish-rs benchmarks across backends and plot performance."""

import argparse
import csv
import os
import subprocess
import sys
import tempfile


def build_bench(project_root):
    """Build the bench binary in release mode."""
    print("Building bench binary...")
    result = subprocess.run(
        ["cargo", "build", "--release", "--features", "bins", "--bin", "bench"],
        cwd=project_root,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print("Build failed:", file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        sys.exit(1)
    print("Build complete.")


def run_bench(project_root, backend, timeout, log_path, extra_args=None):
    """Run the bench binary for a single backend."""
    bench_bin = os.path.join(project_root, "target", "release", "bench")
    cmd = [bench_bin, "-b", backend, "-t", str(timeout), "-l", log_path]
    if extra_args:
        cmd.extend(extra_args)

    label = " ".join(extra_args) if extra_args else backend
    print(f"  Running {backend} ({label}) for {timeout}s...")
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"  Bench failed for {backend}:", file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        return False

    # Print the benchmark summary from stderr
    for line in result.stderr.strip().splitlines():
        if line.startswith("===") or line.startswith("---") or ":" in line:
            print(f"    {line}")

    return True


def parse_log(log_path):
    """Parse a bench CSV log file, returning (elapsed_seconds, total_inserted) lists."""
    timestamps = []
    counts = []

    with open(log_path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            timestamps.append(float(row["timestamp"]))
            counts.append(int(row["total_inserted"]))

    if not timestamps:
        return [], []

    t0 = timestamps[0]
    elapsed = [t - t0 for t in timestamps]
    return elapsed, counts


def parse_log_dir(dir_path, cutoff=0):
    """Parse all log_*.csv files in a directory as sequential runs.

    Files are sorted by name (which embeds a timestamp) and stitched together
    so that total_inserted accumulates across runs and elapsed time is
    continuous.  The first *cutoff* inserts are dropped from the result.

    Returns (elapsed_seconds, total_inserted) lists.
    """
    import glob as globmod

    files = sorted(globmod.glob(os.path.join(dir_path, "log_*.csv")))
    if not files:
        print(f"No log_*.csv files found in {dir_path}", file=sys.stderr)
        return [], []

    all_timestamps = []
    all_counts = []
    cumulative = 0

    for fpath in files:
        with open(fpath) as f:
            reader = csv.DictReader(f)
            for row in reader:
                all_timestamps.append(float(row["timestamp"]))
                all_counts.append(cumulative + int(row["total_inserted"]))
        if all_counts:
            cumulative = all_counts[-1]

    if not all_timestamps:
        return [], []

    # Apply cutoff
    if cutoff > 0:
        start = 0
        for i, c in enumerate(all_counts):
            if c >= cutoff:
                start = i
                break
        else:
            # All data is below cutoff
            return [], []
        all_timestamps = all_timestamps[start:]
        all_counts = all_counts[start:]

    t0 = all_timestamps[0]
    elapsed = [t - t0 for t in all_timestamps]
    return elapsed, all_counts


def compute_speed(elapsed, counts, window=10):
    """Compute smoothed insertion speed (records/s) at each sample point.

    Uses a sliding window over consecutive samples to reduce noise.
    Returns (elapsed_times, record_counts, speeds) lists.
    """
    if len(elapsed) < 2:
        return [], [], []

    times = []
    record_counts = []
    speeds = []
    for i in range(window, len(elapsed)):
        dt = elapsed[i] - elapsed[i - window]
        dn = counts[i] - counts[i - window]
        if dt > 0:
            times.append(elapsed[i])
            record_counts.append(counts[i])
            speeds.append(dn / dt)

    return times, record_counts, speeds


def _millions_formatter(x, _pos):
    """Format axis tick as millions (e.g. 10M, 25.5M)."""
    val = x / 1_000_000
    return f"{val:,.0f}M" if val == int(val) else f"{val:,.1f}M"


def _time_formatter(x, _pos):
    """Format seconds as human-readable time (e.g. 5m, 1h 30m)."""
    x = int(x)
    if x < 60:
        return f"{x}s"
    minutes, secs = divmod(x, 60)
    hours, minutes = divmod(minutes, 60)
    if hours > 0:
        return f"{hours}h {minutes:02d}m" if minutes else f"{hours}h"
    return f"{minutes}m"


def _thousands_formatter(x, _pos):
    """Format axis tick as thousands with comma separators (e.g. 20,000)."""
    return f"{x:,.0f}"


def plot_results(results, output_path):
    """Plot insertion counts over time and speed vs record count."""
    try:
        import matplotlib.pyplot as plt
        from matplotlib.ticker import FuncFormatter
    except ImportError:
        print(
            "matplotlib is required for plotting. Install it with: pip install matplotlib",
            file=sys.stderr,
        )
        sys.exit(1)

    fig, (ax1, ax2, ax3) = plt.subplots(3, 1, figsize=(10, 14))

    for backend, (elapsed, counts) in results.items():
        if elapsed:
            ax1.plot(elapsed, counts, label=backend, linewidth=2)

    ax1.set_xlabel("Elapsed time")
    ax1.set_ylabel("Total records inserted")
    ax1.set_title("Jellyfish MPT — Insertion Performance by Backend")
    ax1.xaxis.set_major_formatter(FuncFormatter(_time_formatter))
    ax1.yaxis.set_major_formatter(FuncFormatter(_millions_formatter))
    ax1.legend()
    ax1.grid(True, alpha=0.3)

    for backend, (elapsed, counts) in results.items():
        if elapsed:
            times, rc, speeds = compute_speed(elapsed, counts)
            if times:
                ax2.plot(times, speeds, label=backend, linewidth=2)

    ax2.set_xlabel("Elapsed time")
    ax2.set_ylabel("Insertion speed (records/s)")
    ax2.set_title("Jellyfish MPT — Insertion Speed over Time")
    ax2.xaxis.set_major_formatter(FuncFormatter(_time_formatter))
    ax2.yaxis.set_major_formatter(FuncFormatter(_thousands_formatter))
    ax2.legend()
    ax2.grid(True, alpha=0.3)

    for backend, (elapsed, counts) in results.items():
        if elapsed:
            _times, rc, speeds = compute_speed(elapsed, counts)
            if rc:
                ax3.plot(rc, speeds, label=backend, linewidth=2)

    ax3.set_xlabel("Total records in tree")
    ax3.set_ylabel("Insertion speed (records/s)")
    ax3.set_title("Jellyfish MPT — Insertion Speed vs Tree Size")
    ax3.xaxis.set_major_formatter(FuncFormatter(_millions_formatter))
    ax3.yaxis.set_major_formatter(FuncFormatter(_thousands_formatter))
    ax3.legend()
    ax3.grid(True, alpha=0.3)

    fig.tight_layout()

    if output_path:
        fig.savefig(output_path, dpi=150)
        print(f"\nChart saved to {output_path}")
    else:
        plt.show()


SWEEP_BATCH_SIZES = [1, 100, 1_000, 10_000, 100_000]
SWEEP_WINDOW_SIZE = 100_000


def run_sweep(project_root, timeout, tmpdir):
    """Run rocks backend with varying batch sizes, return {label: (elapsed, counts)}."""
    results = {}
    for bs in SWEEP_BATCH_SIZES:
        label = f"batch_size={bs:,}"
        log_path = os.path.join(tmpdir, f"sweep_bs{bs}.csv")
        extra = ["-w", str(SWEEP_WINDOW_SIZE), "-c", str(bs)]
        ok = run_bench(project_root, "rocks", timeout, log_path, extra_args=extra)
        if ok and os.path.exists(log_path):
            results[label] = parse_log(log_path)
        else:
            results[label] = ([], [])
    return results


def plot_sweep(sweep_results, output_path):
    """Plot insertion speed vs tree size for each batch size."""
    try:
        import matplotlib.pyplot as plt
    except ImportError:
        print(
            "matplotlib is required for plotting. Install it with: pip install matplotlib",
            file=sys.stderr,
        )
        sys.exit(1)

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(10, 10))

    for label, (elapsed, counts) in sweep_results.items():
        if elapsed:
            ax1.plot(elapsed, counts, label=label, linewidth=2)

    ax1.set_xlabel("Elapsed time (s)")
    ax1.set_ylabel("Total records inserted")
    ax1.set_title(f"RocksDB — Insertion by Batch Size (window={SWEEP_WINDOW_SIZE:,})")
    ax1.legend()
    ax1.grid(True, alpha=0.3)

    for label, (elapsed, counts) in sweep_results.items():
        if elapsed:
            window = max(1, min(10, len(elapsed) // 20))
            _times, rc, speeds = compute_speed(elapsed, counts, window=window)
            if rc:
                ax2.plot(rc, speeds, label=label, linewidth=2)

    ax2.set_xlabel("Total records in tree")
    ax2.set_ylabel("Insertion speed (records/s)")
    ax2.set_title(f"RocksDB — Insertion Speed vs Tree Size by Batch Size")
    ax2.legend()
    ax2.grid(True, alpha=0.3)

    fig.tight_layout()

    if output_path:
        fig.savefig(output_path, dpi=150)
        print(f"Sweep chart saved to {output_path}")
    else:
        plt.show()


def print_summary(results):
    """Print a summary table of results."""
    label_width = max(10, max((len(k) for k in results), default=10))
    header = f"{'Backend':<{label_width}} {'Total Inserted':>15} {'Throughput':>15}"
    print(f"\n{header}")
    print("-" * len(header))
    for backend, (elapsed, counts) in results.items():
        if elapsed and counts:
            total = counts[-1]
            duration = elapsed[-1]
            throughput = total / duration if duration > 0 else 0
            print(f"{backend:<{label_width}} {total:>15,} {throughput:>12,.0f}/s")
        else:
            print(f"{backend:<{label_width}} {'(no data)':>15}")


def main():
    parser = argparse.ArgumentParser(
        description="Run jellyfish-rs benchmarks and plot results"
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=30,
        help="Duration per backend in seconds (default: 30)",
    )
    parser.add_argument(
        "--backends",
        default="rocks,sqlite,memory",
        help="Comma-separated list of backends (default: rocks,sqlite,memory)",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Output PNG path (default: show interactively)",
    )
    parser.add_argument(
        "--sweep",
        action="store_true",
        help="Also run a batch-size sweep for the rocks backend",
    )
    parser.add_argument(
        "--input",
        nargs="+",
        metavar="LABEL=PATH",
        help="Plot existing CSV log files instead of running benchmarks. "
        "Each argument is LABEL=PATH (e.g. rocks=run1.csv sqlite=run2.csv). "
        "A bare path without LABEL= uses the filename stem as the label.",
    )
    parser.add_argument(
        "--input-dir",
        nargs="+",
        metavar="LABEL=DIR",
        help="Plot from a directory of sequential log_*.csv files. "
        "Each argument is LABEL=DIR (e.g. rocks=bench_logs/). "
        "A bare path without LABEL= uses the directory name as the label.",
    )
    parser.add_argument(
        "--cutoff",
        type=int,
        default=2_000_000,
        help="Skip the first N inserts when using --input-dir (default: 2000000)",
    )
    args = parser.parse_args()

    if args.input:
        results = {}
        for spec in args.input:
            if "=" in spec:
                label, path = spec.split("=", 1)
            else:
                label = os.path.splitext(os.path.basename(spec))[0]
                path = spec
            if not os.path.exists(path):
                print(f"File not found: {path}", file=sys.stderr)
                sys.exit(1)
            results[label] = parse_log(path)

        print_summary(results)

        if any(elapsed for elapsed, _ in results.values()):
            plot_results(results, args.output)
        else:
            print("No data to plot.", file=sys.stderr)
        return

    if args.input_dir:
        results = {}
        for spec in args.input_dir:
            if "=" in spec:
                label, path = spec.split("=", 1)
            else:
                label = os.path.basename(os.path.normpath(spec))
                path = spec
            if not os.path.isdir(path):
                print(f"Directory not found: {path}", file=sys.stderr)
                sys.exit(1)
            results[label] = parse_log_dir(path, cutoff=args.cutoff)

        if args.cutoff > 0:
            print(f"(cutoff: first {args.cutoff:,} inserts excluded)")
        print_summary(results)

        if any(elapsed for elapsed, _ in results.values()):
            plot_results(results, args.output)
        else:
            print("No data to plot.", file=sys.stderr)
        return

    backends = [b.strip() for b in args.backends.split(",")]
    project_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

    build_bench(project_root)

    results = {}
    with tempfile.TemporaryDirectory(prefix="jellyfish_bench_") as tmpdir:
        for backend in backends:
            log_path = os.path.join(tmpdir, f"{backend}.csv")
            ok = run_bench(project_root, backend, args.timeout, log_path)
            if ok and os.path.exists(log_path):
                results[backend] = parse_log(log_path)
            else:
                results[backend] = ([], [])

        print_summary(results)

        if any(elapsed for elapsed, _ in results.values()):
            plot_results(results, args.output)
        else:
            print("No data to plot.", file=sys.stderr)

        if args.sweep:
            print("\n--- Batch-size sweep (rocks) ---")
            sweep_results = run_sweep(project_root, args.timeout, tmpdir)
            print_summary(sweep_results)
            if any(elapsed for elapsed, _ in sweep_results.values()):
                if args.output:
                    stem, ext = os.path.splitext(args.output)
                    sweep_output = f"{stem}_sweep{ext}"
                else:
                    sweep_output = None
                plot_sweep(sweep_results, sweep_output)


if __name__ == "__main__":
    main()
