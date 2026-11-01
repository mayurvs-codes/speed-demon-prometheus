# Track B: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board result, all tuning steps combined (commit `bbb8ff3`): run `run-6e0521f9cea3`.** T = 0.411 s, M = 290 MB, K = 12.9, 55.5 points; every gate passed. The Run ID, T and M columns in the step table below are still TODO: the steps were tuned locally and have not each been measured on the board yet, so they are left blank rather than guessed. The "Local measurements" section is from one laptop and is only for comparing changes against each other.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2200 | 0 |
| 1 | Terms of up to 16 bytes live inline in a compact table (no hash pass, no `memcmp`, no pointer chase) | TODO | TODO | TODO | |
| 2 | AVX2 tokenizer | TODO | TODO | TODO | |
| 3 | Map the corpus without `MAP_POPULATE` | TODO | TODO | TODO | |
| 4 | Very common terms fill a bitmap directly (no pairs, no posting lists) | TODO | TODO | TODO | |
| 5 | Hand finished corpus pages back with `madvise(MADV_DONTNEED)` every 4 MB | TODO | TODO | TODO | |
| 6 | Count and sum BOOL results per 64-bit word with popcounts | TODO | TODO | TODO | |
| 7 | Phrase tags as `u16` ranks (generic, falls back to `u32`) | TODO | TODO | TODO | |
| 8 | Stop a BOOL query early when its result is empty and no OR remains | TODO | TODO | TODO | |

## Why each change helped

1. **Terms of up to 16 bytes live inline in a compact table (no hash pass, no `memcmp`, no pointer chase).** The old lookup hashed the bytes, probed, then followed a pointer to compare, which was a cache miss for 79% of all tokens.
2. **AVX2 tokenizer.** The per-byte loop cost about 190 instructions per token; classifying 32 bytes at once and finding token edges with bit tricks cut instructions by 20%.
3. **Map the corpus without `MAP_POPULATE`.** The old flag pre-loaded all 254 MB in one thread before scanning; now each scan thread loads its own slice in parallel.
4. **Very common terms fill a bitmap directly (no pairs, no posting lists).** 369 terms made up 45% of all (term, document) pairs, which cost 12 bytes each; a bitmap row is 31 KB however often the term appears.
5. **Hand finished corpus pages back with `madvise(MADV_DONTNEED)` every 4 MB.** Each byte is read once, so the 254 MB input does not need to stay resident; the page cache is untouched.
6. **Count and sum BOOL results per 64-bit word with popcounts.** 88 million result bits were visited one at a time; a word is now counted and summed in a handful of instructions and only the first ten ids are collected bit by bit.
7. **Phrase tags as `u16` ranks (generic, falls back to `u32`).** The per-token sequence was the largest array (145 MB) and phrase matching reads it at random, so halving it halves the cache misses.
8. **Stop a BOOL query early when its result is empty and no OR remains.** About 23% of BOOL answers are empty; an empty set stays empty under AND and NOT, so the remaining work is skipped.

## Local measurements (AMD Ryzen 7 6800H laptop, 12 threads; the scoring box is an i5-13500, so these are directional only)

| Step | Time | Memory |
|---|---|---|
| 1 | 0.60 s to 0.48 s (-20%) | unchanged |
| 2 | 0.48 s to 0.44 s (-8%) | unchanged |
| 3 | 0.45 s to 0.43 s (-4%) | unchanged |
| 4 | time unchanged (CPU -5%) | RSS 708,796 KB to 600,248 KB (-15%) |
| 5 | time unchanged | RSS 600,124 KB to 355,664 KB (-41%) |
| 6 | 0.45 s to 0.43 s (about -2% to -4%, marginal) | unchanged |
| 7 | 0.47 s to 0.44 s (-6%) | RSS 354,824 KB to 284,124 KB (-20%) |
| 8 | mean -1.6% (marginal) | unchanged |

Overall (interleaved A/B on the full dataset): median 0.61 s to 0.40 s (-34%), peak RSS 707,900 KB to 284,868 KB (-60%).

## Tried and reverted (the log lives in `tuning/LOG_TRACK_B.md` outside the repo)

- **Length / first-byte filter.** Reject non-terms before hashing. 79% of tokens are query terms, so a filter could reject at most 21%.
- **`process::exit` after the output.** Skip the destructors. No gain; the kernel frees the memory either way.
- **Phrase pivot on the rarest term.** Check the phrase around its rarest word instead of its first. Instructions fell 8.5% but time did not: the phase is memory-latency-bound.
- **Skip all-zero words when extracting results.** Skip empty bitmap words. No change; the compiler already did it.
- **Sparse running set for BOOL (three thresholds).** Keep small results as sorted id lists. Unbounded: slower (+24% instructions). Limited to 128 or 512 ids: at most -2% wall time, inside the noise, for about 30 more lines.
- **Duplicate-query reuse.** Answer repeated query lines once. Only 135 of 50,000 lines repeat; hashing all lines cost as much as it saved.
- **Free pairs arrays earlier.** Drop each thread's pairs as soon as it finishes scattering. No change: the memory peak is at the end of the scan.
- **Positional index for phrase terms only.** Store only phrase tokens as (position, rank). Correct but larger (+9%) and no faster than the `u16` sequence.
- **Intersect common-word bitmaps first.** Speed up phrases made of very common words. Slower on a synthetic workload (0.24 s to 0.28 s); the real data has no such phrases.
- **`-C target-cpu=native`.** Tune code generation to the CPU. No gain; also moves the build onto the scoring box.
- **Profile-guided optimisation.** Build with the allowlisted `halloumee/speed-demon-rust` image. No gain (0.47 s to 0.47 s over 41 runs).
- **Thread count.** Fewer than 12 threads. 12 is fastest.

## Memory as the scorer counts it (cgroup `memory.peak`)

The RSS figures above come from `/usr/bin/time`. The scorer reads the container's kernel peak counter instead, so it was re-measured in the board's sandbox shape (Docker Desktop VM, full-scale seed 1, three runs per build): **544.1 MB** with the `madvise` step and **544.6 MB** with the call turned into a no-op. The two agree within run-to-run noise, so handing pages back lowers RSS (about 708 MB to 285 MB) but **does not lower `memory.peak`**: the file pages stay in the page cache, which the cgroup still charges. Treat the memory gain of the `madvise` step as unproven; the board's M column is the authority. This is a local re-measurement, not a board number.
