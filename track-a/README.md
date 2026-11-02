# Track A: Ingest

The task, the input and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md): 3.1 million log lines, and for every (hour, endpoint) the request count, p50/p95/p99 latency and the 5xx count and rate.

## How it works

1. **Read in blocks, in parallel.** We cut `requests.log` into 12 byte ranges at line boundaries. Each thread reads its range with `read_exact_at` into one reused 256 KB buffer and parses only whole lines; a partial last line moves to the front of the buffer. We started with a memory map, but a page fault per 4 KB made it slower on the board.
2. **Keep 6 bytes per line.** The hour comes from fixed digit positions in the 27-byte timestamp, so there is no date parsing. Each endpoint name gets a small id from a per-thread hash table, so names are hashed once and never compared in the hot loop. For each line we append the latency (`u32`) and the endpoint id with the 5xx flag in its top bit (`u16`) to the thread's lists for that line's hour. Nothing else per line is stored.
3. **One sorted endpoint list.** After parsing, we group the threads' (name hash, thread, id) triples by hash and sort one name per group as unsigned bytes. An endpoint's position in that list is its rank, and rank order is the output order inside an hour.
4. **One hour at a time.** Threads take hours in order. For one hour, a counting sort over the twelve threads' lists groups the latencies by endpoint rank in a buffer that fits in cache. In each group we find the three percentiles with `select_nth_unstable` at the nearest-rank positions `(p * n + 99) / 100`, so there is no full sort.
5. **Stream the output.** A finished hour is appended to `summary.txt` as soon as every earlier hour has been written, so at most one hour of text per thread is in memory.

## Correctness

The traps from the spec: 499 is not an error, only 500 to 599 count. Percentiles are nearest rank. Latencies are `u32`, while counts and the ppm product use `u64`. Endpoints compare as exact bytes and sort as unsigned bytes, with a proper prefix first. A timestamp exactly on an hour boundary starts the new hour. Two threads' endpoint ids could collide only if two different names had the same 64-bit hash (about 1 in 10^13 for 1,842 names). We check the full-scale hash, the sample hash and fresh tiny and sample seeds against `harness/reference/a_ingest.py`.

## Build and check

Rust with the standard library only, built offline from allowlisted images pinned by digest, with `-C target-cpu=x86-64-v3`.

```
python3 check.py a --data <data folder> --expected 495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3
```

## Dependencies

None.

## Results

On the board: 0.0435 s, 50 MB, 72.3 points (`run-8d255310178a`, `run-5a4cea4e665c`). The memory-mapped version was 0.064 s and 67.65 points. [PERF.md](PERF.md) lists every change with its board run, including the ones we reverted.

## Next steps

Parsing is about 20 ms above the cost of just reading the file, and the 10 MB of output is written by one thread at a time. A SIMD line splitter and a per-line footprint smaller than 6 bytes (the latency lists are 19 MB) are the next things to try.

*These docs were drafted with an AI assistant, which the event rules allow. Every step above maps to a function in `src/main.rs`.*
