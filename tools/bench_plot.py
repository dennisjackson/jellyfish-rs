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


def compute_speed(elapsed, counts, window=10):
    """Compute smoothed insertion speed (records/s) at each record count.

    Uses a sliding window over consecutive samples to reduce noise.
    Returns (record_counts, speeds) lists.
    """
    if len(elapsed) < 2:
        return [], []

    record_counts = []
    speeds = []
    for i in range(window, len(elapsed)):
        dt = elapsed[i] - elapsed[i - window]
        dn = counts[i] - counts[i - window]
        if dt > 0:
            record_counts.append(counts[i])
            speeds.append(dn / dt)

    return record_counts, speeds


def plot_results(results, output_path):
    """Plot insertion counts over time and speed vs record count."""
    try:
        import matplotlib.pyplot as plt
    except ImportError:
        print(
            "matplotlib is required for plotting. Install it with: pip install matplotlib",
            file=sys.stderr,
        )
        sys.exit(1)

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(10, 10))

    for backend, (elapsed, counts) in results.items():
        if elapsed:
            ax1.plot(elapsed, counts, label=backend, linewidth=2)

    ax1.set_xlabel("Elapsed time (s)")
    ax1.set_ylabel("Total records inserted")
    ax1.set_title("Jellyfish MPT — Insertion Performance by Backend")
    ax1.legend()
    ax1.grid(True, alpha=0.3)

    for backend, (elapsed, counts) in results.items():
        if elapsed:
            rc, speeds = compute_speed(elapsed, counts)
            if rc:
                ax2.plot(rc, speeds, label=backend, linewidth=2)

    ax2.set_xlabel("Total records in tree")
    ax2.set_ylabel("Insertion speed (records/s)")
    ax2.set_title("Jellyfish MPT — Insertion Speed vs Tree Size")
    ax2.legend()
    ax2.grid(True, alpha=0.3)

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
            rc, speeds = compute_speed(elapsed, counts, window=window)
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
