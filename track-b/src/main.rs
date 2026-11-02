// Track B: Index.
// All 50,000 queries are known up front, so only the terms they mention are indexed.
//  1. Read the queries and give every distinct term an id.
//  2. Scan docs.txt in parallel: tokenize, look each token up, record (term, doc) pairs and, for
//     terms used in phrases, the token-id sequence of every document.
//  3. Counting-sort the pairs into per-term posting lists; frequent terms also get a bitmap.
//  4. BOOL queries run left to right on a document bitmap. PHRASE queries walk the rarest term's
//     documents and look for the phrase in their token-id sequences.
use std::arch::x86_64::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
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

#[derive(Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Not,
}

enum Query {
    Bool { terms: Vec<u32>, ops: Vec<Op> }, // ops[i] joins the running result with terms[i + 1]
    Phrase(Vec<u32>),
}

/// One table entry: a term of up to 16 bytes stored inline (zero padded) and its id + 1 (0 = empty).
#[derive(Clone, Copy, Default)]
struct Slot {
    lo: u64,
    hi: u64,
    id: u32,
}

/// The query vocabulary. Terms of up to 16 bytes are packed into one 128-bit number and live inline in an
/// open-addressing table, so a lookup is a multiply, a shift and an integer compare. Terms of 17 to 32 bytes
/// (very rare) are kept in a HashMap. Tokens never contain a zero byte, so zero padding is unambiguous.
struct Vocab {
    words: Vec<Vec<u8>>,
    slots: Vec<Slot>,
    bits: u32,
    long: HashMap<Vec<u8>, u32>,
}

impl Vocab {
    fn index(&mut self) {
        self.bits = (self.words.len() * 3 / 2).next_power_of_two().max(1024).trailing_zeros();
        self.slots = vec![Slot::default(); 1 << self.bits];
        for (t, w) in self.words.iter().enumerate() {
            if w.len() > 16 {
                self.long.insert(w.clone(), t as u32);
                continue;
            }
            let key = w.iter().rev().fold(0u128, |k, &b| k << 8 | u128::from(b));
            let (lo, hi) = (key as u64, (key >> 64) as u64);
            let mut i = Self::home(lo, hi, self.bits);
            while self.slots[i].id != 0 {
                i = (i + 1) & ((1 << self.bits) - 1);
            }
            self.slots[i] = Slot { lo, hi, id: t as u32 + 1 };
        }
    }

    #[inline]
    fn home(lo: u64, hi: u64, bits: u32) -> usize {
        let h = lo.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ hi.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
        ((h ^ (h >> 32)).wrapping_mul(0xFF51_AFD7_ED55_8CCD) >> (64 - bits)) as usize
    }

    /// Looks up a token of at most 16 bytes, given as its two zero-padded 64-bit halves, starting at slot `i`
    /// (its home slot).
    #[inline]
    fn find(&self, lo: u64, hi: u64, mut i: usize) -> Option<u32> {
        let mask = (1 << self.bits) - 1;
        loop {
            let s = &self.slots[i];
            if s.id == 0 {
                return None;
            }
            if s.lo == lo && s.hi == hi {
                return Some(s.id - 1);
            }
            i = (i + 1) & mask;
        }
    }
}

/// What one scanning thread produced from its slice of documents.
#[derive(Default)]
struct Slice {
    count: usize,         // documents scanned
    first: u32,           // id of the first document scanned (ids are dense, so the rest follow)
    pairs: Vec<u32>,      // the terms of each document, once per (term, doc), each document closed by u32::MAX
    per_term: Vec<u32>,   // number of pairs per term
    phrases: Vec<Answer>, // per phrase: the documents of this slice that contain it
    lists: Vec<u16>,      // the pairs' documents (as doc - first) grouped by term, ascending inside each term
    start: Vec<u32>,      // term t's documents are lists[start[t]..start[t + 1]]
}

/// What one scanning thread knows about a term.
struct TermState {
    last_doc: u32, // doc + 1 of the last pair
    row: i32,      // bitmap row, -1 for none, -2 for a term no BOOL query uses
    pairs: u32,    // pairs recorded, plus ANCHOR when some phrase is anchored on this term
}

const ANCHOR: u32 = 1 << 31;

/// Groups a slice's pairs by term with a counting sort. The pairs are in document order, so each group stays ascending.
fn group(s: &mut Slice) {
    let mut n = 0;
    s.start = std::iter::once(0).chain(s.per_term.iter().map(|&c| { n += c; n })).collect();
    let mut at = s.start.clone();
    s.lists = vec![0; n as usize];
    let mut doc = 0u16; // a slice holds at most 65,536 documents (checked by the caller)
    for &t in &s.pairs {
        if t == u32::MAX {
            doc = doc.wrapping_add(1);
        } else {
            s.lists[at[t as usize] as usize] = doc;
            at[t as usize] += 1;
        }
    }
    s.pairs = Vec::new();
}

/// The phrases, found while scanning: each is anchored on its rarest term, so a document is only inspected for a phrase
/// where that term occurs.
struct Phrases<'a> {
    terms: Vec<&'a [u32]>,
    anchored: Vec<u64>,           // bit t: term t anchors some phrase
    at: Vec<Vec<(u32, usize)>>,   // per term: (phrase, offset of the term inside the phrase)
}

/// Bit i of the result is set when byte i of `v` is an ASCII letter or digit (a token byte).
/// OR-ing 0x20 maps A-Z onto a-z and leaves digits alone, so one unsigned range test covers each class.
#[inline(always)]
unsafe fn token_bytes(v: __m256i) -> u32 {
    let letter = _mm256_sub_epi8(_mm256_or_si256(v, _mm256_set1_epi8(0x20)), _mm256_set1_epi8(b'a' as i8));
    let digit = _mm256_sub_epi8(v, _mm256_set1_epi8(b'0' as i8));
    let is_letter = _mm256_cmpeq_epi8(_mm256_min_epu8(letter, _mm256_set1_epi8(25)), letter);
    let is_digit = _mm256_cmpeq_epi8(_mm256_min_epu8(digit, _mm256_set1_epi8(9)), digit);
    _mm256_movemask_epi8(_mm256_or_si256(is_letter, is_digit)) as u32
}

/// The lowercased first 16 bytes of the token of length `len` (at most 16) at `at`, as two 64-bit halves,
/// with every byte past `len` zeroed. Near the end of the file the bytes are copied so nothing is read past it.
#[inline(always)]
unsafe fn token_key(d: &[u8], at: usize, len: usize) -> (u64, u64) {
    let mut copy = [0u8; 16];
    let src = if at + 16 <= d.len() {
        d.as_ptr().add(at)
    } else {
        copy[..d.len() - at].copy_from_slice(&d[at..]);
        copy.as_ptr()
    };
    let lower = _mm_or_si128(_mm_loadu_si128(src as *const __m128i), _mm_set1_epi8(0x20));
    let keep = _mm_cmpgt_epi8(_mm_set1_epi8(len as i8), _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
    let k = _mm_and_si128(lower, keep);
    (_mm_cvtsi128_si64(k) as u64, _mm_extract_epi64(k, 1) as u64)
}

/// Scans the documents in d[at..end]. A term with a bitmap row (`rows[t] >= 0`) sets its bit directly; a term with no row
/// records one (term, document) pair; a term that no BOOL query uses (`rows[t] == -2`) needs neither. Phrases are matched
/// as each document ends.
fn scan(d: &[u8], mut at: usize, end: usize, vocab: &Vocab, rows: &[i32], bitmaps: &[AtomicU64], words: usize, ph: &Phrases) -> Slice {
    let mut s = Slice { phrases: (0..ph.terms.len()).map(|_| Answer::default()).collect(), ..Default::default() };
    // Everything the scan keeps per term sits together, so a token costs one cache miss here.
    let mut state: Vec<TermState> = rows.iter().enumerate().map(|(t, &row)| TermState { last_doc: 0, row, pairs: if ph.anchored[t >> 6] >> (t & 63) & 1 == 1 { ANCHOR } else { 0 } }).collect();
    let mut last_match = vec![0u32; ph.terms.len()]; // doc + 1 of the last match per phrase
    let mut ids: Vec<u32> = Vec::new(); // the current document's term ids (u32::MAX for a token no query uses)
    let mut keys: Vec<(u64, u64, usize)> = Vec::new(); // its tokens before lookup: (key halves, home slot)
    let mut anchors: Vec<usize> = Vec::new(); // positions in `ids` of terms that some phrase is anchored on
    let mut released = at;
    while at < end {
        // Behind us the pages are finished with: hand whole pages back (MADV_DONTNEED drops only this process's
        // mapping; the file stays in the page cache), so the corpus never sits fully resident.
        if at - released >= 1 << 18 {
            let (lo, hi) = ((released + 4095) & !4095, at & !4095);
            unsafe { madvise(d.as_ptr().add(lo) as *mut u8, hi - lo, 4) };
            released = at;
        }
        let mut doc = 0u32;
        while d[at] != b'\t' {
            doc = doc * 10 + u32::from(d[at] - b'0');
            at += 1;
        }
        at += 1;
        if s.count == 0 {
            s.first = doc;
        }
        s.count += 1;
        ids.clear();
        anchors.clear();
        // The text runs to the newline, read 32 bytes at a time. In each block, `m` marks token bytes, and
        // `m ^ (m << 1 | carry)` marks every token start and end (carry = the token continues from the last block).
        let (mut open, mut carry) = (0usize, 0u32);
        loop {
            let mut copy = [0u8; 32]; // only used for the last block of the file
            let block = unsafe {
                if at + 32 <= d.len() {
                    _mm256_loadu_si256(d.as_ptr().add(at) as *const __m256i)
                } else {
                    copy[..d.len() - at].copy_from_slice(&d[at..]);
                    _mm256_loadu_si256(copy.as_ptr() as *const __m256i)
                }
            };
            let newline = unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, _mm256_set1_epi8(b'\n' as i8))) } as u32;
            let stop = newline.trailing_zeros(); // 32 when this block has no newline
            let m = unsafe { token_bytes(block) } & if stop == 32 { u32::MAX } else { (1 << stop) - 1 };
            let mut edges = m ^ (m << 1 | carry);
            while edges != 0 {
                let b = edges.trailing_zeros() as usize;
                edges &= edges - 1;
                if m >> b & 1 == 1 {
                    open = at + b; // a token starts here
                    continue;
                }
                let len = at + b - open; // a token ended just before here
                if len <= 16 {
                    // Looked up once the document is read; the slot is requested from memory now.
                    let (lo, hi) = unsafe { token_key(d, open, len) };
                    let home = Vocab::home(lo, hi, vocab.bits);
                    unsafe { _mm_prefetch(vocab.slots.as_ptr().add(home) as *const i8, _MM_HINT_T0) };
                    keys.push((lo, hi, home));
                } else {
                    let found = if len <= 32 && !vocab.long.is_empty() {
                        vocab.long.get(&d[open..at + b].iter().map(|c| c | 0x20).collect::<Vec<u8>>()).copied()
                    } else {
                        None
                    };
                    keys.push((u64::from(found.unwrap_or(u32::MAX)), 0, usize::MAX));
                }
            }
            carry = m >> 31;
            if stop < 32 {
                at += stop as usize + 1;
                break;
            }
            at += 32;
        }
        // All of the document's slots are on their way, so these lookups overlap their cache misses.
        ids.extend(keys.drain(..).map(|(lo, hi, home)| {
            if home == usize::MAX {
                lo as u32 // resolved already (a long token)
            } else {
                vocab.find(lo, hi, home).unwrap_or(u32::MAX)
            }
        }));
        // Record the document's terms, with the state entry a few tokens ahead already on its way from memory.
        for (i, &t) in ids.iter().enumerate() {
            if let Some(&next) = ids.get(i + 8) {
                unsafe { _mm_prefetch(state.as_ptr().wrapping_add(next as usize) as *const i8, _MM_HINT_T0) };
            }
            if t == u32::MAX {
                continue;
            }
            let e = &mut state[t as usize];
            if e.pairs & ANCHOR != 0 {
                anchors.push(i);
            }
            if e.row != -2 && e.last_doc != doc + 1 {
                e.last_doc = doc + 1;
                if e.row < 0 {
                    s.pairs.push(t);
                    e.pairs += 1;
                } else {
                    bitmaps[e.row as usize * words + (doc >> 6) as usize].fetch_or(1 << (doc & 63), Relaxed);
                }
            }
        }
        s.pairs.push(u32::MAX);
        for &i in &anchors {
            for &(p, off) in &ph.at[ids[i] as usize] {
                let terms = ph.terms[p as usize];
                if i >= off && ids.get(i - off..i - off + terms.len()) == Some(terms) && last_match[p as usize] != doc + 1 {
                    last_match[p as usize] = doc + 1;
                    s.phrases[p as usize].push(doc);
                }
            }
        }
    }
    s.per_term = state.iter().map(|e| e.pairs & !ANCHOR).collect();
    s
}

/// The document ids whose bits are set in a bitmap, ascending.
fn docs_in(words: &[u64]) -> impl Iterator<Item = u32> + '_ {
    words.iter().enumerate().flat_map(|(w, &word)| {
        let mut bits = word;
        std::iter::from_fn(move || {
            (bits != 0).then(|| {
                let b = bits.trailing_zeros();
                bits &= bits - 1;
                w as u32 * 64 + b
            })
        })
    })
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

/// Running statistics of the matching documents, fed in ascending id order.
#[derive(Default)]
struct Answer {
    count: u64,
    sum: u64,
    smallest: Vec<u32>,
}

impl Answer {
    fn push(&mut self, doc: u32) {
        self.count += 1;
        self.sum += u64::from(doc);
        if self.smallest.len() < 10 {
            self.smallest.push(doc);
        }
    }

    /// Adds all set bits of `word` (document ids `base..base + 64`) at once: the count and the sum of ids come from
    /// popcounts (the sum of bit positions is the sum over the six position bits k of 2^k * popcount(word & mask_k)),
    /// and only the first few ids are recorded one by one.
    fn push_word(&mut self, base: u32, word: u64) {
        const POSITION_BIT: [u64; 6] = [0xAAAA_AAAA_AAAA_AAAA, 0xCCCC_CCCC_CCCC_CCCC, 0xF0F0_F0F0_F0F0_F0F0, 0xFF00_FF00_FF00_FF00, 0xFFFF_0000_FFFF_0000, 0xFFFF_FFFF_0000_0000];
        let n = u64::from(word.count_ones());
        self.count += n;
        self.sum += u64::from(base) * n + POSITION_BIT.iter().enumerate().map(|(k, m)| u64::from((word & m).count_ones()) << k).sum::<u64>();
        let mut rest = word;
        while self.smallest.len() < 10 && rest != 0 {
            self.smallest.push(base + rest.trailing_zeros());
            rest &= rest - 1;
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        push_int(out, self.count);
        out.push(b' ');
        push_int(out, self.sum);
        for &d in &self.smallest {
            out.push(b' ');
            push_int(out, u64::from(d));
        }
        out.push(b'\n');
    }
}

fn main() {
    let Some(query_text) = map("/data/queries.txt") else { std::process::exit(1) };

    // 1. Queries and vocabulary.
    let mut vocab = Vocab { words: Vec::new(), slots: Vec::new(), bits: 0, long: HashMap::new() };
    let mut queries: Vec<Query> = Vec::new();
    {
        let mut ids: HashMap<&[u8], u32> = HashMap::new();
        for line in query_text.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
            let phrase = line[0] == b'P';
            let body = if phrase { &line[8..line.len() - 1] } else { &line[5..] };
            let (mut terms, mut ops) = (Vec::new(), Vec::new());
            for w in body.split(|&c| c == b' ') {
                // Operators are the uppercase words in the odd positions; terms are lowercase.
                let op = match w {
                    b"AND" if !phrase => Some(Op::And),
                    b"OR" if !phrase => Some(Op::Or),
                    b"NOT" if !phrase => Some(Op::Not),
                    _ => None,
                };
                if let Some(op) = op {
                    ops.push(op);
                    continue;
                }
                let t = *ids.entry(w).or_insert_with(|| {
                    vocab.words.push(w.to_vec());
                    vocab.words.len() as u32 - 1
                });
                terms.push(t);
            }
            queries.push(if phrase { Query::Phrase(terms) } else { Query::Bool { terms, ops } });
        }
    }
    vocab.index();
    run(&mut vocab, &mut queries);
}

/// Everything after the query parse.
fn run(vocab: &mut Vocab, queries: &mut [Query]) {
    let n_terms = vocab.words.len();

    // 2. Choose the frequent terms. A short sample of each thread's slice estimates document frequencies. A term
    //    in at least 2 bitmap-words' worth of documents gets a bitmap row that the scan fills directly, so it needs
    //    no (term, document) pairs and no posting list.
    let docs = map("/data/docs.txt").unwrap_or(&[]);
    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);
    let mut cut = vec![0usize; threads + 1];
    cut[threads] = docs.len();
    for i in 1..threads {
        let guess = (docs.len() * i / threads).max(cut[i - 1]);
        cut[i] = docs[guess..].iter().position(|&c| c == b'\n').map_or(docs.len(), |k| guess + k + 1);
    }
    // Document ids are dense, so the id on the last line gives the document count.
    let last_line = docs[..docs.len().saturating_sub(1)].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
    let n_docs = if docs.is_empty() { 0 } else { docs[last_line..].iter().take_while(|c| c.is_ascii_digit()).fold(0, |a, &c| a * 10 + usize::from(c - b'0')) + 1 };
    let words_per_bitmap = n_docs.div_ceil(64);
    let no_rows = vec![-1i32; n_terms];
    let no_phrases = Phrases { terms: Vec::new(), anchored: vec![0; n_terms.div_ceil(64)], at: Vec::new() };
    let sample: Vec<Slice> = thread::scope(|sc| {
        let (vocab, cut, no_rows, no_phrases) = (&vocab, &cut, &no_rows, &no_phrases);
        let jobs: Vec<_> = (0..threads)
            .map(|i| {
                sc.spawn(move || {
                    let (b, e) = (cut[i], cut[i + 1]);
                    let mid = b + (e - b) / 32;
                    let stop = docs[mid..e].iter().position(|&c| c == b'\n').map_or(e, |k| mid + k + 1);
                    scan(docs, b, stop, vocab, no_rows, &[], 0, no_phrases)
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    let sampled_docs: usize = sample.iter().map(|s| s.count).sum();
    let seen: Vec<usize> = (0..n_terms).map(|t| sample.iter().map(|s| s.per_term[t] as usize).sum()).collect();
    drop(sample);
    // Number the terms by falling sampled frequency, so the per-term state of the words that make up most tokens
    // sits together in cache. Only the ids change; the vocabulary table and the queries are renumbered to match.
    let mut order: Vec<u32> = (0..n_terms as u32).collect();
    order.sort_by_key(|&t| std::cmp::Reverse(seen[t as usize]));
    let mut new_id = vec![0u32; n_terms];
    for (n, &t) in order.iter().enumerate() {
        new_id[t as usize] = n as u32;
    }
    vocab.words = order.iter().map(|&t| std::mem::take(&mut vocab.words[t as usize])).collect();
    vocab.long.clear();
    vocab.index();
    for q in queries.iter_mut() {
        let (Query::Bool { terms, .. } | Query::Phrase(terms)) = q;
        terms.iter_mut().for_each(|t| *t = new_id[*t as usize]);
    }
    let seen: Vec<usize> = order.iter().map(|&t| seen[t as usize]).collect();
    let (vocab, queries): (&Vocab, &[Query]) = (vocab, queries);
    let mut in_bool = vec![false; n_terms];
    for q in queries {
        if let Query::Bool { terms, .. } = q {
            terms.iter().for_each(|&t| in_bool[t as usize] = true);
        }
    }
    let mut rows = vec![-1i32; n_terms];
    let mut n_dense = 0;
    for t in 0..n_terms {
        if !in_bool[t] {
            rows[t] = -2;
        } else if seen[t] > 0 && seen[t] * n_docs >= 2 * words_per_bitmap * sampled_docs {
            rows[t] = n_dense;
            n_dense += 1;
        }
    }
    // Each phrase is anchored on the term the sample saw least.
    let mut ph = Phrases { terms: Vec::new(), anchored: vec![0; n_terms.div_ceil(64)], at: vec![Vec::new(); n_terms] };
    let mut phrase_of = vec![0usize; queries.len()];
    for (qi, q) in queries.iter().enumerate() {
        if let Query::Phrase(terms) = q {
            let anchor = *terms.iter().min_by_key(|&&t| seen[t as usize]).unwrap();
            phrase_of[qi] = ph.terms.len();
            ph.at[anchor as usize].push((ph.terms.len() as u32, terms.iter().position(|&t| t == anchor).unwrap()));
            ph.anchored[anchor as usize >> 6] |= 1 << (anchor & 63);
            ph.terms.push(terms);
        }
    }

    // 3. Scan the documents.
    //    Posting lists hold 16-bit offsets from their slice's first document, so a slice may hold at most 65,536
    //    documents; if one holds more, the scan is repeated with twice as many slices (setting a bitmap bit twice
    //    is harmless).
    let bitmaps: Vec<AtomicU64> = (0..n_dense as usize * words_per_bitmap).map(|_| AtomicU64::new(0)).collect();
    let mut parts = threads;
    let slices: Vec<Slice> = loop {
        let mut cut = vec![0usize; parts + 1];
        cut[parts] = docs.len();
        for i in 1..parts {
            let guess = (docs.len() * i / parts).max(cut[i - 1]);
            cut[i] = docs[guess..].iter().position(|&c| c == b'\n').map_or(docs.len(), |k| guess + k + 1);
        }
        let slices: Vec<Slice> = thread::scope(|sc| {
            let (vocab, cut, rows, bitmaps, ph) = (&vocab, &cut, &rows, &bitmaps, &ph);
            let jobs: Vec<_> = (0..parts)
                .map(|i| {
                    sc.spawn(move || {
                        let mut s = scan(docs, cut[i], cut[i + 1], vocab, rows, bitmaps, words_per_bitmap, ph);
                        if s.count <= 1 << 16 {
                            group(&mut s);
                        }
                        s
                    })
                })
                .collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        });
        if slices.iter().all(|s| s.count <= 1 << 16) {
            break slices;
        }
        parts *= 2;
    };

    let bitmaps: Vec<u64> = bitmaps.iter().map(|w| w.load(Relaxed)).collect();

    // 4. Answer the queries.
    let next = AtomicUsize::new(0);
    let (queries, bitmaps, rows, slices, phrase_of, next) = (&queries, &bitmaps, &rows, &slices, &phrase_of, &next);
    let answered: Vec<Vec<(usize, Vec<u8>)>> = thread::scope(|sc| {
        let jobs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(move || {
                    let list = |t: u32| slices.iter().flat_map(move |s| s.lists[s.start[t as usize] as usize..s.start[t as usize + 1] as usize].iter().map(move |&d| s.first + u32::from(d)));
                    let bitmap = |t: u32| -> Option<&[u64]> {
                        let r = rows[t as usize];
                        (r >= 0).then(|| &bitmaps[r as usize * words_per_bitmap..][..words_per_bitmap])
                    };
                    let mut mine = Vec::new();
                    let mut run = vec![0u64; words_per_bitmap.max(1)];
                    let mut keep: Vec<u32> = Vec::new();
                    loop {
                        let qi = next.fetch_add(1, Relaxed);
                        let Some(q) = queries.get(qi) else { break };
                        let mut ans = Answer::default();
                        match q {
                            Query::Bool { terms, ops } => {
                                let run = &mut run[..words_per_bitmap];
                                // `empty` is only set when it is known for free (an absent first term, or an AND with a short
                                // list that leaves nothing); then `run` is all zero.
                                let mut empty = bitmap(terms[0]).is_none() && list(terms[0]).next().is_none();
                                match bitmap(terms[0]) {
                                    Some(b) => run.copy_from_slice(b),
                                    None => {
                                        run.fill(0);
                                        for doc in list(terms[0]) {
                                            run[(doc >> 6) as usize] |= 1 << (doc & 63);
                                        }
                                    }
                                }
                                for (i, (&t, &op)) in terms[1..].iter().zip(ops).enumerate() {
                                    // Left to right: an empty result stays empty under AND and NOT, and only an OR can revive it.
                                    if empty && !ops[i..].contains(&Op::Or) {
                                        break;
                                    }
                                    if empty && op != Op::Or {
                                        continue;
                                    }
                                    empty = empty && bitmap(t).is_none() && list(t).next().is_none();
                                    match (op, bitmap(t)) {
                                        (Op::And, Some(b)) => run.iter_mut().zip(b).for_each(|(r, b)| *r &= b),
                                        (Op::Or, Some(b)) => run.iter_mut().zip(b).for_each(|(r, b)| *r |= b),
                                        (Op::Not, Some(b)) => run.iter_mut().zip(b).for_each(|(r, b)| *r &= !b),
                                        (Op::And, None) => {
                                            keep.clear();
                                            keep.extend(list(t).filter(|&d| run[(d >> 6) as usize] >> (d & 63) & 1 != 0));
                                            run.fill(0);
                                            for &d in &keep {
                                                run[(d >> 6) as usize] |= 1 << (d & 63);
                                            }
                                            empty = keep.is_empty();
                                        }
                                        (Op::Or, None) => {
                                            for d in list(t) {
                                                run[(d >> 6) as usize] |= 1 << (d & 63);
                                            }
                                        }
                                        (Op::Not, None) => {
                                            for d in list(t) {
                                                run[(d >> 6) as usize] &= !(1 << (d & 63));
                                            }
                                        }
                                    }
                                }
                                if !empty {
                                    run.iter().enumerate().filter(|&(_, &w)| w != 0).for_each(|(i, &w)| ans.push_word(i as u32 * 64, w));
                                }
                            }
                            Query::Phrase(_) => {
                                for s in slices.iter().map(|s| &s.phrases[phrase_of[qi]]) {
                                    ans.count += s.count;
                                    ans.sum += s.sum;
                                    ans.smallest.extend(s.smallest.iter().take(10 - ans.smallest.len()));
                                }
                            }
                        }
                        let mut line = Vec::with_capacity(64);
                        ans.write(&mut line);
                        mine.push((qi, line));
                    }
                    mine
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    let mut lines: Vec<&[u8]> = vec![&[]; queries.len()];
    for (qi, line) in answered.iter().flatten() {
        lines[*qi] = line;
    }
    File::create("/out/results.txt").expect("create output").write_all(&lines.concat()).expect("write output");
}
