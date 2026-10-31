# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

<!-- TODO after kickoff: how the solution works, in plain prose. Cover parsing/reading, the data
structures, how work is split across the 12 threads, and why each choice is fast. Write it so a
human reader can follow every optimisation: judges check that a person understood them. -->

## Correctness

<!-- TODO: which edge cases from the spec's edge-case list the solution handles, and how they were
tested (generator seeds and scales used, compared against the reference). -->

## Build and check

Language: Rust, standard library only. The Dockerfile builds offline from an allowlisted image.

```
python3 check.py a --data <data dir> --expected <sha256>
```

The output file is `summary.txt`.

## Dependencies

None.

## With another 24 hours

<!-- TODO: what you would do next. -->
