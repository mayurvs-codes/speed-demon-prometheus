# Track C: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board results: TODO.** No run ID or board number has been recorded yet. Fill the Run ID, T and M columns from the live board after pushing; the "Local measurements" section below is from one laptop and is only for comparing changes against each other.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Build the region table without sorting 200,000 codes | TODO | TODO | TODO | |
| 2 | Map the input without `MAP_POPULATE` | TODO | TODO | TODO | |
| 3 | Hand finished input pages back with `madvise(MADV_DONTNEED)` every 4 MB | TODO | TODO | TODO | |
| 4 | Find quotes and commas 32 bytes at a time (AVX2) | TODO | TODO | TODO | |
| 5 | Reserve each shard's key capacity up front | TODO | TODO | TODO | |

## Why each change helped

1. **Build the region table without sorting 200,000 codes.** The old code collected every region's code, sorted and de-duplicated them and binary-searched each one, all on one thread (29 ms of about 90 ms); a three-letter code is its own array index.
2. **Map the input without `MAP_POPULATE`.** The old flag pre-loaded all 204 MB in the main thread before parsing started; now each parse thread loads its own part in parallel.
3. **Hand finished input pages back with `madvise(MADV_DONTNEED)` every 4 MB.** Each byte is read once, so the input does not need to stay resident; the page cache is untouched.
4. **Find quotes and commas 32 bytes at a time (AVX2).** Notes average about 60 bytes and were walked byte by byte to find the closing quote.
5. **Reserve each shard's key capacity up front.** The 144 key vectors grew by doubling, which means repeated reallocation, copying and page faults.

## Local measurements (AMD Ryzen 7 6800H laptop, 12 threads; the scoring box is an i5-13500, so these are directional only)

| Step | Time | Memory |
|---|---|---|
| 1 | 0.11 s to 0.09 s (-22%) | unchanged |
| 2 | 0.09 s to 0.07 s (-19%) | unchanged |
| 3 | 0.072 s to 0.065 s | RSS 263,040 KB to 69,988 KB (-73%) |
| 4 | 0.067 s to 0.062 s (-7%) | unchanged |
| 5 | 0.063 s to 0.059 s (-6%) | RSS -5% |

Overall (interleaved A/B on the full dataset): median 0.10 s to 0.059 s (-41%; mean 0.110 s to 0.059 s), peak RSS 267,992 KB to about 66,000 KB (-75%).

## Tried and reverted (the log lives in `tuning/LOG_TRACK_C.md` outside the repo)

- **Thread count.** Fewer than 12 threads. 12 threads is fastest (57-61 ms against 62-64 for 10).
- **Coarser page release.** Release pages every 16 MB instead of 4 MB. No faster and RSS rose from 67 MB to 185 MB.

## Memory as the scorer counts it (cgroup `memory.peak`)

The RSS figures above come from `/usr/bin/time`. The scorer reads the container's kernel peak counter instead, so it was re-measured in the board's sandbox shape (Docker Desktop VM, full-scale seed 1, three runs per build): **263.8 MB** with the `madvise` step and **266.8 MB** with the call turned into a no-op. The two agree within run-to-run noise, so handing pages back lowers RSS (about 268 MB to 67 MB) but **does not lower `memory.peak`**: the file pages stay in the page cache, which the cgroup still charges. Treat the memory gain of the `madvise` step as unproven; the board's M column is the authority. This is a local re-measurement, not a board number.
