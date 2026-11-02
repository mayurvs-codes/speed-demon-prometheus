# Track C: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board result, all tuning steps combined (commit `bbb8ff3`): run `run-96b55f9e626c`.** T = 0.067 s, M = 60 MB, K = 17.2, 61.8 points; every gate passed. The Run ID, T and M columns in the step table below are still TODO: the steps were tuned locally and have not each been measured on the board yet, so they are left blank rather than guessed. The "Local measurements" section is from one laptop and is only for comparing changes against each other.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Build the region table without sorting 200,000 codes | TODO | TODO | TODO | |
| 2 | Map the input without `MAP_POPULATE` | TODO | TODO | TODO | |
| 3 | Hand finished input pages back with `madvise(MADV_DONTNEED)` every 4 MB | TODO | TODO | TODO | |
| 4 | Find quotes and commas 32 bytes at a time (AVX2) | TODO | TODO | TODO | |
| 5 | Reserve each shard's key capacity up front | TODO | TODO | TODO | |
| 6 | Compute unescaped note length directly without scratch buffer copies | TODO | TODO | TODO | |
| 7 | Build region table directly without intermediate region_rows vector | TODO | TODO | TODO | |

## Why each change helped

1. **Build the region table without sorting 200,000 codes.** The old code collected every region's code, sorted and de-duplicated them and binary-searched each one, all on one thread (29 ms of about 90 ms); a three-letter code is its own array index.
2. **Map the input without `MAP_POPULATE`.** The old flag pre-loaded all 204 MB in the main thread before parsing started; now each parse thread loads its own part in parallel.
3. **Hand finished input pages back with `madvise(MADV_DONTNEED)` every 4 MB.** Each byte is read once, so the input does not need to stay resident; the page cache is untouched.
4. **Find quotes and commas 32 bytes at a time (AVX2).** Notes average about 60 bytes and were walked byte by byte to find the closing quote.
5. **Reserve each shard's key capacity up front.** The 144 key vectors grew by doubling, which means repeated reallocation, copying and page faults.
6. **Compute unescaped note length directly without scratch buffer copies.** Notes average ~60 bytes and frequently contain quotes; since notes are not hashed, their unescaped length is directly calculated as raw byte length minus quote escape count, avoiding scratch buffer copies and reallocations across 3.32M records.
7. **Build region table directly without intermediate region_rows vector.** Scanning `regions.csv` in two fast passes avoids allocating and repeatedly doubling the 200,000-element `Vec<(usize, usize)>` intermediate vector (saving ~3.2 MB heap allocation and lowering cgroup peak memory).

## Local measurements (AMD Ryzen 7 6800H laptop, 12 threads; the scoring box is an i5-13500, so these are directional only)

| Step | Time | Memory |
|---|---|---|
| 1 | 0.11 s to 0.09 s (-22%) | unchanged |
| 2 | 0.09 s to 0.07 s (-19%) | unchanged |
| 3 | 0.072 s to 0.065 s | RSS 263,040 KB to 69,988 KB (-73%) |
| 4 | 0.067 s to 0.062 s (-7%) | unchanged |
| 5 | 0.063 s to 0.059 s (-6%) | RSS -5% |
| 6 | 0.078 s to 0.065 s (-15.7%) | unchanged |
| 7 | 0.065 s to 0.064 s (-2.6%) | Cgroup peak 59.6 MB to 56.8 MB (-4.8%) |

Overall (interleaved A/B on the full dataset): median 0.10 s to 0.059 s (-41%; mean 0.110 s to 0.059 s), peak RSS 267,992 KB to about 66,000 KB (-75%).
With steps 6-7 on Docker volume copy of c_full (7-run median): time 0.078 s -> 0.0637 s (-18.3%), cgroup peak 60.3 MB -> 56.8 MB (-5.8%).

## Tried and reverted (the log lives in `tuning/LOG_TRACK_C.md` outside the repo)

- **Thread count.** Fewer than 12 threads. 12 threads is fastest (57-61 ms against 62-64 for 10).
- **Coarser page release.** Release pages every 16 MB instead of 4 MB. No faster and RSS rose from 67 MB to 185 MB.
- **Serial shard key consolidation.** Moving keys to shard vectors serially in the main thread before deduplicating added ~30 ms.
- **Manual unchecked loops for integer parsing.** Slower than compiler-vectorized slice methods.
- **Shard key collection with exact capacity + extend_from_slice.** Gained 1.4% (under 3% threshold).
- **Tighter per-shard key vector reservation.** Gained 1.7% (under 3% threshold).

## Memory as the scorer counts it (cgroup `memory.peak`)

The RSS figures above come from `/usr/bin/time`. The scorer reads the container's kernel peak counter instead, so it was re-measured in the board's sandbox shape (Docker Desktop VM, full-scale seed 1, three runs per build): **263.8 MB** with the `madvise` step and **266.8 MB** with the call turned into a no-op. The two agree within run-to-run noise, so handing pages back lowers RSS (about 268 MB to 67 MB) but **does not lower `memory.peak`**: the file pages stay in the page cache, which the cgroup still charges. Treat the memory gain of the `madvise` step as unproven; the board's M column is the authority. This is a local re-measurement, not a board number.

## Board-measured steps on Nov 2

| Step | Change | Commit | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| before | 8-byte (name hash, country) key per record, sharded, then sorted and deduplicated | `8c2edb6` | `run-353012c7f3ac` | 0.0570 | 59.7 | 63.64 |
| 8 | Distinct names via a shared lock-free name table: the first thread to see a name's 64-bit hash claims a slot with a compare-and-swap and gets the next dense id; each record then sets one bit (name id, country). No per-record key and no sort. If more than half the table fills, the parse runs again with a table 8x larger, so a bigger name pool stays correct. Mapped pages are also handed back every 256 KB instead of every 4 MB: with 12 threads, 4 MB each kept ~48 MB of the file resident. Local peak RSS 62 to 26 MB. | `57ce137` | `run-29b44bcd6779` | 0.0726 | 13.9 | 66.86 |
| 9 | Each record now waited on up to three random reads (slot, id, bit). They are pipelined three records deep: the slot is prefetched when a record is parsed, its id is resolved one record later (prefetching its bit), and the bit is set one record after that. | `950c34b` | `run-951402a26383` | 0.0618 | 13.8 | 68.40 |
| 9, re-runs | Same code | `891d446`, `6926e1d` | `run-77109c342e8c`, `run-289269c5723a` | 0.1080, 0.0667 | 13.3, 14.0 | 62.50, 67.52 |

The 0.108 s run is an outlier: 15 local runs of the same build span 53-79 ms with no slow tail. Step 8 shows that releasing mapped pages more often does lower the board's M (it is not only the page cache), which corrects the note above. Still slower than the sort version: about 1.8M of the 3.3M (name, country) pairs are new, and each new bit is an atomic write to a cache line another core probably wrote last. A version with 4-byte keys sharded by name id, with non-atomic bits per shard, was tried locally: no faster, and 35 MB. Not pushed.

Also tried on the board and reverted: prefetching the name-table slot two records ahead (`6d263ad`, `run-de3072eff3e5`: 0.0790 s, 65.93 points), and prefetching the region's country with each record folded one record late (`694e15c`, `run-0eb57bc4db7d`: 0.0674 s, 67.42 points, the same as without it).
