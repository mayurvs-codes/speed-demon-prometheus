> **Owner must rewrite in their own words.** This is a draft written from the tuning log. The rules require a human to understand every optimisation, so read each section, check it against `src/main.rs`, and rewrite it so it is yours.

# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

The log is mapped into memory and cut into one slice per thread (12 threads) at line boundaries. Each thread turns its lines into small records: an endpoint id, an hour number from 0 to 71, the latency and a flag for a 5xx status. The hour comes straight from the fixed-width timestamp (month, day and hour digits at known offsets), so no date parsing is needed. Endpoint names are given small integer ids by a per-thread hash table, so long strings are never compared in the hot loop.

When all threads are done, the endpoint names from all threads are sorted by unsigned bytes into one list; an endpoint's position in that list is its rank. A record's bucket is `hour * (number of endpoints) + rank`, so the bucket order is exactly the output order. A parallel counting sort puts every latency into its bucket: each thread counts its own records per bucket, the counts become start positions (chunk after chunk inside each bucket), and each thread then writes its own latencies into those private ranges.

Inside each bucket the three percentiles are found by selection (`select_nth_unstable`) with the nearest-rank positions `(p * n + 99) / 100`, not by sorting. Each thread formats a run of hours into its own buffer, and the buffers are written in hour order.

## Correctness

The edge cases from the spec that matter here: a 499 is not an error (only 500 to 599 count); percentiles are nearest rank (a group of one request has all three equal); latencies are `u32` values but sums and ppm products use `u64`; duplicate lines each count; endpoints compare as exact bytes and sort as unsigned bytes (a prefix sorts first); a timestamp exactly on an hour boundary belongs to the new hour. Checked with the full-scale hash, the sample hash, and 20 tiny seeds against `harness/reference/a_ingest.py`.

## Build and check

Rust, standard library only. The Dockerfile builds offline from an allowlisted image pinned by digest, with `-C target-cpu=x86-64-v3`. The output file is `summary.txt`.

```
python3 check.py a --data <data folder> --expected 495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3
```

Full-scale seed-1 output hash: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Dependencies

None. The memory-map calls (`mmap`, `madvise`) are declared directly with `extern "C"` because the standard library has no memory map.

## Results

Median 0.120 s to 0.077 s (-36%), peak RSS 297,412 KB to 150,900 KB (-49%) (local laptop numbers; see PERF.md for the per-change log and the board runs once filled in).

## With another 24 hours

Fuse parsing and counting to avoid the 37 MB record array (needs the endpoint ranks earlier). Replace the memory map with a `pread`-based reader to remove the page-fault and `madvise` kernel time, which is now about 40% of the CPU.
