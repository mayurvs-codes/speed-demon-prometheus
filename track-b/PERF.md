# Track B: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way the log says it did.

**Board result, all tuning steps combined (commit `bbb8ff3`): run `run-6e0521f9cea3`.** T = 0.411 s, M = 290 MB, K = 12.9, 55.5 points; every gate passed. The Run ID, T and M columns in the step table below are still TODO: the steps were tuned locally and have not each been measured on the board yet, so they are left blank rather than guessed. The "Local measurements" section is from one laptop and is only for comparing changes against each other.

**Latest board run (code of commit `8f492c2`): `run-68b6c247ebcc`.** T = 0.313 s, M = 212 MB, S = 629, K = 16.04, 60.25 points; every gate passed. Board noise on identical Track B code: T 0.313 to 0.346 s, M 177 to 212 MB over eight runs.

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
| 9 | Write all result lines with one `write` | run-5be09f50b1ae | 0.334 | 309 | 57.74 |
| 10 | Check a phrase only in documents that hold all its terms | run-f4f6d4549d46 | 0.313 | 307 | 58.43 |
| 11 | Match phrases while scanning, anchored on their rarest term; no per-token array, and terms used by no BOOL query keep no postings | run-24a01816d67c | 0.326 | 233 | 59.32 |
| 12 | Each scan thread groups its own postings by term (no separate posting phase, no global posting array) | run-83109bd12e4b | 0.329 | 208 | 59.78 |

## Why each change helped

1. **Terms of up to 16 bytes live inline in a compact table (no hash pass, no `memcmp`, no pointer chase).** The old lookup hashed the bytes, probed, then followed a pointer to compare, which was a cache miss for 79% of all tokens.
2. **AVX2 tokenizer.** The per-byte loop cost about 190 instructions per token; classifying 32 bytes at once and finding token edges with bit tricks cut instructions by 20%.
3. **Map the corpus without `MAP_POPULATE`.** The old flag pre-loaded all 254 MB in one thread before scanning; now each scan thread loads its own slice in parallel.
4. **Very common terms fill a bitmap directly (no pairs, no posting lists).** 369 terms made up 45% of all (term, document) pairs, which cost 12 bytes each; a bitmap row is 31 KB however often the term appears.
5. **Hand finished corpus pages back with `madvise(MADV_DONTNEED)` every 4 MB.** Each byte is read once, so the 254 MB input does not need to stay resident; the page cache is untouched.
6. **Count and sum BOOL results per 64-bit word with popcounts.** 88 million result bits were visited one at a time; a word is now counted and summed in a handful of instructions and only the first ten ids are collected bit by bit.
7. **Phrase tags as `u16` ranks (generic, falls back to `u32`).** The per-token sequence was the largest array (145 MB) and phrase matching reads it at random, so halving it halves the cache misses.
8. **Stop a BOOL query early when its result is empty and no OR remains.** About 23% of BOOL answers are empty; an empty set stays empty under AND and NOT, so the remaining work is skipped.
9. **Write all result lines with one `write`.** The 50,000 lines were written with one system call each; they are now joined and written once (board 0.411 s to 0.334 s).
10. **Check a phrase only in documents that hold all its terms.** The old code scanned the token sequence of every document that holds the rarest word; most of them lack another word, which a bitmap bit test or a binary search in a posting list rules out first (about 4 of 5 candidates dropped; board 0.334 s to 0.313 s).
11. **Match phrases while scanning, anchored on their rarest term.** The 36 million-entry token array (73 MB) existed only to check phrases afterwards. Each phrase is now anchored on the term the sample saw least; when that term occurs in a document, the few tokens around it are compared with the phrase right there, while the document's term ids are still in a small buffer. Memory fell 307 MB to 233 MB; time rose slightly on the board (0.313 s to 0.326 s) because the scan does a little more work per token, so the gain is in M.
12. **Each scan thread groups its own postings by term.** The (term, document) pairs were scattered into one global array by a separate phase; each thread now sorts its own pairs by term right after its scan (a counting sort over its own small array), and a term's list is read as one short run per thread. The separate phase and the 44 MB global array are gone; memory fell 233 MB to 208 MB, time stayed flat on the board.

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

- **Remove the `madvise` page release.** The board's M does not count mapped file pages (see Track A), so the call looked dead. Without it the board run was slower (run-4786155891e8: 0.353 s, 59.20 points against about 0.33 s and 59.7 to 60.0 with it, same code otherwise); the likely reason is that the unmap at exit has to drop 254 MB of still-mapped pages. Put back.

- **Prefetch a document's table slots before the lookups.** Collect a document's token keys, prefetch their slots, then look them up. 7% faster on the laptop, no change on the board (run-aa7e06dd6e42: 0.336 s against 0.334 s, 57.58 points against 57.74); the scoring box's larger L2 cache already holds the table.

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

## Local-only steps after run-68b6c247ebcc (not yet measured on the board)

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 13 | A (term, document) pair is stored as the 4-byte term id, with a marker closing each document; the document id follows from position (ids are dense). Local peak RSS 191 -> 155 MB | see combined run below | | | |
| 14 | Corpus pages are handed back every 1 MB instead of every 4 MB. Local peak RSS 156 -> 140 MB | `run-cf8b774d511b` (steps 13 and 14 together, commit `319d3c8`) | 0.328 | 160 | 60.69 |

Local A/B of both steps together against commit 0934019 (Docker, volume copy of b_full, 11 interleaved runs, median): 0.403 -> 0.381 s, peak RSS 192,568 -> 138,820 KB. Docker on Windows is noisy (single runs 0.33 to 0.52 s), so only the memory change is clear; the time difference is inside the noise.

Board result of steps 13 and 14 together (commit `319d3c8`): run `run-cf8b774d511b`, T = 0.328 s (was 0.356 s), M = 160 MB (was 195 MB), 60.69 points (was 59.00); every gate passed. The time change is small and may be noise; the memory drop is real.

## Time steps after run-cf8b774d511b (local only, board runs pending)

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 15 | After the sampling pass, terms are numbered by falling sampled frequency (vocabulary table and queries are renumbered). The few thousand common words that make up most tokens now keep their per-term state next to each other, so it stays in cache. Local: wall -7%, CPU -11% | see combined run below | | | |
| 16 | The scan only tokenizes and looks terms up while it reads a document; recording the document's terms (last-seen stamp, bitmap bit or pair) is a second loop over the document's term ids, with each term's state (now one 12-byte struct) prefetched 8 tokens ahead. Local: wall -13%, CPU -13% | `run-a94d0c6c64fc` (steps 15 and 16 together, commit `a05dc18`) | 0.301 | 142 | 61.95 |

Local A/B of steps 15 and 16 against commit `d99dcc3` (Docker, volume copy of b_full, 15 interleaved runs, median): wall 0.373 -> 0.327 s (-12%), CPU (user+sys) 2.758 -> 2.227 s (-19%), peak RSS 139,116 -> 140,988 KB (+1.4%). Docker on Windows is noisy (single runs 0.27 to 0.47 s), so these are directional; the board decides. Tried and dropped: prefetch in the posting grouping (+5% slower), plain stores instead of atomic bitmap updates (-1.3%), a smaller sampling pass (-1.5%).

Board result of steps 15 and 16 together (commit `a05dc18`): run `run-a94d0c6c64fc`, T = 0.301 s (was 0.328 s), M = 142 MB (was 160 MB), 61.95 points (was 60.69); every gate passed. Locally the steps gave -12% wall; on the board the gain was about -8%.

## Phrase anchors found in the recording loop (commit `ed0831c`)

The phrase check used to be a second pass over every token of a document, although a term that anchors a phrase is almost never present (under 1 million of 37 million tokens). A flag in each term's state now marks anchor terms, and the recording loop, which already loads that state, collects their positions, so only those positions are checked. Measured inside the container on the full data, 15 runs each: median 262 ms before, 252 ms after (about 4% less). On the board the change is inside the noise: `run-edc1a10ffd2f` 0.3011 s, 143.6 MB, 61.86 points, against 0.304 to 0.306 s and 61.7 to 61.9 points before.

## Tried on the board and reverted

- **Block reads instead of mmap** (commit `30489b9`, `run-6ddc8743c1a8`): 0.3493 s, 60.03 points, against about 0.30 s with the mapped file, so it was slower and went back.
- **Where the time goes** (local profile, full data): about 20 ms to parse the queries and build the vocabulary, 7 ms for the sample pass, 170 ms for the scan, 40 ms to answer the queries. In the scan, tokenizing is about 45% of the cycles, recording terms about 30%, grouping about 5%. A cheaper hash for the query words and renumbering the vocabulary in place saved under 5 ms in the container, so they were not kept at the time (step 19 below revisits the query parse).

## Board-measured steps on Nov 2

| Step | Change | Commit | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| before | Mapped corpus, phrase anchors in the recording loop | `9641f29` | `run-d740b9c63b04` | 0.2812 | 148.2 | 62.67 |
| 17 | Posting lists hold 16-bit offsets from their slice's first document instead of 32-bit ids, which halves the 44 MB of lists. A slice with more than 65,536 documents makes the scan run again with twice as many slices, so the format never depends on the data's size. Corpus pages are also handed back every 256 KB instead of every 1 MB. Local peak RSS 152 to 132 MB. | `515731d` (measured in `950c34b`) | `run-3a677d0e7b93` | 0.2807 | 131.3 | 63.13 |
| 18 | A document's tokens are looked up after the whole document is read. While tokenizing, each token's vocabulary slot is prefetched, so the ~170 cache misses per document overlap instead of happening one at a time. Local median 307 to 268 ms; on the board the time did not move. | `891d446` | `run-13368dd8633a` | 0.2808 | 128.0 | 63.18 |
| 19 | Query parse: vocabulary words are slices of the mapped queries file (no copy per word), the word map uses a multiply-xor hash, and the query list is sized from the line count up front. In the container this parse made ~3,900 page faults (fresh heap) and took about 31 ms of a ~225 ms run; it is now about 29 ms with ~3,000 faults. | `6926e1d` | `run-fb71e3f4e876` | 0.2738 | 128.6 | 63.34 |
| 20 | A term gets a bitmap row from 1 bitmap-word's worth of sampled documents instead of 2: more terms set a bit in the scan instead of recording a (term, document) pair, and their queries are bitmap operations. | `6d263ad` | `run-59e5a59b469e` | 0.2670 | 133.3 | 63.45 |

Memory now: posting lists 22 MB, the (term, document) pair buffer 44 MB (alive until each thread has grouped its pairs), bitmaps for 377 frequent terms 12 MB. The pair buffer is the next thing to remove.
