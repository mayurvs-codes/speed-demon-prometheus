// Track C: Join.
// records.csv is mapped and cut into one chunk per thread at record starts. Each thread parses
// its records, looks the region up in a dense table and folds the record into per-country sums.
// Distinct names are counted from 64-bit hashes of the unescaped value, split into shards that
// are deduplicated in parallel.
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::thread;

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

fn hash64(bytes: &[u8]) -> u64 {
    const A: u64 = 0xFF51_AFD7_ED55_8CCD;
    const B: u64 = 0xC4CE_B9FE_1A85_EC53;
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ (bytes.len() as u64).wrapping_mul(0xA24B_AED4_963E_E407);
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(A);
        h ^= h >> 32;
    }
    let mut last = [0u8; 8];
    last[..words.remainder().len()].copy_from_slice(words.remainder());
    h = (h ^ u64::from_le_bytes(last)).wrapping_mul(B);
    h ^= h >> 29;
    h = h.wrapping_mul(0x9FB2_1C65_1E98_DF25);
    h ^ (h >> 32)
}

#[derive(Clone, Copy)]
struct Totals {
    count: u64,
    id_sum: u64,
    name_bytes: u64,
    note_bytes: u64,
    cents_sum: i64,
    cents_min: i64,
    cents_max: i64,
}

impl Totals {
    const NONE: Totals = Totals { count: 0, id_sum: 0, name_bytes: 0, note_bytes: 0, cents_sum: 0, cents_min: i64::MAX, cents_max: i64::MIN };

    fn add(&mut self, o: &Totals) {
        self.count += o.count;
        self.id_sum += o.id_sum;
        self.name_bytes += o.name_bytes;
        self.note_bytes += o.note_bytes;
        self.cents_sum += o.cents_sum;
        self.cents_min = self.cents_min.min(o.cents_min);
        self.cents_max = self.cents_max.max(o.cents_max);
    }
}

/// Everything a parsing thread needs to know about the join.
struct Join<'a> {
    country_of_region: &'a [u16],
    unmatched: u16,
    countries: usize,
    country_bits: u32,
    shards: usize,
}

/// What one thread produced from its chunk.
struct Part {
    totals: Vec<Totals>,
    keys: Vec<Vec<u64>>, // (name hash with the country in its low bits), bucketed by shard
    end: usize,          // where parsing stopped; must equal the next chunk's start
    ok: bool,
}

fn has_bom(d: &[u8], at: usize) -> bool {
    d.len() >= at + 3 && d[at..at + 3] == [0xEF, 0xBB, 0xBF]
}

/// Reads a text field starting at `at`. Returns the index of the byte after it (the comma), the
/// length of its unescaped value and, if asked, that value's hash.
fn field(d: &[u8], at: usize, scratch: &mut Vec<u8>, want_hash: bool) -> Option<(usize, u64, u64)> {
    if d[at] != b'"' {
        let len = d[at..].iter().position(|&c| c == b',')?;
        let h = if want_hash { hash64(&d[at..at + len]) } else { 0 };
        return Some((at + len, len as u64, h));
    }
    let body = at + 1;
    let mut close = body + d[body..].iter().position(|&c| c == b'"')?;
    if d.get(close + 1) != Some(&b'"') {
        // No escaped quote: the value is the bytes between the quotes.
        let h = if want_hash { hash64(&d[body..close]) } else { 0 };
        return Some((close + 1, (close - body) as u64, h));
    }
    scratch.clear();
    let mut from = body;
    loop {
        scratch.extend_from_slice(&d[from..close]);
        if d.get(close + 1) == Some(&b'"') {
            scratch.push(b'"');
            from = close + 2;
            close = from + d[from..].iter().position(|&c| c == b'"')?;
        } else {
            let h = if want_hash { hash64(scratch) } else { 0 };
            return Some((close + 1, scratch.len() as u64, h));
        }
    }
}

fn parse_chunk(d: &[u8], mut at: usize, end: usize, j: &Join) -> Part {
    let mut part = Part { totals: vec![Totals::NONE; j.countries], keys: vec![Vec::new(); j.shards], end: 0, ok: true };
    let mut scratch = Vec::new();
    let country_mask = (1u64 << j.country_bits) - 1;
    let mut released = at;
    while at < end {
        // Behind us the pages are finished with: hand whole pages back (MADV_DONTNEED drops only this process's
        // mapping; the file stays in the page cache), so the input never sits fully resident.
        if at - released >= 1 << 22 {
            let (lo, hi) = ((released + 4095) & !4095, at & !4095);
            unsafe { madvise(d.as_ptr().add(lo) as *mut u8, hi - lo, 4) };
            released = at;
        }
        if has_bom(d, at) {
            at += 3;
        }
        let mut id = 0u64;
        while d[at] != b',' {
            id = id * 10 + u64::from(d[at] - b'0');
            at += 1;
        }
        at += 1;
        let Some((stop, name_len, name_hash)) = field(d, at, &mut scratch, true) else { part.ok = false; return part };
        at = stop + 1;
        let Some((stop, note_len, _)) = field(d, at, &mut scratch, false) else { part.ok = false; return part };
        at = stop + 1;
        let mut region = 0usize;
        while d[at] != b',' {
            region = region * 10 + usize::from(d[at] - b'0');
            at += 1;
        }
        at += 1;
        let negative = d[at] == b'-';
        at += usize::from(negative);
        let mut cents = 0i64;
        while d[at] != b'.' {
            cents = cents * 10 + i64::from(d[at] - b'0');
            at += 1;
        }
        cents = cents * 100 + i64::from(d[at + 1] - b'0') * 10 + i64::from(d[at + 2] - b'0');
        at += 3;
        if negative {
            cents = -cents;
        }
        if at < d.len() {
            at += if d[at] == b'\r' { 2 } else { 1 };
        }

        let country = usize::from(*j.country_of_region.get(region).unwrap_or(&j.unmatched));
        let t = &mut part.totals[country];
        t.count += 1;
        t.id_sum += id;
        t.name_bytes += name_len;
        t.note_bytes += note_len;
        t.cents_sum += cents;
        t.cents_min = t.cents_min.min(cents);
        t.cents_max = t.cents_max.max(cents);
        let key = (name_hash & !country_mask) | country as u64;
        part.keys[((key >> j.country_bits) % j.shards as u64) as usize].push(key);
    }
    part.end = at;
    part
}

/// First record start at or after `from`: just after an LF (and optional BOM) when a digit follows.
/// Lines inside a quoted note never begin with a digit, so this cannot land inside quotes.
fn record_start(d: &[u8], mut from: usize) -> usize {
    while let Some(k) = d.get(from..).and_then(|s| s.iter().position(|&c| c == b'\n')) {
        let line = from + k + 1;
        let digit_at = if has_bom(d, line) { line + 3 } else { line };
        if d.get(digit_at).is_some_and(|c| (b'1'..=b'9').contains(c)) {
            return line;
        }
        from = line;
    }
    d.len()
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

fn push_money(out: &mut Vec<u8>, cents: i64) {
    if cents < 0 {
        out.push(b'-');
    }
    let c = cents.unsigned_abs();
    push_int(out, c / 100);
    out.extend_from_slice(&[b'.', b'0' + (c % 100 / 10) as u8, b'0' + (c % 10) as u8]);
}

fn main() {
    let (Some(regions), Some(records)) = (map("/data/regions.csv"), map("/data/records.csv")) else {
        std::process::exit(1)
    };

    // regions.csv: a header line, then "region_id,CCC" lines. A country code is three letters A-Z, so its base-26 number
    // indexes a table of 26^3 entries directly, and walking that table in order gives the codes already sorted.
    let mut region_rows: Vec<(usize, usize)> = Vec::new(); // (region id, code number)
    let mut present = vec![false; 26 * 26 * 26];
    let mut at = regions.iter().position(|&c| c == b'\n').expect("regions header") + 1;
    while at < regions.len() {
        let comma = at + regions[at..].iter().position(|&c| c == b',').expect("comma");
        let id = regions[at..comma].iter().fold(0usize, |a, &c| a * 10 + usize::from(c - b'0'));
        let code = regions[comma + 1..comma + 4].iter().fold(0usize, |a, &c| a * 26 + usize::from(c - b'A'));
        present[code] = true;
        region_rows.push((id, code));
        at = comma + 5; // "CCC\n"
    }
    // Country keys in byte order. "_UNMATCHED" sorts after every three-letter code.
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut index_of_code = vec![0u16; present.len()];
    for (code, _) in present.iter().enumerate().filter(|&(_, &p)| p) {
        index_of_code[code] = keys.len() as u16;
        keys.push(vec![b'A' + (code / 676) as u8, b'A' + (code / 26 % 26) as u8, b'A' + (code % 26) as u8]);
    }
    keys.push(b"_UNMATCHED".to_vec());
    let unmatched = (keys.len() - 1) as u16;
    let table_len = region_rows.iter().map(|r| r.0).max().map_or(0, |m| m + 1);
    let mut country_of_region = vec![unmatched; table_len];
    for &(id, code) in &region_rows {
        country_of_region[id] = index_of_code[code];
    }
    let country_bits = usize::BITS - (keys.len() - 1).leading_zeros();

    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);
    let end = records.len();
    let mut first = if has_bom(records, 0) { 3 } else { 0 };
    first += records[first..].iter().position(|&c| c == b'\n').expect("header") + 1;

    let parse_with = |parts: usize| -> Option<Vec<Part>> {
        let join = Join { country_of_region: &country_of_region, unmatched, countries: keys.len(), country_bits, shards: parts };
        let mut cut = vec![first; parts + 1];
        cut[parts] = end;
        for i in 1..parts {
            cut[i] = record_start(records, (first + (end - first) * i / parts).max(cut[i - 1]));
        }
        let done: Vec<Part> = thread::scope(|sc| {
            let (join, cut) = (&join, &cut);
            let jobs: Vec<_> = (0..parts).map(|i| sc.spawn(move || parse_chunk(records, cut[i], cut[i + 1], join))).collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        });
        // Every chunk must stop exactly where the next one starts, or a boundary was misjudged.
        let lined_up = done.iter().enumerate().all(|(i, p)| p.ok && p.end == cut[i + 1]);
        lined_up.then_some(done)
    };
    let (parts, shards) = match parse_with(threads) {
        Some(p) => (p, threads),
        None => (parse_with(1).expect("single-threaded parse"), 1),
    };

    // Each thread owns one shard of the (name hash, country) keys: sort, dedupe, count per country.
    let distinct: Vec<Vec<u64>> = thread::scope(|sc| {
        let (parts, countries) = (&parts, keys.len());
        let jobs: Vec<_> = (0..shards)
            .map(|s| {
                sc.spawn(move || {
                    let mut all: Vec<u64> = parts.iter().flat_map(|p| p.keys[s].iter().copied()).collect();
                    all.sort_unstable();
                    all.dedup();
                    let mut per_country = vec![0u64; countries];
                    for k in all {
                        per_country[(k & ((1u64 << country_bits) - 1)) as usize] += 1;
                    }
                    per_country
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    let mut out = Vec::with_capacity(keys.len() * 100);
    for (c, code) in keys.iter().enumerate() {
        let mut t = Totals::NONE;
        parts.iter().for_each(|p| t.add(&p.totals[c]));
        if t.count == 0 {
            continue;
        }
        out.extend_from_slice(code);
        for v in [t.count, t.id_sum] {
            out.push(b' ');
            push_int(&mut out, v);
        }
        for m in [t.cents_sum, t.cents_min, t.cents_max] {
            out.push(b' ');
            push_money(&mut out, m);
        }
        for v in [distinct.iter().map(|d| d[c]).sum::<u64>(), t.name_bytes, t.note_bytes] {
            out.push(b' ');
            push_int(&mut out, v);
        }
        out.push(b'\n');
    }
    File::create("/out/aggregates.txt").expect("create output").write_all(&out).expect("write output");
}
