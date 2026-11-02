# Track B: Index

The task is in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md): a 254 MB corpus of 250,000 documents and 50,000 queries (boolean `AND`/`OR`/`NOT` evaluated strictly left to right, and exact phrases). For each query we print the match count, the sum of the matching ids and the ten smallest ids.

## How it works

1. **Index only what the queries ask for.** We read the queries first and give each distinct word an id (75,747 terms). A term of up to 16 bytes sits inline in an open-addressing table as two 64-bit numbers, so a lookup is a multiply, a shift and two integer compares.
2. **Sample, then plan.** A quick pass over the first 1/32 of each thread's slice estimates how many documents contain each term. We use it in three ways:
   - Terms are renumbered by falling frequency, so the common words' state sits together in cache.
   - Every term used in a boolean query that appears in at least one bitmap word's worth of documents gets its own bitmap.
   - Each phrase is anchored on its rarest term.
3. **Scan in parallel.** The corpus is cut into 12 slices of whole documents. Each thread tokenizes 32 bytes at a time with AVX2: one mask marks letter and digit bytes, and its edges give the start and end of every token. A token's key comes from one 16-byte load, lowercased and masked to its length. A document's tokens are looked up together, after prefetching their table slots, so the cache misses overlap.
4. **Record matches.** For a frequent term, the document sets a bit in that term's bitmap. For any other term in a boolean query, we record one (term, document) pair as a 2-byte term id; the rare ids above 32,766 take two u16s. Phrases are checked as each document ends, by comparing the term-id sequence around each occurrence of the phrase's anchor.
5. **Posting lists.** A counting sort turns each thread's pairs into per-term lists of 16-bit offsets from the slice's first document. That works because a slice holds at most 65,536 documents; if one holds more, the scan runs again with twice as many slices.
6. **Answer.** A boolean query runs left to right on a document bitmap:
   - A bitmap term is a word-wise AND/OR/AND-NOT.
   - A list term touches only its own documents.
   - An empty result stops early when no OR is left.
   - The count and the id sum come from popcounts, and only the first ten ids are listed one by one.

## Correctness

The tokenizer follows the spec: ASCII lowercase only, and a token is a maximal run of `a-z0-9`. Every other byte, including all bytes of 0x80 and above, separates tokens. There is no operator precedence, and `NOT` is binary. Phrases ignore punctuation and may cross sentences but never documents. Terms of 17 to 32 bytes are rare and live in a small `HashMap`. Both fallbacks were forced and tested: the slice re-split, and the two-word encoding for large term ids. We check the full-scale hash, the sample hash and fresh tiny and sample seeds against `harness/reference/b_index.py`.

## Build and check

Rust with the standard library only, built offline from allowlisted images pinned by digest, with `-C target-cpu=x86-64-v3`.

```
python3 check.py b --data <data folder> --expected f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927
```

## Dependencies

None. `mmap` and `madvise` are declared with `extern "C"`, because the standard library has no memory map. Pages behind each thread are handed back every 256 KB.

## Results

On the board: 0.260 s, 111 MB, 64.4 points (`run-95182e864c34`). [PERF.md](PERF.md) lists every change with its board run, including the ones we reverted.

## Next steps

The scan is about 70% of the time. The pair buffer (about 24 MB) is still alive while the lists are built, and the per-thread term state is 11 MB. A more compact scan state, and sparse boolean evaluation for queries whose result becomes small, are the next levers.

*These docs were drafted with an AI assistant, which the event rules allow. Every step above maps to a function in `src/main.rs`.*
