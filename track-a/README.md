> **Owner must rewrite in their own words.** This is a draft written from the tuning log. The rules require a human to understand every optimisation, so read each section, check it against `src/main.rs`, and rewrite it so it is yours.

# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

The log is cut into one byte range per thread (12 threads) at line boundaries, and each thread reads its range with `read_exact_at` into one reused 256 KB buffer (no memory map). For every line it appends the latency (4 bytes) and the endpoint id with the 5xx flag (2 bytes) to its own lists for that line's hour; nothing else per line is kept. The hour comes straight from the fixed-width timestamp (month, day and hour digits at known offsets), so no date parsing is needed. Endpoint names are given small integer ids by a per-thread hash table, so long strings are never compared in the hot loop.

When all threads are done, the threads' (name hash, thread, id) triples are grouped by hash and one name per group is sorted by unsigned bytes; an endpoint's position in that list is its rank, so rank order is the output order within an hour.

Threads then take hours in order. For one hour, a small counting sort over the twelve threads' lists for that hour groups the latencies by endpoint rank in a buffer that stays in cache. Inside each group the three percentiles are found by selection (`select_nth_unstable`) with the nearest-rank positions `(p * n + 99) / 100`, not by sorting. The hour's lines are formatted and appended to the file as soon as every earlier hour has been written, so at most one hour of output per thread is in memory.

## Correctness

The edge cases from the spec that matter here: a 499 is not an error (only 500 to 599 count); percentiles are nearest rank (a group of one request has all three equal); latencies are `u32` values but sums and ppm products use `u64`; duplicate lines each count; endpoints compare as exact bytes and sort as unsigned bytes (a prefix sorts first); a timestamp exactly on an hour boundary belongs to the new hour. Checked with the full-scale hash, the sample hash, and 20 tiny seeds against `harness/reference/a_ingest.py`.

## Build and check

Rust, standard library only. The Dockerfile builds offline from an allowlisted image pinned by digest, with `-C target-cpu=x86-64-v3`. The output file is `summary.txt`.

```
python3 check.py a --data <data folder> --expected 495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3
```

Full-scale seed-1 output hash: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Dependencies

None. The input is read with `std::os::unix::fs::FileExt::read_exact_at`.

## Results

On the board: 0.0435 s, 50 MB, 72.3 points (`run-8d255310178a`, `run-5a4cea4e665c`), from 0.0642 s and 67.65 points with the memory map. Earlier local laptop numbers: median 0.120 s to 0.077 s, peak RSS 297 MB to 151 MB. PERF.md has every change with its run.

## With another 24 hours

Parsing is now about 20 ms above the cost of just reading the file (local), and the 10 MB of output is written in order by one thread at a time. A SIMD line splitter and a smaller per-line footprint than 6 bytes (the 19 MB of latency lists) are the next levers.
