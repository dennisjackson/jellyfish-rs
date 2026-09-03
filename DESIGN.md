# jellyfish-rs — Design

A binary, path-compressed Merkle-Patricia trie over 256-bit keys, backed by RocksDB, built
to measure how cheaply a durable authenticated map absorbs high-rate batch inserts. One
production type, `RocksFrontierMPT`; one test-only oracle, `SimpleMPT`.

Present tense throughout. Numbers cite the measurement record: `§n` is
[docs/REVIEW.md](docs/REVIEW.md), `BASELINE §n` is
[docs/BENCHMARK-BASELINE.md](docs/BENCHMARK-BASELINE.md).

## 1. Tree model

- `Key`, `Value`, `Digest`: distinct 32-byte newtypes. `Entry = (Key, Value)`.
- A node is named by its `Prefix = (key, length)`, the bit string from the root. Leaves have
  `length == 256`; an interior node sits at the first bit where two subtrees diverge.
  Invariant: every bit at or past `length` is zero. `Eq`/`Ord`/`Hash` and the interior hash
  read all 32 bytes, so a dirty tail would give one node two names. `Prefix::new`
  debug-checks it; `codec::decode_prefix` enforces it for rows read off disk.
- Location-addressed: an update rewrites a node in place under its prefix. No versioning,
  no garbage. This is what makes positional (prefix-free) storage of the tree top possible.
- `leaf = SHA-256("leaf" ‖ key ‖ value)`;
  `interior = SHA-256("interior" ‖ prefix.key ‖ prefix.length_be ‖ left ‖ right)`.
  Hashing is 4.2 % of insert CPU (§6.1); not a lever.
- An interior node's child hashes are stored state (frontier rows, `Levels`). Rehashing a
  parent needs the untouched sibling's hash; recomputing it from disk cost ~47 % of all read
  traffic (§5.2).

## 2. The frontier and the tree top

The frontier **F** is the deepest depth at which all 2^F nodes exist and are interior. Above
F the trie is a perfect binary tree, so `(depth, index)` determines the prefix and only hashes
need storing. Below F a positional prefix covers the same leaf range as the compressed prefix
beneath it, so "the hash of the leaves under a position" is still well defined.

`Levels`: one dense array per depth `0..=d`, 32-byte hash + 1 state byte
(unknown / proven empty / hashed) per position, `2^(d+1) − 1` positions.

| depths | holds |
|---|---|
| `0..=F` | every position hashed |
| `F+1` | every position hashed: the child hashes the frontier rows persist |
| `F+2..=d` | cache: hashed, proven empty, or unknown until a batch faults the subtree in |
| below | not materialised |

Every position is written by the recursion that computed its hash, so the levels are current
at every batch boundary.

**Depth `d` is derived, not configured:** `max(frontier term, block floor)` capped at
`max_depth`. The frontier term is `F+1` always (rows are staged from it) and `F+2` while
`F < frontier_cap` (the advance gate reads it). The **block floor** is
`ceil(log2(N / 62))`, the shallowest depth at which a positional subtree's leaves fit one
4 KiB data block (62 = 4096 / 66-byte leaf row). A scan costs about one seek and one data
block per sorted run whatever range it covers, so past the floor a deeper level halves
leaves-read per insert and changes blocks-read per insert by nothing — the shape §5.4
measured from the other side (more resident, fewer leaves read, *lower* throughput). At
1 B leaves the floor is 24 and the tree top 2.2 GB of the 4.4 GB the ceiling allows; 26
starts paying at ~4 B leaves, 28 at ~16 B.

Lazy residency is no discount: a page holds 128 slots, so under uniform keys a level is
>99 % resident after ~5 inserts per page — minutes into a run. Address space is cost, which
is why depth follows the leaf count rather than the budget (§21.8, §21.9).

Memory layout: the state byte lives in its own array so sibling hashes share a cache line
(§21.8). Arrays are allocated by an out-of-line default fill, which the release build folds
into the allocator's zeroed path so untouched pages never become resident; inlined, the fold
stopped firing and every new level was memset whole (§22, §23.2).

Concurrency: sibling subtrees own disjoint index ranges and the descent below the frontier
is sequential within a subtree, so no two workers touch one slot and `rayon::join` orders
every cross-thread read: hash words are `Relaxed`. The state byte is swapped in `Release`
after the hash and read `Acquire`; the swap hands the old state to exactly one writer, so the
per-depth hashed count moves once per transition even when two tasks set the same slot
(concurrent readers of a frontierless tree each build the root).

At **F = 0** the whole tree is one subtree at `(0, 0)`, descended sequentially. There is no
separate small-tree code path (§15).

**Advance:** when depth `F+1` is complete — every position at `F+2` hashed, one counter load
— level `F+1` is persisted as frontier rows and `F` moves. Nothing moves in memory. Capped by
`frontier_cap`. (A leaf-count gate was removed: completeness at D implies 2^(D+1) leaves;
§17.3.)

## 3. Batch insertion

1. `sorted_unique_entries`: stable sort, dedup keeping the *last* value.
2. `upsert` descends positionally from the root, splitting the sorted slice at bit `d` with
   `partition_point` (zero-copy). Above the frontier disjoint children recurse via
   `rayon::join` when the slice exceeds `JOIN_THRESHOLD = 8` (swept 2..=64: plateau to 32,
   −9.5 % at 64; §15.2). From the frontier down it is sequential. An untouched child costs
   one array read. The first position whose children are unknown is where the subtree is
   **merged from disk**: `merge_with_disk` scans its leaves, `build_subtree` recomputes
   bottom-up and records every covered position. Bottoming out at the first unknown
   position rather than at the frontier child: 22.8 vs 37.3 leaves read per insert (§5.5).
   Children are addressed by position, not stored prefix — the compressed child covers the
   same leaf range, so the scan is identical and the merge recovers the compressed root.
   The descent stages nothing.
3. `stage_batches`, after the descent returns: every entry once as a leaf record, plus one
   frontier row per touched subtree from the hashes at `F+1`. Staging rows only after the
   whole descent excludes a stale row by construction (§3.1.0b). The batch is cut at
   ~thread-count points, each advanced to the next frontier-subtree boundary, so **a
   subtree's leaves and its row share one `WriteBatch`** — the batches commit independently
   and a crash can keep any subset, and a row surviving without its leaves would describe a
   tree the leaves do not (§23.5). Per-subtree batches spent ~35 % of worker time forming
   write groups (§6.1); one batch serialises the memtable pass (§5.7).

**Visit-once invariant:** the descent enters each position at most once per batch
(`note_visit`, debug builds). It is what makes staged-but-uncommitted leaves safe to hide
from range scans, and what lets a proven-empty slot authorise building a subtree from the
batch alone with no scan — there is no delete path, so `Empty` is always current. Both fail
silently, by losing leaves, if it stops holding.

The batch wins are structural: a node shared by *k* entries is hashed once, and disjoint
subtrees parallelise without locks.

## 4. Commit protocol and durability

Per `batch_upsert`, in order:

1. the staged batches, committed concurrently (RocksDB parallelises memtable insertion only
   across a write group with more than one writer);
2. each newly complete frontier level, in chunks of 2^16 rows (one batch for depth 23 held
   8.4 M puts and took 69.8 s), the **last chunk carrying the metadata** naming the level;
3. leaf count and frontier depth in one metadata batch;
4. `flush_wal(true)`.

The WAL is one ordered log, so: a frontier row is never recovered without its subtree's
leaves; a frontier depth never without its whole level; a crash inside an advance leaves
orphan rows of a level nothing names, which the open ignores and the next advance rewrites
whole.

**Durable per batch, not atomic.** A crash can keep any subset of step 1's batches; the
recovered tree is internally consistent but reflects a state the caller never saw committed.
The lasting trace is the **leaf count**: committed after the leaves and never recounted, it is
a lower bound after such a crash, permanently (a re-submitted batch adds only keys not on
disk). Only `leaf_count()` and the tree-top sizing read it; no hash depends on it. One
atomic `WriteBatch` per upsert measured −5.7 % against per-subtree commits (§5.7) and
≈ −16 to −19 % implied against the parallel drain (§15); declined.

**Panic mid-batch:** the tree top and count are already ahead of disk, so the tree marks
itself poisoned and every later call panics. Recovery is drop and reopen. The staging is
plain owned values, so a panic leaves nothing half-built.

Census tallies ride inside each `WriteBatch` and land in the shared counters at commit;
per-record global RMWs became contended-cache-line wall time once staging was a burst (§19).

## 5. Read path and cost

A fault-in descends the known levels (`Empty` is proven, no probe), range-scans the leaves
under the first unknown position — one bounded iterator over `length ‖ key`, which clusters a
subtree's leaves — merges that run with the batch's sorted entries in one pass, hashes
bottom-up and records every position. It stages only the batch's leaves.

With L = N / 2^F leaves per frontier node, a fresh process reads ~L/2 leaves per faulted
subtree (~L/4 long-lived). Past the frontier cap this is **O(N) read amplification** in
leaves: 18–23 leaf reads per insert at 40 M leaves (L = 76); ~57 at 1.0 B (L = 119), where
throughput is an I/O-latency number — workers ~25 % CPU-active, the seek across sorted runs
costing about twice the scan it sets up (§8.1). In *blocks* it is flat until N moves the
block floor (§2).

## 6. On-disk format (v4)

**Keys**: `length_be_u16 ‖ key`, 34 bytes. Depth-major, so a level is one contiguous scan
and a subtree's leaves are contiguous in key order. Metadata keys `__mpt_complete_depth__`
and `__mpt_leaf_count__` start with `_` (0x5F) and sort above length 257, outside every node
scan. `storage/codec.rs` is the only module that knows the layout.

**Records** are untagged; the key's length says the kind.

| record | bytes | recomputed |
|---|---|---|
| leaf (length 256): the value | 32 | merkle hash from key and value |
| frontier row: `left ‖ right` child hash | 64 | node hash; child prefixes are positional |

Metadata: the two keys, big-endian, committed together.

**Refused at open, never guessed at:** a frontier depth above `MAX_DEPTH − 2` (the metadata
is untrusted input and depth F allocates 2^F slots); a level whose row count is not 2^F; one
metadata key without the other (torn); interior rows with no metadata naming them (a stripped
database — opening it frontierless would turn every batch into a whole-database merge); a
key with a set bit past its length; a record of the wrong length. Format v3 records are one
byte longer (a tag); the last v3-capable build is commit `acff956`.

A frontierless database (`complete_depth == 0`) needs no recovery pass: the count is seeded
from metadata or an exact leaf count, and the root is rebuilt lazily from one leaf scan.

## 7. RocksDB configuration and build

Options are hardcoded; thirteen public tuning options were measured down to none (§7.3).

| option | value | why |
|---|---|---|
| plain `DB`, no transactions | | single writer; OptimisticTransactionDB was +1.7 % noise and complexity (§3.3.4) |
| `enable_pipelined_write` | on | +5.3 %; giving it and `manual_wal_flush` up together, −26.8 % |
| `manual_wal_flush` | on, one `flush_wal(true)` per batch | |
| `write_buffer_size` | 64 MB (default) | 256 MB within noise (§5.7) |
| block cache | 1 GiB LRU | larger bought RSS only, at both scales (§7). Not a row cache: the hot reads are iterators |
| block size, compression | 4 KiB, Snappy (defaults) | §3.3.5, §3.3.6 |
| `max_open_files` | 4096 | −1 hit EMFILE (BASELINE §1.5) |
| `max_background_jobs` / `max_subcompactions` | 8 / 4 | |
| statistics | `ExceptHistogramOrTimers` | cheapest level that still counts the tickers the census samples |

`unordered_write`: +5.9 % at 40 M, −0.3 % at scale; RocksDB 8.10 self-deadlocks on it with
`manual_wal_flush` and rejects it with `pipelined_write`. `LEAVES_PER_BLOCK = 4096 / 66` is
derived from the codec; random 32-byte keys and values do not compress, so it is honest.

**Build flag.** `.cargo/config.toml` sets `CXXFLAGS=-DROCKSDB_SCHED_GETCPU_PRESENT`.
`librocksdb-sys` never defines it, so `port_posix.cc`'s `PhysicalCoreID()` falls back to
`__get_cpuid(1, …)`. Clang's x86-64 `__cpuid` parks `%rbx` in an *unconstrained* `"=r"`
scratch operand; LLVM may pick `%rax` for it, and for the leaf-1 call it does, so the
function runs `cpuid` on a garbage leaf and clobbers callee-saved `%rbx` without spilling it.
`ConcurrentArena::Repick()` keeps a live pointer there and segfaults — only when two threads
contend for one arena shard, so it looks like a rare crash under concurrent memtable writes
(`concurrent_batch_writes_do_not_corrupt_the_memtable` pins it). `sched_getcpu()` is what
upstream's `build_detect_platform` selects on Linux anyway. Cargo skips the `[env]` entry if
`CXXFLAGS` is already set, and it does not reach dependants of this crate.

## 8. Configuration

Two knobs. `RocksFrontierConfig::with_max_depth(d)` (`Default` is 26) is a **ceiling** on
the levels held; `with_depths(d, cap)` also names the deepest frontier to advance to
(`d − 2` by default). `d ∈ 2..=MAX_DEPTH (28)` — 28 levels are 17.7 GB — and `cap < d`, since
the frontier's children must be held. They are separate because conflated, "let the frontier
reach 25" meant "hold 17.7 GB of levels no scan reads"; split, a deeper frontier costs one
gate level (§21.9).

Operating points: 26 for production (frontier ≤ 24); 22 for the reference-database regime
(BASELINE §5.1 records it as `--max-frontier-depth 20`; `d = cap + 2`). Tests use
`test_config()`: `(6, 4)` with one leaf per block, so a few hundred leaves exercise every
level rule.

## 9. Recovery

Open reads the metadata, streams level F (64 B/row) into depth `F+1` in parallel positional
ranges, checks the row count, then derives every ancestor in one parallel pass per level.
Ranges are sized by their stride so none starts past the level: fixing the range count and
rounding the stride up let a range start past 2^F for non-power-of-two thread counts, where
the index wrapped and the tail was counted twice. One sequential iterator was most of a
16 s open; at F = 24 the parallel load reads 2^24 rows (~1.02 GiB) in ~38 s against the
1.8 B-leaf database. Leaves are faulted in on demand.

## 10. Performance envelope

| regime | configuration | inserts/s |
|---|---:|---:|
| 40 M leaves, resident, F = 19 | reference DB, fixed-work A/B | ~305 K |
| build 40 M from empty | `d = 22` | ~347 K |
| 1.0 B leaves, DB ≈ 1.1× RAM, F = 23, L = 119 | Tier 0, cold cache | ~95–97 K |
| 1.81 B leaves, DB ≈ 2.1× RAM, F = 24, L = 108 | Tier 0, post-compaction | ~116 K |
| same, build average from empty | five chained processes | ~142 K |

The write path is near its floor: 1.91–1.99 record puts and ~160 staged bytes per insert,
flat across three orders of magnitude. The scale-growing cost is sibling-leaf I/O. The
strongest lever is **LSM shape**: one sorted run instead of ~13 is +27–28 %, decaying over
~13 % of the tree's worth of churn, so idle-window compaction (`compact-db`) is worth ~+20 %
at production-like residency, while maintaining the shape concurrently loses everywhere
(§8.2, §10, §12). Residency beats every software candidate: +140 % resident vs Tier 0 on the
same binary (§12.6).

## 11. Rejected directions

Built or bracketed, then rejected; mechanism and numbers in REVIEW.md.

- One atomic `WriteBatch` per upsert: §4.
- Band deletion / `keep_below_frontier = 0`: −8 to −12 % resident (§15).
- `unordered_write`: §7.
- Bloom filters + prefix extractor: −2.8 % at F = 19 (§6.2), then +0.7 % over 9 reps at
  Tier 0 with the mechanism verified working — 59 % of files skipped, −29 % key comparisons,
  converting to nothing because the bill is the leaves actually read (§21).
- Block cache above 1 GiB: noise (§7).
- Subtree pages + merge operator: behind at every operating point, and an operator-less open
  silently truncates data (§9).
- Read-side I/O concurrency (oversubscription, `async_io`/io_uring): wins only at
  harsher-than-production residency (§11).
- Rotating / sharded compaction: −9.4 % resident; compaction bytes are set by churn, not
  scheduling (§12).
- A prefix-addressed small-tree regime, a frontier-cap/retention-floor pair, a
  `RocksStorageConfig`, a `ROOT_KEY`: each deleted after the record showed it executes at no
  operating point or duplicates a derivation (§15–§17, §21.9).
- Untried, with the gates each must pass: Verkle-style commitments, a B-tree engine,
  two-tier Merkle (§12.6).

## 12. Instruments

**`bench`.** The protocol harness: 100 K-entry windows, sorted, in 10 K batches (sorted
windows: +12.3 % at 40 M leaves, equal within noise at 1 B, same root either way — so not a
flag; §7.2). The key stream is wyrand, bit-identical to `fastrand` 2.x, so a recorded seed
still names the same keys and root hash. The seed is random unless `--seed` is given and is
printed in the report and the log; a fixed default made a bare run against a database built
with it a pure update workload, leaf count never moving. The log is
`<db>/bench-log.jsonl`, appended, so a database carries its own history across the dozens of
runs that build a large one; JSON *lines* because a run is routinely killed with ^C and a
truncated last line then costs exactly that line. Records:

- `run` — seed, batch shape, depths, tree size at open;
- `batch` — `timestamp`, `elapsed_secs`, `total_inserted`, `leaf_count`, `frontier_depth`,
  `batch_entries`, `batch_secs`, `sorted_runs`, `rss_bytes`;
- `census` — once a window and at exit: raw counts (`leaf_puts`, `interior_puts`,
  `bytes_staged`, `write_batches`, `subtree_loads`, `leaves_read`, `data_blocks_read`,
  `index_blocks_read`, `seeks`) with the `entries` they cover, so read cost is a curve
  against tree size.

The stderr report is parsed by `tools/ab.py`: treat its format as an API.

**Census.** Relaxed counters at the storage choke points, plus three metrics sampled from
RocksDB's tickers (`census_snapshot` re-reads them, `census_reset` re-baselines them; the
tickers count from open). `puts/insert` is the write headline (11.30 → 2.56 → 1.91 over the
campaign); `blocks read/insert` the read headline; `leaves read/insert` is structural and
keeps falling after the cost has stopped (§2). Level persistence bypassed the counters
before round 15 (§17.5), so from-empty `puts/insert` is not comparable across that boundary.

**`tools/ab.py`** — interleaved fixed-work A/B on scratch copies (`--link-copy` hardlinks
SSTs), page cache dropped per run, medians and spreads, root-hash equality as the
correctness gate. Fixed work rather than fixed time because a faster variant reaches a
bigger, slower tree within a timeout and a fixed-time comparison understates the win.
**`compact-db`** — collapse a database to one sorted run. **`tools/memhog.c`** — pins RAM
to reproduce Tier-0 residency against a small database (BASELINE §5.5).
**`tools/bench_plot.py`** — bench logs to an HTML/PNG report. **`tools/bench_loop.py`** —
runs `bench` in a loop against one growing database.

**Tests.** Root-hash parity against `SimpleMPT` (a different algorithm: single-key,
incremental, over a node store); exhaustive small-shape reopen oracles; byte-level format
pins and refusal pins; sequence-number no-rewrite pins; torn-commit and torn-advance
emulations; a counting-allocator harness for open-time memory. Test databases are left under
`target/` for inspection.

## 13. Known limits

- A batch is not atomic on disk; the leaf count is a lower bound after a crash (§4).
- Read amplification is O(N) in leaves past the frontier cap (§5); in blocks, flat until the
  block floor moves (§2).
- Growth pays for the levels below the frontier: building from empty at the default
  configuration is ~−8.7 % vs the pre-slot representation (§6.2), and each new bottom level
  is unknown until faulted in, one extra scan per subtree, once.
- Tree-top memory is bounded by the configuration, not the data: `2^(d+1) − 1` positions at
  33 bytes once the tree is big enough to ask for `d`.
- Pre-v4 databases do not open (§6).
- Read-side conclusions require a larger-than-RAM database: on a resident working set fewer
  leaf reads do not convert into throughput (§5.4).
