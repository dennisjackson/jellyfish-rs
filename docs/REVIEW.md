# jellyfish-rs — Design & Code Review

*Scope: all four MPT implementations, the shared batch algorithm, and both storage
backends, with emphasis on `RocksTransRelMPT` (the frontier design). Line references
are to the current `chore/fmt-and-clippy` branch. Empirical numbers are taken from
`bench_logs/` (runs against `bigdb`, ~1×10⁹ entries, 124 GB on disk).*

*Sections 1–4 were written against `bigdb`, which no longer exists. Section 5 is
measured on the 40 M-leaf reference database that replaced it
(BENCHMARK-BASELINE.md §5.2). Where a later round falsified an earlier claim, the
correction sits next to the claim rather than replacing it.*

---

## 1. The implemented algorithm

### 1.1 Tree model

The structure is a **binary Merkle-Patricia trie over 256-bit keys** (SHA-256-sized),
with path compression — closer in spirit to Diem's Jellyfish Merkle Tree than to
Ethereum's 16-ary MPT, but binary and bit-addressed.

- A node is identified by its **`Prefix`** — a `(hash: [u8;32], length: u16)` pair
  naming the bit-string from the root to that node (`src/prefix/mod.rs:41`). Leaves
  always have `length == 256`; interior nodes sit at the first bit where two subtrees
  diverge (Patricia compression), so an edge can skip many bits.
- Nodes are stored in a flat map keyed by `Prefix` — **location-addressed, not
  content-addressed**. Updates rewrite a node in place under the same key; there is no
  copy-on-write versioning and no garbage of old versions (a deliberate simplification
  that pays off in write volume).

  > **Corrected — no longer how the rocks backend holds the top of the tree.**
  > Everything at depth < F now lives in `TopLevels`, a flat array of 32-byte hashes in
  > implicit-heap order, and the frontier level itself in `FrontierLevel` at 64 bytes a
  > node; the map holds only what a batch materialises below the frontier. The
  > location-addressing property above is exactly what makes that possible — above a
  > complete level the trie is a perfect binary tree, so a node's prefix is a function of
  > its `(depth, index)` and nothing but the hash needs storing — so the design claim is
  > unchanged and the sentence describing its implementation is not (§5.3).
- Hashing (`src/mpt/mod.rs:38,80`):
  - leaf: `SHA-256("leaf" ‖ key ‖ value)`
  - interior: `SHA-256("interior" ‖ prefix.hash ‖ prefix.length ‖ left_hash ‖ right_hash)`

  Interior hashes commit to their own prefix, and child hashes transitively commit to
  the child positions, so the root hash binds the full key→value mapping. Domain
  separation between leaf/interior is present. One nit: the interior hash includes the
  *bits after the prefix* of `prefix.hash` only because `Prefix` construction zeroes
  them (`zero_bits_from`); the hash's soundness silently depends on that invariant
  being maintained everywhere a `Prefix` is built by hand.

### 1.2 The batch-insert recursion

All batch-optimized implementations share one algorithm (in-memory/SQLite:
`src/mpt/batch_ops.rs`; a slice-based re-implementation with persistence hooks:
`src/mpt/rocks_frontier/mod.rs:475`):

1. Sort the batch by key and dedupe (`sorted_unique_entries`, `src/mpt/mod.rs:201`).
2. Recursively descend from the root. At each node:
   - **Empty slot** → insert the first entry as a leaf, recurse with the rest.
   - **Leaf** → binary-search the batch for an exact-key update; otherwise split the
     leaf by creating an interior node at the common prefix.
   - **Interior** → separate entries *contained* by the node's prefix from *divergent*
     ones (divergent entries force a new parent above this node); partition contained
     entries into left/right by the bit at `prefix.length`, and recurse into both
     children — **in parallel via `rayon::join`**.
3. On the way back up, recompute each touched interior hash exactly once.

Because sorted entries destined for a subtree are contiguous, the rocks variant
partitions with `partition_point` on slices (zero-copy); the in-memory variant
re-allocates `Vec`s at each interior node.

This gives the two headline wins the README claims: an interior node on the shared
path of *k* batch entries is rehashed once instead of *k* times, and left/right
subtrees are fully independent, so the recursion parallelizes cleanly.

### 1.3 The frontier design (`RocksTransRelMPT`)

The RocksDB implementation adds a **persistence/memory boundary**:

- **`complete_interior_depth` (the frontier)** — the largest depth *F* at which all
  2^F nodes exist and are interior. Everything at depth ≤ F (+ a small margin) is
  held in memory; leaves and interiors below F live only in RocksDB (plus
  transiently in memory while being operated on).
- **Writes**: leaves are always persisted; interior nodes are persisted only in the
  band `[F, F + depth_to_write)` (`should_persist_depth`,
  `src/mpt/rocks_frontier/mod.rs:135`). A `WriteBatch` is opened when the recursion
  crosses the frontier ("boundary batch") so each frontier subtree commits
  atomically; root and frontier metadata are committed in a final transaction, then
  `flush_wal(true)` provides batch-granularity durability.
- **Reads**: to modify anything below a frontier node, the *entire* leaf set under
  that node is range-scanned from RocksDB (`get_leaf_nodes_by_prefix`, exploiting
  the `length ‖ hash` key encoding that clusters all leaves contiguously and in key
  order) and the subtree is rebuilt in memory, hashes recomputed
  (`load_subtree_from_storage`, `src/mpt/rocks_frontier/mod.rs:284`).
- **Frontier advance**: after each batch, check whether depth F+1 has become complete
  (all 2^(F+1) nodes interior); if so, persist that level and advance
  (`update_complete_interior_depth`). Advance is gated by an *estimated* leaf count
  so the frontier stays `log_leaves_per_frontier` levels above the true leaf level,
  and hard-capped by `max_frontier_depth = 24`.
- **Recovery**: read the 2^F frontier interiors (one contiguous scan, thanks to the
  depth-major key encoding) and rebuild all ancestors bottom-up
  (`rebuild_tree_from_interior_nodes`); leaves are only faulted in on demand. Falls
  back to a full leaf scan if the frontier level is inconsistent.
- **Pruning**: after each batch, `prune_below_frontier` drops in-memory nodes deeper
  than `F + keep_below_frontier` (also always keeping depth ≤ `depth_always_keep`).

> **Corrected — four of these bullets, in round 3.** The boundary itself is unchanged;
> what happens either side of it is not.
>
> - *Reads.* A fault-in no longer rebuilds the subtree into the store. The subtree after
>   a batch is the leaves on disk merged with the batch's entries for it, so it is
>   computed in one pass over two sorted runs, materialising only the levels the prune
>   keeps: 62.3 → 2.6 store inserts per inserted entry, 58.5 → 0.0 removals (§5.5).
> - *Reads, again.* An untouched sibling used to be faulted in purely to read its hash;
>   an interior now records its children's hashes, so it is not touched at all —
>   1.995 → 0.977 subtree loads per insert (§5.2).
> - *Memory.* Depths < F and depth F are flat arrays, not map entries (§5.3).
> - *Pruning.* `prune_below_frontier` still runs, and now has nothing to remove: the
>   eviction machinery existed to undo work that no longer happens. `depth_always_keep`
>   is also capped at `max_frontier_depth − 1`, so it can no longer act as a permanent
>   floor below the frontier in a scaled-down configuration (§5.4).
>
> *Writes* are unchanged in volume — the census measures 1.911 record puts per insert at
> both ends of the session — and the batch that spans a frontier subtree is now opened by
> `upsert_frontier` and staged after the recursion, with the hashes it returned.

This achieves the stated goal: **O(1) durable writes per insert** (1 leaf + a shared
frontier interior + the extra `depth_to_write` band), at the cost of **reading
N/2^F leaves per touched frontier subtree** and holding the top of the tree in memory.

---

## 2. Asymptotic performance

Notation: *N* = leaf count, *B* = batch size (post-dedupe), *F* = frontier depth
= min(⌈log₂N⌉ − 1 − `log_leaves_per_frontier`, 24), *L* = N/2^F = leaves per frontier
subtree. Keys are SHA-256 outputs, so expected trie depth is ~log₂N; the adversarial
worst case replaces every log₂N below with 256 (keys are hashes, so this needs a
preimage-style effort to trigger, but nothing in the code bounds it).

### 2.1 In-memory batch upsert (`BatchMPT`)

- **Hash work**: the union of B random root-to-leaf paths in an N-leaf trie has
  ≈ 2B + B·log₂(N/B) distinct nodes; each is hashed once ⇒
  **Θ(B·log(N/B) + B)** SHA-256 compressions, vs Θ(B·log N) for B sequential
  inserts — the batch saving is the log B factor on the shared upper levels.
- **Traversal overhead**: each entry is examined at every level of its path, and
  `Prefix::contains` at depth d costs O(d) *bit* operations
  (`src/prefix/mod.rs:137`), giving **O(B·log²N)** bit ops.
  ~~At N = 10⁹ this is ~900 bit-loop iterations per entry, comparable in
  wall-time to the hashing itself.~~ **Corrected — this was wrong, and measurement
  says so.** Summing d over a root-to-leaf path at N = 10⁹ (depth ≈ 30) gives
  ≈ 465 bit-loop iterations per entry, against ≈ 30 SHA-256 compressions on the
  same path at roughly 500 cycles each. The bit loops are on the order of a few
  percent of the hashing, not comparable to it. An A/B of the in-memory backend
  before and after the word-level rewrite of §3.2.1 found no difference outside
  noise (628–630 K vs 622–627 K entries/s). Hashing dominates this path.
- **Allocation**: `batch_ops.rs` clones the entry vector at every interior split ⇒
  O(B·log N) element copies; the rocks variant avoids this with slices.
- **Span** (parallel critical path): O(log N · log B) — near-linear speedup on
  random keys, modulo the missing join threshold (§3.2).

### 2.2 `RocksTransRelMPT` per batch of B (steady state, N large)

| Resource | Cost | At N = 10⁹, B = 10⁴ (measured config: F = 23, L ≈ 60–120) |
|---|---|---|
| Disk seeks (reads) | ≈ min(B, 2^F) range-scan seeks — one per distinct frontier subtree touched | ~10⁴ seeks/batch → this is the observed bottleneck (~3–4 K inserts/s) |
| Bytes read | ≈ B·L leaf records (~130 B each) | ~10⁴·100·130 B ≈ 130 MB/batch |
| Rebuild CPU | ≈ 2·B·L SHA-256 + trie construction | ~2×10⁶ hashes/batch |
| Durable writes | ≈ B leaves + touched frontier interiors + `depth_to_write` band ⇒ **O(1)/insert** app-level; ×RocksDB LSM write-amp (~10–30× leveled) | small vs reads |
| New-node insert CPU | Θ(B·log(N/B)) hashes above the frontier | ~3×10⁵ hashes |
| Per-batch fixed costs | `prune_below_frontier` full-map `retain` **O(\|store\|)**; `estimate_leaf_count` ⇒ up to 100 extra range scans; metadata txn + `flush_wal` | \|store\| ≈ 2^F+ⁿ ⇒ tens of millions of map entries scanned *every batch* |
| Memory | all interiors at depth ≤ F+2 ⇒ **Θ(min(N, 2^26))** nodes, plus the DashMap pre-allocation bug (§3.1.3) | observed ~40 GB RSS (mostly §3.1.3) |
| Recovery | one scan of 2^F interiors + Θ(2^F) rebuild hashing | measured ~50 s init |
| Point lookup | O(1) map hit above frontier; 1 seek below | — |

> **Corrected — four rows, and one caveat that matters more than the four.** The round-3
> figures below are from the 40 M-leaf reference database (F = 19, L = 76), not from
> `bigdb`.
>
> - *Disk seeks.* "One range-scan seek per distinct frontier subtree touched" understated
>   it by about 2×. An interior did not record its children's hashes, so rehashing a
>   parent faulted in the child the batch had *not* touched, for a subtree whose hash
>   cannot have changed: 1.995 subtree loads per insert measured, 0.977 after the fix
>   (§5.2).
> - *Bytes read.* "~130 B each" was never measured; the leaf record was 107 raw bytes,
>   and the compact leaf record took it to 75 (§3.3.6).
> - *Rebuild CPU.* The "trie construction" half is gone — a faulted-in subtree is
>   computed rather than materialised (§5.5) — and the hashing is now bounded by the
>   levels a prune keeps rather than by the whole subtree.
> - *Per-batch fixed costs.* The full-map `retain` was replaced in round 2 (§3.1.5) and as
>   of §5.5 has nothing left to remove. *Memory*: "all interiors at depth ≤ F+2" is no
>   longer how they are held (§5.3).
>
> **The read-bound reading now rests on `bigdb` alone, and `bigdb` is gone.** Round 3
> cannot corroborate it and did not try: at 3 GB against 62 GB of RAM the reference
> database's working set is resident. §5.4's `keep_below_frontier = 3` row is the direct
> test — leaf reads fall from 22.8 to 20.3 per insert and throughput drops 4.2 %. Fewer
> reads did not convert. That refutes nothing about `bigdb`; it says the question is
> unanswerable on a database that fits in RAM, and that **a larger-than-RAM reference
> database is a prerequisite for trusting any read-side conclusion at all** — including
> the ones this table is used to justify.
>
> One caveat on the paragraph below predates round 3 and was never brought back here:
> 49.6 % is the *worst* of ten consecutive runs and is the F = 21 row taken minutes
> before the frontier advanced; at F = 23 the same counter reads 8.8 % rising to 50.6 %
> as L grows (BENCHMARK-BASELINE.md §6.3).

A third measurement, taken later and more important than either observation below:
**the benchmark spends about half its wall clock stalled.** `bigdb/LOG` records
`Cumulative stall: 00:59:29.277 H:M:S, 49.6 percent` — RocksDB throttling the
foreground writer because compaction cannot keep up, with compaction saturating its
six slots. Cumulative ingest is 357.7 GB against 2902 GB of compaction writes, so
**write amplification is ~8.1×**. That means the system is not purely read-bound as
the table above implies: it is read-IOPS-bound for the half of the time it is running,
and compaction-bound for the other half. Anything that reduces bytes written pays
twice.

Two structural observations:

1. **The design intends a read/write trade-off knob (`depth_to_write`) that the read
   path never uses.** Extra interior levels below the frontier are written
   (`should_persist_depth` spans 3 levels) but `load_subtree_from_storage` always
   re-reads *all* leaves under the frontier node and rebuilds from scratch. The
   README's "every additional write halves the number of leaf reads" is currently
   write-only cost with zero read benefit.

   > **Corrected: it is not even write-only cost — the knob does nothing at all.**
   > I proposed setting it to 1 to halve the interior writes per insert. The test
   > written to demonstrate that saving showed there was none: with
   > `depth_to_write = 3` the deepest persisted interior is the frontier level,
   > exactly as with 1. `batch_upsert_at_interior` only adds an interior to a write
   > batch on the arm that *opens* the boundary batch, and that arm requires
   > `active_batch.is_none()`; with a complete level at F every root-to-leaf path
   > crosses exactly one node at depth F, so the batch is opened there and passed
   > down, leaving the deeper `persist_here` arms unreachable. The default is now 1
   > and the field documents its own inertness, pinned by
   > `test_depth_to_write_above_one_is_inert`. Implementing the trade-off for real
   > means writing *and* reading the band; neither side exists today.
2. **The frontier cap turns the design from O(1)-read to O(N)-read per insert past
   2^24·(2^`log_leaves_per_frontier`) leaves**: L grows linearly with N once F is
   capped, and with it both bytes read and rebuild hashing per insert. The observed
   throughput decay from earlier runs to the 1B-entry runs is consistent with this.

### 2.3 `DurableBatchMPT` (SQLite)

As designed: every node on every touched path is rewritten ⇒ **Θ(B·log N) row writes
per batch (O(log N) write amplification per insert)**, one fsync'd transaction per
chunk (≤1000 entries). `pre_advise` loads path+siblings in BFS rounds with batched
`IN` queries; the code itself flags the O(n²) behavior of its chunk loop. This
implementation is the baseline the frontier design was built to beat; I'd spend no
optimization effort here beyond what §3.4 notes.

---

## 3. Findings and opportunities

Ordered by expected impact. **[C]** = correctness, **[P]** = performance, **[M]** = memory.

### 3.1 High impact

**3.1.0 [C] Concurrent batch inserts segfaulted: a clang miscompile of RocksDB.**
Found while establishing a test baseline, not present in the first draft of this
review. `cargo test --release` died with SIGSEGV in
`mpt::rocks_frontier::tests::test_incremental_batch_inserts` — 100 % reproducible
with the default rayon pool, never with `RAYON_NUM_THREADS=1`.

It is a toolchain bug, not a bug in this crate. `librocksdb-sys` never defines
`ROCKSDB_SCHED_GETCPU_PRESENT`, so `port::PhysicalCoreID()` compiles its
`__get_cpuid(1, ...)` fallback. Clang's x86-64 `__cpuid` preserves `%rbx` through an
*unconstrained* `"=r"` scratch operand, and for the leaf-1 call LLVM allocates
`%rax` — simultaneously the leaf input and cpuid's `EAX` output — so the parked
`%rbx` is destroyed:

```
mov  $0x1,%eax     ; leaf
xchg %rax,%rbx     ; scratch == leaf input == cpuid output register
cpuid              ; garbage leaf, and clobbers the parked %rbx
xchg %rax,%rbx
```

`%rbx` is callee-saved and the function never spills it, so any caller holding a
live value there loses it. `ConcurrentArena::Repick()` does exactly that
(`mov %rdi,%rbx; call PhysicalCoreID; mov 0x50(%rbx),%ecx`) and faults. `Repick()`
runs only when two threads contend for the same memtable arena shard, which is why
it needed concurrency to show up, and why `RAYON_NUM_THREADS=1` hid it.

> **Implemented** — `.cargo/config.toml` passes
> `-DROCKSDB_SCHED_GETCPU_PRESENT` so `PhysicalCoreID()` uses `sched_getcpu()`,
> which is what upstream RocksDB's `build_detect_platform` selects on Linux
> anyway. `port_posix.cc` is the only file in vendored RocksDB that includes
> `<cpuid.h>`, and it already includes `<sched.h>`, so the fix is complete.
> Covered by `concurrent_batch_writes_do_not_corrupt_the_memtable`, which
> crashes 10/10 without the flag and passes with it.

**3.1.0b [C] Two reopen bugs at the frontier's first level.** Both found while chasing
the redundant split-partner write in 3.1.5, neither present in the original review, and
both confirmed by running rather than reasoning.

*Data loss.* `batch_insert_into_empty` created a leaf, counted it, then persisted it only
`if let Some(batch)` — and `batch_upsert_optimized` starts the recursion with `None`. A
batch of **exactly one entry into an empty database** left the leaf in memory only, while
the final transaction stored `ROOT_KEY` pointing at a record that was never written. On
reopen, `get_root_hash()` returned `None` and the entry was gone. Two or more entries were
always safe, and the reason is the punchline: the second entry splits the first, and the
split arm's local batch — the write 3.1.5 proposed deleting — is what made the first leaf
durable. Removing it without this fix would have converted a narrow edge case into general
data loss. Over 3000 seeds shaped like `test_rocks_impl_randomized_reopen_batches`, the
baseline lost 556 leaves, precisely the seeds whose first batch held one entry.

*Silent wrong root hash — worse.* The boundary arm of `batch_upsert_at_leaf` stages the
interior it just created and *then* recurses beneath it with that batch open. The
recursion grows the subtree and updates the interior in memory, but never re-stages it,
because `batch_upsert_at_interior` writes an interior only on the arm that *opens* a
boundary batch. The committed row therefore describes two leaves. That row sits at
`complete_depth`, which is exactly the level `load_interior_nodes_from_storage` reads on
reopen to rebuild every ancestor from — so recovery succeeds, skips the full leaf scan,
and returns a root computed from a stale hash. **Every leaf present, root hash wrong,
nothing detects it.** A new oracle (`tests/reopen_root_oracle.rs`) walking every subset of
the 3-bit prefix space at every batch split up to four keys found **80 of 674 shapes
affected**; all were three or more keys in a single first batch, which is why no existing
test caught it.

> **Both implemented.** New leaves are persisted at creation, and the boundary arm
> re-stages the frontier interiors from `store` before committing. The durability fix
> deliberately takes the shape that does *not* incidentally mask the second bug: a
> differently-shaped fix suppressed it as a side effect, which would have left the
> mechanism live and undocumented.

**3.1.1 [C] `sorted_unique_entries` keeps the *first* value for a duplicate key, not
the last** (`src/mpt/mod.rs:201`). `Vec::dedup_by_key` removes all but the *first* of
consecutive equal elements, and the stable sort preserves input order — so for a batch
containing `(k, v1), …, (k, v2)`, `v1` wins, contradicting both the doc comment
("keeping the last value") and normal upsert semantics. Fix: after the stable sort,
dedupe keeping the last of each run (e.g. reverse-scan, or `sorted.reverse()` between
sort and dedup with a reversed comparator). Affects every implementation.

> **Implemented** — `sorted_unique_entries` now uses `dedup_by`, copying the
> later element's value into the retained earlier one, so the last write wins at
> O(n log n) with the output still sorted ascending. The consequence was worse
> than stated above: `SimpleMPT` upserts one key at a time and so kept the *last*
> value, while every batch implementation kept the *first* — the backends
> disagreed on the root hash for any batch containing a repeated key. Three tests
> cover it, all of which fail without the fix:
> `test_sorted_unique_entries_keeps_last_value` (the helper directly),
> `test_batch_upsert_duplicate_keys_in_batch` (which previously asserted only
> `is_some()`, so it passed either way), and
> `test_cross_impl_root_hash_duplicate_keys_in_batch` (batch root vs sequential
> root across all four implementations).

**3.1.2 [C] Batch atomicity is torn on crash.** A single `batch_upsert` commits many
independent `WriteBatch`es (one per boundary subtree) plus a final metadata
transaction. A crash mid-batch leaves some subtrees updated and others not; on
restart, recovery rebuilds from frontier nodes and *re-reads all leaves*, so the
partially-applied entries silently become part of the recovered state, and the
recovered root hash corresponds to a state the caller never saw as committed. Each
subtree is internally consistent, so nothing detects this. If batch atomicity
matters (the README's "durability after a batch insert completes" suggests it does),
accumulate **one** `WriteBatch` per `batch_upsert` — all subtree nodes + root +
complete-depth metadata — write it once, then `flush_wal(true)`. This is also
faster (one write syscall path, no OptimisticTransactionDB validation) and would let
you drop the transaction machinery entirely (§3.3.4).

> **Refuted on the throughput half — built, measured, reverted.** One `WriteBatch` per
> `batch_upsert` is a **consistent 5.7 % regression** (117,924 → 111,239 entries/s, five
> interleaved repetitions, no overlap between the two groups). The mechanism is in
> RocksDB's `WriteThread::EnterAsBatchGroupLeader`: memtable insertion is parallelised
> across a write *group* only when the group has more than one writer, so collapsing the
> ~9,100 small batches a run issues into one serialises onto a single skiplist pass what
> up to 64 rayon workers were doing concurrently. "One write syscall path" is true and
> is not where the time goes.
>
> The correctness half stands untouched: this is still the only change that closes the
> torn-batch gap described above, and it is still the enabler for §3.3.4. It is a
> correctness purchase at a measured price of 5.7 %, not a throughput win — and it was
> reverted here because the caller has confirmed batch atomicity is not a requirement.
> Numbers in §5.7.

**3.1.3 [M] `DashMap::with_capacity(2^(complete_depth + 4))`**
(`src/mpt/rocks_frontier/mod.rs:380`) is almost certainly the dominant RSS cost. At
the measured `complete_depth = 23` this requests capacity 2^27; hashbrown rounds
bucket count up to 2^28, and at ~140 B per `(Prefix, Node)` slot that is **~38 GB
allocated at open** — matching the observed ~40 GB peak RSS almost exactly. The map
only ever needs ~2^(F+keep) entries plus transient subtree loads. Cap the
pre-allocation (or just use `DashMap::new()` and let it grow).

> **Implemented** — replaced by `store_prealloc_entries(complete_depth)`, which
> reserves `2^(depth+1)` (the number of interior nodes an open actually loads,
> at depths `0..=depth`) capped at `1 << 24` ≈ 4.6 GB. The cost per requested
> entry is 274 B, not the 140 B guessed above: `DashMap::with_capacity` treats
> its argument as a total and eagerly allocates every shard, hashbrown rounds
> each shard's bucket count up to the next power of two above `capacity * 8/7`
> — which for these power-of-two shard capacities always doubles — and a bucket
> costs `size_of::<(Prefix, Node)>()` (136 B) plus one control byte. So the true
> figure at `complete_depth = 23` was 2^27 × 274 B = **36.8 GB reserved at
> open**. *(Correction: the commit message said this "accounts for essentially all
> of the ~40 GB peak RSS". That overstated it. The reservation is real, but so is
> a second, independent cause found later — see 3.1.5 — where nothing ever evicts
> nodes at depths 24–25 and they accumulate towards 50.3M entries, ~18.4 GB of
> table. `bench_loop.log` shows RSS climbing with run length — ~6.5–7.6 GB at ten
> minutes, ~12 GB at an hour, 18.5 then 39.5–42.2 GB at two hours — which is
> growth, not a constant reservation. Both causes are real; neither alone explains
> the curve.)* The cap is chosen not to bite on a
> healthy database: `max_frontier_depth = 24` holds `complete_depth <= 23`, and
> depths `0..=23` contain `2^24 - 1` nodes. The new form also fixes a latent
> panic — `2_usize.pow(depth + 4)` overflows for a `complete_depth >= 60` read
> back from (untrusted) database metadata; `checked_shl` saturates to the cap
> instead. `test_open_sizes_store_from_frontier_depth` fails against the old
> expression (reserved capacity 458752 vs the 131072 bound at depth 14).
>
> *(Round 3 moved the bound twice. It is now expressed in **bytes** — a 4 GiB budget —
> rather than in entries, because adding the child hashes took
> `size_of::<(Prefix, Node)>()` from 136 to 200 bytes and would silently have turned the
> `1 << 24` entry cap from a ~4.6 GB reservation into a ~6.7 GB one. And the requested
> entry count halved, 2^(depth+1) → 2^depth, when the levels above the frontier left the
> map (§5.3). The 274 B-per-requested-entry figure above is likewise now 402 B, for the
> same layout reason.)*

**3.1.4 [P] `estimate_leaf_count()` (up to 100 RocksDB range scans + rebuild-free but
still seek-heavy) runs on *every* batch** inside `update_complete_interior_depth`
(`src/mpt/rocks_frontier/mod.rs:1366`), and twice more per open in the `from_storage`
log line. That's ~100 extra disk seeks per batch purely to decide whether the frontier
may advance — and it makes frontier advancement nondeterministic. Replace with an
exact leaf counter: increment on every genuinely-new leaf insert (the code already
distinguishes update vs split), persist it in the metadata batch. `len()` then also
becomes exact and free.

> **Partly implemented — the cheap subset, without the exact counter.** Advancing
> the frontier needs *both* a complete next level and the leaf-count gate, and
> neither test mutates anything, so the order was simply wrong: the expensive
> sampler ran first. `check_depth_complete` (an in-memory probe that bails at the
> first missing node, and immediately once `next_depth >= max_frontier_depth`)
> now runs first, so in steady state — where the frontier never advances — the
> sampling disappears from the batch path entirely. This is a reorder, not a
> policy change; `test_frontier_advance_respects_leaf_count_gate` pins that the
> depth reached is unchanged. Two related fixes landed alongside:
>
> - `is_empty()` was `len() == 0` over the sampled estimate, and at frontier
>   depth 0 that estimate is `approximate_entry_count()`, which counts interior
>   *and metadata* rows — so a genuinely empty database reported non-empty,
>   because opening it writes `ROOT_KEY`. It now tests for the root node, which
>   `prune_below_frontier` never evicts: exact, O(1), no I/O.
> - the startup log called the sampler **twice** and divided by `2 << depth`.
>   That is 2^(depth+1) where a complete level holds 2^depth, so the reported
>   leaves-per-frontier figure was half the truth. It now samples once, behind a
>   `log_enabled!` guard, through a named `leaves_per_frontier_node`.
>
> Three `.unwrap()`s on the sampler are gone with them; an I/O error in a
> heuristic should not abort a committed batch or an open. The exact counter
> remains future work.

**3.1.5 [P] Per-batch full-map scans.** `prune_below_frontier`
(`src/mpt/rocks_frontier/mod.rs:1442`) does `retain` over the entire store — tens of
millions of entries — after *every* batch, even though a batch of 10⁴ entries can
only have added ~10⁴·L below-frontier nodes. Track the prefixes materialized below
the frontier during the current batch (a per-batch `Vec<Prefix>`) and remove exactly
those; `release_subtree` (which walks whole subtrees recursively per boundary commit)
then becomes redundant and can go.

> **Implemented — but the fix as specified above is a pessimization.** Benchmarked
> against the real `dashmap` and the real `(Prefix, Node)` layout at the 16.7M
> entries the store holds when `bigdb` opens: the full-map `retain` costs 182 ms
> with nothing to remove and 209–231 ms after a batch, rising to 718–797 ms as the
> map grows. A per-batch `Vec<Prefix>` plus sequential removal — exactly what this
> finding proposed — measures **405–497 ms, roughly twice as slow as what it
> replaces**. What works is recording per *rayon worker* (+2.2–3.4 ms, no
> contention) and removing through `par_iter` (26–31 ms, since each removal is a
> random probe into a multi-GB table and is memory-latency bound). The sharding and
> the parallel removal are the change, not incidental detail.
>
> `release_subtree` was indeed redundant and is deleted — its threshold is the same
> expression as the retain's, and in the production configuration it is unreachable
> anyway, since all five call sites need `active_batch.is_none()`, which never holds
> at or below the frontier once level F is complete.
>
> Two things fell out along the way. The retain's `frontier + keep_below_frontier`
> could **overflow `u16`** on a corrupt `complete_interior_depth` — a panic in
> debug, and in release a wrap to a tiny depth that evicts the entire tree; it now
> saturates. And nothing evicts nodes at depths 24–25 at all, so they accumulate
> monotonically towards 50.3M entries (~18.4 GB of table), which is the second cause
> of the RSS curve discussed in 3.1.3.

**3.1.6 [P] Make `depth_to_write` actually reduce reads — or remove it.** Two options:

- *Use the persisted band*: when faulting in a subtree, first point-read the deepest
  persisted interior level (frontier + `depth_to_write` − 1) covering the target key,
  then range-scan only the leaves under the one child that's actually needed, taking
  sibling hashes from the stored interiors. Reads drop by 2^(depth_to_write−1) as the
  README promises.
- *Or set `depth_to_write = 1`* and stop paying the extra write bandwidth for data
  that is never read. Also note stale interiors from *former* frontier positions are
  never deleted (space bloat; `DeleteRange` on the old depth when the frontier
  advances would clean this up).

> **Discounted by measurement, at this scale.** The first option is the head of a family
> of read-reduction ideas, and §5.4 sweeps a direct proxy for it: `keep_below_frontier = 3`
> keeps more of the tree resident, cuts leaves read per insert from 22.8 to 20.3, and
> **loses 4.2 % throughput**. Fewer leaf reads did not convert, because at 3 GB against
> 62 GB of RAM the working set is resident and a "read" is a page-cache hit. The
> child-hash change (§5.2) has also already collected the cheap half of what band reads
> were for — an untouched sibling no longer costs a load at all — while the write side
> would cost roughly +0.9 interior puts per insert on a system whose write volume is the
> thing that stalls at production scale. This is a statement about the measuring
> instrument as much as about the idea: on a database larger than RAM it could easily go
> the other way, and until such a database exists this item cannot be ranked honestly.
> The second option (`depth_to_write = 1`) landed in round 2, where the knob turned out
> to be inert anyway (§2.2).

**3.1.7 [P] Prefetch subtree loads with parallel I/O.** Below the `count > 64` join
threshold the recursion is sequential, so most of the ~10⁴ per-batch range scans
issue serially — leaving NVMe queue depth at ~1. Since the batch is sorted, the set
of distinct frontier prefixes it touches is computable upfront in O(B): do a
`par_iter` prefetch of all needed subtrees before the recursion starts. On NVMe this
alone could be an order of magnitude on the read-bound phase; it also composes with
RocksDB `async_io`/`multi_get`-style readahead.

### 3.2 Rust-level (CPU) improvements

**3.2.1 Word-level prefix arithmetic** (`src/prefix/mod.rs`). The hot functions are
bit-at-a-time loops:
- `contains` — O(length) `get_bit` calls; used in the interior-node windowing on
  every visit. Rewrite as the masked-byte comparison `prefix_of` already uses
  (compare `length/8` full bytes with `memcmp`, then one masked byte) — or better,
  XOR + `leading_zeros` on `u64` words.
- `common_prefix` — bit loop up to 256 iterations; XOR the two hashes as 4×`u64`
  and count leading zeros: ~4 ops.
- `zero_bits_from` — loops over up to 256 bits doing per-bit RMW; zero whole bytes
  and mask one partial byte. Called on every `Prefix::parent`/`common_prefix`.

Combined these turn O(B·log²N)+O(256)/op into effectively O(B·log N) word ops.

> **Implemented** — `contains` and `common_prefix` both now go through one
> `common_leading_bits(a, b)` helper that XORs the two hashes as four big-endian
> `u64` words and takes `leading_zeros` of the first non-zero word: at most four
> iterations instead of up to 256. `zero_bits_from` clears one masked partial
> byte and `fill(0)`s the rest.
>
> One subtlety worth recording, because it is easy to get wrong: the helper
> returns `u16::MAX`, not 256, when all 256 bits agree. `Prefix::length` is a
> plain `u16` that nothing validates on decode, so lengths above 256 are
> representable, and the bit loop being replaced treated every position ≥ 256 as
> *equal* (because `HashExt::get_bit` returns `false` out of range). Returning
> 256 would have silently changed `contains` for those prefixes. `common_prefix`
> clamps the sentinel with `.min(min_length)`.
>
> The seven new tests are differential rather than fail-before: each carries the
> original bit-at-a-time implementation as a reference oracle and asserts the two
> agree, over randomised inputs and every boundary (lengths 0, 255, 256, 257,
> `u16::MAX`, and every word- and bit-in-word alignment). CI runs `cargo mutants`,
> and a new helper is exactly where weak tests show up, so this was checked
> directly: of the 24 mutants in the four changed functions, 23 are caught and 1
> is unviable (`Prefix` has no `Default`). None survive.
>
> **But it is not measurably faster, and the finding above oversold it.** An A/B
> of the in-memory backend (`bench -b memory -t 20`, which is CPU-bound and
> exercises these helpers with no RocksDB in the way) gives 628–630 K entries/s
> before and 622–627 K after — no difference outside noise. The loops were never
> running near their worst case: `contains` iterates `prefix.length` times and
> `common_prefix` stops at the first differing bit, and for random keys both are
> ~log₂N ≈ 20–30, not 256. Only `zero_bits_from` really did loop to 256 on every
> call, and it runs once per `common_prefix` — well under the cost of the SHA-256
> that dominates this path.
>
> Kept anyway, on the narrower justification that it is provably equivalent to
> what it replaces, removes a genuine O(256)-per-call worst case, and scales
> better with tree depth. It should not be cited as a throughput win.

**3.2.2 Remove release-mode assertions and linear windowing in
`batch_upsert_at_interior`** (`src/mpt/rocks_frontier/mod.rs:731-805`). The
`left_edge` scan is linear in the window and the four "windowing correctness"
`assert!`s re-scan the contained segment on every interior visit — O(B) work per node,
O(B·log N) per batch, each step calling bit-loop `contains`. Since entries are sorted,
the contained window is `[partition_point(< low_bound), partition_point(< high_bound))`
— two binary searches. Demote the asserts to `debug_assert!`.

> **Half implemented.** The four `assert!`s are now `debug_assert!`s. They walked
> the whole window calling `contains` again — roughly a third of the windowing
> cost — only to restate what the scan immediately above had just computed. CI's
> test job runs the debug profile, so they still execute on every push; only the
> release binary stops paying for them.
>
> **Still open: the binary-search windowing.** It is the larger and riskier half,
> because a wrong range under- or over-reads silently rather than failing loudly,
> and it wants a companion refactor — a `Prefix::key_range` shared with
> `RocksStorage::leaf_scan_bounds`, so the half-open bit-range is derived in one
> place rather than two. Land it with a differential test against the linear scan
> over randomised inputs. Note also that it is only a win *given* the word-level
> `contains` from 3.2.1: measured against the old bit-loop `contains`, the extra
> comparisons a binary search performs made it a pessimization.
>
> **Mostly moot as of §5.3, and demoted for it.** Above the frontier the traversal is
> `upsert_top` over the flat array, and it does **no windowing scan at all**: every key
> is contained by its own length-d prefix by construction, so divergence is impossible
> and the split is just the bit at *d*. Those were the levels where the windows were
> largest — the window at depth 1 is half the batch — so the binary search's prize is
> now confined to the band between the frontier and the eviction depth, where windows
> are already small. The risk is unchanged (a wrong range under- or over-reads silently
> rather than failing loudly), so the ratio has moved the wrong way. Kept on the list,
> ranked below where it was.

**3.2.3 Bring `batch_ops.rs` up to the slice-based design.** The in-memory/SQLite
recursion allocates two fresh `Vec`s per interior node and uses `Vec::remove(0)`
(O(len) memmove) in the leaf/empty paths. The rocks module already demonstrates the
slice/`partition_point` approach; unify them (they've already drifted — that's how
3.1.1's dedup semantics can differ from a future fix in one copy).

**3.2.4 Stop cloning `Node`s in the hot path.** Every visit clones a ~100 B node out
of the DashMap, and after each join the code does *two more* map lookups just to read
child hashes (`src/mpt/batch_ops.rs:175`, `rocks_frontier/mod.rs:917`). Have the
recursion return `(Prefix, Hash)` so parents never re-look-up children; store
`Arc<Node>` if cheap sharing is needed elsewhere.

> **Implemented for the rocks backend (§5.2), and it was worth far more than this
> finding claimed.** The recursion returns `(Prefix, Hash)` and `InteriorNode` now keeps
> the child hashes it already took in order to compute its own and then discarded, so
> the two post-join map probes and the `.expect()` after them are gone. The cost being
> removed was not "two DashMap probes and a 100 B clone": below the frontier the child
> being probed is *not resident*, so the probe range-scanned that child's entire leaf
> set off disk — 1.995 → 0.977 subtree loads and 24.0 → 19.0 leaves read per insert.
> Above the frontier the clones are gone too, for an unrelated reason: there are no
> nodes there to clone any more (§5.3). `batch_ops.rs` is untouched.

**3.2.5 Add a join threshold to `batch_ops.rs`.** `rayon::join` fires at every
interior node regardless of batch size (`src/mpt/batch_ops.rs:157`); the rocks
version's `count > 64` gate is the right idea — apply it here too.

**3.2.6 Hashing.** SHA-256 is ~40–50 % of CPU in the insert path (leaf + interior +
subtree rebuild hashing). Ensure `sha2`'s hardware path is active on the target
(SHA-NI; the `asm` feature or `sha2-asm` on non-x86). If the hash function is not
externally fixed, BLAKE3 is a 5–10× drop-in for this access pattern. Also note the
subtree *rebuild* recomputes every interior hash from leaves on load; with 3.1.6's
stored-interior variant most of that hashing disappears too.

> **Refuted by the round-4 profile — do not spend effort here.** The 40–50 % figure
> came from profiles that predate rounds 2–3, which removed most of what surrounded
> the hashing. On the current tree SHA-256 is **4.2 % of a worker's active time**
> (§6.1): the SHA-NI path is active (the hot frames are the `_mm_sha256*` intrinsic
> family), and a BLAKE3 swap would buy a few percent at the price of changing every
> hash in every database. The round-3 A/B of the word-level prefix rewrite already
> hinted at this — "hashing dominates this path" was true only of the in-memory
> backend's arithmetic, not of the rocks path's wall clock.

**3.2.7 Dead / leaking state.** `loaded_subtrees` is inserted into on every load and
**never read** (only `.len()` logged) — unbounded growth over a long run and shard
lock traffic; delete it or actually consult it to skip redundant empty-range scans.
`recover_full_tree_from_storage` collects the *entire* DB into a `Vec` before its
1M-entry chunking loop (`src/mpt/rocks_frontier/mod.rs:143`) — stream the iterator
instead, otherwise full recovery of a 124 GB DB OOMs the process it was meant to save.

> **Implemented (`loaded_subtrees` half)** — the field is deleted. The
> alternative, consulting it to skip repeat scans, is not merely unimplemented
> but unsound: nothing ever removes from the set, while `prune_below_frontier`
> and `release_subtree` evict the corresponding nodes, so a "already loaded"
> memo would make the next descent treat a populated subtree as empty and orphan
> its leaves. `get_leaf_value` also records the full 256-bit prefix of *absent*
> keys, so such a memo would make any key probed before it was written
> permanently invisible. Both hazards are now pinned by
> `test_reload_after_prune_preserves_earlier_leaves` and
> `test_absent_key_lookup_then_insert_is_visible`. `prefix_loads` is kept — it is
> genuinely read by the debug log.

> **Implemented (recovery half)** — `recover_full_tree_from_storage` now consumes
> `iter_nodes()` lazily. The borrow conflict that presumably forced the `collect`
> in the first place (the iterator borrows `self.storage` for the whole scan,
> while the per-chunk upsert wanted `&mut self`) is resolved by giving the worker
> a `&self` signature and threading the root through a local, writing it back
> once the iterator is dropped. The chunk size is now a parameter so tests can
> drive the boundary cases without a million-leaf fixture.
>
> A second problem showed up next to it: the buffer was
> `Vec::with_capacity(1024 * 1024)`, a flat 64 MiB request, and recovery is
> entered on *every* open whose frontier metadata is missing. Measured with a
> counting global allocator in `tests/full_recovery_memory.rs`, opening a
> database containing **one leaf** peak-allocated 67 MB before the change and
> under 8 MB after; the streaming test (60 leaves plus 200 000 interior nodes
> that recovery skips) went from 103 MB to under 8 MB.

### 3.3 RocksDB-level improvements

**3.3.1 The 1 GB row cache is doing nothing.** `set_row_cache`
(`src/mpt/storage/rocks.rs:129`) only serves point `Get`s, but every hot read in this
workload is an **iterator** (`get_leaf_nodes_by_prefix`, recovery scans) — iterators
bypass the row cache entirely. Replace it with a properly sized **block cache** via
`BlockBasedOptions::set_block_cache` (and consider `cache_index_and_filter_blocks`),
which serves both.

> **Implemented** — the 1 GiB LRU is now installed as a block cache via
> `BlockBasedOptions::set_block_cache`. The claim is not just argued from the
> RocksDB source but demonstrated: `iterator_reads_populate_the_block_cache`
> writes leaves, reopens the database, scans every node, and asserts the cache
> grew. Against the old row-cache configuration it fails with `usage 0 -> 0`,
> i.e. the gigabyte was never touched by any read this backend performs. Worth
> noting what the old setup actually got: with no block cache configured
> `BlockBasedTableFactory` silently installs its own 32 MiB default, so the
> scans were sharing 32 MiB while a gigabyte sat idle. `RocksStorage` also gains
> `block_cache_usage()` — the field has to be read from a non-test path anyway,
> since CI runs `clippy --all-targets -- -D warnings` and a non-underscore field
> read only under `cfg(test)` trips `dead_code`.

> **Sized in round 3, and 1 GiB stays.** A 4 GiB block cache measured **+2.4 % for
> +1.9 GB of peak RSS** on the reference database — the wrong trade while memory is a
> stated goal, and a weak claim anyway at 2.4 % against a 1–5 % noise floor (§5.7).

**3.3.2 Bloom filters + prefix extractor.** No filters are configured. Add:
- a block-based **bloom filter** (~10 bits/key) — makes point gets and the
  empty-subtree probes cheap;
- a **fixed-prefix extractor** over `2 + k` bytes (length prefix + first k hash
  bytes, k ≈ 3 covers frontier depth 24) with `memtable_prefix_bloom_size_ratio` and
  prefix-seek read options — turns each subtree range scan into a prefix seek that
  can skip non-overlapping files entirely.

> **Built in round 4, measured, rejected (§6.2).** A 4-byte fixed-prefix extractor
> (k = 2 — the extractor must be *coarser* than the shallowest frontier or a subtree
> scan spans prefixes and needs the total-order fallback), 10-bits/key full filters
> and a 0.02 memtable prefix bloom, with every iterator's read mode set explicitly
> because rocksdb 0.22 exposes no `auto_prefix_mode`. On a reference database
> rebuilt so the SSTs actually carry filters, bloom-on vs bloom-off on the *same*
> database measured **202,147 vs 208,026 entries/s (−2.8 %)** — the filter reads
> cost more than the file skips save at 63 range-partitioned SSTs with a resident
> working set. The k = 3 in the sketch above was also wrong on its own terms: at
> F = 19 a 24-bit prefix is *finer* than a frontier subtree's scan range, which
> breaks the one-prefix-per-scan property prefix mode needs. The idea's remaining
> case is the production L0 backlog (§3.3.5 measured 4.7 of 8 files touched per
> scan in L0), which a 3 GB database cannot exhibit; the patch is preserved in
> `bench_out/r4_p3_rejected.patch` should a larger-than-RAM database revive it.

**3.3.3 Bound the iterators.** `get_leaf_nodes_by_prefix` and
`get_nodes_by_prefix_length` iterate with no `ReadOptions` bounds and stop by
decoding keys in Rust. Set `iterate_lower_bound`/`iterate_upper_bound` — RocksDB can
then prune SST files and blocks before touching them and won't stall walking
tombstones past the range end.

> **Implemented, with two of my claims above corrected.** All three scans —
> including `iter_nodes`, which this finding missed and which is the
> full-database recovery scan — now pass an `iterate_upper_bound`:
> `get_nodes_by_prefix_length` bounds at `length + 1`, `get_leaf_nodes_by_prefix`
> at the successor prefix via the new `leaf_scan_bounds`, and `iter_nodes` at
> `NODE_KEY_RANGE_END` (`257u16`), which sorts above every node key and below
> the `_`-prefixed metadata keys. Because there is now always an end key, the
> in-loop `Option` check collapses to a plain comparison.
>
> Corrections: "won't stall walking tombstones" is **wrong** — this workload
> never deletes, so there are no tombstones. And "prune SST files and blocks
> before touching them" is overstated: the seek has already positioned the
> iterators, so what the bound actually saves is the tail step at the range end
> (one data block, occasionally one SST file, and readahead past the boundary),
> plus one round of the Rust-side iterator that currently boxes a key and value
> for the first out-of-range record only to discard it. `iterate_lower_bound` is
> deliberately not set: a forward iterator that already seeks to the start of
> its range gains nothing, and it would cost a second bound allocation on a path
> taken ~10⁴ times per batch.

**3.3.4 Drop `OptimisticTransactionDB`.** There is a single writer; transactions are
used only for root/metadata and the rare frontier persist, while the bulk data goes
through plain `WriteBatch`es anyway. With 3.1.2's single-atomic-batch design, a plain
`DB` + `WriteBatch` + one `flush_wal(true)` per batch gives strictly stronger
atomicity with less overhead (no commit-time validation, no snapshot tracking).

> **The premise this shares with §3.1.2 is refuted; the finding itself survives, alone.**
> "With 3.1.2's single-atomic-batch design" is doing the work in that sentence, and that
> design measures 5.7 % *slower*. Dropping `OptimisticTransactionDB` still stands on its
> own terms — there is one writer, and the transaction path carries commit-time
> validation this workload never needs — but the case for it is smaller than it looks:
> the census reports **0.000 transactions per insert** in steady state, because the only
> transactions left are the per-batch metadata commit and the frontier persist. It
> should be measured as its own change rather than bundled with, or justified by, the
> single-batch one.
>
> **Implemented in round 4, measured as its own change as asked: +1.7 %, inside the
> noise floor (§6.2).** Landed anyway, on what it deletes and what it enables: the
> metadata commit is a plain staged `WriteBatch` now, the wrapper's silent rewrite of
> `max_write_buffer_size_to_maintain` to 128 MB is gone (pinned by test), and a plain
> `DB` accepts `unordered_write` — which measured **+44.7 % against its own control
> arm** and is the direct confirmation that write-group ordering is what the round-4
> profile was looking at, but nets only +5.9 % because it must give up
> `pipelined_write` and the buffered WAL (their absence alone costs 26.8 %). It ships
> as a knob, default off; the per-worker batch aggregation (§6.2) takes more while
> keeping both.

**3.3.5 Column families.** Split `leaves` / `interiors` / `meta` into CFs:
per-CF tuning (big data blocks 16–64 KB and prefix bloom for the leaf CF's range
scans; small blocks + whole-key bloom for interiors/meta), independent compaction,
and cheap `DeleteRange` housekeeping for stale interior bands (§3.1.6).

> **The "big data blocks 16–64 KB" half is not settled by the numbers — needs
> benchmarking; `block_size` stays at 4096 for now.** Measured against the
> benchmark database's real LSM shape (levels and per-file key ranges from the
> MANIFEST, block boundaries from each SST's index block), one depth-23
> frontier-subtree leaf scan touches **8.04 SST files and 11.62 data blocks,
> 41.8 KB**:
>
> | level | files touched | blocks | bytes |
> |---|---|---|---|
> | L0 | 4.72 | 4.78 | 14 248 |
> | L3 | 0.52 | 0.54 | 2 180 |
> | L4 | 0.88 | 0.93 | 3 755 |
> | L5 | 0.92 | 1.32 | 5 297 |
> | L6 | 1.00 | 4.06 | 16 326 |
>
> The "~12 KB of leaves ⇒ 3 blocks ⇒ 1 block" argument is true of L6 in isolation
> and incomplete as a description of the scan. A `payload/block_size + 1 blocks per
> touched file` model reproduces the measured 11.62 with a payload of 14.7 KB (of
> which ~12.2 KB is in L6, matching ~112 leaves × 107 B), and projects:
>
> | block_size | blocks/scan | Δ blocks | bytes/scan |
> |---|---|---|---|
> | 4 096 | 11.62 | — | 41.8 KB |
> | 8 192 | 9.83 | −15 % | 70.7 KB |
> | 16 384 | 8.93 | −23 % | 128.6 KB |
> | 32 768 | 8.49 | −27 % | 244 KB |
>
> (Byte column scaled from the *measured* 3.6 KB average block actually read, not
> from a full 4 096; block counts are unaffected by that choice.) So 16 KiB buys
> exactly the L6 win the record size predicts — 4.06 blocks → 1, at the same ~16 KB
> — and pays for it seven times over on the other levels, each of which now reads a
> 16 KiB block to collect a handful of bytes (all of L0 holds 26 B in a depth-23
> range). RocksDB's iterator readahead (`initial_auto_readahead_size 8192`,
> `num_file_reads_for_auto_readahead 2`) already coalesces part of the L6 run, so
> the −23 % is an upper bound; bigger blocks also cut the block cache's usable
> entry count by 4× for the same gigabyte.
>
> **But the fixed per-file cost that dominates here is itself an artefact of the L0
> backlog.** 4.72 of the 8.04 files touched are L0, and the LOG for that snapshot is
> stalling on `estimated pending compaction bytes 69146684064`
> (`pending-compaction-bytes-delays: 7215`, `memtable-limit-stops: 98`, ~50–56 % of
> wall time). Drain L0 and the same model gives 6.90 blocks at 4 KiB versus 4.21 at
> 16 KiB — **−39 %**, not −23 %. So the honest reading is that bigger blocks get
> *more* attractive as §3.1.7 and the write-path work land, not less.
>
> Whether the trade wins still depends on the production NVMe's QD1
> latency-versus-size curve, which cannot be measured from here. What would settle
> it: an `fio` QD1 4/8/16 KiB random-read latency curve for the target device, then
> an A/B on a database *rebuilt* at the new size — block boundaries are baked into
> written SSTs, so an existing database keeps its 4 KiB blocks until it has been
> fully recompacted and the change cannot be A/B'd in place (unlike `compression`,
> §3.3.6). Two cheaper levers still dominate: fewer files touched (parallel subtree
> prefetch, §3.1.7, and the L0 backlog above) and fewer bytes per leaf (§3.4's
> redundant `merkle_hash`, ~30 %).

**3.3.6 Compression: turn it off (or LZ4-bottom-only).** Values are 64–100 bytes of
hashes — incompressible. Snappy costs CPU on every block read/write and buys ~nothing.
`compression = None` (or `Lz4` only at the bottommost level to compress key
redundancy) is the right default here; verify with `sst_dump`.

> **Refuted by measurement — leave `compression` alone.** *(Census taken before
> leaf records dropped their stored merkle hash — 65-byte values then, 33 now.
> The removed bytes were a SHA-256 output, i.e. the least compressible part of
> the record, so the ratio can only have moved further from the floor and the
> conclusion is unchanged.)** `sst_dump` is not
> installed, so this was settled two ways: by parsing the SSTs directly (footer →
> metaindex → properties → index, plus the compression-type byte in each block's
> 5-byte trailer, `table/block_based/block_based_table_builder.cc:1311-1341`), and
> independently by aggregating RocksDB's own `table_file_creation` properties out
> of `bigdb/LOG*`. Both agree. Sizes below are 10⁹ bytes; the LOG's own `Sum` row
> calls the same live set `2282/44 118.03 GB`, i.e. GiB.
>
> Two of the finding's premises are wrong.
>
> - **`Options.bottommost_compression: Disabled` does not mean L6 is
>   uncompressed.** `kDisableCompressionOption` is the default and means "no
>   bottommost override": `GetCompressionType`
>   (`db/compaction/compaction_picker.cc:87-91`) consults `bottommost_compression`
>   only when it is *not* that sentinel and otherwise falls through to
>   `compression`; `options/options.cc:177-181` prints the literal string
>   `Disabled` for it, and `options/cf_options.h:227` defaults `compression` to
>   Snappy. L6 is configured Snappy like every other level.
> - **The read path already decompresses nothing.** Keys are
>   `length_be_u16 ‖ hash`, so leaves (length 256) and interiors (lengths ~20–25)
>   occupy disjoint key ranges and land in disjoint SSTs: of the 2282 live files,
>   19 are interior-only, 3 straddle the boundary, and the other ~2260 hold only
>   leaves. A leaf record is a 42-byte internal key over a random hash plus a
>   65-byte value of (32-byte value, 32-byte SHA-256). Snappy reaches only
>   **0.968** on a real 4 KiB leaf block, missing `GoodCompressionRatio`'s
>   896-bytes-per-KiB floor (`block_based_table_builder.cc:108-113`,
>   `advanced_options.h:189`), so RocksDB discards the compressed copy. Sampling
>   20 data blocks in each live file: **45 189 of 45 193 leaf blocks are stored
>   `kNoCompression`**, while 367 of 367 interior blocks are Snappy at 0.428. The
>   per-file properties say the same thing without any custom tooling —
>   `data_size / (raw_key_size + raw_value_size)` is **0.9894** aggregated over
>   every leaf SST and **0.436** over every interior SST. Interiors compress
>   because `Prefix::zero_bits_from` zeroes every bit past `length`, leaving ~58
>   zero bytes in a 99-byte record. Bigger blocks do not rescue the leaves:
>   re-encoding real leaf entries into 8/16/32/64 KiB blocks gives
>   0.964/0.962/0.959/0.958, all still rejected.
>
> So the prize is neither 61 GB nor any read-path CPU. What is left is the futile
> compression *attempt*, paid on every compaction output byte: **1.03 ns/byte**,
> measured with the vendored snappy on the same Threadripper PRO 3975WX (970 MB/s).
> `Write(GB)` is GiB, so `1374.80 GiB × 1.03 ns/B ≈ 1520 core-seconds`, which
> against the same table (`CompMergeCPU(sec) 11413.74`, `Comp(sec) 29070.04`) is
> **13.3 % of compaction merge CPU and 5.2 % of compaction thread-time**. 5.2 % is
> the ceiling, not the expectation: those 29 070 compaction seconds are only 39 %
> CPU (the rest is I/O wait) over an uptime of 4 800 s, i.e. all six compaction
> slots busy the whole time, so merge CPU is not obviously on the critical path.
>
> It is not free either:
>
> - **+1.6 GB on disk (+1.2 %).** Data 126.40 → ~127.79 (an uncompressed data
>   section is ~0.989 × raw, which is exactly what the leaf files already measure —
>   *not* raw itself), index 0.391 → 0.570 stored. Note `index_size` in the
>   properties block is the **uncompressed** size
>   (`block_based_table_builder.cc:1655`, `index_builder.h:262-266`), which is why
>   `data_size + index_size` exceeds `file_size` in every event log line; the index
>   is stored at ~0.686 of it. That extra 1.2 % returns as extra compaction bytes,
>   cancelling roughly a quarter of the 5.2 %.
> - **2.3× the disk bytes for interior-band scans** (0.989 × 2.500 / 1.091), which
>   is the band `get_nodes_by_prefix_length` walks. The block cache is unaffected —
>   it caches uncompressed blocks either way.
> - **RocksDB's per-block adaptivity**, which is what keeps the current setting
>   correct if a caller ever stores low-entropy 32-byte values: the leaf blocks
>   would start compressing by themselves. `bench.rs:357-368` fills both key and
>   value from `fastrand`, i.e. the worst case for compression, and that is the
>   case measured here.
>
> Note also that `Cargo.toml` builds `rocksdb` with `default-features = false,
> features = ["snappy"]`, so the "LZ4 at the bottommost level" half of the finding
> is not even linked in.
>
> Verdict: **leave `compression` alone**, but it is the cheap one to revisit. The
> algorithm is recorded per block and `BlockFetcher` reads it from the trailer
> (`table/block_fetcher.cc:45-46`, `328-332`), never from the option, so unlike
> `block_size` (§3.3.5) this can be A/B'd in place on the existing database — flip
> it, and only newly written blocks change while old SSTs stay readable. The number
> to beat is an ingest A/B on the production device, not a CPU profile. The
> reasoning now lives in `RocksStorage::open`, and
> `node_records_have_the_sizes_the_compression_note_assumes` pins the 34/65/99 byte
> counts the whole argument rests on.
>
> The redundancy that *would* pay is in §3.4, not here: a leaf value stores a
> 32-byte `merkle_hash` recomputable from (key, value). Dropping it takes the leaf
> record from 107 raw bytes to 75 (105.9 → 73.9 as actually stored) — about 30 %
> off the whole database, off every subtree scan, and off all 2902 GB of compaction
> — at the price of one SHA-256 per leaf on read and a disk-format break.
>
> *(One input has since moved, in the direction that strengthens the verdict. The 0.436
> ratio for interior blocks was measured on 99-byte interior records, ~58 of whose bytes
> were the zeros `Prefix::zero_bits_from` leaves. An interior now carries its two child
> hashes as well — 163 bytes, and the added bytes are SHA-256 output, the least
> compressible content there is — and a frontier row is 65 bytes of tag plus two hashes
> (§5.2, §5.6). Both ratios can therefore only have moved towards 1. This is arithmetic
> about record contents, not a re-measurement; interiors remain ~1.5 % of records, so it
> does not move the aggregate.)*

**3.3.7 Write-path tuning for the scale observed** (124 GB, uniformly random leaf
keys ⇒ full-keyspace compaction pressure): larger `write_buffer_size` (256–512 MB),
`pipelined_write = true`, revisit `increase_parallelism(32)` vs
`max_background_jobs(8)` (the former overrides the latter's thread pools — pick one),
> **Implemented (the parallelism half; my description above was backwards)** —
> `increase_parallelism(32)` is removed. It is `set_max_background_jobs(8)` that
> wins the *option*, because it runs afterwards, and the benchmark database's
> LOG confirms `max_background_jobs: 8`. What the `32` actually left behind was
> 32 OS threads in the process-wide default Env's LOW pool, of which RocksDB
> will schedule at most 6 compactions plus 2 flushes. Opening the database sizes
> those pools by itself. `background_thread_pool_matches_max_background_jobs`
> counts `rocksdb:low` threads in `/proc/self/task`: 32 before, ≤ 8 after.
and consider **universal compaction** to trade space-amp for the ~2–3× lower
write-amp this insert-heavy workload wants. `ReadOptions::async_io` + readahead on
the subtree scans complements 3.1.7.

> **Implemented (`enable_pipelined_write`), plus two settings swept and left alone.**
> `set_enable_pipelined_write(true)` measures **118,065 → 124,329 entries/s (+5.3 %)**,
> and takes the run-to-run spread from 9.6 % to 2.7 % — the more interesting half.
> Separating the WAL append from the memtable insertion stops a writer that has finished
> its append from holding the next one behind its skiplist work, and this workload has
> up to 64 rayon workers issuing small batches concurrently; less queueing shows up as
> both a higher median and a much tighter distribution. `write_buffer_size = 256 MB`
> measures +1.7 % against a 5.2 % spread — not resolvable — and stays at 64 MB.
> Universal compaction was not tried.
>
> Also landed here, and operational rather than measurable: `set_max_open_files(4096)`
> in place of RocksDB's `-1`. It is the documented cause of 522 of the 575 runs that
> built `bigdb` dying on EMFILE (BENCHMARK-BASELINE.md §1.5), and a finite cap makes the
> process independent of whatever `ulimit -n` it inherits. It cannot bind on the
> reference database, which has 63 SSTs. An earlier sweep appeared to show `-1` beating
> 4096 by 5.6 %; that cannot be a real effect at 63 files, the run's spread was 6.0 %,
> and it was noise.
>
> Every RocksDB option is now a field of `RocksStorageConfig` and a `bench` flag, which
> is what made the sweep this finding asks for possible without a recompile.

**3.3.8 A more radical option: subtree pages + merge operator.** The frontier design
approximates "one I/O unit per frontier subtree". Making that literal — store all
leaves under a frontier node as **one value** keyed by the frontier prefix — turns
the per-insert range scan into a single point `Get`, and a RocksDB **merge operator**
("append these leaf upserts") eliminates the read-before-write entirely: inserts
become blind merges resolved at compaction/read time. Reads apply pending merge
operands. This removes the dominant seek cost at the price of page-sized values
(~8–16 KB at L≈100) and a custom merge/split policy when pages grow past a bound.
Worth a prototype next to the current design; it is the same trade the README's
"ZFS RAID-10-style layout" future work is reaching for, but expressible inside
RocksDB today.

### 3.4 Smaller notes

- `check_depth_complete` probes all 2^(F+1) prefixes when a level *is* complete and
  then `persist_interior_nodes_at_depth_in_tx` rewrites all of them in one
  transaction — a multi-second stall at F ≈ 20+ exactly once per advance. Amortized
  fine, but it's a latency cliff (p99/max batch latency); an incremental
  per-depth counter of interior nodes would make the check O(1) and the persist could
  stream.
  > **Worth more after §5.2, not less — promoted.** `check_depth_complete` decides the
  > frontier may advance by probing whether all 2^(F+1) nodes are *resident*, and the
  > sibling loads it was implicitly relying on used to materialise the untouched half of
  > every descent. Removing them means a depth-(F+1) node becomes resident only when a
  > batch actually descends into it, so the frontier needs roughly twice the inserts in
  > one process to advance. It still advances — both 40 M-leaf reference builds reached
  > the configured cap of 19 — but a read-path optimisation silently changed a
  > *persistence policy*, which is the clearest possible statement that conflating
  > structure with residency is load-bearing. An incremental per-depth interior count
  > fixes the latency cliff and removes the conflation in one change.
  > BENCHMARK-BASELINE.md §5's "B2" gate is the same phenomenon at production scale: it
  > is why `bigdb` sat at F = 21 for 26 hours and 274 runs.
  >
  > **Implemented in round 4 — the check half, not the conflation.** Both completeness
  > checks are one relaxed atomic load (per-depth counters over the store and over the
  > band slots), a probe-loop oracle pins the equivalence, and building 3 M entries from
  > empty measures **+12.2 %** (§6.2) — the probe cost was real and lived exactly where
  > this note said, in the growing tree's nearly-complete levels. The advance-time
  > persist spike and the structure/residency conflation are deliberately unchanged.
- `get_leaf_value` on a memory miss loads via the range-scan subtree loader; a full
  256-bit prefix makes it scan one key, but a direct point `Get` (+ bloom) is the
  honest implementation and avoids materializing the subtree bookkeeping.
  > **Implemented** — added `RocksStorage::get_node` (a point `get_pinned` over the
  > same key encoding) and `get_leaf_value` now uses it. Worse than "materializing
  > the subtree bookkeeping": because `load_subtree_from_storage` re-enters the
  > insert recursion with no active write batch, reading a single absent-from-memory
  > leaf *wrote it back to RocksDB* and left it in the in-memory store, violating the
  > invariant that no leaf sits above the frontier.
  > `test_get_leaf_value_does_not_populate_store` fails on that invariant before the
  > change.
- `MerklePatriciaTree::new()` for the rocks type silently creates a `TempDir` DB, and
  `default_config()` switches on `#[cfg(test)]` — surprising behavior differences
  between test and production builds of the same constructor.
- The `bincode`-encoded node values carry redundant bytes: a leaf value stores the
  merkle hash, which is recomputable from `(key, value)`; interior values store
  full 34-byte child prefixes where (given the parent's prefix and the child hashes'
  first divergent bit) shorter encodings are possible.
  > **Implemented, leaf half only.** Measured over the benchmark database's 2 315 live
  > SSTs (`table_file_creation` events): 1 218 591 870 live entries, 51.18 GB of raw
  > keys and 79.84 GB of raw values, splitting as 1 200 114 830 leaves at 65 B (98.5 %
  > of records, 97.7 % of value bytes) and 18 477 040 interiors at 99 B. Dropping the
  > leaf's merkle hash takes value bytes to 41.43 GB (−48.1 %) and key+value to
  > 92.62 GB (−29.3 %). The estimate in the first draft of this note (~30–40 % of
  > value size) was low for leaves and far too high for interiors.
  >
  > The interior half is **not** worth doing and is withdrawn. An interior's merkle
  > hash is not recomputable from its own record (it needs both children's hashes) and
  > it *is* read — `rebuild_tree_from_interior_nodes` walks it to the root. Only the
  > child prefixes compress, and Patricia compression makes their lengths arbitrary, so
  > the best case is ~37 B against 99 B: 1.15 GB out of 131 GB, **0.9 %**, bought with
  > unaligned bit-shifting on the one code path that must never misread the database.
  >
  > **Overturned for the frontier level specifically (§5.6).** The withdrawal argues
  > that Patricia compression makes the child prefixes' lengths arbitrary. That is true
  > *below* the frontier and false *at* it: above a complete level the tree is a perfect
  > binary tree, so a frontier node's children are the two positional length-(F+1)
  > prefixes and both are recoverable from the record's own key, with no bit-shifting
  > and no ambiguity. The node's own merkle hash is recoverable too, as
  > `InteriorNode::calculate_hash(prefix, left, right)`. What is left is the two child
  > hashes, which are irreducible — they are a function of data below the frontier, and
  > preimage resistance rules out recovering one from the parent's hash. `TAG_FRONTIER`
  > is therefore 65 bytes where the generic record is 163: bytes staged per insert
  > 115.8 → 91.5, database size −6.1 % at F = 16. The withdrawal stands for interiors
  > *below* the frontier, where the original argument holds unchanged.
- SQLite backend: the `idx_prefix` index duplicates the primary key (pure write
  overhead — drop it); per-node `INSERT` statements in `flush` could use a prepared
  multi-row form; `pre_advise`'s sibling-discovery loop is O(frontier × needed_keys)
  per round as its own comment admits. Given this backend exists as the
  write-amplification baseline, I'd leave it alone.
- `bench.rs` sorts each 10 K window before chunking into 1 K batches — this makes
  batches *range-clustered* rather than uniform, which flatters frontier-subtree
  sharing relative to a realistic arrival order. Worth benchmarking both orders.

---

## 4. Status

### End-to-end: the branch point against HEAD

Both built with the same toolchain and run for 120 s from an empty database on the same
machine (`bench -b rocks -t 120 -w 100000 -c 10000`).

**The branch point cannot run the benchmark at all** — `ef0e480` SIGSEGVs 3/3 within
30 s, because it predates the `-DROCKSDB_SCHED_GETCPU_PRESENT` fix in 3.1.0. To get a
throughput number at all, the "start" column below is `ef0e480` **plus that one fix** and
nothing else, so the comparison isolates everything after it.

| | start (+cpuid fix) | HEAD |
|---|---|---|
| entries in 120 s | 9.34 M | **21.3 M** |
| throughput | 77,728/s | **177,287/s** |
| batch p50 | 139.7 ms | **56.0 ms** |
| batch p99 | 247.7 ms | **133.0 ms** |
| batch max | 1.184 s | **838 ms** |

The averages *understate* it, because HEAD spends much of its run at tree sizes the start
state never reaches. Compared at matched tree size, from the insertion-rate time series:

| tree size | start | HEAD | speedup |
|---|---|---|---|
| 1 M | 188,187/s | 348,721/s | 1.85× |
| 2 M | 125,522/s | 254,083/s | 2.02× |
| 4 M | 73,601/s | 205,114/s | 2.79× |
| 6 M | 56,006/s | 177,934/s | 3.18× |
| 8 M | 55,074/s | 185,973/s | 3.38× |

The interesting quantity is not the ratio but its trend. Over 1 M → 8 M leaves the start
state decays 3.4× (188K → 55K) while HEAD decays 1.9× (349K → 186K). That is the
signature of the reload write-back: its cost scales with leaves-per-frontier-node, so
removing it flattens the curve rather than shifting it. Time to reach 9 M entries went
from 117.6 s to 41.8 s.

> **Re-run at the round-4 tip (`8db741b`), same methodology** — 120 s from empty, default
> configuration, three interleaved repetitions per side, medians; the start binary is the
> same `ef0e480` + cpuid fix, rebuilt today, and it reproduced its round-2 numbers to
> within 4 % (8.97 M entries at 74,733/s against 9.34 M at 77,728/s), which is the
> control that makes the rest comparable. Raw runs in
> `bench_out/hist_start_vs_tip.json`.
>
> | | start (+cpuid fix) | round-4 tip |
> |---|---:|---:|
> | entries in 120 s | 8.97 M | **44.6 M (4.97×)** |
> | throughput | 74,733/s | **371,746/s** |
> | batch p50 | 138.1 ms | **22.0 ms** |
> | batch p99 | 248.4 ms | **151.7 ms** |
>
> Compared at matched tree size, which is the honest read (the fixed-time averages
> understate the tip, since it spends most of the run at tree sizes the start never
> reaches):
>
> | tree size | start | tip | speedup | (round-2 HEAD was) |
> |---|---:|---:|---:|---:|
> | 1 M | 191,897/s | 665,431/s | 3.5× | 1.85× |
> | 2 M | 126,071/s | 864,521/s | 6.9× | 2.02× |
> | 4 M | 60,839/s | 746,868/s | 12.3× | 2.79× |
> | 6 M | 57,984/s | 757,485/s | 13.1× | 3.18× |
> | 8 M | 56,596/s | 645,335/s | **11.4×** | 3.38× |
>
> The trend reading sharpens further: over 1 M → 8 M the start decays 3.4× while the tip
> is essentially flat (665K → 645K). The tip keeps going where the start's run ends —
> 533K/s at 12 M, 360K at 16 M, 336K at 24 M, 140K at 32 M — the tail being the familiar
> L-growth once the frontier nears its default cap, which no round has claimed to change.
> Three caveats: no root-hash gate is possible (the start binary reseeds per window and
> has no `--seed`); peak RSS is not comparable across the columns (3.23 vs 4.99 GB, but
> the tip's process is holding a tree five times larger by the end of the window); and
> the start's spread is wide (one of three runs at 49.8K/s with a 559 ms p99 — the
> pre-`pipelined_write` variance round 3 measured), which the interleaving and medians
> absorb.

### Landed on `perf/rocksdb-improvements`

Each of these is one commit, with tests and a justification in the commit message.
Where a fix could be demonstrated rather than argued, the commit records the
before/after measurement.

| # | Change | Effect |
|---|---|---|
| 3.1.0 | `-DROCKSDB_SCHED_GETCPU_PRESENT` | fixes a 100 %-reproducible SIGSEGV under concurrent inserts |
| 3.1.1 | last-wins `sorted_unique_entries` | backends no longer disagree on the root hash for repeated keys |
| 3.1.3 | capped store pre-allocation | ~36.8 GB → ~4.6 GB reserved at open; also fixes an overflow panic |
| 3.2.7 | streaming recovery | one-leaf open: 67 MB → <8 MB peak; recovery no longer needs the DB in RAM |
| 3.2.7 | delete `loaded_subtrees` | removes unbounded growth on the subtree-load path |
| 3.3.1 | block cache instead of row cache | the 1 GiB cache is now actually consulted by the scans |
| 3.3.3 | `iterate_upper_bound` on all three scans | RocksDB stops at the range end instead of overrunning it |
| 3.3.7 | drop `increase_parallelism(32)` | 32 background threads → 6 |
| 3.1.4 | frontier gate reordered | ~100 range scans per batch removed in steady state |
| 3.4 | point-read `get_leaf_value` | a read no longer writes back to RocksDB |
| 3.4 | exact `is_empty` | an empty database no longer reports non-empty |
| 3.4 | leaves-per-frontier log arithmetic | halves the sampling at open; the ratio was 2× off |
| 3.2.1 | word-level `Prefix` helpers | equivalent and bounds the worst case; **no measured speedup** |
| 3.2.2 | `debug_assert!` windowing checks | release builds stop re-validating the window |
| 3.4 | compact leaf record (tag 2) | 65 B → 33 B per leaf; −29.3 % of key+value bytes |
| 3.1.5 | delete `release_subtree` | dead in production; same threshold as the end-of-batch prune |
| 3.1.5 | one saturating `eviction_depth` | fixes a `u16` overflow that would evict the whole tree |
| 3.1.5 | targeted eviction | 209–231 ms (→780 ms as the map grows) → ~30 ms per batch |
| 3.1.6 | `depth_to_write = 1` | no behaviour change: the knob was **inert**; now documented |
| 3.1.4 | exact leaf counter | removes the sampler from the frontier-advance path |
| 3.3.6 | compression / `block_size` | **refuted** — both left alone, with the measurement recorded |
| 3.1.7 | populate-only subtree rebuild | **11.301 → 2.556 writes/insert; 64.9K → 116.8K entries/s** |
| 3.1.7 | `JOIN_THRESHOLD` 64 → 8 | +9.5%, chosen from a 6-point measured sweep |
| 3.1.7 | parallel prefetch | **rejected** — built, measured 9.3% slower, not landed |
| 3.1.0b | persist a new leaf at creation | **fixes data loss**: a 1-entry first batch lost its leaf |
| 3.1.0b | re-stage frontier interiors | **fixes a silent wrong root hash**: 80/674 shapes |
| 3.1.5 | drop the split-partner rewrite | 2.556 → 1.910 writes/insert; 138.1K → 149.9K entries/s |
| T1.1 | populate-only subtree loads | a read path stops writing; ~65 % of all `put_node` calls removed |

With one exception, nothing in this list changes the on-disk format or the frontier
algorithm itself, so any of it can be reverted independently. The exception is the
compact leaf record: it is backward compatible (tags 0 and 1 decode through the
unchanged `Node::deserialize`, so an existing database is read correctly and needs no
migration) but **not forward compatible** — once a build carrying it has written to a
database, an older build fails on that database with `Unknown node tag 2`. That
failure is loud, never a wrong hash, but it is one-way.

*This table covers rounds 1 and 2. Round 3's is §5.1, and it is not repeated here.*

> **Corrected — "the throughput ceiling in §2.2 is untouched" was true when written and
> is false now.** Round 3 moved end-to-end throughput on the 40 M-leaf reference
> database from 108,479 to **257,951 entries/s (+137.8 %)** — four interleaved runs,
> spreads 1.6 % and 1.8 %, identical root hash — with no redesign: an untouched sibling
> that is no longer faulted in, a top of the tree that is an array rather than a hash
> map, and a subtree fault-in that is computed rather than materialised (§5). Two of
> those move the constant in front of §2.2's read term, which is what "ceiling" was
> reaching for.
>
> What is genuinely untouched is the **asymptotics**: L still grows linearly with N once
> the frontier caps, so the decay measured in BENCHMARK-BASELINE.md §6.3 is unaddressed
> by everything on this branch.
>
> The "one exception" sentence above also now has three. Round 3 added the interior
> record carrying its child hashes (**breaking**: an old database fails loudly at open
> with `Failed to decode interior: UnexpectedEnd`, never with a wrong hash) and
> `TAG_FRONTIER` (**additive**, exactly like the compact leaf record: old rows keep
> decoding, and a build without the tag rejects a new row rather than misreading it —
> so it is backward compatible and one-way forward, same as tag 2).

### Remaining, in priority order

Re-ranked after round 3, which changed four of the inputs this list was ordered on.

- **The read-reduction premise is measured down — at this scale.** Keeping more of the
  tree resident cuts leaves read per insert from 22.8 to 20.3 and *loses* 4.2 %
  throughput (§5.4), because a 3 GB database against 62 GB of RAM is entirely resident
  and a "read" is a page-cache hit. Nothing read-side can be ranked honestly until that
  is no longer true.
- **The single atomic `WriteBatch` is a 5.7 % regression**, not the throughput win the
  previous ranking had it as (§3.1.2). It moves out of the performance list entirely.
- **The in-memory churn that dominated CPU is gone** — ~121 shard-locked map operations
  per inserted entry, down to ~2.6 (§5.5). Every profile this document cites predates
  that, so the evidence the old ranking rested on no longer describes the binary.
- **Write volume per insert has not moved.** The census reads 1.911 record puts per
  insert (1.000 leaf + 0.911 interior) at both ends of the session; only the bytes
  changed, 188.1 → 157.1 staged per insert. On a system that spends its time in
  compaction stalls at production scale, that is the term nothing on this branch has
  attacked.

**Tier 0 — the prerequisite for ranking anything read-side**

1. **Build a reference database larger than RAM.** This is not instrumentation, it is
   the precondition for the central claim in §2.2 being testable at all. The 40 M-leaf
   database costs 8 minutes and 3 GB and is the right instrument for write volume,
   memory and CPU; it cannot answer a read question, and §5.4's sweep is the proof.
   BENCHMARK-BASELINE.md §5.1's milestone table puts 300 M leaves at 36.70 GiB and
   600 M at 75.18 GiB, so the crossing point for this machine's 62 GiB is somewhere near
   500 M leaves. Note that lowering `max_frontier_depth` does **not** help here: it makes
   the *regime* cheap to reach, which is what §5's database exploits, but the working set
   is set by bytes on disk against bytes of RAM, and scaling the frontier down does not
   change either. The two levers are more leaves, or less RAM — a cgroup memory limit
   would cap the page cache as well as the heap, and would be far cheaper than 500 M
   leaves, but it has not been tried and its equivalence to a genuinely larger database
   is an assumption. **Unmeasured either way**: no figure here is a measurement of what
   this would cost on the current code, beyond the build phase being faster than the one
   that produced that table (82,027 → 97,972 entries/s from the child-hash change alone).

**Tier 1 — bounded, and attacks something measured**

2. ~~**Incremental per-depth interior counts** (§3.4).~~ **Done in round 4: +12.2 %
   building from empty, neutral at the cap (§6.2).** The check is O(1); the persist
   spike and the residency conflation remain, as §3.4's note records.
3. ~~**Re-profile before choosing between anything below.**~~ **Done — §6.1 is the
   profile this item asked for, and it re-ranked everything:** SHA-256 turned out to be
   4.2 % of a worker's active time, not 40–50 %, and the top of the profile was ~35 %
   RocksDB write-group formation around the ~9,100 per-subtree commits — which items
   5–9 never mentioned, because the old profile could not see it.
4. ~~**Bound the retained band at the production frontier** (§3.1.5).~~ **Done in round
   4 (§6.2): the band is positional slots now** — ~150 B a resident entry against ~402,
   peak RSS 6.81 → 3.79 GB and batch p99 −49 % on the insert workload at F = 19. The
   F = 23 figure (~7.5 GB of slots against ~18.4 GB of table) is still arithmetic, not
   a measurement, and the growth phase pays −8.7 % and +0.8 GB for it at the default
   configuration (§6.2) — slot-per-node band compression is the remaining piece.

**Tier 2 — smaller, or newly demoted**

5. **Binary-search windowing** (§3.2.2), with the shared
   `Prefix::key_range`/`leaf_scan_bounds` refactor. Demoted: the levels where the windows
   were largest no longer do a windowing scan at all (§5.3).
6. ~~**Bloom filters and prefix seek** (§3.3.2), minding the `total_order_seek` hazard.~~
   **Built, measured, rejected in round 4: −2.8 % on-off on the same rebuilt database
   (§6.2).** The demotion reason above was right — file selection is already a binary
   search over range-partitioned SSTs here — and the surviving case (skipping L0 files
   under a production backlog) waits on Tier 0 like every other read-side idea.
7. **Column families** (§3.3.5) and `DeleteRange` for stale interior bands. The bands are
   real — interiors written at former frontier positions are never deleted (§3.1.6) — and
   have never been costed.
8. **CPU on the in-memory path**: slice-based `batch_ops.rs` (§3.2.3) and its join
   threshold (§3.2.5). §3.2.4 landed for the rocks backend only. Worth doing only if
   `BatchMPT`/`DurableBatchMPT` matter for their own sake; measure first.
9. **Make `depth_to_write` real** (§3.1.6) — still needs both sides built, and its read
   side is exactly the family §5.4 discounts at this scale. Below Tier 0 by construction.

**Exploratory**

10. **Subtree pages with a merge operator** (§3.3.8). Still the only idea with
    order-of-magnitude potential, and still the largest commitment: it deletes the
    read-before-write, and it trades away fast crash resumption, which is a requirement.
    It needs a resumption story before a prototype is worth the time.
11. **Universal compaction and the rest of §3.3.7.** *Less* attractive than when filed,
    not more: the 49.6 % stall that motivated it is the worst of ten consecutive runs
    and the steady state at F = 23 starts at 8.8 % (BENCHMARK-BASELINE.md §6.3).

**Correctness debt, now priced**

12. **Single atomic `WriteBatch` per batch** (§3.1.2, enabling §3.3.4). The one known
    correctness gap: a crash mid-batch leaves a state the caller never saw as committed,
    and nothing detects it. Built and measured at **−5.7 %**. It is not a performance
    item and should not be sold as one; take it when atomicity is required, knowing the
    price.

**Closed since the previous ranking.** Kept here, rather than deleted, because the
reasoning is the record.

1. ~~**Stop `load_subtree_from_storage` writing the leaves it just read.**~~
   **Done — the largest single win of round 2.**

   > Measured before: 61.8% (build) and 64.8% (steady state) of *every* RocksDB write
   > the tree made was a leaf being rewritten byte-identically, at 11.3 writes per
   > insert. At production scale it was worse — `bigdb` consumed 83,142,407,310 sequence
   > numbers for 978,620,000 inserts, ~85 writes per insert at 59 leaves per frontier
   > node.
   >
   > Replaced with `build_subtree_memory_only`, which builds the subtree directly from
   > the sorted leaf slice and never touches storage. Structural equivalence to the
   > recursion was checked by differential simulation over 654 leaf sets, not argued.
   >
   > | | before | after |
   > |---|---|---|
   > | steady-state writes/insert | 11.301 | **2.556** |
   > | steady-state throughput | 64,904/s | **116,810/s** |
   > | build-phase writes/insert | 2.836 | 2.365 |
   >
   > Conservative: the "after" database had grown to 11.66M leaves (44 per frontier
   > node) from 9.83M (37), so it is the harder case.
   >
   > Round 3 then removed what this left behind: `build_subtree_memory_only` still built
   > the subtree *into the store*, where the prune removed it again (§5.5).

2. ~~**The split partner is still rewritten unchanged.**~~ **Done.** In the split arm of
   `batch_upsert_at_leaf` the *pre-existing* leaf was written back alongside the new one.
   A leaf record is `tag ‖ value` keyed by `length ‖ hash`; splitting changes neither the
   key nor the value, only the leaf's position in the tree, which the record does not
   encode — so the put was redundant, the leaf having been persisted when it was created
   or already staged in this very batch. It was the same defect as item 1 in a different
   place: 2.556 → 1.910 writes per insert, 138.1K → 149.9K entries/s. Chasing it also
   turned up the two reopen bugs in §3.1.0b, one of which it would have converted from a
   narrow edge case into general data loss.

   > Round 12 named the split arms as methods over a `Staging` enum
   > (`store_regime.rs`) and parked the full measurement record here. In a
   > byte-faithful simulation of the recursion, 24,374 arrivals at the staged split
   > arm found 13,405 partners already durable and 10,969 already staged in the same
   > batch — none memory-only, none staged in another live batch, none differing in a
   > byte; suppressing the put left the 675-scenario differential and 1,500 seeds of
   > `test_rocks_impl_randomized_reopen_batches` bit-for-bit identical to the
   > unmodified code, down to the exact set of leaf records on disk. The two
   > *batchless* split arms are reachable only before a complete level exists at F —
   > 517 of the 24,891 splits in the 675-scenario simulation, not one at F ≥ 1. Their
   > partner puts are redundant too (suppressing all three was bit-for-bit identical),
   > but they are kept: they cost 0 writes per insert in production, and before
   > `batch_insert_into_empty` wrote at creation, suppressing them cost 318 of 675
   > scenarios their on-disk state and panicked 24 with "Left child missing after
   > batch upsert" — that path has already lost data once.

3. ~~**Parallel subtree prefetch** (§3.1.7).~~ **Rejected — measured 9.3 % slower.**
4. ~~**Bound the depth-24/25 accumulation.**~~ **Partly done, and re-opened as Tier 1
   item 4 above:** the eviction-depth cap fixes only scaled-down configurations, and the
   churn removal does not touch the retained band itself.
5. ~~**An exact leaf counter.**~~ **Done** — `len()` is now exact and O(1), and the CSV
   carries it per batch.
6. ~~**`block_size` and `compression`.**~~ **Refuted, both left alone**, with the
   measurement recorded in §3.3.6.
7. ~~**A 4 GiB block cache; `write_buffer_size = 256 MB`.**~~ **Swept and rejected**
   (§5.7). ~~**`enable_pipelined_write`.**~~ **Landed, +5.3 %** (§3.3.7).

---

## 5. Round 3 — measured on a rebuildable reference database

*Everything in this section was measured on a **40 M-leaf reference database**, built from

> **All steady-state A/B numbers in this section are an *update* workload.** `tools/ab.py`
> defaulted its seed to `bench`'s own, and the reference database is built with that default, so
> every run replayed keys the database already held -- leaf count came out at exactly 40.0M in
> every run. Splits, new-leaf creation and frontier advance were not exercised. The default was
> changed afterwards and the cumulative comparison re-run as a genuine insert workload
> (40.0M -> 42.0M leaves): **106,335 -> 249,218 entries/s, +134.4%**, against the +137.8%
> recorded below. The conclusion is robust to the flaw -- the transient-subtree path does the
> same work either way, an insert adding a key to the merged set where an update replaces one --
> but the individual figures should be read with it in mind.
empty in **8 min 8 s** (487.6 s at 82,027 entries/s) with `--max-frontier-depth 20`, which
puts the frontier at its cap of 19 and gives **L = 76 leaves per frontier node** — the same
regime as the deleted 124 GB database (L = 75–113 in BENCHMARK-BASELINE.md §6.3's table) at
3 GB. `tools/ab.py` is the protocol: fixed work (`--max-entries`, 2 M entries), a fresh copy
of the reference database per run, the page cache dropped with `posix_fadvise` between runs,
interleaved repetitions, and the root hash compared across every variant as a correctness
gate. The noise floor on a quiet machine is 1–5 %; anything smaller is not claimed. The full
recipe, and what the protocol does and does not put under measurement, is
BENCHMARK-BASELINE.md §5.2.*

### 5.1 What landed, in order

| # | Change | Effect |
|---|---|---|
| — | measurement harness: deterministic keys, the write census, `tools/ab.py` | makes everything below falsifiable |
| §5.2 | child hashes in `InteriorNode`; the recursion returns `(Prefix, Hash)` | subtree loads/insert 1.995 → 0.977, leaves read/insert 24.0 → 19.0, build-phase throughput 82,027 → 97,972/s; costs 64 B an interior record |
| §3.4 | leaf-value fast path on the load path (no double SHA-256) | +2.3 % |
| §3.3.7 | `enable_pipelined_write` | 118,065 → 124,329/s (+5.3 %), and spread 9.6 % → 2.7 % |
| §3.3.7 | `set_max_open_files(4096)` in place of `-1` | operational, not measurable here: 63 SSTs, so 4096 cannot bind |
| §5.3 | `TopLevels` — the levels above the frontier in a flat array, 32 B a node | +4.5 % and +7.8 % in two sessions; reopen 3.075 → 2.887 s |
| §5.4 | allocator tuning, and `depth_always_keep` capped at `max_frontier_depth − 1`, both on by default | peak RSS 4.15 → 3.45 GB (−17 %); at F = 19, RSS 6.58 → 4.20 GB and p99 305.7 → 184.0 ms |
| §5.3 | `FrontierLevel` — the frontier level flat too, 64 B a node | peak RSS at open 1.20 → 1.02 GB; reopen 2.98 → 2.87 s (parity at F = 19) |
| §5.5 | transient subtrees computed, not built into the store | 128,913 → 230,406/s (+78.7 %); store inserts/insert 62.3 → 2.6, removals 58.5 → 0.0 |
| §5.6 | on-disk frontier record 163 B → 65 B | bytes staged/insert 115.8 → 91.5; database size −6.1 % at F = 16 |

One of these **breaks the on-disk format**: the child-hash change takes an interior record
from three fields to five, and an old database fails loudly at open with `Failed to decode
interior: UnexpectedEnd`, never with a wrong hash. Nothing else here does. The flat arrays
are in-memory only — neither commit touches storage code — and `TAG_FRONTIER` is additive in
exactly the way the compact leaf record was: old rows keep decoding, and a build without the
tag rejects a new row rather than misreading it. *(An earlier draft of this section said the
flat array broke the format as well. It does not.)*

### 5.2 What the census found that the review had not

The first thing the write census showed was that **half of every subtree load existed only
to read a 32-byte hash**. `InteriorNode` stored its children's *prefixes* but not their
hashes, so rehashing a parent required both children in memory — and below the frontier a
child is not in memory. `ensure_node_loaded` range-scanned an entire leaf set off disk to
recover one hash, for a subtree no batch entry was modifying:

| | before | after |
|---|---|---|
| subtree loads / insert | 1.944 (0.969 of them sibling loads) | 0.977 |
| leaves read / insert | 37.4 (17.5 of them by sibling loads) | 19.0 |

`InteriorNode::new` already took both child hashes — it needed them to compute its own — and
discarded them after hashing; it now keeps them. That also closes §3.2.4, whose two post-join
map probes turn out to be the same defect seen from the other side.

This was not in §3 at all. It was found by instrumenting, not by reading — which is the
argument for the census outliving the round that produced it.

**What it costs.** An interior record goes from 99 to 163 bytes, so bytes staged per insert
rose 31 % (188.1 → 246.4). Interiors are ~1.5 % of records, so the database is the same size
to two significant figures, but on a system that spends half its wall clock in compaction
stalls at production scale this is not free — and it is what §5.6 goes on to recover. Peak
RSS at the default `depth_always_keep` went the *other* way, 7.98 → 6.45 GB, because not
materialising the untouched half of every descent outweighs the 64 extra bytes.

**And one interaction, recorded because it is a policy change nobody asked for.**
`check_depth_complete` decides the frontier may advance by probing whether all 2^(F+1) nodes
are *resident*, and the sibling loads used to materialise the untouched half of every
descent. Without them a depth-(F+1) node becomes resident only when a batch actually descends
into it, so the frontier needs roughly twice the inserts in one process to advance. It still
advances — both 40 M-leaf reference builds reached the cap of 19 — but §3.4's incremental
per-depth count is worth more than it was.

### 5.3 Flat arrays: above the frontier, and then the frontier level

Above a complete level the trie is a perfect binary tree, notwithstanding Patricia
compression: `check_depth_complete` only advances the frontier to F once a node exists at all
2^F prefixes of length F, so every length-(F−1) prefix has both children at exactly depth F,
and by induction every node above the frontier does. A node's position is then a function of
`(depth, index)`, its prefix follows from the index, and its children are at `(d+1, 2i)` and
`(d+1, 2i+1)` — so nothing needs storing but the hash.

`TopLevels` holds those depths in implicit-heap order as `AtomicU64` words (the recursion
writes through `&self` from many rayon workers; sibling subtrees own disjoint index ranges,
so no two workers touch a slot). The traversal above the frontier, `upsert_top`, needs no node
fetch, no clone, and **no windowing scan at all** — every key is contained by its own length-d
prefix by construction, so divergence is impossible and the split is just the bit at *d*. That
subsumes §3.2.2's binary-search windowing for the levels where the windows were largest.

    entries/s   119,287 → 124,620   (+4.5 %)
                117,473 → 126,659   (+7.8 %)   two independent sessions, four runs each

and 342,690 → 394,051 (+15 %) building 2 M entries from empty, where the frontier is
shallower and a larger share of the work is the top of the tree. Reopen, which is what crash
resumption costs, 3.075 → 2.887 s over three interleaved runs.

**The frontier level itself came next, and needed one more argument.** Its children are below
the frontier and Patricia-compressed, so their prefixes are not derivable from position — but
they do not need to be. A length-(F+1) positional prefix and the compressed prefix beneath it
cover the same leaf key range (compression extends a prefix, it never shortens one), so a scan
at the position returns the same leaves and the compressed root is recovered from them as
`common_prefix(first, last)`. What *is* irreducible is the two child hashes: rehashing a parent
whose left child a batch just changed needs the right child's hash, and preimage resistance
rules out recovering it from the parent's old hash. So `FrontierLevel` stores exactly those,
64 bytes a node, against ~201 bytes of hash-table bucket plus a `TopLevels` slot before.

    peak RSS at open   1.20 GB → 1.02 GB   (−15 %)
    reopen             2.98 s  → 2.87 s    (parity)

Reopen is only at parity because 524 K map inserts are not what dominates 2.9 s — the scan of
the frontier level is, which is what §5.6 then attacks. At F = 23 it would be 8.4 M inserts
against the same scan; this database cannot show that.

**On memory, the honest result at this scale is: no measurable saving from the flattening
itself.** The array replaces 2^F − 1 map entries costing ~402 B each with 32 B each, which at
F = 19 is ~190 MB against a 4.2 GB footprint — and hashbrown's bucket count is a power of two,
so dropping 524 K of 4.2 M entries does not cross a boundary. Peak RSS measured 4.18 → 4.33
GB, slightly *worse*. At F = 23 the same arithmetic is ~3.1 GB against ~27 GB and would cross
a boundary, but that is unverified and this database cannot verify it.

### 5.4 Where the memory actually is

Three things were measured, and none of them is the flattening:

1. **`MALLOC_TRIM_THRESHOLD_`.** Setting it (with `MALLOC_TOP_PAD_` and
   `MALLOC_MMAP_THRESHOLD_`) takes peak RSS from **4.15 GB to 3.45 GB, −17 %**, with
   throughput inside noise, over three interleaved repetitions. No code change; glibc is
   returning and re-faulting arena pages, and a profile put 24.5 % of one worker's self time
   in `mprotect` under `malloc`, which is the same phenomenon. It is now on by default, as
   `jellyfish_rs::allocator::tune_for_large_heaps()` — a function the binary calls rather than
   something the library does on load, because `mallopt` is process-global and a library that
   calls it behind an embedder's back has changed every allocation in their program.
2. **`depth_always_keep` must not exceed the frontier.** At F = 19 the default 23 retains four
   levels below the frontier for nothing: **RSS 6.58 → 4.20 GB (−36 %) and p99 305.7 → 184.0
   ms (−40 %)** with throughput slightly *up*. `eviction_depth` now caps it at
   `max_frontier_depth − 1`, so it can only bind while the tree is still growing, which is the
   whole of what it is for. It does earn its keep there: building from empty, dak = 23 gives
   319,973/s against 288,868/s at dak = 8 and 284,732/s at dak = 0. This changes nothing at
   the production default, where 24 and 23 are already consistent — which is also why it does
   **not** address the depth-24/25 accumulation §3.1.5 found.
3. **`keep_below_frontier` is the real lever, and it is not free.** Swept at F = 19:

   | | entries/s | leaves read/insert | RSS |
   |---|---|---|---|
   | 1 | 106,518 (−11.7 %) | 27.7 | 4.26 GB |
   | **2 (default)** | **120,567** | 22.8 | 4.19 GB |
   | 3 | 115,484 (−4.2 %) | 20.3 | 6.59 GB |

   The default is already the optimum on both axes. Note the shape: keeping *more* below the
   frontier reduces leaves read and still loses throughput. **That is the measured argument
   against the whole read-reduction family** — at this scale, cutting leaf reads does not
   convert into throughput, because the working set is resident. It is equally an argument
   about the instrument: it would convert on a database larger than RAM, which is exactly what
   this one is not, so no read-side conclusion measured here should be trusted in either
   direction. Both halves are load-bearing, and they are why §4's Tier 0 is a database and not
   a code change.

### 5.5 Transient subtrees computed, not built — the largest win of the round

The shared in-memory store was doing about sixty times more work per inserted entry than
RocksDB was, and ninety-five per cent of it was undone before the batch ended. Faulting a
subtree in ran the ordinary insert recursion over it, so every node went into the DashMap and
`prune_below_frontier` then removed almost all of them again — anything deeper than the
eviction depth is dropped by definition. Counters on both sides put it at **62.3 store inserts
per inserted entry, 58.5 of them removed again**: roughly 121 shard-locked map operations per
entry against 1.9 RocksDB puts.

None of it was needed. A subtree after a batch is exactly the leaves already on disk merged
with the batch's entries for it, the batch winning on equal keys, so its root prefix and hash
follow from that merge directly. The merge runs in one pass over two sorted runs — which also
yields the new-leaf count for free, since a batch key absent from the loaded set is a leaf
that did not exist — and only the levels a prune keeps are materialised. Leaves live at depth
256 and so are never retained, which is half the nodes gone on its own.

**Where the merge is taken matters, and the first attempt got it wrong.** Merging at the
frontier child re-reads that child's whole leaf set on every visit, where the old descent used
the retained band when a previous batch had left it resident and loaded a smaller sub-subtree.
Descending the band first and merging at the first level the prune does not keep recovers
that:

| | entries/s | leaves read/insert |
|---|---:|---:|
| frontier (previous commit) | 128,913 | 22.8 |
| merge at the child | 212,272 (+64.7 %) | 37.3 |
| descend, then merge | **230,406 (+78.7 %)** | 22.8 |

Four interleaved runs each, spreads 4.1 % / 2.6 % / 1.7 %. Against the previous commit the
final state is throughput +78.7 %, batch p99 157.4 → 75.8 ms (−52 %), peak RSS 4.62 → 3.32 GB
(−28 %), store inserts/insert 62.3 → 2.6, removals 58.5 → 0.0, and puts and leaves read per
insert unchanged. The prune now has nothing to remove at all, which is the clearest statement
of what this was: **the eviction machinery existed to undo work that no longer happens.**

One subtlety, and it is a data-loss bug rather than a slow path if it is got wrong: the merge
may only be taken on a genuine fault-in. A node already in `store` was put there earlier in
*this* batch, and the leaves under it are staged in an uncommitted `WriteBatch` that a range
scan cannot see, so recomputing from disk would silently drop them. Eight tests failed on that
before the residency check went in.

Like §5.2, this was found by the census rather than by reading the code. The two largest wins
of the round were both invisible to inspection: one was a read that should not have happened,
the other was a write into a hash map that was thrown away before the batch ended.

### 5.6 The on-disk frontier record: 163 bytes → 65

The in-memory frontier level stores nothing but two child hashes per node (§5.3); the on-disk
record did not follow. A frontier node was still written as a full `TAG_INTERIOR` bincode
record — `(merkle_hash, left, right, left_hash, right_hash)`, 163 bytes — with two 34-byte
child prefixes that, after §5.3, nothing reads. `TAG_FRONTIER` is 65 bytes: tag ‖ left_hash ‖
right_hash. `decode_node` reconstructs the rest from the record's own key — the child prefixes
positionally, and the node's own hash as `InteriorNode::calculate_hash(prefix, left, right)` —
because everything above the frontier is a perfect binary tree. This is the case §3.4 withdrew
as not worth doing, and it is worth doing here for a reason that does not apply below the
frontier.

Measured on a database built with `--max-frontier-depth 20` where F settled at 16:

- **bytes staged/insert 115.8 → 91.5**, a 24.3-byte drop — exactly 0.248 interior puts/insert
  × 98 bytes saved per record, which is an independent confirmation that every frontier put on
  the real insert path lands in the new format.
- **on disk, `du -sb` 196,147,418 → 184,208,973 bytes (−6.1 %)**, reproduced on a second pair
  of builds at 196,120,985 → 184,218,571. The frontier level alone accounts for 2^16 × 98 =
  6.4 MB of that; the rest is WAL and SST bookkeeping across the 16 depth advances.
- root hash unchanged end to end, both formats.
- throughput: 400,412.8 → 424,193.7 entries/s on one adjacent pair, consistent in direction
  across two runs but a single sample each, inside the stated 1–5 % noise floor. **Not claimed
  as a real effect.**

The same arithmetic holds at F = 19, and the end-of-session census is where it shows: bytes
staged per insert 246.4 → 157.1 is 0.911 interior puts × 98 bytes to within a rounding, which
also says that at this configuration *every* interior put is a frontier row.

**The case this is really for is reopen, and it is not measured.** Crash resumption scans the
whole frontier level; at the production frontier of 23 that is 8.4 M records, so 163 → 65
bytes is 1.37 GB → 546 MB of record scanned. That figure is **arithmetic, not a measurement** —
no F = 23 database was built in this session.

### 5.7 Rejected, with numbers

- **One atomic `WriteBatch` per `batch_upsert`** (§3.1.2 + §3.3.4). Built, and it is a
  **consistent 5.7 % regression** — 117,924 → 111,239 entries/s, every repetition, no overlap
  between the groups. *(Round-13 correction, §15.3: this was measured against the round-3
  per-subtree-commit baseline, on the accidental update workload — the same baseline the
  per-worker drain later beat by +16.2 % (§6.2). Against today's code the record-implied
  cost is ≈ −16 to −19 % resident, not −5.7 %.)* The mechanism is in RocksDB's `WriteThread::EnterAsBatchGroupLeader`:
  memtable insertion is parallelised across a write *group* only when the group has more than
  one writer, so collapsing ~9,100 small batches into one serialises onto a single skiplist
  pass what up to 64 rayon workers were doing concurrently. The review predicted this change
  would be *faster* ("one write syscall path, no commit-time validation"); it is not. It
  remains the correct fix if batch atomicity is wanted — it is the only thing that closes the
  torn-batch gap — but it is a correctness purchase, not a throughput one, and it was reverted
  here because atomicity is not required.
- **A 4 GiB block cache**: +2.4 % for +1.9 GB of RSS. Wrong trade when memory is a goal, and a
  thin claim against a 1–5 % noise floor.
- **`write_buffer_size` 256 MB**: +1.7 % against a 5.2 % spread. Not resolvable; stays at
  64 MB.
- **`max_open_files = -1` beating 4096 by 5.6 %**: an artefact. The reference database has 63
  SSTs, so a 4096-file cache cannot bind; that run's spread was 6.0 %.

### 5.8 The cumulative result

**Mid-session checkpoint**, taken after the flat array and before the allocator tuning and the
churn removal. Five interleaved repetitions, 2 M entries into copies of the respective
40 M-leaf reference databases, page cache dropped, spreads 3.7 % and 5.1 %:

| | start | changes so far |
|---|---|---|
| entries/s | 106,901 | **123,676** (+15.7 %) |
| subtree loads / insert | 1.916 | **0.972** (−49 %) |
| leaves read / insert | 33.7 | **22.8** (−32 %) |
| batch p99 | 277 ms | **187 ms** (−32 %) |
| reopen | 3.49 s | **3.02 s** (−13 %) |
| peak RSS | 3.22 GB | 4.34 GB (**+35 %**) |

That memory column was a real regression when it was written, and was recorded as one: the
child hashes take an interior record from 99 to 163 bytes, and at that point nothing offset
them.

**End of session.** Four interleaved repetitions, same protocol, spreads 1.6 % and 1.8 %:

| | start | all changes |
|---|---|---|
| entries/s | 108,479 | **257,951 (+137.8 %)** |
| subtree loads / insert | 1.916 | **0.972** (−49 %) |
| leaves read / insert | 33.7 | **22.8** (−32 %) |
| batch p50 | 81.1 ms | **34.7 ms** (−57 %) |
| batch p99 | 272.2 ms | **72.2 ms** (−73 %) |
| reopen | 3.47 s | **2.59 s** (−26 %) |
| bytes staged / insert | 188.1 | **157.1** (−16 %) |
| record puts / insert | 1.911 | 1.911 (unchanged) |
| peak RSS | 3.21 GB | 3.27 GB (parity) |

Both columns produce the same root hash,
`338e51ea3b356fdef3ca87bd650369fbc18b6127a00086b64188b610063df1d9`.

Two things to read off it. **The memory regression was absorbed**: the allocator tuning and
the churn removal took it back to parity while doing 2.4× the work. The child hashes still
cost 64 bytes an interior; they are simply no longer the dominant term. And **the number of
records written per insert did not move at all** — 1.911, of which 1.000 is the leaf. This
round moved reads, memory, CPU and bytes-per-record; it did not touch write volume, which is
the term that stalls at production scale.

*One methodological note, because it cost a measurement.* The first attempt at this A/B ran
while other work was on the machine and produced spreads of 31.3 % and 20.2 % (97,958 →
118,208 for the same two binaries). The medians were wrong by more than several of the
individual changes this round landed. It was discarded and re-run on a quiet box. A
measurement taken against a busy machine is not a weak measurement; it is not a measurement.

### 5.9 What this says about the remaining ideas

- **Nothing read-side can be ranked until there is a larger-than-RAM database.** §5.4's sweep
  is the argument, and it applies to this section's own read-side numbers as much as to the
  proposals: leaves read per insert is a structural metric and travels, throughput on a
  resident working set does not.
- **Band reads (§3.1.6)** are a worse bet than when filed. §5.2 already removed the part that
  was cheap — an untouched sibling no longer costs a load — and §5.4's `keep_below_frontier = 3`
  row is a direct proxy for what is left: fewer leaves read, lower throughput. The write side
  would cost roughly +0.9 interior puts per insert.
- **Incremental interior counts (§3.4)** are worth more than before, and for a new reason:
  §5.2 changed when the frontier advances, without meaning to, because the check conflates
  structure with residency.
- **Subtree pages + a merge operator (§3.3.8)** remains the only idea with order-of-magnitude
  potential, but it trades away fast crash resumption, which is a requirement.
- **The depth-24/25 accumulation (§3.1.5) is not fixed.** §5.4's cap only binds in scaled-down
  configurations, and §5.5 stops materialising everything below the retained band, not the
  band itself. At F = 23 the band is still 2^24 + 2^25 nodes by definition. Every memory
  number in this section was taken at F = 19, where that band is 3.1 M nodes; none of them
  bears on the production case. *(Round 4 addressed the representation — the band is
  positional slots at ~150 B a resident entry, §6.2 — though the F = 23 total remains
  arithmetic rather than measurement.)*
- **Re-profile.** The store churn §5.5 removed was ~96 % of the map traffic, and the flat
  arrays removed the probes above the frontier. Every profile cited anywhere in this document
  predates both. Nothing here identifies what is now at the top. *(Done — §6.1. What was at
  the top was RocksDB write-group formation, at ~35 % of every worker's active time.)*

---

## 6. Round 4 — the write path, profiled and then paid down

*§5.9 closed with "Re-profile: nothing here identifies what is now at the top." This round
is that profile and what it bought. Method and instrument are round 3's: the 40 M-leaf
reference database (`refdb_childhash`, F = 19 at its cap, L = 76), `tools/ab.py` with fixed
work, a fresh database copy and a dropped page cache per run, interleaved repetitions, a
quiet box, and the root hash as a cross-variant correctness gate. Steady-state numbers are
the **insert** workload (ab.py's post-§5 default seed; 40.0 M → 42.0 M leaves per run), not
round 3's accidental update workload. Raw results are preserved in `bench_out/ab_r4_*.json`;
the profile itself is `bench_out/profile_round4_insert_64threads_2026-08-30.json.gz`
(samply format — symbols resolve via `addr2line` against the binary that produced it).*

### 6.1 The profile

Taken over 2 M inserts on 64 rayon workers (221 K entries/s unprofiled, 168 K under
sampling). The workers are symmetric to within 2 %, so one worker's active samples stand
for the pool:

| share | of a worker's active time |
|---:|---|
| **~35 %** | **synchronisation around RocksDB's write path** — `sched_yield` 18.5 % self, `WriteThread::AwaitState` 4.6 %, futex/mutex waits the rest |
| 17.4 % | RocksDB read-path CPU (MergingIterator seek, block decode, index binary search) |
| 9.3 % | `pread` |
| 7.4 % | memcpy/memmove |
| 4.2 % | SHA-256 |
| 3.5 % | this crate's own tree code |
| ~6 % | malloc/free, DashMap, rayon |

By call tree: `RocksStorage::write_batch` (i.e. `DBImpl::Write`) is under **38.7 %** of a
worker's time, `rocksdb_iter_seek` 25.0 %, and `upsert_transient_subtree` 41.4 % (it
contains the seeks and the merge). Off-CPU futex *waits* are excluded by construction, so
the true synchronisation share of wall clock is higher than 35 %.

The mechanism: `upsert_frontier` committed one `WriteBatch` per touched frontier subtree —
~9,100 `db.write()` calls per 10 K-entry batch, from 64 threads at once — and every commit
joins RocksDB's write group behind a leader election that spins on `sched_yield`. Two
corroborations, cheap enough to run before betting a build on the reading: dropping to 16
rayon threads costs only 34 % of throughput (146 K vs 221 K — each thread becomes ~2.6×
more productive when there is less write-path fighting), and §6.2's control arms below.

Two things the profile overturned. **Hashing is not the bottleneck and has not been for
two rounds**: SHA-256 at 4.2 % retires §3.2.6's 40–50 % figure, and with it any interest
in BLAKE3. And the read path, while real (25 % of worker time), is *second* — every prior
ranking put reads first because the profiles predated §5.5.

### 6.2 Five proposals, built in parallel, measured one at a time

Every proposal was implemented against the same base, tested to a green suite, and
measured in one 9-variant interleaved ab.py session (36 runs; **the root hash agreed
across all of them**), plus a from-empty growth session. Steady state is 2 M inserts into
the reference copy; growth is 3 M inserts into an empty database at the default
configuration.

| proposal | steady state | from empty | verdict |
|---|---:|---:|---|
| per-worker `WriteBatch` aggregation | **+16.2 %** | **+13.3 %** | **landed** |
| positional band slots | **+11.9 %**, RSS **−44 %**, p99 **−49 %** | −8.7 %, RSS +48 % | **landed** |
| per-depth interior counts | +0.4 % (noise) | **+12.2 %** | **landed** |
| plain `DB` (drop `OptimisticTransactionDB`) | +1.7 % (noise) | — | **landed** (simplification + knob) |
| `unordered_write` (needs plain `DB`) | +5.9 % | −8.3 % | knob, default off — **removed in round 5** |
| prefix bloom + extractor | **−2.8 %** on/off, same DB | — | **rejected** |

- **Per-worker batches** (the profile's direct answer): `upsert_frontier` stages into one
  open batch per rayon worker; `batch_upsert_optimized` drains the ~65 batches
  concurrently — still a multi-writer group, so memtable insertion stays parallel — before
  the metadata commit. The middle point between the two measured ends: per-subtree
  (~9,100 group joins, the 35 % above) and §5.7's single atomic batch (−5.7 %,
  one-writer group). Batch p99 rises 175 → 192 ms steady-state, the drain bunching
  commits; the cumulative column below shows the integrated tree more than wins it back.
- **Band slots** (§3.1.5's accumulation, finally represented instead of bounded): the
  retained band moves out of the `DashMap<Prefix, Node>` into positional slots of two
  child hashes each — `FrontierLevel`'s argument, one band deeper. Steady state is the
  design's target regime and it collects on every axis at once. The growth phase pays:
  with `depth_always_keep` binding, a pass-through chain costs one slot per *position*
  rather than one node, so building 3 M entries from empty runs −8.7 % at +0.8 GB. That
  trade is accepted knowingly — production is the cap regime — and slot-per-node
  compression of pass-through chains is the named follow-up. An adversarial review
  attempted refutation across eight grounds (store-to-slots handover, Empty-vs-uncached,
  eviction-depth movement, advance re-basing, concurrency, key packing, persist parity,
  store staleness) and closed all of them.
- **Interior counts** (§3.4, promoted by §5.9): neutral at the cap — the cap check
  refuses before any counter is read — and +12.2 % while growing, which is where the
  probe loop lived. Landed with its band-side twin (`BandSlots::full_counts`), since the
  slots would otherwise have re-created the very probe the counters remove.
- **Plain `DB`**: +1.7 % is inside the 1–5 % noise floor and is not claimed; it landed
  for what it deletes (§3.3.4's verdict) and for `unordered_write`, whose control-arm
  decomposition is the cleanest confirmation of the profile this round produced:
  `--no-pipelined-write --no-manual-wal-flush` alone is **−26.8 %**; adding
  `--unordered-write` to that is **+44.7 %** — removing write-group ordering is worth
  almost half again of throughput — netting +5.9 % over baseline, less than the
  per-worker batches take while keeping pipelined writes and the buffered WAL. Found on
  the way: vendored RocksDB 8.10 self-deadlocks on `unordered_write` +
  `manual_wal_flush` (`ConcurrentWriteToWAL` re-locks `log_write_mutex_`), so that pair
  is rejected at open with an error naming both knobs.

  > **Corrected — the knob is gone (round 5).** At the 1.0 B-leaf Tier-0 database the
  > +5.9 % did not survive: −0.3 % against a 2–5 % spread (§7.2). At scale the workload
  > is read-bound and write-group sync no longer binds. With its best case at noise, its
  > growth case at −8.3 %, and a version-specific deadlock to guard against,
  > `unordered_write` was removed along with the rest of `RocksStorageConfig`.
- **Prefix bloom**: rejected on the one clean comparison — bloom-on vs bloom-off against
  the *same* rebuilt database, −2.8 %. See §3.3.2 for the two errors in the original
  sketch this exposed.

One protocol lesson, recorded for the next per-variant-reference A/B: a freshly built
reference database is measurably slower to insert into than the day-old copy of its
logical twin (−8.8 % for the bloom-off arm against baseline, LSM shape and compaction
debt being real state), so **a format- or filter-changing variant must be judged against
its own database's on/off arms**, never directly against the shared reference.

### 6.3 The cumulative result

Steady state: 2 M inserts into copies of the 40 M-leaf reference database, four
interleaved repetitions, spreads 2.0 % and 3.2 %, identical root hash:

| | round-3 end (`4afbafd`) | round 4 |
|---|---:|---:|
| entries/s | 225,240 | **307,494 (+36.5 %)** |
| peak RSS | 6.77 GB | **3.70 GB (−45 %)** |
| batch p99 | 192.2 ms | **85.8 ms (−55 %)** |
| record puts / insert | 1.911 | 1.911 (unchanged) |
| leaves read / insert | 18.5 | 18.3 |

The parts compose better than they sum (+16.2 % and +11.9 % standalone): the band slots
make each subtree visit cheaper, which sharpens the write-group contention the per-worker
batches then remove.

Building the reference database itself — 40 M entries from empty at the §5.2 recipe's
configuration (`--max-frontier-depth 20`), two repetitions, spreads 2.6 % and 0.1 %:

| | round-3 end | round 4 |
|---|---:|---:|
| entries/s | 226,759 | **346,940 (+53.0 %)** |
| peak RSS | 3.05 GB | **2.35 GB (−23 %)** |

**and both builds end at root `338e51ea…`** — the canonical reference root, so the
integrated tree is bit-for-bit the same logical structure. Note what this says about the
band-slot growth trade of §6.2: it belongs to the *default* configuration, where
`depth_always_keep = 23` holds the band deep and sparse while the tree is small; at the
scaled-down configuration every reference build actually uses, the round is +53 % on the
build phase too. For the record against round 3: its corrected insert-workload A/B ran
106,335 → 249,218 entries/s, and this round's baseline re-measured that same end state at
225,240 in its own session (same binary, different day — a reminder of why every
comparison here is same-session and interleaved). Chaining the two rounds on their own
baselines puts the branch at roughly **2.9× the round-3 starting point** on the insert
workload, now at 3.70 GB peak RSS where the round-3 code ran ~6.8 GB on the same
workload.

### 6.4 What this leaves

- **Tier 0 is unchanged and now blocks three things**: the read path (25 % of worker
  time, second place), bloom's surviving L0 case, and any at-scale verdict on
  `unordered_write` — all wait on a larger-than-RAM database.

  > **Resolved (round 5).** The Tier-0 database exists — 1.0009 B leaves, 68 GB of SST
  > against 62.6 GiB of RAM (BENCHMARK-BASELINE.md §5.4) — and the verdicts it was
  > holding are in §7: block cache and `unordered_write` measured and retired; the read
  > path confirmed dominant at scale (57.1 leaves read/insert against 18.3 at 40 M) and
  > now the standing target.
- **The growth phase regressed knowingly** (band slots, −8.7 % from empty at the default
  configuration). Slot-per-node compression of pass-through chains would take the trade
  back; until then, bulk builds prefer a scaled-down `max_frontier_depth`, which they
  use anyway (BENCHMARK-BASELINE.md §5.1).
- **The torn-batch gap (§3.1.2) is unchanged in kind**: the F ≥ 1 batch now commits as
  ~65 per-worker batches at the drain instead of ~9,100 spread through the recursion —
  a narrower window, still not atomicity.
- **§3.3.8 (subtree pages + merge operator)** remains the only order-of-magnitude idea,
  still gated on a crash-resumption story.

---

## 7. Round 5 (2026-08-30): Tier 0 arrives, and the knobs retire

The goal of this round was not throughput but **surface**: reduce the tuning options so
the crate is performant with nothing to configure. The method is the same as every other
round — nothing was deleted on argument alone. Every knob either already carried a
recorded sweep, or got one now against the database that its open verdict was waiting
for.

### 7.1 The Tier-0 database, and the first at-scale numbers

`my_100m_db` (the name undersells it: **1.0009 B leaves**, frontier at its cap of 23,
L = 119, 68.45 GB of SST against 62.6 GiB of RAM) was built 2026-08-30 entirely at the
default configuration — the flagship database needed zero knobs, which is the summary of
this round in one fact. Provenance and shape: BENCHMARK-BASELINE.md §5.4. A 74 GB
reference cannot be copied per run, so `ab.py` gained `--link-copy` (hardlinked SSTs,
real copies of everything RocksDB appends to; ~102 MB per scratch copy), described
there too.

First at-scale steady state (2 M inserts, cold cache, median of 3): **96,740 entries/s**
— 3.2× slower than the same binary at the 40 M scale — with **57.1 leaves read/insert**
against 18.3 at 40 M and puts/insert unchanged at 1.994. The frontier cap's O(N) read
amplification, measured rather than extrapolated: the read path is now not second place
but the whole game at scale.

### 7.2 The sweeps: five arms at Tier 0, one at 40 M

One interleaved session (`ab_t5_tier3.json`, 5 variants × 3 reps, 2 M entries each, all
15 runs root-hash-identical at `5a86bd92…`):

| variant | entries/s (median) | spread | p99 ms | peak RSS | vs base |
|---|---:|---:|---:|---:|---:|
| base (all defaults) | 96,740 | 5.3 % | 168.4 | 5.80 GB | — |
| block cache 4 GiB | 97,665 | 3.1 % | 163.7 | 8.92 GB | +1.0 % |
| block cache 16 GiB | 96,147 | 3.1 % | 161.3 | 21.61 GB | −0.6 % |
| unordered write | 96,404 | 2.4 % | 169.2 | 5.81 GB | −0.3 % |
| unsorted windows | 95,615 | 0.3 % | 152.9 | 5.81 GB | −1.2 % |

- **Block cache: 1 GiB stands, knob removed.** §5.7's rejection at 40 M was argued from
  a resident working set and left open at scale; at 68 GB against 62 GiB of RAM the
  verdict is the same — +1.0 % / −0.6 % against 3–5 % spreads, with the 16 GiB arm
  paying +15.8 GB of RSS for its nothing. A cold-cache fixed-work run re-reads too
  little for the cache to matter, and a warm cache is the page cache's job.
- **`unordered_write`: removed** (correction recorded at §6.2). Round 4's +5.9 % at 40 M
  inverted to −0.3 % here: the at-scale workload is read-bound, and per-worker batch
  aggregation (§6.2) had already removed most of the write-group pressure the knob
  existed to relieve. Its growth arm was −8.3 %, and keeping it meant keeping a
  RocksDB-8.10 deadlock guard. The deadlock's root cause stays documented at the
  removal site in `rocks.rs` for whoever considers re-adding it.
- **Window sorting: stays, flag removed.** §3's worry that sorted windows flatter
  subtree sharing was answered by measuring both orders at both scales
  (`ab_t5_sort40m.json`): sorted is **+12.3 %** at 40 M (305,568 vs 268,080, spreads
  1.5 %/0.1 %) and a wash at Tier 0, same root hash either way. Sorted is also visible
  in the census — 1.911 vs 1.991 puts/insert, range-clustered chunks touching fewer
  frontier subtrees per commit. The recorded protocol was already the right one, so the
  A/B knob had nothing left to decide.

### 7.3 What the surface is now

Thirteen public tuning options became one. `RocksTransRelConfig` keeps a single knob,
`max_frontier_depth` (`with_max_frontier_depth(24)` is the default; 20 is the cheap
reference-database regime of BENCHMARK-BASELINE.md §5.1) — it survives because it is a
genuine trade-off (memory and write volume against read amplification) with two
recorded operating points, not a value with one defensible setting. Everything else:

| removed | resolution |
|---|---|
| `depth_to_write` | deleted — values above 1 were inert (pinned by test before removal) |
| `depth_always_keep` | derived: `max_frontier_depth − 1`, the only value that was never measured worse (§5.4) |
| `keep_below_frontier` | fixed at 2, the swept optimum on both axes (§5.4) |
| `log_leaves_per_frontier` | fixed at 1; the gate it feeds never binds at depths that matter |
| `write_buffer_size` | fixed at 64 MB; 256 MB measured +1.7 % against a 5.2 % spread (§5.7) |
| `block_cache_size` | fixed at 1 GiB; swept at both scales (§7.2) |
| `max_open_files`, `max_background_jobs`, `max_subcompactions` | fixed at 4096/8/4, the settled campaign values |
| `pipelined_write` | always on (+5.3 %, §3.3.7); its A/B flag is gone |
| `manual_wal_flush` | always on (its absence is part of the −26.8 % control arm, §6.2) |
| `unordered_write` | deleted (§7.2) |
| bench `--no-sort`, `--no-malloc-tuning`, `--in-memory` | deleted; sorting and malloc tuning are unconditional, `-b memory` remains |

`RocksStorageConfig` no longer exists; each hardcoded value carries its measurement in a
doc comment at the definition. `RocksTransRelMPT::new_with_path(path)` is now the whole
performant API. `bench` went from 22 flags to 10, of which one tunes anything, and its
`-w/-c` defaults changed from 10 000/1 000 — a shape nothing was ever measured at — to
the protocol's 100 000/10 000.

The refactor was gated on its own A/B (`ab_t5_neutrality.json`): pre- vs post-removal
binaries on the 40 M reference measure +0.5 % inside a 1.2 % spread, identical census,
identical root hash `7227d3e1…` — the same root the sort session's base arm produced in
its own session, as it must.

### 7.4 Artifacts

- `bench_out/ab_t5_tier3.json` — the 5-arm Tier-0 sweep; `ab_t5_sort40m.json` — sort
  order at 40 M; `ab_t5_neutrality.json` — the refactor gate; `ab_t5_smoke.json` — the
  100 K-entry Tier-0 smoke run (56 K/s cold, before any warm-up).
- `bench_out/bin/t5_knobs` — the last binary with every flag (for re-sweeping against a
  future database); `bench_out/bin/t5_tip` — the post-removal binary.
- `bench_out/mybuild.csv` — the Tier-0 database's final build segment.

### 7.5 What this leaves

- **The read path at scale is the whole game**: 57.1 leaves read/insert at L = 119 is
  the frontier design paying its known rent, and §3.3.8 (subtree pages + merge
  operator) is still the only idea sized to it. Tier 0 now exists to judge it against.
- **Bloom's surviving L0 case** (§3.3.2) can now also be tested honestly — build a
  bloom variant of a Tier-0-scale database and judge on/off within it.
- The growth-phase band-slot regression (−8.7 %, §6.2) is unchanged, with the same
  named fix (slot-per-node compression of pass-through chains).

---

## 8. Round 6 (2026-08-30): the read path profiled at Tier 0, and the LSM-shape tax

§7 ended with the read path as the whole game at scale. This round is the profile that
says *which part* of a read costs, and the first measured lever against it.

### 8.1 The profile: the seek costs twice the scan it sets up

Instrument: samply over 2 M inserts into a `--link-copy` scratch of the Tier-0 database,
cold cache, insert-workload seed; 87 K entries/s under sampling (95 K unsampled).
Artifact: `bench_out/profile_round6_tier0_insert_2026-08-30.json.gz`, symbols via
`addr2line` against `bench_out/bin/t5_tip`.

- **Workers are ~25 % CPU-active** (≈5.7 s of CPU per worker over the 23 s insert
  window, 64 workers). The other 75 % is off-CPU, blocked on SST reads. Throughput at
  Tier 0 is an I/O-latency number, not a compute number.
- Of a worker's *active* time, **32.4 % is `pread` of SST data blocks**, split
  unevenly: **21.5 % under `MergingIterator::Seek`** — positioning the leaf-scan
  iterator — against **10.7 % under `Next`/`FindBlockForward`**, the scan itself. The
  seek walks every sorted run (each L0 file plus one file per populated level; this
  database sits at ~9–13 runs), reading an index and data block in each, most of which
  contribute nothing to the 57-leaf range that follows.
- Census mechanics behind that: 0.997 subtree loads per insert (at L = 119 and 10 K-entry
  batches, nearly every insert faults its own subtree) and 57.3 leaves read per load —
  L/2, the frontier-child merge, because a fresh process starts with a cold band; a
  long-lived process descends to the band bottom and pays L/4.

So the per-insert read bill is roughly `seek × sorted_runs + scan(L/2)`, and the seek
term is the larger half.

### 8.2 One tree, two shapes: the sorted-run count is worth 28 %

The run-count term is separable from everything else by comparing the same logical tree
in two LSM shapes. New reference pair at the Tier-0 regime, per §5.1's scaling
(F = 19 at its cap, **L = 119** — the Tier-0 value):

- `ref62_l119` — 62.4 M leaves built from empty in 195 s (320 K entries/s), 4.6 GB,
  84 SSTs across L0(9)/L4/L5/L6 ≈ 13 sorted runs, root `f9b38701…`
  (`bench_out/ref62build.csv`):
  `bench -b rocks -n 62400000 --max-frontier-depth 20 --seed 456968137849 -t 7200 ref62_l119`
- `ref62_compact` — its copy collapsed to **one sorted run** (79 files, all L6) by the
  new `compact-db` tool in 24.7 s.

`ab.py --link-copy`, 2 M inserts, 3 interleaved reps, cold cache, same binary
(`ab_r6_compaction.json`; all runs root `dc96b997…`):

| variant | entries/s | spread | leaves read/ins | p99 ms | peak RSS |
|---|---:|---:|---:|---:|---:|
| as-built (~13 runs) | 215,872 | 2.4 % | 28.6 | 154.3 | 3.85 GB |
| compacted (1 run) | **276,738 (+28.2 %)** | 0.6 % | 28.6 | 105.5 | 3.47 GB |

Identical census — the *logical* reads didn't move; every physical read behind each of
them got cheaper. **+28.2 % throughput, p99 −32 %, and the tight 0.6 % spread is itself
evidence**: less variance is what fewer random reads per operation looks like.

### 8.3 What this ranks

1. **§3.3.8 subtree pages (+ merge operator) is promoted from "worth a prototype" to
   the next round's build.** The profile prices exactly what it removes: the per-load
   seek-and-merge across every sorted run becomes one point `Get` (bloom-skippable,
   one data block), and with a merge operator the insert path reads nothing at all.
   Protocol note for that round: it is a format change, so both arms must be
   fresh-built at the `ref62_l119` recipe and judged within their own databases
   (§6.2's lesson). The questions it has to answer: crash resumption (§3.3.8's gate),
   page-split policy around L ≈ 128, and whether page-sized values (~8 KB at L = 119)
   move the write path backwards — staged bytes today are 165 per insert.
2. **An ops lever exists today**: full compaction is +28 % at the L = 119 regime for
   25 s of work on 4.6 GB. `my_100m_db` sits at ~13 runs and would benefit
   proportionally; deliberately **not** done here — it is the shared reference and its
   LSM shape is recorded, comparable state (§6.2's fresh-vs-aged lesson). A future
   Tier-0 round that wants the compacted baseline should compact a copy, or accept
   re-baselining.
3. **Compaction policy** (keep the run count low continuously, paying background
   writes) is the smaller, always-on version of 2 — `level_compaction_dynamic_level_bytes`,
   periodic compaction, or a lower L0 trigger. Worth one sweep only after 1 settles,
   since subtree pages change what compaction is compacting.

### 8.4 Artifacts *(round 7's are at §9.6)*

- `bench_out/profile_round6_tier0_insert_2026-08-30.json.gz` — the Tier-0 profile
  (36 K lost events noted; sampling buffer overflow during the heaviest I/O bursts).
- `bench_out/ab_r6_compaction.json`, `bench_out/ref62build.csv`.
- `ref62_l119/`, `ref62_compact/` — the L = 119 reference pair (rebuild: 195 s via the
  §8.2 command; the compact twin in another 25 s). Kept on disk for the §3.3.8 round.
  *(Both deleted 2026-08-30 during that round's disk crunch; same rebuilds apply.)*
- `src/bin/compact_db.rs` — `compact-db <path>`, full-range compaction with the
  storage layer's background-job settings.

---

## 9. Round 7 (2026-08-30): subtree pages, built and rejected at every scale

§8.3 promoted §3.3.8 — "the only order-of-magnitude idea" — from sketch to build. This
round built it twice (the read-modify-write form, then the merge-operator form the sketch
actually meant), measured it at three scales, and rejects it with the numbers below. The
branch is archived whole: `bench_out/r7_pages_rejected.patch` (four commits,
`r6/subtree-pages`, 605 insertions), plus the binaries.

### 9.1 What was built

A `TAG_PAGE` record holds one frontier node's entire sorted leaf set (64 bytes a leaf),
keyed by the node's positional prefix under a high bit that keeps pages out of every node
scan. `upsert_frontier` becomes: one bloom-filtered point `Get` of the page, merge in
memory, hash the touched half through `build_band` (band slots and the advance gate stay
maintained), stage the write. Pages follow the frontier — each advance splits every page
at the old frontier bit; the first advance converts the F = 0 row era and deletes it.
Every correctness gate this repo has was green the whole way: the full suite, clippy,
from-empty root parity with the row binary (`1ca935de…` at 500 K), reference-build parity
(`f9b38701…` at 62.4 M — bit-identical logical tree in both formats), and fixed-work A/B
parity (`dc96b997…`, all reps, both formats).

Two write strategies for the staged page:

- **Read-modify-write** (`put_page(&merged)`): stage the whole ~7.6 KB page per insert.
- **Blind merge operand** (`merge_page(entries)`): stage only the batch's 64-byte-a-leaf
  slice; a RocksDB merge operator (`page_merge`, registered at every open) folds operands
  into the page at compaction or on read.

### 9.2 Read-modify-write: rejected in one session

`ab_r6_pages.json`, 62.4 M-leaf references (L = 119, the Tier-0 regime per §5.1 scaling),
each format fresh-built per §6.2's format-changer protocol, 2 M inserts, 3 reps:

| | entries/s | leaves read/ins | bytes staged/ins |
|---|---:|---:|---:|
| rows | 207,940 | 28.6 | 165 |
| pages, RMW | **14,621 (−93.0 %)** | 110.1 | 3,899 |

Exactly the trade §3.3.8 warned about, unmitigated: page-sized values per insert turn the
workload write-bound. Its from-empty build ran at 26 K entries/s against rows' 320 K.

### 9.3 Merge operands: correct, and still behind at every operating point

The blind write repairs the write side (staged bytes back to row-format levels; from-empty
500 K build at 493 K entries/s vs rows' 510 K) and keeps every root-hash gate green. The
ledger, all sessions cold-cache fixed-work (`ab_r6_pagemerge.json`,
`ab_r7_tier0_pages*.json`, `tier0pages_build.csv`):

| operating point | rows | pages + merge | pages vs rows |
|---|---:|---:|---:|
| steady state, 62.4 M (≤ RAM) | 222,848 (1.2 %) | 185,791 (1.8 %) | **−16.6 %** |
| — its cold first segment | 118 K | 142 K | **+20 %** |
| — its warm tail | 298 K | 217 K | −27 % |
| Tier 0 (1.0009 B, > RAM), as-built | 96,740 (§7.1, recorded) | 87,371 (19.1 % spread) | **−9.7 %** |
| Tier 0, fully compacted | ~124 K (projected: 96,740 × §8.2's +28.2 %) | 127,996 (10.6 %) | **≈ parity (+3 %, inside spreads)** |
| build to 1.0009 B | ~175 K/s (95 min, §5.4) | 127,381/s (131 min) | −27 % |

The cold-segment win is real and is the fraction of §3.3.8's promise that survives
contact: where reads are cold random I/O, one page `Get` beats a multi-run seek-and-scan.
It is not enough. The page `Get` is an ~8.8 KB value (2–3 blocks cold) plus a full-page
decode and a half-page rehash per touch — 118.7 leaves read per insert against rows' 28.6
— and the as-built page database is **bigger** (87 GB vs 68 GB; full 32-byte keys per leaf
inside values, and random-hash page bodies that Snappy cannot touch), so its cold reads
are colder. Folding operands compacts it to 62 GB, below rows — but rows compacted gain
+28.2 % too, and at that point the formats are indistinguishable within spread. **Round
6's ops lever captures the order-of-magnitude idea's entire best case, with no format
change.**

### 9.4 The disqualifier: silent truncation by any operator-less open

Found because the Tier-0 measurements' root hashes refused to reconcile: a database with
pending merge operands, opened once by a binary that does not register the operator, does
not error — RocksDB's point-in-time recovery **silently stops early and the WAL tail is
consumed**. The Tier-0 build lost 200,000 of its 1,000,900,000 leaves to one such open
(this round's own `compact-db`, before its fix in the branch: `Point in time recovered to
log #29237 seq #2013326038`). Reproduced minimally: a 200 K-leaf page database opened
once operator-less reopens at **100 K leaves with a forked root**; its untouched twin
keeps all 200 K.

A format whose integrity silently depends on every future tool registering the right
merge operator is disqualified for a system whose root hash is the product, independent
of the throughput ledger above. Together with the two gaps the prototype declared up
front (no full recovery for pages, crash-mid-repage unstitched — the §3.3.8 crash story
that was always its gate, plus a measured 194 s advance stall for the depth-23 re-page),
the verdict is **rejected and archived**, with the cold-segment number recorded as what a
future read-path idea has to beat without giving up the write path.

### 9.5 What this settles

- **§3.3.8 is closed.** Both halves are now measured: RMW pages −93 %, merge-operand
  pages at best parity-after-compaction, plus an operational failure mode. The
  "order-of-magnitude" potential was the seek fan-out, and §8's compaction lever already
  collects it.
- **The row format stands** on write volume (165 B staged/insert), size (68 GB at 1 B),
  build rate, warm-path CPU, and operational simplicity (any binary can open it).
- **Standing read-path headroom**: keep the run count low (compaction policy, §8.3 item
  3, now the front of the queue) — and the cold-segment +20 % marks what a
  non-format-changing read optimisation could still win at Tier 0.

### 9.6 Artifacts

- `bench_out/r7_pages_rejected.patch` — the whole branch against `main` (0078453);
  binaries `bench_out/bin/r6_pages` (RMW) and `bench_out/bin/r6_pagemerge` (operands).
- `bench_out/ab_r6_pages.json`, `ab_r6_pagemerge.json`, `ab_r7_tier0_pages.json`,
  `ab_r7_tier0_pages_compacted.json`, `cold_rows.csv`/`cold_pages.csv` (the segment
  decomposition), `ref62pages_build.csv`, `tier0pages_build.csv`.
- Databases: all deleted for disk (`tier0_pages` 62 GB rejected; `ref62_pages`;
  `ref62_l119`; `ref62_compact`; `refdb_childhash`; `my_100m_db` §5.4). Rebuild recipes:
  §5.2 (40 M rows, ~2 min), §8.2 (62.4 M rows, 195 s), §5.4 (1 B rows, ~95 min); the page
  builds only from the archived branch.
- One measurement-hygiene lesson, recorded because it cost a session: an A/B harness
  invocation names binaries by *path*, and a stale path plus a `cd` produced a round of
  numbers for the wrong binary — caught only because `leaves read/insert 0.1` was
  impossible and the root hash disagreed. The census and the root oracle are why the
  protocol catches what eyeballs miss.

---

## 10. Round 8 (2026-08-30): compaction policy — the knobs do nothing, the schedule is the policy

§8.3 left "keep the run count low continuously" at the front of the queue, and §9.5 moved
it up. This round swept the candidate settings and measured the one policy that works.
`bench` now reports the **sorted-run count** (each L0 file plus one per populated level —
the multiplier on every leaf-scan seek) so a run's throughput arrives next to the shape
that produced it; `ab.py` medians it.

### 10.1 The sweep: leveled-compaction settings are noise here

Five arms, 20 M inserts each (long enough for ~70 L0 file lifecycles, so policy has room
to express), 3 interleaved reps on copies of the rebuilt `ref62_l119`, cold cache, all 15
runs root-identical at `9f71f294…` (`ab_r8_policy.json`; variant binaries verified
option-by-option in their scratch LOGs — §9.6's lesson applied):

| arm | entries/s | spread | p99 ms | end runs | vs base |
|---|---:|---:|---:|---:|---:|
| base (trigger 4, multiplier 10) | 244,679 | 4.6 % | 142.7 | 7 | — |
| L0 trigger 2 | 240,018 | 4.0 % | 167.5 | 7 | −1.9 % |
| multiplier 16 | 245,405 | 1.7 % | 152.9 | 8 | +0.3 % |
| both | 241,899 | 4.0 % | 193.7 | 8 | −1.1 % |
| **base, from a compacted start** | **285,338** | 3.1 % | **102.6** | 10 | **+16.6 %** |

The settings arms are indistinguishable from base, and the aggressive L0 trigger buys its
nothing at +17 % p99 — more frequent small compactions stall the foreground without
changing the resting shape (end-state run count is set by the run's own churn, not the
trigger). `level_compaction_dynamic_level_bytes` was already on; universal compaction was
not run — its ~2× space amplification is disqualifying on this disk, and with every
cheaper lever measuring zero the run-count-bounding it offers is already available for
free below.

### 10.2 The decay curve: compaction's half-life is ~13 % of the tree

What *does* work is starting compacted — but §8.2's +28.2 % was a 2 M probe taken at the
peak of a curve. Two instrumented 20 M runs (`decay_asbuilt.csv` / `decay_compacted.csv`,
rate per 2 M-insert segment):

```
compacted start: 277K 358K 392K 341K 320K | 279K 268K 159K 246K
as-built start:  233K 278K 291K 316K 315K | 310K 285K 153K 256K
```

The advantage runs +19 % → +35 % for the first ~8 M inserts (~13 % of the 62 M tree),
then decays to parity as both copies converge on the same churned shape. Integrated over
the 20 M-run: +16.6 % (§10.1); integrated to its horizon: a 29.4 s compaction of this
database buys back **about 5 s** of insert time per cycle.

### 10.3 The verdict, and the honest arithmetic

- **Nothing lands in the defaults.** Both swept settings are hardcoded-by-measurement at
  their current values, consistent with round 5: the values are right because the
  alternatives measured zero, not because nobody looked.
- **Periodic full compaction is an ops schedule, not a code change — and it is a net
  *loss* if paid inline at this scale** (29.4 s spent, ~5 s returned). It pays when the
  stall is free: an idle window, a maintenance slot, or immediately before a read-heavy
  phase. At Tier 0 the delta is larger (+28 % class, §8.2) and the compaction longer
  (~5 min per 62 GB), so the inline arithmetic is closer to break-even there — schedule
  it, don't inline it. `compact-db` exists for exactly this.
- This also retro-contextualises §8.2 and §9.3: the +28.2 % (and the pages format's
  parity-after-compaction) are peak-of-curve numbers. Sustained, the LSM gives some of it
  back on its own schedule whatever the settings say — which is the strongest argument
  yet that the remaining read-path headroom at Tier 0 belongs to ideas that don't fight
  compaction at all (the §9.5 cold-segment mark stands).

### 10.4 Artifacts

`bench_out/ab_r8_policy.json` (+ `.log`), `bench_out/decay_{asbuilt,compacted}.csv`,
`bench_out/bin/r8_{base,l0t2,mult16,combo}` (the option-verified variant binaries).
`ref62_l119` and `ref62_compact` are back on disk (rebuilt at the §8.2 recipe after round
7's deletions; roots `f9b38701…` and byte-twin respectively). The sorted-run
instrumentation landed on `main`.

---

## 11. Round 9 (2026-08-30): I/O concurrency at the read path — built, measured, rejected at the operating point

§8.1 left one term nobody had attacked: the leaf-scan seek walks every sorted run as a
chain of *serial* blocking `pread`s, one worker thread at a time, while the workers sit
75 % off-CPU. Two non-format-changing levers promise to overlap those reads — oversizing
the rayon pool, and RocksDB's `ReadOptions::async_io` over io_uring. This round built a
cheap read-bound instrument, verified the device has headroom, measured both levers at
three residency points, and rejected both as defaults. The surviving output is the
residency arithmetic itself, an ops-lever note, and the costing that closes ranking
item 7 (stale interior bands).

### 11.1 The instrument: a read-bound regime without a 95-minute rebuild

Ranking item 1's aside — "a cgroup memory limit … has not been tried" — is still not
tryable here (no root, `no_new_privileges` blocks sudo), but the same squeeze works from
userspace: `tools/memhog.c` faults in N GiB of anonymous memory, re-touches it in a slow
rolling pass so reclaim always prefers evicting file-backed cache, and sets its own
`oom_score_adj` to 1000 so the OOM killer can never take the benchmark instead. Against
`ref62_l119` (4.6 GB, L = 119 — §8.2's at-scale instrument) on the standard `ab.py
--link-copy` protocol:

| regime | hog | cache left for ~5 GB of DB | tip entries/s |
|---|---:|---:|---:|
| resident (uncapped) | — | all of it | 232,805 (0.9 % spread) |
| Tier-0-like (~90 % resident) | 43 GiB | ~5 GB | 221,949 |
| harsh (~40 % resident) | 46 GiB | ~2 GB | 120,699–139,536 |

The gates hold under the hog — identical census (1.911 puts, 28.6 leaves read/insert)
and root `dc96b997…` on every run — so only the physical cost of a read moved, which is
the definition of the instrument working. Its flaw, honestly: run-to-run spread at the
harsh point is 7–14 % against the quiet machine's 1–5 %, because reclaim pressure is
itself noisy. Verdicts from it need interleaved reps and should be read at the ±3–5 %
grain, and anything inside that at the *mild* point was judged there instead, where
spreads stay at 2–4 %.

### 11.2 The diagnostic: the device has headroom; overlap should convert

Sampled `/proc/diskstats` (`bench_out/r9_disk_probe{A,B}.log`): uncapped, the NVMe is
busy for the first ~13 s of a cold-cache run and idle after — the resident regime reads
nothing, as §5.4 said. Under the harsh hog it sustains ~220 K read IOPS, ~1.4 GB/s,
0.17 ms per read, ~85–88 % utilisation at queue depth ~40 — busy but *not saturated*:
latency is flat, and the same device had already shown 245 K IOPS in bursts. So the
fork read: more overlap per operation should convert to throughput. It did — but only
deep in the uncached regime, which is the finding.

### 11.3 Thread oversubscription: real, cheap, and wrong-way at the operating point

The rayon pool defaults to nproc (64), sized for CPU-bound work; blocked-on-`pread`
workers leave it undersubscribed as an I/O issue queue. `RAYON_NUM_THREADS` is already
an external knob, so this needed no code — wrapper scripts as `ab.py` variants:

| regime | 64 threads | 128 | 256 |
|---|---:|---:|---:|
| harsh (~40 % resident) | 127,898 | **146,739 (+14.7 %)** | 144,538 (+13.0 %) |
| Tier-0-like (~90 %) | 221,949 | 205,423 (**−7.4 %**) | — |
| resident | 232,805 | 214,046 (**−8.1 %**, 1.4 % spread) | — |

The crossover sits far below Tier 0's ~92 % residency: production never reaches the
regime where this pays. It stays what it already was — an environment variable — with
the measurement recorded: worth setting only when the database dwarfs RAM by enough
that most seeks miss cache, and costing ~8 % everywhere else. Nothing lands.

### 11.4 `async_io` + io_uring: engaged, verified, and still a net loss

The direct attack on §8.1's 21.5 % seek term: `ReadOptions::set_async_io(true)` on the
leaf-scan iterator, with the `io-uring` cargo feature compiling in
`ROCKSDB_IOURING_PRESENT` (liburing built from source into a home prefix and linked
statically; the container's seccomp allows `io_uring_setup` — checked before building).
Engagement was verified behaviourally, not assumed: the ring fd's `fdinfo` SqTail
counter advances only in the async build (9,101+ submissions in a 300 K-insert smoke)
and stays 0 in the feature-only control. Three arms so the option is isolated from the
C++ rebuild it rides on — `tip` (no feature), `uring_off` (feature, option off),
`async`:

| regime | uring_off | async | p99 |
|---|---:|---:|---:|
| harsh, 6 reps | 147,642 | 150,899 (**+2.2 %**, won 5/6 interleaved pairs) | flat |
| Tier-0-like, 4 reps | 222,757 | 217,716 (**−2.3 %**, 1.6 % spread) | 118.0 → 138.2 ms (**+17 %**) |
| resident, 6 reps | 233,787 | 226,824 (**−3.0 %**, 3.5 % spread) | 124.9 → 141.2 ms (**+13 %**) |

The feature alone is free (`uring_off` vs `tip`: −0.1 % resident), but the option costs
its setup on *every* seek and collects only on the misses — and at any residency the
production system actually sees, misses are the thin tail. **Rejected**, the
`unordered_write` shape again: best case +2 % in a regime ~2.5× harsher than Tier 0,
paying 2–3 % throughput and double-digit p99 at and near the operating point. The
whole branch is `bench_out/r9_async_rejected.patch`; the Cargo feature was reverted
with it, since a liburing build dependency buying nothing is itself a cost. Parallel
subtree prefetch (§5.7's −9.3 %) was *not* re-tested: it is a third mechanism for the
same overlap this round measured twice, and the crossover arithmetic binds it equally.

### 11.5 Stale interior bands, costed and closed (ranking item 7)

`count-depths` (new read-only bin) over `ref62_l119`: interiors are exactly 2^d records
at every depth 0–19 — a perfect complete tree, nothing parked anywhere — totalling
~104 MB raw against 4.18 GB of leaves, **2.4 % of the database**. Even the worst case
(delete every interior record) cannot move a working set by more than that, and the
live band is most of it. `DeleteRange`/column-family reclamation has nothing to
reclaim; item 7 closes without being built.

### 11.6 What this settles, and what it leaves

- **The §9.5 cold-segment mark (+20 %) still stands unclaimed.** This round's lesson
  sharpens it: at Tier 0 the miss tail is too thin for per-operation overlap to pay,
  so whatever claims that mark must cut *logical* reads or misses (fewer/warmer
  seeks), not parallelise physical ones. The band-warming idea (a fresh process pays
  L/2 per load where a long-lived one pays L/4, §8.1) is the remaining named candidate
  of that kind, worth a look only if restart transients matter operationally.
- **Slot-per-node band compression stays unbuilt, deliberately.** Its whole payoff is
  round 4's −8.7 % growth-phase trade *at the default configuration*, and §6.4 already
  records that bulk builds use a scaled-down `max_frontier_depth`, where the regression
  does not exist. High-risk surgery on the hottest structure for a configuration
  nothing uses fails round 8's honest arithmetic.
- **The residency arithmetic generalises.** Any read-side proposal now has to state
  which side of ~60 % residency it pays on; the instrument to check costs one hog and
  ten minutes, not a 95-minute rebuild.

### 11.7 Artifacts, and one trap that cost an hour

`bench_out/ab_r9_{threads,async,async_pair,async_resident,async_mild,t128_mild,probeA,probeB}.json`;
`bench_out/r9_disk_probe{A,B}.log`; `bench_out/r9_async_rejected.patch`;
`bench_out/bin/r9_{tip,uring_off,async}` and `r9_threads{128,256}.sh` (wrapper-script
variants); `tools/memhog.c`; `src/bin/count_depths.rs` (landed). `r9_tip` rebuilt from
HEAD byte-identical (`090dcdce…`) to `r8_base` — the build is deterministic, which
retroactively pinned every probe to the binary it claimed.

The trap: **setting `CXXFLAGS` in the environment silently drops the cpuid fix.**
`.cargo/config.toml` carries `-DROCKSDB_SCHED_GETCPU_PRESENT` via `[env]`, and cargo
does not apply that when the variable is already set — the config file even says so.
The first io-uring builds exported `CXXFLAGS=-I…` for liburing's headers, lost the
define, and resurrected §3.1.0a's `%rbx` clobber verbatim: SIGSEGV in
`ConcurrentArena::Repick` under 64-worker contention, in *both* variants including the
one that never touches a ring. Diagnosis was the §3.1.0a stack signature
(`this` = a small field offset); the fix is additive flags
(`CXXFLAGS="-DROCKSDB_SCHED_GETCPU_PRESENT -I…"`), and the
`concurrent_batch_writes_do_not_corrupt_the_memtable` gate test passed before any
number from those binaries was believed.

---

## 12. Round 10 (2026-08-30): rotating compaction — the shape is worth +20 %, maintaining it concurrently is not

An architecture question prompted this round: could sharding the keyspace across
independent LSMs, compacting one shard per idle slice in rotation, turn §8.2's decaying
compaction advantage into a sustained property? The mechanism was built in its cheapest
faithful form — sub-range `CompactRange` over 2^k slices of the leaf keyspace inside the
single database, cycled by a background thread — because that captures what rotation
does (per-slice freshness, bounded per-slice cost) without multiplying the memtable
budget by 2^k, and the verdict transfers: separate shard DBs rewrite the same compaction
bytes per byte of churn; only the L0 residue and the write-buffer bill differ, and both
point the wrong way.

### 12.1 The ceiling, finally measured at the production-like point

§8.2's +28.2 % was uncapped. Same pair (`ref62_l119` vs `ref62_compact`, same binary),
2 M inserts, 4 reps, under the 43 GiB hog (§11.1's Tier-0-like point): **+27.0 %**
(192,758 → 244,812), p99 −15 %, end runs 8 vs 3 (`ab_r10_ceiling_mild.json`). The prize
rotation chases is real at the residency production actually runs at.

### 12.2 The mechanism

`RocksStorage` now shares its `DB` in an `Arc`; `compaction_handle()` returns a
`CompactionHandle` whose `compact_leaf_slice(k, i)` compacts slice `i` of `2^k` equal
slices of the leaf keyspace, non-exclusive so RocksDB's own compactions keep running,
with the default bottommost-level skip so a cycle rewrites only bytes that arrived above
the bottom since the slice's last turn. `bench --rotate-compact k,gap_ms` drives it from
a background thread. Verified: identical roots on/off, cycle logs, and it does hold the
resident run count at 6–7 against the base's 9 over a 20 M-insert lifecycle.

### 12.3 The measurement: it loses everywhere, for a reason worth keeping

20 M inserts (~2.5 decay half-lives), 3 interleaved reps, `ref62_l119` copies, all 21
runs across both sessions root-identical at `9f71f294…` (`ab_r10_rotate.json`,
`ab_r10_rotate_mild.json`):

| arm | resident | p99 | runs | Tier-0-like | p99 | runs |
|---|---:|---:|---:|---:|---:|---:|
| base | 242,630 (8.5 %) | 155.2 | 9 | 175,565 (13.1 %) | 194.8 | 8 |
| rotate 6,100 | 218,995 (**−9.7 %**) | 217.5 | 7 | — | — | — |
| rotate 6,500 | 219,771 (**−9.4 %**, 0.3 % spread) | 205.8 | 6 | 169,930 (**−3.2 %**) | 206.4 | **13** |
| compacted start | 262,627 (+8.2 %) | 149.3 | 10 | 210,188 (**+19.7 %**, 1.0 % spread) | 160.6 | 10 |

Three facts settle it. Cadence is irrelevant (fast ≈ slow to 0.4 %): the cost is the
compaction *work*, not its scheduling. At the read-bound point the rotation thread's own
reads miss too — it fell behind churn and finished at **13 runs, worse than base's 8**,
so under exactly the pressure where the shape matters most, concurrent maintenance is
not merely expensive but counterproductive. And the compacted-*start* arm, which pays
nothing during the run, took +8.2 % resident and +19.7 % read-bound on the same days —
the state is valuable precisely when nobody is paying to maintain it.

### 12.4 The verdict

**Option-3-class designs (rotating compaction, sharded or not) are rejected.** Round 8
inferred "the schedule is the policy" from a decay curve; this round re-proves it with a
live concurrent implementation at two residency points. The compaction bytes needed to
sustain freshness are set by churn, not by how the work is sliced, and the foreground
pays for them whenever it is not idle. What survives, sharpened: **idle-window
compaction at the Tier-0-like point is worth ~+20 % of the following ~13 %-of-tree worth
of inserts** — the strongest, tightest-spread statement of the ops lever yet.
`compact-db` remains the tool that collects the win when the idle window exists.

> The mechanism (`CompactionHandle`, `--rotate-compact`) was first kept "as an
> instrument", then removed in the round's cleanup: a rejected policy should not live on
> as an option — the same culling every retired knob got in round 5 — and the
> measurement instruments that *are* worth keeping (`memhog`, `count-depths`, the
> sorted-run report, the census) don't include it. Commit `8d32d6f` holds the whole
> implementation; `git revert` of its removal resurrects it for a future schedule
> experiment in minutes.

### 12.5 Artifacts

`bench_out/ab_r10_{ceiling_mild,rotate,rotate_mild}.json`; binary `bench_out/bin/r10_rotate`
(the flag lives on in it); the implementation in commit `8d32d6f`, removed again in the
cleanup commit that follows the round record. Tests: all 202 pass both with the `Arc<DB>`
change and after its removal.

### 12.6 The architecture survey this round came from, recorded

Round 10 evaluated one of five alternatives from a first-principles pass over "could a
different architecture beat this one". The other four were not built; their reasoning
and gates are recorded here so the next person starts where this stopped.

The frame: after ten rounds the write path is near its floor (1.9 puts, ~165 staged
bytes per insert), hashing is 4.2 % of active CPU, and the one scale-growing cost is
**sibling-data I/O** — a Merkle update needs the untouched sibling, and this design
pays a seek across every sorted run plus an L/2 leaf scan per subtree load (§8.1). An
alternative wins only by deleting that obligation, reshaping its storage, or making it
irrelevant.

1. **Vector-commitment trees (Verkle-style).** The only design that *deletes* the
   sibling read: commitments update homomorphically (`C' = C + (v'−v)·Gᵢ`), so an
   insert touches ~3–4 path records and nothing else — no scan, no seek fan-out,
   trading I/O-latency-bound for CPU-bound on curve math. Categorical costs: different
   cryptographic assumptions (discrete log, not hash-based, not post-quantum), a
   different proof format for every verifier, total root incompatibility. Gate: only
   admissible if the proof system itself is negotiable. Not a variant of this crate; a
   successor to it.
2. **Single-sorted-run storage (B-tree/Bε-tree, or "subtree pages in a paged
   engine").** §8.2/§12.1 price the LSM shape tax at +27–28 %; a B-tree *is* the
   one-run shape permanently, and §9's page rejection was largely LSM machinery
   (merge-operand chains, the operator-less-open disqualifier) eating an intrinsic
   +20 % cold-segment advantage. One page per frontier subtree, updated in place over a
   WAL, plausibly holds the +20–30 % class sustained. Cost: write amplification
   (~8 KB page per ~165 logical bytes) — rational for a read-bound, write-cheap
   workload, but it is a storage-engine replacement, not a patch. Gates it must pass,
   from this document's own record: crash resumption (§3.3.8's gate), any-binary-opens
   operational simplicity (§9.4), and the §12.3 lesson that maintenance I/O competes
   with foreground reads.
3. **Sharded LSM with rotating compaction.** Built in its cheapest faithful form and
   rejected — this round. The verdict transfers to true multi-DB sharding: compaction
   bytes per byte of churn are invariant to slicing, and 2^k memtables multiply the
   write-buffer bill.
4. **Two-tier Merkle (fresh tree over recent writes + base tree, `root = H(fresh,
   base)`).** Converts random sibling reads into periodic sequential re-merkelization —
   the LSM idea applied at the Merkle layer. Sound, but it changes the root formula and
   proof shape, and the fold is a stall to schedule; dead on arrival wherever the
   canonical root is a compatibility requirement.
5. **Hardware/residency.** The boring one that dominates: the same binary runs +140 %
   faster fully resident than at Tier 0 (233 K vs 97 K entries/s), more than any
   software candidate above. RAM ≥ working set — bought as memory or as sharding across
   machines — outperforms every architecture change at near-zero engineering risk, and
   options 1–2 shrink as residency rises (they improve the miss path, and the miss
   tail thins) while this one attacks residency itself. Price this first.

---

## 13. Round 11 (2026-08-31): the tidy — no behavior change, measured to prove it

Not a perf round. The goal was readability: dead code out, scaffolding comments out,
idiomatic forms in, and the design written down in one place (DESIGN.md). Method: a
survey pass over every module first, then staged edits gated per the usual rules —
nothing on-disk, nothing in the hash formulas, nothing in the RocksDB options or census
semantics, every measurement-bearing doc comment preserved or compressed with its
numbers intact. Net −1,010 lines across 23 files.

**Deleted, all verified caller-free before removal:** the `boundary_started`
out-parameter (write-only state threaded through eight functions of the F = 0 recursion;
its removal collapsed the three duplicated boundary-batch arms in
`batch_upsert_at_interior` onto the `handle_divergent_entries` wrapper),
`RocksTransRelMPT::flush`, `RocksStorage::write_nodes` (orphaned by the §5.6 record
change; its unique measurement moved to `write_frontier_nodes`' doc),
`InteriorNode::child_hash`, the trait method `new_with_path` (never trait-dispatched,
and its default silently ignored the path), `batch_upsert_iter`, `BorrowDecode for
Prefix`, `DurableBatchMPT::{new_in_memory, set_safety_mode}`, `SqliteStore::new`, the
SQLite `idx_prefix` index (§3.4's "pure write overhead — drop it"; existing databases
keep theirs), the 27-line unreachable cache-overflow warning, the broken `dump-sqlite`
binary (panicked on the post-§5.6 metadata row; inserted 1 of its announced 8 entries),
bincode's unused `serde` feature, the commented-out `optimize_universal_style_compaction`
line (BENCHMARK-BASELINE.md's configuration-D note now records the values directly), and
three tests (two assertion-free, one a strict subset). Three near-identical exact-match/boundary code paths were deduplicated;
visibility tightened where nothing outside a module looked.

**Kept deliberately:** the `Display` impls the survey had called dead (a production
error path and the sqlite debug log use them — rule 10 caught it), the slice-based
`batch_ops` redesign and join threshold (§3.2.3/§3.2.5 remain measurement-gated), the
sqlite `pre_advise` shape (§3.4's verdict), and every recorded constant.

**The gate** (`bench_out/ab_tidy_neutrality.json`): pre- vs post-tidy binaries,
`ref62_l119`, 2 M entries fixed work, 3 interleaved reps, cold cache — **233,102 vs
233,098 entries/s (−0.0 %)**, census identical (1.911 puts/insert, 28.6 leaves
read/insert), RSS identical (3.85 GB), root hash `dc96b997…` identical across all six
runs, matching §8.2's recorded root for this workload. Suite: 199 tests, clippy
`--all-targets --all-features -D warnings` clean, fmt clean.

Alongside the tidy: DESIGN.md now describes the final design in present tense (this
document stays the history); BENCHMARK-BASELINE.md §5.6 records the Tier-0 rebuild to
1.81 B leaves at frontier 24 with the session ledger reconstructed from the RocksDB
logs; README's design narrative was replaced by a summary plus pointers, and its
tooling section now documents `compact-db`, `count-depths`, `ab.py` and `memhog`.

## 14. Round 12 (2026-08-31): the restructure — the file tree now mirrors DESIGN.md

Not a perf round. Round 11 removed what was dead; this round reorganized what remains
so each file tells one design chapter's story, on the maintainer's brief that types,
layout, everything could change as long as the result is simpler, easier to follow and
shorter. Fifteen staged changes (S1–S12, S14–S16 of the reviewed plan; the census
macro S13 and the SQLite-family deletion S17 were declined by the maintainer, the
`get_bit` rename skipped as churn), each leaving the suite green; the two monoliths
are gone as files:

- **`rocks_frontier/mod.rs` 3,270 → 897 across seven files**, one per DESIGN.md
  chapter: `levels.rs` (the positional structures + `position_prefix`/`position_index`
  free fns + `WorkerSlots<T>`, deduplicating the per-rayon-worker slot idiom),
  `read_path.rs` (§6), `store_regime.rs` (the F = 0 recursion), `band_regime.rs`
  (the F ≥ 1 machine, with its lock rule promoted to the module doc and
  `combine_children` deduplicating the verbatim child-hash match), `advance.rs`
  (completeness checks + level persistence, `update_complete_interior_depth`
  decomposed into `advance_one_level` + `recompute_ancestors_above`), `recovery.rs`
  (§9). Cross-file items are `pub(super)`; tests compile against the same names.
- **`storage/rocks.rs` 2,502 → `rocks/{mod 818, codec 262, tests 1,428}`** — the
  inline test module was over half the file; `codec.rs`'s header states that the
  record formats are the one place tree semantics deliberately cross the storage
  boundary.
- **Names caught up with the design**: `RocksTransRelMPT` → `RocksFrontierMPT`,
  `batch_upsert_optimized` → `apply_batch` (`DurableBatchMPT`'s same-named,
  different function → `apply_chunk`), `RocksTransaction` → `RocksMetadataBatch`
  (its own doc had to un-teach "transaction"), `should_persist_depth` →
  `is_frontier_level`, census `node_writes` → `frontier_level_writes` (printed
  stderr label byte-identical, so `ab.py` parsing is unchanged).
- **Invariants moved into types**: `Staging<'a> { Inherited, Unstaged }` replaces the
  F = 0 recursion's `Option<&mut RocksWriteBatch>` — the one-open-batch-per-path rule
  that lived in a 17-line prose block is now the match shape at all six batch-open
  sites, and `batch_upsert_at_leaf`'s four persistence arms are named methods
  carrying their own records. `Prefix` documents both invariants, gains a
  debug-checked `Prefix::new`, and absorbs the bit math storage had leaked
  (`child(Side)`, `successor`, now unit-tested directly). The duplicated frontier
  depth is documented + debug_asserted at batch boundaries, NOT unified — the desync
  during an advance is load-bearing and the fault-injection tests need the atomic to
  hold unrepresentable values.
- **Deleted or demoted**: `batch_read_nodes` (the dead multi_get path; its tests now
  pin `get_node`), the write-only census counters `entries_upserted`/
  `batches_upserted`, `build_subtree_memory_only` (== `build_subtree_retained` with
  `keep_to = u16::MAX`; release assert kept), the `estimate_leaf_count` wrapper,
  `validate_level`/`expected_nodes_for_depth` (inlined), dead re-exports, and the
  test-only pub surface (`#[cfg(test)] pub(crate)`, the `get_raw`/`put_raw` pattern).
  The archaeology comment blocks compressed to invariants + pointers, with their
  measurements first written into §4's closed item 2 (they were nowhere in this file).
- **SQLite hygiene under the recorded keep verdict**: the no-op `upsert` override
  gone, `new()` uses a `TempDir` guard (every test run used to leak WAL-mode SQLite
  files into /tmp), the redundant per-chunk re-sort gone, field `cache` → `store`,
  phantom `Arc`s removed from five fields (`db` keeps its Arc — the tests clone that
  handle, which the plan's "six never-cloned fields" claim missed). `test-mermaid`
  is `examples/mermaid.rs` now; `compact-db` reads the compaction constants from the
  storage layer instead of restating them; `bench`'s frontier-depth default is
  single-sourced from `RocksFrontierConfig::DEFAULT_MAX_FRONTIER_DEPTH`.

Net across the round: **32 files, +5,452/−5,195 (≈ +257 lines)** — the payoff is
distribution, not deletion: no production file over ~900 lines, and the additions are
module headers, invariant docs, and +6 new `Prefix` unit tests (suite 199 → 205).

**The gate**, three measurements, all census- and root-identical (1.911 puts/insert,
28.6 leaves read/insert, root `dc96b997…` matching §8.2 and §13):

| point | protocol | base → tip |
|---|---|---|
| resident | `ref62_l119`, 2 M fixed work, 5 interleaved reps, cold cache | 232,754 → 235,050/s (**+1.0 %**, spreads 1.9 %/0.6 %) |
| Tier-0-like | same + 43 GiB `memhog` (§11's instrument) | 219,495 → 218,863/s (**−0.3 %**, spreads 20.8 %/1.8 %) |
| F = 0 growth | 3 M from empty, 3 interleaved pairs | 678,745 → 683,123/s median (**+0.6 %**, wash; roots `02552f76…` identical) |

Raw sessions: `bench_out/ab_r12_neutrality_resident.json`,
`ab_r12_neutrality_tier0like.json`; binaries `bench_out/bin/r12_base` (= round-11 tip,
sha `176772e0…`), `bin/r12_tip`.

**Ops note:** `my_100m_db` (the §5.6 Tier-0 rebuild, 1.81 B leaves, 132 GB) was
**deleted this round on the maintainer's instruction** — the disk hit 100 % mid-gate
(the first A/B attempt died on ENOSPC; `target/debug`, 5 GB of debug-profile test
artifacts, was cleared first). Tier 0 must be rebuilt from the §5.6 recipe before the
next at-scale session; `ref62_l119`/`ref62_compact`/`refdb_childhash` remain on disk.

## 15. Round 13 (2026-08-31): the simplification round — one write regime, format v3, 40 % of the rocks code deleted

The goal changed. Twelve rounds optimized this implementation; the maintainer's brief for
round 13 was the inverse trade: *make the rocks implementation much shorter, with
performance only minorly impacted — major rewrites welcome.* Eight candidate rewrites were
drafted and adversarially verified against this record first (the vetted proposals, with
per-proposal corrected line and perf estimates, are in `SIMPLIFY-PROPOSALS.md`); the
maintainer approved four, and all four landed behind the standing neutrality gate.

### 15.1 What landed, in order

- **W1 — one deep code path** (`b477eef`). The SQLite family (`DurableBatchMPT`,
  `SqliteStore`) and the in-memory batch pair (`BatchMPT`, `batch_ops`, the `NodeStore`
  trait) are deleted; `SimpleMPT` is the sole oracle. Every oracle site used only
  `batch_upsert` + `get_root_hash` on a fresh instance, so the swap is
  detection-equivalent — and `SimpleMPT` is the *more* independent oracle, being the only
  implementation that never calls `sorted_unique_entries`. The census drops from eleven
  counters to the six `tools/ab.py` parses; `bench` is rocks-only (it still accepts
  `-b rocks`, and only that, because ab.py and the recorded protocols pass it); `rusqlite`
  leaves the build. Revival is one checkout: git tag `sqlite-baseline-final`. The recorded
  write-amplification baseline survives as text in §2.3.

- **W2 — one write regime** (`5d4baaf`), the biggest cut. The `F == 0` prefix-addressed
  regime — `store_regime.rs` (711 lines), the `DashMap` store, `interior_counts`,
  `evictable` + `prune_below_frontier`, the store-side halves of `advance.rs` and
  `read_path.rs`, and store-driven full recovery — is deleted whole. A small tree is one
  band subtree at position (0, 0): `apply_batch`'s F == 0 arm stages into the caller's
  worker slot and calls `upsert_band(0, 0, …)`, whose miss→merge→build machinery already
  handled empty scans, single leaves and pass-through shapes. At `F >= 1` the batch path
  is call-graph-identical to round 12 — the deleted regime executed at **no recorded
  operating point** (§4's closed item 2 counted zero batchless splits at F ≥ 1), which is
  what made this deletion a measurement-free bet at the gated points and a gated bet only
  at the growth point. `enumerate_nodes` is rebuilt from the leaf records by a pure
  emitter (the compressed tree over a sorted leaf set is unique). Two behavioural notes,
  both accepted: the first advance out of F == 0 leaves the band cold at the
  `depth_always_keep` boundary (one extra scan per frontier subtree, once), and the F == 0
  phase runs single-threaded (~tens of ms, once per database). The 674-shape reopen
  oracle, `every_batch_shape` — now including the `(3,3)/(4,4)/(5,5)` shapes that were
  flaky under the old boundary arm — and cross-impl parity all pass unchanged.

- **W3 — format v3** (`1836cbd`). The only records read or written are the compact leaf
  (tag 2), the compact frontier row (tag 3) and the three metadata keys. The bincode-era
  decode arms, `Node::serialize`/`deserialize` (bincode leaves the crate), the sampling
  leaf-count estimator (`rand` moves to dev-dependencies) and the encode fallback are
  deleted; `put_leaf` stages `tag ‖ value` directly — canonical by construction, and the
  two SHA-256 the old canonical check spent per staged leaf are gone with it. Anything
  else on disk is refused loudly at open, with the escape hatch named (git tag
  `format-v2-final`): a frontier level the metadata names but the rows do not deliver, a
  frontier without a persisted leaf count (pre-v3 by definition), interior rows with no
  metadata naming them (the metadata-stripped guard — without it, a stripped F ≥ 1
  database would silently open frontierless and turn every batch into a whole-database
  merge), and tag-0/1 records on any read path. **No migration**: the round's read-only
  census showed all three surviving reference databases 100 % compact (their only legacy
  bytes are one dead 163 B depth-0 root row each, which nothing reads), so they open
  byte-identical — which is also what kept this round's own A/B gate runnable.

- **W4 — doc-mass compression** (`87d0ec3`). Sweep tables and mechanism essays that
  duplicate this file became one-line citations carrying the decisive numbers; the two
  records that existed only in code moved to §15.2; the correctness invariants (the band
  lock rule, staged-after-recursion, proven emptiness, do-not-unify, the crash-ordering
  contracts, the `Relaxed` justifications) are kept verbatim.

**The ledger.** The rocks implementation (`rocks_frontier/` + `storage/rocks/`,
non-test): **4,403 → 2,663 lines (−40 %)**. Library production code crate-wide:
6,720 → 3,629 (−46 %). Tests: 7,455 → 5,031 lines, suite 205 → 162, with the black-box
safety net (reopen oracles, format pins, oracle parity, counting-allocator, mutation CI)
intact and new pins added for the round's own hazards (F == 0-through-band shapes,
cold-root fault-in, format refusals, sequence-number no-rewrite pins).

**Not executed**, by the maintainer's selection: the storage-layer fold (proposal P5
stage 1, ~−110–150 lines, neutral) and frontier-is-a-cache (P4, ~−450–480 lines, the one
positive-perf-sign candidate, gated on a minutes-scale cold reopen at 1.8 B leaves and a
mandatory legacy-row tombstone). Both remain open options; their verified estimates are
in `SIMPLIFY-PROPOSALS.md`.

### 15.2 Records promoted from code comments

These existed only in doc comments; W4 moved them here so the code can cite one line.

**The `JOIN_THRESHOLD` sweep** (on the constant in `rocks_frontier/mod.rs`). Benchmarked
on identical copies of a 23.09M-leaf database (frontier depth 19, 44 leaves per frontier
node), two 25-second runs each:

| threshold | entries/s (mean of 2) |
|---|---|
| 2  | 137,274 |
| 4  | 134,913 |
| 8  | **138,440** |
| 16 | 134,706 |
| 32 | 133,472 |
| 64 | 126,391 |

Everything at or below 32 is a plateau within run-to-run noise; the previous value of 64
was the only outlier, costing ~9.5 %, with much wider spread (129.7K/123.1K against
138.5K/138.4K at 8) — what starving the read path of concurrency looks like.

**The depth-23 single-transaction persist** (on `write_level_in_chunks`). Writing a whole
frontier level through the old `OptimisticTransactionDB` transaction held 8.4 M puts — a
1.14 GB `Vec<(Prefix, Node)>`, a ~1.1 GB encoded copy — and the run that advanced through
depths 22 and 23 took 69.8 s for that one batch against a 2.3 s median, ~5.4 µs per node.
That is why levels are streamed as plain chunked `WriteBatch`es.

### 15.3 Corrections and strikes

- **§5.7's −5.7 % is mis-baselined for post-round-4 code.** Both the single-atomic-batch
  measurement (−5.7 %, §5.7) and per-worker aggregation (+16.2 %, §6.2) were taken
  against the *round-3 per-subtree-commit* baseline — §6.2 calls them "the two measured
  ends" — and §5.7's number is additionally from the accidental update workload
  (§5 preamble). Adopting one atomic batch from today's HEAD therefore abandons the
  +16.2 % and re-buys the −5.7 %: composed, ≈ **−16 to −19 % resident**, before the
  batch-merge overhead a real implementation would add. Recorded so no future round
  prices the atomicity upgrade off the wrong number. (A note now sits at §5.7 itself.)

- **Band deletion is struck.** `keep_below_frontier = 0` was never swept, but the record
  brackets it: keep = 1 measured −11.7 % (§5.4, on the pre-merge architecture) and the
  no-band read shape — merging at the frontier child — measured −7.9 % with 37.3 against
  22.8 leaves read/insert on today's architecture (§5.5), at the resident point where
  reads are page-cache hits and the loss is pure CPU. At Tier 0 the long-lived-process
  extrapolation is roughly −10 to −20 % (the scan term of §8.1's read bill doubles,
  L/4 → L/2). Structurally, keep = 0 also re-adds a band-lite: the F ≥ 1 advance needs
  per-position fullness and child hashes at F+1 from somewhere. Judged well outside the
  round's low-single-digit budget; do not re-propose without a run that beats this
  bracket.

### 15.4 The gate

The standing refactor-neutrality protocol (rounds 5/11/12): `tools/ab.py`, 2 M fixed
work, 5 interleaved reps, `--link-copy` scratches of `ref62_l119`, cold cache, base =
`bench_out/bin/r12_tip` (the round-12 tip binary), tip = `bin/r13_tip`; plus the 3 M
from-empty arm — the one point whose executed code the round changes.

| point | protocol | base → tip |
|---|---|---|
| resident | `ref62_l119`, 2 M fixed work, 5 interleaved reps, cold cache | 234,646 → 235,102/s (**+0.2 %**, spreads 3.9 %/0.9 %) |
| Tier-0-like | same + 43 GiB `memhog` (BENCHMARK-BASELINE §5.5) | 218,705 → 218,046/s (**−0.3 %**, spreads 23.1 %/8.0 %) |
| F = 0 growth | 3 M from empty, 3 interleaved pairs, default config | 682,787 → 709,959/s median (**+4.0 %**; roots `02552f76…` identical) |

Census identical at both ref62 points (1.911 puts/insert = 1.000 leaf + 0.911 interior,
28.6 leaves read/insert) and root-identical everywhere (`dc96b997…` at ref62, matching
§8.2/§14). At the growth point the predicted batch-1 census delta (the store regime's
first-batch interior puts are gone) lands below the printed 3-decimal precision — every
parsed metric is identical (1.336 puts/insert, 100.3 bytes staged/insert, 0.006 write
batches/insert). The growth uplift is real and has a mechanism: `put_leaf` stages
`tag ‖ value` without constructing a `LeafNode`, deleting two SHA-256 per staged leaf,
and hashing is a larger share of a from-empty build than of steady state. At the gated
points it is inside noise, as the neutrality bet predicted.

One instrument fix mid-gate: `ab.py` passed `-b rocks` to every binary, which W1's bench
no longer accepted — the first tip arm failed all runs on usage text. `bench` now accepts
`-b rocks` (and only that, with the deletion explained in the refusal), and `ab.py` no
longer passes it; the recorded protocols keep working against both binary generations.

### 15.5 Artifacts

Raw sessions: `bench_out/ab_r13_neutrality_resident.json`,
`ab_r13_neutrality_tier0like.json`, `bench_out/r13_growth/` (per-run logs);
binaries `bench_out/bin/r12_tip` (base), `bin/r13_tip`; the growth-arm runner
`tools/r13_growth_arm.sh`; the vetted proposal set `SIMPLIFY-PROPOSALS.md`; git tags
`sqlite-baseline-final` (last build with the deleted backends) and `format-v2-final`
(last build that reads pre-v3 records).


## 16. Round 14 (2026-08-31): the interface round — honest types, one derivation, no false Results

Round 13 asked for *shorter*. The brief for round 14 was different: is the **interface**
honest, is there functionality nobody needs, and does the code read like Rust? The
candidate set (`TIDY-PROPOSALS.md`, written against the code and the record before any
edit) was approved whole and landed in two waves behind the standing neutrality gate.

The organising finding was that the crate asked a reader to believe several things that
were not true. Each item below is one of them.

### 16.1 What landed

**W1 — vestigial surface, the trait, and the false `Result`s** (`3e39574`).

- **`ROOT_KEY` was a fossil.** `apply_batch` wrote `tx.set_root(Prefix::root())` — a
  constant — at the end of every batch, and nothing outside the storage tests had read it
  back since the root stopped being able to sit anywhere but the root prefix. The key,
  `load_root` and `set_root` are deleted; the metadata batch is now the leaf count and the
  frontier depth. Pinned by `a_merge_does_not_rewrite_untouched_leaves`, whose
  sequence-number delta drops 4 → 3.
- **Seven infallible functions returned `RocksResult<()>`** (`put_leaf`,
  `put_frontier_node`, the metadata setters, `batch_write_leaves`, `rollback`). Three
  hot-path sites wrote `let _ = batch.put_leaf(..)` — a permanent silencer had the call
  ever acquired a failure mode — and `apply_batch` spent three `.expect()`s on writes that
  cannot fail, diluting the one that guards a real `db.write`. They return `()`. The codec
  returns typed errors; `impl From<String> for RocksStorageError`, which mapped *any*
  string error to `Codec`, is gone.
- **`MerklePatriciaTree` kept only what the parity suite needs**: `batch_upsert`,
  `enumerate_nodes`, `get_root_hash`, `get_leaf_value`. Deleted: `fn new()` (for
  `RocksFrontierMPT` it opened a temporary database and `expect`ed), the `upsert` default
  (shadowed by `SimpleMPT`'s inherent one), and the three `Option` telemetry hooks whose
  only `None`s were the trait's own defaults — `bench` now holds a `&RocksFrontierMPT` and
  prints `leaf_count()`/`frontier_depth()`/`sorted_runs()` unconditionally. Construction
  moved to a test-side `TestTree`.
- **`from_storage` accepted frontier depths that abort.** The match arm was `1..=256` and
  depth F allocates `1 << F` slots, so a corrupt `COMPLETE_DEPTH_KEY` of 200 reached
  `1usize << 200`. Anything above `MAX_SUPPORTED_FRONTIER_DEPTH = 48` is now refused by
  name.
- **Layering.** The key layout (34, `256u16`, the scan range) moved into `codec.rs`, the
  only file that should know it; the compaction constants went private behind
  `RocksStorage::open_for_compaction`, so `compact-db` no longer assembles
  `rocksdb::Options`; the nine `#[cfg(test)]` members moved to
  `storage/rocks/test_support.rs`.
- **Dead surface**: three unused `Display` impls, `Prefix::get_bit` (the asserting twin of
  the silent `Key::get_bit` the hot paths call — production-dead and a genuine trap),
  `is_empty` (a full leaf scan behind an O(1) name, no caller), `rollback` (a no-op
  returning `Ok`), `FrontierLevel::depth()`, and the two coordinate forwarders.
- **Census**: counters bump themselves (`CENSUS[Metric::LeafPuts].bump()`), and
  `snapshot`/`reset`/`since` are loops over `Metric::ALL` instead of six restatements of
  the field list. The printed report — `ab.py`'s contract — is unchanged.

**W2 — the types** (`6ae5d5e`).

- **`Key`, `Value` and `Digest`** are distinct `#[repr(transparent)]` newtypes; the
  crate-wide `type Hash = [u8; 32]` and its `HashExt` extension trait are gone. `(Hash,
  Hash)` was sometimes an entry and sometimes a pair of child hashes; it is now
  `Entry = (Key, Value)`. Same bytes, same codegen — the root hashes below are the proof.
- **`src/hash.rs`** holds the merkle algebra as two free functions. The rocks path called
  `InteriorNode::calculate_hash` ten times without ever constructing an `InteriorNode`.
- **`Position`** (depth + index, with `child(Side)`, `prefix()`, `of()`) replaces the
  `(depth, index)` pair threaded through the whole below-frontier path; `index * 2 + 1` is
  written once. **`AtomicHash`** replaces the flat `Vec<AtomicU64>` plus
  `load/store_hash_words` in `TopLevels` and `FrontierLevel` — identical layout, no
  `* 4` / `* 8` / `base + 4` at the call sites. **`split_at_bit`** names the sorted-batch
  split five descents shared.
- **The tested tree is now the tree that is configured.** `RocksFrontierMPT::open(path,
  config)` / `::temporary(config)` take the configuration explicitly, and the `cfg(test)`
  switch that silently gave the in-crate suite a 5-deep frontier while production ran at
  24 is deleted — the 47 test sites pass `RocksFrontierConfig::test_config()` by name.

**The ledger.** Production (`src/**`, excluding `tests.rs`/`test_support.rs`): 4,268 →
4,021 after W1 → **4,119** after W2, i.e. −247 then +98. The rise is deliberate and is
the round's shape: W2 *adds* type definitions (three newtypes, `Position`, `AtomicHash`,
`src/hash.rs`) to delete ambiguity, not lines. Tests 5,031 → 5,298, almost all of it the
newtype churn and the now-explicit test configuration; the suite is 162 tests, unchanged
in what it covers plus one new pin (`every_metric_has_its_own_slot`).

### 16.2 Recovery collapsed onto the advance path

The one substantive rewrite. `recovery.rs` rebuilt the tree top at open with a bottom-up
`BTreeMap` sibling-pairing pass; but above a complete level the trie is perfect, so a
parent's hash is arithmetic over its children's — which `advance.rs::recompute_ancestors_
above` already computes on every advance. Open now streams the persisted frontier level
into the dense array (`for_each_frontier_row`) and calls that same function.

Deleted with it: the parent map, `ParentChildren`, `is_right_child`, the level loop, and
two `bool` returns that had collapsed six distinct failure modes into one generic
"missing or incomplete" message. Also deleted, and this is where the measurement went: a
whole-level `Vec<(Prefix, Node)>`, a second filtered `Vec<(Prefix, InteriorNode)>`, a sort
of 2^F keys RocksDB had already returned in order, and one discarded SHA-256 per row
(`decode_node` rebuilt a merkle hash the frontier level then threw away).

The safety the parent pass provided is not lost but strengthened: a level of 2^F rows
could in principle alias two positions, so the streaming loader asserts each row's key is
exactly `Position::prefix()` and that positions arrive strictly increasing, and the
refusals now name which of the four things was wrong.

**Measured at open** (`ref62_l119`, five interleaved reps per arm, cold cache), from the
same sessions as the gate below:

| point | base init | tip init | Δ | peak RSS |
|---|---:|---:|---:|---|
| resident | 4.227 s | 3.907 s | **−7.6 %** | 3.87 → 3.62 GB (−6.5 %) |
| Tier-0-like | 4.281 s | 3.917 s | **−8.5 %** | 3.87 → 3.65 GB (−5.7 %) |

Spreads are tight enough to read directly (base 4.21–4.26, tip 3.90–4.04). Both effects
scale with 2^F, so the production frontier is where they are worth having.

### 16.3 The hot leaf loop

`for_each_leaf_record` — 22–37 leaves read per insert at the recorded operating points —
carried five stop conditions per record: `key.len() != 34`, `!is_leaf_key`,
`key >= end_key`, `length != 256`, `!prefix.prefix_of(..)`. Given the seek at
`256u16 ‖ start` and the `iterate_upper_bound` at `256u16 ‖ successor` (or
`NODE_KEY_RANGE_END`), every key RocksDB can return in range is a 34-byte leaf key under
the prefix, and the metadata keys (`_`, 0x5F) sort above the bound. Four were unreachable.
The length check stays as the one cheap guard against a corrupt row; the rest are
`debug_assert!`s, with the argument stated once where the bound is set.

### 16.4 The gate

The standing refactor-neutrality protocol (rounds 5/11/12/13): `tools/ab.py`, 2 M fixed
work, 5 interleaved reps, `--link-copy` scratches of `ref62_l119`, cold cache, base =
`bench_out/bin/r13_tip`, tip = `bin/r14_tip`; plus the 3 M from-empty arm.

| point | protocol | base → tip |
|---|---|---|
| resident | `ref62_l119`, 2 M fixed work, 5 interleaved reps, cold cache | 232,389 → 233,144/s (**+0.3 %**, spreads 2.1 %/2.7 %) |
| Tier-0-like | same + 43 GiB `memhog` | 212,367 → 216,799/s (**+2.1 %**, spreads 11.7 %/12.9 %) |
| F = 0 growth | 3 M from empty, 3 interleaved pairs, default config | 707,908 → 713,625/s median (**+0.8 %**) |

**Every parsed census metric is identical at every point** — 1.911 puts/insert
(1.000 leaf + 0.911 interior) and 28.6 leaves read/insert at both ref62 points; 1.336
puts/insert, 100.3 bytes staged/insert, 0.006 write batches/insert, 0.081 subtree
loads/insert from empty. **Root hashes are identical**: `dc96b997078cd623…` at ref62
(matching §8.2, §14 and §15) and `02552f766c82907e…` from empty (matching §15.4). That
identity is the round's real correctness statement: the key-space newtypes and the
`hash::` split changed no byte fed to SHA-256, and the collapsed recovery path rebuilds
the same tree the sibling-pairing pass did.

A pre-gate smoke check compared the two binaries' whole stderr reports on 200 K
from-empty inserts: identical except throughput and latency, which is the `ab.py`
contract holding.

### 16.5 Artifacts

Raw sessions: `bench_out/ab_r14_neutrality_resident.json`,
`ab_r14_neutrality_tier0like.json`, `bench_out/r14_growth/` (per-run logs); binaries
`bench_out/bin/r13_tip` (base), `bin/r14_tip`; the growth-arm runner
`tools/r14_growth_arm.sh`; the candidate set and its disposition in
`TIDY-PROPOSALS.md`.

## 17. Round 15 (2026-08-31): the ablation round — gates that cannot fire, fields that are derived, a counter that was not counting

Round 13 asked for *shorter*, round 14 for an *honest interface*. Round 15 asked a narrower
question: **which previously introduced features can be removed, condensed or ablated with
little loss to performance?** The candidate set (`ABLATION-PROPOSALS.md`, written against the
code and the record before any edit) was approved whole and landed in four waves behind the
standing neutrality gate.

The finding that organises the round: below the two open proposals (P4 frontier-is-a-cache,
P5 storage fold) there is no large *logic* fat left. What is left is features **this record
had already resolved** — round 5's knob table retired two knobs as "fixed" and "derived", and
the fields and branches behind them stayed — plus surface only tests execute, plus stale
references from rounds 13–14's own deletions. Measuring the starting point in code rather than
lines made that legible: 3,490 non-test lines were 2,101 code, 1,074 comment and 315 blank.

### 17.1 What landed

**W1 — the leftovers** (B1–B9). Seven doc references to items rounds 13–14 deleted
(`build_subtree_retained`, `batch_upsert_at_interior`, `evictable`, `interior_counts`,
`calculate_hash` ×3 — `hash.rs` replaced the last three in round 14's W2).
`Prefix::parent`, production-dead since recovery stopped pairing siblings. `Prefix::new`,
added last round as "the paved path" with no production caller, now has one: `Position::prefix()`
— the crate's one production prefix factory — routes through it, so its two invariants are
actually debug-checked on the prefixes the tree builds. `RocksStorage::start_batch`, a forwarder
that never touched `self` (`RocksWriteBatch` now just derives `Default`). `Census::new`'s six
restatements of `Counter::new()` → one inline-const array. `for_each_frontier_row`'s
unreachable length break → a `debug_assert!`, the same argument §16.3 made for the leaf loop.
`leaves_per_frontier_node`, eight lines and a doc serving one `info!` line, inlined into it.
`Value::ZERO`/`Digest::ZERO` (macro-generated, no callers) and two needlessly `pub(super)`
helpers. And ~35 lines of `debug!` tracing across 10 sites in `SimpleMPT`, whose only runs are
on trees of a few thousand leaves under test — `insert_node` existed only to log, so it went too.

**W2 — the ablations** (A1–A3), each covered in its own section below.

**W3 — the prefix invariant** (C1, C2), §17.4.

**W4 — the census correction** (E), §17.5.

**The ledger.** Production (`src/**`, excluding `tests.rs`/`test_support.rs`, including bins):
4,119 → **4,032**. Non-test library: 3,490 → **3,403**, of which code 2,101 → **2,004** (−97)
and comment 1,074 → **1,093** (+19). The comment count *rising* while code falls is the round's
shape and is deliberate: three of the deletions are arguments about why a feature cannot matter,
and those arguments have to survive the code they justify or the next round re-adds it. Tests
5,298 → 5,255; the suite is 162 tests, unchanged in count (one deleted, one merged, one added).

### 17.2 The retention floor and the frontier cap are one number

`RocksFrontierConfig` carried four fields. `depth_always_keep` was set by
`with_max_frontier_depth` to `max_frontier_depth - 1` and then read through
`depth_always_keep.min(max_frontier_depth - 1)` in `eviction_depth` — a tautology for **every
configuration the constructor can produce**. Round 5 had already recorded the resolution
("*derived: `max_frontier_depth − 1`*"); only the field survived it. The four in-crate test
literals were the only states where the `min` did anything, and one test existed solely to pin
that impossible state; it now pins the derivation instead, at a scaled-down cap and at the
default.

The behaviour is untouched and still worth what it was measured at (+9.7 % over a floor of 8 and
+11.0 % over 0, at 3M from empty; §5.4), and a floor above `max_frontier_depth - 1` — the state
that measured 6.58 GB peak RSS against 4.20 GB at cap 20 — is now unrepresentable rather than
capped after the fact.

`MAX_SUPPORTED_FRONTIER_DEPTH` also became a real limit rather than an open-time one. There were
three: 48 (refused at open), 64 (`BandSlots::full_counts`, panics above), and an
`advance_frontier` break at 256 that could not be reached because the completeness check refuses
at `max_frontier_depth` first. `with_max_frontier_depth(100)` used to build a config that opened
fine and aborted later; it now refuses at construction, the band's own structural limit is named
where it is derived (`MAX_BAND_DEPTH`, from `DEPTH_SHIFT`), and the 256 break is gone. Two limits
remain, each with a distinct reason.

### 17.3 The advance gate that could not fire

`advance_frontier` had two gates. The structural one — an interior node exists at every one of
the `2^(F+1)` positions, read as an O(1) full-slot count. And a leaf-count gate: refuse unless
`current_depth <= ceil(log2(leaves)) - log_leaves_per_frontier`.

The second cannot fire at the only value the crate ships. It is reached only *after* the
structural check passes, and a full slot at every position at depth `next_depth` means both
children of each are non-empty, so at least `2^(next_depth + 1)` leaves exist, so
`ceil(log2(leaves)) >= next_depth + 1 = current_depth + 2` — while breaking required
`ceil(log2(leaves)) <= current_depth`. Two levels of slack, against a count that a crash can
under-report by at most one batch.

This is not a new derivation. §5's knob table records `log_leaves_per_frontier` as "*fixed at 1;
the gate it feeds never binds at depths that matter*", and BENCHMARK-BASELINE.md §5 Gate A works
out `N > 2^F` and calls it "never the binding constraint". Round 5 retired the knob; the field,
the branch and its `next_power_of_two().ilog2()` note stayed. All three are deleted, and the
test that demonstrated the gate at `log_leaves_per_frontier = 3` — a value nothing can construct
— is replaced by one that pins what the structural limit alone produces for a tree of exactly
known shape. **Measured consequence: none.** Frontier depth, leaf count and peak RSS are
identical at every gate point (F = 16 at 3M from empty, 24 at both ref62 points).

### 17.4 `InteriorNode`'s child hashes, and where the prefix invariant is established

**The child hashes** (A3). `InteriorNode` stored `left_hash` and `right_hash` under a 15-line
essay explaining that caching a child's hash is worth about half of all read traffic on the hot
path. The essay is true; the fields are not where it applies. Their only readers in the whole
tree were two asserts in `storage/rocks/tests.rs`: the rocks path never constructs an
`InteriorNode` outside `enumerate_nodes`, and `SimpleMPT`'s descent re-reads each child's own
`merkle_hash()` from its store. The cache that earns the essay is `FrontierLevel` and
`BandSlots`. The struct is now `{merkle_hash, left, right}`, `new` still takes both hashes
because it still hashes them, and the essay moved to a citation next to the cache it describes.

**The invariant** (C1). `Prefix::length` is a plain `u16` that nothing validated, so the crate
carried a sentinel: `common_leading_bits` returned `u16::MAX` rather than 256 for equal keys,
because a length of 300 had to keep behaving as it always had. `decode_prefix` is the only
decode path in the crate and already returns `RocksResult`, so it now refuses `length > 256` by
name. With that, the sentinel becomes a plain 256, `successor` drops its upper guard,
`leaf_scan_range` builds a checked `Prefix::new` instead of a literal with a caveat, and one
latent panic closes: a row naming a length above 256 reaches `Position::of` → `Position::prefix()`,
which would index past a `[u8; 32]` and shift a `u64` by ≥ 64 *before* the "not positional"
refusal could fire. It was unreachable only because a scan bound two files away pinned the
length — an invariant held at a distance, which is the thing this round traded for a check at
the boundary that owns it. The four `prefix/tests.rs` sweeps over lengths of 257/300/`u16::MAX`
retire with the sentinel; a direct pin on the refusal replaces them.

### 17.5 The census was not counting frontier persistence

Found while reading `write_frontier_nodes` for the ablation pass, and it is a correction to the
instrument the last four rounds gated on.

`write_frontier_nodes` — the level-persist path, one caller, `persist_level` — staged its rows
with `batch.batch.put(...)` directly rather than through `put_frontier_node`. So an advance's
`2^depth` interior records incremented **neither `InteriorPuts` nor `BytesStaged`**; only
`BatchesCommitted` saw them. Every arm that advanced the frontier has been under-reporting its
own write volume in the metric `tools/ab.py` parses.

Both paths now go through one private `put_encoded_frontier_node`, which keeps the parallel
encode `write_frontier_nodes` needs and gives the counters one accounting site. The census
therefore **moves at the growth point, and the movement is exactly the missing rows**:

| point | metric | base | tip | Δ | rows implied | rows actually persisted |
|---|---|---:|---:|---:|---:|---:|
| 3M from empty (F = 16) | puts/insert | 1.336 | 1.380 | +0.044 | 132,000 | 131,070 (`2^17 − 2`) |
| 3M from empty (F = 16) | bytes staged/insert | 100.3 | 104.6 | +4.3 | — | 131,070 × 99 B = 4.33 B/insert |
| 200K from empty (F = 13) | puts/insert | 1.021 | 1.102 | +0.081 | 16,200 | 16,382 (`2^14 − 2`) |
| 200K from empty (F = 13) | bytes staged/insert | 69.0 | 77.1 | +8.1 | — | 16,382 × 99 B = 8.11 B/insert |

Both ref62 points are unchanged (1.911 puts/insert, 100.3 → 100.3): the frontier is already at
its cap of 24 there, so no level is persisted during those runs, which is why the blind spot
survived four rounds of census-identity gates. **Anyone comparing puts/insert or bytes
staged/insert from empty against a pre-round-15 number must add the persist rows back**, and
P4's projected write-volume win is now visible to the census that is supposed to demonstrate it
(under-counting it was worth ~3 % of puts/insert at 3M and would have been ~0.9 % at the
production frontier).

### 17.6 The gate

The standing refactor-neutrality protocol (rounds 5/11/12/13/14): `tools/ab.py`, 2 M fixed work,
5 interleaved reps, `--link-copy` scratches of `ref62_l119`, cold cache, base =
`bench_out/bin/r14_tip`, tip = `bin/r15_tip`; plus the 3 M from-empty arm, 3 interleaved pairs.

| point | protocol | base → tip |
|---|---|---|
| resident | `ref62_l119`, 2 M fixed work, 5 interleaved reps, cold cache | 231,234 → 231,428/s (**+0.1 %**, spreads 2.6 %/4.6 %) |
| Tier-0-like | same + 43 GiB `memhog` | 207,189 → 208,442/s (**+0.6 %**, spreads 23.3 %/18.6 %) |
| F = 0 growth | 3 M from empty, 3 interleaved pairs, default config | 710,543 → 731,308/s median (**+2.9 %**, spreads 11.0 %/3.0 %) |

All three are inside their own spreads: the round is neutral, as intended. The Tier-0-like
spreads are the widest recorded for this point (23.3 % on the base arm, one 184.7K outlier
against a 227.8K best) — noted, not explained away; the medians are 0.6 % apart and no arm's
census moved.

Two provenance notes, because the tip binary was cut twice. The resident arm was run once
before and once after two comment-only edits to `advance.rs` (230,687 → 232,831/s, **+0.9 %**,
spreads 2.0 %/6.0 %, in the earlier session); the table reports the **later** run, whose tip
binary is the final tree, and the JSON artifact holds that one. The Tier-0-like arm was run
from the earlier binary, which differs from the final tree only in those comments — §9 item 5
already records that release binaries here are never byte-identical across a comment-only diff
(`debug = true` bakes panic `Location`s), so this is stated rather than gated on. The growth arm
and the smoke check used the final binary. The two resident sessions bracket each other at
+0.1 % and +0.9 %, which is the point: this arm's honest resolution is its ~2–6 % spread, not
either median.

**Every read-side census metric is identical at every point** — 1.911 puts/insert and 28.6
leaves read/insert at both ref62 points; 0.081 subtree loads/insert, 0.1 leaves read/insert and
0.006 write batches/insert from empty. The two write-side metrics move from empty by exactly the
amount §17.5 accounts for, and only there. **Root hashes are identical**: `dc96b997078cd623…`
at ref62 (matching §8.2, §14, §15, §16) and `02552f766c82907e…` from empty (matching §15.4,
§16.4). Frontier depth, leaf count and peak RSS agree run for run (F = 16, 3.00M, 2.23 GB from
empty; F = 24, 3.62–3.65 GB at ref62).

A pre-gate smoke check compared the two binaries' whole stderr reports on 200 K from-empty
inserts: identical except throughput, latency and the two corrected census lines.

### 17.7 Evaluated and deliberately kept

The round's largest deletable block was left alone, and the reason belongs in the record. The
**oracle stack** — `SimpleMPT` (241 lines), the node types (~90), `enumerate_nodes` (~75) and
`emit_subtree_nodes` (~40) — is ~450 non-test lines that production never executes. It stays: it
is the only independent check on the root hash, which is the quantity every round in this
document uses as its correctness statement. Feature-gating it would buy no runtime and would
invite bit-rot in exactly the thing that catches a wrong hash.

Also evaluated and kept: `count_leaves_by_prefix`'s open-time fallback, which looks like pre-v3
support but is load-bearing for a live failure (a crash between a batch's leaf writes and its
metadata commit at F == 0 leaves leaves on disk with no persisted count); and unifying
`subtree_hash` with `emit_subtree_nodes`, two structurally identical compressed recursions —
real, ~−25 lines, but it puts an oracle-only function on the below-band hot path for nothing.

### 17.8 Artifacts

Raw sessions: `bench_out/ab_r15_neutrality_resident.json`,
`ab_r15_neutrality_tier0like.json`, `bench_out/r15_growth/` (per-run logs); binaries
`bench_out/bin/r14_tip` (base), `bin/r15_tip`; the growth-arm runner
`tools/r15_growth_arm.sh`; the candidate set and its disposition in
`ABLATION-PROPOSALS.md`.

## 18. Round 16 (2026-08-31): the risk round — a threading rule becomes a non-issue, two arguments become checks

Rounds 13–15 asked for shorter, honest, and ablated. Round 16 asked where the remaining
**complexity** is and which of it is load-bearing risk. The candidate set is
`RISK-PROPOSALS.md`; five of its seven items were taken.

The organising observation: complexity here is not in function size — the longest function in
the rocks path is 71 lines and only four exceed 40 — nor in any single file. It is in
**cross-file invariants the type system does not carry**. Three were load-bearing, and two of
those fail *silently*, by losing leaves rather than by crashing.

### 18.1 The lock rule is gone (R1)

`upsert_frontier` locked its worker's `pending_batches` slot and passed the `&mut WriteBatch`
down the entire subtree recursion — band descent, `merge_with_disk`, `build_band` and every
storage range scan beneath. That was safe against re-entrancy and deadlock only because
nothing reachable below the frontier enters the rayon scheduler: a rule stated in a module
header and enforced by nothing, whose violation deadlocks a worker against its own
non-reentrant `Mutex`. It also stood between this crate and any future parallelism on the read
path, which round 9 wanted once already.

The batch is now **taken** out of the slot for the duration and put back at the end. The lock
is held across a `take` and a `put`, never across the recursion. A nested frame on the same
worker would find an empty slot, stage into a batch of its own, and be absorbed on the way out
by `RocksWriteBatch::absorb` — built on `WriteBatch::iterate`, since rocksdb 0.22 exposes no
`append`. Nothing reaches that path today; it is what makes taking the slot safe if the
sequential property ever stops holding. Sequential is still what happens below the frontier;
it is no longer what correctness rests on.

`WorkerSlots<T>` had exactly one instantiation, so it became the concrete `PendingBatches` and
absorbed the drain `apply_batch` used to open-code.

### 18.2 Two silent-data-loss arguments became one debug check (R3)

Two fast routes rest on a single sentence — *the positional descent enters each position at
most once per app batch* — and both fail by losing leaves rather than by crashing: staged
leaves are invisible to range scans until the drain, and `upsert_band_child` builds a whole
subtree with **no disk scan** when a band slot proves the position empty. The prefix-addressed
recursion this design replaced could revisit a prefix within one batch (§5.5) and carried a
residency check as a data-loss guard; nothing replaced that guard.

`note_visit` is that replacement: a `#[cfg(debug_assertions)]` `DashSet` cleared at the top of
`apply_batch` and written at the three descent entry points. It checks the property directly
rather than a symptom of losing it. The whole suite — including the randomised batch tests —
passes with it armed, which is the first mechanical evidence the crate has for that sentence.
Release builds do not have the field: the assertion string is absent from `r16_tip`.

### 18.3 Three narrower ones

- **R4.** `BandSlots::{slots, full_counts}` were `pub(super)`, and the field doc already said
  the count invariant "breaks *silently* if a mutation path stops going through
  `set`/`remove`". Both are private now, with a `#[cfg(test)]` window for the tests that
  census the band. The invariant is structural rather than conventional.
- **R6.** `enumerate_nodes` left the public API. `SimpleMPT` answers it from its own map, but
  `RocksFrontierMPT` answers it by scanning **every leaf record in the database**, once per
  frontier subtree — a trap on a shared trait, and the objection that deleted `is_empty` in
  round 14. The trait method is `#[cfg(test)]`, `SimpleMPT::nodes()` is the cheap inherent one
  the example uses, and the rocks implementation plus `emit_subtree_nodes` leave release
  builds entirely — with them, the last construction of a `Node` in the flagship.
- **R7.** `Position::of`'s `.min(63)` silently returned a position for a *different* node
  given a long prefix. Unreachable twice over since round 15 (decode refuses lengths above
  256; the frontier depth is capped at 48), so the precondition is asserted in debug and the
  clamp is documented as totality rather than as the guard.

### 18.4 Not taken

**Torn-batch detection** (R2) is the cheap 90 % of §3.1.2: a generation marker written before
the leaf batches and stamped into the metadata commit would let `open` say "the last batch was
torn" instead of silently absorbing it, for ~25 lines and one extra small put per app batch.
It needs a policy ruling first, and the ruling is not obvious: it must **report**, not refuse.
The recorded operational reality is kill-heavy (522/575 bigdb runs died), those databases
reopen fine today, and a refusal would break the campaign's own workflow.

**Unifying `subtree_hash` and `emit_subtree_nodes`** (R5) would make two of the three builders
that must agree bit-for-bit agree by construction. Not taken: the existing pin is the
strongest in the suite (7 shapes including the adversarial caterpillar × 5 `keep_to` values ×
all three builders), so the marginal win is small against putting an oracle-only function on
the below-band hot path.

### 18.5 The gate, and two arms that had to be re-run

R4, R6 and R7 have no measurable surface and R3 is absent from release builds, so R1 is what
the gate measures. Standing protocol; base = `bench_out/bin/r15_tip`, tip = `bin/r16_tip`.

| point | base → tip | note |
|---|---|---|
| resident | 229,206 → 231,647/s (**+1.1 %**) | spreads 2.4 %/4.5 % |
| Tier-0-like | 222,891 → 222,342/s (**−0.2 %**) | spreads 4.3 %/12.5 %; **second session** |
| F = 0 growth, 3 M | 708,311 → 707,248/s (**−0.2 %**) | **pooled median of both sessions**, 8 runs each |

**Both negatives in the first pass were noise, and the record should say how that was
decided rather than only report the second number.** The first Tier-0-like session read
−3.8 %, with a **26.3 % spread on the base arm** — larger than the delta it was being asked to
resolve, so unresolvable by `ab.py`'s own stated standard ("a wide spread is itself a
finding"). The first growth session read −8.9 %, with an **18.5 % base spread** containing a
626 K outlier against a 742 K best, and it ran immediately after a 43 GiB `memhog` was killed.
Both were re-run on an idle machine (load < 2 enforced before starting). The re-runs read
−0.2 % and −1.5 % with base spreads of 4.3 % and 18.5 %. The growth figure quoted above is
the **pooled median of all sixteen runs, the bad session included** — the fair combination,
not the better session.

A supporting observation, not a claim: batch p99 was lower on the tip in all three ref62
sessions (140.2 → 134.2, 139.7 → 128.0, 156.8 → 146.2 ms). Shorter lock holds on the slot
shared by off-pool threads is a plausible mechanism, but it was not isolated and the p99
spreads at this point are wide.

**Every census metric is identical at every point** — 1.911 puts/insert and 28.6 leaves
read/insert at both ref62 points; 1.380 puts/insert, 104.6 bytes staged/insert, 0.006 write
batches/insert and 0.081 subtree loads/insert from empty. **Root hashes are identical** across
all 36 runs: `dc96b997078cd623…` at ref62 and `02552f766c82907e…` from empty. This round moves
no bytes — only when a lock is held.

**The ledger.** Production incl. bins 4,032 → **4,199**; non-test library 3,403 → **3,570**, of
which code 2,004 → **2,089** and comment 1,093 → **1,167**. A risk round costs lines, and this
is what they bought: a deadlock class deleted, two silent-corruption arguments turned into a
mechanical check, one invariant made structural, and a whole-database scan removed from the
public API. Tests 5,255 → 5,232; the suite is 162 tests.

### 18.6 Artifacts

Raw sessions: `bench_out/ab_r16_neutrality_resident.json`,
`ab_r16_neutrality_tier0like.json` (first), `ab_r16_neutrality_tier0like_rerun.json`,
`bench_out/r16_growth/` (first session's per-run logs; the re-run's figures are in this
section). Binaries `bench_out/bin/r15_tip` (base), `bin/r16_tip`; growth-arm runner
`tools/r16_growth_arm.sh`; candidate set and disposition in `RISK-PROPOSALS.md`.

## 19. Round 17 (2026-08-31): the first-principles round — staging leaves the recursion, the storage layer folds in, and the census stops costing

The candidate set is `FIRST-PRINCIPLES-PROPOSALS.md` (drafted from a first-principles
read: the crate's entire logical state is the sorted leaf set; everything else is a
cache of it). The maintainer approved **F3** (delete `PendingBatches`; staging becomes
owned values) and **F2** (delete `RocksMetadataBatch`; fold `storage/rocks/` into
`rocks_frontier/`) behind "as long as they don't regress perf substantially" — and made
one ruling this record has waited four rounds for:

**P4 (frontier-is-a-cache) is dead.** "We need to keep persisting the frontier for fast
crash recovery." That answers SIMPLIFY-PROPOSALS.md §11.4: the frontier level stays
durable state, minutes-scale cold reopen is not acceptable, and P4 must not be
re-proposed without a new ruling. F4 (two small dead-code ablations) was not selected
and stays open.

### 19.1 What landed

**W2 — `RocksMetadataBatch` deleted.** Thirty lines of transaction-shaped wrapper (a
`WriteBatch` behind setters, residue of the `OptimisticTransactionDB` era) served two
puts at one call site. `RocksStorage::commit_metadata(leaf_count, frontier_depth)`
builds the one `WriteBatch` itself, so the pair still cannot tear; the torn shapes the
refusal tests plant come from a test-only `plant_frontier_depth`, not the production
API. The stage-until-commit pin retired with its subject (atomicity is now by
construction in a five-line method).

**W3 — the storage layer folded in.** The two-line `src/mpt/storage/mod.rs` was the
tell: nothing had been abstracted over since round 13 deleted the `NodeStore` trait, and
`rocks_frontier/` was the only consumer. The layer moved to `rocks_frontier/storage/`
unchanged; `mpt::rocks_frontier` is now `pub` for the two outside consumers
(`tests/full_recovery_memory.rs`, `compact-db`).

**W1/W1b/W1c/W1d/W1e — staging left the recursion, in five steps, three of which the
gate killed.** The final shape: the recursion (`upsert_top` → `upsert_frontier` →
band descent) is **batch-free** — it reads disk, updates the caches and the leaf
counter, and stages nothing — and what a batch writes is a **closed form** once it
returns: every sorted-unique entry exactly once as a leaf record (a corollary of the
visit-once invariant: each entry reaches exactly one terminal, an empty-slot build or a
merge), plus one frontier row per touched subtree, whose child hashes sit in
`FrontierLevel`. `stage_batches` builds ~thread-count `WriteBatch`es in one balanced
pass (split points advanced to frontier-subtree boundaries; groups rediscovered locally,
no group table materialised). `PendingBatches`, its take/put custody, the `absorb`
backstop and the poison-aware `reset` are gone; a panic means no batch was ever built.
A stale frontier row (§3.1.0b) is excluded by phase order rather than call-site
discipline.

### 19.2 The three shapes the gate killed, so no one rebuilds them

All against `bin/r16_tip`, standing protocol:

| shape | result | mechanism |
|---|---|---|
| fold accumulators pinned to one segment per thread (`with_min_len`) | **−30.5 % resident** (227.9K → 158.4K/s; tip p99 144.7 → 207.7 ms) | pinning disables stealing *inside* a segment; the groups are I/O-bound with high-variance fault-in latency, so every batch waits on its straggler |
| unpinned fold + parallel absorb-merge to ~thread count | **−11.6 % resident, −14.4 % warm-cache** | under I/O-bound stealing rayon fragments to ~one accumulator per group — an isolated probe measured 8,811–9,067 accumulators for 10,000 groups — i.e. ~10⁴ `WriteBatch` FFI allocations per app batch plus a merge that re-stages every record |
| batch-free descent, staging separate — but census counted per record | **−5.0 % growth** (pooled, 10 pairs; uniform p50 +0.6–0.9 ms per ~14 ms batch); resident −0.4 %, warm −3.4 % | `put_leaf`'s two relaxed `fetch_add`s per record were harmless interleaved with the descent's I/O; fired from 64 threads in a ~200 µs staging burst, ~20K contended RMWs on three cache lines serialise into wall time |

A fourth hypothesis — that the extra pool wake/quiesce cycles of three parallel regions
were the cost — was tested (staging joined with the top rehash under one `rayon::join`)
and rejected: −6.2 % growth, no better.

**The control that calibrated the growth arm.** Before believing the −5 %, a
base-vs-base session (same binary, both arms, same script) read **703.8K vs 729.9K —
identical binaries 3.7 % apart — with a 641.9K deep outlier on one arm.** Every growth
session this round produced exactly one such deep outlier (577/622/623/636/641/620K),
on tip and base alike: they are machine-owned, and the arm's honest resolution today is
~±4 %. What made the W1d deficit real despite that was the sign pattern (all ten tip
runs below the base median); what made W1e's fix credible is that it dissolved the
pattern (tip's best run became the session best, and the deep outlier landed on base).

### 19.3 The census correction

`RocksWriteBatch` now tallies `bytes_staged`/`leaf_puts`/`interior_puts` in plain `u64`
fields; `RocksStorage::write_batch` flushes them to the global counters in one shot at
commit, next to `BatchesCommitted`. The accounting sites are unchanged (the §17.5 rule);
the per-batch readings are unchanged (the census is read at batch boundaries, after the
commit); a batch that is dropped rather than committed now counts nothing, which is the
honest reading. What changed is the cost model: per-record global counting was priced at
"~2.6 relaxed fetch_adds/insert — nanoseconds" (SIMPLIFY §2), which was true only while
staging was interleaved with I/O. Concentrated into a burst, the instrument itself was
the round's last regression — worth remembering for any future change that batches
formerly-scattered work.

### 19.4 The gate

Standing protocol; base = `bench_out/bin/r16_tip`, tip = `bin/r17_tip` (final, W1e).

| point | base → tip | note |
|---|---|---|
| resident | 233,328 → 226,336/s (**−3.0 %**) | spreads 4.1 %/5.2 %; an earlier session on the W1c binary read **−0.4 %** (spreads 7.4 %/2.1 %) — the two bracket this arm's resolution |
| Tier-0-like | 231,796 → 229,524/s (**−1.0 %**) | spreads 2.4 %/0.3 %; tip p99 131.9 vs 134.1 ms; this session's memhog point ran resident-fast on both arms — the delta, not the level, is the gate |
| F = 0 growth, 3 M | 736,252 → 723,210/s median (**−1.8 %**) | 5 pairs; vs the ±3.7 % base-vs-base control; six sessions and the control are all in `bench_out/` |

**Every census metric is identical at every point** — 1.911 puts/insert and 28.6 leaves
read/insert at both ref62 points; 1.380 puts/insert, 104.6 bytes staged/insert, 0.006
write batches/insert and 0.081 subtree loads/insert from empty (the pre-declared
`write batches/insert` delta never materialised: the final shape stages ~thread-count
batches, like the slots did). **Root hashes are identical across all ~70 runs**:
`dc96b997078cd623…` at ref62 and `02552f766c82907e…` from empty, matching every round
since §8.2. Peak RSS agrees run for run (3.62–3.65 GB at ref62; 2.21–2.24 GB from
empty).

**The ledger.** Non-test library 3,570 → **3,470** (code 2,092 → 2,015, comment
1,164 → 1,152); production incl. bins 4,199 → **4,100**; tests 5,513 → 5,450. The suite
is 160 tests (−2): the pending-slots drain pin died with the slots (the property is
structural — an unwound batch never existed), and the metadata stage-until-commit pin
died with the wrapper it kept honest.

### 19.5 Artifacts

Raw sessions: `bench_out/ab_r17_neutrality_resident.json` (W1, the −30.5 % kill),
`ab_r17_neutrality_resident_rerun.json` (W1b), `ab_r17_neutrality_resident_v3.json`
(W1c, −0.4 %), `ab_r17_neutrality_resident_final.json` (W1e, −3.0 %),
`ab_r17_diag_warm.json`/`ab_r17_diag_warm2.json` (the warm-cache discriminator),
`ab_r17_neutrality_tier0like.json`; growth sessions `bench_out/r17_growth/` …
`r17_growth6/` and the control `r17_growth_ctrl/`; smoke logs `r17_smoke/`. Binaries
`bench_out/bin/r16_tip` (base), `bin/r17_tip`; growth runner `tools/r17_growth_arm.sh`
(sessions 2–6 ran the same shape at 5 pairs); candidate set and disposition in
`FIRST-PRINCIPLES-PROPOSALS.md`.

## 20. Round 18 (2026-09-01): the isolation round — boundaries the compiler enforces

Four waves, all boundary work, none of them touching a hot-path decision: the aim was
component independence, not size (the ledger goes *up*, as round 16's did, and for the
same reason — deleted ambiguity costs lines). Candidates and dispositions were discussed
against the standing struck list; nothing here re-opens P4, band deletion, the atomic
batch, or the §19 staging shapes.

### 20.1 What landed

- **W1 — the census is instance-owned** (`de41045`). `RocksStorage` owns a `Census`;
  `RocksFrontierMPT::census` hands it out; `bench` snapshots and resets through the
  tree. The counter sites, the ride-in-the-batch tallies (§19.3's shape) and the stderr
  report are unchanged — `ab.py` never noticed. The library now holds no global mutable
  state, and two trees in one process are independently measurable.
- **W2 — `Prefix`'s invariants are constructor-enforced** (`92aa467`). The fields went
  private: production reads use `key()`/`length()` (inline, same codegen), construction
  goes through the debug-checked `Prefix::new`. The decode path and the corrupt-row
  tests use `pub(crate) from_raw_parts`, so a dirty on-disk tail still surfaces as the
  open-time format refusal — never a debug panic inside the decoder. The one external
  literal (`full_recovery_memory`'s junk rows) now zeroes its tail; 100 random bits keep
  the rows distinct and the scan-skip they exercise is unchanged.
- **W3 — explicit imports** (`7192d30`). `band_regime`, `advance`, `read_path` and
  `recovery` name exactly what they use instead of `use super::*`; a file's true
  dependency set is now visible at its head, and `mod.rs` shed six imports it held only
  on the glob's behalf.
- **W4 — the oracle stack is feature-gated** (`4853782`). `SimpleMPT` and the node types
  compile only under `cfg(test)` or the new `test-support` feature; a self-referential
  dev-dependency turns the feature on for every `cargo test` build, so the integration
  tests and the mermaid example (now `required-features = ["test-support"]`) run
  unchanged. "The oracle is not production code" was a comment; it is now a compile
  error — a release build of the library provably contains no `Node`.

### 20.2 The gate

Standing protocol; base = `bench_out/bin/r18_base` (58dc222), tip = `bin/r18_tip`.

| point | base → tip | note |
|---|---|---|
| resident | 229,382 → 228,959/s (**−0.2 %**) | spreads 2.4 %/7.7 % (tip's is one 214.2K outlier in rep 4); p99 134.2 → 143.8 ms, same outlier |
| F = 0 growth, 3 M | 689,098 → 696,239/s median (**+1.0 %**) | 3 pairs, pairwise +0.4/+9.0/−4.5 % — mixed signs inside this arm's ±4 % resolution (§19.4's control) |

**Every census metric is identical at both points** — 1.911 puts/insert, 28.6 leaves
read/insert, 157.2 bytes staged/insert, 0.006 write batches/insert, 0.965 subtree
loads/insert at ref62; 1.380 puts/insert and 104.6 bytes staged/insert from empty.
**Root hashes identical across all 16 runs**: `dc96b997078cd623…` at ref62 and
`02552f766c82907e…` from empty. Peak RSS 3.63 GB on both arms; init 3.91 → 3.89 s.

**The ledger.** Non-test library 3,470 → **3,557** (code 2,015 → 2,066, comment
1,152 → 1,180); production incl. bins 4,100 → **4,187**. Up on purpose: accessors,
cfg gates and explicit imports are lines that delete ambient authority. The figure that
moved the right way is new: a **release build of the library now compiles ~280 fewer
lines** (simple.rs and the node types are absent), which no previous round could say.
Suite still 160 tests, all green; CI (`cargo build`, `build --release`, `fmt`) clean.

### 20.3 Not taken, for the record

The deeper isolation moves were priced and declined in the same pass: extracting a
`Descent` view struct from the five-file `impl RocksFrontierMPT` renames the coupling
without reducing it (the recursion legitimately needs the whole state); feature-gating
`pub mod storage` behind test-support adds cargo machinery for two documented consumers;
a workspace split pays crate-boundary overhead the 3.5 K-line library cannot amortise;
and the `bench` stderr ↔ `ab.py` contract stays a deliberate API (§17, §19).

### 20.4 Artifacts

`bench_out/r18_gate_resident.json` (5 interleaved reps, cold cache, link-copy);
growth pairs in the round log (3 × base/tip interleaved, temporary DBs, fixed seed);
binaries `bench_out/bin/r18_base`, `bin/r18_tip`. Commits `de41045..4853782` plus the
fmt pass `3719c1b` on `perf/rocksdb-improvements`.

## 21. Round 19 (2026-09-01): prefix seek, measured at Tier 0 — the mechanism works and does not pay

§3.3.2 proposed a prefix extractor with bloom filters; round 4 built it, measured **−2.8 %**
and archived it (§6.2). That verdict was always weak: it was taken at F = 19 on a 3 GB
database against 62 GB of RAM, the one regime §5.4/§5.9 say cannot rank a read-side change.
Two things then changed — §9 rejected subtree pages, leaving few live read-side ideas, and
§7.1 measured the read path becoming the whole game at scale (57.1 leaves read/insert against
18.3 at 40 M). So it was re-run properly, at the scale where its best case lives.

**Verdict: measured neutral at Tier 0, and this time the mechanism is fully accounted for.**
Archived again, at `bench_out/r19_prefix_seek_rejected.patch` (base `f07c6d0`). The
instrument built to judge it is gone too (§21.4) -- it did its job during the round and
earned no resident surface afterwards. What survives is two documented facts and one handle:
§21.1's extractor-width constraint, §21.4's proven-emptiness finding, and
`with_keep_below_frontier` for the sweep §21.5 leaves open.

### 21.1 The constraint round 4 never wrote down

An extractor may be **no finer than the shallowest leaf scan**, because iterating past the
seek key's extractor prefix in prefix mode is *undefined* — RocksDB may stop early, and a
leaf scan that stops early folds the leaves it never saw out of the subtree hash. No error,
no panic, a different root. The shallowest scan is a band fault-in at `F + 1`, so

    8 * extractor_hash_bytes <= F + keep_below_frontier + 1

Selectivity pulls the other way: a skip requires an SST to hold *no* key with the seek
prefix, so the filter is useful only when **keys per extractor prefix per file** is below
about 1. With a median SST of 574 K keys spread over the key space:

| extractor | keys per prefix per file | P(absent) | measured skip rate |
|---|---:|---:|---:|
| 16 bits (k=2, forced at F=19) | 8.76 | ~1.6e-4 | **3.4 %** |
| 24 bits (k=3) | 0.034 | ~0.97 | **70.8 %** |

**The two constraints are in direct conflict below ~1 B leaves**, because F is capped by tree
size. At F = 19 only k = 2 is legal, and k = 2 can never prove absence. That is why round 4
could not have found this, and why its own sketch's k = 3 was wrong on its own terms at
F = 19. It is also the whole reason Tier 0 was required rather than merely desirable.

### 21.2 Stage 1 (62.4 M leaves, resident): the gate, and how it was cheated open

`ref62_bloom` (k=2, keep=2) and `ref62_k3` (k=3, keep=5) — 62.4 M leaves each, built from
§8.2's recipe plus the extractor. `keep = 5` is an instrument, not a configuration: raising
retention moves the common scan to `F + keep + 1 = 25` bits and so buys a 24-bit extractor
without the ~1 B leaves that F = 23 costs. Same binary both arms, same database, read
options the only difference — which removes the −8.8 % fresh-vs-day-old confound §6.2
recorded, and is a better protocol than round 4's binary-against-binary.

Per leaf scan, prefix mode against total order:

| | k=2 | k=3 |
|---|---:|---:|
| prefix-mode scans | 1.000 | 0.504 |
| bloom skips / hits per scan | 0.198 / 5.688 | 2.204 / 0.909 |
| key comparisons/scan | −7.7 % | **−20.7 %** |
| block reads/scan | **0 %** | −4.6 % |
| throughput | — | **−0.8 %** (6 reps) |

The declared gate — block reads/scan −15 % — failed in both. **The gate metric was itself
residency-sensitive, which was a flaw in its design**: `BlockReadCount` counts cache misses,
and index blocks total **21 MB against the 1 GiB block cache**, so they are permanently
resident and "does this file hold the prefix" was already answered from cache. The filter
re-derived a cached answer more cheaply (−20.7 % comparisons) and saved no reads.

One protocol note worth more than the result: 3 reps read **+9.3 %**; 6 reps read −0.8 %.
The spread (12 %) was larger than the effect, exactly as §5.2's noise-floor rule warns.

### 21.3 Stage 2 (1.0009 B leaves, F = 23, larger than RAM): the answer

`tier0_k3` — the §5.4 operating point rebuilt at production configuration (keep = 2, cap 24)
with k = 3, which is legal there because the shallowest scan is exactly 24 bits. 1.00 B
leaves, L = 119, 73 GB / 1232 SSTs, 98 min at 170,115/s. The run reproduces the recorded
Tier-0 census exactly — **57.1 leaves read/insert, 1.994 puts/insert** (§7.1) — so this is
the real operating point and not a near-miss.

9 interleaved reps, 2 M inserts, cold cache, `--link-copy`, natural LSM shape (no
`compact-db`: not a production lever). **Both spreads under §5.2's 6 % threshold, so this is
measured, not unmeasured:**

| | total-order | prefix-seek | Δ |
|---|---:|---:|---:|
| throughput, 9 reps | 90,318/s (3.8 %) | 90,970/s (5.8 %) | **+0.7 %** |
| throughput, 5 reps (separate session) | 91,266/s (4.3 %) | 90,254/s (6.4 %) | −1.1 % |
| batch p99 | 223.6 ms | 210.4 ms | −5.9 % |
| prefix-mode scans | 0.000 | **1.000** | — |
| bloom skips / hits per scan | 0 / 0 | **3.816 / 2.607** | 59.4 % skip rate |
| key comparisons/scan | 301.6 | **213.9** | **−29.1 %** |
| block reads/scan | 3.61 | 3.38 | −6.4 % |
| filter read/scan | 3.1 us | 3.0 us | flat |

Everything the stage-2 rationale predicted happened. Prefix mode covered every scan, the
filter proved absence 59.4 % of the time, index blocks stopped being cache-resident
(21 MB → **338 MB** against 1 GiB), and block reads finally moved. It bought 29 % of the
scan's key comparisons. It converted to nothing.

**Why, in one number: `leaves read/insert` is 57.1.** A prefix filter can only remove the
cost of *deciding* a file is irrelevant; it cannot touch the cost of reading the 57 leaves
per insert that are relevant. Skipping 3.8 files per scan removed 0.23 of 3.61 block reads —
the other 3.38 are data the design has to read. That argument does not depend on scale,
which is what makes this a conclusion rather than another wrong-regime deferral, and it is
the thing round 4's throughput-only rejection could not say.

Two costs to record against any future re-proposal. Filter blocks total **1365 MB — they do
not fit the 1 GiB block cache at all** (84 MB and fully resident at 62 M), so they evict data
blocks; the neutral throughput already absorbs that. And filter-read time is *symmetric*
across arms (3.1 vs 3.0 us), so the filters are paid for whether or not prefix mode consults
them. On the write side they are free: 195.9 s / 318,560 per s against §8.2's recorded
195 s / 320,000, for +1.8 % of database size.

**Correctness, which is not a footnote here** — prefix mode makes over-running a boundary
undefined, and the failure is silent. Both 62.4 M builds reproduced §8.2's recorded
bloom-less root `f9b38701ad9eb9cb…` bit-for-bit; all 21 stage-1 A/B runs agreed on §8.2's
`dc96b997078cd623…`; all 38 Tier-0 runs agreed on `5c5831598bc0a88b…`. Roughly 1.05 B
prefix-mode scans, plus a gate test per iterator family. Every iterator that spans extractor
prefixes had to state `total_order_seek` explicitly (rocksdb 0.22 exposes no
`auto_prefix_mode`) — `has_interior_rows` in implicit prefix mode would answer "no interior
rows" for a database full of them and open an `F >= 1` tree frontierless.

### 21.4 The instrument, and why none of it stayed

The census had six counters, all measuring what the tree asks RocksDB *for*; none said what
a scan costs underneath. So the round added RocksDB perf-context counters
(`UserKeyComparisonCount`, `SeekChildSeekCount`, `BlockReadCount`) plus a `LeafScans`
denominator, behind an opt-in flag. They earned their keep *during* the round -- they are the
only reason §21.3 can say prefix seek removed 29 % of the scan's key comparisons *and* moved
throughput +0.7 %, instead of reporting one number and guessing, which is exactly round 4's
failure. **All of it is nonetheless gone**, for two separate reasons, both measured.

**The perf counters cost -10.4 % at Tier 0 and -12.4 % at 62 M when enabled.** That is fine
for a structural arm and unacceptable as resident surface. With the flag *off* the residual
was not measurable -- present-and-off against fully removed came out at **-0.08 %** resident
(8 interleaved pairs, 5/8 favouring *present*) and **-0.63 %** on 3 M from empty (10 pairs,
7/10 favouring *present*), no systematic sign at either point, against a growth arm whose own
resolution is ±4 % (§19). So they could have stayed at no measured cost; they were removed on
**surface area**, in a library that has spent six rounds deleting knobs, once the idea they
served was closed. Binaries kept for a re-run: `bench_out/bin/r19_census`, `bin/r19_nocensus`;
artifact `bench_out/ab_r19_census_overhead_resident.json`.

**`LeafScans` was kept one revision longer and then dropped, because the distinction it
existed for is empirically null.** It counted every leaf range scan, against `SubtreeLoads`
counting only those that found something, so the gap was meant to expose the rate at which
the descent faults in a position that turns out to hold no leaves. Measured, that gap is
**0 of 382,592 scans in steady state** and **1 of 20,482 building from empty**.

That null is worth keeping as a *finding* rather than as a counter: **essentially every leaf
scan finds data, because `upsert_band_child` proves emptiness from the band slot and builds
the subtree from the batch with no disk scan at all.** The fast path DESIGN.md §5 claims is
therefore doing its job at both scales, which was previously assumed rather than observed --
and having observed it, an always-on duplicate of `SubtreeLoads` has no ongoing job. The
census is back to its six counters, byte-identical to before the round.

### 21.5 By-products

* **The frontier-advance timeline**, extending §5's table by a row. Depths 19–23 complete at
  **14.84 M / 32.16 M / 61.21 M / 144.71 M / 264.88 M** leaves — within ~8 % of the recorded
  values for 19–22, and depth 23 is new (§5.6's depth-24 landing at ~804 M leaves is the
  next point up).
* **A `keep_below_frontier` data point, and an open question.** keep = 5 built 62.4 M leaves
  at **367,686/s against keep = 2's 318,560/s (+15.4 %)**, with leaves read/insert **8.1 →
  1.2** for peak RSS **2.64 → 9.80 GB**. Not an isolated arm (the extractor differed) and a
  from-empty build rather than steady state — but §5.4 swept that knob *only* at F = 19 on a
  resident database, which §5.9 says cannot rank it, and the 8.1 → 1.2 is precisely the
  read-side quantity that should convert on a database larger than RAM. It is now reachable
  via `with_keep_below_frontier` / `bench --keep-below-frontier`. **This is the live idea this
  round surfaced, and it is a production knob rather than an archived one.**

### 21.7 The band goes dense — measured, and the trade is real

The round's memory arithmetic exposed something the prefix-seek question had nothing to do
with: **the band was the last layer still on a map**, and most of what it spent was the
container. `TopLevels` and `FrontierLevel` went dense in round 5 and 5.3 measured the win;
DESIGN 3's table reads 402 -> 150 -> 64 -> 32 B with the band's row the unfinished one.

Two things were verified first, because the case rests on them:

* **Per-slot cost.** A slot's payload is 74 B (8 B key + two `Option<Digest>`), and `DashMap`
  costs **108-165 B** depending on where its shards sit in the doubling cycle -- 5.86 GB for
  the 50.3 M-slot Tier-0 band with the production allocator settings. Dense is **72 B flat**
  at every size, 3.62 GB. (The 247 B/slot this round first quoted was differenced from bench
  *peak RSS*, which includes allocator retention; it is not a live-bytes figure.)
* **Occupancy**, since dense pays `2^d` regardless. A read-only census of four independent
  8-bit slices of the 1.0009 B-leaf database found **100.0 % of positions occupied at both
  band depths** (59.6 and 29.8 leaves a position), matching the Poisson estimate exactly. A
  map paying per-entry overhead to represent a fully occupied level is the wrong container.

**The design, after one false start.** Levels allocated *lazily* looked right and was wrong:
`eviction_depth`'s `max_frontier_depth - 1` floor makes the band's depth *range* widest when
the tree is *smallest*, so a 10 K-entry tree at the production cap faulted in a whole 604 MB
level -- **0.067 s -> 0.842 s**, and `tests/reopen_root_oracle.rs` (a thousand four-leaf
trees) stopped finishing. Chunking does not fix it either: with uniform random keys, 10 K
leaves spread across depth 23 touch nearly every chunk, so no granularity recovers sparsity.

What works is the opposite, and it is simpler: **allocate up front for the steady state.** The
total is maximised at the deepest frontier the cap allows -- `2^(cap+keep) - 2^cap` slots,
**3.62 GB** at the production configuration -- and every shallower frontier needs strictly
less, because the floor's extra levels are all smaller than the two at the bottom. So band
memory becomes a constant the *configuration* fixes rather than something that grows with
uptime, which is what an eviction policy would have been for, with no policy. It also makes
the retention floor free: keeping a small tree's shallow levels resident costs nothing once
the memory is reserved. `RocksFrontierConfig::band_bytes` reports it before a batch runs.

**Measured at Tier 0**, 20 M inserts so the band is ~91 % populated (`--link-copy`, cold
cache, 3 interleaved reps; `ab_r19_dense_tier0_20m_reps3.json`):

| | map band | dense band | |
|---|---:|---:|---|
| peak RSS | 11.18 GB | **7.57 GB** | **-3.61 GB (-32 %)** |
| throughput | 109,445/s | 110,630/s | +1.1 % (spreads 2.5 %/5.6 %) |
| batch p99 | 296.3 ms | 328.7 ms | **+10.9 %, worse** |

RSS is deterministic across all five runs (dense 7.55-7.57, map 11.15-11.37). Throughput is
neutral on three independent readings: **-1.4 %** (2 M inserts, 3 reps -- per-rep insert
times overlap completely), **+1.9 %** (20 M, 1 rep) and **+1.1 %** here. The p99 regression is
the one consistent negative: 3/3 reps worse (310/329/346 against 292/296/315), most likely
first-touch page faults and TLB pressure from random access across 3.62 GB where a map keeps
a compact hot table.

**The costs, in full, because they are what makes this a decision rather than a win:**

1. **p99 ~+11 %**, above.
2. **Init +2.5 s** at Tier 0 (18.0 -> 20.5 s): the arena `memset`. Insert throughput unaffected.
3. **Small trees pay the arena.** From empty at the production cap, 10 K entries takes
   0.067 -> 0.842 s and reserves 1.2 GB whatever the tree's size -- the `F == 0` floor levels.
   A deployment that stays small is strictly worse off.
4. **Partially-touched runs are worse, not better**: the 2 M-insert Tier-0 run measured
   **+2.8 GB** RSS, because the arena is paid whether used or not. The win needs a process
   that actually populates the band.
5. **`max_frontier_depth`'s range stops being honest**: `1..=48` becomes `1..=26` (cap 26 is
   14.5 GB, cap 28 is 58 GB). The constructor now refuses above 26 rather than letting a
   configured cap grow into an OOM at some later advance.
6. **"An open allocates next to nothing" is given up deliberately.**
   `tests/full_recovery_memory.rs` exists to catch "a buffer is being reserved up front",
   which this now does on purpose; it and `reopen_root_oracle` were retargeted to a shallow
   cap (neither test's intent depends on the cap) and assert "nothing beyond the configured
   band". That is a weakening of a guarded invariant and is the maintainer's call, not the
   measurement's.

Suite green at 160 tests; roots identical throughout (`5c58315…` at 2 M, `78b2327…` at 20 M).

### 21.6 Artifacts

`bench_out/ab_r19_dense_tier0.json` (2 M, 3 reps), `ab_r19_dense_tier0_20m.json` and
`ab_r19_dense_tier0_20m_reps3.json` (the dense-band arms, 21.7), binaries
`bench_out/bin/r19_{nocensus,dense}`;
`bench_out/ab_r19_stage1.json` (k=2, 4 arms × 3), `ab_r19_stage1_k3.json` (k=3, 4 arms × 3),
`ab_r19_stage1_k3_reps6.json` (2 arms × 6), `ab_r19_stage2_tier0.json` (4 arms × 5),
`ab_r19_stage2_tier0_reps9.json` (2 arms × 9); build logs and CSVs `ref62bloom_build.*`,
`ref62k3_build.*`, `tier0k3_build.*`; the archived branch
`bench_out/r19_prefix_seek_rejected.patch` (base `f07c6d0`). Databases `ref62_bloom`,
`ref62_k3`, `tier0_k3` retained on disk (82 GB total; disk at 93 %).

### 21.8 One structure for the tree top: the dense band's memory, less code, and an advance that moves nothing

§21.7 landed the dense band and listed its price honestly: p99 +11 %, init +2.5 s, a 10 K-entry
tree reserving 1.2 GB, the cap range cut to 26, two guarded test invariants loosened, and the
fiddliest code in `levels.rs` (`rebase`, carrying levels across an advance) for a throughput
gain of nothing. The question this section answers is whether the memory can be had for
*less* code rather than more. It can.

**The observation.** `TopLevels` (hashes, `0..F`), `FrontierLevel` (two child hashes, `F`) and
`BandSlots` (two child hashes plus presence, `F+1..=E`) were three encodings of one fact -- the
hash of the leaves under a position -- split by depth, and the split cost real work: every
advance copied 2^F slots from the band into the frontier array, rebuilt every ancestor into
the top array, and re-based the band. Stored as *one array of position hashes per depth*,
`0..=E+1`, with a state byte (unknown / proven empty / hashed), the frontier node's children
are simply depth `F+1`, the band's children are depth `d+1`, and the levels above the frontier
are the same arrays with every position hashed. The one rule that makes it hold: **each
position is written by the recursion that computed its hash, on the way back up**, so the
levels are current at every batch boundary and an advance changes nothing in memory -- it
persists the level and, if the retained range grew, appends one array. `recompute_ancestors`
survives only at open, parallelised (16.7 M hashes across rayon at F = 23). `get_root_hash`
is one read at every frontier depth including 0; the `F == 0` fault-in is what runs when that
read finds nothing.

Memory is the same as §21.7's total -- 33 B a position, 4.4 GB at the production cap against
268 MB + 537 MB + 3.62 GB -- because the dense band already stored every hash the unified
form stores; what it lost was the duplication and the code. Non-comment lines across
`levels.rs`, `advance.rs`, `band_regime.rs`, `recovery.rs`, `mod.rs`:

| | map (`f07c6d0`) | dense band (§21.7) | one structure |
|---|---:|---:|---:|
| non-comment lines | 891 | 988 | **835** |
| `levels.rs` | 416 | 564 | **334** |

The arrays are allocated zeroed (`Box::new_zeroed_slice`), which glibc serves as untouched
pages, so a level costs RAM only where written.

**Measured.** Same seed, same databases, root hashes identical in every run below
(`5144cae9…` at 10 K, `02552f76…` at 3 M from empty, `5c583159…` on `tier0_k3`).

From empty at the production cap, single runs:

| | map | dense band | one structure |
|---|---:|---:|---:|
| 10 K entries: init / peak RSS | 0.013 s / 0.03 GB | 0.621 s / 1.23 GB | **0.163 s / 0.37 GB** |
| 3 M entries: throughput | 725.7 K/s | 760.9 K/s | **881.6 K/s** |
| 3 M entries: peak RSS / p99 | 2.22 GB / 55.9 ms | 1.69 GB / 56.1 ms | **1.57 GB / 47.6 ms** |

The 3 M build crosses sixteen advances, each of which used to copy and rehash a level; that
is where the from-empty gain comes from, and it is the growth phase §12 lists as the band's
standing cost. The 10 K figure is the honest one: better than dense by 4x on both counts, but
not the map's, because the `cap - 1` retention floor keeps depths 14–24 for a tree that
touches ~10 K positions in each, one page apiece. That is §21.7's cost 3 reduced, not
removed; removing it is the floor's job, not the container's.

Tier 0 (`tier0_k3`, 1.0009 B leaves, F = 23, `--link-copy`, cold cache), 2 M inserts,
3 interleaved reps (`ab_r19_levels_tier0.json`):

| | throughput (spread) | p99 | peak RSS |
|---|---:|---:|---:|
| map | 88,280/s (15.1 %) | 208.2 ms | 4.44 GB |
| dense band | 91,041/s (17.5 %) | 187.8 ms | 7.25 GB |
| one structure | 92,104/s (28.8 %) | **183.2 ms** | 7.20 GB |

**Every spread is above §5.2's 6 % threshold, so throughput is not ranked here**: rep 0 was
the slowest run for all three arms (72.5 K/s for this arm against 92–93 K/s in reps 1–2),
the pattern of a cold first pass rather than of a variant. What is measured: RSS equal to the
dense band's at this insert count (2 M random inserts touch nearly every page of the deepest
levels, so lazy zero pages do not help here), p99 not worse than the dense band's -- §21.7's
one consistent negative against the map does not reappear -- and 57.1 leaves/insert, 1.994
puts/insert, unchanged, as they must be.

**Huge pages, tried and removed.** The natural fix for the dense structure's random access
over gigabytes is `MADV_HUGEPAGE` plus `MADV_POPULATE_WRITE` at allocation: 512x fewer TLB
misses and first-touch faults. Built as a second arm and measured 5 interleaved reps
(`ab_r19_levels_thp_tier0.json`), with the kernel confirmed to have granted them (784 MB of
`AnonHugePages` in a 1.4 GB process):

| | throughput (spread) | p99 | init | peak RSS |
|---|---:|---:|---:|---:|
| one structure | 92,742/s (7.6 %) | 208.4 ms | 16.4 s | 7.21 GB |
| + huge pages, pre-faulted | 91,669/s (4.0 %) | 195.5 ms | 16.6 s | 7.25 GB |

Neutral on throughput, p99 inside the run-to-run range (this arm's own p99 read 183 ms in
the three-way run above), and populating brings the 10 K-entry tree back to 1.13 GB resident.
Forty lines of `libc` for no measured return; gone, binary kept as `bin/r19_levels_thp`.

**Throughput, pinned.** The three-way run above could not rank throughput, so it was
re-run at the record's best Tier-0 resolution: 2 M inserts, 9 interleaved reps
(`ab_r19_levels_tier0_reps9.json`). Rep 0 was again the slowest run of every arm (87.7 /
90.2 / 91.6 K/s against 92–97 K/s afterwards), so the medians are given with and without it:

| | median, 9 reps (spread) | median, reps 1–8 (spread) | p99 | init |
|---|---:|---:|---:|---:|
| map | 94,682/s (9.2 %) | 94,920/s (**4.5 %**) | 180.3 ms | 18.1 s |
| dense band | 95,810/s (7.7 %) | 95,880/s (**4.1 %**) | 177.9 ms | 20.4 s |
| one structure | 95,020/s (5.4 %) | 95,178/s (**5.4 %**) | 185.7 ms | 16.5 s |

With the cold pass dropped every spread is inside §5.2's 6 %, and the three arms are within
1.0 % of each other: **Tier-0 throughput is neutral across map, dense band and one
structure**, as §21.3's leaves-read argument predicts for any change that leaves 57.1
leaves/insert alone. The one number that moves is init: 16.5 s against the map's 18.1 s and
the dense band's 20.4 s, from the parallel ancestor pass and the absence of a memset.

**The state byte, tried without.** The obvious economy is to fold the three states into the
hash slot as two reserved digests (all-zero for unknown, all-ones for empty; a 2^-256 event,
the collision the tree already assumes away) -- 32 B a position, one array, one load per
read. Built and measured (`bin/r19_levels32`, `ab_r19_levels32_tier0.json`, 7 reps):

| | Tier 0, 2 M, reps 1–6 | p99 | RSS | 3 M from empty (3 runs) |
|---|---:|---:|---:|---:|
| state byte | 93,682/s (5.6 %) | 194.9 ms | 7.20 GB | 874 / 814 / 820 K/s |
| reserved digests | 93,432/s (3.4 %) | 211.8 ms | 7.07 GB | 680 / 729 / 837 K/s |

Neutral at Tier 0 (-0.3 %), 130 MB less RSS, p99 worse in both regimes, and slower from
empty by a noisy but one-sided margin. The mechanism is the growth phase: a growing tree's
deep levels are mostly *unknown* or *empty*, and the byte array -- 16 MB at depth 24 against
512 MB of hashes -- is the compact, cache-resident index that answers for them; with the
states folded in, every empty-write and unknown-probe lands on the hash pages instead. So
the byte is not overhead; it is the index. Kept.

**The state byte only where it means something.** The other economy: keep the byte for the
incomplete levels (where it is the index) and drop it for the complete ones, `0..=F+1`,
where every position is hashed and a read at an untouched sibling -- `upsert_top`'s common
case, one per depth per path -- becomes one cache line instead of two. Twenty lines: the
state array becomes `Option`, dropped by the advance for the level that has just become
fully hashed (`bin/r19_levels_split`, `ab_r19_levels_split_tier0.json`, 7 reps):

| | Tier 0, 2 M, all 7 | reps 1–6 (spread) | p99 | init | RSS |
|---|---:|---:|---:|---:|---:|
| state byte everywhere | 93,035/s | 93,169/s (4.1 %) | 196.5 ms | 16.5 s | 7.20 GB |
| byte on incomplete levels only | 94,144/s | 94,182/s (**2.7 %**) | 194.4 ms | 16.3 s | 7.20 GB |

+1.1 %, ahead in six of seven paired reps, spreads under 6 % -- and a gap smaller than the
spread, so by §5.2 it is neutral, not a win; from empty (3 interleaved runs each) the two
overlapped completely. The mechanism is real and small: at Tier 0 the extra line per top-level
read is hidden behind the 57 leaf reads per insert. **Kept**, as the maintainer's call: the
type now says what is true -- a complete level has no states to record -- and the cost is
twenty lines and one `Option`. `bin/r19_levels_split` is the binary that landed.

**And the split with the byte folded away.** Given the split, the tempting final form is a
single integer for the complete boundary and the two reserved digests below it -- no `Level`
struct, no `Option`, no state array at all. Built as `bin/r19_levels_split32` and measured
against the split (`ab_r19_levels_split32_tier0.json`, 7 reps):

| | Tier 0, 2 M, all 7 | reps 1–6 (spread) | p99 | init | RSS | 3 M from empty (3 runs) |
|---|---:|---:|---:|---:|---:|---:|
| split, state byte below the frontier | 93,475/s | 93,894/s (3.3 %) | 188.1 ms | 16.5 s | 7.20 GB | 728 / 861 / 829 K/s |
| split, integer + reserved digests | 93,222/s | 93,510/s (2.8 %) | 191.6 ms | 17.0 s | 7.07 GB | 825 / 778 / 745 K/s |

Neutral at Tier 0 (-0.4 %), p99 and init slightly worse, 130 MB less RSS, and slower from
empty by about a tenth once each session's cold first run is set aside -- the same shape as
the first reserved-digest arm, for the same reason: below the frontier the byte array is the
index. The 10 K-entry tree also grows from 0.37 GB to 0.59 GB resident, because every
empty-write now lands on a hash page. Reverted to the split with the byte; the binary and the
run stay.

**Steady state (20 M inserts, levels ~91 % populated), 1 rep (`ab_r19_levels_tier0_20m.json`):**
| | throughput | p99 | peak RSS | sorted runs at end |
|---|---:|---:|---:|---:|
| dense band | 107,269/s | 291.2 ms | 7.58 GB | 10 |
| one structure | 109,799/s | 309.3 ms | **7.55 GB** | 8 |

RSS is the figure this run exists for and it is the dense band's: **7.55 GB against §21.7's
11.18 GB for the map, -32 %**, with the same root (`78b2327…`) and the same 41.2 leaves/insert.
Throughput and p99 are one rep each and the two arms ended at different LSM shapes (10 against
8 sorted runs), so neither is ranked; the p99 sits inside the 292–346 ms range §21.7 recorded
for the dense band across its own reps.

**What this settles and what it leaves.** The dense band's memory did not need `rebase`, a
copy loop, an ancestor recompute per advance, or a second copy of the frontier depth; one
array per depth with a write-yourself rule is smaller than the map version it replaced and
carries the same 4.4 GB bound. Two of §21.7's costs stand unchanged and belong to the *floor*,
not the container: a small tree at a deep cap still pays for the pages it touches (0.37 GB at
10 K entries), and `tests/full_recovery_memory.rs` / `reopen_root_oracle.rs` still run at a
shallow cap for that reason. The cap bound stays at 26 (17.7 GB), now as a bound on what the
tree can grow *into* rather than what an open reserves. Whether the floor should be
`min(cap - 1, ceil(log2(leaves)))` -- never retain a level with more positions than the tree
has leaves -- is the next question, and a one-line one.

Artifacts: `bench_out/ab_r19_levels_tier0.json` (3 arms × 3), `ab_r19_levels_tier0_reps9.json`
(3 arms × 9), `ab_r19_levels_thp_tier0.json` (2 arms × 5), `ab_r19_levels32_tier0.json`
(2 arms × 7), `ab_r19_levels_split_tier0.json` (2 arms × 7), `ab_r19_levels_split32_tier0.json`
(2 arms × 7), `ab_r19_levels_tier0_20m.json` (2 arms × 1); binaries `bench_out/bin/r19_levels`,
`bin/r19_levels_thp`, `bin/r19_levels32`, `bin/r19_levels_split`, `bin/r19_levels_split32`.
Suite green at 160 tests.

### 21.9 Round 19, closing: one knob, one descent, one depth limit, and a ten-times faster open

Four simplifications on top of §21.8's structure, plus the parallel frontier load, built and
measured together (`bin/r19_depth`; `ab_r19_depth_tier0.json`, `ab_r19_depth_open_tier0.json`).

**One knob: the deepest level held in RAM.** `max_frontier_depth` (cap), `keep_below_frontier`
(swept at 2) and the `cap - 1` retention floor were three ways of saying how many levels the
tree top holds, and only that number matters: for a fixed deepest level `d` the structure
costs `2^(d+1)` positions whatever the frontier does, and the merge granularity is
`N / 2^(d+1)` leaves per insert however the levels split between frontier and cache -- a batch
merges at the first position the levels do not know, which is `d`. So the frontier is free to
advance as far as the budget allows (`d - 2`: its children and the completeness gate one below
them must fit) and the levels below it are whatever is left. `RocksFrontierConfig::with_max_depth(26)`
is the production point: the same 26 levels the old cap 24 + keep 2 held, the frontier now
allowed to 24 rather than 23. For a small tree the depth follows the leaf count -- one past
`ceil(log2(leaves))` -- which is what closes §21.7's cost 3 and §21.8's open question: a
10 K-entry tree at the production configuration holds levels `0..=15`.

| from empty, production config | map (`f07c6d0`) | §21.8 split | one knob |
|---|---:|---:|---:|
| 10 K entries: init / peak RSS | 0.013 s / 0.03 GB | 0.171 s / 0.37 GB | **0.011 s / 0.03 GB** |
| 3 M entries: throughput, 3 interleaved runs | 726 K/s | 755 / 824 / 836 K/s | 814 / 804 / 788 K/s |
| 3 M entries: peak RSS / p99 | 2.22 GB / 56 ms | 1.58 GB / 45–60 ms | **1.07 GB / 44 ms** |

`tests/full_recovery_memory.rs` and `reopen_root_oracle.rs` are back at the default
configuration with their original "allocates next to nothing" assertions, and
`levels_bytes` and `with_keep_below_frontier` are gone from the public surface. The bench
flag is `--max-depth`; the old `--max-frontier-depth n` maps to `n + 2`.

**One depth limit.** `MAX_SUPPORTED_FRONTIER_DEPTH` (48, what metadata may name) and
`MAX_DENSE_FRONTIER_DEPTH` (26, what a config may ask for) became `MAX_DEPTH = 28` (17.7 GB),
enforced at configuration *and* at open: a corrupt metadata depth of 30 would otherwise have
asked the allocator for 280 GB.

**One descent.** `upsert_top`, `upsert_frontier`, `upsert_band` and `upsert_band_child`
differed in which of three storages they read; with one storage they differ in two facts --
above the frontier siblings may run in parallel and children are always known, below it an
unknown child means a merge -- so they are `upsert` and `upsert_child`, with `build_band`
asking the levels how deep they go instead of carrying `keep_to` through four signatures.
Non-comment lines across the five tree-top modules: **818**, against 891 for the map and 988
for the dense band.

**The open, in parallel.** 16.3 s of a Tier-0 open was one iterator over 8.4 M frontier rows;
the level is now scanned as disjoint positional ranges on rayon, each validating its own rows,
their counts summing to `2^F`. **16.3 s → 1.7 s.** Which exposed a protocol artifact worth
recording. Against the previous binary the new one read **-4.7 %** throughput and p99
196 → 240 ms, in every rep -- and ended every run with **10 sorted runs against 5**. The same
code with the load forced serial (`bin/r19_depth_serial`) reads **95,374/s (3.6 %)** against
the parallel build's **89,321/s (1.2 %)**, 5 interleaved reps, again 5 against 10 runs:

| same code, open only | throughput (spread) | p99 | init | RSS | sorted runs at end |
|---|---:|---:|---:|---:|---:|
| serial open | 95,374/s (3.6 %) | 180.1 ms | 16.4 s | 7.20 GB | 5 |
| parallel open | 89,321/s (1.2 %) | 233.5 ms | **1.7 s** | 6.23 GB | 10 |

`ab.py` hardlinks a checkpoint whose L0 files are pending compaction; a 16 s open let RocksDB
finish that before the timed window began, and a 1.7 s open lets it overlap the inserts -- on a
workload that is I/O-bound. The insert path is unchanged, and the serial-open build is level
with every arm in §21.8. The 1 GB of "saved" RSS is the same artifact: compaction buffers no
longer land inside the peak. Two consequences: the A/B protocol should let compaction settle
after open before timing (a `compact-db`-free way to do that is to poll `sorted_runs` until it
stops falling), and a production restart is genuinely 15 s faster.

Suite green at 160 tests; roots identical throughout. Diff against `f07c6d0`: 13 files,
+917 / -1054.

## 22. Round 20 (2026-09-02): no `unsafe`, four dependencies, no temporary directories

Two questions, both answered by measurement before the code moved: can the library's two
`unsafe` sites go, and how many of the nine direct dependencies are earning their keep.

### 22.1 The `unsafe` A/B

The library had exactly two `unsafe` sites. `allocator.rs` called glibc's `mallopt` through
an `extern "C"` declaration — the round-4 tuning that measured −17 % peak RSS on the 40 M-leaf
reference database (§6, item 1). `levels.rs` built the tree top's atomic arrays with
`Box::new_zeroed_slice(..).assume_init()` behind a `Zeroable` marker trait, so that a level
became resident only where written (§21.8).

Five arms, standing protocol (`tier0_k3`, 2 M entries, 5 interleaved reps, cold cache,
link-copy; `ab_r20_unsafe_tier0.json`):

| arm | what | throughput (spread) | init | peak RSS | p99 |
|---|---|---:|---:|---:|---:|
| `base` | HEAD `f7d8323` | 69,410/s (12.0 %) | 2.33 s | 6.27 GB | 322 ms |
| `nomallopt` | `allocator.rs` deleted | 72,146/s (7.8 %) | 2.63 s | 6.24 GB | 296 ms |
| `safelevels` | zeroed arrays as a `Default` fill | 70,853/s (3.8 %) | 2.32 s | 6.25 GB | 284 ms |
| `nounsafe` | both | 71,143/s (9.6 %) | 2.66 s | 6.23 GB | 297 ms |
| `nounsafe_env` | both, plus the same tuning as `MALLOC_*_` environment variables | 70,367/s (8.1 %) | 2.36 s | 6.27 GB | 309 ms |

Root hash `5c583159…` on all 25 runs; census identical (1.994 puts/insert, 57.1 leaves
read/insert). Every arm is inside the base arm's own 12 % spread, and **peak RSS is the
same 6.2–6.3 GB whether or not the allocator is tuned**. The −17 % the tuning was kept for
belonged to the round-4 design, whose per-batch churn was millions of small node
allocations; the dense tree top allocates its arrays once and the transient per-subtree
`Vec`s are a few KB from the arena, so there is nothing left for `M_TRIM_THRESHOLD` to
prevent. The one thing the tuning still bought is visible in the init column — 0.3 s off
the parallel frontier load, `M_TOP_PAD` sparing some `brk` calls while 8.4 M rows land —
and that is not worth an FFI declaration in a library. The environment-variable arm shows
the knob is available to any operator who wants it back without code.

The zeroed arrays needed no trade at all. `std::iter::repeat_with(T::default).take(n).collect()`
compiles, in release, to the same `__rust_alloc_zeroed` call the `unsafe` version made by
hand: LLVM recognises the fill of a fresh allocation and folds it (a 1 GiB array of
`AtomicU64` costs 14 µs and 0 resident pages either way, checked in isolation before the
arm was built), and the Tier-0 init time — 3.5 GB of levels allocated at open — is 2.32
against 2.33 s. So the property DESIGN.md §3 relies on holds without `assume_init`, at the
price that it is now an optimisation rather than a guarantee: a debug build touches every
page. The tests run at depth 6.

**Both sites are gone.** `grep unsafe src/` is empty. The one `unsafe` left in the
repository is the counting `GlobalAlloc` in `tests/full_recovery_memory.rs`, a separate
test crate: implementing that trait is `unsafe` by definition, and it is the instrument that
lets the open-time-memory pins measure Rust-side allocation exactly (RSS would see
RocksDB's WAL replay and thread stacks first). It stays.

### 22.2 Dependencies: nine to four

| was | for | now |
|---|---|---|
| `tempfile` (library) | `RocksFrontierMPT::temporary`, every test database | **gone**: the constructor is gone with it (§22.3) |
| `indicatif` (bins) | the spinner | a 30-line stderr readout, drawn only when stderr is a terminal |
| `env_logger` (bins, dev) | the log sink | a 20-line `log::Log` that reads `RUST_LOG` as a level |
| `fastrand` (bins) | the key stream | wyrand reproduced bit for bit: same seed, same keys, same root hashes (`a0ba7c5d…` at 300 K from empty on both binaries) |
| `rand` (dev) | seeded test entropy | `DefaultHasher` in counter mode over `(seed, n)` — std's deterministic keyed hash *is* a seeded generator; the tests compare against the oracle, never a recorded value |
| `--seed random` | OS entropy | `RandomState::new().build_hasher().finish()`, which std seeds from the OS |

Left: `rocksdb`, `rayon`, `sha2`, `log`. Resolved crates in a `--features bins` build:
**47 → 19** (lockfile 109 → 55 packages). `log` was kept deliberately: zero transitive
dependencies, and the frontier-advance and persist-failure messages have no other channel.

### 22.3 No temporary directories

`RocksFrontierMPT::temporary` was the library deciding where a database lives. Now
`open(path, config)` is the only constructor, `bench` requires `<db_path>` (the scripts
already passed one; `bench_plot.py` now does), and the tests take a fresh directory under
`target/test-dbs/<test name>/<n>` — the harness names each test's thread after the test —
wiped on creation and left in place, so a failing test's database is inspectable and
`cargo clean` is the cleanup. No guard type, no `/tmp`, no cleanup-on-drop to get wrong.

### 22.4 The gate

Standing protocol; base = `bench_out/bin/r20_base` (`f7d8323`), final = `bin/r20_final`
(everything above), `ab_r20_gate_tier0.json`:

| | base → final | note |
|---|---|---|
| Tier 0, 2 M entries | 73,999 → 74,351/s (**+0.5 %**) | spreads 6.6 % / 4.2 %; p99 271.9 → 306.8 ms, inside the 284–322 ms the five §22.1 arms spanned; init 2.33 → 2.62 s is the `mallopt` `M_TOP_PAD` effect on the frontier load, accepted |
| from empty, 300 K entries | root `a0ba7c5d…` on both | the key stream is the same stream |

Census identical (1.994 puts/insert, 57.1 leaves read/insert), peak RSS 6.25 → 6.23 GB,
root hash `5c583159…` on all ten runs. **The ledger:** non-comment library lines
1,772 → **1,738**; 16 files, +269 / −790 against `f7d8323`; suite 146 tests, green; `fmt`
clean; `unsafe` in `src/`: **0**; direct dependencies **9 → 4**, resolved crates 47 → 19.

## 23. Round 21 (2026-09-02): format v4, the oracle goes test-only, and a fold that had stopped firing

A review pass over the whole crate with one brief: simpler, smaller, easier to read, at the
same throughput. The big structural moves were done in rounds 13–20; what was left was
layered defensiveness, dead surface, and a handful of merges. All of it landed, plus two
decisions the review put to the maintainer and got: the on-disk format loses its tag byte,
and `SimpleMPT` compiles only under `cfg(test)`.

### 23.1 What landed

- **Format v4.** A leaf record is its 32-byte value; a frontier record is the two 32-byte
  child hashes; no tag byte, because the key already says which kind a record is (length
  256 is a leaf). Both decoders are one `try_into` through a shared `record::<N>` helper. A
  v3 record (33 or 65 bytes) is refused by length with the escape hatch named: commit
  `acff956` is the last v3-capable build, and **every reference database on this machine
  (`ref62_l119`, `ref62_compact`, `tier0_k3`, `bigdb`, …) needs it**. A v4 twin of the
  ref62 recipe was built for the gate below (`ref62_v4`, 62.4 M leaves, 227 s, 274 K/s,
  `--max-depth 22`).
- **One corrupt-row check, at decode.** `decode_prefix` now refuses a key with a set bit
  past its length. That makes the load's "not positional" comparison and its duplicate-index
  check unreachable (clean prefixes at one length map to distinct indices, and RocksDB
  returns keys in order), so both went, along with `Prefix::from_raw_parts` and the
  dirty-tail narrative on the type. The leaf scan no longer decodes a prefix at all: bytes
  2..34 of an admitted key *are* the leaf's key.
- **Dead surface deleted.** `Prefix::child`, `Prefix::prefix_of`, the `Io` error variant,
  `NODE_KEY_RANGE_END`, the `Default` derive on the byte types, the `cache` field
  (the `DB` clones the block cache into its own outlive list), the never-firing empty-batch
  filter, the out-of-range guards in `Key::get_bit` and `key_goes_right`, the depth-63
  clamps in `Position` (`unbounded_shr`/`unbounded_shl` instead), the `saturating_add` on
  an already-bounded frontier depth, and the `AtomicU16` around a field written only under
  `&mut self`.
- **Merges.** `open` absorbs `from_storage`; `batch_upsert` absorbs `apply_batch`;
  `read_frontier_depth` + `read_leaf_count` become `read_metadata`, which refuses a torn
  pair; `persist_level` puts straight into a `RocksWriteBatch`, deleting
  `write_frontier_nodes`, its rayon encode of a memcpy and `put_encoded_frontier_node`;
  `merge_with_disk`'s two-index loop is a peekable merge; `storage/test_support.rs` folds
  into `storage/tests.rs`.
- ~~**Staging is plain chunks.**~~ Landed as equal `par_chunks` with a straddled subtree's
  row staged by both chunks, byte-identical, and **reverted the same day** — the maintainer
  asked whether a row could then survive a crash without its leaves, and it can (§23.5).
  The gate figures below were taken with the chunked version: 0.405 interior puts/insert
  from empty and 1.954 at ref62 include the duplicate rows; the boundary-aligned staging
  puts 0.401 and, at ref62, the 1.911 of round 20 plus whatever the deeper v4 frontier costs.
- **The oracle is test-only.** `SimpleMPT` and the node types are `#[cfg(test)]` with no
  feature plumbing: the integration test compares a reopened root against the root it read
  before reopening, and the `mermaid` example is gone. `MerklePatriciaTree` had one
  production implementation left and is gone too; `batch_upsert`, `get_root_hash` and
  `get_leaf_value` are inherent, and the parity suite's own `TestTree` trait covers both
  types. `lib.rs` exports only what `bench`, `compact-db` and the integration test use.
- **No `bins` feature, and no `count-depths`.** The binaries build with a plain
  `cargo build`; `count-depths` existed to size the interior rows parked at former frontier
  depths, a question §11.5 closed, so it is gone with the review.
- **State arrays on every level** (§23.3): the `Option<Box<[AtomicU8]>>` that a complete
  level dropped, and `mark_complete_through`, are gone; `get`, `set` and `hashed_count`
  lose a branch each.

### 23.2 A regression caught by the gate: the zeroed fill had stopped folding

The first from-empty gate came back **−10 % with p99 doubled** (22 → 45 ms), and the
per-batch log put every slow batch at a power of two of the leaf count — where
`ensure_depth` adds a level — with RSS stepping by exactly the new level's size (35, 68,
136, 277 MB). Round 20 replaced `Box::new_zeroed_slice` with a `Default` fill and noted
that the calloc fold was "an optimisation rather than a guarantee"; in this round's build,
inlined into `ensure_depth`, LLVM no longer folded it, so every new level was memset and
faulted in whole. At the production cap that would have made the 4.4 GB tree top resident
at open. The fix is `#[inline(never)]` on `zeroed_slice` (a `with_capacity` +
`resize_with` fill in a function of its own), which gives the fold a bare allocate-then-zero
to recognise. Verified by the same log: RSS steps of 8–23 MB at level growth (the pages a
batch touches), p99 back to 23.7 ms, throughput 824 K/s. The fold remains fragile; the
per-batch `rss_bytes` column is the instrument that catches it.

### 23.3 The gate

Roots identical in every run below: `f390d096…` at 3 M from empty (the v3 and v4 builds
hash the same tree), `dc96b997…` at ref62 + 2 M across all three arms and both encodings.

From empty, 3 M entries, three binaries interleaved, 3 reps (`r21_gate_growth2.txt`):

| arm | throughput, 3 reps | median | p99 |
|---|---|---:|---:|
| `r20_final` (v3, `acff956`) | 827.6 / 829.0 / 814.2 K/s | 827.6 K/s | 23 ms |
| `r21_tip` (this round, states dropped on complete levels) | 809.1 / 818.5 / 795.4 K/s | 809.1 K/s (**−2.3 %**) | 18–26 ms |
| `r21_states` (this round, states kept) | 803.9 / 817.9 / 809.7 K/s | 809.7 K/s (**−2.2 %**) | 20–33 ms |

Inside the growth arm's ±4 % resolution (§19.4's control), the two new arms
indistinguishable from each other.

Resident, 2 M entries, 5 interleaved reps, cold cache, link-copy (`r21_gate_resident.json`):

| arm | reference | median (spread) | leaves read/insert | puts/insert | p99 |
|---|---|---:|---:|---:|---:|
| `r20_final` | `ref62_l119` (v3, F = 19) | 211,550/s (2.7 %) | 27.1 | 1.911 | 159.9 ms |
| `r21_tip` | `ref62_v4` (F = 20) | 219,861/s (3.7 %) | 19.5 | 1.954 | 142.4 ms |
| `r21_states` | `ref62_v4` (F = 20) | 220,487/s (1.3 %) | 19.5 | 1.954 | 138.2 ms |

**The base row is not a clean comparison**: the v4 reference was built at today's
`--max-depth 22`, whose cap lets the frontier reach 20, where the v3 reference's older cap
left it at 19 — hence 19.5 against 27.1 leaves read per insert, and the +3.9 % is mostly
the deeper frontier. The from-empty table is the round's whole-round gate. The **tip
against states** rows share a reference and are the state-array A/B: **+0.3 %**, spreads
3.7 % / 1.3 %, p99 142 → 138 ms, RSS 5.99 → 6.01 GB. The one cache line the dropped
state array saved on an untouched-sibling read does not show; the simplification stays.

### 23.4 The ledger and artifacts

Non-comment library lines 1,714 → **1,491** (comment lines 448 → 391), after §23.5 put the
aligned staging back; 25 files, +810 / −1,088 against `acff956` (the REVIEW.md record
included); suite 32 unit + 2 integration tests, green; `fmt` and `clippy -D warnings`
clean; `unsafe` in `src/`: 0; dependencies unchanged at four.

Artifacts: `bench_out/bin/r20_final`, `r21_tip`, `r21_states`; `bench_out/r21_gate_growth.txt`
(the pre-fix run that exposed §23.2, with `gate_r20_final.csv` / `gate_r21_tip.csv`),
`r21_gate_growth2.txt`, `r21_gate_resident.{txt,json}`, `r21_measure.sh`,
`r21_measure2.sh`; the reference `ref62_v4/` (4.7 GB).

### 23.5 Correction: the staging cuts are a crash-safety invariant

The review proposed cutting the batch into plain equal chunks, on the argument that a
subtree straddling a cut merely has its row staged twice with identical bytes. That is
true of a commit that completes. It is false of one that does not. The write batches
commit independently and become durable together only at the WAL flush; a crash between
commits, or before the flush, keeps some prefix of them in WAL order and the metadata of
the previous batch. With the old boundary-aligned cuts every surviving batch was
self-consistent — a subtree's new leaves and its new row were both there or both absent,
so the reopened tree was internally consistent whatever subset survived, which is the
property DESIGN.md §5 rests the crash story on. With equal chunks a straddled subtree's
row could survive in the committed chunk while half its leaves sat in the lost one: the
reopened root would then hash leaves that do not exist, and stay wrong until some later
batch happened to touch that subtree and rebuild it from disk.

The maintainer caught it by reading the doc comment. The aligned cuts are back, the comment
now says what they are for, and
`a_torn_commit_leaves_the_rows_agreeing_with_the_leaves` pins it: a batch is descended and
staged by hand, every other write batch is committed and the rest dropped, the database is
reopened with its old metadata, and the root must equal the oracle's root over the leaves
actually on disk. Against the chunked staging that test fails; against the aligned staging
it passes for every subset.
