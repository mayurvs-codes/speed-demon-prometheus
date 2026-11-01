> **Owner must rewrite in their own words.** This is a draft written from the tuning log. The rules require a human to understand every optimisation, so read each section, check it against `src/main.rs`, and rewrite it so it is yours.

# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

All 50,000 queries are known before the corpus is read, so only the terms they mention are indexed (75,747 terms, of which 33,087 appear in phrases). The queries are read first and each distinct term gets an id; the terms go into an open-addressing table where a term of up to 16 bytes is stored inline as two 64-bit integers, so a lookup is a multiply, a shift and two integer compares.

The corpus is cut into 12 slices. Each thread tokenizes its documents with AVX2: 32 bytes are classified at once as token bytes (letters and digits) or separators, and bit tricks give the start and end of every token. A token's key is built with one 16-byte load, lowercased and masked to its length. Every token is looked up in the term table. For each document and query term one (term, document) pair is recorded (or, for very common terms, a bit is set directly in a per-term bitmap, which needs no pairs and no posting list). For phrase terms the thread also stores a 2-byte rank per token position, so a phrase can later be checked in place.

Pairs are counting-sorted into per-term posting lists (slices are in document order, so each list comes out ascending). BOOL queries run strictly left to right on a document bitmap: a frequent term is a bitmap AND/OR/ANDNOT over 3,907 words, a rare term touches only its list; an empty result stops early when no OR is left. The count and the id sum come from popcounts, and only the first ten ids are collected bit by bit. PHRASE queries walk the documents of their rarest term and look for the phrase in the stored rank sequences.

## Correctness

The tokenizer follows the spec exactly: ASCII lowercase only, a token is a maximal run of `a-z0-9`, every other byte (including all bytes of 0x80 and above) is a separator, so `Zurich` written with a u-umlaut is two tokens. BOOL has no operator precedence; `NOT` is binary. Phrases ignore punctuation and may cross sentence boundaries but never documents. Terms of 17 to 32 bytes (very rare) are kept in a small `HashMap`. The tag width is generic: `u16` ranks when there are fewer than 65,536 phrase terms, `u32` otherwise, so the answer never depends on the vocabulary size (the `u32` path was forced and tested). Checked with the full-scale and sample hashes and tiny seeds 1 to 60 against `harness/reference/b_index.py`.

## Build and check

Rust, standard library only. The Dockerfile builds offline from an allowlisted image pinned by digest, with `-C target-cpu=x86-64-v3`. The output file is `results.txt`.

```
python3 check.py b --data <data folder> --expected f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927
```

Full-scale seed-1 output hash: `f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927`.

## Dependencies

None. The memory-map calls (`mmap`, `madvise`) are declared directly with `extern "C"` because the standard library has no memory map.

## Results

Median 0.61 s to 0.40 s (-34%), peak RSS 707,900 KB to 284,868 KB (-60%) (local laptop numbers; see PERF.md for the per-change log and the board runs once filled in).

## With another 24 hours

Bidirectional or sparse BOOL evaluation only pays if the set operations (about 4% of CPU) grow; a pread-based reader would remove most of the kernel time; the phrase phase is memory-latency-bound, so a more compact sequence layout is the next lever.
