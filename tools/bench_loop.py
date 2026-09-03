#!/usr/bin/env python3
"""Run the bench binary in a loop, growing one database across runs. CLI output and peak
RSS are kept per run; the per-batch log is bench's own, inside the database."""

import os
import subprocess
import sys
import threading
import time
from datetime import datetime


DB_DIR = "bigdb"
LOG_DIR = "bench_logs"

BACKEND = "rocks"
TIMEOUT = 7200
WINDOW_SIZE = 100_000
BATCH_SIZE = 10_000


def get_peak_rss_kb(pid):
    """Read VmHWM (peak resident set size) from /proc/<pid>/status."""
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])  # value is in kB
    except (FileNotFoundError, ProcessLookupError):
        pass
    return None


def poll_peak_rss(pid, result, stop_event):
    """Poll peak RSS in a background thread, storing the last reading."""
    while not stop_event.is_set():
        rss = get_peak_rss_kb(pid)
        if rss is not None:
            result[0] = rss
        stop_event.wait(1.0)


def format_rss(kb):
    """Format kB value as a human-readable string."""
    if kb is None:
        return "unknown"
    if kb >= 1_048_576:
        return f"{kb / 1_048_576:.2f} GB"
    if kb >= 1024:
        return f"{kb / 1024:.1f} MB"
    return f"{kb} kB"


def log_msg(meta_f, msg):
    """Print a message and also write it to the meta log."""
    print(msg)
    meta_f.write(msg + "\n")
    meta_f.flush()


def main():
    project_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    os.chdir(project_root)

    os.makedirs(LOG_DIR, exist_ok=True)

    bench_bin = os.path.join(project_root, "target", "release", "bench")
    meta_log = os.path.join(LOG_DIR, "bench_loop.log")

    with open(meta_log, "a") as meta_f:
        log_msg(meta_f, f"\n{'=' * 60}")
        log_msg(meta_f, f"bench_loop started at {datetime.now():%Y-%m-%d %H:%M:%S}")
        log_msg(meta_f, f"  backend={BACKEND} timeout={TIMEOUT} window={WINDOW_SIZE} batch={BATCH_SIZE}")
        log_msg(meta_f, f"  db_dir={DB_DIR}")
        log_msg(meta_f, f"{'=' * 60}")

        log_msg(meta_f, "Building release binary...")
        result = subprocess.run(
            ["cargo", "build", "--release", "--bin", "bench"],
            cwd=project_root,
        )
        if result.returncode != 0:
            log_msg(meta_f, "Build failed!")
            sys.exit(1)
        log_msg(meta_f, "Build complete.\n")

        run = 1
        while True:
            timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
            cli_log = os.path.join(LOG_DIR, f"cli_{timestamp}.log")

            log_msg(meta_f, f"=== Run {run} starting at {datetime.now():%Y-%m-%d %H:%M:%S} ===")
            log_msg(meta_f, f"  batch log: {os.path.join(DB_DIR, 'bench-log.jsonl')}")
            log_msg(meta_f, f"  CLI log:  {cli_log}")

            cmd = [
                bench_bin,
                "-b", BACKEND,
                "-t", str(TIMEOUT),
                "-w", str(WINDOW_SIZE),
                "-c", str(BATCH_SIZE),
                DB_DIR,
            ]

            with open(cli_log, "w") as log_f:
                proc = subprocess.Popen(
                    cmd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                )

                # Poll peak RSS in background
                peak_rss = [None]
                stop_event = threading.Event()
                rss_thread = threading.Thread(
                    target=poll_peak_rss,
                    args=(proc.pid, peak_rss, stop_event),
                    daemon=True,
                )
                rss_thread.start()

                # Stream output to both terminal and log file
                for line in proc.stdout:
                    sys.stdout.write(line)
                    log_f.write(line)

                proc.wait()
                stop_event.set()
                rss_thread.join(timeout=2.0)

            status = "OK" if proc.returncode == 0 else f"FAILED (exit {proc.returncode})"
            rss_str = format_rss(peak_rss[0])
            log_msg(meta_f, f"=== Run {run} finished at {datetime.now():%Y-%m-%d %H:%M:%S} — {status} — peak RSS: {rss_str} ===")
            log_msg(meta_f, "")

            run += 1


if __name__ == "__main__":
    main()
