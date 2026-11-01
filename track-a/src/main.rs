// Track A: Ingest.
// The log is cut into one byte range per thread; each thread reads its range in small blocks (no mapping)
// and reduces its lines to compact records (endpoint, hour, latency, error flag). Records are then counting-sorted into one bucket
// per (hour, endpoint); percentiles come from select_nth_unstable inside each bucket, and each
// thread formats a range of hours so the output is simply the hours in order.
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

const HOURS: usize = 72;

/// Bytes read per call; each thread keeps one buffer of this size, small enough to stay in a core's cache while it is parsed.
const BLOCK: usize = 128 << 10;

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

/// Pointer shared by threads that write to provably disjoint slots.
#[derive(Clone, Copy)]
struct Disjoint<T>(*mut T);
unsafe impl<T> Send for Disjoint<T> {}
unsafe impl<T> Sync for Disjoint<T> {}

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

#[derive(Clone, Copy)]
struct Rec {
    meta: u32, // thread-local endpoint id (24 bits), hour (7 bits), 5xx flag (1 bit); later replaced by the bucket number
    latency: u32,
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

struct Chunk {
    recs: Vec<Rec>,
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
        let endpoint = c.dict.id(name);
        c.recs.push(Rec { meta: endpoint | u32::from(hour) << 24 | u32::from(server_error) << 31, latency });
    }
}

/// Reads bytes `start..end` (which begin and end on line boundaries) in blocks and reduces them.
fn read_chunk(f: &File, start: u64, end: u64) -> Chunk {
    let mut c = Chunk { recs: Vec::with_capacity((end - start) as usize / 70 + 16), dict: Dict::new() };
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
    let mut chunks: Vec<Chunk> = thread::scope(|sc| {
        let (cut, f) = (&cut, &f);
        let jobs: Vec<_> = (0..threads).map(|i| sc.spawn(move || read_chunk(f, cut[i], cut[i + 1]))).collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    // One global, byte-ordered endpoint list; its index is the endpoint's rank within an hour.
    let mut endpoints: Vec<Vec<u8>> = chunks.iter().flat_map(|c| c.dict.names.iter().cloned()).collect();
    endpoints.sort_unstable();
    endpoints.dedup();
    let width = endpoints.len();
    let buckets = HOURS * width;

    // Single compact atomic errors array (530 KB instead of 6.4 MB per-thread error tracking)
    let errors: Vec<AtomicU32> = (0..buckets).map(|_| AtomicU32::new(0)).collect();

    // Counting sort: each thread keeps only a u32 count per bucket (6.3 MB total, 50% less memory than [u32; 2])
    let mut pos: Vec<Vec<u32>> = thread::scope(|sc| {
        let endpoints = &endpoints;
        let errors = &errors;
        let jobs: Vec<_> = chunks
            .iter_mut()
            .map(|c| {
                sc.spawn(move || {
                    let rank: Vec<u32> = c.dict.names.iter().map(|n| endpoints.binary_search(n).unwrap() as u32).collect();
                    let mut count = vec![0u32; buckets];
                    for r in c.recs.iter_mut() {
                        let b = (r.meta >> 24 & 127) as usize * width + rank[(r.meta & 0xFF_FFFF) as usize] as usize;
                        if r.meta >> 31 != 0 {
                            errors[b].fetch_add(1, Ordering::Relaxed);
                        }
                        r.meta = b as u32;
                        count[b] += 1;
                    }
                    count
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    // Totals and start positions in parallel over ranges of buckets using 4-byte arrays
    let nt = pos.len().max(1);
    let per = buckets.div_ceil(nt).max(1);
    let arrays: Vec<Disjoint<u32>> = pos.iter_mut().map(|p| Disjoint(p.as_mut_ptr())).collect();
    let mut begin = vec![0u32; buckets + 1];

    thread::scope(|sc| {
        for (i, total) in begin[1..].chunks_mut(per).enumerate() {
            let arrays = &arrays;
            sc.spawn(move || {
                for (k, t) in total.iter_mut().enumerate() {
                    let idx = i * per + k;
                    let mut sum = 0u32;
                    for a in arrays.iter() {
                        sum += unsafe { *a.0.add(idx) };
                    }
                    *t = sum;
                }
            });
        }
    });

    for b in 0..buckets {
        begin[b + 1] += begin[b];
    }

    thread::scope(|sc| {
        for i in 0..nt {
            let (arrays, begin) = (&arrays, &begin);
            sc.spawn(move || {
                for b in (i * per).min(buckets)..((i + 1) * per).min(buckets) {
                    let mut at = begin[b];
                    for a in arrays.iter() {
                        let c = unsafe { &mut *a.0.add(b) };
                        let n = *c;
                        *c = at;
                        at += n;
                    }
                }
            });
        }
    });

    let mut latencies = vec![0u32; begin[buckets] as usize];
    let dst = Disjoint(latencies.as_mut_ptr());

    // Scatter latencies straight into bucket slots, then drop chunks and pos immediately
    thread::scope(|sc| {
        for (c, mut p) in chunks.into_iter().zip(pos) {
            sc.spawn(move || {
                let dst = dst;
                for r in &c.recs {
                    let slot = &mut p[r.meta as usize];
                    unsafe { *dst.0.add(*slot as usize) = r.latency };
                    *slot += 1;
                }
            });
        }
    });

    // Each thread takes a run of hours: its latency range and output slots are disjoint.
    let labels = hour_labels();
    let mut hour_out: Vec<Vec<u8>> = vec![Vec::new(); HOURS];
    {
        let (begin, errors, endpoints, labels) = (&begin, &errors, &endpoints, &labels);
        let mut lat_left: &mut [u32] = &mut latencies;
        let mut out_left: &mut [Vec<u8>] = &mut hour_out;
        thread::scope(|sc| {
            for t in 0..threads {
                let (h0, h1) = (HOURS * t / threads, HOURS * (t + 1) / threads);
                let (lat, rest) = std::mem::take(&mut lat_left).split_at_mut((begin[h1 * width] - begin[h0 * width]) as usize);
                lat_left = rest;
                let (outs, rest) = std::mem::take(&mut out_left).split_at_mut(h1 - h0);
                out_left = rest;
                let base = begin[h0 * width] as usize;
                sc.spawn(move || {
                    for (h, out) in (h0..h1).zip(outs.iter_mut()) {
                        out.reserve(width * 80);
                        for (e, endpoint) in endpoints.iter().enumerate() {
                            let b = h * width + e;
                            let (lo, hi) = (begin[b] as usize - base, begin[b + 1] as usize - base);
                            let n = hi - lo;
                            if n == 0 {
                                continue;
                            }
                            let v = &mut lat[lo..hi];
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
                            let errs = u64::from(errors[b].load(Ordering::Relaxed));
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

    drop(latencies);

    let mut f = File::create("/out/summary.txt").expect("create output");
    for out in &hour_out {
        f.write_all(out).expect("write output");
    }
}
