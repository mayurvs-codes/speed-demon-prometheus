> **Owner must rewrite in their own words.** This is a draft written from the tuning log. The rules require a human to understand every optimisation, so read each section, check it against `src/main.rs`, and rewrite it so it is yours.

# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

The graph file is mapped and cut into 12 slices at line boundaries. Each thread parses its edges as `(tail, head, weight)` and counts out-degree per tail and in-degree per head (self-loops excluded) with atomic counters. Prefix sums over the out-degrees give a compressed adjacency array (CSR): one flat array of `(head, weight)` arcs and an offset array. A second parallel pass writes every edge into its tail's range; a per-node atomic cursor hands out unique slots.

The node ids in the file are a random relabelling, so every edge hop of a search would land far away in memory. The nodes are therefore renumbered in breadth-first order (a visited bitset keeps the search cache-friendly and the next nodes' data is prefetched), and the arc array is rebuilt in the new order by 12 threads, each writing a contiguous range. Neighbours then have nearby ids and a search walks nearby memory.

Each query is one Dijkstra search that stops as soon as the target is settled; the 2,000 queries are shared by 12 threads. The search state per node is 12 bytes (distance plus the number of the query that wrote it, so nothing is cleared between queries) and the heap holds one packed 128-bit key (distance and node) per entry. A target with no real in-edges is answered `-1` at once, because otherwise it would explore the whole 1.7 million-node main body.

## Correctness

Distances are `u64` (they can exceed 2^32); weight 0 is an ordinary weight; parallel edges use the lightest; duplicate lines and self-loops change nothing; `d(s, s) = 0` for every node including nodes with no edges; unreachable pairs print `-1`; queries are answered in input order whatever order the threads finish in. The renumbering is applied to a query's endpoints only after the "no in-edges" shortcut, which uses the original ids. Checked with the full-scale hash, the sample hash, the spec's worked example and 20 tiny seeds against `harness/reference/d_traverse.py`.

## Build and check

Rust, standard library only. The Dockerfile builds offline from an allowlisted image pinned by digest, with `-C target-cpu=x86-64-v3`. The output file is `distances.txt`.

```
python3 check.py d --data <data folder> --expected a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e
```

Full-scale seed-1 output hash: `a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e`.

## Dependencies

None. The memory-map calls (`mmap`, `madvise`) are declared directly with `extern "C"` because the standard library has no memory map.

## Results

Median 0.972 s to about 0.52 s (-47%), peak RSS 478,888 KB to about 355,300 KB (-26%) (local laptop numbers; see PERF.md for the per-change log and the board runs once filled in).

## With another 24 hours

Bidirectional search (needs a reverse adjacency array: +40 MB and +60 ms to build, estimated -15% net); parse the edges twice instead of keeping them (-60 MB for +40 ms, roughly a wash on the score).
