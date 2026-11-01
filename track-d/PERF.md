# Track D: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board results: TODO.** No run ID or board number has been recorded yet. Fill the Run ID, T and M columns from the live board after pushing; the "Local measurements" section below is from one laptop and is only for comparing changes against each other.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Renumber the nodes in breadth-first order and rebuild the arc array in that order | TODO | TODO | TODO | |
| 2 | Track the BFS's visited nodes in a bitset instead of the 7 MB id array | TODO | TODO | TODO | |
| 3 | Prefetch the next nodes' offsets and arcs in the BFS loop | TODO | TODO | TODO | |
| 4 | Hand finished graph pages back with `madvise` and drop `MAP_POPULATE` | TODO | TODO | TODO | |
| 5 | Pack the per-node search label to 12 bytes | TODO | TODO | TODO | |
| 6 | One packed 128-bit key per heap entry | TODO | TODO | TODO | |

## Why each change helped

1. **Renumber the nodes in breadth-first order and rebuild the arc array in that order.** The searches were 90% of the CPU and cache-missing on every hop because the ids are random; after renumbering a node's neighbours sit close to it in memory.
2. **Track the BFS's visited nodes in a bitset instead of the 7 MB id array.** The renumbering itself had grown to about 220 ms; a 218 KB bitset stays in cache.
3. **Prefetch the next nodes' offsets and arcs in the BFS loop.** The queue already says which nodes come next, so several cache misses can overlap.
4. **Hand finished graph pages back with `madvise` and drop `MAP_POPULATE`.** Each byte of the graph file is read once, so it does not need to stay resident.
5. **Pack the per-node search label to 12 bytes.** Each relaxation reads and writes a label; 25% less memory per label means fewer cache lines and fewer pages touched per thread.
6. **One packed 128-bit key per heap entry.** A distance and a node compare as one integer instead of a branchy tuple comparison.

## Local measurements (AMD Ryzen 7 6800H laptop, 12 threads; the scoring box is an i5-13500, so these are directional only)

| Step | Time | Memory |
|---|---|---|
| 1 | 0.964 s to 0.672 s (-30%) | RSS +1% |
| 2 | 0.673 s to 0.633 s (-6%) | unchanged |
| 3 | 0.636 s to 0.566 s (-11%) | unchanged |
| 4 | time within noise (0.573 s to 0.564 s) | RSS 485,936 KB to 436,816 KB (-10%) |
| 5 | 0.570 s to 0.536 s (-6%) | RSS 436,764 KB to 355,276 KB (-19%) |
| 6 | 0.539 s to 0.519 s (-3.7%) | unchanged |

Overall (interleaved A/B on the full dataset): median 0.972 s to about 0.52 s (-47%), peak RSS 478,888 KB to about 355,300 KB (-26%).

## Tried and reverted (the log lives in `tuning/LOG_TRACK_D.md` outside the repo)

- **Prefetch in the arc rebuild.** Prefetch while the arc array is rebuilt in the new order. -1.6%, under the 3% bar.
- **Thread count.** Fewer than 12 threads. 12 threads is fastest (557-578 ms against 593-636 for 10).
- **Prefetch the next heap top.** Prefetch the next heap top's data in the search. -1.7%, under the 3% bar.
- **Prefetch in the edge scatter.** Prefetch in the edge scatter that builds the arc array. -1%, under the 3% bar.
