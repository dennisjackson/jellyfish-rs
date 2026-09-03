# `bigdb` benchmark baseline

`bigdb` was a 124 GB RocksDB database built by `src/bin/bench.rs` over roughly two
calendar days and 46.6 hours of process runtime, and used as the reference workload for
the analysis in [REVIEW.md](REVIEW.md). **The database itself has been deleted to
reclaim disk.** This document is the record it left behind.

Everything here was extracted from the database and its logs before deletion. Each
figure carries the command that produced it, so the same measurement can be re-derived
against a future database. Where a number could not be established exactly, that is
stated rather than smoothed over — see the *Gaps* section at the end.

## Preserved alongside this document

`bench_logs/bigdb-archive/` (gitignored, on-disk only — **not** in the repository):

| Archive | Contents | Size |
|---|---|---|
| `bigdb-LOGs.tar.zst` | all 575 RocksDB `LOG` files | 166 MB (3.34 GB raw, 20x) |
| `bigdb-metadata.tar.zst` | `MANIFEST-1319646`, both `OPTIONS-*`, `CURRENT`, `IDENTITY` | 310 KB |

The LOGs are the richer record: they answer questions nobody thought to ask when this
summary was written. The MANIFEST fixes the exact file-to-level assignment and key
ranges, so the LSM shape stays reconstructible.

## Three warnings before comparing anything to these numbers

1. **`bigdb`'s storage configuration is not the current code's.** Diffing the last
   `bigdb` open against a database created by the current tree gives exactly two
   differences: `bigdb` ran a 1 GiB **row** cache with RocksDB's silently-installed
   32 MiB default block cache, whereas the current code runs a 1 GiB **block** cache and
   no row cache (commit `64c5afc`). Every other option is byte-identical. A throughput
   comparison that ignores this is comparing cache configurations, not algorithms.

2. **RocksDB's "Cumulative" counters are per-`DB::Open()`, not per-database.** They reset
   on every open, and this database was opened 574 times. No single line in any LOG
   states a lifetime total. The lifetime figures below were reconstructed from the
   `EVENT_LOG_v1` stream and manifest sequence numbers; the per-session figures quoted
   in REVIEW.md (357.7 GB ingest against 2902 GB of compaction writes, 8.1x write
   amplification, 49.6% stall) are **one session**, not the database's life.

3. **The gap in warning 1 has widened since it was written**, and part of it is on disk.
   `perf/rocksdb-improvements` additionally runs `enable_pipelined_write`,
   `max_open_files = 4096` instead of `-1`, and glibc allocator tuning, and it has changed
   the node encoding three times: a 33-byte compact leaf record, an interior record
   carrying its two child hashes (99 → 163 bytes, and **not** readable by a `bigdb`-era
   build or vice versa), and a 65-byte frontier record. The per-record byte counts in the
   on-disk-composition chapter below describe `bigdb` and no longer describe the code.
   The replacement reference database, and what may and may not be compared against
   `bigdb`, are in §5.2.

---


# How to rebuild an equivalent database, and what to measure next time

Everything below was read out of `bigdb/LOG*`, `bench_logs/`, and the git history of
this repo. Commands are given so each figure can be re-derived on a future database.
All paths are relative to the repo root (`/workspaces/project`).

---

## 1. The exact command that built `bigdb`

### 1.1 Build

```sh
cargo build --release --features bins --bin bench
```

`tools/bench_loop.py` runs exactly this before each loop session. `Cargo.toml` sets
`[profile.release] debug = true`, so the binary carries full debug info (this is what
made the panic line numbers in the CLI logs usable).

**A rebuild today additionally needs `.cargo/config.toml`** (commit `1ba1bd7`, added
*after* `bigdb` was built). It sets `CXXFLAGS = "-DROCKSDB_SCHED_GETCPU_PRESENT"` to
dodge a clang miscompile of RocksDB's `PhysicalCoreID()` that segfaults under
concurrent memtable writes. Without it, `cargo test --release` and concurrent inserts
crash. Note the file's own caveat: cargo ignores `[env]` if `CXXFLAGS` is already set
in the environment.

### 1.2 The literal insert command

One process per run, driven in a `while True:` loop by `tools/bench_loop.py`:

```sh
target/release/bench \
    -b rocks \
    -t 7200 \
    -w 100000 \
    -c 10000 \
    -l bench_logs/log_$(date +%Y%m%d_%H%M%S).csv \
    bigdb
```

| Flag | Value | Meaning (`src/bin/bench.rs` arg parsing) |
|---|---|---|
| `-b, --backend` | `rocks` | `RocksTransRelMPT::new_with_path("bigdb")` |
| `-t, --timeout` | see below | run duration in seconds; the loop is deadline-driven, not count-driven |
| `-w, --window-size` | `100000` | entries generated *and sorted* per window |
| `-c, --batch-size` | `10000` | entries per `batch_upsert` call ⇒ **10 batches per window** |
| `-l, --log-file` | `bench_logs/log_<ts>.csv` | one `timestamp,total_inserted` row per batch |
| positional | `bigdb` | database directory, reopened by every run |

`bench_loop.py` also polls `/proc/<pid>/status:VmHWM` once a second and writes the peak
RSS into `bench_logs/bench_loop.log`. Nothing sets `RAYON_NUM_THREADS`, so rayon used
the machine default (64 threads).

### 1.3 Timeout was not constant

The `TIMEOUT` constant in `bench_loop.py` was edited between loop sessions. From the 18
session headers in `bench_logs/bench_loop.log`:

| `timeout=` | sessions | `window`/`batch` |
|---|---|---|
| 600 | 5 | `100000`/`10000` (one session used `2000000`/`100000`) |
| 1800 | 6 | `100000`/`10000` |
| 3600 | 5 | `100000`/`10000` |
| 7200 | 2 | `100000`/`10000` |

The final, longest-lived configuration — and the one committed in `tools/bench_loop.py`
today — is `timeout=7200 window=100000 batch=10000`. **Use 7200 (or longer) from the
start**; §5 explains why the short timeouts cost about 26 hours.

```sh
grep -o "timeout=[0-9]*" bench_logs/bench_loop.log | sort | uniq -c
grep -c "^bench_loop started" bench_logs/bench_loop.log      # 18 sessions
```

### 1.4 Wall time and volume invested

| Quantity | Value | Source |
|---|---|---|
| First run started | 2026-02-26 21:03:07 | `bench_logs/bench_loop.log` |
| Last run started | 2026-02-28 20:43:37 | same |
| Elapsed wall clock | **47.68 h** | difference of the two |
| Actual insertion time (Σ per-run CSV first→last row) | **44.08 h** | 330 `log_*.csv` |
| Entries inserted (Σ of each run's final `total_inserted`) | **978,620,000** | same |
| Process launches | **575** | 575 `LOG`/`LOG.old.*` files, and 575 `Run N starting` lines |
| Runs that exited 0 | **35** | `bench_loop.log` |
| Runs that panicked (exit 101) | **282** | same |
| Runs that exited 1 | **240** | same |
| Runs killed mid-flight (one per session) | 18 | 575 starts − 557 finish lines |
| Peak RSS across finished runs | 5.98 GB … **42.21 GB** | `peak RSS:` fields |
| Final on-disk size | **132,133,808,588 B** (123.1 GiB; `du -sh` says 124 G) | `du -sb bigdb` |
| SST files on disk / live | 2321 / **2282 (118.03 GiB)** | `ls bigdb/*.sst`, LOG `Sum` row |

```sh
# entries inserted, run accounting, insertion time
python3 - <<'PY'
import glob, re, datetime
runs=[]
for f in sorted(glob.glob('bench_logs/log_*.csv')):
    r=[]
    with open(f) as fh:
        fh.readline()
        for line in fh:
            a,b=line.strip().split(','); r.append((float(a),int(b)))
    if r: runs.append((r[0][0], f, r[-1][0], r[-1][1]))
runs.sort()
print("runs with data:", len(runs))
print("total inserted:", sum(r[3] for r in runs))
print("insert hours:  %.2f" % (sum(r[2]-r[0] for r in runs)/3600))
PY

grep -c "^=== Run .* starting" bench_logs/bench_loop.log
grep -o "— \(OK\|FAILED ([^)]*)\) —" bench_logs/bench_loop.log | sort | uniq -c
ls bigdb/*.sst | wc -l ; du -sb bigdb ; grep -a "^ Sum" bigdb/LOG | tail -1
```

### 1.5 Why 522 of 575 runs died — fix this before rebuilding

Every failure has one cause: **`EMFILE` (“Too many open files”)**.

```
thread '<unnamed>' panicked at src/mpt/rocks_frontier/mod.rs:709:14:
Failed to commit leaf-merge batch: Db(Error { message: "IO error: While open a file
for random read: bigdb/123631.sst: Too many open files" })
```

`RocksStorage::open` never calls `set_max_open_files` (the line is commented out at
`src/mpt/storage/rocks.rs:217` and was equally commented out in February), so
`Options.max_open_files: -1` — RocksDB keeps **every** SST in the table cache. The
first EMFILE crash was at 2026-02-27 01:50:43, with 224.16 M entries inserted and
**512 live SST files / 27.67 GiB** — consistent with a 1024 fd soft limit. (This
container currently reports `ulimit -n` 1048576; the limit in force in February cannot
be recovered from the logs, but 512 SSTs plus WAL/LOG/rayon fds landing on EMFILE
points squarely at 1024.)

Both fixes are one line each; do at least one:

```sh
ulimit -n 1048576          # before launching bench_loop.py
# and/or, in RocksStorage::open:
# options.set_max_open_files(4096);
```

Two other one-off failures: `IO error: While lock file: bigdb/LOCK: Resource
temporarily unavailable` (two overlapping `bench_loop.py` instances) and one
`Error: Too many open files (os error 24)` at open.

```sh
grep -h "panicked at" bench_logs/cli_*.log | sed 's/.*panicked at //' | sort | uniq -c
grep -h -A1 "panicked at src/mpt/rocks_frontier/mod.rs:709" bench_logs/cli_*.log \
  | grep -v "panicked at\|^--" | sed 's/[0-9a-f]\{6,\}/HEX/g' | sort | uniq -c | head
```

Note also that `bench_loop.py` names its logs `log_%Y%m%d_%H%M%S.csv`; runs that die
inside one second **overwrite each other's logs**. That is why there are only 338
`cli_*.log` for 575 runs. Add the PID or a monotonic counter to the filename.

---

## 2. Workload character

From `generate_entries` and the main loop in `src/bin/bench.rs`:

```rust
fn generate_entries(count: usize) -> Vec<(Hash, Hash)> {
    let mut rng = fastrand::Rng::new();
    // per entry: key = 32 random bytes, value = 32 random bytes
}
// in the loop:
let mut entries = generate_entries(window_size);   // 100_000
entries.sort_unstable_by_key(|a| a.0);             // sorted by key, whole window
for chunk in entries.chunks(batch_size) {          // 10 chunks of 10_000
    tree.batch_upsert(chunk);
}
```

- **Keys**: 32 uniformly random bytes. `impl From<Hash> for Prefix` uses the key
  *directly* as the 256-bit trie path (`src/prefix/mod.rs:86`) — the key is **not**
  re-hashed. So the trie is a perfectly balanced random binary Patricia trie, matching
  the “keys are SHA-256 outputs” model REVIEW.md assumes.
- **Values**: 32 uniformly random bytes, stored verbatim alongside the leaf's
  SHA-256 (`LeafNode::new` hashes `"leaf" ‖ key ‖ value`). Both halves of the record
  are incompressible — this is the root cause of the “Snappy is rejected on leaf
  blocks” finding in REVIEW 3.3.6.
- **Keys never repeat.** A new `fastrand::Rng` is seeded per window from the
  thread-local generator; nothing coordinates across windows. Expected duplicate pairs
  among 9.79 × 10⁸ draws from a 2²⁵⁶ space is ~10⁻⁶⁰. `sorted_unique_entries`
  (`src/mpt/mod.rs:207`) dedupes *within* a batch, but has nothing to do. **Every
  insert therefore creates a new leaf**: leaf count ≈ entries inserted, and this
  workload never exercises the update-in-place path at all.
- **Seeding is non-deterministic**, so a rebuild produces different keys and a
  different root hash. `bigdb`'s root hash after the run ending 2026-02-28 03:14 was
  `8f24cd6eceaf6df165e5a83471486591804b295a144eb2d2a9539210d5ccb068`; that value is
  not reproducible and is not a correctness oracle for a future build.
- **The batch is sorted before insertion, and this materially flatters the
  benchmark.** The sort covers the whole 100 000-entry *window*, then the window is
  cut into 10 chunks of 10 000. Chunk *i* is therefore order statistics
  `[10000i, 10000(i+1))` of 100 000 uniform draws, i.e. its keys occupy roughly the
  *i*-th tenth of the 256-bit key space. Consequences for frontier-subtree locality:
  - A batch touches ~9 940 distinct frontier subtrees (10 000 keys spread over
    2²³/10 ≈ 839 k frontier nodes; ~60 expected collisions), but they are drawn from a
    contiguous **10 % slice** of the key space, not the whole of it.
  - Leaf keys are encoded `length_be_u16(256) ‖ hash`, so leaf order = key order, and
    the ~2 260 leaf-only SSTs are range-partitioned by key. A batch therefore reads
    from ~226 of them, and its range scans walk forward monotonically.
  - Successive batches sweep the key space bottom-to-top, then jump back — a
    sawtooth with period 10 batches, not uniform random write traffic.
  - `batch_upsert` calls `sorted_unique_entries` anyway, so the pre-sort is **not**
    needed for correctness. Removing it (or shuffling the window) is the honest
    variant, and would be a valuable A/B: it should raise per-batch file fan-out ~10×
    with no change to the algorithm.

---

## 3. The machine

Observed from inside the dev container, which is bind-mounted from the host that owns
`bigdb`:

| Property | Value | Command |
|---|---|---|
| CPU | AMD Ryzen Threadripper PRO 3975WX, 32 cores | `grep -m1 "model name" /proc/cpuinfo` |
| Logical CPUs | 64 | `nproc` |
| RAM | 62 GiB total | `free -h` |
| Swap | 2.0 GiB | `free -h` |
| Filesystem | ext4 on `/dev/nvme0n1p5`, 569 G partition | `df -h /workspaces/project; mount \| grep workspaces` |
| Device | **WDC PC SN730 SDBQNTY-1T00-1001** NVMe SSD, `rotational=0` | `cat /sys/block/nvme0n1/device/model`, `.../queue/rotational` |
| Container memory cgroup | `max` (unlimited) | `cat /sys/fs/cgroup/memory.max` |
| Toolchain now | rustc 1.98.0 (88d9e12ae 2026-08-18), edition 2024 | `rustc --version` |

Relevant sizing note: **peak RSS reached 42.21 GB against 62 GiB of RAM.** The box had
headroom but not much; a machine with less than ~48 GiB cannot run this configuration
to 10⁹ leaves without swapping. The partition is currently 100 % full with 4.8 G
available, which is why `bigdb` is being deleted.

**Could not determine:** the rustc/LLVM version and the `ulimit -n` in force in
February 2026; whether the host was otherwise loaded during the 47 h; the NVMe's QD1
random-read latency curve (REVIEW 3.3.5 explicitly wants an `fio` 4/8/16 KiB QD1 curve
for the target device and it was never taken). I also cannot *prove* the February host
is this same host, only that the database and logs live on this filesystem and
REVIEW.md attributes its Snappy microbenchmark to “the same Threadripper PRO 3975WX”.

---

## 4. Tree configuration as built

`bigdb` was built by whatever was checked out at each session start; `bench_loop.py`
rebuilds the binary every session. The commits in the window are `4157dc4`
(2026-02-26 18:22, “Fix deadlock”) → `005a773` (2026-02-26 21:35, “Add loop
benchmark”) → `61cd5b7` (22:06) → `ef8d4e1` (2026-02-27 12:20) → `05a34a8`
(2026-02-28 20:49, “Tweaks”, committed 6 minutes after the last run started but
already present in the working tree). `RocksTransRelConfig::default()` is identical
across all of them.

### 4.1 `RocksTransRelConfig::default()` — as built (`git show 005a773:src/mpt/rocks_frontier/mod.rs`)

| Field | Value as built | Value at HEAD | Meaning |
|---|---|---|---|
| `keep_below_frontier` | 2 | 2 | levels retained below the frontier by the prune |
| `log_leaves_per_frontier` | 1 | 1 | how close the frontier may get to the leaves |
| `max_frontier_depth` | 24 | 24 | `check_depth_complete` returns false at `depth >= 24`, so the frontier caps at **23** |
| `depth_always_keep` | 23 | 23 | nothing at or above this depth is ever evicted |
| **`depth_to_write`** | **3** | **1** | levels persisted from the frontier down |

`depth_to_write = 3` is what `bigdb` was written with, and it made **no difference**:
commit `7b0b08e` (2026-08-29, “Set depth_to_write to 1 and document that larger values
are inert”) changed the default to 1 as a pure documentation change, because
`batch_upsert_at_interior` only adds an interior to a write batch on the arm that
*opens* the boundary batch, which requires `active_batch.is_none()`; with a complete
level at F every root-to-leaf path crosses exactly one node at depth F. Pinned by
`test_depth_to_write_above_one_is_inert`. **A rebuild at HEAD with `depth_to_write = 1`
produces the same on-disk structure as `bigdb`.**

Derived: `eviction_depth() = max(F + keep_below_frontier, depth_always_keep)` = 23
while F ≤ 21, and 25 once F = 23 — the depths 24/25 accumulation that produced the
39–42 GB RSS.

### 4.2 Frontier depth reached: **23** (the configured maximum)

```
[2026-02-28T01:13:07Z INFO jellyfish_rs::mpt::rocks_frontier] Advancing frontier depth to 23 with nodes 8388608
[2026-02-28T20:43:52Z INFO jellyfish_rs::mpt::rocks_frontier] Loaded 8388608 interior nodes at depth 23 from storage
[2026-02-28T20:44:23Z INFO jellyfish_rs::mpt::rocks_frontier] Initialized RocksTransRelMPT with root empty, complete depth 23, approx entries 999502643, leaves per frontier 59
```

Two caveats on that last line, both from February-era code:

- `approx entries 999502643` is **not** an exact leaf count. In February
  `len()` called `estimate_leaf_count()`, which samples ≤100 frontier subtrees and
  extrapolates; the exact counter landed later (`bcc5efe`, 2026-08-29). The
  CSV-derived figure of **978,620,000** inserted entries is the number to trust; the
  2.1 % gap is sampler error plus a handful of runs whose CSV was overwritten.
- `leaves per frontier 59` is **off by one level**. February computed
  `estimate / (2 << depth)` = estimate / 2²⁴; fixed in `836f346`. The real figure is
  999 502 643 / 2²³ = **119.2 leaves per frontier node**, or 978 620 000 / 2²³ =
  **116.7** using the trustworthy count.

Open cost at that size: 15 s to load the 8.39 M depth-23 interiors, 46 s to a usable
tree; the two long runs report `Init time: 48.205 s` and `50.798 s`.

### 4.3 RocksDB options as built — **they changed mid-build**

```sh
cd bigdb && for f in LOG LOG.old.*; do
  printf 'jobs=%s subc=%s\n' \
    "$(head -250 "$f" | grep -m1 'Options.max_background_jobs:' | awk '{print $NF}')" \
    "$(head -250 "$f" | grep -m1 'Options.max_subcompactions:'  | awk '{print $NF}')"
done | sort | uniq -c
```

| Period | opens | `max_background_jobs` | `max_subcompactions` | entries inserted in period |
|---|---|---|---|---|
| 2026-02-26 21:03 → 2026-02-27 12:51 | 549 | 32 (via `increase_parallelism(32)`) | 1 | 0 → 397,190,000 |
| 2026-02-27 12:51 → 2026-02-28 20:43 | 25 | 8 | 4 | 397,190,000 → 978,620,000 |

Constant throughout: `max_open_files: -1`, `write_buffer_size: 67108864`,
`max_write_buffer_number: 2`, `compression: Snappy`, `target_file_size_base:
67108864`, `max_bytes_for_level_base: 268435456`, `row_cache: 1073741824`.

**The 1 GiB cache was a `row_cache`, not a block cache, for the whole build.** February
code: `options.set_row_cache(&cache)`. Since every hot read in this backend is an
iterator and RocksDB consults the row cache only from `TableCache::Get`/`MultiGet`, that
gigabyte was never read from; `BlockBasedTableFactory` silently installed its own
32 MiB default block cache instead. HEAD moves the gigabyte to the block cache
(`block_options.set_block_cache(&cache)`), drops `set_row_cache` and
`increase_parallelism(32)`, and keeps `set_max_background_jobs(8)` /
`set_max_subcompactions(4)`. **A rebuild at HEAD is therefore not option-identical to
`bigdb`** — it has 32× the effective read cache. Record which you used.

```sh
sed -n '1,200p' bigdb/LOG                      # full Options dump at the last open
git show 005a773:src/mpt/storage/rocks.rs | sed -n '122,136p'   # options as built
```

---

## 5. Hitting the same regime more cheaply

`update_complete_interior_depth` advances F → F+1 only if **both** gates pass.

**Gate A — leaf-count ("don't get close to the true frontier").**
```rust
if current_depth > log_leaf_nodes.saturating_sub(self.config.log_leaves_per_frontier) { break; }
// log_leaf_nodes = ceil(log2(leaf_count))
```
With `log_leaves_per_frontier = 1`, advancing from F requires `ceil(log₂N) ≥ F + 1`,
i.e. **N > 2^F**. At the depths that matter this is never the binding constraint.

**Gate B — `check_depth_complete(F+1)`**: an `Interior` node must exist **in the
in-memory store** at every one of the 2^(F+1) prefixes of length F+1, and
`F + 1 < max_frontier_depth`. This splits into two independent requirements:

- **B1 (statistical).** The tree is Patricia-compressed — a single leaf is stored at
  its full 256-bit prefix (`batch_insert_into_empty` uses `Prefix::from(first_key)`),
  so a node exists at length-D prefix *p* only if *p* is a genuine branch point, i.e.
  both `p‖0` and `p‖1` are non-empty. Depth D is complete ⟺ **all 2^(D+1) buckets at
  depth D+1 are non-empty** — plain coupon collector:
  **N ≳ 2^(D+1) · (D+1) · ln 2**.
- **B2 (operational, and the expensive one).** A fresh open loads *only* depths
  `0..=complete_depth`. Every depth-(F+1) node must therefore be re-materialised
  **inside the current process**, which happens when a batch touches its ancestor
  frontier node. So a single uninterrupted process must issue
  **R ≳ 2^F · F · ln 2** inserts since its last open.

| D | Gate A: N > 2^(D−1) | B1: N ≳ 2^(D+1)(D+1)ln2 | B2: inserts in one process ≳ 2^(D−1)(D−1)ln2 | **Observed N at advance** | obs / B1 |
|---:|---:|---:|---:|---:|---:|
| 15 | 16,384 | 726,817 | 158,991 | 650,000 – 880,000 | 1.21 |
| 16 | 32,768 | 1,544,487 | 340,696 | 1,750,000 – 1,860,000 | 1.20 |
| 17 | 65,536 | 3,270,679 | 726,817 | 4,180,000 – 4,230,000 | 1.29 |
| 18 | 131,072 | 6,904,766 | 1,544,487 | 6,070,000 – 6,100,000 | 0.88 |
| 19 | 262,144 | 14,536,350 | 3,270,679 | 13,440,000 | 0.92 |
| 20 | 524,288 | 30,526,335 | 6,904,766 | 34,260,000 | 1.12 |
| 21 | 1,048,576 | 63,959,940 | 14,536,350 | 128,380,000 | 2.01 |
| 22 | 2,097,152 | 133,734,420 | **30,526,335** | 600,180,000 | 4.49 |
| 23 | 4,194,304 | 279,097,919 | 63,959,940 | 600,180,000 | 2.15 |

*(Depths 1–14 all advanced inside the first logged second, bracketed at 20 000–360 000
inserts; the CLI log has 1 s resolution so they cannot be separated. Observed values
are bracketed by the last CSV row before the log second and the last row before the
next second; above D = 18 the two agree exactly.)*

**The model matches within ~1.3× up to D = 20, and then blows out — and B2 is why.**
The frontier reached 21 at 128.4 M entries on 2026-02-26 23:38 and stayed there for
**26 hours and 274 runs**. Those runs were 600–3600 s each and the largest inserted
28.6 M entries; B2 at F = 21 demands ≈ 30.5 M *in one process*, so none of them could
finish materialising all 2²² depth-22 nodes, and each restart threw the partial work
away. The 7200 s run of 2026-02-27 23:13 finally got there:

```
[2026-02-28T01:12:54Z] Advancing frontier depth to 22 with nodes 4194304   # 28.6 M inserts into this run
[2026-02-28T01:13:07Z] Advancing frontier depth to 23 with nodes 8388608   # 13 s later
```

Depth 23 followed 13 seconds later because `depth_always_keep = 23` means depths 22
*and* 23 both survive the prune, so both levels had been filling all along.

### 5.1 Recipe for a cheap equivalent

**Reaching frontier depth 23 needs ~300 M leaves, not 978 M — provided you do not
restart.** With one uninterrupted process, B1 at D = 23 (≈279 M) is the binding gate,
and B2 at every depth is automatically satisfied by the same process. Observed cost of
getting to that size, even on the slow path this database actually took:

| Milestone | Cumulative insert time | Live SSTs / size at that point |
|---|---|---|
| 100 M leaves | 1.18 h | — |
| 200 M leaves | 3.29 h | — |
| **300 M leaves** | **7.28 h** | **754 files / 36.70 GiB** |
| 350 M leaves | 10.26 h | 809 files / 45.94 GiB |
| 600 M leaves (F reached 23 here in practice) | 24.72 h | 1562 files / 75.18 GiB |
| 978.6 M leaves (final) | 44.08 h | 2282 files / 118.03 GiB |

So: **~7–10 h and ~37–46 GiB gets you a frontier-23 database**, against 47.7 h and
123 GiB actually spent — roughly a 5× saving in time and 3× in disk. Concretely:

1. `ulimit -n 1048576` (or set `max_open_files`) — non-negotiable past ~500 SSTs.
2. Set `TIMEOUT = 86400` (or drop the loop entirely and run one `bench -t 86400`).
   **Never restart the process while the frontier is still climbing.**
3. Add the PID to the log filenames so concurrent-second runs stop clobbering.
4. Stop at ~350 M entries if the target is the F = 23 regime; keep going only if the
   goal is the *post-cap* O(N)-read regime, which needs N ≫ 2²³ (see §6's table — the
   interesting decay happens between 600 M and 1 B).
5. If you want the frontier-cap regime cheaper still, drop `max_frontier_depth` (and
   `depth_always_keep`) by *k*: every gate above scales by 2⁻ᵏ, so
   `max_frontier_depth = 20, depth_always_keep = 19` reaches its cap at ~35 M leaves
   (≈5 GiB, well under an hour) with the same *shape* — same L growth, same stall
   dynamics, same eviction-depth accumulation, at 1/8 the memory. **This is the single
   highest-leverage change for making this benchmark iterable.**

```sh
# frontier advances and their timestamps
grep -h "Advancing frontier depth" bench_logs/cli_*.log | sort -u
# live file count / size at any past instant: find the LOG.old rotated just after it
grep -a "^ Sum" bigdb/LOG.old.<unix_micros> | tail -1
```

### 5.2 The recipe as actually executed — 40 M leaves in eight minutes

Done on 2026-08-29 on the machine described in §3, and it is what every number in
REVIEW.md §5 rests on. `bigdb` is gone; this is its replacement.

**Build.**

```sh
cargo build --release --features bins --bin bench
./target/release/bench -b rocks -n 40000000 -w 100000 -c 10000 -t 7200 \
    --seed 456968137849 --max-frontier-depth 20 -l bench_out/refbuild.csv refdb
```

| | |
|---|---|
| build time | **487.6 s (8 min 8 s)** at 82,027 entries/s |
| leaves | 40.0 M |
| frontier depth | **19** — its cap, `max_frontier_depth − 1` |
| **L = leaves per frontier node** | **76** — `bigdb`'s range was 75–113 (§6.3) |
| on disk | ~3 GB, 63 live SSTs (against 124 GB and 2282) |
| peak RSS | 4.85 GB |
| root hash | `338e51ea3b356fdef3ca87bd650369fbc18b6127a00086b64188b610063df1d9` |

So §5.1's item 5 was right and slightly pessimistic: it predicted "~35 M leaves, ≈5 GiB,
well under an hour" for this configuration, and it is 40 M leaves in eight minutes. The
frontier-cap regime — the property of `bigdb` that the design's weakness depends on — is
now reachable in a coffee break, which is what makes this benchmark iterable at all.

`--depth-always-keep 19` was passed alongside `--max-frontier-depth 20` in these builds and
is **no longer needed**: `eviction_depth` now caps `depth_always_keep` at
`max_frontier_depth − 1`. Leaving it at the default 23 under a frontier of 19 retains four
levels below the frontier for nothing, and measured +57 % peak RSS and +66 % batch p99 for
throughput that was if anything slightly worse (REVIEW.md §5.4).

**The A/B protocol: `tools/ab.py`.** Every comparison in REVIEW.md §5 is one invocation of

```sh
tools/ab.py --ref refdb --entries 2000000 --reps 4 --out bench_out/ab_x.json \
    'base=bench_out/bin/base' 'variant=bench_out/bin/variant'
```

and each part of it is there for a reason:

- **Fixed work, not fixed time** (`--entries`, passed to `bench --max-entries`). A
  fixed-time comparison *understates* a win, because the faster variant reaches a bigger
  tree inside the timeout and a bigger tree is slower per insert. The previous round had to
  correct for this by comparing "at matched tree size" off the insertion-rate series
  (§4 of the performance-history chapter); fixed work removes the confound instead of
  compensating for it.
- **A fresh copy of the reference database per run.** RocksDB state — LSM shape, L0 backlog
  — is part of what is being measured, so every run must start from byte-identical state.
- **The page cache dropped between runs**, via `posix_fadvise(POSIX_FADV_DONTNEED)` on every
  file in the copy, which needs no privileges. Without it the whole 3 GB sits in 62 GB of
  page cache after the copy and every read-path change measures as exactly zero.
- **Interleaved repetitions** (A,B,A,B,…, not AAA,BBB), so thermal drift or a background
  process cannot be attributed to whichever variant ran second. The reported figure is the
  median and the spread is printed beside it, because a wide spread is itself a finding.
- **The root hash as a correctness gate.** All variants insert the same keys into the same
  starting state, so they must agree on the final root; a variant that disagrees is broken
  however fast it is, and the script says so instead of reporting its throughput.
- **A per-variant reference database** (`label=binary@otherref`), for the format-breaking
  variants that cannot read each other's database. Both must have been built from the same
  seed, entry count and frontier configuration, in which case they hold the same logical
  tree and differ only in encoding — and any size difference the encoding costs is then
  correctly part of what is being compared.

**The noise floor is 1–5 % on a quiet machine, and "quiet" is not decoration.** Two runs of
the *same binary* against itself measured spreads of 10.4 % and 28.1 % before the box was
quiesced. Worse, the session's cumulative A/B had to be thrown away and re-run when it came
back with spreads of 31.3 % and 20.2 % — larger than most of the individual changes the
session landed, and enough to reverse a verdict. **Measurements taken while other work is
running on the machine are worthless.** Check the box is idle first, and treat any variant
whose spread exceeds ~6 % as unmeasured rather than as a weak result.

**What this reference database does *not* put under measurement.** Stated plainly, because
the temptation is to read every number as if it came from `bigdb`:

1. **Reads against a working set larger than RAM.** 3 GB against 62 GB is entirely
   resident, so a "read" is a page-cache hit. REVIEW.md §5.4 measured the consequence
   head-on: `keep_below_frontier = 3` cuts leaves read per insert from 22.8 to 20.3 and
   *loses* 4.2 % throughput. Structural counters (puts/insert, bytes staged/insert, leaves
   read/insert) travel to a larger database; wall-clock read wins do not. **A
   larger-than-RAM reference database is a prerequisite for any read-side conclusion**, and
   this one is not it. §5.1's milestone table puts 300 M leaves at 36.70 GiB and 600 M at
   75.18 GiB, so the crossing point for this machine is somewhere near 500 M leaves.
2. **Compaction pressure.** 63 SSTs and no stall, where `bigdb` spent 8.8–50.6 % of its
   wall clock stalled (§6.3). Nothing measured here can rank a write-path change by its
   effect on stall.
3. **New-leaf growth, in the steady-state A/B.** `bench`'s key stream is a function of
   `--seed` alone, and `tools/ab.py`'s default seed is `bench`'s default seed — the one the
   reference database was built with — so the 2 M entries replay the first 2 M pairs
   already in the tree. Leaf count stays at exactly 40.0 M in every run and the root hash is
   invariant end to end, which is what makes it such a strong oracle, but it means the
   steady-state A/B measures **upsert of existing keys**, not leaf creation. Splits, new-leaf
   persistence and frontier advance are exercised by the from-empty builds and by the test
   suite instead. Pass a different `--seed` to measure growth.
4. **Frontier depth 23.** F = 19 here, so anything whose size goes as 2^F — the reopen scan
   (REVIEW.md §5.6), the retained band at depths F+1…F+2 (REVIEW.md §3.1.5) — is projected
   arithmetically and labelled as such, never measured.

**Preserved, gitignored, on disk only** (like the `bigdb` archives above):
`bench_out/*.json` — every A/B run, with its config, per-run parsed output, medians,
spreads and root hashes; `bench_out/refbuild*.log` and `bench_out/refbuild*.csv` — the two
reference builds, one per on-disk format.

### 5.3 Round 4 artifacts (2026-08-30)

Everything REVIEW.md §6 rests on, same conventions:

- `bench_out/profile_round4_insert_64threads_2026-08-30.json.gz` — the samply profile of
  the re-profile round (insert workload, seed 1852727072, 2 M entries into a
  `refdb_childhash` copy, page cache dropped). Symbols resolve with `addr2line` against
  the binary that produced it (`bench_out/bin/r4_base`).
- `bench_out/ab_r4_steady.json` (9 variants × 4 reps), `ab_r4_growth.json`,
  `ab_r4_cumulative.json`, `ab_r4_build40m.json` — the round's A/B sessions.
- `bench_out/bin/r4_{base,p1,p2,p3,p4,p5,all}` — the measured binaries, plus `r4_start`
  (`ef0e480` + the cpuid fix, the section-4 baseline rebuilt) and
  `bench_out/hist_start_vs_tip.json`, the round-4 re-run of REVIEW.md §4's end-to-end
  comparison.
- `bench_out/r4_p3_rejected.patch` — the prefix-bloom variant, rejected with numbers
  (REVIEW.md §3.3.2/§6.2), preserved in case a larger-than-RAM database revives it.
  **A larger-than-RAM database did revive it, in round 19, and it was rejected again with
  the mechanism measured** — see §5.7 below and REVIEW.md §21. Do not revive it a third
  time without a reason that answers §21.3's `leaves read/insert` argument.
- `refdb_bloom/` — the reference database rebuilt by the bloom binary (same seed, same
  logical tree, root `338e51ea…`; its SSTs carry filters). It also demonstrated a
  protocol caveat: a *fresh* rebuild measures −8.8 % against the day-old
  `refdb_childhash` under the same bloom-off binary — LSM shape and compaction debt are
  state — so per-variant-reference comparisons must be judged on/off within the same
  database. **Deleted 2026-08-30 to reclaim disk**, like `bigdb` before it; rebuild in
  ~3 minutes with
  `bench_out/bin/r4_p3 -b rocks -n 40000000 -w 100000 -c 10000 -t 7200 --seed 456968137849 --max-frontier-depth 20 --depth-always-keep 19 refdb_bloom`
  (its `refbuild_bloom.csv` build record remains).

Two harness facts worth re-stating from §5.2 because round 4 leaned on both: `ab.py`'s
default seed makes the steady-state runs a genuine **insert** workload now, and
`refdb_childhash` is the reference a current binary can read. (`refdb`, the pre-child-hash
build that no current binary can decode, was deleted 2026-08-30 along with `refdb_bloom`
and assorted scratch databases; the `refbuild.csv`/`refbuild.log` records remain, and the
recipe at the top of §5.2 reproduces the logical tree in either format.)

### 5.4 `my_100m_db` — the Tier-0 database (2026-08-30)

The name undersells it: it holds **1.0009 B leaves**, not 100 M. Built at the round-4 tip
entirely at `RocksTransRelConfig::default()` / `RocksStorageConfig::default()` (its LOG
pins every option: `write_buffer_size=64MB`, `enable_pipelined_write=1`,
`manual_wal_flush=1`, `unordered_write=0`, `max_background_jobs=8`, `max_subcompactions=4`,
`max_open_files=4096`), across four chained processes in roughly 95 minutes
(07:36–09:11 local; compare §5.1's 44 h for `bigdb`'s 978 M — the round-4 code is that much
faster). `bench_out/mybuild.csv` records the last process: 202.3 M inserts, 798.8 M →
1,000.9 M leaves, ~140 K entries/s sustained.

| | |
|---|---|
| leaves | **1.0009 B** (`mybuild.csv` final row) |
| frontier depth | **23** — the default cap, same as `bigdb` |
| L = leaves per frontier node | **119** (1.0009 B / 2²³) — `bigdb` ended at 117 by the same arithmetic (978.6 M / 2²³) |
| on disk | 1,191 live SSTs / 68.45 GB (74 GB directory) |
| host RAM | 62.6 GiB — **the database no longer fits**, which is the point |

This is the "Tier 0" database REVIEW.md §6.4 said three verdicts were waiting on: the
read path at scale, block-cache sizing, and `unordered_write`. Those verdicts are in
REVIEW.md §7 (measured 2026-08-30). First at-scale facts, from the §7 baseline arm:
cold-cache steady state is **~95 K entries/s** with **57.1 leaves read/insert** (18.3 at
the 40 M scale) and puts/insert unchanged at 1.994 — the design's read amplification in
the post-cap regime, measured rather than extrapolated.

> **Deleted 2026-08-30** (owner-approved) to make room for round 7's page-format builds,
> like `bigdb` and `refdb` before it. Rebuild: ~95 minutes at the round-4+ tip, entirely at
> defaults — chain `bench -b rocks -n 1000900000 -t 28800 <db>` processes, or one
> uninterrupted run; §5.1's gates say one process reaches the depth-23 cap on the way. Round
> 5's measurements against it live in `bench_out/ab_t5_*.json`; round 6's profile in
> `bench_out/profile_round6_*.json.gz`. (`ref62_compact`, §8.2's one-run twin, was deleted
> the same day — rebuild in ~25 s with `compact-db` from `ref62_l119`.)
>
> **Rebuilt the same evening, deeper** — 1.81 B leaves at frontier depth 24; see §5.6.

A 74 GB reference cannot be copied per run, so `tools/ab.py` gained `--link-copy`:
SSTs are hardlinked (RocksDB never modifies one in place; compaction writes new files and
unlinks old ones), everything RocksDB appends to (MANIFEST, WAL, CURRENT, LOG, OPTIONS)
is really copied — ~102 MB per "copy" instead of 74 GB, byte-identical starting state
either way. Hardlinks share the page-cache inode, so the protocol's cache drop on the
scratch copy evicts the reference's pages too, which is what a cold-cache run wants.

### 5.5 A read-bound regime without the Tier-0 database (round 9)

The Tier-0 rebuild costs ~95 minutes and 68 GB; most read-side questions only need
*reads that miss*, and a page-cache squeeze produces those against `ref62_l119` in the
time it takes to fault in a hog. `tools/memhog.c` (build: `clang -O2 -o /tmp/memhog
tools/memhog.c`) maps N GiB anonymous, faults it in, keeps every page young with a slow
rolling re-touch so reclaim evicts file-backed cache first, and raises its own
`oom_score_adj` to 1000 so a pressure kill takes the hog, never the benchmark. No root
needed — the environment's `no_new_privileges` rules out the cgroup-limit variant
REVIEW.md item 1 proposed, and this replaces it.

Calibration on this 62.6 GiB machine, against a ~5 GB working set (`ref62_l119` scratch):

| hog | `free` available during run | regime | tip throughput |
|---:|---:|---|---:|
| none | ~52 GB | resident — reads are cache hits after warmup | ~233 K/s |
| 43 GiB | ~8–9 GB | ~90 % resident — the Tier-0 ratio (68 GB vs 62.6 GiB) | ~222 K/s |
| 46 GiB | ~5–6 GB | ~40 % resident — harsher than production ever sees | ~121–140 K/s |

Census and root hash are unchanged by the hog (only physical read cost moves), which is
both the validation and the point. Caveats: spreads at the 46 GiB point run 7–14 %, so
use interleaved reps and distrust anything under ~5 % there; re-check `free` after
starting the hog (other residents shift); and kill the hog for builds — 6 GB available
makes linking slow and an OOM kill of a compiler mid-build is a wasted round trip.
Judged at these three points in round 9 (REVIEW.md §11): thread oversubscription and
`ReadOptions::async_io` both win only at the 46 GiB point and lose at the two the
production system resembles.

### 5.6 `my_100m_db` rebuilt, deeper — 1.81 B leaves at frontier 24 (2026-08-30 evening)

> **Deleted 2026-08-31** (round 12, maintainer's instruction, disk at 100 %). Rebuild
> from the recipe below; REVIEW.md §14 records the deletion.

The Tier-0 database exists again, and it is not the old one: the evening's first session
**created the directory from scratch** (`SST files in my_100m_db dir, Total Num: 0`,
`Creating manifest 1` — `LOG.old.1788117404586866`; `IDENTITY` mtime 18:41:34 UTC) and
the build then overshot the old 1.0009 B operating point to **1,813,508,427 leaves at
frontier depth 24**. No command lines or stderr were captured, so the exact
`--max-frontier-depth` is unrecoverable — but the depth-24 layer holds exactly 2^24
persisted records (census below), which the code only writes on an advance *to* 24, so
the flag was **≥ 25**. Everything below is reconstructed from the RocksDB logs (the
per-session key counts are exact, from the `largest_seqno` chain across recovery
flushes), `bench_out/mybuild.csv`, and a read-only `count-depths` census run against a
hardlink scratch copy.

**Session ledger** (UTC; the bench host's log lines are internally UTC+1):

| # | role | open → last activity | wall (s) | exit | keys written (exact) | inserts/s (derived) |
|---|---|---|---:|---|---:|---:|
| S1 | build (creates DB) | 18:41:34 → 19:11:06 | 1,771 | clean | 939,147,200 | ~270,800 |
| S2 | build | 19:16:44 → 20:08:59 | 3,135 | clean | 993,099,006 | ~161,800 |
| S3 | build | 20:09:34 → 20:30:24 | 1,250 | killed | 292,271,044 | ~119,400 |
| S4 | build | 20:30:39 → 21:54:03 | 5,004 | clean | 998,663,813 | ~101,900 |
| S5 | build | 21:55:33 → 22:17:03 | 1,290 | killed | 228,654,036 | ~90,500 |
| C | `compact-db` | 22:19:02 → 22:35:48 | 1,006 | clean | 0 | — |
| S6 | post-compaction bench | 22:41:30 → 22:49:26 | 475 | killed | 98.2 M flushed (+~0.5 M in the WAL) | **116,269 (measured)** |

- **Build totals**: S1–S5 = 12,450 s (207.5 min) for 1,762,780,000 leaves —
  **141,583 inserts/s average from empty**, on the round-10 tip at otherwise-default
  configuration. Total keys flushed across the evening: **3,550,003,355**
  (final `largest_seqno`), i.e. **1.9575 puts/insert** against the census leaf count —
  inside the recorded 1.911–1.994 census range, which is also the arithmetic that proves
  this was a rebuild, not an extension of the old 1.0009 B database (an extension would
  imply an impossible 4.37 puts/insert).
- **The depth-24 advance** landed mid-S2 at 19:45:47–19:46:04 UTC (~804 M leaves): 2^24
  frontier records ≈ 1.02 GiB of 65-byte values persisted in ~17 s, which outran the
  flush pipeline and caused the only material stall of the night (`Stopping writes: 2
  immutable memtables`, 0.374 s; S1 logged a separate 0.020 s). Total stall across
  12,926 bench-seconds: **0.394 s = 0.003 %** — compare `bigdb`'s 49.6 %.
- **Compaction**: opened in 0.7 s, `compacted in 1005.2 s`; 2,167 files / 124.6 GiB
  (L0–L6) → **2,107 files / 123.75 GiB, all L6**; 112 jobs, 155.3 GB written
  (154.5 MB/s). Write volume for the whole evening: 2,884 GB of table files written
  against ~305 GB (≈ 284 GiB) of user ingest — **≈ 9.5× amplification** by ingest, or
  10.5× against the 275 GB of flushed memtables, both exact event sums (lifetime `bigdb`
  was 11.0 by the flush measure).
- **Post-compaction steady state (S6)**: 50.18 M inserts in 431.6 s = **116,269
  inserts/s** against a fully-compacted 123.75 GiB database on a 62.6 GiB host
  (DB ≈ 2× RAM), `complete_depth = 24` throughout, init 37.7 s (loading the 2^24-node
  frontier). Per-minute curve: 118 → 128 → 119 → 118 → 111 → 112 → 110 → 109 K/s — a
  slow decay as its own writes rebuild an L0 stack, no cold-cache dip beyond init.
  Versus S5 on the same database pre-compaction (~90.5 K/s): **~+28 % from the
  idle-window compaction**, the round-6/round-10 lever reproducing at twice the old
  Tier-0 scale.
- **Depth census** (`count-depths`, read-only, 2026-08-31 against a hardlink scratch
  copy since removed — census output not archived, rerun to reproduce): every interior
  depth 0–24 holds exactly 2^d records — 2^25 − 1 = 33,554,431 interiors, of which the
  16,777,215 at depths 0–23 are parked former-frontier rows (~1.5 GiB raw; the
  never-deleted bands REVIEW.md §11.5 costed and closed). Leaves: 1,813,508,427 at
  33.0 B average value. Raw K+V 116.3 GiB.
- **Directory now**: 140.7 GB (`du`), 99.9 % of it 2,296 SSTs; live LSM
  `[8, 0, 0, 8, 28, 114, 2107]` = 2,265 files (the S6 kill preempted obsolete-file
  purging); MANIFEST-054304 current; one 43 MB WAL that will replay ~0.26 M inserts on
  the next open.
- **Operating point vs the old Tier-0**: L = 1,813,508,427 / 2^24 = **108** — *below*
  the old 119, because doubling the frontier more than covers the 1.81× leaf growth; at
  depth 23 this tree would sit at L = 216. The "larger than RAM" property is now 2×
  overshot rather than 1.09×.

Caveats, stated plainly: per-session insert rates assume the global 1.9575 puts/insert
(only end-of-S5 and the census are exact); S6's totals exclude the unflushed WAL tail;
the exact `--max-frontier-depth` and all `Advancing frontier depth` lines went to
uncaptured stderr — item 6.2.6 (a run manifest) would have prevented exactly this; and
an unexplained 10× drop in mean RocksDB write-batch size at the S3→S4 boundary
(2,853–3,057 keys/write → 306–307) is visible in the stats dumps but attributable to no
captured artifact — most plausibly a changed `-c/--batch-size` between processes.

---

### 5.7 Round-19 databases — the prefix-seek arms, and a Tier 0 rebuilt at defaults (2026-09-01)

Three databases, all built by a binary carrying a prefix extractor and bloom filters, so
their SSTs actually hold filter blocks (round 4's caveat: filters exist only in files
*written* with the extractor configured). All retained on disk; **disk is at 93 %, and these
are 82 GB of it.** REVIEW.md §21 has the measurements.

| | leaves | F | keep | extractor | build | size / SSTs | root |
|---|---:|---:|---:|---|---:|---|---|
| `ref62_bloom` | 62.4 M | 19 | 2 | 16-bit | 195.9 s @ 318,560/s | 4.7 GB / 85 | `f9b38701…` |
| `ref62_k3` | 62.4 M | 19 | 5 | 24-bit | 169.7 s @ 367,686/s | 4.7 GB / 83 | `f9b38701…` |
| `tier0_k3` | 1.0009 B | 23 | 2 | 24-bit | 98 min @ 170,115/s | 73 GB / 1232 | `1fea1270…` |

Rebuild recipes (defaults except as shown):

```sh
./target/release/bench -b rocks -n 62400000 -w 100000 -c 10000 --max-frontier-depth 20 \
    --seed 456968137849 -t 7200 --extractor-hash-bytes 2 ref62_bloom          # ~3.5 min
./target/release/bench -b rocks -n 1000900000 -w 100000 -c 10000 -t 28800 \
    --extractor-hash-bytes 3 tier0_k3                                          # ~98 min
```

…but note `--extractor-hash-bytes` **no longer exists**, nor does `--scan-perf-counters`:
both went back into `bench_out/r19_prefix_seek_rejected.patch` (base `f07c6d0`) with the rest
of the idea and its instrument (REVIEW.md §21.4). Drop the flags to rebuild the same
*logical* trees without filters. `--keep-below-frontier` does survive, for item 2 below.

Three things these are worth keeping for:

1. **`tier0_k3` reproduces §5.4's operating point at defaults** — 1.00 B leaves, F = 23,
   L = 119, and in a 2 M-insert steady-state run it reproduces §7.1's recorded census
   exactly: **57.1 leaves read/insert, 1.994 puts/insert, ~90 K entries/s cold**. It is the
   Tier-0 database §5.4 said is a prerequisite for any read-side conclusion, present again.
2. **The two 62.4 M arms are a `keep_below_frontier` pair** at the same scale and tip:
   keep = 5 against keep = 2 is **+15.4 %** from empty with leaves read/insert **8.1 → 1.2**
   for peak RSS **2.64 → 9.80 GB**. Not isolated (the extractor differs) and from-empty
   rather than steady state, but §5.4 swept that knob only at F = 19 on a resident database.
   Re-sweeping it against `tier0_k3` is the open question round 19 surfaced.
3. **`ref62_bloom` and `ref62_k3` both carry root `f9b38701…`**, the same root §8.2 records
   for the bloom-less `ref62_l119`. Same seed, same entry count, same logical tree — which
   makes them drop-in cross-checks that a read-path change has not altered the tree.

`tier0_k3` also served as the reference for the tree-top unification (REVIEW.md §21.8):
`ab_r19_levels_tier0.json`, `ab_r19_levels_tier0_reps9.json`, `ab_r19_levels_thp_tier0.json`,
`ab_r19_levels32_tier0.json`, `ab_r19_levels_split_tier0.json`, `ab_r19_levels_split32_tier0.json`,
`ab_r19_levels_tier0_20m.json`, `ab_r19_depth_tier0.json`, `ab_r19_depth_open_tier0.json`, binaries
`bin/r19_levels`, `bin/r19_levels_thp`, `bin/r19_levels32`, `bin/r19_levels_split`,
`bin/r19_levels_split32`, `bin/r19_depth` and `bin/r19_depth_serial`, all against
`bin/r19_{nocensus,dense}`.
Two protocol notes from those runs. The first rep of a three-arm session read 10–15 % slower
for *every* arm than reps 1–2, which is a cold-pass artifact and not a variant; do not read a
one-rep result from this database. And the open time is part of the protocol whether or not
it is meant to be: the hardlinked checkpoint carries pending L0 compaction, and a 16 s open let
RocksDB finish it before the timed inserts; a 1.7 s open (REVIEW.md §21.9) does not, and
measures −6 % and 5 → 10 sorted runs at the end for the same insert path. Let compaction
settle after open before comparing throughput across binaries with different open times.
The bench flag `--max-frontier-depth n` used in the recipes above is now `--max-depth n+2`
(REVIEW.md §21.9); the frontier may advance one level further than the old cap allowed.

**Filter-block sizing, for anyone re-proposing a filter.** At 62.4 M the filters are 84 MB
and the index 21 MB, both permanently inside the 1 GiB block cache. At 1.0009 B the index is
**338 MB (32 % of the cache)** and the filters are **1365 MB — they do not fit at all**, so
they evict data blocks. Filter-read time measured symmetric across prefix-mode and
total-order arms (3.1 vs 3.0 us/scan): the filters are paid for whether or not they are
consulted.

## 6. What to measure next time

### 6.1 Keep these — they are what made the current analysis possible

RocksDB writes all of this to `bigdb/LOG` for free every `stats_dump_period_sec`
(default 600 s). **`LOG` is rotated on every open, so preserve `LOG.old.*`** — 575
files, 3.2 GB, and the entire time series lives in them. Losing them loses the history.

| Signal | Exact line / grep | What it bought |
|---|---|---|
| Options actually in force | `Options.*` block, first ~200 lines after each open | Caught that `max_background_jobs` changed mid-build and that the 1 GiB cache was a never-consulted row cache |
| Write volume + stall | `Cumulative writes: N writes, N keys, …, ingest: X GB` and `Cumulative stall: HH:MM:SS, P percent` | The headline “49.6 % stalled, 8.1× write amp” |
| Per-level compaction table | header `Level Files Size Score Read(GB) Rn(GB) Rnp1(GB) Write(GB) … W-Amp … Comp(sec) CompMergeCPU(sec) Comp(cnt) KeyIn KeyDrop`, plus the `Sum`/`Int` rows | Live size and file count at any past instant; compaction CPU vs thread-time |
| Per-SST table properties | `EVENT_LOG_v1 … table_file_creation` with `table_properties` (`num_entries`, `raw_key_size`, `raw_value_size`, `data_size`, `index_size`, `compression`) | The 1.22 B live entries / 51.18 GB keys / 79.84 GB values census, and the whole compression refutation, with no `sst_dump` |
| Frontier advances | `Advancing frontier depth to D with nodes 2^D` | §5's entire table |
| Open state | `Loaded N interior nodes at depth D`, `Initialized … complete depth D, approx entries N, leaves per frontier L` | Frontier depth and open cost per run |
| Peak RSS per run | `bench_loop.py`'s `VmHWM` poll → `peak RSS: X GB` | The 6.5 → 42 GB memory curve. **Now internal**: `bench` polls `VmHWM` itself and prints `Peak RSS:` in its results block, so a memory change and a throughput change come out of the same run and `tools/ab.py` reports both. |
| Batch latency distribution | `bench`'s `--- Batch Latency ---` block (mean/p50/p95/p99/min/max) | Frontier-advance spikes show up as `max` |
| Insertion curve | `log_*.csv` (`timestamp,total_inserted`, one row per batch) | Every “N at time T” correlation in this document |

### 6.2 Add these — the gaps that cost the most

1. **A write census by call site.** This is the biggest hole. RocksDB says
   ~2.6–3.0 `Write()` calls per application insert (REVIEW.md quotes ~3.13; I could
   not find its derivation in the document, but the shape matches) and, far more
   alarmingly, **39.8 to 131.3 *keys* written per insert**. Nothing in the logs
   attributes those keys to a call site. Add per-batch atomic counters, dumped in the
   `=== Benchmark Results ===` block, for:
   `leaves_written`, `frontier_interiors_written`,
   `leaves_rewritten_by_load_subtree_from_storage` (REVIEW Tier-1 item 1 — the
   suspected dominant term), `interiors_written_by_persist_interior_nodes_at_depth`,
   `metadata_writes`, `write_batches_committed`, `subtree_loads`,
   `leaves_read_by_subtree_loads`. One `keys_written / inserts` ratio per call site
   would settle in a single run what the LOG can only bound in aggregate.

   > **Done — `src/census.rs`.** Relaxed atomics at the choke points rather than at
   > named call sites: `put_node` split by record kind, `write_batch`, `write_nodes`,
   > transaction commits, point gets, and the subtree-load read path. `bench` prints them
   > per insert in a `--- Write census ---` block and `--census-from` separates a build
   > phase from steady state. Counting at choke points rather than call sites is what made
   > it cheap enough to leave switched on permanently, and it loses nothing: the headline
   > `puts/insert` is how REVIEW.md now tracks this branch (11.301 → 2.556 → 1.910 →
   > 1.911), and its read-side counterpart `leaves read/insert` is what the frontier cap
   > makes grow with tree size. It earned its cost immediately — the two largest wins of
   > the next round were both found by it and neither was visible by reading the code
   > (REVIEW.md §5.2 and §5.5). This was the right thing to build first.
2. **`options.enable_statistics()` + `set_stats_dump_period_sec`.** Never enabled, so
   the tickers that would have answered read-side questions are simply absent:
   `STALL_MICROS`, `BLOCK_CACHE_DATA_{HIT,MISS}`, `NUMBER_KEYS_{WRITTEN,READ}`,
   `BYTES_{WRITTEN,READ}`, `NUMBER_DB_SEEK`/`NEXT`, `COMPACTION_*`. Add
   `PerfContext`/`IOStatsContext` around one batch in a hundred for per-batch
   `block_read_count` and `block_read_time`.

   > **Still open.** The census covers the write side from the application's end; nothing
   > covers the read side from RocksDB's.
3. **Log the RNG seed and use a deterministic key stream.** `fastrand::Rng::new()`
   makes runs incomparable and the root hash meaningless as an oracle. A
   counter-derived key (`sha256(run_seed ‖ i)`) keeps the uniform trie shape, makes
   the root hash a real cross-run check, and lets a rebuild replay the exact workload.

   > **Done.** One `fastrand::Rng` seeded from `--seed` is threaded through the whole run
   > rather than reseeded per window, so a run is a function of the seed and the entry
   > count alone, and the seed is echoed in the results block. That is exactly what turns
   > the root hash into `tools/ab.py`'s correctness gate (§5.2). `--max-entries` landed
   > with it, which is what makes an A/B fixed-work rather than fixed-time. Verified: the
   > same seed reproduces the hash and a different seed does not.
4. **Record `leaf_count` and `complete_depth` in the CSV**, not just at open. The CSV
   is `timestamp,total_inserted` only, so leaf count vs frontier depth had to be
   reconstructed by joining three log families by wall-clock second. Two extra columns
   remove all of that.

   > **Done.** The header is now
   > `timestamp,total_inserted,leaf_count,complete_depth`, one row per batch, and
   > `leaf_count` is the exact counter rather than the old ~100-sample estimate.
5. **Emit the frontier-advance events with the leaf count inline.** The current
   `Advancing frontier depth to D with nodes 2^D` omits the only number anyone wants:
   `leaf_count` at that instant. Add it — §5's table would then be one grep.

   > **Still open**, and now cheap: the CSV carries `leaf_count` per batch (item 4), so
   > the join is against one file instead of three, but the log line itself is unchanged.
6. **A run manifest.** Write `git rev-parse HEAD`, `git status --porcelain`,
   `rustc -V`, `ulimit -n`, `nproc`, `free -b`, and the resolved `RocksTransRelConfig`
   into each `cli_*.log` header. Recovering “which code built this database” from
   commit timestamps versus session start times is guesswork, and here it mattered
   (`05a34a8` was committed *after* the last run it affected).
7. **An `fio` QD1 4/8/16 KiB random-read latency curve for the device**, once, at
   build time. REVIEW 3.3.5 cannot conclude on `block_size` without it, and the
   database is being deleted with the question still open.
8. **Timestamp the SSTs' creation against the insert curve.** Every
   `table_file_creation` event has a timestamp; joining that to `log_*.csv` gives
   bytes-written-per-insert as a function of N with no extra instrumentation at all.

### 6.3 The baseline table a future run has to beat

Ten consecutive 7200 s runs, all with the frontier at its cap of 23 (except the
first). Everything after `inserted` is per-run RocksDB `Cumulative` state, so each row
is self-contained. **This is the reference curve.**

| Run start (UTC) | F | N start | N end | inserted | L = N/2^F | keys written | keys/insert | writes/insert | ingest (GB) | stall % | entries/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 02-27 23:13 | 21 | 571,590,000 | 600,190,000 | 28,600,000 | 279 | 3755 M | 131.3 | 2.94 | 357.69 | 49.6 | 3962 |
| 02-28 01:14 | 23 | 600,190,000 | 653,350,000 | 53,160,000 | 75 | 2116 M | 39.8 | 2.71 | 202.72 | **8.8** | **7437** |
| 02-28 03:14 | 23 | 653,350,000 | 705,150,000 | 51,800,000 | 81 | 2405 M | 46.4 | 2.97 | 230.26 | 9.0 | 7237 |
| 02-28 05:14 | 23 | 705,150,000 | 753,920,000 | 48,770,000 | 87 | 2475 M | 50.7 | 2.97 | 236.84 | 12.4 | 6811 |
| 02-28 07:15 | 23 | 753,920,000 | 799,510,000 | 45,590,000 | 93 | 2516 M | 55.2 | 2.98 | 240.61 | 16.5 | 6368 |
| 02-28 09:15 | 23 | 799,510,000 | 842,110,000 | 42,600,000 | 98 | 2358 M | 55.4 | 2.65 | 225.39 | 22.7 | 5956 |
| 02-28 11:15 | 23 | 842,110,000 | 878,580,000 | 36,470,000 | 103 | 2226 M | 61.0 | 2.63 | 212.67 | 32.7 | 5101 |
| 02-28 13:15 | 23 | 878,580,000 | 910,270,000 | 31,690,000 | 107 | 2119 M | 66.9 | 2.59 | 202.28 | 41.0 | 4432 |
| 02-28 15:15 | 23 | 910,270,000 | 939,680,000 | 29,410,000 | 110 | 2094 M | 71.2 | 2.58 | 199.84 | 44.9 | 4115 |
| 02-28 17:15 | 23 | 939,680,000 | 963,060,000 | 23,380,000 | 113 | 2040 M | 87.3 | 2.95 | 194.60 | **50.6** | **3269** |

Two readings that any future optimisation must be compared against:

- **REVIEW.md's headline “49.6 % stalled” is the *worst* row, and it is the F = 21 row
  taken minutes before the frontier advanced.** The moment the frontier reached 23,
  stall collapsed to 8.8 % and throughput nearly doubled to 7437 entries/s. Quoting
  49.6 % without saying which run it came from overstates the steady state by ~5×.
- **The decay after the cap is the design's central weakness, measured.** With F
  pinned at 23, L = N/2²³ grows linearly with N; over 363 M further inserts L went
  75 → 113 (+51 %), keys written per insert went 39.8 → 87.3 (+119 %), stall went
  8.8 % → 50.6 %, and throughput fell 7437 → 3269 entries/s (−56 %). That is REVIEW
  §2.2's “the frontier cap turns the design from O(1)-read to O(N)-read per insert”,
  confirmed end-to-end. A future run should reproduce this curve at
  `max_frontier_depth = 20` in under an hour rather than over 20.

  > **Done in eight minutes (§5.2) — but only the regime, not the curve.** The 40 M-leaf
  > reference database sits at its frontier cap with L = 76, inside this table's 75–113,
  > and it reproduces the *mechanism*: F pinned, L = N/2^F growing with N. It does not
  > reproduce this table's decay, which needs hundreds of millions of further inserts, and
  > it cannot reproduce the stall column at all — 63 SSTs and 3 GB do not stall. Compare
  > structural columns (keys/insert, writes/insert, L) against it; do not compare
  > entries/s or stall %.

```sh
# per-run cumulative write and stall state (LOG.old is rotated at the NEXT open,
# so LOG.old.<t> is the log of the run that ENDED at <t>)
for f in bigdb/LOG.old.*; do
  tail -c 40000000 "$f" | grep -a "Cumulative writes\|Cumulative stall" | tail -2
done
```

---

## Gaps and caveats, stated plainly

- **Exact leaf count is not recoverable.** 978,620,000 is the sum of per-run CSV
  totals; 36 runs wrote a header-only CSV and ~245 runs' CSVs were overwritten by a
  same-second successor. All of those were sub-second crashes that inserted nothing,
  so the figure should be exact or a hair low. The database's own
  `approx entries 999502643` is a 100-sample extrapolation from February-era code and
  should not be preferred.
- **The `ulimit -n` in force during the build is unknown**; 1024 is inferred from
  EMFILE at 512 live SSTs, not observed.
- **`bigdb` is not option-identical to a HEAD rebuild** (row cache vs block cache,
  `increase_parallelism(32)`, and `max_background_jobs` 32→8 mid-build at 397 M
  entries). Any A/B against a fresh database is confounded by this.
- **The February host cannot be verified as this host.** CPU/RAM/disk above are read
  from the container today.
- **Frontier-advance leaf counts for depths 1–14 cannot be separated** — they all
  land in the same 1-second log bucket, bracketed only as 20 000–360 000.
- **REVIEW.md's “~3.13 writes/insert” has no derivation I could locate** in the
  document. The per-run `Cumulative writes` figures give 2.58–2.98 with a mean of
  2.80, which is the same quantity's shape; I have not reconciled the difference.
- **Compaction/write-amp attribution is aggregate only.** Nothing in the logs
  separates leaf writes, frontier-interior writes, and `load_subtree_from_storage`
  write-back. That is precisely the census §6.2.1 asks for, and it is the one number
  a future optimisation most needs and this database cannot supply.


## On-disk composition: records, sizes and compression

*Database: `bigdb`, RocksDB 8.10.0, `db_id = 66abbf6c-b664-426e-aefe-acabe3f16117`.
Written by `src/bin/bench.rs` over 575 RocksDB sessions between `2026/02/26-21:03:07`
(first line of `LOG.old.1772140388962074`) and `2026/02/28-20:48:15` (last line of `LOG`),
i.e. ~47.7 h wall. The process was killed mid-write: `LOG` ends in a truncated JSON line
and 38 SSTs on disk are orphans.*

### 0. How "live" was determined

`ldb`/`sst_dump` are not installed, so liveness came from **parsing
`MANIFEST-1319646` directly** (the file named by `bigdb/CURRENT`). The MANIFEST is a
RocksDB write-ahead-log-format file whose records are `VersionEdit`s; replaying
`kNewFile*` (tags 7/100/102/103) and `kDeletedFile` (tag 6) from the start-of-manifest
snapshot to EOF yields the current version's file set.

Result: **4058 new-file edits, 1775 delete-file edits, 2283 live files.**

| check | value |
|---|---|
| `.sst` files on disk | 2321 |
| live files in `MANIFEST-1319646` | **2283** |
| live files missing from disk | **0** |
| on-disk files not in the MANIFEST (orphans) | **38** (1,989,139,845 B = 1.99 GB) |
| live files whose MANIFEST size ≠ `stat()` size | **0** |
| live files whose MANIFEST size ≠ `table_file_creation.file_size` | **0** |
| live files with a `table_file_creation` event in `LOG*` | **2283 / 2283** |
| duplicate creation events for one live file number | 0 |

The 38 orphans are compaction/flush outputs whose `VersionEdit` was never committed
before the kill (6 of them — 1321611, 1321616–1321620 — have no creation event either,
because `LOG` is truncated). **Every number below is over the 2283 live files only.**

> Cross-check on the parse: the last per-level table RocksDB itself printed, at the
> final open (`bigdb/LOG` line 371-378, `2026/02/28-20:43:38`), was
> `L0 6, L3 8, L4 43, L5 279, L6 1946 = 2282 files / 118.03 GB`. My MANIFEST replay of
> the state ~4.5 min later gives `L0 6, L3 12, L4 43, L5 275, L6 1947 = 2283 /
> 118.08 GiB` — consistent with the compactions the log shows running in between.
> **The MANIFEST is the source used for the level split below**, not that table, because
> the table is 4.5 minutes stale.

> Note on a number already in `REVIEW.md`: its "2 315 live SSTs, 1 218 591 870 live
> entries, 51.18 GB raw keys, 79.84 GB raw values" is the census over *all SSTs on disk
> that have a creation event* (2315 = 2321 − 6 truncated), not over the MANIFEST's live
> set. I reproduced that figure exactly, so the two are reconciled — the live-only
> figures are ~1.4 % smaller.

```bash
# ---- 1. live file set from the MANIFEST -> /tmp/live_files.tsv  (level, file_number, size)
cat > /tmp/parse_manifest.py <<'PY'
import sys, struct
BLOCK, HDR = 32768, 7
def records(path):
    d = open(path,'rb').read(); off = 0; buf = b''
    while off + HDR <= len(d):
        if BLOCK - (off % BLOCK) < HDR: off += BLOCK - (off % BLOCK); continue
        crc, ln, ty = struct.unpack_from('<IHB', d, off); off += HDR
        if ty == 0 and ln == 0: off += (BLOCK - (off % BLOCK)) % BLOCK; continue
        p = d[off:off+ln]; off += ln
        if ty == 1: yield p
        elif ty == 2: buf = p
        elif ty == 3: buf += p
        elif ty == 4: buf += p; yield buf; buf = b''
def gv(b,i):
    r=s=0
    while True:
        c=b[i]; i+=1; r |= (c & 0x7f) << s
        if not c & 0x80: return r, i
        s += 7
def gs(b,i):
    n,i = gv(b,i); return b[i:i+n], i+n
live = {}
for rec in records(sys.argv[1]):
    i = 0
    while i < len(rec):
        tag,i = gv(rec,i)
        if tag == 1: _,i = gs(rec,i)
        elif tag in (2,3,4,9,10): _,i = gv(rec,i)
        elif tag == 5: _,i = gv(rec,i); _,i = gs(rec,i)
        elif tag == 6:
            l,i = gv(rec,i); fn,i = gv(rec,i); live.pop(fn, None)
        elif tag in (7,100,102,103):
            l,i = gv(rec,i); fn,i = gv(rec,i)
            if tag == 102: _,i = gv(rec,i)
            sz,i = gv(rec,i); sm,i = gs(rec,i); lg,i = gs(rec,i)
            if tag != 7: _,i = gv(rec,i); _,i = gv(rec,i)
            if tag == 103:
                while True:
                    ct,i = gv(rec,i)
                    if ct == 1: break
                    _,i = gs(rec,i)
            live[fn] = (l, sz, sm, lg)
        elif tag == 200: _,i = gv(rec,i)
        elif tag == 201: _,i = gs(rec,i)
        elif tag == 202: pass
        elif tag == 203: _,i = gv(rec,i)
        elif tag == 300: _,i = gv(rec,i)
        elif tag >= 8192 or tag in (400,401): _,i = gs(rec,i)   # kTagSafeIgnoreMask family
        else: raise SystemExit(f'unknown tag {tag}')
print(f'live files: {len(live)}', file=sys.stderr)
for fn,(l,sz,sm,lg) in sorted(live.items()): print(f'{l}\t{fn}\t{sz}')
PY
python3 /tmp/parse_manifest.py "bigdb/$(cut -d- -f2 bigdb/CURRENT | tr -d '\n' | sed 's/^/MANIFEST-/;s/^MANIFEST-/MANIFEST-/')" > /tmp/live_files.tsv
# simpler: python3 /tmp/parse_manifest.py bigdb/MANIFEST-1319646 > /tmp/live_files.tsv

# ---- liveness cross-checks
ls bigdb/*.sst | sed 's#.*/##;s#\.sst##' | sort -n > /tmp/ondisk.txt
cut -f2 /tmp/live_files.tsv | sort -n > /tmp/livenums.txt
comm -13 /tmp/ondisk.txt /tmp/livenums.txt | wc -l    # live but missing -> 0
comm -23 /tmp/ondisk.txt /tmp/livenums.txt | wc -l    # orphans          -> 38
```

---

### 1. Total on-disk bytes and the split by level

Level split is from the MANIFEST replay; per-file sizes agree byte-for-byte with
`stat()` and with `table_file_creation.file_size`.

| level | live files | bytes | GiB | GB |
|---:|---:|---:|---:|---:|
| L0 | 6 | 220,571,532 | 0.21 | 0.22 |
| L1 | 0 | 0 | — | — |
| L2 | 0 | 0 | — | — |
| L3 | 12 | 572,341,208 | 0.53 | 0.57 |
| L4 | 43 | 2,290,835,724 | 2.13 | 2.29 |
| L5 | 275 | 17,016,897,464 | 15.85 | 17.02 |
| L6 | 1947 | 106,691,377,946 | 99.36 | 106.69 |
| **live total** | **2283** | **126,792,023,874** | **118.08** | **126.79** |

Everything else in the directory:

| component | bytes | note |
|---|---:|---|
| live SSTs | 126,792,023,874 | the 2283 files above |
| orphan SSTs | 1,989,139,845 | 38 files, reclaimable |
| all `.sst` on disk | 128,781,163,719 | 2321 files |
| `LOG` + 574 × `LOG.old.*` | 3,335,732,951 | 575 files |
| WAL `1321613.log` | 15,728,640 | unflushed writes at the kill |
| `MANIFEST-1319646` | 648,517 | |
| 2 × `OPTIONS-*` | 14,516 | |
| **`du -sb bigdb/`** | **132,133,808,588** | 123.06 GiB / 132.13 GB |

```bash
du -sb bigdb/
python3 - <<'PY'
import collections
c = collections.Counter(); z = collections.Counter(); tot = n = 0
for line in open('/tmp/live_files.tsv'):
    l, f, s = line.split(); s = int(s)
    c[int(l)] += 1; z[int(l)] += s; tot += s; n += 1
for l in sorted(c): print(f'L{l}\t{c[l]:>5} files\t{z[l]:>15,} B\t{z[l]/2**30:8.2f} GiB')
print(f'TOT\t{n:>5} files\t{tot:>15,} B\t{tot/2**30:8.2f} GiB')
PY
du -cb bigdb/LOG bigdb/LOG.old.* | tail -1
```

---

### 2. Record census from `table_file_creation`

Extracted from the 1,165,721 `table_file_creation` events in `bigdb/LOG*` (3.34 GB of
logs), keeping the 2283 whose `file_number` is in the live set.

```bash
# how many creation events exist over the db's whole life
for f in bigdb/LOG bigdb/LOG.old.*; do LC_ALL=C grep -c table_file_creation "$f"; done \
  | awk '{s+=$1} END {print "total table_file_creation events:", s}'      # -> 1165721

cat > /tmp/extract_props.py <<'PY'
import sys, json, re
live = {}
for line in open('/tmp/live_files.tsv'):
    l, f, s = line.split(); live[int(f)] = (int(l), int(s))
fnre = re.compile(rb'"file_number": (\d+)')
cols = ['file_number','level','manifest_size','file_size','data_size','index_size',
        'filter_size','raw_key_size','raw_value_size','raw_average_key_size',
        'raw_average_value_size','num_entries','num_data_blocks','num_deletions',
        'num_filter_entries','num_range_deletions','num_merge_operands','compression']
out = {}
for raw in sys.stdin.buffer:
    m = fnre.search(raw)
    if not m: continue
    fn = int(m.group(1))
    if fn not in live: continue
    ev = json.loads(raw[raw.find(b'{', raw.find(b'EVENT_LOG_v1')):].decode('utf-8','replace'))
    if ev.get('event') != 'table_file_creation': continue
    tp = ev['table_properties']
    r = {'file_number': fn, 'level': live[fn][0], 'manifest_size': live[fn][1],
         'file_size': ev['file_size']}
    r.update({k: tp[k] for k in cols[4:]})
    out[fn] = r
sys.stderr.write(f'live files with properties: {len(out)}/{len(live)}\n')
w = open('/tmp/live_props.tsv','w'); w.write('\t'.join(cols)+'\n')
for fn in sorted(out): w.write('\t'.join(str(out[fn][c]) for c in cols)+'\n')
PY
LC_ALL=C grep -h table_file_creation bigdb/LOG bigdb/LOG.old.* | python3 /tmp/extract_props.py
```

**Live totals (exact sums over the 2283 files):**

| property | value | |
|---|---:|---|
| `num_entries` | **1,201,676,755** | physical records, incl. LSM duplicates |
| `raw_key_size` | **50,470,423,540** | 47.00 GiB — *internal* keys (user key + 8 B seqno/type) |
| `raw_value_size` | **78,737,208,611** | 73.33 GiB |
| raw key + value | **129,207,632,151** | 120.33 GiB |
| `data_size` | **126,395,569,116** | 117.72 GiB — compressed data blocks |
| `index_size` | **573,025,238** | 546.5 MiB — **uncompressed** (see note) |
| `filter_size` | **0** | `filter_policy=nullptr`; no bloom filters exist |
| `num_filter_entries` | 0 | |
| `num_deletions` | **0** | nothing is ever deleted (location-addressed, no GC) |
| `num_range_deletions` | 0 | |
| `num_merge_operands` | 0 | |
| `num_data_blocks` | 31,776,204 | avg data block = 3977.7 B (`block_size=4096`) |
| `compression` | `Snappy` on **all 2283** | `compression=kSnappyCompression`, `bottommost_compression=kDisableCompressionOption` (i.e. inherits Snappy) |

> `index_size` is the **uncompressed** index size, which is why `data_size + index_size`
> (126,968,594,354) exceeds `file_size` (126,792,023,874). Derived stored fraction:
> `(file_size − data_size)/index_size = 0.6919` (this lumps the footer and meta blocks
> in with the index, so it is a slight over-estimate of the index's own ratio).
> `format_version=5`, `index_type=kBinarySearch`, `enable_index_compression=true`.

**Per level:**

| level | files | file_size | data_size | index_size | entries | leaves | interiors | metadata | raw_key | raw_value |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| L0 | 6 | 220,571,532 | 219,844,482 | 990,769 | 2,084,298 | 2,066,189 | 18,105 | 4 | 87,540,448 | 136,094,752 |
| L3 | 12 | 572,341,208 | 570,477,015 | 2,564,535 | 5,396,285 | 5,378,336 | 17,949 | 0 | 226,643,970 | 351,368,791 |
| L4 | 43 | 2,290,835,724 | 2,283,435,752 | 10,307,059 | 21,652,733 | 21,455,433 | 197,298 | 2 | 909,414,752 | 1,414,135,683 |
| L5 | 275 | 17,016,897,464 | 16,963,480,485 | 76,580,008 | 160,834,181 | 159,367,687 | 1,466,492 | 2 | 6,755,035,568 | 10,504,082,399 |
| L6 | 1947 | 106,691,377,946 | 106,358,331,382 | 482,582,867 | 1,011,709,258 | 994,932,041 | 16,777,215 | 2 | 42,491,788,802 | 66,331,526,986 |
| **total** | **2283** | **126,792,023,874** | **126,395,569,116** | **573,025,238** | **1,201,676,755** | **1,183,199,686** | **18,477,059** | **10** | **50,470,423,540** | **78,737,208,611** |

Live file size distribution: min 12,720 B, p25 61,234,253, median 67,317,259,
p75 67,319,923, max 95,867,471; 1603 of 2283 sit at the ~64 MiB target file size.

#### 2a. This database is 100 % LEGACY-format — record it

`src/mpt/storage/rocks.rs` now has three value tags:

| tag | shape | length |
|---|---|---:|
| `TAG_LEAF = 0` (legacy) | `1 ‖ value[32] ‖ merkle_hash[32]` | **65 B** |
| `TAG_INTERIOR = 1` | `1 ‖ merkle_hash[32] ‖ bincode(left Prefix) ‖ bincode(right Prefix)` = `1+32+33+33` | **99 B** |
| `TAG_LEAF_COMPACT = 2` | `2 ‖ value[32]` | **33 B** |

`TAG_LEAF_COMPACT` was added **after** this database was built, and the code comment at
`src/mpt/storage/rocks.rs:141-144` already says so. The data confirms it: solving each
file's record mix (below) leaves **zero residual** on all 2283 files, i.e. every one of
the 1,183,199,686 leaf records is exactly 65 B and every one of the 18,477,059 interior
records is exactly 99 B. **There is not a single 33-byte compact leaf in the database.**
Any future measurement against a database written by the current code is measuring a
different value encoding and is not comparable to these numbers.

(The interior being *exactly* 99 B everywhere is itself a finding: bincode 2
`config::standard()` varint-encodes the `Prefix.length` `u16`, so a child prefix is 33 B
only while `length ≤ 250` and becomes 35 B at `length ≥ 251`. No live interior has a
child prefix longer than 250 bits — in particular no live interior has a direct leaf
child, whose prefix length would be 256.)

#### 2b. Splitting leaves from interiors — method

`raw_average_value_size` alone is not enough: it is integer-truncated and 9 files hold a
mix. Instead each file was solved exactly. Node keys are `length_be_u16 ‖ hash` = 34 B
for both leaves and interiors, so RocksDB counts 34 + 8 = 42 raw key bytes per node
record. Two metadata records sort above every node key
(`src/mpt/storage/rocks.rs:14-15`):

* `__mpt_root__` → 12 + 8 = 20 raw key B, value = `encode_prefix(root)` = 34 B
* `__mpt_complete_depth__` → 22 + 8 = 30 raw key B, value = `u16` BE = 2 B

so a file holding both shows a `raw_key_size` deficit of exactly **34** against `42·N`,
and 36 B of metadata value. Then

```
m  = 2 if (42*N - raw_key_size) == 34 else 0          # metadata records
Nn = N - m
interiors = ((raw_value_size - 36*(m//2)) - 65*Nn) / 34
leaves    = Nn - interiors
```

**The deficit histogram over the 2283 live files is `{0: 2278, 34: 5}` — no other value —
and the interior residual is an exact non-negative multiple of 34 in every single file.**
That is a complete, self-consistent fit; the split below is exact arithmetic, not an
estimate.

The 5 metadata-bearing files are 1321580 (L0), 1321596 (L0), 1321603 (L4), 1320721 (L5),
1317847 (L6) → 10 physical metadata records = 5 stale copies of the same 2 keys.
**A deficit of 34 (not 50) also proves `__mpt_leaf_count__` is absent from every live
SST**, i.e. this database predates that key too.

Independent confirmation from the MANIFEST's smallest/largest keys: the length-field of
the first 2 bytes of each live file's key range is `0..23` for interior files, `256` for
leaf files, and `0x5F5F = 24415` (`"__"`) for exactly the 5 metadata files. Bucketing the
2283 live files that way gives 2252 wholly-leaf + 19 wholly-interior + 7 straddling +
5 metadata-tailed = 2283.

#### 2c. The split

| bucket | files | entries | leaf records | interior records | metadata |
|---|---:|---:|---:|---:|---:|
| leaf-only files | 2255 | 1,179,679,103 | 1,179,679,097 | 0 | 6 |
| interior-only files | 19 | 17,732,331 | 0 | 17,732,331 | 0 |
| mixed files | 9 | 4,265,321 | 3,520,589 | 744,728 | 4 |
| **total** | **2283** | **1,201,676,755** | **1,183,199,686** | **18,477,059** | **10** |

| bucket | files | file_size | data_size | index_size | raw_key | raw_value |
|---|---:|---:|---:|---:|---:|---:|
| leaf-only | 2255 | 125,273,143,387 | 124,885,721,394 | 560,561,090 | 49,546,522,224 | 76,679,141,413 |
| interior-only | 19 | 1,098,352,076 | 1,090,843,243 | 10,360,188 | 744,757,902 | 1,755,500,769 |
| mixed | 9 | 420,528,411 | 419,004,479 | 2,103,960 | 179,143,414 | 302,566,429 |

**The disjoint-key-range claim holds, and tightly.** Only **28 of 2283** live files
contain any interior record at all:

* 19 pure-interior files (17 × L6, 1 × L5, 1 × L3),
* **exactly one** interior/leaf straddling file per compacted level — 1319775 (L6),
  1320874 (L5), 1321524 (L4),
* the 6 L0 flush outputs, which each span the whole keyspace and so each carry ~3000
  interiors alongside ~345,000 leaves (<1 %, which is why their
  `raw_average_value_size` still rounds to 65).

```bash
python3 - <<'PY'
import csv, collections
rows = list(csv.DictReader(open('/tmp/live_props.tsv'), delimiter='\t'))
per = collections.defaultdict(collections.Counter)
for r in rows:
    N, RK, RV = int(r['num_entries']), int(r['raw_key_size']), int(r['raw_value_size'])
    d = 42*N - RK
    assert d in (0, 34), (r['file_number'], d)            # <- the fit assertion
    m = 2 if d == 34 else 0
    Nn = N - m
    resid = (RV - 36*(m//2)) - 65*Nn
    assert resid % 34 == 0 and 0 <= resid <= 34*Nn, (r['file_number'], resid)
    ni = resid // 34; nl = Nn - ni
    kind = 'leaf-only' if ni == 0 else ('interior-only' if nl == 0 else 'mixed')
    for k in ('L'+r['level'], 'TOTAL', kind):
        c = per[k]; c['files'] += 1
        for f in ('file_size','data_size','index_size','raw_key_size','raw_value_size',
                  'num_entries','num_data_blocks'): c[f] += int(r[f])
        c['leaves'] += nl; c['interiors'] += ni; c['meta'] += m
for k in ('L0','L3','L4','L5','L6','TOTAL','leaf-only','interior-only','mixed'):
    c = per[k]; raw = c['raw_key_size'] + c['raw_value_size']
    print(f"{k:14s} files={c['files']:>5} file={c['file_size']:>15,} data={c['data_size']:>15,} "
          f"idx={c['index_size']:>11,} entries={c['num_entries']:>13,} leaves={c['leaves']:>13,} "
          f"int={c['interiors']:>10,} meta={c['meta']:>3} rawK={c['raw_key_size']:>14,} "
          f"rawV={c['raw_value_size']:>14,} data/raw={c['data_size']/raw:.4f}")
PY
```

---

### 3. Compression achieved

Ratio = `data_size / (raw_key_size + raw_value_size)`. Snappy on all files; there is no
separate bottommost setting in force (`bottommost_compression=kDisableCompressionOption`
means "inherit", and every L6 file's event says `"compression": "Snappy"`).

| bucket | raw key+value | data_size | **data/raw** | ×reduction | file_size/raw |
|---|---:|---:|---:|---:|---:|
| **leaf files** | 126,225,663,637 | 124,885,721,394 | **0.9894** | **1.011×** | 0.9925 |
| **interior files** | 2,500,258,671 | 1,090,843,243 | **0.4363** | **2.292×** | 0.4393 |
| mixed (boundary) files | 481,709,843 | 419,004,479 | 0.8698 | 1.150× | 0.8730 |
| **all live** | 129,207,632,151 | 126,395,569,116 | **0.9782** | 1.022× | 0.9813 |

Per level: L0 0.9830, L3 0.9870, L4 0.9827, L5 0.9829, **L6 0.9773**.

**Leaves are incompressible, and that is structural, not a tuning miss.** A leaf record
is a 256-bit key hash plus a 32-byte value plus a 32-byte SHA-256 — three
uniformly-random blobs. Snappy recovers 1.1 %, which is roughly what key
prefix-compression inside the 4 KiB blocks buys and nothing more.

**Interiors compress 2.3×**, because an interior's key is `length ‖ prefix-hash` with the
tail bits zeroed (so keys at the same depth share long prefixes and the block's
delta-encoding eats them), and its 99-byte value is `merkle_hash ‖ left_prefix ‖
right_prefix` where both child prefixes share the parent's leading bits.

Whole-database bottom line: **the live SST bytes (126,792,023,874) are 1.87 % *smaller*
than the raw key+value bytes they contain (129,207,632,151).** After index, footer and
block overhead, compression on this workload nets essentially zero.

---

### 4. Derived per-record byte layout, and bytes attributable to each

**Logical layout (from the source, confirmed exactly by the census):**

| | leaf (`TAG_LEAF`, legacy) | interior (`TAG_INTERIOR`) | compact leaf (`TAG_LEAF_COMPACT`, *unused here*) |
|---|---:|---:|---:|
| user key `length_be_u16 ‖ hash` | 34 | 34 | 34 |
| + RocksDB internal-key footer | 8 | 8 | 8 |
| value | 65 | 99 | 33 |
| **raw bytes / record** | **107** | **141** | **75** |

**Measured on-disk cost per record** (from the pure-kind files, where there is no mixing
to unpick):

| | bytes / record | derivation |
|---|---:|---|
| legacy leaf | **106.19** | 125,273,143,387 ÷ 1,179,679,097 |
| interior | **61.94** | 1,098,352,076 ÷ 17,732,331 |

Sanity check on those two rates: applying them to the 9 mixed files predicts
419,989,294 B against an actual 420,528,411 B — **−0.128 %**. The rates are sound, so
they can be used to attribute the whole database:

| | records | attributed bytes | share of live SST bytes |
|---|---:|---:|---:|
| **leaf records** | 1,183,199,686 | **125,647,003,746** | **99.10 %** |
| **interior records** | 18,477,059 | **1,144,481,011** | **0.90 %** |
| sum | | 126,791,484,757 | vs actual 126,792,023,874 (−0.0004 %) |

Other useful per-record denominators:

* **105.51 B** per live entry (126,792,023,874 ÷ 1,201,676,755)
* **107.16 B** per physical leaf record (whole db ÷ 1,183,199,686)
* **127.44 B** per *distinct* leaf (whole db ÷ 994,932,041, see §5) — this is the number
  to beat: **~127 bytes of disk per logical key→value pair**, for a 32-byte key and a
  32-byte value.

Forward-looking, since this database is the *before* picture: the 33-byte compact leaf
removes 32 raw value bytes per leaf, i.e. **37,862,389,952 B (37.86 GB) of raw value**
at this record count — and because leaf data is ~0.989× raw, close to that much on disk.

---

### 5. Frontier depth, leaf count, leaves per frontier node

**Frontier depth reached: 23.** Three independent confirmations:

1. `bench_logs/cli_20260228_204337.log` (the last open, 20:43:52) —
   `Loaded 8388608 interior nodes at depth 23 from storage` and
   `complete depth 23`. 8,388,608 = 2²³ exactly.
2. **L6 holds exactly 16,777,215 interior records = 2²⁴ − 1 = Σ(d=0..23) 2^d.** The
   interior layer is a *perfect* binary tree down to depth 23 with not one node missing
   and not one duplicate — a strong signal that L6's interior key band is fully compacted
   and version-collapsed.
3. The MANIFEST's smallest/largest keys put every interior file's length-field in
   **0..23** and nothing deeper (see §2b), so no depth-24 layer was ever persisted.

Total live interior records are 18,477,059; the excess of **1,699,844** over 2²⁴−1 is
newer copies sitting in L0–L5 that shadow the L6 originals. **Distinct interior nodes =
16,777,215.**

The frontier's history, from the `complete depth` line of the 331 `cli_*.log` files that
have one:

| first log at that depth | depth | reported entries | logged "leaves per frontier" |
|---|---:|---:|---:|
| `cli_20260226_210307.log` | 0 | 1 | 0 |
| `cli_20260226_211308.log` | 19 | 26,817,331 | 25 |
| `cli_20260226_212309.log` | 20 | 40,988,835 | 19 |
| `cli_20260226_234956.log` | 21 | 148,897,792 | 35 |
| `cli_20260228_011404.log` | 23 | 614,213,877 | 36 |
| `cli_20260228_204337.log` (last) | 23 | 999,502,643 | 59 |

*(Depth 22 never appears as the value at an open — it was passed within a single run.)*

> **Caveat on those two logged columns, both needed to read the table correctly.**
> (a) `approx entries` is `instance.len()`, which for a database with no
> `__mpt_leaf_count__` key — which is this one — is seeded from
> `estimate_leaf_count()`, a **sample of 100 frontier subtrees** scaled up
> (`src/mpt/rocks_frontier/mod.rs:1540-1550`); the code's own comment puts its error at
> ~1 %. It is not a count.
> (b) The logged "leaves per frontier" is **half** what today's code would print: every
> value in that column equals `leaves >> (depth+1)` (999,502,643 >> 24 = 59), whereas
> `leaves_per_frontier_node` in the current source divides by `1 << depth`
> (`mod.rs:1556-1561`, whose doc comment explicitly says "not by `2 << D`"). The formula
> was corrected after these logs were written. Read 59 as 119.

**Leaf count.** No single exact figure survives, so here are four measurements with their
provenance, which bracket **~9.9 × 10⁸**:

| source | value | what it actually is |
|---|---:|---|
| sum of `num_entries` over live L6 files, minus its 16,777,215 interiors and 2 metadata | **994,932,041** | exact count of *physical* leaf records in the bottommost level; equals distinct leaves iff L6 is version-collapsed (which its perfect interior layer argues it is) |
| all live leaf records | 1,183,199,686 | exact, but includes L0–L5 records that shadow an L6 copy |
| `len()` at the last open, `cli_20260228_204337.log` | 999,502,643 | 100-subtree **sample**, ~1 % error |
| Σ of final `total_inserted` over `bench_logs/log_*.csv` | 978,620,000 | **lower bound**: 36 of 330 CSVs are empty, and a killed run loses its untailed samples |

**Leaves per frontier node** (2²³ = 8,388,608 nodes at depth 23):

| using | leaves / 2²³ |
|---|---:|
| L6 leaf records (994,932,041) | **118.61** |
| sampled `len()` (999,502,643) | **119.15** |
| bench CSV sum (978,620,000) | 116.66 |
| all physical leaf records (1,183,199,686) | 141.05 |

The ~139 figure quoted in `REVIEW.md:578` corresponds to the last row (physical records
÷ 2²³); the *logical* value is **~119**.

**`rocksdb.estimate-num-keys` does not appear anywhere in `bigdb/LOG*`** — verified,
zero matches across all 575 log files, so the cross-check the task asked for cannot be
done against the log. It is only reachable indirectly: `approximate_entry_count()`
(`src/mpt/storage/rocks.rs:292-298`) reads that property, and it is what `len()` falls
back to at depth 0 — which is the `approx entries 1` in the very first run's line above
(the freshly-opened empty database, whose only key was `__mpt_root__`). At depth > 0 the
logged number comes from the sampler instead, so **no logged value of
`estimate-num-keys` at scale exists to compare against.**

```bash
# frontier depth history
for f in bench_logs/cli_*.log; do
  l=$(grep -m1 -o "complete depth [0-9]*, approx entries [0-9]*, leaves per frontier [0-9]*" "$f")
  [ -n "$l" ] && printf '%s\t%s\n' "$f" "$l"
done
grep -h "Loaded [0-9]* interior nodes at depth" bench_logs/cli_20260228_204337.log

# ground-truth-ish insert total from the benchmark's own CSVs
python3 - <<'PY'
import glob
tot = n = e = 0
for f in sorted(glob.glob('bench_logs/log_*.csv')):
    last = None
    for line in open(f):
        p = line.strip().split(',')
        if len(p) >= 2 and p[0] != 'timestamp':
            try: last = int(p[1])
            except ValueError: pass
    if last is None: e += 1
    else: tot += last; n += 1
print(f'csvs={n+e} with_data={n} empty={e} sum_total_inserted={tot:,}')
PY

# confirm estimate-num-keys is absent
LC_ALL=C grep -c 'estimate.num.keys' bigdb/LOG bigdb/LOG.old.* | grep -v ':0'   # -> no output
```

---

### 6. Things that could not be determined

* **Exact distinct leaf count.** It needs either a full key scan (no `ldb`/`sst_dump`
  available, and the DB must not be modified) or a `__mpt_leaf_count__` key, which this
  database predates. Bracketed at 978,620,000 (hard lower bound) – 999,502,643 (±1 %
  sample), best single estimate **994,932,041** (L6 leaf records).
* **Whether L6 is strictly duplicate-free.** Its interior band is provably perfect
  (exactly 2²⁴−1), which is strong evidence, but the leaf band cannot be proved
  duplicate-free without a scan. If some L6 leaf ranges arrived by trivial move, the true
  distinct count is slightly below 994,932,041.
* **Depth distribution of the 18,477,059 interior records.** The MANIFEST gives only each
  file's first and last key, so I can bound the band (0..23) but not count nodes per
  depth. The 2²⁴−1 identity makes the per-depth counts inferable *if* the tree is perfect
  (2^d at each depth d ≤ 23), but that is an inference, not a measurement.
* **Contents of the 15,728,640 B WAL (`1321613.log`).** Not parsed; it holds writes that
  were never flushed, so a handful of records — including possibly a newer root/depth —
  are outside the SST census.
* **The 6 truncated-log orphans (1321611, 1321616–1321620)** have no properties anywhere;
  they are dead anyway, but their record counts are unrecoverable.


# RocksDB configuration and compaction/stall accounting

Everything below is extracted from `/workspaces/project/bigdb/LOG` and the 574
`/workspaces/project/bigdb/LOG.old.*` files (3.34 GB, 575 files). RocksDB build:
**8.10.0**, compile date 2023-12-15 (`librocksdb-sys 0.16.0+8.10.0`). DB ID
`66abbf6c-b664-426e-aefe-acabe3f16117`.

---

## 0. Reading the corpus: how sessions were identified

`Options.max_log_file_size: 0`, so RocksDB never rotated `LOG` by size — it rotated it
**once per `DB::Open()`**. Therefore *one `LOG` file == one DB open attempt*, and the
`LOG.old.<micros>` suffix is the time the *next* open rotated it away.

```bash
cd /workspaces/project/bigdb
ls LOG* | wc -l                                   # 575 files
ls -l LOG* | python3 -c "import sys;print(sum(int(l.split()[4]) for l in sys.stdin))"
grep -h "DB Session ID" LOG* | awk '{print $NF}' | sort -u | wc -l   # 574 unique
for f in LOG*; do [ -s "$f" ] || echo "EMPTY $f"; done              # LOG.old.1772149796983206
```

| Fact | Value |
|---|---|
| `LOG` files on disk | 575 (one is 0 bytes: `LOG.old.1772149796983206`) |
| Distinct `DB Session ID`s | **574** |
| Opens that **succeeded** | **335** |
| Opens that **failed** (`DB::Open() failed: IO error: While lock file: bigdb/LOCK: Resource temporarily unavailable`) | **239** — all inside a 2-second retry storm at 2026/02/26 23:46:22–23:46:24 |
| Successful sessions that ended with `Shutdown complete` | 318 |
| Successful sessions killed without shutdown | 17 |
| Opens that logged a truncated/corrupt WAL tail on recovery | 10 |
| First open | `2026/02/26-21:03:07.863729` (`LOG.old.1772140388962074`, `last_sequence is 0` — DB created here) |
| Last open | `2026/02/28-20:43:37.739107` (`LOG`) |
| Calendar span (first log line → last log line) | **1 day 23:45:07** |
| Sum of successful-session wall durations | 167,853 s = **46.63 h** |

```bash
# session index (open ts, close ts, duration, #stats dumps, size)
python3 - <<'EOF'
import glob,os,datetime
def P(t): return datetime.datetime.strptime(t,'%Y/%m/%d-%H:%M:%S.%f')
for f in ['LOG']+sorted(glob.glob('LOG.old.*')):
    if os.path.getsize(f)==0: continue
    first=last=None; nd=0; failed=False
    for line in open(f,errors='replace'):
        if line.startswith('20'):
            t=line.split()[0]; first=first or t; last=t
        nd += 'DUMPING STATS' in line
        failed |= 'DB::Open() failed' in line
    print(first,last,(P(last)-P(first)),nd,os.path.getsize(f),f,'FAILED' if failed else '')
EOF
```

### Which session is "the last / most complete"

Two different sessions matter and they are **not** the same one:

* **Session 574 = `LOG`** — opened `2026/02/28-20:43:37`, ran only **4 m 37 s**, was
  killed at 20:48:15. It emitted exactly **one** stats dump, the one printed at open
  (uptime 1.1 s). That dump carries **the most recent LSM shape** (file counts and sizes
  per level) but zero cumulative work.
* **Session 573 = `LOG.old.1772311417737349`** — opened `2026/02/28-19:15:03`, ran
  **1 h 27 m 50 s**, killed at 20:42:53. **9 stats dumps.** This is the **last session
  with real compaction/stall accounting**, and its final dump (`2026/02/28-20:35:03`,
  uptime 4800.6 s) is the reference "final" figure everywhere below.

> **Critical caveat — "Cumulative" in RocksDB is per-`DBImpl`, not per-database.**
> Every `Cumulative writes / Cumulative stall / Cumulative compaction / Flush(GB)
> cumulative` counter resets to zero on each `DB::Open()`. There is **no** single
> line in any LOG that gives a database-lifetime total. Lifetime figures in §3.1 were
> therefore rebuilt from the `EVENT_LOG_v1` stream and the manifest sequence numbers,
> which *are* durable.

Helper used repeatedly below (extracts the *n*-th stats dump, `-1` = last):

```bash
cat > /tmp/dump.py <<'EOF'
import sys,re
fn=sys.argv[1]; which=int(sys.argv[2])
strip=re.compile(r'^\d{4}/\d\d/\d\d-\d\d:\d\d:\d\d\.\d+ \d+ ')
blocks=[];cur=None
for line in open(fn,errors='replace'):
    s=strip.sub('',line.rstrip('\n'))
    if 'DUMPING STATS' in s:
        if cur: blocks.append(cur)
        cur=[s]
    elif cur is not None:
        cur.append(s)
        if 'File Read Latency Histogram' in s: blocks.append(cur); cur=None
if cur: blocks.append(cur)
print('\n'.join(blocks[which]))
EOF
python3 /tmp/dump.py LOG.old.1772311417737349 -1
```

---

## 1. Effective options at the last open

Full dump (200 lines of DB options, then the CF options):

```bash
sed -n '11,244p' /workspaces/project/bigdb/LOG | sed 's/^[0-9\/:.-]* [0-9]* //'
```

### 1.1 Options this project sets explicitly

Source: `/workspaces/project/src/mpt/storage/rocks.rs`, `RocksStorage::open`. Defaults
verified against the vendored headers at
`~/cargo-home/registry/src/index.crates.io-*/librocksdb-sys-0.16.0+8.10.0/rocksdb/include/rocksdb/{options.h,advanced_options.h,table.h}`.

| Option | Value in `bigdb/LOG` (last open) | RocksDB 8.10 default | Set at `rocks.rs:` |
|---|---|---|---|
| `create_if_missing` | `1` | `false` | 216 |
| `max_background_jobs` | **`8`** | `2` (`options.h:731`) | 271 |
| `max_subcompactions` | **`4`** | `1` (`options.h:757`) | 270 |
| `manual_wal_flush` | **`1`** | `false` (`options.h:1263`) | 269 |
| `allow_concurrent_memtable_write` | `1` | `true` (`options.h:1136`) — **set redundantly** | 268 |
| `row_cache` | **`1073741824`** (1 GiB) | `nullptr` | *(historic — see below)* |
| block cache `capacity` | `33554432` (32 MiB) | 32 MiB, silently installed by `BlockBasedTableFactory` when none is given (`block_based_table_factory.cc:448-449 → co.capacity = 32 << 20`) | *(historic — see below)* |

> **The `bigdb` config is NOT the current code's config.** Diffing the last `bigdb`
> open against a database created by the current tree
> (`tempdb/measure.db/LOG`, 2026/08/29, same RocksDB 8.10.0) gives **exactly two**
> differences:
>
> ```bash
> cd /workspaces/project
> head -300 bigdb/LOG              | grep -o "Options\.[a-z_0-9]*: .*" | sed 's/0x[0-9a-f]*/PTR/' > /tmp/a.txt
> head -300 tempdb/measure.db/LOG  | grep -o "Options\.[a-z_0-9]*: .*" | sed 's/0x[0-9a-f]*/PTR/' > /tmp/b.txt
> for f in bigdb/LOG tempdb/measure.db/LOG; do head -400 $f | grep -E "(capacity :|block_cache_name:|block_size:)" | sed 's/^[0-9\/:.-]* [0-9]* //'; done
> diff /tmp/a.txt /tmp/b.txt
> ```
> ```
> 46c46
> < Options.row_cache: 1073741824      # bigdb
> ---
> > Options.row_cache: None            # current code
> 145c145
> <   capacity : 33554432              # bigdb block cache = 32 MiB RocksDB default
> ---
> >   capacity : 1073741824            # current code = 1 GiB LRU block cache
> ```
> Every other option is byte-identical. So **any future benchmark against `bigdb`'s
> numbers is comparing a 32 MiB block cache + 1 GiB (never-consulted) row cache against
> a 1 GiB block cache + no row cache** — commit `64c5afc` "Spend the 1 GiB LRU cache on
> blocks, not rows". Nothing else about the storage configuration changed.

### 1.2 RocksDB defaults that dominated this workload

These were **not** set by the project, but they are the ones that explain the numbers in
§3–§4 and must be held constant for a comparison to mean anything.

| Option | Effective value | Note |
|---|---|---|
| `compression` | `Snappy` | default; only `kSnappyCompression supported: 1` in this build (no ZSTD/LZ4/Zlib compiled in) |
| `bottommost_compression` | `Disabled` | default |
| `block_size` / `metadata_block_size` | `4096` / `4096` | defaults (`table.h:276,303`) |
| `filter_policy` | **`nullptr`** | default (`table.h:445`) — **no bloom filters anywhere**; SST properties confirm `filter_size: 0`, `num_filter_entries: 0` |
| `whole_key_filtering` | `1` | default, inert with no filter policy |
| `checksum` | `4` (kXXH3) | default (`table.h:257`) |
| `format_version` | `5` | default (`table.h:521`) |
| `cache_index_and_filter_blocks` | `0` | default — index blocks live outside the block cache budget |
| `write_buffer_size` | `67108864` (64 MiB) | default (`options.h:186`) |
| `max_write_buffer_number` | `2` | default (`advanced_options.h:305`) |
| `min_write_buffer_number_to_merge` | `1` | default |
| `db_write_buffer_size` | `0` | default (no cross-CF memtable budget) |
| `compaction_style` | `kCompactionStyleLevel` | default |
| `num_levels` | `7` | default (`advanced_options.h:574`) |
| `level_compaction_dynamic_level_bytes` | `1` | default *since 8.x* (`advanced_options.h:691`) — this is why **L1 and L2 are permanently empty** and the base level is L3 |
| `level0_file_num_compaction_trigger` | `4` | default (`options.h:240`) |
| `level0_slowdown_writes_trigger` | `20` | default (`advanced_options.h:583`) |
| `level0_stop_writes_trigger` | `36` | default (`advanced_options.h:590`) |
| `target_file_size_base` / `_multiplier` | `67108864` (64 MiB) / `1` | default (`advanced_options.h:604`) |
| `max_bytes_for_level_base` | `268435456` (256 MiB) | default (`options.h:288`) |
| `max_bytes_for_level_multiplier` | `10.0` | default |
| `max_compaction_bytes` | `1677721600` (1.5 GiB) | derived default: `25 × target_file_size_base` |
| `compaction_pri` | `kMinOverlappingRatio` | default (`advanced_options.h:761`) |
| **`soft_pending_compaction_bytes_limit`** | **`68719476736`** (64 GiB) | default (`advanced_options.h:745`) — **the throttle that ran this benchmark** |
| **`hard_pending_compaction_bytes_limit`** | **`274877906944`** (256 GiB) | default (`advanced_options.h:753`) — never reached (0 stops) |
| **`delayed_write_rate`** | **`16777216`** (16 MiB/s) | derived default from `delayed_write_rate = 0` — the ceiling writes were clamped to while stalling |
| `compaction_readahead_size` | `2097152` | default (`options.h:969`) |
| `max_open_files` | **`-1`** (unlimited) | default (`options.h:632`) — directly caused the fd exhaustion in §7.2 |
| `ttl` | `2592000` (30 d) | derived default for level compaction |
| `periodic_compaction_seconds` | `0` | default |
| `use_fsync` / `bytes_per_sync` / `wal_bytes_per_sync` | `0` / `0` / `0` | defaults |
| `stats_dump_period_sec` | `600` | default — sets the 10-minute granularity of everything in §3–§5 |
| `keep_log_file_num` | `1000` | default — why 575 LOGs survived and none were pruned |
| `flush_verify_memtable_count` / `compaction_verify_record_count` / `verify_sst_unique_id_in_manifest` / `force_consistency_checks` | `1` / `1` / `1` / `1` | all defaults; all cost CPU on every flush and compaction |
| `enable_blob_files` | `false` | default — no blob files were ever created (`Blob file count: 0` in every dump) |
| `enable_pipelined_write`, `two_write_queues`, `unordered_write`, `atomic_flush` | `0` | defaults |
| `wal_recovery_mode` | `2` (`kPointInTimeRecovery`) | default |
| `max_total_wal_size` | `0` | default |
| `delete_obsolete_files_period_micros` | `21600000000` (6 h) | default |

`Options.max_background_compactions: -1` and `Options.max_background_flushes: -1` mean
both are derived from `max_background_jobs = 8`, which `DBImpl::GetBGJobLimits` splits as
**2 flush + 6 compaction** slots. That split is confirmed empirically: in the final
phase, compaction thread-seconds / wall-seconds = **6.0×** (§3.3).

---

## 2. The options were **not** constant — five configurations across the run

This is the single biggest trap in the corpus. Scanning every session's option block
yields exactly **5 distinct configurations**:

```bash
cd /workspaces/project/bigdb && python3 - <<'EOF'
import glob,os,re
from collections import defaultdict
keys=['Options.max_background_jobs','Options.max_subcompactions','Options.write_buffer_size',
      'Options.max_write_buffer_number','Options.min_write_buffer_number_to_merge',
      'Options.compaction_style','Options.level_compaction_dynamic_level_bytes',
      'Options.level0_file_num_compaction_trigger','Options.target_file_size_base',
      'Options.max_bytes_for_level_base','Options.row_cache','Options.compression']
combo=defaultdict(list)
for f in ['LOG']+sorted(glob.glob('LOG.old.*')):
    if os.path.getsize(f)==0: continue
    d={}; ts=None
    for i,line in enumerate(open(f,errors='replace')):
        if i>1200: break
        if 'DB SUMMARY' in line and ts is None: ts=line.split()[0]
        m=re.search(r'(Options\.[a-z0-9_]+):\s*(\S.*)$',line)
        if m and m.group(1) in keys and m.group(1) not in d: d[m.group(1)]=m.group(2).strip()
        if 'capacity :' in line and 'blockcache' not in d: d['blockcache']=line.split()[-1]
    if ts: combo[tuple(sorted(d.items()))].append((ts,f))
for k,v in sorted(combo.items(),key=lambda kv:kv[1][0][0]):
    print('='*70); print(f"n={len(v)} first={v[0][0]} ({v[0][1]}) last={v[-1][0]} ({v[-1][1]})")
    for a,b in k: print('   ',a,'=',b)
EOF
```

| Phase | Window | Opens | Keys written (exact, §3.1) | Distinguishing options |
|---|---|---|---|---|
| **A** | 02-26 21:03 → 02-27 12:17 | 310 | 34,374,387,304 | level; `max_background_jobs=32`, `max_subcompactions=1`; everything else as §1.2 |
| **(lock storm)** | 02-26 23:46:22–24 | 239 | – | opens that never got past `LOCK`; no CF options printed |
| **C** | 02-27 12:51 (one open, 3 s) | 1 | 19,860,464 | `optimize_level_style_compaction(4 MiB)` shape: `write_buffer_size=1 MiB`, `target_file_size_base=512 KiB`, `max_bytes_for_level_base=4 MiB`, `level0_file_num_compaction_trigger=2`, `max_write_buffer_number=6`, `min_write_buffer_number_to_merge=2`, per-level compression `NoCompression,NoCompression,Snappy,…`. **Abandoned after 3 s** — see §4.4 |
| **D** | 02-27 12:53 → 16:53 | 5 | 10,649,659,938 | **`compaction_style=kCompactionStyleUniversal`**, `level_compaction_dynamic_level_bytes=0`, `write_buffer_size=256 MiB`, `max_write_buffer_number=6`, `min_write_buffer_number_to_merge=2` — i.e. `optimize_universal_style_compaction(1 GiB)` *(the commented-out call was removed from `rocks.rs` in the round-11 tidy; the effective values are exactly the ones listed in this row)* |
| **E** | 02-27 17:18 → 02-28 20:43 | 19 | 38,098,499,604 | **the final config, identical to §1** — level, `jobs=8`, `sub=4`, `wbuf=64 MiB` |

`row_cache = 1 GiB` and the 32 MiB block cache were constant across **all** phases.

**Only phase E is directly comparable to the current code.** Phase A differs in
`max_background_jobs` (32 vs 8) and `max_subcompactions` (1 vs 4); D differs in
compaction style.

The benchmark's own workload shape was, helpfully, near-constant:

```bash
grep -h "Benchmarking: backend" /workspaces/project/bench_logs/cli_*.log | sed 's/^.*Benchmarking: //' | sort | uniq -c | sort -rn
```
```
294 backend=rocks, timeout=1800s, window_size=100K, batch_size=10.0K
 13 backend=rocks, timeout=3600s, window_size=100K, batch_size=10.0K
 12 backend=rocks, timeout=7200s, window_size=100K, batch_size=10.0K
 11 backend=rocks, timeout=600s,  window_size=100K, batch_size=10.0K
  1 backend=rocks, timeout=600s,  window_size=2.00M, batch_size=100K
```
330 of 331 writing runs used `window_size=100K, batch_size=10.0K`; only the timeout
varied. So cross-session RocksDB numbers are comparable *as workloads*; the confound is
the options in the table above plus the evolving MPT record layout.

---

## 3. Compaction statistics

### 3.1 Database-lifetime totals (exact, rebuilt from `EVENT_LOG_v1`)

Because "Cumulative" resets per open, these were recomputed from the durable event
stream. All are **exact counts over all 575 LOG files**.

```bash
cd /workspaces/project/bigdb && python3 - <<'EOF'
import glob,os,re
rj=re.compile(r'"job": (\d+), "event": "flush_started"')
rf=re.compile(r'"event": "flush_started", "num_memtables": \d+, "num_entries": (\d+), "num_deletes": \d+, "total_data_size": (\d+)')
rt=re.compile(r'"job": (\d+), "event": "table_file_creation", "file_number": \d+, "file_size": (\d+)')
nf=fe=fd=tf=tb=ff=fb=0
for f in ['LOG']+sorted(glob.glob('LOG.old.*')):
    if os.path.getsize(f)==0: continue
    txt=open(f,errors='replace').read().split('\n'); jobs=set()
    for line in txt:
        m=rj.search(line)
        if m:
            jobs.add(m.group(1)); nf+=1
            m2=rf.search(line)
            if m2: fe+=int(m2.group(1)); fd+=int(m2.group(2))
    for line in txt:
        m=rt.search(line)
        if m:
            tf+=1; b=int(m.group(2)); tb+=b
            if m.group(1) in jobs: ff+=1; fb+=b
print(f"flushes                {nf:,}\nmemtable entries       {fe:,}\nmemtable data bytes    {fd:,}")
print(f"SST files created      {tf:,}  (flush {ff:,} / compaction {tf-ff:,})")
print(f"SST bytes written      {tb:,}  (flush {fb:,} / compaction {tb-fb:,})")
print(f"LSM W-Amp = SST/flush  {tb/fb:.2f}")
EOF

# lifetime keys, from the manifest sequence numbers
for f in LOG LOG.old.*; do [ -s "$f" ] || continue; head -400 "$f" | grep -m1 -o "last_sequence is [0-9]*"; done | sort -t' ' -k3 -n | tail -1
grep -o '"largest_seqno": [0-9]*' LOG | sort -t' ' -k2 -n | tail -1
```

| Lifetime metric | Value | How |
|---|---|---|
| **Keys written (sequence numbers consumed)** | **83,142,407,310** | max `largest_seqno` in `LOG`; `last_sequence is 83041940899` at the final open, +100,466,411 in the last session |
| Memtable entries flushed | 83,055,616,531 | sum of `flush_started.num_entries` — cross-checks the above to 0.1 % |
| Memtable data flushed (uncompressed) | 9,086,108,196,736 B = **9,086.11 GB** | sum of `flush_started.total_data_size` |
| **SST files ever created** | **1,165,721** | count of `table_file_creation` |
| — by flush | 145,703 | job id also seen in a `flush_started` |
| — by compaction | 1,020,018 | remainder |
| **SST bytes ever written** | **66,264,913,755,026 B = 66,264.91 GB = 60.27 TiB** | sum of `table_file_creation.file_size` |
| — flush output | 6,005,533,635,417 B = 6,005.53 GB | |
| — compaction output | 60,259,380,119,609 B = 60,259.38 GB | |
| **Lifetime LSM write amplification** (SST bytes / flush bytes) | **11.03** | directly comparable to RocksDB's `Sum … W-Amp` column |
| Flush compression ratio (SST bytes / memtable bytes) | 0.661 | Snappy on the mixed interior+leaf stream |
| Flushes | 145,745 (`flush_started` = `flush_finished`) | |
| Compactions started | 227,444 | `grep -c '"event": "compaction_started"'` |
| Compaction reasons | `LevelMaxLevelSize` 196,330 · `LevelL0FilesNum` 29,122 · `UniversalSortedRunNum` 1,177 · `UniversalSizeRatio` 815 | `grep -h -o '"compaction_reason": "[A-Za-z0-9]*"' LOG LOG.old.* \| sort \| uniq -c` |
| Final on-disk state | 2,321 `.sst` = 128,781,163,719 B (128.78 GB / 119.94 GiB); whole dir 132,133,288,396 B (132.13 GB / **123.06 GiB**), of which 3.34 GB is LOGs | |

**Derived write amplification against user ingest.** RocksDB's `ingest:` counter is only
available per session, but `ingest / keys` is extremely stable across all 36 sessions
that reported it: **95.2 – 102.4 B/key, weighted mean 95.48 B/key** over 55.13e9 keys.

| Derived figure | Value |
|---|---|
| Lifetime user ingest (83,142,407,310 × 95.48 B) | **≈ 7,939 GB ≈ 7.94 TB** *(derived, not read from any line)* |
| SST bytes / user ingest | **≈ 8.35×** |
| (SST + WAL) bytes / user ingest | **≈ 9.35×** — WAL bytes equal ingest bytes exactly in every dump (`Cumulative WAL … written: 143.36 GB` vs `ingest: 143.36 GB`) |

### 3.2 Final cumulative compaction figures — last complete session

`LOG.old.1772311417737349`, dump at `2026/02/28-20:35:03`, uptime 4800.6 s of a 5,270 s
session (the last ~470 s is not in any dump).

```
Uptime(secs): 4800.6 total, 600.0 interval
Cumulative writes: 40M writes, 1504M keys, 3946K commit groups, 10.4 writes per commit group, ingest: 143.36 GB, 30.58 MB/s
Cumulative WAL:    40M writes, 0 syncs, 40999238.00 writes per sync, written: 143.36 GB, 30.58 MB/s
Cumulative stall:  00:45:3.306 H:M:S, 56.3 percent
Interval  stall:   00:04:8.948 H:M:S, 41.5 percent
Flush(GB): cumulative 100.682, interval 12.013
Cumulative compaction: 1374.80 GB write, 293.25 MB/s write, 1372.48 GB read, 292.76 MB/s read, 29070.0 seconds
Interval   compaction:  166.52 GB write, 284.20 MB/s write,  166.17 GB read, 283.60 MB/s read,  3619.1 seconds
```

Derived for that session: W-Amp (write/flush) **13.7**; write/ingest **9.59**;
(write+WAL)/ingest **10.59**. **`0 syncs` — the WAL was never fsynced** (`manual_wal_flush=1`,
`use_fsync=0`, `wal_bytes_per_sync=0`), so none of these numbers include durability cost.

### 3.3 How it evolved — every session that emitted a ≥10 GB stats dump

Values are from each session's **last** stats dump, so each row understates that session
by up to one 600 s interval.

```bash
# uses /tmp/extract.py-style parsing; see §0 helper + the per-session table script
python3 /tmp/dump.py <LOGFILE> -1 | grep -E "^Uptime|^Cumulative (writes|stall|compaction)|^Flush\(GB\)|^Write Stall"
```

| Last-dump ts | Phase | up_s | ingest GB | MB/s | flush GB | comp W GB | comp R GB | comp thread-s | stall % | Sum W-Amp | DB size |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 02-26 21:13:07 | A | 600 | 11.06 | 18.88 | 8.96 | 49.73 | 46.60 | 372 | 0.0 | 5.5 | 3.13 GB |
| 02-26 21:33:09 | A | 600 | 17.39 | 29.68 | 13.14 | 105.01 | 103.48 | 767 | 0.0 | 8.0 | 6.29 GB |
| 02-26 21:53:12 | A | 600 | 23.43 | 39.96 | 17.25 | 142.91 | 141.49 | 1,154 | 0.0 | 8.3 | 9.36 GB |
| 02-26 23:49:55 | A | 1,801 | 69.43 | 39.49 | 50.14 | 452.61 | 450.19 | 4,013 | 0.8 | 9.0 | 17.58 GB |
| 02-27 00:09:57 | A | 1,200 | 55.25 | 47.14 | 39.59 | 301.96 | 300.36 | 4,358 | 23.6 | 7.6 | 19.18 GB |
| 02-27 00:40:29 | A | 1,203 | 70.32 | 59.86 | 50.35 | 445.18 | 442.62 | 6,021 | 2.5 | 8.8 | 22.34 GB |
| 02-27 01:10:28 | A | 1,200 | 73.12 | 62.37 | 52.07 | 464.43 | 462.99 | 7,084 | 2.9 | 8.9 | 24.41 GB |
| 02-27 01:50:31 | A | 1,800 | 99.46 | 56.58 | 70.75 | 663.96 | 661.99 | 10,372 | 10.1 | 9.4 | 27.64 GB |
| 02-27 02:17:25 | A | 1,201 | 77.24 | 65.87 | 54.72 | 511.22 | 515.78 | 9,547 | 11.5 | 9.3 | 29.31 GB |
| *…9 h gap: fd exhaustion, §7.2 — 287 sessions, median 58 s, none reached a dump…* |
| 02-27 10:50:24 | A | 1,800 | 117.13 | 66.63 | 82.37 | 796.77 | 796.77 | 20,829 | 28.6 | 9.7 | 46.38 GB |
| 02-27 11:10:27 | A | 1,201 | 81.30 | 69.34 | 57.04 | 518.12 | 513.52 | 13,484 | 34.6 | 9.1 | 50.98 GB |
| 02-27 11:40:31 | A | 1,201 | 71.75 | 61.19 | 50.25 | 462.79 | 464.18 | 13,813 | 45.7 | 9.2 | 48.18 GB |
| 02-27 12:10:34 | A | 1,200 | 75.85 | 64.71 | 53.11 | 478.79 | 474.61 | 13,606 | 41.5 | 9.0 | 53.05 GB |
| 02-27 12:37:58 | A | 1,204 | 73.00 | 62.11 | 51.14 | 461.84 | 459.85 | 13,600 | 45.0 | 9.0 | 52.58 GB |
| 02-27 13:43:48 | **D** | 3,001 | 190.24 | 64.92 | 135.08 | 1,456.77 | 1,450.72 | 3,925 | **0.0** | 10.8 | 59.74 GB |
| 02-27 14:43:50 | **D** | 3,001 | 195.16 | 66.59 | 138.41 | 1,477.10 | 1,473.45 | 4,064 | **0.0** | 10.7 | 80.76 GB |
| 02-27 15:43:56 | **D** | 3,004 | 195.19 | 66.53 | 138.46 | 1,503.39 | 1,489.14 | 4,219 | **0.0** | 10.9 | 111.57 GB |
| 02-27 16:43:58 | **D** | 3,004 | 190.68 | 65.01 | 135.04 | 1,550.89 | 1,503.23 | 4,107 | **0.0** | 11.5 | **120.80 GB** |
| 02-27 17:13:59 | **D** | 1,202 | 93.52 | 79.64 | 65.69 | 623.77 | 570.96 | 1,810 | **0.0** | 9.5 | 116.03 GB |
| 02-27 18:08:43 | **E** | 3,002 | 132.27 | 45.11 | 92.74 | 1,152.65 | 1,207.54 | 18,295 | 59.4 | 12.4 | 68.82 GB |
| 02-27 19:18:44 | E | 3,601 | 198.99 | 56.59 | 139.54 | 1,544.23 | 1,542.66 | 22,136 | 46.2 | 11.1 | 70.52 GB |
| 02-27 20:18:48 | E | 3,600 | 189.10 | 53.78 | 132.51 | 1,473.81 | 1,472.38 | 22,116 | 50.3 | 11.1 | 71.98 GB |
| 02-27 21:18:52 | E | 3,601 | 184.22 | 52.39 | 129.06 | 1,449.07 | 1,447.78 | 22,080 | 52.6 | 11.2 | 73.25 GB |
| 02-27 22:18:55 | E | 3,600 | 181.90 | 51.74 | 127.34 | 1,433.25 | 1,432.01 | 22,084 | 53.8 | 11.3 | 74.49 GB |
| 02-27 23:08:58 | E | 3,000 | 150.82 | 51.47 | 105.50 | 1,194.67 | 1,193.85 | 18,378 | 53.9 | 11.3 | 75.34 GB |
| 02-28 01:13:30 | E | 7,201 | 357.69 | 50.87 | 250.80 | 2,901.99 | 2,902.27 | 44,122 | 49.6 | 11.6 | 75.18 GB |
| 02-28 03:04:18 | E | 6,614 | 202.72 | 31.38 | 145.60 | 1,901.85 | 1,899.76 | 36,533 | **8.8** | 13.1 | 75.37 GB |
| 02-28 05:14:11 | E | 7,200 | 230.26 | 32.75 | 164.32 | 2,193.74 | 2,187.79 | 40,757 | 9.0 | 13.4 | 81.85 GB |
| 02-28 07:14:18 | E | 7,200 | 236.84 | 33.68 | 168.58 | 2,257.80 | 2,252.58 | 43,005 | 12.4 | 13.4 | 87.07 GB |
| 02-28 09:14:25 | E | 7,201 | 240.61 | 34.22 | 170.95 | 2,287.81 | 2,282.27 | 43,518 | 16.5 | 13.4 | 92.61 GB |
| 02-28 11:04:32 | E | 6,601 | 225.39 | 34.97 | 159.74 | 2,118.88 | 2,112.54 | 39,983 | 22.7 | 13.3 | 98.95 GB |
| 02-28 13:04:38 | E | 6,601 | 212.67 | 32.99 | 150.38 | 1,973.21 | 1,966.29 | 40,095 | 32.7 | 13.1 | 106.25 GB |
| 02-28 15:04:45 | E | 6,601 | 202.28 | 31.38 | 142.77 | 1,902.33 | 1,899.60 | 40,019 | 41.0 | 13.3 | 109.78 GB |
| 02-28 17:04:51 | E | 6,601 | 199.84 | 31.00 | 140.93 | 1,904.35 | 1,901.38 | 40,039 | 44.9 | 13.5 | 112.87 GB |
| 02-28 19:14:57 | E | 7,201 | 194.60 | 27.67 | 137.09 | 1,814.67 | 1,811.88 | 43,350 | 50.6 | 13.2 | 115.66 GB |
| **02-28 20:35:03** | **E** | 4,801 | 143.36 | 30.58 | 100.68 | 1,374.80 | 1,372.48 | 29,070 | **56.3** | **13.7** | **117.97 GB** |

**Trajectory in the final config:** as the DB grew 68.8 → 118.0 GB, `Sum W-Amp` rose
**11.1 → 13.7**, ingest throughput fell **56.6 → 27.7 MB/s** (−51 %), and cumulative
stall rose **8.8 % → 56.3 %**. Write amplification and stall are the two things a future
optimisation has to move.

### 3.4 Phase aggregates

Sums over the sessions that emitted a ≥10 GB dump (coverage stated, since phase A is
badly under-sampled):

| Phase | Sessions in sum | Key coverage of the phase | uptime h | ingest GB | flush GB | comp write GB | comp read GB | comp kilo-thread-h | stall h | stall % | W-Amp |
|---|---|---|---|---|---|---|---|---|---|---|---|
| A level, jobs=32, sub=1 | 14 of 310 | 11.93e9 / 34.37e9 = **35 %** | 4.67 | 915.7 | 650.9 | 5,855.3 | 5,834.4 | 33.1 | 0.89 | 19.0 | 9.00 |
| C tiny-file experiment | 0 of 1 | 0 % (3 s session) | – | – | – | – | – | – | – | – | – |
| **D universal** | 5 of 5 | **100 %** | 3.67 | 864.8 | 612.7 | 6,611.9 | 6,487.5 | **5.0** | **0.00** | **0.0** | 10.79 |
| **E level (final)** | 17 of 19 | **100 %** | 26.17 | 3,483.6 | 2,458.5 | 30,879.1 | 30,885.1 | **157.1** | **9.24** | **35.3** | 12.56 |

Aggregate device traffic implied (compaction read+write, plus WAL = ingest):

| Phase | compaction R+W | over | = | + WAL → total |
|---|---|---|---|---|
| A | 11,690 GB | 16,812 s | 695 MB/s | 750 MB/s |
| D | 13,099 GB | 13,212 s | **991 MB/s** | 1,057 MB/s |
| E | 61,764 GB | 94,212 s | 656 MB/s | 693 MB/s |

Phase E ran ~6 concurrent compactions (157,100 thread-s / 26.17 h wall = **6.0×**),
exactly the `max_background_jobs=8 → 6 compaction + 2 flush` split. Phase D ran at
**1.4×** concurrency and still moved more bytes per second, because it did **837**
compactions averaging 1.85 GB each versus phase E's **13,828** averaging 0.16 GB
(`Comp(cnt)` / `Write(GB)` on the `Sum` row). Per-compaction-thread read rate was
**374.8 MB/s (D)** vs **55.0 MB/s (E)**. That contrast is recorded, not explained — see
§8.

---

## 4. Stall accounting

### 4.1 The stall counter breakdown

```bash
python3 /tmp/dump.py LOG.old.1772311417737349 -1 | grep -E "^Cumulative stall|^Interval stall|^Write Stall"
```

Final complete session (`2026/02/28-20:35:03`, 4800.6 s uptime):

```
Cumulative stall: 00:45:3.306 H:M:S, 56.3 percent
Interval stall:   00:04:8.948 H:M:S, 41.5 percent
Write Stall (count): cf-l0-file-count-limit-delays-with-ongoing-compaction: 5,
  cf-l0-file-count-limit-stops-with-ongoing-compaction: 0, l0-file-count-limit-delays: 5,
  l0-file-count-limit-stops: 0, memtable-limit-delays: 0, memtable-limit-stops: 98,
  pending-compaction-bytes-delays: 7215, pending-compaction-bytes-stops: 0,
  total-delays: 7220, total-stops: 98
Write Stall (count): write-buffer-manager-limit-stops: 0
```

| Cause | Events in the final session | Share of delays |
|---|---|---|
| **`pending-compaction-bytes-delays`** | **7,215** | **99.93 %** |
| `l0-file-count-limit-delays` (all "with ongoing compaction") | 5 | 0.07 % |
| `memtable-limit-stops` (hard stops) | 98 | — all of `total-stops` |
| `pending-compaction-bytes-stops` (hard, 256 GiB) | **0** | never reached |
| `write-buffer-manager-limit-stops` | 0 | no WBM budget set |

**The database was throttled almost exclusively by
`soft_pending_compaction_bytes_limit` (64 GiB), i.e. compaction debt**, not by L0 file
count and not by memtable pressure. Hard stops came only from
`max_write_buffer_number = 2` filling both 64 MiB memtables.

### 4.2 Summed over every session's last dump (lower bound)

```bash
# sums the "Write Stall (count):" line from the last dump of all 335 successful sessions
```

| Counter | Lifetime sum (lower bound) |
|---|---|
| `pending-compaction-bytes-delays` | **113,114** |
| `l0-file-count-limit-delays` | 3,375 (of which 3,283 "with ongoing compaction") |
| `memtable-limit-stops` | 6,157 |
| `l0-file-count-limit-stops` | 1 |
| `memtable-limit-delays` | 0 |
| `pending-compaction-bytes-stops` | 0 |
| `total-delays` / `total-stops` | 116,489 / 6,158 |
| Summed `Cumulative stall` time | 36,465 s = **10.13 h** over 124,444 s of dump-covered uptime = **29.3 %** |

The raw WARN lines give an independent, dump-independent count:

```bash
grep -h "Stalling writes\|Stopping writes" LOG LOG.old.* | sed 's/^[0-9\/:.-]* [0-9]* //' \
  | sed 's/[0-9][0-9]*/N/g' | sort | uniq -c | sort -rn
```
```
 116774  Stalling writes because of estimated pending compaction bytes N rate N
  18710  Stalling writes because we have N level-N files rate N
  17582  Stopping writes because we have N immutable memtables (waiting for flush), max_write_buffer_number is set to N
   2065  Stalling writes because we have N immutable memtables (waiting for flush), ... rate N
    436  Stopping writes because we have N level-N files
```

### 4.3 What the throttle actually did

Parsing the `estimated pending compaction bytes N rate N` messages across all LOGs:

| Quantity | Value |
|---|---|
| Events | 116,774 |
| Estimated pending compaction bytes: min / median / p95 / **max** | 68,719,477,885 / 68,961,635,334 / 69,935,458,202 / **271,667,645,240 (253.0 GiB)** |
| Soft limit / hard limit | 64 GiB / 256 GiB — the peak came within **1.2 %** of the hard stop |
| Most common applied write rate | **16,777,216 B/s (16.8 MB/s)** — 50,489 events; i.e. `delayed_write_rate` at full value |
| Rates seen, decaying | 16.8 → 13.4 → 10.7 → 8.6 MB/s …; on L0 stalls as low as **16,384 B/s (16 KB/s)** (795 events) |

The benchmark wanted 30–66 MB/s (§3.3). RocksDB clamped it to ≤16.8 MB/s whenever the
compaction backlog exceeded 64 GiB. **That is the entire stall story.**

L0 file counts at which stalls fired (`slowdown` trigger 20, `stop` trigger 36):

| L0 files | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 28 | 29 | 30 | 31 | 32 | 33 | 34 | 85 (STOP) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| events | 9,172 | 3,580 | 1,425 | 812 | 654 | 501 | 428 | 340 | 296 | 233 | 166 | 118 | 101 | 45 | 839 | 436 |

### 4.4 Stall is backlog-driven, not size-driven

Session `LOG.old.1772248451147453` (02-28 01:14 → 03:14) is the counterexample that
pins down the mechanism — cumulative stall *fell* from 35.2 % to 8.8 % **while the DB
stayed the same size**, because the backlog drained:

```bash
python3 /tmp/series.py LOG.old.1772248451147453   # per-dump time series
```

| dump ts | up_s | interval stall % | interval comp write GB | DB size | L6 files |
|---|---|---|---|---|---|
| 01:24:18 | 614 | 36.0 | 209.76 | 79.18 GB | 1315 |
| 01:34:18 | 1,214 | 36.1 | 198.00 | 79.52 GB | 1324 |
| 01:44:18 | 1,814 | 20.7 | 193.12 | 79.77 GB | 1330 |
| 01:54:18 | 2,414 | **3.8** | 187.40 | 79.60 GB | 1349 |
| 02:04:18 | 3,014 | **0.0** | 178.18 | 76.86 GB | 1325 |
| 02:14:18 | 3,614 | **0.0** | 171.57 | 73.52 GB | 1248 |
| … | … | 0.0 | … | … | … |
| 03:04:18 | 6,614 | **0.0** | 148.27 | 75.37 GB | 1205 |

Conversely, the last complete session started at **59.7 %** interval stall in its first
10 minutes and only fell to 41.5 % — it inherited a saturated backlog from its
predecessor. Restarting the process does **not** reset compaction debt: the first thing
`LOG` logs after recovery is

```
[WARN] [db/column_family.cc:1048] [default] Stalling writes because of estimated pending compaction bytes 69181865908 rate 16777216
```

**94 ms after the DB opened.**

### 4.5 The two configurations that produced zero stalls

* **Phase D (universal compaction)**: `Cumulative stall: 00:00:0.000, 0.0 percent` and
  `total-delays: 0, total-stops: 0` in all 5 sessions — at the cost of the DB inflating
  to **120.80 GB across 1,922 files spread over L3(15.66 GB)+L4(21.73 GB)+L5(33.99 GB)+L6(48.54 GB)**,
  roughly 1.7× the space level compaction needed for the same data (68.82 GB one session
  later, after switching back).
* **Phase C (1 MiB memtables, 512 KiB SSTs)**: catastrophic — L0 hit **85 files** and
  `Stopping writes because we have 85 level-0 files` fired 436 times in a 3-second
  session. Abandoned immediately.

---

## 5. Per-level tables

Command: `python3 /tmp/dump.py <LOGFILE> -1 | sed -n '/Compaction Stats/,/^ Int/p'`

### 5.1 Final LSM shape — session 574 (`LOG`), open dump `2026/02/28-20:43:38`

This is the **most recent snapshot of the tree**. Read/write columns are zero because
the session had just started; the `Score` column is what matters.

```
Level    Files   Size     Score Read(GB)  Rn(GB) Rnp1(GB) Write(GB) ... W-Amp Comp(sec) Comp(cnt)
  L0      6/6   197.52 MB   0.0 ...
  L3      8/8   424.83 MB   0.0 ...
  L4     43/5    2.23 GB   14.1 ...
  L5    279/25  15.95 GB   12.6 ...
  L6   1946/0   99.24 GB    0.0 ...
 Sum   2282/44  118.03 GB   0.0 ...
```

L1 and L2 are absent (`level_compaction_dynamic_level_bytes = 1`; base level is L3).
**`Score` 14.1 at L4 and 12.6 at L5** means those levels were 12–14× over their target
size — the LSM was, at the moment the benchmark stopped, carrying ~12× its intended
compaction debt.

### 5.2 Last full accounting — session 573, dump `2026/02/28-20:35:03` (uptime 4800.6 s)

```
Level    Files   Size     Score Read(GB)  Rn(GB) Rnp1(GB) Write(GB) Wnew(GB) Moved(GB) W-Amp Rd(MB/s) Wr(MB/s) Comp(sec) CompMergeCPU(sec) Comp(cnt) Avg(sec) KeyIn KeyDrop
  L0      5/0   177.49 MB  12.5      2.4     0.0      2.4     103.0    100.7       0.0   1.0      3.4    149.9    704.07            503.82      2946    0.239     23M    987
  L3      8/1   528.64 MB  10.4    322.9   100.9    222.0     322.8    100.7       0.0   3.2    378.2    378.0    874.27           2181.55       463    1.888   3282M  1725K
  L4     37/4    2.01 GB  12.6    226.9    95.7    131.2     226.1     94.9       5.1   2.4     53.8     53.6   4315.61           1784.70      1787    2.415   2304M  7870K
  L5    278/10  16.20 GB  13.7    442.2   100.2    342.0     434.1     92.1       0.4   4.3     36.2     35.5  12507.93           3874.55      1920    6.515   4492M    82M
  L6   1933/17  99.07 GB   0.0    378.2    90.9    287.3     288.8      1.5       0.0   3.2     36.3     27.7  10668.17           3069.12      1655    6.446   3840M   907M
 Sum   2261/32  117.97 GB   0.0   1372.5   387.6    984.9    1374.8    389.9       5.5  13.7     48.3     48.4  29070.04          11413.74      8771    3.314     13G   999M
 Int      0/0    0.00 KB   0.0    166.2    45.9    120.3     166.5     46.2       0.6  13.9     47.0     47.1   3619.07           1381.04      1051    3.443   1690M   118M

Priority table:
 Low      0/0    0.00 KB   0.0   1372.5   387.6    984.9    1274.1    289.2       0.0   0.0     49.5     46.0  28383.73          10924.57      5836    4.864     13G   999M
High      0/0    0.00 KB   0.0      0.0     0.0      0.0     100.7    100.7       0.0   0.0      0.0    150.2    686.18            489.16      2934    0.234       0      0
User      0/0    0.00 KB   0.0      0.0     0.0      0.0       0.0      0.0       0.0   0.0      0.0     76.1      0.14              0.00         1    0.136       0      0
```

Notes: `High` = flushes (2,934 flushes, 100.7 GB, 686 thread-s); `Low` = background
compaction (5,836 jobs, 1,274.1 GB, 28,384 thread-s = 5.9× concurrency); `User` = the one
manual/recovery compaction. **907M keys dropped at L6** and 82M at L5 — the workload
overwrites heavily, which is why 143 GB of ingest becomes 100.7 GB of flush and the DB
grows only ~2 GB per session.

### 5.3 An earlier session in the same configuration — `LOG.old.1772255657743249`, dump `2026/02/28-05:14:11` (uptime 7200.5 s, DB 36 GB smaller)

```
  L0      2/0   71.90 MB   0.5      4.3     0.0      4.3     168.6    164.3       0.0   1.0      3.8    147.8   1167.90            843.64      4733    0.247     43M   1888
  L3      5/0   222.07 MB  0.9    471.1   164.4    306.7     470.4    163.7       0.0   2.9    393.5    393.0   1225.82           3194.02       744    1.648   4808M  6248K
  L4     17/3  1007.07 MB 10.1    344.1   152.1    192.0     342.4    150.4      11.7   2.3     89.6     89.1   3934.38           2370.30      2692    1.462   3513M    17M
  L5    147/17   8.45 GB   9.9    721.6   161.7    559.9     704.9    145.0       0.6   4.4     42.2     41.2  17500.44           5928.80      3028    5.780   7361M   172M
  L6   1309/13  72.14 GB   0.0    646.8   144.5    502.3     507.4      5.1       0.0   3.5     39.1     30.7  16928.34           5178.60      2631    6.434   6619M  1426M
 Sum   1480/33  81.85 GB   0.0   2187.8   622.6   1565.2    2193.7    628.5      12.3  13.4     55.0     55.1  40756.89          17515.36     13828    2.947     22G  1622M
Cumulative writes: 154M writes, 2405M keys, ..., ingest: 230.26 GB, 32.75 MB/s
Cumulative stall: 00:10:51.454 H:M:S, 9.0 percent
```

Between these two sessions (81.85 → 117.97 GB): **L6 grew 72.14 → 99.07 GB and
1,309 → 1,933 files**, L5 grew 8.45 → 16.20 GB, L5 `Score` went 9.9 → 13.7, `Sum W-Amp`
13.4 → 13.7, and stall 9.0 % → 56.3 %.

### 5.4 LSM growth across the whole run

Each cell is `files / size`, from the last dump of the named session.

| Last-dump ts | Phase | L0 | L3 | L4 | L5 | L6 | Sum |
|---|---|---|---|---|---|---|---|
| 02-26 21:13:07 | A | 2 / 80.98M | – | 3 / 198.54M | 6 / 319.62M | 46 / 2.55G | 57 / 3.13 GB |
| 02-26 23:49:55 | A | 7 / 254.43M | – | 8 / 463.02M | 40 / 2.21G | 278 / 14.66G | 333 / 17.58 GB |
| 02-27 01:50:31 | A | 7 / 751.67M | – | 12 / 723.42M | 47 / 2.59G | 445 / 23.61G | 511 / 27.64 GB |
| 02-27 10:50:24 | A | 15 / 1.90G | 31 / 2.93G | 3 / 351.54M | 65 / 3.68G | 694 / 37.52G | 808 / 46.38 GB |
| 02-27 12:37:58 | A | 13 / 3.42G | 21 / 2.46G | 4 / 513.60M | 92 / 5.32G | 823 / 40.88G | 953 / 52.58 GB |
| 02-27 13:43:48 | D | 0 | 28 / 1.74G | 40 / 2.30G | 205 / 12.77G | 687 / 42.94G | 960 / 59.74 GB |
| 02-27 16:43:58 | D | 1 / 895.08M | 252 / 15.66G | 349 / 21.73G | 544 / 33.99G | 776 / 48.54G | **1922 / 120.80 GB** |
| 02-27 18:08:43 | E | 11 / 387.85M | 13 / 679.82M | 39 / 2.02G | 246 / 14.22G | 1166 / 51.54G | 1475 / 68.82 GB |
| 02-27 22:18:55 | E | 9 / 316.24M | 12 / 580.60M | 38 / 2.12G | 247 / 14.13G | 1242 / 57.37G | 1548 / 74.49 GB |
| 02-28 01:13:30 | E | 2 / 70.77M | 11 / 514.60M | 26 / 1.42G | 204 / 11.92G | 1319 / 61.27G | 1562 / 75.18 GB |
| 02-28 05:14:11 | E | 2 / 71.90M | 5 / 222.07M | 17 / 1007.07M | 147 / 8.45G | 1309 / 72.14G | 1480 / 81.85 GB |
| 02-28 09:14:25 | E | 3 / 107.45M | 8 / 334.24M | 23 / 1.14G | 165 / 9.59G | 1500 / 81.46G | 1699 / 92.61 GB |
| 02-28 13:04:38 | E | 9 / 501.38M | 10 / 544.32M | 37 / 1.94G | 251 / 14.60G | 1754 / 88.68G | 2061 / 106.25 GB |
| 02-28 17:04:51 | E | 3 / 285.30M | 11 / 659.32M | 41 / 2.30G | 258 / 14.79G | 1873 / 94.86G | 2186 / 112.87 GB |
| 02-28 20:35:03 | E | 5 / 177.49M | 8 / 528.64M | 37 / 2.01G | 278 / 16.20G | 1933 / 99.07G | 2261 / 117.97 GB |
| **02-28 20:43:38** | **E** | 6 / 197.52M | 8 / 424.83M | 43 / 2.23G | 279 / 15.95G | **1946 / 99.24G** | **2282 / 118.03 GB** |

The 02-27 16:43 row is the universal-compaction space blow-up; one session later,
level compaction had ground it back to 68.82 GB (at the cost of 59.4 % stall in that
session).

---

## 6. Flush statistics and write sessions

| Metric | Value | Source |
|---|---|---|
| **Flushes, lifetime** | **145,745** | `grep -c '"event": "flush_started"'` across all LOGs (`flush_finished` matches exactly) |
| SST files produced by flush | 145,703 | job-id correlation, §3.1 |
| Flush output bytes, lifetime | 6,005,533,635,417 = **6,005.53 GB** | |
| Mean flush output per file | 41.2 MB | 6,005.53 GB / 145,703 — consistent with a 64 MiB memtable at Snappy 0.661 |
| Memtable entries flushed | 83,055,616,531 | |
| Memtable bytes flushed (uncompressed) | 9,086.11 GB | |
| Last complete session | 3,203 `flush_started`; `Flush(GB): cumulative 100.682`; `High` priority 2,934 jobs / 686.18 thread-s | |
| Flush thread utilisation, last session | 686.18 s / 4800.6 s = **0.14×** of one thread | flushes were never the bottleneck |

**Write sessions:**

| Count | Meaning |
|---|---|
| **574** | `DB::Open()` attempts recorded (one LOG file each) |
| **335** | opens that succeeded |
| **239** | opens that failed on `LOCK` (all in a 2-second window on 02-26 23:46) |
| **331** | successful sessions that advanced the manifest `last_sequence` (i.e. actually wrote) |
| **36** | sessions long enough to emit a stats dump reporting ≥10 GB of ingest |
| **318 / 17** | clean shutdown / killed |
| **338 / 330** | `bench_logs/cli_*.log` / `bench_logs/log_*.csv` — the benchmark-side count, consistent with 335 successful opens |

WAL/recovery cost per open:

```bash
# WAL size at open + recovery_started→recovery_finished duration, per session
grep -h "Write Ahead Log file in" LOG LOG.old.* | sed 's/.*size: //'
```

| Metric | Value |
|---|---|
| WAL bytes replayed at open, total over 335 opens | 15,789,303,050 = **15.79 GB** |
| WAL size at open: median / max | 52.0 MB / **1,740.0 MB** |
| `recovery_started` → `recovery_finished`: median / max / total | 0.50 s / 13.61 s / 188.2 s |
| Opens with a corrupt WAL tail | 10 (`dropping N bytes; Corruption: truncated record body` / `error reading trailing data`) |

---

## 7. Other things a future benchmark should compare against

### 7.1 The block cache was 100 % full for the whole run

```bash
python3 /tmp/dump.py <LOGFILE> -1 | grep "Block cache"
```

| Session (last dump) | capacity | usage | entry breakdown |
|---|---|---|---|
| 02-27 18:08 | 32.00 MB | 31.29 MB | |
| 02-28 01:13 | 32.00 MB | 30.94 MB | |
| 02-28 09:14 | 32.00 MB | 31.17 MB | |
| 02-28 15:04 | 32.00 MB | 31.81 MB | |
| **02-28 20:35** | **32.00 MB** | **31.73 MB** | `DataBlock(7921, 31.02 MB, 96.93%) Misc(7, 24.05 KB, 0.073%)` |

97 % of a 32 MiB cache spent on data blocks, pinned at capacity, against a 118 GB
database — a **0.027 %** cache-to-data ratio. This is the number the current code's
1 GiB block cache is meant to move. The 1 GiB **row** cache never appears in any cache
statistic in any LOG (row cache is only consulted from `TableCache::Get`/`MultiGet`).

### 7.2 A 9-hour window destroyed by fd exhaustion (`max_open_files = -1`)

```bash
grep -h "Too many open files" LOG LOG.old.* | awk '{print $1}' | sort | sed -n '1p;$p'
grep -h "\[ERROR\]" LOG LOG.old.* | sed 's/^[0-9\/:.-]* [0-9]* //' | sed 's/[0-9][0-9]*/N/g' | sort | uniq -c | sort -rn
```

```
first: 2026/02/27-01:57:21.180418      last: 2026/02/27-10:10:34.918847
 239  Waiting after background compaction error: IO error: While open a file for random read: bigdb/N.sst: Too many open files
  61  Waiting after background compaction error: IO error: While open a file for appending: ...
  57  OpenCompactionOutputFiles for table #N fails at NewWritableFile with status IO error: ... Too many open files
  43  Waiting after background flush error: ... Too many open files
  21  (other flush/WAL variants)
```

| Window 2026/02/27 01:50 → 10:25 | |
|---|---|
| LOG files (all successful opens) | **287** |
| Opens that hit `Too many open files` | **282** |
| Median session duration | **58 s** (max 1,801 s) |
| Wall span | 32,392 s = 9.00 h; DB open for 97 % of it, but crash-looping |
| Sessions in the window reaching a 600 s stats dump | **0** |

`max_open_files = -1` keeps every SST fd open; with ~700–1,000 live SSTs plus compaction
I/O the process exceeded its fd rlimit. `max_bgerror_resume_count = 2147483647` made
RocksDB retry forever rather than fail fast. There is a commented-out
`// options.set_max_open_files(512);` at `rocks.rs:217` — this window is the empirical
case for it. **Any throughput number from phase A between 01:57 and 10:10 is worthless.**

### 7.3 Durability was effectively off

`use_fsync: 0`, `wal_bytes_per_sync: 0`, `bytes_per_sync: 0`, `manual_wal_flush: 1`, and
`Cumulative WAL: … 0 syncs` in every single stats dump. No benchmark number here
includes an fsync. 17 sessions were SIGKILLed and 10 opens recovered a truncated WAL
tail; `wal_recovery_mode: 2` (point-in-time) silently dropped those tails.

### 7.4 Non-stall warnings worth knowing about

```
  5  [WARN] [db/column_family.cc:N] level_compaction_dynamic_level_bytes only makes sense for level-based compaction
```
— exactly the 5 universal-compaction sessions of phase D; the option was left at its
default `1` while the style was switched.

### 7.5 Manifest / file numbers at the end

```
Recovered from manifest file:bigdb/MANIFEST-1285725 succeeded, manifest_file_number is 1285725,
next_file_number is 1319636, last_sequence is 83041940899, log_number is 1319631
```
Final live manifest `MANIFEST-1319646` (648,517 B), one WAL `1321613.log` (15,728,640 B),
`SST files in bigdb dir, Total Num: 2327` at the last open vs **2,282 live in the LSM**
vs **2,321 on disk now** (the difference is obsolete files pending the 6-hour
`delete_obsolete_files_period_micros`).

---

## 8. Gaps and caveats

1. **No database-lifetime `Cumulative` line exists.** All lifetime figures in §3.1 are
   reconstructions from `EVENT_LOG_v1` and manifest sequence numbers. They are exact,
   but they are not quotable as "RocksDB said X".
2. **Lifetime user ingest (≈7,939 GB / 7.94 TB) is derived**, not measured: keys ×
   95.48 B/key. The per-key figure is stable (95.2–102.4 B/key over 36 sessions) but the
   record layout changed during the run, so the true value could differ by a few percent.
   Everything downstream of it (`SST/ingest ≈ 8.35`, `(SST+WAL)/ingest ≈ 9.35`) inherits
   that uncertainty. The **exact** amplification number is `SST bytes / flush bytes =
   11.03`.
3. **Phase A is only 35 % covered by stats dumps** (11.93e9 of 34.37e9 keys). Its phase
   aggregates in §3.4 are lower bounds. Phases D and E are 100 % covered.
4. **Every session's last dump misses its final partial interval** (up to 600 s). For the
   last complete session that is 470 s of 5,270 s (~9 %).
5. **`File Read Latency Histogram By Level` is empty in every dump** — `Options.statistics: (nil)`
   and `report_bg_io_stats: 0`. There is **no read-latency, cache-hit-rate,
   bytes-read, or per-operation histogram data anywhere in the corpus.** A future run
   should set a `Statistics` object; this record cannot supply a read-side baseline.
6. **The phase D vs phase E compaction-efficiency gap is unexplained.** Universal
   compaction moved 991 MB/s of device traffic at 1.4× thread concurrency; level
   compaction moved 656 MB/s at 6.0×. Compaction size (1.85 GB vs 0.16 GB per job) is
   *correlated* with it, but I have no measurement isolating page-cache effects, device
   queue depth, or the `compaction_verify_record_count` CPU cost. Recorded as an
   observation only.
7. **I did not determine why the run ended** at 2026/02/28 20:48:15 — the last LOG line
   is an ordinary `table_file_creation`, with no shutdown and no error.
8. Cross-session throughput comparisons are confounded by simultaneous changes to the
   MPT record layout (see the git history of `src/mpt/storage/rocks.rs`). The RocksDB
   options are pinned by §2; the application-side record format is not.


# Benchmark performance history

> **This is a BEFORE baseline.** Every run below predates the optimisations recorded in
> `REVIEW.md` §4 ("Landed on `perf/rocksdb-improvements`"). `git log` confirms `src/` was
> unchanged from commit `005a773` (2026-02-26 21:35 UTC) until after the last run started;
> the only later commit in the window, `05a34a8` "Tweaks" (2026-02-28 20:49:45 UTC, which
> sets `max_subcompactions(4)` / `max_background_jobs(8)`), landed **six minutes after** the
> final run began at 20:43:37, so **no run in this history contains it**. Do not quote these
> numbers as current performance.

## 0. What was run

All 575 runs used the same configuration except one aborted experiment:

| Parameter | Value |
|---|---|
| backend | `rocks` (`RocksTransRelMPT`, `OptimisticTransactionDB`) |
| `db_dir` | `bigdb` (single database, grown continuously across all runs) |
| `window_size` | 100 000 entries generated, sorted, then chunked |
| `batch_size` | 10 000 entries per `batch_upsert` → 10 batches per window |
| `timeout` | 600 s → 1800 s → 3600 s → 7200 s (raised over the campaign) |
| exception | one run at `window=2000000 batch=100000` (`cli_20260226_230653.log`) was killed before producing results |

Campaign span: **2026-02-26 21:03:05 → 2026-02-28 20:43:37 UTC** (~47.7 h wall).

| Metric | Value |
|---|--:|
| `bench_loop` sessions (each rebuilds the release binary) | 18 |
| Runs launched | 575 |
| Runs that finished and printed results (`OK`) | 35 |
| Runs that crashed (`FAILED`) | 522 |
| Runs interrupted before `bench_loop` recorded an end | 18 |
| Wall time in runs with a recorded end | 41.77 h |
| Wall time in the 35 `OK` runs | 33.93 h |
| Entries inserted across **all** logged runs (sum of last `total_inserted` over 294 non-empty CSVs) | **978 620 000** |
| Entries inserted by the 35 `OK` runs alone | 801 690 000 |
| Entries inserted by the 252 crashed runs that got as far as writing a CSV | 108 090 000 |
| RocksDB `estimate-num-keys` at the final open (2026-02-28 20:44:23) | **999 502 643** |
| Final on-disk size: 2 321 SST files | 128 781 163 719 B (119.94 GiB) |
| Final on-disk size: whole `bigdb/` incl. 3.11 GiB of `LOG*` | 132 133 808 588 B (123.06 GiB) |
| Derived: SST bytes per estimated key at the end | 128.8 B |

Notes on the size proxy:

- The "keys at open" column everywhere below is the value the process logs as
  `approx entries N`. It is `RocksStorage::approximate_entry_count()` →
  `rocksdb.estimate-num-keys` (`src/mpt/storage/rocks.rs:292`), i.e. **an estimate of all
  keys in the column family** (leaves + persisted interior nodes + metadata), not an exact
  leaf count. Across the campaign it tracks cumulative inserts to within a few percent
  (978.6 M inserted vs 999.5 M estimated at the end) but individual run-to-run deltas are
  noisy in both directions.
- Peak-RSS strings come from `tools/bench_loop.py:format_rss`, which divides kB by
  `1_048_576` and labels the result `GB`. **They are GiB.** The tables keep the log's own
  labels; 42.21 "GB" = 42.21 GiB = 45.3 GB.

## 1. Every `=== Benchmark Results ===` block, in order

35 completed runs. `steady /s` is the insertion rate over the last 20 % of the run's own
CSV (see §2 — the whole-run `throughput` figure is depressed by a cold start that gets
worse as the DB grows, so the two columns diverge more and more).

| # | run start (UTC) | keys at open | depth | timeout | inserted | throughput /s | steady /s | p50 | p95 | p99 | mean | min | max | init s | peak RSS |
|--:|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 1 | 2026-02-26 21:03:07 | 1 | 0 | 600 s | 26.5M | 44,139 | 31,671 | 250 ms | 343 ms | 405 ms | 226 ms | 17 ms | 2.42 s | 0.0 | 7.61 GB |
| 2 | 2026-02-26 21:13:08 | 26,817,331 | 19 | 600 s | 14.2M | 23,763 | 24,407 | 384 ms | 637 ms | 890 ms | 420 ms | 287 ms | 4.79 s | 3.0 | 6.57 GB |
| 3 | 2026-02-26 21:23:09 | 40,988,835 | 20 | 600 s | 13.4M | 22,445 | 22,289 | 429 ms | 527 ms | 609 ms | 445 ms | 362 ms | 974 ms | 4.9 | 6.52 GB |
| 4 | 2026-02-26 21:33:10 | 54,431,580 | 20 | 600 s | 12.3M | 20,671 | 20,406 | 468 ms | 582 ms | 729 ms | 483 ms | 405 ms | 1.19 s | 5.1 | 6.53 GB |
| 5 | 2026-02-26 21:43:11 | 66,710,405 | 20 | 600 s | 11.5M | 19,297 | 20,661 | 498 ms | 655 ms | 756 ms | 517 ms | 419 ms | 1.37 s | 5.2 | 5.98 GB |
| 6 | 2026-02-26 21:53:13 | 77,238,108 | 20 | 600 s | 11.0M | 18,443 | 20,207 | 506 ms | 738 ms | 1.00 s | 541 ms | 434 ms | 1.90 s | 5.2 | 6.05 GB |
| 7 | 2026-02-26 22:03:14 | 87,902,126 | 20 | 600 s | 10.3M | 17,322 | 19,340 | 531 ms | 825 ms | 1.20 s | 576 ms | 439 ms | 2.37 s | 5.3 | 6.09 GB |
| 8 | 2026-02-26 22:52:18 | 99,373,547 | 20 | 600 s | 9.67M | 16,298 | 19,390 | 550 ms | 967 ms | 1.45 s | 613 ms | 451 ms | 2.46 s | 7.1 | 6.10 GB |
| 9 | 2026-02-26 23:19:54 | 125,378,232 | 20 | 1800 s | 22.4M | 12,463 | 8,806 | 713 ms | 1.19 s | 1.50 s | 801 ms | 470 ms | 11.19 s | 5.3 | not recorded |
| 10 | 2026-02-27 00:20:26 | 168,736,849 | 21 | 1800 s | 25.9M | 14,511 | 14,539 | 589 ms | 1.22 s | 1.71 s | 688 ms | 460 ms | 5.47 s | 12.1 | 11.30 GB |
| 11 | 2026-02-27 00:50:28 | 193,273,528 | 21 | 1800 s | 22.9M | 12,795 | 13,347 | 693 ms | 1.42 s | 2.06 s | 780 ms | 481 ms | 3.92 s | 9.8 | 11.40 GB |
| 12 | 2026-02-27 01:20:30 | 215,671,111 | 21 | 1800 s | 19.3M | 10,788 | 12,831 | 793 ms | 1.60 s | 3.06 s | 926 ms | 494 ms | 15.23 s | 10.0 | 11.54 GB |
| 13 | 2026-02-27 01:57:24 | 241,927,454 | 21 | 1800 s | 19.4M | 10,875 | 12,305 | 785 ms | 1.49 s | 2.80 s | 918 ms | 521 ms | 57.06 s | 11.6 | 11.48 GB |
| 14 | 2026-02-27 10:20:24 | 368,930,979 | 21 | 1800 s | 12.2M | 6,839 | 11,171 | 1.02 s | 2.51 s | 6.22 s | 1.46 s | 611 ms | 54.32 s | 8.9 | 11.71 GB |
| 15 | 2026-02-27 10:50:26 | 376,900,157 | 21 | 1800 s | 11.1M | 6,195 | 6,891 | 1.10 s | 3.24 s | 14.85 s | 1.61 s | 591 ms | 35.10 s | 10.0 | 11.75 GB |
| 16 | 2026-02-27 11:20:30 | 388,119,920 | 21 | 1800 s | 9.49M | 5,300 | 6,388 | 1.18 s | 3.55 s | 20.17 s | 1.89 s | 619 ms | 55.83 s | 9.7 | 11.20 GB |
| 17 | 2026-02-27 12:53:47 | 407,602,462 | 21 | 3600 s | 24.9M | 6,928 | 8,115 | 1.40 s | 2.26 s | 2.76 s | 1.44 s | 649 ms | 4.07 s | 10.4 | 14.52 GB |
| 18 | 2026-02-27 13:53:49 | 435,641,384 | 21 | 3600 s | 23.2M | 6,470 | 6,651 | 1.49 s | 2.56 s | 3.30 s | 1.54 s | 670 ms | 4.30 s | 13.5 | 14.38 GB |
| 19 | 2026-02-27 14:53:52 | 461,813,841 | 21 | 3600 s | 22.8M | 6,347 | 7,916 | 1.50 s | 2.60 s | 3.82 s | 1.57 s | 723 ms | 6.13 s | 16.1 | 14.65 GB |
| 20 | 2026-02-27 15:53:54 | 488,741,273 | 21 | 3600 s | 20.0M | 5,587 | 5,922 | 1.71 s | 2.83 s | 3.60 s | 1.79 s | 750 ms | 4.12 s | 15.6 | 14.57 GB |
| 21 | 2026-02-27 17:18:41 | 516,926,996 | 21 | 3600 s | 12.4M | 3,456 | 5,836 | 2.05 s | 4.79 s | 25.05 s | 2.89 s | 717 ms | 220.22 s | 18.5 | 12.10 GB |
| 22 | 2026-02-27 18:18:43 | 526,175,436 | 21 | 3600 s | 15.8M | 4,386 | 5,668 | 2.01 s | 4.83 s | 5.59 s | 2.28 s | 714 ms | 6.59 s | 11.7 | 11.95 GB |
| 23 | 2026-02-27 19:18:48 | 539,785,953 | 21 | 3600 s | 14.1M | 3,927 | 5,096 | 2.37 s | 5.08 s | 6.12 s | 2.54 s | 751 ms | 9.17 s | 13.6 | 12.00 GB |
| 24 | 2026-02-27 20:18:51 | 553,564,241 | 21 | 3600 s | 13.1M | 3,648 | 5,001 | 2.52 s | 5.57 s | 6.94 s | 2.74 s | 743 ms | 8.27 s | 12.8 | 12.10 GB |
| 25 | 2026-02-27 21:18:54 | 568,957,337 | 21 | 3600 s | 12.4M | 3,465 | 4,733 | 2.60 s | 5.88 s | 7.45 s | 2.88 s | 750 ms | 8.44 s | 13.6 | 12.05 GB |
| 26 | 2026-02-27 23:13:29 | 589,425,541 | 21 | 7200 s | 28.6M | 3,961 | 4,278 | 2.33 s | 4.86 s | 6.57 s | 2.52 s | 748 ms | 69.76 s | 11.0 | 18.46 GB |
| 27 | 2026-02-28 01:14:04 | 614,213,877 | **23** | 7200 s | 53.2M | 7,436 | 8,175 | 1.22 s | 2.09 s | 3.45 s | 1.34 s | 886 ms | 6.62 s | 50.8 | 41.84 GB |
| 28 | 2026-02-28 03:14:11 | 679,393,361 | 23 | 7200 s | 51.8M | 7,236 | 8,014 | 1.24 s | 2.32 s | 3.70 s | 1.38 s | 914 ms | 7.28 s | 42.0 | 41.90 GB |
| 29 | 2026-02-28 05:14:17 | 736,016,465 | 23 | 7200 s | 48.8M | 6,810 | 7,844 | 1.28 s | 2.78 s | 4.32 s | 1.47 s | 960 ms | 7.88 s | 39.6 | 41.95 GB |
| 30 | 2026-02-28 07:14:24 | 773,597,429 | 23 | 7200 s | 45.6M | 6,368 | 7,664 | 1.31 s | 3.19 s | 4.94 s | 1.57 s | 1.01 s | 7.60 s | 41.3 | 42.21 GB |
| 31 | 2026-02-28 09:14:32 | 824,348,508 | 23 | 7200 s | 42.6M | 5,955 | 7,590 | 1.34 s | 3.78 s | 5.81 s | 1.68 s | 1.05 s | 8.87 s | 46.0 | 41.86 GB |
| 32 | 2026-02-28 11:14:37 | 865,872,117 | 23 | 7200 s | 36.5M | 5,100 | 7,262 | 1.42 s | 4.67 s | 6.58 s | 1.96 s | 1.08 s | 9.14 s | 49.8 | 39.65 GB |
| 33 | 2026-02-28 13:14:44 | 874,512,384 | 23 | 7200 s | 31.7M | 4,431 | 6,528 | 1.52 s | 5.63 s | 7.83 s | 2.26 s | 1.07 s | 9.99 s | 48.2 | 39.82 GB |
| 34 | 2026-02-28 15:14:51 | 939,272,437 | 23 | 7200 s | 29.4M | 4,114 | 6,119 | 1.63 s | 6.10 s | 8.26 s | 2.43 s | 1.13 s | 10.98 s | 51.4 | 39.73 GB |
| 35 | 2026-02-28 17:14:56 | 951,771,463 | 23 | 7200 s | 23.4M | 3,269 | 4,524 | 2.39 s | 7.10 s | 9.24 s | 3.06 s | 1.15 s | 12.92 s | 48.2 | 39.49 GB |

Backend is `rocks` for all 35. `inserted` is the log's own rounded `Inserted:` field;
the exact per-run count is the last `total_inserted` in the matching CSV.

**Headline decay: 44 139 entries/s on an empty database → 3 269 entries/s at ~952 M keys,
a 13.5× fall. Batch p50 rose from 250 ms to 2.39 s and p99 from 405 ms to 9.24 s for the
same 10 000-entry batch.**

### The frontier-depth discontinuity at run 27

The single largest non-monotonicity in the table is not noise. The frontier advanced
**21 → 22 → 23** at the very end of run 26:

```
[2026-02-28T01:12:54Z ...] Advancing frontier depth to 22 with nodes 4194304
[2026-02-28T01:13:07Z ...] Advancing frontier depth to 23 with nodes 8388608
```

Run 27 was the first to open at depth 23. Across that boundary, with the DB *larger*:

| | run 26 (589 M keys, depth 21) | run 27 (614 M keys, depth 23) |
|---|--:|--:|
| throughput | 3 961 /s | **7 436 /s** (+88 %) |
| batch p50 | 2.33 s | 1.22 s |
| batch p99 | 6.57 s | 3.45 s |
| init time | 11.0 s | **50.8 s** (4.6×) |
| peak RSS | 18.46 GB | **41.84 GB** (2.3×) |

Depth 23 buys roughly 2× insert throughput for 2.3× memory and 4.6× open time. Any future
comparison must control for frontier depth, or it will measure this instead of the change
under test. All four whole-depth transitions in the campaign: depth 20 reached
2026-02-26 21:18:47, depth 21 at 2026-02-26 23:38:31, depth 22 at 2026-02-28 01:12:54,
depth 23 at 2026-02-28 01:13:07.

## 2. Shape of the insertion rate *within* a run

The CSV writes one row per completed batch (10 000 entries), so `log_*.csv` is a per-batch
completion timeline and consecutive-timestamp gaps are batch latencies.

**Run 1 (`log_20260226_210307.csv`) — empty database, the only run whose rate decays.**
The rate falls monotonically as the tree is built from nothing: it is dominated by the
frontier racing from depth 0 to depth 19 in the first ~4 minutes.

| elapsed | entries | rate /s |
|---|--:|--:|
| 0–10 s | 1 970 000 | 197 532 |
| 10–20 s | 1 150 000 | 116 041 |
| 20–30 s | 880 000 | 89 192 |
| 30–40 s | 690 000 | 70 027 |
| 50–60 s | 650 000 | 66 072 |
| 100–110 s | 520 000 | 52 867 |
| 250–300 s | 1 920 000 | 38 614 |
| 450–500 s | 1 670 000 | 34 049 |
| 550–600 s | 1 380 000 | 27 695 |

**Run 35 (`log_20260228_171456.csv`) — 951.8 M keys, depth 23, 7 200 s. The shape inverts:
the run *warms up*.** Rate roughly doubles over two hours and never plateaus.

| elapsed (12 equal buckets) | entries | rate /s |
|---|--:|--:|
| 0–596 s | 1 300 000 | 2 184 |
| 596–1192 s | 1 320 000 | 2 259 |
| 1192–1788 s | 1 420 000 | 2 394 |
| 1788–2384 s | 1 550 000 | 2 621 |
| 2384–2980 s | 1 710 000 | 2 915 |
| 2980–3576 s | 1 850 000 | 3 118 |
| 3576–4172 s | 1 870 000 | 3 157 |
| 4172–4767 s | 2 250 000 | 3 789 |
| 4767–5363 s | 2 240 000 | 3 783 |
| 5363–5959 s | 2 410 000 | 4 063 |
| 5959–6555 s | 2 600 000 | 4 370 |
| 6555–7151 s | 2 740 000 | **4 609** |

Finer-grained, the first minute is *fast* (4 016 /s), then it collapses to a floor at
1 657 /s around t≈150 s and climbs back from there — the memtable absorbs the opening
batches, then compaction and cold subtree reads take over.

**This cold-start penalty is systematic and grows with database size.** First-300 s rate
vs last-300 s rate, all 35 runs:

| keys at open | first 300 s /s | last 300 s /s | ratio |
|--:|--:|--:|--:|
| 1 (empty) | 54 269 | 33 981 | 0.63 |
| 26 817 331 | 24 108 | 23 341 | 0.97 |
| 99 373 547 | 13 489 | 19 123 | 1.42 |
| 241 927 454 | 5 669 | 12 449 | 2.20 |
| 407 602 462 | 4 481 | 8 069 | 1.80 |
| 568 957 337 | 1 652 | 4 706 | 2.85 |
| 614 213 877 (depth 23) | 5 918 | 8 263 | 1.40 |
| 824 348 508 | 4 498 | 7 584 | 1.69 |
| 951 771 463 | 2 294 | 4 979 | 2.17 |

(Worst case, run 21: 385 /s in the first 300 s vs 5 854 /s in the last 300 s, a 15.2× ratio
— caused by a single 220 s first batch, see §6.)

**Consequence for future comparisons:** the whole-run `Throughput:` figure is a blend of a
size-dependent cold start and a steady state, and short runs are penalised more. Compare
the `steady /s` column, or fix the run length exactly.

## 3. Peak RSS vs run length and frontier depth

Recorded by `tools/bench_loop.py` polling `/proc/<pid>/status` `VmHWM` once per second.
(Note the `n/r` for run 9: the benchmark completed but `bench_loop` was killed before it
wrote the finished line.)

| frontier depth at open | interior nodes held resident | run length | runs | peak RSS min | peak RSS max |
|--:|--:|--:|--:|--:|--:|
| 0 (empty→19) | 0 | 600 s | 1 | 7.61 GB | 7.61 GB |
| 19 | 524 288 | 600 s | 1 | 6.57 GB | 6.57 GB |
| 20 | 1 048 576 | 600 s | 6 | 5.98 GB | 6.53 GB |
| 21 | 2 097 152 | 1800 s | 7 | 11.20 GB | 11.75 GB |
| 21 | 2 097 152 | 3600 s | 9 | 11.95 GB | 14.65 GB |
| 21 | 2 097 152 | 7200 s | 1 | 18.46 GB | 18.46 GB |
| 23 | 8 388 608 | 7200 s | 9 | 39.49 GB | 42.21 GB |

Two independent drivers, both visible:

1. **Frontier depth.** At a fixed 7 200 s run: depth 21 → 18.46 GB, depth 23 → 39.5–42.2 GB.
2. **Run length, at fixed depth 21.** 1800 s → ~11.2–11.8 GB; 3600 s → ~12.0–14.7 GB;
   7200 s → 18.46 GB. **Memory grows with time spent inserting, not just with tree size** —
   the resident store accumulates within a run. This is the "unbounded growth" /
   "depth-24/25 accumulation" behaviour that `REVIEW.md` §3.1.5 / §4 Tier-2 §4 addresses.

Also note the 5.98 GB floor on a 600 s run against a ~55 M-key database: that is the
uncapped store pre-allocation `REVIEW.md` §3.1.3 reports cutting from ~36.8 GB to ~4.6 GB
reserved at open. **Every RSS number in this table is pre-fix.**

The peak, 42.21 GiB (45.3 GB), is ~67 % of the 62.6 GiB `MemTotal` reported by the machine
this record was produced on — but the benchmark host is not identified in `bench_logs/`, so
treat that ratio as indicative only.

## 4. Throughput as a function of database size — the comparison table

The relationship that matters. Because the depth 21→23 transition doubles throughput on its
own (§1), the two eras are listed separately; **compare a future run against the era with
the same frontier depth.**

### Depth 23 era — use this for comparison at ≥ 600 M keys

All rows: `timeout=7200 window=100000 batch=10000`, so run length is controlled.

| keys at open | est. SST bytes ¹ | throughput /s | steady /s | batch p50 | batch p99 | peak RSS | init s |
|--:|--:|--:|--:|--:|--:|--:|--:|
| 614 213 877 | ~79 GB | 7 436 | 8 175 | 1.22 s | 3.45 s | 41.84 GB | 50.8 |
| 679 393 361 | ~88 GB | 7 236 | 8 014 | 1.24 s | 3.70 s | 41.90 GB | 42.0 |
| 736 016 465 | ~95 GB | 6 810 | 7 844 | 1.28 s | 4.32 s | 41.95 GB | 39.6 |
| 773 597 429 | ~100 GB | 6 368 | 7 664 | 1.31 s | 4.94 s | 42.21 GB | 41.3 |
| 824 348 508 | ~106 GB | 5 955 | 7 590 | 1.34 s | 5.81 s | 41.86 GB | 46.0 |
| 865 872 117 | ~112 GB | 5 100 | 7 262 | 1.42 s | 6.58 s | 39.65 GB | 49.8 |
| 874 512 384 | ~113 GB | 4 431 | 6 528 | 1.52 s | 7.83 s | 39.82 GB | 48.2 |
| 939 272 437 | ~121 GB | 4 114 | 6 119 | 1.63 s | 8.26 s | 39.73 GB | 51.4 |
| 951 771 463 | ~123 GB | 3 269 | 4 524 | 2.39 s | 9.24 s | 39.49 GB | 48.2 |
| **999 502 643 (final)** | **128.78 GB (measured)** | — | — | — | — | — | **46 (§5)** |

¹ **Extrapolated, not measured.** Only the final point is a real measurement
(128 781 163 719 B of SST at 999 502 643 estimated keys = 128.8 B/key); the other rows are
that ratio applied linearly. Space amplification varies with compaction state, so the true
intermediate sizes are **not** derivable from `bench_logs/` — take them from the per-level
`Sum` rows in `bigdb/LOG` instead. Flagged rather than presented as fact.

Over 614 M → 952 M keys (a 1.55× growth) throughput fell 2.27× and p99 rose 2.68×.

### Depth 21 era — for comparison at 170–590 M keys

Run length is *not* controlled here (1800/3600/7200 s), so read `steady /s`.

| keys at open | throughput /s | steady /s | batch p50 | batch p99 | peak RSS | timeout |
|--:|--:|--:|--:|--:|--:|--:|
| 168 736 849 | 14 511 | 14 539 | 589 ms | 1.71 s | 11.30 GB | 1800 s |
| 241 927 454 | 10 875 | 12 305 | 785 ms | 2.80 s | 11.48 GB | 1800 s |
| 368 930 979 | 6 839 | 11 171 | 1.02 s | 6.22 s | 11.71 GB | 1800 s |
| 407 602 462 | 6 928 | 8 115 | 1.40 s | 2.76 s | 14.52 GB | 3600 s |
| 488 741 273 | 5 587 | 5 922 | 1.71 s | 3.60 s | 14.57 GB | 3600 s |
| 568 957 337 | 3 465 | 4 733 | 2.60 s | 7.45 s | 12.05 GB | 3600 s |
| 589 425 541 | 3 961 | 4 278 | 2.33 s | 6.57 s | 18.46 GB | 7200 s |

### Depth ≤ 20 era — small-database reference

| keys at open | throughput /s | steady /s | batch p50 | batch p99 | peak RSS | timeout |
|--:|--:|--:|--:|--:|--:|--:|
| 1 (empty) | 44 139 | 31 671 | 250 ms | 405 ms | 7.61 GB | 600 s |
| 26 817 331 | 23 763 | 24 407 | 384 ms | 890 ms | 6.57 GB | 600 s |
| 54 431 580 | 20 671 | 20 406 | 468 ms | 729 ms | 6.53 GB | 600 s |
| 99 373 547 | 16 298 | 19 390 | 550 ms | 1.45 s | 6.10 GB | 600 s |
| 125 378 232 | 12 463 | 8 806 | 713 ms | 1.50 s | not recorded | 1800 s |

(The 125 M row's low `steady` is real and explained: the frontier advanced 20→21 at
23:38:31, 18.5 min into a 30 min run, so the tail was paying for the advance.)

## 5. Open / init time as a function of database size

`Init time:` measures the whole `RocksTransRelMPT::new_with_path` call. The `cli_*.log`
timestamps split it into two phases (1 s resolution). **331 opens were logged** — far more
than the 35 completed runs, because every crashed run also logged an open.

| frontier depth | interior nodes loaded | opens measured | median open+read | median rebuild | median total | min | max |
|--:|--:|--:|--:|--:|--:|--:|--:|
| 0 (empty) | 0 | 1 | 0 s | 0 s | 0.036 s | — | — |
| 19 | 524 288 | 1 | 2 s | 1 s | 3 s | 3 s | 3 s |
| 20 | 1 048 576 | 11 | 1 s | 4 s | 5 s | 4 s | 8 s |
| 21 | 2 097 152 | 307 | 3 s | 8 s | 11 s | 9 s | 23 s |
| 23 | 8 388 608 | 11 | 15 s | 32 s | **47 s** | 40 s | 51 s |

**Open time is a function of frontier depth (interior-node count), essentially not of key
count.** Within depth 21 the DB grew from 149 M to 589 M keys — a 4× growth — and the
median open stayed at 11 s. The per-node rate is roughly constant at 160–200 k nodes/s
across depths 20/21/23.

**The full open at the final size:** the last open, at `estimate-num-keys = 999 502 643`
(2026-02-28 20:43:37 → 20:44:23), took **46 s** — 15 s to open RocksDB and read
8 388 608 interior nodes, then 31 s to rebuild the in-memory frontier. The last
process-measured `Init time:` (run 34, 939 272 437 keys) was **51.438 s**.

```
[2026-02-28T20:43:37Z INFO  bench] Using RocksDB-backed RocksTransRelMPT at bigdb
[2026-02-28T20:43:52Z INFO  ...rocks_frontier] Loaded 8388608 interior nodes at depth 23 from storage
[2026-02-28T20:44:23Z INFO  ...rocks_frontier] Initialized RocksTransRelMPT with root empty,
                             complete depth 23, approx entries 999502643, leaves per frontier 59
```

## 6. Stalls and tail latency

Counting inter-sample gaps in `log_*.csv` (= per-batch latencies) above thresholds:

| keys at open | batches | >5 s | >10 s | >30 s | max gap | % of run in >5 s batches |
|--:|--:|--:|--:|--:|--:|--:|
| 87 902 126 | 1 030 | 0 | 0 | 0 | 2.4 s | 0.0 % |
| 241 927 454 | 1 944 | 2 | 1 | 1 | 57.1 s | 3.5 % |
| 388 119 920 | 948 | 26 | 17 | 6 | 55.8 s | 29.3 % |
| 516 926 996 | 1 237 | 46 | 25 | 0 | 28.5 s | 21.8 % |
| 589 425 541 | 2 859 | 132 | 1 | 1 | 69.8 s | 11.8 % |
| 614 213 877 (depth 23) | 5 315 | 11 | 0 | 0 | 6.6 s | 0.8 % |
| 824 348 508 | 4 259 | 78 | 0 | 0 | 8.9 s | 6.6 % |
| 939 272 437 | 2 940 | 274 | 3 | 0 | 11.0 s | 24.9 % |
| 951 771 463 | 2 337 | 350 | 17 | 0 | 12.9 s | **33.3 %** |

Across all 35 runs: **122 660 run-seconds, of which 12 787 s (10.4 %) were spent inside
batches taking longer than 5 s.** By the last run that is 33.3 %. The depth-23 transition
resets it (0.8 % at run 27) and it climbs back to 33 % over nine runs.

**The reported `max:` is sometimes the very first batch after open, not a steady-state
stall.** Comparing each run's first CSV timestamp against its "Writing insertion log"
log line:

| run | first-batch latency | reported max | reported p99 |
|---|--:|--:|--:|
| `20260227_171841` | **220.9 s** | 220.215 s | 25.045 s |
| `20260227_102024` | 35.5 s | 54.323 s | 6.221 s |
| `20260227_015724` | 20.7 s | 57.058 s | 2.803 s |
| typical depth-23 run | 1.1 – 2.1 s | 6.6 – 12.9 s | 3.4 – 9.2 s |

Run 21's 220 s `max` is entirely the first batch. Prefer p95/p99 for tail comparisons; use
`max` only with the first-batch check applied.

## 7. Run failures — the 2026-02-27 crash-loop window

Of 575 launched runs, **522 failed**: 282 with exit 101 (panic), 240 with exit 1. Every one
of them fell in a single 10.4 h window, **2026-02-26 23:46:22 → 2026-02-27 10:08:49**.

| failure | occurrences | where |
|---|--:|---|
| `IO error: ... Too many open files` | 38 553 lines across **283 logs** | `Failed to commit leaf-merge batch` at `src/mpt/rocks_frontier/mod.rs:709` (34 892) and `Failed to commit subtree batch` at `:935` (3 660) |
| `Error: Too many open files (os error 24)` at open | 1 | top-level |
| `IO error: While lock file: bigdb/LOCK: Resource temporarily unavailable` | 2 | two `bench_loop` sessions overlapping |

This is an `RLIMIT_NOFILE` exhaustion against RocksDB's open-file demand, not an algorithmic
failure. Two things it costs this record:

- **A ~127 M-key hole in the series.** The last good run before the window opened at
  241 927 454 keys; the next at 368 930 979. The 252 crashed runs that got as far as
  writing a CSV inserted **108 090 000 entries** into the DB with no results block —
  which is exactly the size of the unexplained jump. Any per-run entry arithmetic across
  this window will not balance.
- **7.84 h of wall clock** with no throughput measurement.

The 307 depth-21 opens recorded during the crash loop are, however, the largest clean sample
of open-time measurements in the whole campaign (§5).

## 8. Commands

Everything above is re-derivable from `bench_logs/` alone (plus one `ls` against `bigdb/`).
Run from the repository root.

```sh
# Inventory
ls bench_logs/cli_*.log | wc -l                                  # 338
ls bench_logs/log_*.csv | wc -l                                  # 330
grep -l "=== Benchmark Results ===" bench_logs/cli_*.log | wc -l # 35

# Run configurations and outcomes
grep -n "bench_loop started\|backend=rocks\|db_dir=" bench_logs/bench_loop.log
grep -oP '(?<=— )(OK|FAILED \(exit \d+\))(?= — peak RSS)' bench_logs/bench_loop.log \
  | sort | uniq -c | sort -rn                                    # 282 / 240 / 35
grep -c "=== Run .* starting" bench_logs/bench_loop.log          # 575
grep -c "Building release binary" bench_logs/bench_loop.log      # 18 sessions

# Failure classes
grep -h "^Error:\|panicked at" bench_logs/cli_*.log | sed 's/[0-9]\{6,\}/N/g' \
  | sort | uniq -c | sort -rn
grep -l "Too many open files" bench_logs/cli_*.log | wc -l       # 283

# Frontier depth transitions
grep -h "Advancing frontier depth" bench_logs/cli_*.log | sort -u

# Final database size
du -sb bigdb                                                     # 132133808588
python3 -c "import glob,os; f=glob.glob('bigdb/*.sst'); \
  print(len(f), sum(os.path.getsize(x) for x in f))"             # 2321 128781163719
```

```python
# parse_bench.py — every "=== Benchmark Results ===" block -> JSON
# usage: python3 parse_bench.py > bench.json
import re, os, glob, json
rows = []
for f in sorted(glob.glob('bench_logs/cli_*.log')):
    txt = open(f, errors='replace').read()
    if '=== Benchmark Results ===' not in txt:
        continue
    g = lambda p: (re.search(p, txt).group(1) if re.search(p, txt) else None)
    rows.append({
        'file': os.path.basename(f), 'stamp': os.path.basename(f)[4:-4],
        'backend': g(r'Backend:\s+(\S+)'),
        'init_s': g(r'Init time:\s+([\d.]+) s'),
        'timeout_s': g(r'Timeout:\s+([\d.]+) s'),
        'inserted': g(r'Inserted:\s+(\S+) entries'),
        'insert_time_s': g(r'Insert time:\s+([\d.]+) s'),
        'throughput': g(r'Throughput:\s+([\d.]+) entries/s'),
        'windows': g(r'Windows processed:\s+(\d+)'),
        'batches': g(r'Batches processed:\s+(\d+)'),
        'mean': g(r'mean:\s+(\S+ ?\S*?)\n'), 'p50': g(r'p50:\s+(\S+ ?\S*?)\n'),
        'p95': g(r'p95:\s+(\S+ ?\S*?)\n'),  'p99': g(r'p99:\s+(\S+ ?\S*?)\n'),
        'min': g(r'min:\s+(\S+ ?\S*?)\n'),  'max': g(r'max:\s+(\S+ ?\S*?)\n'),
        'root': g(r'Root hash: (\S+)'),
        'approx_entries_at_init': g(r'approx entries (\d+)'),
        'complete_depth': g(r'complete depth (\d+)'),
        'leaves_per_frontier': g(r'leaves per frontier (\d+)'),
        'interior_loaded': g(r'Loaded (\d+) interior nodes'),
        'interior_depth': g(r'Loaded \d+ interior nodes at depth (\d+)'),
    })
print(json.dumps(rows, indent=1))
```

```python
# parse_loop.py — bench_loop.log -> per-run config, status, peak RSS, wall time
import re, json, datetime
lines = open('bench_logs/bench_loop.log', errors='replace').read().split('\n')
cfg = cur = None; runs = []
for l in lines:
    if (m := re.match(r'bench_loop started at (.+)', l)):
        cfg = {'session_start': m.group(1)}
    elif (m := re.match(r'\s+backend=(\S+) timeout=(\d+) window=(\d+) batch=(\d+)', l)):
        cfg.update(backend=m.group(1), timeout=int(m.group(2)),
                   window=int(m.group(3)), batch=int(m.group(4)))
    elif (m := re.match(r'\s+db_dir=(\S+)', l)):
        cfg['db_dir'] = m.group(1)
    elif (m := re.match(r'=== Run (\d+) starting at (.+) ===', l)):
        cur = {'run': int(m.group(1)), 'start': m.group(2), 'cfg': dict(cfg)}
        runs.append(cur)
    elif (m := re.match(r'\s+CLI log:\s+bench_logs/(\S+)', l)) and cur:
        cur['cli'] = m.group(1)
    elif (m := re.match(r'\s+CSV log:\s+bench_logs/(\S+)', l)) and cur:
        cur['csv'] = m.group(1)
    elif (m := re.match(r'=== Run \d+ finished at (.+?) — (.+?) — peak RSS: (.+?) ===', l)) and cur:
        cur.update(end=m.group(1), status=m.group(2), rss=m.group(3))
        fmt = '%Y-%m-%d %H:%M:%S'
        cur['wall_s'] = (datetime.datetime.strptime(cur['end'], fmt)
                         - datetime.datetime.strptime(cur['start'], fmt)).total_seconds()
        cur = None
json.dump(runs, open('loop.json', 'w'), indent=1)
```

```python
# rate.py — within-run insertion-rate profile (§2)
import csv, sys
def prof(path, nbuck=12):
    ts, tot = [], []
    with open(path) as f:
        c = csv.reader(f); next(c)
        for row in c:
            if len(row) >= 2:
                ts.append(float(row[0])); tot.append(int(row[1]))
    t0, T = ts[0], ts[-1] - ts[0]
    print(f"{path}: samples={len(ts)} elapsed={T:.1f}s overall={(tot[-1]-tot[0])/T:.1f}/s")
    for i in range(nbuck):
        lo, hi = t0 + T*i/nbuck, t0 + T*(i+1)/nbuck
        idx = [k for k, t in enumerate(ts) if lo <= t <= hi]
        if len(idx) < 2: continue
        a, b = idx[0], idx[-1]
        print(f"  {lo-t0:7.0f}-{hi-t0:<7.0f}s {tot[b]-tot[a]:>9} entries "
              f"{(tot[b]-tot[a])/(ts[b]-ts[a]):>9.0f}/s")
for p in sys.argv[1:]: prof(p)
# python3 rate.py bench_logs/log_20260226_210307.csv bench_logs/log_20260228_171456.csv
```

```python
# stalls.py — long-batch accounting (§6) and steady-state rate (§1 "steady /s")
import csv, glob
T = G = 0
for p in sorted(glob.glob('bench_logs/log_*.csv')):
    ts = []
    with open(p) as f:
        c = csv.reader(f); next(c)
        for row in c:
            if len(row) >= 2: ts.append(float(row[0]))
    if len(ts) < 10: continue
    gaps = [ts[i+1]-ts[i] for i in range(len(ts)-1)]
    tot = ts[-1]-ts[0]; g5 = sum(g for g in gaps if g > 5)
    T += tot; G += g5
    print(f"{p} batches={len(gaps)} >5s={sum(g>5 for g in gaps)} "
          f"max={max(gaps):.1f}s in_gaps={100*g5/tot:.1f}%")
print(f"ALL: run-seconds={T:.0f} in >5s batches={G:.0f} ({100*G/T:.1f}%)")
```

```python
# opens.py — init time split by frontier depth (§5), all 331 recorded opens
import re, glob, datetime, statistics as st
from collections import defaultdict
d = defaultdict(list)
for f in sorted(glob.glob('bench_logs/cli_*.log')):
    txt = open(f, errors='replace').read()
    def t(p):
        m = re.search(p, txt)
        return datetime.datetime.strptime(m.group(1), '%Y-%m-%dT%H:%M:%SZ') if m else None
    t0 = t(r'\[(\S+Z) INFO  bench\] Using RocksDB')
    t1 = t(r'\[(\S+Z) INFO  jellyfish_rs::mpt::rocks_frontier\] Loaded')
    t2 = t(r'\[(\S+Z) INFO  jellyfish_rs::mpt::rocks_frontier\] Initialized')
    if not (t0 and t1 and t2): continue
    m = re.search(r'Loaded (\d+) interior nodes at depth (\d+)', txt)
    d[(int(m.group(2)), int(m.group(1)))].append(
        ((t1-t0).total_seconds(), (t2-t1).total_seconds(), (t2-t0).total_seconds()))
for k in sorted(d):
    v = d[k]
    print(f"depth={k[0]} nodes={k[1]} n={len(v)} read={st.median(x[0] for x in v):.0f}s "
          f"rebuild={st.median(x[1] for x in v):.0f}s total={st.median(x[2] for x in v):.0f}s "
          f"min={min(x[2] for x in v):.0f} max={max(x[2] for x in v):.0f}")
```

```python
# totals.py — entries inserted across every logged run
import glob
tot = n = 0
for p in sorted(glob.glob('bench_logs/log_*.csv')):
    last = None
    for line in open(p): last = line
    if last and not last.startswith('timestamp'):
        tot += int(last.strip().split(',')[1]); n += 1
print(n, 'runs with data;', tot, 'entries')   # 294 978620000
```

## Gaps and caveats

- **On-disk size over time is not in `bench_logs/`.** Only the final size is measured
  (128 781 163 719 B of SST / 2 321 files; 132 133 808 588 B for the whole directory). The
  `~GB` column in §4 is a linear extrapolation from the final 128.8 B/key ratio, explicitly
  labelled as such. The real size series must come from the per-level `Sum` rows in
  `bigdb/LOG` — capture it there before deletion.
- **"Keys at open" is `rocksdb.estimate-num-keys`, not an exact leaf count.** It counts all
  keys in the CF (leaves + persisted interior nodes + metadata) and is an estimate. Run-to-run
  deltas swing ±30 M against a per-run insert of 10–50 M. The exact totals from the CSVs are
  the trustworthy figures.
- **No benchmark host record.** `bench_logs/` contains no CPU count, RAM, disk model,
  filesystem, `ulimit -n`, kernel or RocksDB build info. RSS is interpretable only in relative
  terms. (The machine this analysis ran on reports 64 CPUs / 62.6 GiB — unverified as the
  same host.)
- **No exact commit per session.** `bench_loop` logs "Building release binary..." but not the
  SHA. The claim that `src/` was constant across the campaign is inferred from `git log`
  dates (`005a773` 2026-02-26 21:35 → `05a34a8` 2026-02-28 20:49), not from the logs. One
  caveat: session 1 (runs 1–8) was built before `005a773`, which removed a `depth >= 20`
  early-return in `write_complete_frontier` — that is why the persisted frontier could not
  pass depth 20 until session 2.
- **This is an insert-only benchmark.** No read, proof-generation, or query throughput was
  ever measured. There is no read baseline to compare against.
- **The `Throughput:` field is not steady-state** — see §2. The `steady /s` column is
  a derived quantity (last 20 % of each run's own CSV), not something the binary printed.
- **The `max:` batch latency is unreliable as a stall indicator** — in at least three runs it
  is the first batch after open (§6).
- **Two runs have no result block:** run 9's `bench_loop` peak RSS was never written (the
  session was killed), and the `window=2M batch=100K` experiment
  (`cli_20260226_230653.log`) was killed 5 minutes in, so nothing is known about
  larger-batch behaviour.
- **36 of the 330 CSVs are empty and 7 `cli_*.log` files are ≤ 2 lines** (killed at or
  before open). They contribute nothing and are excluded from all totals.
- **The 108 090 000 entries inserted by crashed runs are in the database but have no
  latency or throughput measurement attached.** Roughly 11 % of the final database was
  built blind.


---

# Gaps: what could not be determined

Recorded so that a future reader does not mistake absence for zero.

## How to rebuild an equivalent database, and what to measure next time

1. EXACT LEAF COUNT. 978,620,000 is the sum of the final `total_inserted` of every log_*.csv. 36 CSVs are header-only and ~245 runs' CSVs were overwritten by a same-second successor (338 cli logs / 330 csv for 575 runs), so this is a very tight lower bound rather than a certainty. The DB's own "approx entries 999502643" is a <=100-sample extrapolation (February code: len() == estimate_leaf_count()), not exact, and the 2.1% gap between the two is unresolved. I did NOT count leaves by scanning the SSTs.

2. ULIMIT -N DURING THE BUILD. Not recorded anywhere. Inferred as ~1024 from the fact that the first EMFILE crash occurred with 512 live SSTs. The container reports 1048576 today, which is not the February value.

3. HOST IDENTITY. CPU/RAM/disk are read from the container today (2026-08-29). I cannot verify the February 2026 host was this machine; only that bigdb and bench_logs are on this filesystem, and REVIEW.md attributes a microbenchmark to "the same Threadripper PRO 3975WX".

4. FRONTIER ADVANCES FOR DEPTHS 1-14. All logged in the same wall-clock second (2026-02-26T21:03:08Z) at 1 s resolution. I can only bracket them at 20,000-360,000 entries; individual per-depth counts are unrecoverable.

5. "~3.13 WRITES/INSERT" IN REVIEW.MD (line 864) HAS NO DERIVATION I COULD FIND. `grep -n "3.13" REVIEW.md` returns exactly one hit and no working. My own computation from the per-run `Cumulative writes: N writes` lines gives 2.58-2.98 (mean 2.80). Same shape, different value; not reconciled.

6. WRITE ATTRIBUTION BY CALL SITE. Nothing in LOG or the CLI logs separates leaf writes, frontier-interior writes, persist_interior_nodes_at_depth writes, and load_subtree_from_storage write-back. I can report 39.8-131.3 RocksDB keys written per application insert and show it correlates with L = N/2^F, but I cannot attribute it. This is the single most valuable thing the database could not tell me.

   **CLOSED** by `src/census.rs` (see 6.2.1). It was indeed the most valuable one: the
   census attributed the writes on its first run, and the two largest wins of the next
   round came out of it rather than out of code reading.

7. rustc/LLVM VERSION USED IN FEBRUARY. Unknown; only today's 1.98.0 is observable.

8. NVMe QD1 LATENCY CURVE. Never measured. REVIEW 3.3.5 needs an fio 4/8/16 KiB QD1 curve to conclude on block_size and it does not exist.

9. HOST LOAD during the 47 h. Unknown; throughput figures may include contention from other work on the box.

10. DB SIZE AT EVERY FRONTIER ADVANCE. I sampled the `Sum` row from five LOG.old files (at ~132M, ~224M, ~300M, ~350M, 600M leaves) plus the final LOG. A continuous size-vs-N series is extractable from the 575 LOG files but I did not build one (3.2 GB of logs).

11. LEAF RECORD SIZE. A comment in src/mpt/storage/rocks.rs claims leaf values dropped from 65 to 33 bytes, but Node::serialize at HEAD still encodes (value, merkle_hash), i.e. the change is not on this branch. I did not resolve this and deliberately made no claim about it.

    **RESOLVED**: the comment was ahead of the code, and the code has since caught up.
    `TAG_LEAF_COMPACT` (tag then a 32-byte value, 33 bytes) is on
    `perf/rocksdb-improvements`; the older `(value, merkle_hash)` form still decodes, so
    `bigdb`-era records were never at risk of being misread. See REVIEW.md §4's landed
    table and §3.3.6. A third tag, `TAG_FRONTIER` (65 bytes), landed later still for
    frontier rows — so the record sizes this document's census assumes (34 key / 65 leaf
    value / 99 interior value) describe `bigdb` and no longer describe the code.

## On-disk composition: records, sizes and compression

1. EXACT DISTINCT LEAF COUNT — not determinable. No ldb/sst_dump in the container, the DB
   may not be modified/scanned, and this database predates the __mpt_leaf_count__ key
   (proved: the raw_key_size deficit in the 5 metadata-bearing files is exactly 34, which
   is __mpt_root__ + __mpt_complete_depth__ only; adding __mpt_leaf_count__ would make it
   50). Bracketed: 978,620,000 (hard lower bound from bench CSVs, 36/330 CSVs empty) to
   999,502,643 (a 100-subtree sample, ~1% error per the code's own comment). Best single
   estimate 994,932,041 = L6 leaf records.

2. rocksdb.estimate-num-keys DOES NOT APPEAR IN bigdb/LOG* AT ALL — verified with
   grep -c across all 575 log files, zero matches. The cross-check the task requested
   against the LOG is therefore impossible. The only related logged figure is the bench's
   own "approx entries", which at depth>0 comes from a sampler, NOT from that property.

3. TWO LOGGED FIELDS ARE MISLEADING AND I COULD ONLY CORRECT THEM, NOT RE-DERIVE THEM:
   (a) "leaves per frontier N" in every cli_*.log equals leaves>>(depth+1); the current
       source divides by 1<<depth. Every logged value is half the current definition
       (59 should be read as 119). Confirmed arithmetically on all 5 distinct log lines.
   (b) "approx entries" is a 100-subtree sample, not a count.

4. WHETHER L6 IS STRICTLY DUPLICATE-FREE — inferred, not proved. Its interior band is
   provably perfect (exactly 2^24-1 = 16,777,215 records, matching Sum(d=0..23) 2^d), which
   is strong evidence, but the leaf band cannot be proved version-collapsed without a scan.
   If any L6 range arrived by trivial move, distinct leaves is slightly below 994,932,041.

5. PER-DEPTH INTERIOR COUNTS — not measurable. The MANIFEST gives only first/last key per
   file, so I can bound the interior band to length-fields 0..23 but cannot count nodes at
   each depth. The 2^24-1 identity implies 2^d per depth only if the tree is perfect.

6. THE 15,728,640 B WAL (bigdb/1321613.log) WAS NOT PARSED. It holds writes never flushed
   to an SST, so a small number of records — possibly including a newer root/depth — sit
   outside this census.

7. THE 6 TRUNCATED-LOG ORPHANS (1321611, 1321616-1321620) have no table_file_creation
   event anywhere, because bigdb/LOG is cut off mid-JSON-line. They are dead files, but
   their record counts are permanently unrecoverable.

8. SMALL DISCREPANCY WITH REVIEW.md LEFT UNRESOLVED: REVIEW.md:784-786 reports 1,200,114,830
   leaves / 18,477,040 interiors over the 2315-file on-disk set; my exact per-file solve
   over the same 2315 files gives 1,200,114,797 leaves / 18,477,059 interiors / 14 metadata
   (totals agree exactly at 1,218,591,870). The ~33/19-record difference is almost certainly
   REVIEW.md classifying whole files by raw_average_value_size rather than solving per
   record, but I did not re-run its method to confirm.

## RocksDB configuration and compaction/stall accounting (from `bigdb/LOG*`)

1. No database-lifetime "Cumulative" figure exists anywhere in the LOGs. RocksDB resets every Cumulative counter on each DB::Open(), and this DB was opened 574 times. All lifetime totals I report (66,264.91 GB SST written, 1,165,721 SST files, 145,745 flushes, 83,142,407,310 keys) were reconstructed from the EVENT_LOG_v1 stream and manifest sequence numbers. They are exact, but no single LOG line states them.

2. Lifetime USER INGEST is derived, not measured: 83,142,407,310 keys x 95.48 B/key = ~7,939 GB. The 95.48 B/key is a weighted mean over the 36 sessions that reported ingest (spread 95.2-102.4 B/key). Everything downstream (SST/ingest ~= 8.35x, (SST+WAL)/ingest ~= 9.35x) inherits that uncertainty. The exact, non-derived amplification is SST bytes / flush bytes = 11.03.

3. Stats-dump coverage is uneven. stats_dump_period_sec=600, so any session shorter than 600 s produced only the at-open dump. Phase A (jobs=32, sub=1) covers only 11.93e9 of its 34.37e9 keys (35%) — its aggregates are lower bounds. Phases D (universal) and E (final config) are 100% covered. Phase C (the tiny-file experiment) has 0% coverage; it ran 3 s.

4. Every session's last dump misses that session's final partial interval (up to 600 s). For the last complete session that is 470 s of 5,270 s (~9%) of unaccounted work. The very last session (LOG, 4 m 37 s) has no accounting at all beyond its open-time snapshot.

5. NO READ-SIDE DATA EXISTS. Options.statistics is (nil) and report_bg_io_stats is 0, so "** File Read Latency Histogram By Level **" is empty in all 542 stats dumps. There are no cache hit/miss counts, no bytes-read counters, no per-operation latency histograms anywhere in the 3.34 GB corpus. This record cannot supply a read-path baseline; a future run must install a Statistics object.

6. I could not explain the phase D vs phase E compaction-throughput gap (universal: 991 MB/s device traffic at 1.4x thread concurrency; level: 656 MB/s at 6.0x; per-thread read 374.8 vs 55.0 MB/s). Compaction job size (1.85 GB vs 0.16 GB average) correlates, but nothing in the LOG isolates page-cache effects, device queue depth, or the compaction_verify_record_count CPU cost. Recorded as an observation only.

7. I could not determine why the run ended at 2026/02/28-20:48:15. The last line of LOG is an ordinary table_file_creation; there is no shutdown sequence, no error, and no signal recorded.

8. The 239 failed opens all occurred inside a 2-second window (2026/02/26 23:46:22-24) with "LOCK: Resource temporarily unavailable". I did not determine what was holding the lock or what drove the retry storm — that would be in the bench-loop script, not the LOGs.

9. I did not attempt to attribute the ingest-rate decline (66 -> 28 MB/s over the run) between RocksDB stall and application-side slowdown. Session LOG.old.1772248451147453 shows ingest falling while interval stall was 0.0%, so at least part of it is application-side; the split is not determinable from the LOGs alone and needs bench_logs/log_*.csv.

10. The MPT record layout changed during the 47-hour run (see git history of src/mpt/storage/rocks.rs). RocksDB options are pinned by the 5-configuration table, but the application record format is not, so cross-phase throughput comparisons are confounded by more than the options.

## Benchmark performance history (BEFORE baseline)

1. DB size over time is NOT derivable from bench_logs/. I measured only the final size
   (128,781,163,719 B of SST across 2,321 files; 132,133,808,588 B for the whole bigdb/
   directory including 3.11 GiB of LOG*). The "est. SST bytes" column in section 4 is a
   LINEAR EXTRAPOLATION from the final 128.8 B/key ratio and is labelled as such in the
   table footnote. The real intermediate size series must be taken from the per-level Sum
   rows in bigdb/LOG (another section's source) before the DB is deleted.

2. "Keys at open" is rocksdb.estimate-num-keys, not an exact leaf count. It counts leaves +
   persisted interior nodes + metadata and is an estimate; consecutive-run deltas swing by
   +/-30M against per-run inserts of 10-50M. I used it as the size axis because it is the
   only per-run size signal in bench_logs, and flagged it everywhere it appears.

3. No benchmark host record. bench_logs/ contains no CPU count, RAM, disk model, filesystem,
   ulimit -n value, kernel version or RocksDB build info. Peak RSS is therefore only
   interpretable relatively. (I reported the analysis container's 64 CPU / 62.6 GiB figures
   with an explicit "unverified as the same host" caveat.)

4. No exact commit SHA per session. bench_loop.log logs "Building release binary..." but not
   the revision. My claim that src/ was constant across the campaign is INFERRED from git
   commit dates, not from the logs.

5. Insert-only. No read, lookup, or proof-generation throughput was ever measured anywhere in
   bench_logs, so there is no read baseline at all.

6. Run 9 (cli_20260226_231954.log, 125,378,232 keys) completed and printed results, but
   bench_loop was killed before writing its finished line, so its peak RSS is unknown.

7. The single window=2,000,000 / batch=100,000 experiment (cli_20260226_230653.log) was
   killed ~5 minutes in with an empty CSV. Nothing is known about larger-batch behaviour.

8. 108,090,000 entries (~11% of the final database) were inserted by the 252 crashed runs
   during the 2026-02-27 EMFILE crash loop. Those inserts have no throughput or latency
   measurement attached, and they create a ~127M-key hole in the series between the runs
   opening at 241,927,454 and 368,930,979 keys. Per-run entry arithmetic will not balance
   across that window.

9. 36 of 330 CSVs are empty and 7 cli logs are <=2 lines (processes killed at or before
   open). Excluded from all totals.

10. "steady /s", the first/last-300s ratios, the stall percentages and the first-batch
    latencies are all DERIVED by me from the CSVs; the binary never printed them. The
    first-batch latency is accurate to about +/-1 s because the log timestamp it is
    differenced against has 1-second resolution.
