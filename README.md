# Team PROMETHEUS: Speed Demon 2026

Our entry for all four tracks of [Speed Demon 2026](https://speeddemon.dev): Rust, standard library only, zero dependencies. Each track folder builds offline from an allowlisted image pinned by digest, reads `/data` and writes one file to `/out`.

| Track | Task | How it works | Board result |
|---|---|---|---|
| [A Ingest](track-a/) | Per (hour, endpoint): count, p50/p95/p99 latency, 5xx count and rate | Block reads per thread, 6 bytes kept per line, per-hour counting sort in cache, percentiles by selection, output streamed hour by hour | 0.0435 s, 50 MB, 72.3 |
| [B Index](track-b/) | 50,000 boolean and phrase queries over 250,000 documents | Index only the query terms; AVX2 tokenizer; bitmaps for frequent terms, 16-bit posting lists for the rest; left-to-right bitmap evaluation | 0.260 s, 111 MB, 64.4 |
| [C Join](track-c/) | Hostile CSV joined to a region table, per-country aggregates | Safe cut points at record starts, hand-written quote-aware scanner, distinct names via a shared lock-free name table and one bit per (name, country) | 0.0623 s, 13.4 MB, 68.4 |
| [D Traverse](track-d/) | 2,000 shortest-path queries on a 1.74M-node road graph | CSR adjacency, nodes renumbered in breadth-first order for locality, one early-stopping Dijkstra per query across 12 threads | 0.47 s, 250 MB, 53.9 |

Each folder's `README.md` explains the approach and the correctness traps, and its `PERF.md` lists every change with the board run that measured it, including the ones we tried and reverted.

## Checking a track locally

Needs Docker and Python 3. Data comes from the organisers' generators in [speed-demon-2026](https://github.com/hackathon-raptors/speed-demon-2026) (seed 1 at `full` scale is the board's dataset).

```
python3 check.py <a|b|c|d> --data <data folder> --expected <sha256>
```

| Track | Output | Full-scale seed-1 sha256 |
|---|---|---|
| A | `summary.txt` | `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3` |
| B | `results.txt` | `f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927` |
| C | `aggregates.txt` | `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536` |
| D | `distances.txt` | `a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e` |

`check.py` builds the folder offline, runs it with the board's sandbox settings and compares the output hash. Besides seed 1, we compare fresh seeds at tiny and sample scale against the Python references in the organisers' harness.

## Licence

MIT, see [LICENSE](LICENSE).

*These docs were drafted with an AI assistant, which the event rules allow.*
