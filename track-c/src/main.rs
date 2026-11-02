// Track C: Join.
// records.csv is mapped and cut into one chunk per thread at record starts. Each thread parses
// its records, looks the region up in a dense table and folds the record into per-country sums.
// Distinct names: every thread interns the 64-bit hash of the unescaped name in one shared lock-free table that
// hands out dense name ids, and sets the bit (name id, country). No per-record key is stored.
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

/// Lock-free set of name hashes shared by all threads. A name's id is its order of first insertion, so the
/// (name, country) bits stay dense. It holds at most half of `slots`, which keeps probes short; a file with more
/// distinct names sets `full` and is parsed again with a bigger table.
struct Names {
    slots: Vec<AtomicU64>, // name hash (never 0); 0 = empty
    ids: Vec<AtomicU32>,   // id + 1 of the name in the same slot; 0 = not yet published
    next: AtomicU32,
    full: AtomicBool,
    words: usize,         // u64 words of country bits per name
    seen: Vec<AtomicU64>, // bit (id, country) is at seen[id * words + country / 64]
}

impl Names {
    fn new(capacity: usize, countries: usize) -> Self {
        let words = countries.div_ceil(64);
        Names {
            slots: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            ids: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            next: AtomicU32::new(0),
            full: AtomicBool::new(false),
            words,
            seen: (0..capacity / 2 * words).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn id(&self, hash: u64) -> usize {
        let h = hash.max(1);
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            let mut cur = self.slots[i].load(Ordering::Acquire);
            if cur == 0 {
                if self.next.load(Ordering::Relaxed) as usize >= self.slots.len() / 2 {
                    self.full.store(true, Ordering::Relaxed);
                    return 0;
                }
                match self.slots[i].compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        let id = self.next.fetch_add(1, Ordering::Relaxed);
                        if id as usize >= self.slots.len() / 2 {
                            self.full.store(true, Ordering::Relaxed);
                            return 0;
                        }
                        self.ids[i].store(id + 1, Ordering::Release);
                        return id as usize;
                    }
                    Err(now) => cur = now,
                }
            }
            if cur == h {
                // Another thread may have claimed the slot and not yet published its id.
                loop {
                    let id = self.ids[i].load(Ordering::Acquire);
                    if id != 0 {
                        return id as usize - 1;
                    }
                    if self.full.load(Ordering::Relaxed) {
                        return 0;
                    }
                    std::hint::spin_loop();
                }
            }
            i = (i + 1) & mask;
        }
    }

    /// Starts loading the table entries that `id(hash)` will read first.
    fn prefetch_slot(&self, hash: u64) {
        let i = hash.max(1) as usize & (self.slots.len() - 1);
        unsafe {
            _mm_prefetch(self.slots.as_ptr().add(i) as *const i8, _MM_HINT_T0);
            _mm_prefetch(self.ids.as_ptr().add(i) as *const i8, _MM_HINT_T0);
        }
    }

    fn word(&self, id: usize, country: usize) -> &AtomicU64 {
        &self.seen[id * self.words + country / 64]
    }

    fn mark(&self, id: usize, country: usize) {
        let (word, bit) = (self.word(id, country), 1u64 << (country % 64));
        // Most pairs repeat: read first so the shared line is written only when the bit is new.
        if word.load(Ordering::Relaxed) & bit == 0 {
            word.fetch_or(bit, Ordering::Relaxed);
        }
    }
}

/// Four records in flight, so each random table access has time to arrive: the newest record's slot is prefetched,
/// two records later it gets its id and prefetches its bit, and one record after that it sets its bit.
#[derive(Default)]
struct Pipeline {
    waiting: Option<(u64, usize)>, // (name hash, country): slot requested, looked up one record later
    lookup: Option<(u64, usize)>,  // (name hash, country)
    mark: Option<(usize, usize)>,  // (name id, country)
}

impl Pipeline {
    fn push(&mut self, names: &Names, next: Option<(u64, usize)>) {
        if let Some((id, country)) = self.mark {
            names.mark(id, country);
        }
        self.mark = self.lookup.map(|(hash, country)| {
            let id = names.id(hash);
            unsafe { _mm_prefetch(names.word(id, country) as *const AtomicU64 as *const i8, _MM_HINT_T0) };
            (id, country)
        });
        if let Some((hash, _)) = next {
            names.prefetch_slot(hash);
        }
        self.lookup = std::mem::replace(&mut self.waiting, next);
    }
}

/// Everything a parsing thread needs to know about the join.
struct Join<'a> {
    country_of_region: &'a [u16],
    unmatched: u16,
    countries: usize,
    names: &'a Names,
}

/// What one thread produced from its chunk.
struct Part {
    totals: Vec<Totals>,
    end: usize, // where parsing stopped; must equal the next chunk's start
    ok: bool,
}

fn has_bom(d: &[u8], at: usize) -> bool {
    d.len() >= at + 3 && d[at..at + 3] == [0xEF, 0xBB, 0xBF]
}

/// Reads a text field starting at `at`. Returns the index of the byte after it (the comma), the
/// length of its unescaped value and, if asked, that value's hash.
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

fn field(d: &[u8], at: usize, scratch: &mut Vec<u8>, want_hash: bool) -> Option<(usize, u64, u64)> {
    if d[at] != b'"' {
        let len = find(d, at, b',')? - at;
        let h = if want_hash { hash64(&d[at..at + len]) } else { 0 };
        return Some((at + len, len as u64, h));
    }
    let body = at + 1;
    let mut close = find(d, body, b'"')?;
    if d.get(close + 1) != Some(&b'"') {
        // No escaped quote: the value is the bytes between the quotes.
        let h = if want_hash { hash64(&d[body..close]) } else { 0 };
        return Some((close + 1, (close - body) as u64, h));
    }
    if !want_hash {
        let mut escapes = 1;
        let mut from = close + 2;
        while let Some(next_close) = find(d, from, b'"') {
            close = next_close;
            if d.get(close + 1) == Some(&b'"') {
                escapes += 1;
                from = close + 2;
            } else {
                break;
            }
        }
        return Some((close + 1, (close - body - escapes) as u64, 0));
    }
    scratch.clear();
    let mut from = body;
    loop {
        scratch.extend_from_slice(&d[from..close]);
        if d.get(close + 1) == Some(&b'"') {
            scratch.push(b'"');
            from = close + 2;
            close = find(d, from, b'"')?;
        } else {
            let h = if want_hash { hash64(scratch) } else { 0 };
            return Some((close + 1, scratch.len() as u64, h));
        }
    }
}

fn parse_chunk(d: &[u8], mut at: usize, end: usize, j: &Join) -> Part {
    let mut part = Part { totals: vec![Totals::NONE; j.countries], end: 0, ok: true };
    let mut scratch = Vec::new();
    let mut pipeline = Pipeline::default();
    let mut released = at;
    while at < end {
        // Behind us the pages are finished with: hand whole pages back (MADV_DONTNEED drops only this process's
        // mapping; the file stays in the page cache), so the input never sits fully resident.
        if at - released >= 1 << 18 {
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
        pipeline.push(j.names, Some((name_hash, country)));
    }
    pipeline.push(j.names, None);
    pipeline.push(j.names, None);
    pipeline.push(j.names, None);
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

    // regions.csv: a header line, then "region_id,CCC" lines.
    let mut present = vec![false; 26 * 26 * 26];
    let header_end = regions.iter().position(|&c| c == b'\n').expect("regions header") + 1;
    let mut at = header_end;
    let mut max_id = 0usize;
    while at < regions.len() {
        let mut id = 0usize;
        while regions[at] != b',' {
            id = id * 10 + usize::from(regions[at] - b'0');
            at += 1;
        }
        at += 1;
        let code = usize::from(regions[at] - b'A') * 676 + usize::from(regions[at + 1] - b'A') * 26 + usize::from(regions[at + 2] - b'A');
        present[code] = true;
        max_id = id;
        at += 4;
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
    let mut country_of_region = vec![unmatched; max_id + 1];
    at = header_end;
    while at < regions.len() {
        let mut id = 0usize;
        while regions[at] != b',' {
            id = id * 10 + usize::from(regions[at] - b'0');
            at += 1;
        }
        at += 1;
        let code = usize::from(regions[at] - b'A') * 676 + usize::from(regions[at + 1] - b'A') * 26 + usize::from(regions[at + 2] - b'A');
        country_of_region[id] = index_of_code[code];
        at += 4;
    }

    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);
    let end = records.len();
    let mut first = if has_bom(records, 0) { 3 } else { 0 };
    first += records[first..].iter().position(|&c| c == b'\n').expect("header") + 1;

    let parse_with = |parts: usize, names: &Names| -> Option<Vec<Part>> {
        let join = Join { country_of_region: &country_of_region, unmatched, countries: keys.len(), names };
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
    // Start with 2^18 slots; a file with more than half that many distinct names is parsed again with 8x the room.
    let mut capacity = 1 << 18;
    let (parts, names) = loop {
        let names = Names::new(capacity, keys.len());
        let parts = match parse_with(threads, &names) {
            Some(p) => p,
            None => {
                let names = Names::new(capacity, keys.len());
                let p = parse_with(1, &names).expect("single-threaded parse");
                if names.full.load(Ordering::Relaxed) {
                    capacity *= 8;
                    continue;
                }
                break (p, names);
            }
        };
        if names.full.load(Ordering::Relaxed) {
            capacity *= 8;
            continue;
        }
        break (parts, names);
    };

    // names_distinct per country: the set bits of that country across all name ids.
    let mut distinct = vec![0u64; keys.len()];
    for row in names.seen[..names.next.load(Ordering::Relaxed) as usize * names.words].chunks(names.words) {
        for (w, word) in row.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                distinct[w * 64 + bits.trailing_zeros() as usize] += 1;
                bits &= bits - 1;
            }
        }
    }

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
        for v in [distinct[c], t.name_bytes, t.note_bytes] {
            out.push(b' ');
            push_int(&mut out, v);
        }
        out.push(b'\n');
    }
    File::create("/out/aggregates.txt").expect("create output").write_all(&out).expect("write output");
}
