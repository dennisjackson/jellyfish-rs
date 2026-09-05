# Hash chains

Status: implemented on the `hashchains` branch (format v5). Extends DESIGN.md; nothing there
is changed except where this document says so.

## Goal

Every key carries a history. Instead of a leaf storing a bare 32-byte value, it stores a
**record**: the new value followed by a hash of the entry it replaced. Successive writes to
one key form a hash chain, and the Merkle root commits to the head of every chain. The tree
also keeps every superseded record so a key's full history can be read back and verified.

Decisions taken (from the discussion that produced this document):

- The Merkle leaf hash covers the whole record, so the root commits to history.
- A key repeated within one batch adds one link per occurrence, in slice order.
- Superseded records are persisted, not just their hashes.
- **Assumed, not confirmed:** the link is the *previous Merkle leaf hash*, and a key's first
  record links to an all-zero digest. This is the recommended form because the tree already
  computes it, and because each link then commits to the previous value *and* the previous
  link, so a chain is verifiable from the records alone. If a different link function is
  wanted, only `Record::next` and `hash::leaf` change (and every root).

## Definitions

```
Record        = value (32) ‖ link (32)
GENESIS_LINK  = 0x00…00 (32 bytes)

record_0      = value_0 ‖ GENESIS_LINK
record_n      = value_n ‖ leaf(key, record_{n-1})

leaf(key, r)  = SHA-256("leaf" ‖ key ‖ r.value ‖ r.link)          (was: "leaf" ‖ key ‖ value)
interior      = unchanged
```

A key's **version** is the number of records written before it; the first write is version 0.
Versions are not part of any hash. They are an index into history and are implied by chain
length, so a wrong version on disk is caught by chain verification, not hidden by it.

Every root ever published commits to the head record of each key at that time. Intermediate
records produced by a repeated key within one batch never appear under a root, but they are
in every later chain and are persisted like any other version.

## Public API

### Types (`prefix` module, re-exported at the crate root)

```rust
/// What a leaf stores: the value and the link to the record it replaced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    pub value: Value,
    pub link: Digest,
}

impl Record {
    /// The link of a key's first record.
    pub const GENESIS_LINK: Digest = Digest([0u8; 32]);

    /// A first record.
    pub fn first(value: Value) -> Record;

    /// `H("leaf" ‖ key ‖ value ‖ link)`: the Merkle leaf hash, and the next record's link.
    pub fn leaf_hash(&self, key: Key) -> Digest;

    /// The record that replaces `self` under `key` with `value`.
    pub fn next(&self, key: Key, value: Value) -> Record;

    /// `history` is a valid chain for `key`: it is non-empty, starts at `GENESIS_LINK`, and
    /// every record links to the leaf hash of the one before it.
    pub fn verify_chain(key: Key, history: &[Record]) -> bool;
}

/// Unchanged. What callers write; the tree derives the record.
pub type Entry = (Key, Value);
```

`Record` is `Copy`, 64 bytes, and has no invariants beyond its fields, so it is a plain
struct with public fields like `Key` and `Value`. The storage module, which `compact-db` and
the integration tests use directly, adds the two row types of the schema below:
`LeafRow { key, hash, head, version }` and `Version { key, seq, prev, version, record }`.

### `RocksFrontierMPT`

```rust
/// Unchanged signature. Entries are applied in slice order. Each occurrence of a key adds
/// one record to its chain, so a key that appears k times gains k versions and its leaf
/// ends at the last one. Durable per call, not atomic (DESIGN.md).
pub fn batch_upsert(&mut self, entries: &[Entry]);

/// Unchanged: the current value, `None` if the key was never written.
pub fn get_leaf_value(&self, key: Key) -> Option<Value>;

/// The current record and its version. `None` if the key was never written.
pub fn get_record(&self, key: Key) -> Option<(u64, Record)>;

/// Every record ever written under `key`, oldest first, ending with the current one. Empty
/// if the key was never written. `Record::verify_chain(key, &history)` holds for what this
/// returns.
pub fn get_history(&self, key: Key) -> Vec<Record>;

/// Versions `range` of `key`, oldest first, for long histories.
pub fn get_history_range(&self, key: Key, range: impl RangeBounds<u64>) -> Vec<Record>;
```

Everything else on `RocksFrontierMPT` (`open`, `get_root_hash`, `leaf_count`,
`frontier_depth`, census, compaction) is unchanged. `leaf_count` still counts distinct keys;
`version_count` gives the history rows written so far. The census gains
`Metric::HistoryPuts`, and `total_puts` includes it.

Read costs under the v6 schema: `get_leaf_value` and `get_record` are two point reads (the
leaf row, then the history row it names); `get_history` is one point read per version,
walking backwards from the head, so a range near the head costs its length and one near the
genesis costs the whole chain.

`SimpleMPT`, the test oracle, gets the same `Record` semantics so the two implementations
can still be compared root for root.

## Database Schema

Format version 6. Three column families; RocksDB write batches span them atomically, so the
crash-safety argument below holds. v4 and v5 databases are refused at open with a message
naming the version; there is no in-place migration (see Migration).

The first implementation (v5) kept `value ‖ link ‖ version` in the leaf record and a
second column family keyed by `key ‖ version`. It worked, and it was measured (see Cost): the
leaf row on the scan path grew from 66 to 106 bytes, and history landed in random key order,
so compaction rewrote it about ten times and competed with the leaves for the disk. v6 is the
same chain with the storage laid out around those two measurements.

### Column family `default` — leaves

Key `256_be_u16 ‖ key`, as before. The record holds no value:

```
leaf record:  leaf_hash (32) ‖ head_seq_be_u64 (8) ‖ version_be_u64 (8)    48 bytes
```

The subtree rebuild needs each leaf's Merkle hash and nothing else, so that is what is
stored; no untouched sibling is ever rehashed. `head_seq` names the history row holding the
key's current record; `version` is how many records preceded it. An overwrite needs all three
and reads nothing beyond the leaf row it already scanned: the new link *is* the stored hash,
the new row's `prev` is `head_seq`, the new version is `version + 1`.

A leaf row is 82 bytes against 66 in v4 and 106 in v5, so `LEAVES_PER_BLOCK` is 49 (62 in
v4, 38 in v5).

The column family carries **prefix bloom filters** over the first 5 key bytes (the length
and 24 bits of key), with whole-key filtering off. A subtree scan under a prefix of at least
24 bits, which is every scan once the frontier passes 23, seeks in prefix mode: each sorted
run's filter says whether it holds any leaf under the prefix before a data block is read
from it. Scans under shorter prefixes seek in total order. Whole-key filters were measured to
do nothing (DESIGN.md, Performance); they cannot serve a range scan. With 16 M distinct
prefixes at most, the filters are small.

### Column family `frontier` — the persisted level

Key `length_be_u16 ‖ key`, record `left ‖ right` (64 bytes), exactly the v4 frontier row,
in its own column family. Frontier rows are 0.99 puts per insert and were 48% of the bytes
flushed into the node column family in v5, riding through leveled compaction with 100 GB of
leaves although only 0.8 GB of them are live. Alone they are a shallow LSM that compacts
almost for free.

### Column family `history` — every version

```
key:    seq_be_u64                                     8 bytes
value:  value (32) ‖ link (32) ‖ prev_seq_be_u64 (8)   72 bytes
```

One row per version of every key, including the current one, keyed by a sequence number
allocated in write order (from 1; 0 means "none"). Each row points at the row it replaced.
Rows therefore arrive in key order: each flush is disjoint from everything on disk, leveled
compaction moves the files down without rewriting them, and history costs its flush and
nothing more. The sequence counter needs no metadata: open seeks to the last row.

A key's history is read by walking `prev_seq` back from the leaf's `head_seq`, one point read
per version. Reads are rare and the walk is verified as it goes: version 0 must be the row
with `prev_seq = 0` and no other, and a missing row is refused rather than read as a
shorter chain. The head row must hash to the leaf's stored hash.

The column family has its own small block cache so its flushes and reads cannot evict leaf
blocks.

### Store-wide

Flushes and compactions write through direct I/O. Past the RAM size the page cache's hit
rate on leaf blocks decides throughput (DESIGN.md, Performance: residency), and compaction
output would otherwise evict the blocks the scans want.

### Metadata

```
__mpt_format__          u16 be, = 6
__mpt_complete_depth__  unchanged
__mpt_leaf_count__      unchanged
```

Written with every metadata commit. Open refuses a database whose metadata has a count and
a depth but no format key (a v4 database that had committed a batch), or a format key other
than 6. A legacy database that never committed metadata is indistinguishable at open from a
v6 one in the same state, since the open decodes no leaf; its first read refuses the 32- or
72-byte record by name, exactly as v3 records are refused.

## Updating the Representation

The descent is unchanged in shape. What changes is what flows through it and what it emits.

**Sorting.** `sorted_unique_entries` becomes a stable sort by key with **no** deduplication.
`sort_by_key` is stable, so occurrences of a key keep slice order. `split_at_bit` and
`partition_point` work unchanged on a sorted slice with repeats; the debug assertions that
assume unique keys are relaxed to "sorted".

**Folding.** The old leaf row is available at exactly one point in the descent: where a
subtree is merged from disk (`merge_with_disk`), or where a child is proven empty and built
from the batch alone (no old row). At that point each run of equal keys is folded, taking a
run's worth of sequence numbers from one atomic counter:

```
head = old leaf row on disk, or None
for each value in the run, in order, at the next seq:
    version = match head {
        None       => Version::first(key, value, seq)          // link 0, prev 0, version 0
        Some(leaf) => leaf.next(value, seq)                    // link = leaf.hash, prev = leaf.head
    }
    emit version; head = version.leaf_row()
final leaf row = head
```

The fold yields a unique, sorted `Vec<LeafRow>` and `build_subtree` / `subtree_hash` run on
the stored hashes; only the batch's own new records are hashed.

**Emitting.** Before this change the descent wrote only memory and `stage_batches`
reconstructed the writes from the sorted entries. That no longer works: the records to write
exist only where they were folded. The fold therefore pushes every version it derives into a
per-batch sink, one group per merged or freshly built subtree (one lock per subtree, not per
entry; a subtree is one disk block, so this is noise against the scan). The groups cover
disjoint key ranges and are each in key order, so `stage_batches` sorts them by first key,
concatenates, and cuts at frontier boundaries as before: every version becomes a history row,
the last version of each key its leaf record, and a subtree's leaves, history rows and
frontier row still share one `WriteBatch`.

**Counting.** `leaf_count` still counts keys not on disk before the batch; repeats of a key
count once. The sequence counter is recovered at open from the last history row, so it needs
no metadata and rows a torn batch committed are simply continued from.

## Safety and Correctness

**The root is correct.** Unchanged argument, with "value" read as "record". The fold is
deterministic given the old record and the ordered run, and the old record is read from disk
inside the same single visit that DESIGN.md already relies on, so each key's record is
derived exactly once per batch.

**Chains are correct on disk.** For a key at version *n*, the leaf row's `head_seq` names a
history row whose record hashes to the leaf's stored hash, following `prev_seq` from it
reaches *n* more rows ending at one with `prev_seq = 0`, and `verify_chain` holds over them.
A subtree's leaf rows, its history rows and its frontier row are in one write batch, so a
crash never leaves a leaf pointing at a row that is not there, or rows ahead of their leaf;
`prev_seq` only ever points at rows committed by earlier batches, whose WAL was flushed before
the call returned. The intermediate rows of a repeated key are in the same batch as the final
one. Rows a torn batch committed without their leaf are unreachable and harmless.

**Durable, not atomic.** Unchanged. After a crash mid-call some subtrees may hold the batch
and others not; each is internally consistent, including its history.

**Open.** Metadata, the frontier level, and one seek to the last history row for the sequence
counter. No leaf and no other history row is read.

## Migration

None in place. A v4 database that had committed a batch is refused at open by its missing
format key, naming v4 and the last build that reads it (commit 7f87873); a v5 database by its
format key. Legacy leaf records met on a read are refused by their length, naming the format.

A migration tool is possible: scan every v4 leaf, write history row `(value, GENESIS_LINK,
0)` at the next sequence number and the leaf row `(leaf_hash, seq, 0)`, then recompute the
frontier. The recompute is a full leaf scan, which for the target size is the minutes-long
cold start DESIGN.md exists to avoid, and every root changes anyway. Roots do *not* change
between v5 and v6, which store the same chain differently; a v5 database could be rewritten
row for row.

## Cost

Per insert under v6, on top of the v4 leaf record and shared frontier row: one history row
(8 + 72 bytes) in a column family that is flushed and never compacted, and 16 more bytes in
the leaf record, so a leaf row on the scan path is 82 bytes instead of 66. Hashing per
insert *falls*: only the new record is hashed, never an untouched sibling.

The measurements below were taken on v5, which is what motivated v6; they are kept as the
record of why the schema is what it is. The v5 leaf row was 106 bytes and its history was
keyed by `key ‖ version`.

**Measured on v5** (2026-09-04, 64 cores, 62 GB RAM, NVMe; `tools/ab.py`, three interleaved
reps, median). `main` is commit 7f87873, `hashchains` this branch. `no-fresh-history` is a
measurement-only build of this branch that skips the history row for a key's first version:
it shows how much of the cost is the second column family's writes and how much the larger
leaf record, and it is what the "superseded records only" alternative above would cost on a
fresh-key workload.

Fresh inserts into a 50 M-leaf reference (seed 1234, frontier 20), page cache dropped, 2 M
entries per run:

| variant           | entries/s | puts/insert | data blocks read/insert | p99 batch |
|-------------------|----------:|------------:|------------------------:|----------:|
| main              |   255,272 |       1.954 |                   1.768 |    133 ms |
| hashchains        |   180,193 |       2.954 |                   2.592 |    156 ms |
| no-fresh-history  |   201,303 |       1.954 |                   2.598 |    149 ms |

From empty, 10 M entries per run, warm cache:

| variant           | entries/s | puts/insert | data blocks read/insert |
|-------------------|----------:|------------:|------------------------:|
| main              |   496,211 |       1.659 |                   0.067 |
| hashchains        |   287,464 |       2.660 |                   0.158 |
| no-fresh-history  |   356,294 |       1.660 |                   0.126 |

Building the two 50 M references from empty took 137 s (366 k/s) for `main` and 237 s
(211 k/s) for `hashchains`; the databases are 3.8 GB and 7.5 GB.

So the chain costs about 30% of insert throughput at 50 M leaves with a cold cache, and about
40% when everything is resident. Roughly a third of that is the history row, the rest the
larger leaf record: blocks read per insert rise by about 47% cold, because 38 rather than 62
leaf rows fit a block and a subtree scan straddles a boundary more often, and the resident
database is twice the size, so at 10 M leaves it already outgrows the 1 GB block cache that
`main` fits in. Leaves read per insert are identical, as expected: the trie shape has not
changed. Every run of a variant ended at the same root, and the two hashchains builds agree
with each other.

**One billion entries, half of them updates** (same machine, same day). `bench -n 1000000000
-u 0.5 -s 2026`: every entry is a fresh key or, with probability one half, an overwrite of a
key chosen uniformly over every key inserted so far, so updates are as cold as inserts. One
billion entries yield 500 M distinct leaves; a billion *leaves* at this mix would need two
billion entries and about 250 GB of disk, which the machine did not have.

| entries (M) | leaves (M) | frontier | k entries/s over that 100 M | sorted runs |
|------------:|-----------:|---------:|----------------------------:|------------:|
|         100 |         50 |       20 |                         190 |           5 |
|         200 |        100 |       21 |                         150 |          10 |
|         400 |        200 |       22 |                         128 |           6 |
|         600 |        300 |       22 |                         115 |           7 |
|         800 |        400 |       23 |                         105 |           8 |
|         900 |        449 |       23 |                          80 |          14 |
|        1000 |        499 |       23 |                          79 |           8 |

Overall 114 k entries/s (8744 s), peak RSS 5.7 GB, 124 GB on disk, batch p50 64 ms and p99
365 ms. Per insert over the whole run: 2.99 puts (1 leaf + 0.99 frontier + 1 history),
307 bytes staged, 12.9 leaves read in 0.99 subtree loads, 3.5 data blocks read. Leaves read
per insert are what the trie shape dictates and updates cost the same as inserts; the number
that grew is data blocks per subtree scan, 1.4 at 50 M leaves to 4.3 at 350 M, as the
database passed the 62 GB of RAM (at about 250 M leaves) and the LSM fragmented.

The drop from 105 k/s to 80 k/s after 800 M entries is RocksDB throttling, not the tree.
Its log records 819 write stalls on the *node* column family, sporadic for the first hundred
minutes and continuous from 10:43 on, because estimated pending compaction bytes (68 GB)
passed the default 64 GB soft limit. Compaction had fallen behind: eight background jobs
serve both column families, and the history column family, keyed by `key ‖ version`, lands
in random order, so every flush overlaps every level and leveled compaction rewrites each
history byte about ten times. History is roughly two thirds of the bytes on disk and is never
overwritten, so that work grows with the run. The process sat at about 39 of 64 cores
throughout: it was waiting on the disk.

**Where the cost could go.** All storage layout; none of it changes the chain or the root.

1. *Tune the history column family alone* (no format change): universal compaction, its own
   small block cache so its compactions cannot evict leaf blocks, no bloom filters. Perhaps
   10–20%. Raising the node column family's pending-compaction limit would remove the throttle
   seen above but trade it for more sorted runs, so it is a knob to measure, not a fix.
2. *Store only superseded records in history*: half a put per insert on this workload, about
   12% measured on fresh inserts (the `no-fresh-history` column above), at the price of a
   two-source `get_history`.
3. *Key history by a monotonic sequence number rather than `key ‖ version`.* Each flush is
   then disjoint from everything on disk and compaction rewrites nothing: write amplification
   near one, and nearly all compaction bandwidth handed back to the leaves. The leaf keeps the
   sequence number of its head row and each row keeps its predecessor's, so `get_history` is
   one point read per version walking backwards instead of one range scan; history reads are
   rare, so that is the right side to pay on. Atomicity with the leaf write is kept, since it
   is the same RocksDB batch.
4. *Store only `leaf_hash ‖ version` in the leaf record* (40 bytes) and keep value and link in
   history alone. The subtree rebuild needs each leaf's Merkle hash and nothing else, so the
   scan path carries 74 bytes per leaf against 66 for v4 and 106 now (55 leaves per block
   instead of 38), no untouched sibling is ever rehashed, and an update takes its new link
   straight from the stored hash. `get_leaf_value` becomes two point reads, leaf then history.

Items 3 and 4 together would put the read path within about 10% of v4 and cut history write
amplification by an order of magnitude; that is where most of the 30–40% gap should close.
Dropping the version from the leaf record was considered and kept: it saves 8 bytes but makes
every overwrite seek into history for the next version number.

**Second run: item 1 alone (v5 with universal compaction on history).** RocksDB's own
statistics showed it doing what it was meant to: by 635 M entries history compaction had
fallen from 30,172 to 1,820 CPU-seconds (extrapolated: about 2.4 times the leaves' to about
a quarter of it), write amplification on history from 11 to 4, stall time from 4.7% to 0.1%,
and the cumulative throughput lead over the first run was 5–10%. It then **filled the disk**
at 677 M entries: the directory held 194 GB of table files against 79 GB of live data, because
universal compaction holds every input of a merge until the whole merge completes and keeps
several sorted runs of the whole history at once. The run was lost. So universal compaction
is the wrong tool for a large, never-overwritten column family; keying it in write order,
item 3, is the right one, and v6 does that.

**Third run: v6** (items 2–4 above, plus the prefix bloom filters and direct I/O described
under Database Schema; same machine, same arguments, wiped database). It ended at the same
root as the first run, `bd224678…`, which is the correctness check for the new layout: v5
and v6 store the same chain.

| entries (M) | leaves (M) | frontier | v5 run 1, k entries/s over that 100 M | v6 |
|------------:|-----------:|---------:|--------------------------------------:|----:|
|         100 |         50 |       20 |                                   190 | 235 |
|         200 |        100 |       21 |                                   150 | 173 |
|         400 |        200 |       22 |                                   128 | 172 |
|         600 |        300 |       22 |                                   115 | 159 |
|         800 |        400 |       23 |                                   105 | 152 |
|         900 |        449 |       23 |                                    80 | 149 |
|        1000 |        499 |       23 |                                    79 | 146 |

| | v5 run 1 | v6 |
|---|---:|---:|
| overall | 114 k entries/s (8744 s) | **164 k entries/s** (6087 s) |
| batch p50 / p99 | 64 / 365 ms | 55 / 138 ms |
| data blocks read per insert | 3.5 | 2.5 |
| bytes staged per insert | 307 | 259 |
| write stalls / stall time | 819 / 4.7% | 17 / 0.1% |
| on disk at the end | 124 GB | 106 GB |
| peak RSS | 5.7 GB | 5.9 GB |

Leaves read per insert (12.9), puts per insert (2.99) and subtree loads are identical, as
they must be: the trie did the same work; the storage did less. RocksDB's compaction totals
for the run: leaves 44 GB live, 784 GB written by compaction (write amplification 9.4,
10,900 CPU-seconds); frontier rows 1.7 GB live, 202 GB written (3,600 CPU-seconds, out of
the leaves' way); history 58 GB live, 128 GB written of which 143 GB were *moved* rather
than rewritten (1,700 CPU-seconds, against 30,200 for v5's history). Past the RAM size the
rate held between 146 and 159 k/s where v5 fell from 115 to 79 k/s.

Against `main` (v4, fresh inserts only, docs/old/BENCHMARK-BASELINE.md §5.4–5.6, same class
of machine): main averaged about 271 k/s over its first 480 M entries and 142 k/s over a
1.76 B-leaf build, and held 95–116 k/s once the database exceeded RAM. The workloads differ,
since main's tree has twice the leaves at equal entries, but v6's 164 k/s average over a
billion entries and 146 k/s past RAM sit inside main's range rather than 30–40% below it,
which is where v5 sat. Leaf compaction (write amplification 9.4) is now the largest single
cost and is shared with v4; the frontier rows' compaction is the next.

## Testing

The oracle (`SimpleMPT`) keeps every key's chain and applies the same link rule by a
different algorithm, so the parity tests compare roots, records and histories after every
batch. The model behind the RocksDB tests is a per-key history rather than a map of current
values; its mixed batches repeat one existing and one new key so runs fold within a batch on
both the merge path and the fresh-subtree path. Specific tests cover: a key written sixty
times in one batch and once more in the next; cold extension of a chain after a reopen;
ranged history reads; a torn commit, where every leaf on disk must end its own history and
the reopened root must match an oracle fed the on-disk chains; the census counting one
history row per version; v4 and v3 records refused by name through every read path; a
history with a missing version refused rather than read as a shorter chain.
