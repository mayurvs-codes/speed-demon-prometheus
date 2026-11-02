# Track C: Join

The task is in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md): parse a hostile 202 MB CSV of 3.32 million records, join each record to a region table and print per-country count, id sum, amount sum/min/max in exact cents, distinct names, name bytes and note bytes.

## How it works

1. **Regions.** A country code is three letters, so its base-26 number indexes a table of 26^3 entries. Walking that table in order gives the countries already sorted, with `_UNMATCHED` added last. Every region id then maps to its country with one array read.
2. **Safe cut points.** `records.csv` is mapped and cut into 12 chunks. Each chunk starts at a true record start: just after an LF, then an optional BOM, then a digit 1 to 9. A line inside a quoted note never starts with a digit (a generator guarantee), so a cut can never land inside quotes. To make sure, every chunk must stop exactly where the next one starts. If any doesn't, we parse the whole file again on one thread.
3. **Hand-written scanner.** For each record we skip an optional BOM and read the id. The name and the note go through one function that handles unquoted, quoted and `""`-escaped text; it copies bytes only when an escape occurs and finds quotes and commas 32 bytes at a time with AVX2. Then we read the region id and the amount in exact integer cents, and consume CRLF or LF. The record is folded into its country's sums. Pages behind each thread are handed back every 256 KB, so only about 3 MB of the file is resident at once.
4. **Distinct names without storing records.** The 64-bit hash of the unescaped name goes into one lock-free table shared by all threads. The first thread to see a hash claims a slot with a compare-and-swap and gets the next dense id. The record then sets the bit (name id, country), and at the end the bits per country are counted.
   - **Prefetching:** the three random reads (table slot, id, bit) are pipelined across records. A record's slot is prefetched when it is parsed, its id is read one record later, and its bit is set one record after that.
   - **Table size:** if more than half the table fills, the file is parsed again with a table 8 times larger, so a bigger name pool stays correct.

## Correctness

This is the correctness track. Cases handled:
- **File format:** a BOM at the file start and at record starts mid-file; mixed CRLF and LF.
- **Quoted fields:** commas, `""` escapes and embedded LFs inside quotes.
- **Names:** an empty unquoted and an empty quoted name count as the same value; trailing spaces make distinct names.
- **Amounts:** in exact cents, with no floating point; negative amounts above -1.00 print as `-0.05`.
- **Groups:** unknown regions inside and above the table's range go to `_UNMATCHED`; a country with no records prints no line.

Names are told apart by their 64-bit hash. With about 100,000 names, the chance of two different names colliding is about 1 in 10^9. We check the full-scale hash, the sample hash and fresh tiny and sample seeds against `harness/reference/c_join.py`. The single-thread fallback and the larger-table restart were both forced and tested.

## Build and check

Rust with the standard library only, built offline from allowlisted images pinned by digest, with `-C target-cpu=x86-64-v3`.

```
python3 check.py c --data <data folder> --expected d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536
```

## Dependencies

None. `mmap` and `madvise` are declared with `extern "C"`, because the standard library has no memory map.

## Results

On the board: 0.0623 s, 13.4 MB, 68.4 points (`run-92aa3f276dfb`). The earlier version sorted an 8-byte key per record and ran at 0.057 s, 59.7 MB and 63.6 points. [PERF.md](PERF.md) lists every change with its board run, including the ones we reverted.

## Next steps

Counting distinct names is about half of the run (about 28 ms of 60 locally). Block reads instead of the map, and using the table slot as the name id, were not faster on the board. A small per-thread filter for (name, country) pairs seen recently is the next thing to try.

*These docs were drafted with an AI assistant, which the event rules allow. Every step above maps to a function in `src/main.rs`.*
