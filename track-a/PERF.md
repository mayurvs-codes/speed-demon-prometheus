# Track A: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board result, all tuning steps combined (commit `bbb8ff3`): run `run-abca22fa67d2`.** T = 0.065 s, M = 76 MB, K = 20.6, 65.7 points; every gate passed. The Run ID, T and M columns in the step table below are still TODO: the steps were tuned locally and have not each been measured on the board yet, so they are left blank rather than guessed. The "Local measurements" section is from one laptop and is only for comparing changes against each other.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Map the log without `MAP_POPULATE` | TODO | TODO | TODO | |
| 2 | Hand finished log pages back with `madvise(MADV_DONTNEED)` every 4 MB | TODO | TODO | TODO | |
| 3 | Parallel counting sort of the latencies | TODO | TODO | TODO | |
| 4 | Find the end of each endpoint name 32 bytes at a time (AVX2) | TODO | TODO | TODO | |

## Why each change helped

1. **Map the log without `MAP_POPULATE`.** The old flag made the main thread pre-load all 239 MB before any parsing started; now each parse thread loads its own part in parallel.
2. **Hand finished log pages back with `madvise(MADV_DONTNEED)` every 4 MB.** Every byte of the input is read once, so there is no reason to keep it resident; the file stays in the page cache, only this process's mapping shrinks.
3. **Parallel counting sort of the latencies.** Counting and scattering 3.1 million records were two single-threaded passes (a third of the runtime); now each chunk counts and scatters its own records, and even the start-position arithmetic runs in parallel over ranges of buckets.
4. **Find the end of each endpoint name 32 bytes at a time (AVX2).** Names average about 25 bytes and were walked byte by byte for every one of the 3.1 million lines.

## Local measurements (AMD Ryzen 7 6800H laptop, 12 threads; the scoring box is an i5-13500, so these are directional only)

| Step | Time | Memory |
|---|---|---|
| 1 | 0.120 s to 0.101 s (-16%) | RSS unchanged |
| 2 | time 0.101 s to 0.096 s | RSS 296,792 KB to 150,760 KB (-49%) |
| 3 | 0.096 s to 0.081 s (-16%) | RSS +0.4 MB |
| 4 | 0.081 s to 0.077 s (-6%) | unchanged |

Overall (interleaved A/B on the full dataset): median 0.120 s to 0.077 s (-36%), peak RSS 297,412 KB to 150,900 KB (-49%).

## Tried and reverted (the log lives in `tuning/LOG_TRACK_A.md` outside the repo)

- **First two parallel counting-sort variants.** Parallel counting with the start positions still computed by one thread. No gain (0.096 s to 0.096 s, then 0.094 s): the single-threaded start-position loop still took about 17 ms; the third variant parallelised it and is the one kept.
- **Inline name comparison.** Compare endpoint names with an inline word-by-word loop instead of `memcmp`. No gain (0.080 s to 0.081 s).
- **Thread count.** Fewer than 12 threads. 12 threads is as fast as 8 or 10 and faster than fewer; no change.
- **Reserved output buffers.** Reserve each hour's output buffer up front. No gain (0.077 s to 0.078 s).

## Memory as the scorer counts it (cgroup `memory.peak`)

The RSS figures above come from `/usr/bin/time`. The scorer reads the container's kernel peak counter instead, so it was re-measured in the board's sandbox shape (Docker Desktop VM, full-scale seed 1, three runs per build): **307.8 MB** with the `madvise` step and **307.7 MB** with the call turned into a no-op. The two agree within run-to-run noise, so handing pages back lowers RSS (about 297 MB to 151 MB) but **does not lower `memory.peak`**: the file pages stay in the page cache, which the cgroup still charges. Treat the memory gain of the `madvise` step as unproven; the board's M column is the authority. This is a local re-measurement, not a board number.
