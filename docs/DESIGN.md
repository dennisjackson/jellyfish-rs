# jellyfish-rs — Design

jellyfish-rs stores a set of key-value pairs, both 32 bytes, and maintains a Merkle root over
the set. The only write operation is a batch upsert. The database is expected to grow past
RAM, to survive a crash with a consistent tree, and to open without reading the leaves.

## Desired Functionality

The tree is a binary Merkle-Patricia trie. A key's bits, most significant first, give its
path from the root. An interior node sits at the first bit where two non-empty subtrees
diverge, so there are no single-child nodes.

A node is identified by its prefix `(key, length)`: the first `length` bits of the path, with
the remaining bits zero. Leaves have length 256. Each node is associated with a hash:

```
leaf     = SHA-256("leaf"     ‖ key ‖ value)
interior = SHA-256("interior" ‖ prefix.key ‖ prefix.length_be ‖ left ‖ right)
```

Two properties of this hashing are used throughout. A subtree's hash depends only on the
leaves beneath it, so any node can be recomputed from a scan of those leaves. Recomputing a
node requires both child hashes, so when a batch changes one child, the other child's hash
must be available without recomputation.

The operations required are:

- **Batch upsert.** Insert or overwrite many key-value pairs in one call. Batches arrive
  continuously; a batch of some thousands of entries is typical.
- **Root hash.** The Merkle root of the current set, available after every batch.
- **Lookup.** The value stored under a key.
- **Open.** Resume an existing database.

There is no delete, and no access to earlier versions of the tree. Both simplifications are
used: nodes can be updated in place under their prefix, so nothing is versioned and nothing
needs garbage collection, and a region of the tree once known to be empty stays empty until
something is written into it.

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

**In memory.** The tree top is one array per depth, from the root down to some depth a little
below the frontier. Each entry holds a hash and a small state: *hashed*, *known empty*, or
*unknown*. Every entry down to depth F+1 is hashed; those at F+1 are the child hashes of the
frontier nodes, and they are what the frontier nodes persist. Entries below F+1 are a cache.
They are filled in as subtrees are rebuilt from disk, so later updates to the same subtree
can start lower and scan fewer leaves. An unknown entry only ever costs a scan; it cannot
produce a wrong hash.

Positions below the frontier are still meaningful even though the compressed trie may have
no node there. A position covers a definite key range, and the hash of the leaves in that
range is well defined: an interior node at that depth if both halves are non-empty, or the
hash of the non-empty half otherwise. So the same arrays serve above and below the frontier.

How far below the frontier to cache is decided by the storage medium rather than by a memory
budget. Once the leaves under a position fit in one disk block, caching deeper reduces the
leaves scanned but not the blocks read, and buys nothing. The arrays are sized so that the
tree top is a few gigabytes at 10^9 leaves.

**On disk.** The store is an ordered key-value store (RocksDB). Nodes are keyed by
`(length, key)`, so the order is depth-major: every leaf under a given prefix falls in one
contiguous key range, and a whole frontier level is one contiguous range. A leaf's record is
its value; a frontier node's record is its two child hashes. Two small metadata records hold
the frontier depth and the leaf count. Only leaves and the current frontier level are
stored; interior nodes between them are recomputed.

**Advancing the frontier.** As leaves are added, the level below the frontier fills. When
every position at F+2 is hashed, level F+1 is written out as frontier nodes and F increases
by one. Nothing moves in memory. The frontier starts at 0, where the whole tree is one
subtree, and the same machinery applies throughout; there is no separate small-tree case. A
configured cap stops the frontier at a depth whose level fits the intended memory.

## Updating the Representation

A batch is first sorted by key, with duplicate keys resolved to the last value. The update
is then one recursive descent from the root, followed by a write to the store.

**Descent from the root.** At each node, the sorted batch is split at the node's bit into
the entries for the left and right children, which is a binary search on the sorted slice.
Only children that receive entries are visited; a sibling that receives none keeps its
cached hash. This continues through the complete tree above the frontier and into the cached
levels below it, for as long as the children of the current node are known.

**Fetching below the frontier.** The descent stops at the first node whose children are not
known. The leaves under that node are read from disk as one range scan, merged with the
batch's entries for that node in a single pass, and the subtree is rebuilt bottom-up from the
merged leaves. Every position the rebuild passes through, down to the cached depth, is
recorded, so the next batch to touch this subtree can descend further before it needs the
disk. A child that is known to be empty is rebuilt from the batch's entries alone, with no
scan.

**Recursing back up to the root.** Each node's new hash is computed from its children's
hashes as the recursion returns, ending with the root. The descent reads the disk and writes
only memory; the tree on disk is unchanged until the whole descent has finished.

**Writing the frontier and leaves as a batch.** After the descent, the writes are assembled:
each entry of the batch as a leaf record, and, for each frontier subtree the batch touched,
the frontier node's record with its two new child hashes. A frontier node and the leaves
beneath it are always written in the same atomic write, so the store never holds a frontier
node whose hash disagrees with its leaves. If a frontier advance is due it follows, the new
level being written with the metadata that names it, then the metadata for the batch, then a
flush of the store's write-ahead log.

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
same time, not from splitting a subtree further.

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

**Durable on successful insert, but not atomic.** When a batch call returns, every write for
it has reached the store's log and been flushed. A crash during the call can leave a subset
of its write batches on disk. Because each frontier node is written together with its
leaves, every frontier node on disk agrees with its leaves, and the recovered tree is
internally consistent; but it may be a tree the caller never saw as committed. Committing
each batch as a single atomic write would remove this case, at the cost of serialising the
store's insertion, and was judged not worth it. The one lasting effect is on the stored leaf
count, which is written after the leaves and is never recounted: after such a crash it is a
lower bound. No hash depends on it.

**Crash-safe resumption.** The frontier depth is written only after the whole level it names,
and with the last of it, so a recovered frontier depth always has its complete level on
disk. A crash in the middle of an advance leaves nodes of a level that no metadata names;
they are ignored at open and rewritten by the next advance. Open validates what it finds
rather than repairing it: a level with the wrong number of nodes, metadata that is half
present, interior nodes with no metadata naming them, or a malformed key or record all cause
open to refuse, since each could otherwise produce a plausible tree with a wrong root. A
panic in the middle of a batch leaves memory ahead of disk; the tree then refuses further
calls, and reopening it recovers.

**Quick resumption.** Open reads the metadata and the frontier level, which is 2^F records of
two hashes each, and computes the tree above the frontier from them. It reads no leaves. Its
cost is proportional to the frontier level, not to the tree, and the level is read in
parallel ranges. Everything below the frontier is filled in on demand.

## Performance

**What each insert costs.** The write side is constant in the size of the tree: one leaf
record, plus a frontier node record shared with any other entries of the batch under the
same frontier node. At a large frontier, where entries rarely share a node, this is about
two records per insert. Hashing is a small fraction of the CPU time.

The cost that grows with the tree is reading the sibling leaves needed to recompute a
subtree. If a frontier subtree holds L leaves on average, the first time a process updates
it costs a scan of about L/2 leaves, and later updates about L/4, since the cache below the
frontier lets them start lower. While the frontier can still advance, L stays roughly
constant. Once the frontier reaches its cap, L grows linearly with the number of leaves, and
so does the read work per insert. At 10^9 leaves the workers spend most of their time waiting
on these reads.

Counted in disk blocks rather than leaves, a subtree scan costs roughly one block plus one
seek per sorted run in the store, regardless of the range it covers. So the number of blocks
read per insert is flat until the tree outgrows its block floor, and caching more levels
below the frontier than the block floor reduces leaves read without reducing blocks read.

**What determines throughput**, in decreasing order of effect:

1. *Residency.* When the database fits in RAM the same code runs a little over twice as fast
   as when it is slightly larger than RAM. Nothing else comes close.
2. *The shape of the store.* Each scan pays a seek per sorted run, and the seek costs more
   than the block it precedes. Compacting to a single sorted run gives a substantial gain
   that decays as new writes fragment the store again; compacting in idle windows pays,
   compacting concurrently with inserts does not.
3. *Batching and parallelism*, as described above.
4. *Cache depth below the frontier*, down to the block floor.

Store-level tuning beyond the shape of the store, such as larger block caches or bloom
filters, was measured to make no difference at scale: the bill is the leaves actually read,
not the files consulted to find them.

**Reference points.** With a resident database of a few tens of millions of leaves the tree
sustains around 3 × 10^5 inserts per second. At 10^9 leaves, with the database somewhat
larger than RAM on a commodity SSD, it sustains around 10^5 per second, and around 1.2 × 10^5
after compaction to one sorted run. Open at that size reads about 1 GiB of frontier nodes.
