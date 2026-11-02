// Track A: Ingest.
// The log is cut into one byte range per thread; each thread reads its range in small blocks (no mapping)
// and appends each line's latency (4 bytes) and endpoint id with its 5xx flag (2 bytes) to that thread's lists for
// the line's hour. No other per-line data exists. Each thread then takes a run of hours: it groups the hour's
// lines by endpoint with a small counting sort, takes percentiles with select_nth_unstable and formats the hour.
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::thread;

const HOURS: usize = 72;

/// Bytes read per call; each thread keeps one buffer of this size (plus one partial line).
const BLOCK: usize = 512 << 10;

/// Start of the first line that begins at or after `guess`.
fn line_start(f: &File, guess: u64, len: u64) -> u64 {
    let mut buf = [0u8; 512];
    let mut at = guess;
    while at < len {
        let n = ((len - at) as usize).min(buf.len());
        f.read_exact_at(&mut buf[..n], at).expect("read input");
        if let Some(k) = buf[..n].iter().position(|&c| c == b'\n') {
            return at + k as u64 + 1;
        }
        at += n as u64;
    }
    len
}

/// A 64-bit hash of an endpoint name. The names of one file are told apart by this hash alone: 1,842 names make a
/// collision about as likely as 1 in 10^13.
#[inline(always)]
fn hash64(bytes: &[u8]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ (bytes.len() as u64).wrapping_mul(0xA24B_AED4_963E_E407);
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 32;
    }
    // The last 8 bytes (overlapping the words above), or the whole name when it is shorter than 8 bytes.
    let last = bytes.len().checked_sub(8).map_or_else(
        || bytes.iter().rev().fold(0, |a, &b| a << 8 | u64::from(b)),
        |n| u64::from_le_bytes(bytes[n..].try_into().unwrap()),
    );
    h = (h ^ last).wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 29;
    h = h.wrapping_mul(0x9FB2_1C65_1E98_DF25);
    h ^ (h >> 32)
}

/// Open-addressing set of endpoint strings, assigning dense ids in first-seen order.
struct Dict {
    slots: Vec<(u64, u32)>, // (hash, id + 1); 0 = empty
    names: Vec<Vec<u8>>,
}

impl Dict {
    fn new() -> Self {
        Dict { slots: vec![(0, 0); 8192], names: Vec::new() }
    }

    #[inline(always)]
    fn id(&mut self, name: &[u8]) -> u32 {
        if self.names.len() * 2 > self.slots.len() {
            self.grow();
        }
        let h = hash64(name);
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            match self.slots[i] {
                (0, 0) => {
                    self.slots[i] = (h, self.names.len() as u32 + 1);
                    self.names.push(name.to_vec());
                    return self.names.len() as u32 - 1;
                }
                (sh, sid) if sh == h => return sid - 1,
                _ => i = (i + 1) & mask,
            }
        }
    }

    fn grow(&mut self) {
        let mut slots = vec![(0, 0); self.slots.len() * 2];
        let mask = slots.len() - 1;
        for (id, name) in self.names.iter().enumerate() {
            let h = hash64(name);
            let mut i = h as usize & mask;
            while slots[i].1 != 0 {
                i = (i + 1) & mask;
            }
            slots[i] = (h, id as u32 + 1);
        }
        self.slots = slots;
    }
}

/// One thread's lines, split by hour: `lat[h][i]` and `ep[h][i]` describe the same line. `ep` holds the
/// thread-local endpoint id in the low 15 bits (a seed has 1,842 endpoints) and the 5xx flag in the top bit.
struct Chunk {
    lat: Vec<Vec<u32>>,
    ep: Vec<Vec<u16>>,
    dict: Dict,
}

/// Index of the first `needle` byte at or after `from`, looking at 32 bytes at a time.
#[inline(always)]
fn find(d: &[u8], from: usize, needle: u8) -> Option<usize> {
    let mut i = from;
    while i + 32 <= d.len() {
        let hits = unsafe {
            _mm256_cmpeq_epi8(
                _mm256_loadu_si256(d.as_ptr().add(i) as *const __m256i),
                _mm256_set1_epi8(needle as i8),
            )
        };
        let m = unsafe { _mm256_movemask_epi8(hits) } as u32;
        if m != 0 {
            return Some(i + m.trailing_zeros() as usize);
        }
        i += 32;
    }
    d[i..].iter().position(|&c| c == needle).map(|p| i + p)
}

#[inline(always)]
fn digit(b: u8) -> usize {
    usize::from(b - b'0')
}

/// Reduces the whole lines in `d` to records.
fn read_lines(d: &[u8], c: &mut Chunk) {
    let mut at = 0;
    while at < d.len() {
        // "YYYY-MM-DDTHH:MM:SS.ffffffZ " is 28 bytes; the hour index is counted from 2026-10-30T18.
        let day_index = if d[at + 6] == b'0' {
            digit(d[at + 9])
        } else {
            digit(d[at + 9]) + 1
        };
        let hour_of_day = digit(d[at + 11]) * 10 + digit(d[at + 12]);
        let hour = (day_index * 24 + hour_of_day - 18) as u8;
        at += 28 + 3; // method min len 3
        while d[at] != b' ' {
            at += 1; // method
        }
        at += 1;
        let name_start = at;
        at = find(d, at, b' ').expect("space after the endpoint");
        let name = &d[name_start..at];
        at += 1;
        let server_error = u8::from(d[at] == b'5');
        at += 4;
        let mut latency = 0u32;
        while d[at] != b'\n' {
            latency = latency * 10 + u32::from(d[at] - b'0');
            at += 1;
        }
        at += 1;
        let endpoint = c.dict.id(name) as u16;
        c.lat[hour as usize].push(latency);
        c.ep[hour as usize].push(endpoint | u16::from(server_error) << 15);
    }
}

/// Reads bytes `start..end` (which begin and end on line boundaries) in blocks and reduces them.
fn read_chunk(f: &File, start: u64, end: u64) -> Chunk {
    // Lines average ~77 bytes; reserve twice an hour's expected share (pages never written cost no memory).
    let per_hour = (end - start) as usize / 77 / HOURS * 2 + 64;
    let mut c = Chunk {
        lat: (0..HOURS).map(|_| Vec::with_capacity(per_hour)).collect(),
        ep: (0..HOURS).map(|_| Vec::with_capacity(per_hour)).collect(),
        dict: Dict::new(),
    };
    let mut buf = vec![0u8; BLOCK];
    let (mut off, mut filled) = (start, 0);
    loop {
        let n = ((end - off) as usize).min(BLOCK - filled);
        f.read_exact_at(&mut buf[filled..filled + n], off).expect("read input");
        off += n as u64;
        filled += n;
        // Only whole lines are reduced; the partial last line moves to the front of the buffer.
        let upto = if off == end { filled } else { buf[..filled].iter().rposition(|&b| b == b'\n').expect("line longer than a block") + 1 };
        read_lines(&buf[..upto], &mut c);
        buf.copy_within(upto..filled, 0);
        filled -= upto;
        if off == end {
            return c;
        }
    }
}

fn push_int(out: &mut Vec<u8>, mut v: u64) {
    let mut digits = [0u8; 20];
    let mut n = digits.len();
    loop {
        n -= 1;
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[n..]);
}

/// "2026-10-30T18:00:00Z " for each of the 72 hours of the span.
fn hour_labels() -> Vec<String> {
    (0..HOURS)
        .map(|h| {
            let (day, hh) = ((h + 18) / 24, (h + 18) % 24);
            let (month, dom) = match day {
                0 => (10, 30),
                1 => (10, 31),
                _ => (11, day - 1),
            };
            format!("2026-{month:02}-{dom:02}T{hh:02}:00:00Z ")
        })
        .collect()
}

fn main() {
    let f = File::open("/data/requests.log").expect("open input");
    let len = f.metadata().expect("input size").len();
    if len == 0 {
        File::create("/out/summary.txt").expect("create output");
        return;
    }
    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);

    let mut cut = vec![0u64; threads + 1];
    cut[threads] = len;
    for i in 1..threads {
        cut[i] = line_start(&f, (len * i as u64 / threads as u64).max(cut[i - 1]), len);
    }
    let chunks: Vec<Chunk> = thread::scope(|sc| {
        let (cut, f) = (&cut, &f);
        let jobs: Vec<_> = (0..threads).map(|i| sc.spawn(move || read_chunk(f, cut[i], cut[i + 1]))).collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    // One global, byte-ordered endpoint list; its index is the endpoint's rank within an hour.
    let mut endpoints: Vec<Vec<u8>> = chunks.iter().flat_map(|c| c.dict.names.iter().cloned()).collect();
    endpoints.sort_unstable();
    endpoints.dedup();
    assert!(chunks.iter().all(|c| c.dict.names.len() < 1 << 15), "too many endpoints for 15-bit ids");
    let width = endpoints.len();
    let ranks: Vec<Vec<u32>> =
        chunks.iter().map(|c| c.dict.names.iter().map(|n| endpoints.binary_search(n).unwrap() as u32).collect()).collect();

    // Each thread takes a run of hours. Per hour: count lines per endpoint, place the latencies grouped by
    // endpoint into one small buffer (a counting sort that stays in cache), then select the percentiles.
    let labels = hour_labels();
    let mut hour_out: Vec<Vec<u8>> = vec![Vec::new(); HOURS];
    {
        let (chunks, ranks, endpoints, labels) = (&chunks, &ranks, &endpoints, &labels);
        let mut out_left: &mut [Vec<u8>] = &mut hour_out;
        thread::scope(|sc| {
            for t in 0..threads {
                let (h0, h1) = (HOURS * t / threads, HOURS * (t + 1) / threads);
                let (outs, rest) = std::mem::take(&mut out_left).split_at_mut(h1 - h0);
                out_left = rest;
                sc.spawn(move || {
                    let mut begin = vec![0u32; width + 1];
                    let mut errors = vec![0u32; width];
                    let mut lat: Vec<u32> = Vec::new();
                    for (h, out) in (h0..h1).zip(outs.iter_mut()) {
                        begin.fill(0);
                        errors.fill(0);
                        for (c, rank) in chunks.iter().zip(ranks) {
                            for &e in &c.ep[h] {
                                let r = rank[usize::from(e & 0x7FFF)] as usize;
                                begin[r + 1] += 1;
                                errors[r] += u32::from(e >> 15);
                            }
                        }
                        for e in 0..width {
                            begin[e + 1] += begin[e];
                        }
                        let mut next = begin.clone();
                        lat.clear();
                        lat.resize(begin[width] as usize, 0);
                        for (c, rank) in chunks.iter().zip(ranks) {
                            for (&e, &l) in c.ep[h].iter().zip(&c.lat[h]) {
                                let slot = &mut next[rank[usize::from(e & 0x7FFF)] as usize];
                                lat[*slot as usize] = l;
                                *slot += 1;
                            }
                        }
                        out.reserve(width * 80);
                        for (e, endpoint) in endpoints.iter().enumerate() {
                            let v = &mut lat[begin[e] as usize..begin[e + 1] as usize];
                            let n = v.len();
                            if n == 0 {
                                continue;
                            }
                            // Nearest rank: ceil(p * n / 100), 1-indexed.
                            let rank = |p: usize| (p * n + 99) / 100;
                            let (r50, r95, r99) = (rank(50), rank(95), rank(99));
                            v.select_nth_unstable(r99 - 1);
                            let p99 = v[r99 - 1];
                            if r95 < r99 {
                                v[..r99 - 1].select_nth_unstable(r95 - 1);
                            }
                            let p95 = v[r95 - 1];
                            if r50 < r95 {
                                v[..r95 - 1].select_nth_unstable(r50 - 1);
                            }
                            let p50 = v[r50 - 1];
                            let errs = u64::from(errors[e]);
                            out.extend_from_slice(labels[h].as_bytes());
                            out.extend_from_slice(endpoint);
                            for x in [n as u64, u64::from(p50), u64::from(p95), u64::from(p99), errs, errs * 1_000_000 / n as u64] {
                                out.push(b' ');
                                push_int(out, x);
                            }
                            out.push(b'\n');
                        }
                    }
                });
            }
        });
    }

    let mut f = File::create("/out/summary.txt").expect("create output");
    for out in &hour_out {
        f.write_all(out).expect("write output");
    }
}
