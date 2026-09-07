# jellyfish-rs — Design

jellyfish-rs stores a set of key-value pairs, both 32 bytes, and maintains a Merkle root over
the set. Every key also carries a hash chain of the values written under it, and the root
commits to the head of every chain. The only write operation is a batch upsert. The database
is expected to grow past RAM, to survive a crash with a consistent tree, and to open without
reading the leaves.

## Desired Functionality

The tree is a binary Merkle-Patricia trie. A key's bits, most significant first, give its
path from the root. An interior node sits at the first bit where two non-empty subtrees
diverge, so there are no single-child nodes.

A node is identified by its prefix `(key, length)`: the first `length` bits of the path, with
the remaining bits zero. Leaves have length 256. What a leaf holds is a **record**: the value
and a **link**, the leaf hash of the record it replaced, or zero for a key's first write. Each
node is associated with a hash:

```
leaf     = SHA-256("leaf"     ‖ key ‖ value ‖ link)
interior = SHA-256("interior" ‖ prefix.key ‖ prefix.length_be ‖ left ‖ right)
```

Successive records under one key therefore form a hash chain, each committing to the value
and the link before it, and a root commits to every key's whole history. Every record ever
written is kept, so a key's chain can be read back and verified against the root. The
history rows and how the chain is stored are described in [HASHCHAINS.md](HASHCHAINS.md).

Two properties of this hashing are used throughout. A subtree's hash depends only on the
leaves beneath it, so any node can be recomputed from a scan of those leaves. Recomputing a
node requires both child hashes, so when a batch changes one child, the other child's hash
must be available without recomputation.

The operations required are:

- **Batch upsert.** Insert or overwrite many key-value pairs in one call. Batches arrive
  continuously; a batch of some thousands of entries is typical. A key repeated within a
  batch adds one link per occurrence, in order.
- **Root hash.** The Merkle root of the current set, available after every batch.
- **Lookup.** The current record under a key, and its version.
- **History.** Every record written under a key, oldest first, or a range of versions.
- **Open.** Resume an existing database.

There is no delete, and no access to earlier *roots*. Both simplifications are used: the
tree's nodes can be updated in place under their prefix, so nothing in the tree is versioned
and nothing needs garbage collection (history is append-only and never rewritten), and a
region of the tree once known to be empty stays empty until something is written into it.

The targets are a tree of order 10^9 leaves on a commodity SSD with a few gigabytes of RAM,
sustained insertion of around 10^5 entries per second at that size, every completed batch on
disk before the call returns, a consistent tree after a crash, and a cold start measured in
seconds rather than the minutes a full scan of the leaves would take.

## Representation

With N random keys the top of the trie is dense: at a depth d well below log2(N), each of the
2^d possible prefixes has many leaves beneath it and so is an interior node. Define the
**frontier** F as the deepest depth at which all 2^F prefixes are interior nodes.

Above the frontier level F the trie is a complete binary tree. Below F the trie thins out
into small subtrees. We can exploit this observation to represent the tree efficiently in
memory and operate on it concurrently.

Nodes above the frontier can be stored in a dense array; we need only store the 32-byte
hash for each node. This works because the node's position in the array determines both its
own prefix and its children's prefixes.

Below the frontier, the MPT is not complete, so we can't exploit this representation.
However, each subtree below a frontier node is entirely independent of the other subtrees
and corresponds to all the leaf nodes which match the frontier node's prefix. For these
nodes we can adopt a sparse representation where we only store the leaf nodes on disk as a
contiguous range from which we can recompute the entire subtree.

On a cold start, we need to be able to resume quickly. Doing a scan of every leaf node to
rebuild the tree would be expensive, so we persist nodes in the frontier to disk whenever
they're updated. Then when we restart, we need only read the frontier nodes from disk, which
are a tiny fraction of the overall tree size.

**In memory.** The tree top is one array per depth, from the root down to a depth below the
frontier. Each entry holds a hash and a small state: *hashed*, *known empty*, or *unknown*.
Every entry down to depth F+1 is hashed; those at F+1 are the child hashes of the frontier
nodes, and they are what the frontier nodes persist. Entries below F+1 are a cache. They are
filled in as subtrees are rebuilt from disk, so later updates to the same subtree can start
lower and scan fewer leaves. An unknown entry only ever costs a scan; it cannot produce a
wrong hash.

Positions below the frontier are still meaningful even though the compressed trie may have
no node there. A position covers a definite key range, and the hash of the leaves in that
range is well defined: an interior node at that depth if both halves are non-empty, or the
hash of the non-empty half otherwise. So the same arrays serve above and below the frontier.

How far below the frontier to hold is decided by the storage medium rather than by a memory
budget. Once the leaves under a position fit in one disk block, caching deeper reduces the
leaves scanned but not the blocks read, and buys nothing on its own. What does buy something
is starting every scan at least 26 bits deep: the leaf store's bloom filters are over 26-bit
key prefixes (below), and a scan whose prefix is at least that long skips every sorted run
holding no leaf under it. So the tree top is held to depth 26 from the first batch; the
arrays are 4.4 GB, resident as positions are touched.

**On disk.** The store is an ordered key-value store (RocksDB) with three column families.

*Leaves.* Keyed by the key, in an order-preserving encoding whose first 4 bytes are the key's
first 26 bits, so every leaf under a given prefix falls in one contiguous key range and the
prefix bloom filters can name it. A leaf's record is its Merkle hash, a pointer to the
history row holding its value and link, and its version. The subtree rebuild needs the hash
alone, so no untouched leaf is ever rehashed; an overwrite needs the hash (it becomes the
new link) and the pointer (the new row's predecessor) and reads nothing beyond the row it
already scanned.

*History.* Every record ever written, one row per version, keyed by a sequence number
allocated in write order and pointing at the row it replaced. Rows arrive in key order, so
the store moves them down its levels without rewriting them. A key's history is read by
walking the pointers back from its leaf.

*Frontier.* Frontier nodes keyed by `(length, key)`, so a whole level is one contiguous
range; a node's record is its two child hashes. They are rewritten at every batch that
touches their subtree, and live in their own column family so that churn never passes
through the leaves' compactions. Three small metadata records beside them hold the frontier
depth, the leaf count and the format version.

Only leaves and the current frontier level are stored; interior nodes between them are
recomputed.

**Advancing the frontier.** As leaves are added, the level below the frontier fills. When
every position at F+2 is hashed, level F+1 is written out as frontier nodes and F increases
by one. Nothing moves in memory. The frontier starts at 0, where the whole tree is one
subtree, and the same machinery applies throughout; there is no separate small-tree case. A
configured cap stops the frontier at a depth whose level fits the intended memory.

## Updating the Representation

A batch is first sorted by key, stably, so that repeats of a key keep their order. The update
is then one recursive descent from the root, followed by a write to the store.

**Descent from the root.** At each node, the sorted batch is split at the node's bit into
the entries for the left and right children, which is a binary search on the sorted slice.
Only children that receive entries are visited; a sibling that receives none keeps its
cached hash. This continues through the complete tree above the frontier and into the held
levels below it, for as long as the children of the current node are known.

**Fetching below the frontier.** The descent stops at the first node whose children are not
known. The leaf rows under that node are read from disk as one range scan and merged with the
batch's entries for that node in a single pass. Where the batch has entries for a key, its
run of values is folded onto the row on disk (or onto nothing, for a new key): each value
becomes a new record whose link is the previous leaf hash, at the next sequence number, and
the last of them is the key's new leaf row. The subtree is then rebuilt bottom-up from the
merged rows' stored hashes. Every position the rebuild passes through, down to the held
depth, is recorded, so the next batch to touch this subtree can descend further before it
needs the disk. A child that is known to be empty is rebuilt from the batch's entries alone,
with no scan.

**Recursing back up to the root.** Each node's new hash is computed from its children's
hashes as the recursion returns, ending with the root. The descent reads the disk and writes
only memory; the tree on disk is unchanged until the whole descent has finished. The records
the folds derive are collected in a per-batch sink, one group per merged subtree.

**Writing the batch.** After the descent, the writes are assembled from the sink: every
derived record as a history row, the last record of each key as its leaf row, and, for each
frontier subtree the batch touched, the frontier node's record with its two new child
hashes. A frontier node, the leaf rows beneath it and their history rows are always written
in the same atomic write, so the store never holds a frontier node whose hash disagrees with
its leaves, or a leaf whose history row is missing. If a frontier advance is due it follows,
the new level being written with the metadata that names it, then the metadata for the
batch, then a flush of the store's write-ahead log.

## Making use of Concurrency

**Batches.** Applying a batch as one descent, rather than one insert at a time, makes the
cost of the tree top independent of how many entries share it: an ancestor common to k
entries is hashed once, and a subtree is scanned once however many of its leaves the batch
changes. It also makes it possible to keep the batch's leaves out of the store until the
descent is over, which is what allows the descent to trust a range scan (see the correctness
argument below). Finally the writes to the store are grouped into a small number of large
write batches rather than one per subtree; forming very many small write batches was
measured to cost more than the writes themselves.

**Above the frontier.** The two children of a node cover disjoint key ranges and disjoint
array positions, so they can be updated by independent tasks with no locking. The descent
above the frontier is a fork-join recursion: a node forks its children when the slice it
holds is large enough to be worth it, and combines their hashes when both have returned. A
parent is written only after its children's tasks have joined, so no two tasks ever write the
same array entry, and the join orders every cross-task read. Each entry's hash is stored
before its state flag is set, with release/acquire ordering on the flag, so a task that sees
an entry as hashed also sees its hash.

**Below the frontier.** A subtree below the frontier is small, about one disk block, and its
rebuild is one scan and one pass. It is processed sequentially by the task that reached it.
Parallelism at this level comes from the many subtrees a batch touches being processed at the
same time, not from splitting a subtree further. Sequence numbers come from one atomic
counter, a run's worth at a time, and the sink is locked once per subtree, so neither is
contended.

**Writes.** The write batches are submitted to the store concurrently, so that the store's
own in-memory insertion runs in parallel. The frontier-advance writes and the final
metadata write are sequential, because their order is what makes recovery correct.

## Safety and Correctness

**The root is correct.** After a batch, the root is the Merkle root of the leaves on disk
together with the batch. The argument:

- A cached hash is written only by a computation that has just derived it from the leaves
  beneath its position (from disk, the batch, or both), and every ancestor of a rewritten
  entry is rewritten on the way back up. So a hashed entry is always current.
- Rebuilding a subtree by splitting at each position's bit yields exactly the compressed
  trie's hash: a two-sided split is an interior node at that depth, a one-sided split passes
  the hash through. The positional recursion and the pure compressed recursion are therefore
  identical, and the tests check both against an independent reference implementation.
- Each position is visited at most once per batch. This is what makes the range scan safe:
  the batch's leaves are not on disk during the descent, so a second visit to the same
  region would scan the store and miss them. The property holds because the descent
  partitions the batch by key, and it is asserted in debug builds.
- An entry marked empty is trustworthy because there is no delete: it stays empty until a
  batch writes beneath it, and that batch rewrites the entry.
- A key's chain is derived exactly once per batch, inside that single visit, from the row on
  disk and the ordered run, so the stored leaf hash is the hash of the record its pointer
  names. The reference implementation keeps every chain whole and the tests compare roots,
  records and histories after every batch.

**Durable on successful insert, but not atomic.** When a batch call returns, every write for
it has reached the store's log and been flushed. A crash during the call can leave a subset
of its write batches on disk. Because each frontier node is written together with its leaves
and their history rows, every frontier node on disk agrees with its leaves, every leaf's
chain is whole, and the recovered tree is internally consistent; but it may be a tree the
caller never saw as committed. Committing each batch as a single atomic write would remove
this case, at the cost of serialising the store's insertion, and was judged not worth it. The
one lasting effect is on the stored leaf count, which is written after the leaves and is
never recounted: after such a crash it is a lower bound. No hash depends on it. History rows
a torn batch committed without their leaf are unreachable and harmless.

**Crash-safe resumption.** The frontier depth is written only after the whole level it names,
and with the last of it, so a recovered frontier depth always has its complete level on
disk. A crash in the middle of an advance leaves nodes of a level that no metadata names;
they are ignored at open and rewritten by the next advance. Open validates what it finds
rather than repairing it: a level with the wrong number of nodes, metadata that is half
present, interior nodes with no metadata naming them, a database of an earlier format, or a
malformed key or record all cause open to refuse, since each could otherwise produce a
plausible tree with a wrong root. A chain that ends early, points at a missing row, or does
not hash to its leaf is refused when read. A panic in the middle of a batch leaves memory
ahead of disk; the tree then refuses further calls, and reopening it recovers.

**Quick resumption.** Open reads the metadata, the frontier level, which is 2^F records of
two hashes each, and the last history row for the sequence counter, then computes the tree
above the frontier. It reads no leaves. Its cost is proportional to the frontier level, not to
the tree, and the level is read in parallel ranges. Everything below the frontier is filled
in on demand.

## Performance

**What each insert costs.** The write side is constant in the size of the tree: one leaf row,
one history row, plus a frontier node record shared with any other entries of the batch
under the same frontier node. At a large frontier, where entries rarely share a node, this is
about three records per insert. Hashing is one SHA-256 per new record and a small fraction of
the CPU time; untouched siblings are never rehashed, since their hashes are stored.

The cost that grows with the tree is reading the sibling leaves needed to recompute a
subtree. If a frontier subtree holds L leaves on average, the first time a process updates
it costs a scan of about L/2 leaves, and later updates about L/4, since the cache below the
frontier lets them start lower. While the frontier can still advance, L stays roughly
constant. Once the frontier reaches its cap, L grows linearly with the number of leaves, and
so does the read work per insert. At 10^9 leaves the workers spend most of their time waiting
on these reads.

Counted in disk blocks rather than leaves, a subtree scan costs about one block per sorted
run that holds a leaf under its prefix, plus a filter probe for every run that does not. With
26-bit prefix filters and half a billion leaves, about seven leaves share a prefix, so the
largest level always holds one and the smaller levels usually do not: measured at 1.8 blocks
per scan over four populated levels, 2.5 once the tree doubled and the frontier reached its
cap. Without the filters every run costs a block whether or not it holds a leaf.

**What determines throughput**, in decreasing order of effect:

1. *Residency.* When the database fits in RAM the same code runs a little under twice as fast
   as when it is larger than RAM. Nothing else comes close. Flushes and compactions use
   direct I/O so their output does not evict the leaf blocks the scans want.
2. *The shape of the store.* Each scan pays a block for every sorted run that holds a leaf
   under it. The leaf store is kept to few levels (a large level base) and its runs are
   probed through prefix bloom filters; compacting to a single sorted run in an idle window
   still pays, compacting concurrently with inserts does not. History is keyed by write order
   so it is never compacted at all; frontier rows have their own column family so their churn
   never passes through the leaves.
3. *Batching and parallelism*, as described above.
4. *Depth held below the frontier*, down to the filters' prefix length.

Store-level tuning beyond this was measured to make no difference at scale: whole-key bloom
filters cannot serve a range scan, and larger block caches or blocks bought nothing. Prefix
filters are the exception because they change which runs a scan touches, not how it reads
them.

**Reference points** (docs/HASHCHAINS.md, Cost, has the measurements). On a 64-core machine
with 62 GB of RAM and an NVMe disk, with half of every batch overwriting existing keys: a
billion entries into an empty tree at 197 × 10^3 entries per second overall, 171 × 10^3 over
the last hundred million at 500 × 10^6 leaves and a 106 GB database; then another 500 × 10^6
fresh keys at 137 × 10^3 per second overall and 117 × 10^3 over the last hundred million, at
10^9 leaves and 164 GB, with the frontier at its cap of 24. Peak RSS 9 GB. Open at
500 × 10^6 leaves took 12 seconds and read 0.8 GB of frontier nodes.
