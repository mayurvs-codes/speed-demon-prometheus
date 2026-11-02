# Track D: Traverse

The task is in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md): a road-like graph of 1.74 million nodes and 5 million weighted directed edges, and 2,000 source-target queries. We print each shortest distance, or `-1` when the target can't be reached.

## How it works

1. **Parse in parallel.** The graph file is mapped and cut into 12 slices at line boundaries. Each thread parses its edges as `(tail, head, weight)` and counts out-degree per tail and in-degree per head with atomic counters; self-loops are not counted as in-edges.
2. **Compressed adjacency.** Prefix sums over the out-degrees give a compressed sparse row layout: one flat array of `(head, weight)` arcs plus an offset per node. A second parallel pass writes each edge into its tail's range, and a per-node atomic cursor hands out the slots.
3. **Renumber for locality.** The node ids in the file are a random relabelling, so each hop of a search would land far away in memory. We renumber the nodes in breadth-first order, using a 218 KB visited bitset and prefetching the next nodes' data, and rebuild the arc array in that order with 12 threads. Neighbours then have nearby ids, and a search walks nearby memory.
4. **One search per query.** Each query runs a Dijkstra search that stops as soon as the target is settled. The 2,000 queries are shared by 12 threads through a counter.
   - **Per-thread state:** each thread has an 8-byte distance array plus a list of the nodes it touched, so only those are reset after a query.
   - **Heap:** each entry is one 128-bit key, the distance and the node packed together, so comparing two entries is one integer compare.
   - **Shortcut:** a target with no real in-edges is answered `-1` at once, instead of exploring the whole graph.

## Correctness

Distances are `u64`, because they can exceed 2^32. Weight 0 is an ordinary weight. Parallel edges are harmless: the search keeps the lightest. Self-loops change nothing. `d(s, s) = 0` for every node, including nodes with no edges. Unreachable pairs print `-1`. Answers come out in query order, whatever order the threads finish in. The "no in-edges" shortcut uses the original ids, before renumbering. We check the full-scale hash, the sample hash, the spec's worked example and fresh tiny and sample seeds against `harness/reference/d_traverse.py`.

## Build and check

Rust with the standard library only, built offline from allowlisted images pinned by digest, with `-C target-cpu=x86-64-v3`.

```
python3 check.py d --data <data folder> --expected a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e
```

## Dependencies

None. `mmap` and `madvise` are declared with `extern "C"`, because the standard library has no memory map.

## Results

On the board: about 0.47–0.50 s, 250 MB, 53.3–53.9 points (for example `run-0bec5312389c` on the final commit). [PERF.md](PERF.md) has the change log.

## Next steps

Most of the memory is the 12 per-thread distance arrays (12 × 14 MB). Bidirectional search would explore far fewer nodes, but it needs a reverse adjacency array and a second distance array per thread. Landmark-based A* is the other option.

*These docs were drafted with an AI assistant, which the event rules allow. Every step above maps to a function in `src/main.rs`.*
