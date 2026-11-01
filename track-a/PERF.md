# Track A: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board result, all tuning steps combined (commit `bbb8ff3`): run `run-abca22fa67d2`.** T = 0.065 s, M = 76 MB, K = 20.6, 65.7 points; every gate passed. The Run ID, T and M columns in the step table below are still TODO: the steps were tuned locally and have not each been measured on the board yet, so they are left blank rather than guessed. The "Local measurements" section is from one laptop and is only for comparing changes against each other.

**Latest board run (code of commit `8f492c2`): `run-bf3e42893655`.** T = 0.0562 s, M = 68.9 MB, S = 360, K = 22.94, 68.03 points; every gate passed. Board noise on identical Track A code is about 5% in T (0.0562 to 0.0615 s over four runs of this code).

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Map the log without `MAP_POPULATE` | TODO | TODO | TODO | |
| 2 | Hand finished log pages back with `madvise(MADV_DONTNEED)` every 4 MB | TODO | TODO | TODO | |
| 3 | Parallel counting sort of the latencies | TODO | TODO | TODO | |
| 4 | Find the end of each endpoint name 32 bytes at a time (AVX2) | TODO | TODO | TODO | |
| 5 | Tell endpoint names apart by their 64-bit hash alone (no `memcmp`, no `memcpy` in the hash) | run-b237de1bd35f | 0.0593 | 77.7 | 66.84 |
| 6 | 8-byte records: the endpoint id, hour and 5xx flag share one `u32` | run-b10da1d9fb74 | 0.0579 | 66.3 | 67.82 |
| 7 | Remove the `madvise` page release (step 2): the board's M does not count mapped file pages | run-4ee67ae87f66 | 0.0573 | 65.8 | 68.03 |
| 8 | Compact u32 bucket counters, AtomicU32 error counters, early drop of chunks/pos before formatting, fast hour parsing | TODO | TODO | TODO | |

## Why each change helped

1. **Map the log without `MAP_POPULATE`.** The old flag made the main thread pre-load all 239 MB before any parsing started; now each parse thread loads its own part in parallel.
2. **Hand finished log pages back with `madvise(MADV_DONTNEED)` every 4 MB.** Every byte of the input is read once, so there is no reason to keep it resident; the file stays in the page cache, only this process's mapping shrinks.
3. **Parallel counting sort of the latencies.** Counting and scattering 3.1 million records were two single-threaded passes (a third of the runtime); now each chunk counts and scatters its own records, and even the start-position arithmetic runs in parallel over ranges of buckets.
4. **Find the end of each endpoint name 32 bytes at a time (AVX2).** Names average about 25 bytes and were walked byte by byte for every one of the 3.1 million lines.
5. **Tell endpoint names apart by their 64-bit hash alone.** Every lookup compared the whole name again with `memcmp` (a library call and a read of the stored name, 26% of the cycles in a profile); with 1,842 names a 64-bit hash collision is about as likely as 1 in 10^13, and the hash no longer copies the name's tail. Board 0.0614 s to 0.0593 s; CPU time -14% on the laptop.
6. **8-byte records.** A record held the endpoint id, hour, flag and latency in 12 bytes; the id (24 bits), hour (7 bits) and flag (1 bit) now share one `u32`, so the 3.1 million records take 25 MB instead of 37 MB. Board M 77.7 MB to 66.3 MB.
7. **Remove `madvise`.** Steps 2's page release never changed the board's M (66.3 MB with it, 65.8 MB without), so it was dead code: removing it saved 7 lines and made no difference to the time.
8. **Compact u32 bucket counters, AtomicU32 error counters, and early drops.** Halved per-thread bucket counters from `[u32; 2]` (12.7 MB) to `u32` (6.3 MB), replaced per-thread error tracking with a single 530 KB AtomicU32 array, and dropped chunks and pos arrays before formatting so the 10 MB output buffer does not stack on top of chunk memory. Local cgroup peak dropped from 63.3 MB to 46.5 MB (-26.5%) and wall time improved to 0.0685 s (-11.9%).

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
- **Release pages every 512 KB instead of 4 MB.** More `madvise` calls: M unchanged (65.4 MB) and 0.0692 s against 0.0593 s on the board (run-f14ef337efbd), so it was slower.

## Memory as the scorer counts it (cgroup `memory.peak`)

The RSS figures above come from `/usr/bin/time`. The scorer reads the container's kernel peak counter instead, so it was re-measured in the board's sandbox shape (Docker Desktop VM, full-scale seed 1, three runs per build): **307.8 MB** with the `madvise` step and **307.7 MB** with the call turned into a no-op. The two agree within run-to-run noise, so handing pages back lowers RSS (about 297 MB to 151 MB) but **does not lower `memory.peak`**: the file pages stay in the page cache, which the cgroup still charges. Treat the memory gain of the `madvise` step as unproven; the board's M column is the authority. This is a local re-measurement, not a board number.
