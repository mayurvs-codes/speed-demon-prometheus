> **Owner must rewrite in their own words.** This is a draft written from the tuning log. The rules require a human to understand every optimisation, so read each section, check it against `src/main.rs`, and rewrite it so it is yours.

# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

`records.csv` and `regions.csv` are mapped into memory. `regions.csv` becomes a dense table indexed by region id: a country code is three letters A-Z, so its base-26 number indexes a table of 26^3 entries directly, and walking that table in order gives the countries already sorted (with `_UNMATCHED` added last). Every region id then maps to its country index with one array read.

`records.csv` is cut into 12 chunks, each starting at a true record start. A record start is found as "just after an LF, then an optional BOM, then a digit 1 to 9": lines inside a quoted note never start with a digit (a guarantee of the generator), so this can never land inside quotes. Each thread parses its chunk with a hand-written scanner: skip an optional BOM, read the id, read the name and the note (one function handles quoted and unquoted text and `""` escapes, copying bytes only when an escape occurs), read the region id and the amount in exact cents, then consume a CRLF or LF. It folds the record into per-country sums (count, id sum, amount sum, min, max, name bytes, note bytes). Distinct names are counted without storing anything per record: the 64-bit hash of the unescaped name is interned in one shared lock-free table (the first thread to see it claims a slot with a compare-and-swap and gets the next dense id), and the record sets the bit (name id, country). The table, the id and the bit are reached through prefetches one and two records ahead. If more than half the table fills, the file is parsed again with a table 8 times larger, so a larger name pool stays correct. Mapped pages behind each thread are handed back every 256 KB.

Every chunk must stop exactly where the next one starts; if any does not, the whole file is parsed again on one thread, so a wrongly judged boundary can never corrupt the answer.

## Correctness

This is the correctness track. The cases handled: a BOM at the file start and mid-file at record starts only; CRLF and LF mixed; quoted fields with commas, `""` escapes and 1 to 3 LFs; empty unquoted and empty quoted fields as the same value; names with trailing spaces as distinct values; negative amounts below 1.00 printed as `-0.05`; zero amounts; unknown regions inside and above the lookup's range (`_UNMATCHED`); a country with no records (no line); amounts in exact integer cents with no floating point. Checked with the full-scale hash, the sample hash, and 20 tiny seeds against `harness/reference/c_join.py` (plus other seeds during development).

## Build and check

Rust, standard library only. The Dockerfile builds offline from an allowlisted image pinned by digest, with `-C target-cpu=x86-64-v3`. The output file is `aggregates.txt`.

```
python3 check.py c --data <data folder> --expected d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536
```

Full-scale seed-1 output hash: `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Dependencies

None. The memory-map calls (`mmap`, `madvise`) are declared directly with `extern "C"` because the standard library has no memory map.

## Results

On the board: 0.0623 s, 13.4 MB, 68.4 points (`run-92aa3f276dfb`), from 0.057 s, 59.7 MB and 63.6 points with sorted per-record keys. Earlier local laptop numbers: median 0.10 s to 0.059 s. PERF.md has every change with its run.

## With another 24 hours

Counting distinct names is about half the run locally (about 28 ms of 60). Block reads instead of the map, and using the table slot as the name id, were tried and were not faster on the board (see PERF.md). A per-thread pre-filter for repeated (name, country) pairs is the next thing to try.
