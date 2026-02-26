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


def run_bench(project_root, backend, timeout, log_path):
    """Run the bench binary for a single backend."""
    bench_bin = os.path.join(project_root, "target", "release", "bench")
    cmd = [bench_bin, "-b", backend, "-t", str(timeout), "-l", log_path]

    print(f"  Running {backend} backend for {timeout}s...")
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


def plot_results(results, output_path):
    """Plot insertion counts over time for each backend."""
    try:
        import matplotlib.pyplot as plt
    except ImportError:
        print(
            "matplotlib is required for plotting. Install it with: pip install matplotlib",
            file=sys.stderr,
        )
        sys.exit(1)

    fig, ax = plt.subplots(figsize=(10, 6))

    for backend, (elapsed, counts) in results.items():
        if elapsed:
            ax.plot(elapsed, counts, label=backend, linewidth=2)

    ax.set_xlabel("Elapsed time (s)")
    ax.set_ylabel("Total records inserted")
    ax.set_title("Jellyfish MPT — Insertion Performance by Backend")
    ax.legend()
    ax.grid(True, alpha=0.3)

    fig.tight_layout()

    if output_path:
        fig.savefig(output_path, dpi=150)
        print(f"\nChart saved to {output_path}")
    else:
        plt.show()


def print_summary(results):
    """Print a summary table of results."""
    print("\n{:<10} {:>15} {:>15}".format("Backend", "Total Inserted", "Throughput"))
    print("-" * 42)
    for backend, (elapsed, counts) in results.items():
        if elapsed and counts:
            total = counts[-1]
            duration = elapsed[-1]
            throughput = total / duration if duration > 0 else 0
            print(
                "{:<10} {:>15,} {:>12,.0f}/s".format(backend, total, throughput)
            )
        else:
            print("{:<10} {:>15}".format(backend, "(no data)"))


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
    args = parser.parse_args()

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


if __name__ == "__main__":
    main()
