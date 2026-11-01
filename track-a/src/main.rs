// Track A: Ingest.
// The log is mapped and cut into one chunk per thread. Each thread reduces its lines to compact
// records (endpoint, hour, latency, error flag). Records are then counting-sorted into one bucket
// per (hour, endpoint); percentiles come from select_nth_unstable inside each bucket, and each
// thread formats a range of hours so the output is simply the hours in order.
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::thread;

const HOURS: usize = 72;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// Maps a file read-only into memory (std has no mmap, so this declares the libc call).
fn map(path: &str) -> Option<&'static [u8]> {
    let f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    // PROT_READ, MAP_PRIVATE
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 0x2, f.as_raw_fd(), 0) };
    (p as isize != -1).then(|| unsafe { std::slice::from_raw_parts(p, len) })
}

/// Pointer shared by threads that write to provably disjoint slots.
#[derive(Clone, Copy)]
struct Disjoint<T>(*mut T);
unsafe impl<T> Send for Disjoint<T> {}
unsafe impl<T> Sync for Disjoint<T> {}

/// A 64-bit hash of an endpoint name. The names of one file are told apart by this hash alone: 1,842 names make a
/// collision about as likely as 1 in 10^13.
fn hash64(bytes: &[u8]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ (bytes.len() as u64).wrapping_mul(0xA24B_AED4_963E_E407);
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 32;
    }
    // The last 8 bytes (overlapping the words above), or the whole name when it is shorter than 8 bytes.
    let last = bytes.len().checked_sub(8).map_or_else(|| bytes.iter().rev().fold(0, |a, &b| a << 8 | u64::from(b)), |n| u64::from_le_bytes(bytes[n..].try_into().unwrap()));
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
    names: Vec<&'static [u8]>,
}

impl Dict {
    fn new() -> Self {
        Dict { slots: vec![(0, 0); 8192], names: Vec::new() }
    }

    fn id(&mut self, name: &'static [u8]) -> u32 {
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
                    self.names.push(name);
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
fn find(d: &[u8], from: usize, needle: u8) -> Option<usize> {
    let mut i = from;
    while i + 32 <= d.len() {
        let hits = unsafe { _mm256_cmpeq_epi8(_mm256_loadu_si256(d.as_ptr().add(i) as *const __m256i), _mm256_set1_epi8(needle as i8)) };
        let m = unsafe { _mm256_movemask_epi8(hits) } as u32;
        if m != 0 {
            return Some(i + m.trailing_zeros() as usize);
        }
        i += 32;
    }
    d[i..].iter().position(|&c| c == needle).map(|p| i + p)
}

fn digit(b: u8) -> usize {
    usize::from(b - b'0')
}

fn read_chunk(d: &'static [u8], mut at: usize, end: usize) -> Chunk {
    let mut c = Chunk { recs: Vec::with_capacity((end - at) / 70 + 16), dict: Dict::new() };
    let mut released = at;
    while at < end {
        // Behind us the pages are finished with: hand whole pages back (MADV_DONTNEED drops only this process's
        // mapping; the file stays in the page cache), so the input never sits fully resident.
        if at - released >= 1 << 22 {
            let (lo, hi) = ((released + 4095) & !4095, at & !4095);
            unsafe { madvise(d.as_ptr().add(lo) as *mut u8, hi - lo, 4) };
            released = at;
        }
        // "YYYY-MM-DDTHH:MM:SS.ffffffZ " is 28 bytes; the hour index is counted from 2026-10-30T18.
        let month = digit(d[at + 5]) * 10 + digit(d[at + 6]);
        let day = digit(d[at + 8]) * 10 + digit(d[at + 9]);
        let hour_of_day = digit(d[at + 11]) * 10 + digit(d[at + 12]);
        let day_index = if month == 10 { day - 30 } else { day + 1 };
        let hour = (day_index * 24 + hour_of_day - 18) as u8;
        at += 28;
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
    c
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
    let Some(d) = map("/data/requests.log") else {
        File::create("/out/summary.txt").expect("create output");
        return;
    };
    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);

    let mut cut = vec![0usize; threads + 1];
    cut[threads] = d.len();
    for i in 1..threads {
        let guess = (d.len() * i / threads).max(cut[i - 1]);
        cut[i] = d[guess..].iter().position(|&c| c == b'\n').map_or(d.len(), |k| guess + k + 1);
    }
    let mut chunks: Vec<Chunk> = thread::scope(|sc| {
        let cut = &cut;
        let jobs: Vec<_> = (0..threads).map(|i| sc.spawn(move || read_chunk(d, cut[i], cut[i + 1]))).collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    // One global, byte-ordered endpoint list; its index is the endpoint's rank within an hour.
    let mut endpoints: Vec<&[u8]> = chunks.iter().flat_map(|c| c.dict.names.iter().copied()).collect();
    endpoints.sort_unstable();
    endpoints.dedup();
    let width = endpoints.len();
    let buckets = HOURS * width;

    // Counting sort of the latencies into buckets, in parallel. Every chunk counts its own records (and its server
    // errors) per bucket; the counts become start positions, chunk after chunk inside each bucket; then every chunk
    // scatters its own latencies into those private ranges.
    let mut pos: Vec<Vec<[u32; 2]>> = thread::scope(|sc| {
        let endpoints = &endpoints;
        let jobs: Vec<_> = chunks
            .iter_mut()
            .map(|c| {
                sc.spawn(move || {
                    let rank: Vec<u32> = c.dict.names.iter().map(|n| endpoints.binary_search(n).unwrap() as u32).collect();
                    let mut count = vec![[0u32; 2]; buckets];
                    for r in c.recs.iter_mut() {
                        let b = (r.meta >> 24 & 127) as usize * width + rank[(r.meta & 0xFF_FFFF) as usize] as usize;
                        count[b][1] += r.meta >> 31;
                        r.meta = b as u32;
                        count[b][0] += 1;
                    }
                    count
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    // Totals and start positions, in parallel over ranges of buckets: a thread touches only its own buckets, in every
    // chunk's array. Prefix sums over the 132,000 bucket totals are cheap and stay sequential.
    let nt = pos.len().max(1);
    let per = buckets.div_ceil(nt).max(1);
    let arrays: Vec<Disjoint<[u32; 2]>> = pos.iter_mut().map(|p| Disjoint(p.as_mut_ptr())).collect();
    let mut begin = vec![0u32; buckets + 1];
    let mut errors = vec![0u32; buckets];
    thread::scope(|sc| {
        for (i, (total, errs)) in begin[1..].chunks_mut(per).zip(errors.chunks_mut(per)).enumerate() {
            let arrays = &arrays;
            sc.spawn(move || {
                for (k, (t, e)) in total.iter_mut().zip(errs.iter_mut()).enumerate() {
                    for a in arrays.iter() {
                        // SAFETY: read only; bucket i * per + k exists in every chunk's array.
                        let c = unsafe { *a.0.add(i * per + k) };
                        *t += c[0];
                        *e += c[1];
                    }
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
                        // SAFETY: this thread is the only one that touches bucket b in any chunk's array.
                        let c = unsafe { &mut *a.0.add(b) };
                        let n = c[0];
                        c[0] = at; // first slot of this chunk inside bucket b
                        at += n;
                    }
                }
            });
        }
    });
    let mut latencies = vec![0u32; begin[buckets] as usize];
    let dst = Disjoint(latencies.as_mut_ptr());
    thread::scope(|sc| {
        for (c, mut p) in chunks.iter().zip(pos) {
            sc.spawn(move || {
                let dst = dst;
                for r in &c.recs {
                    let slot = &mut p[r.meta as usize][0];
                    // SAFETY: every (chunk, bucket) pair owns its own range of `latencies`, sized by its count.
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
                            let errs = u64::from(errors[b]);
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
