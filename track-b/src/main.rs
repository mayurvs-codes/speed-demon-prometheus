// Track B: Index.
// All 50,000 queries are known up front, so only the terms they mention are indexed.
//  1. Read the queries and give every distinct term an id.
//  2. Scan docs.txt in parallel: tokenize, look each token up, record (term, doc) pairs and, for
//     terms used in phrases, the token-id sequence of every document.
//  3. Counting-sort the pairs into per-term posting lists; frequent terms also get a bitmap.
//  4. BOOL queries run left to right on a document bitmap. PHRASE queries walk the rarest term's
//     documents and look for the phrase in their token-id sequences.
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

/// Maps a file read-only into memory (std has no mmap, so this declares the libc call).
fn map(path: &str) -> Option<&'static [u8]> {
    let f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    // PROT_READ, MAP_PRIVATE | MAP_POPULATE
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 0x2 | 0x8000, f.as_raw_fd(), 0) };
    (p as isize != -1).then(|| unsafe { std::slice::from_raw_parts(p, len) })
}

/// Pointer shared by threads that write to provably disjoint slots.
#[derive(Clone, Copy)]
struct Disjoint<T>(*mut T);
unsafe impl<T> Send for Disjoint<T> {}
unsafe impl<T> Sync for Disjoint<T> {}

fn hash64(bytes: &[u8]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ (bytes.len() as u64).wrapping_mul(0xA24B_AED4_963E_E407);
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 32;
    }
    let mut last = [0u8; 8];
    last[..words.remainder().len()].copy_from_slice(words.remainder());
    h = (h ^ u64::from_le_bytes(last)).wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 29;
    h = h.wrapping_mul(0x9FB2_1C65_1E98_DF25);
    h ^ (h >> 32)
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

/// The query vocabulary with a hash table for lookups while scanning documents.
struct Vocab {
    words: Vec<Vec<u8>>,
    in_phrase: Vec<bool>,
    slots: Vec<(u64, u32)>, // (hash, term + 1); 0 = empty
}

impl Vocab {
    fn index(&mut self) {
        let mut cap = 1024;
        while cap < self.words.len() * 2 {
            cap *= 2;
        }
        self.slots = vec![(0, 0); cap];
        for (t, w) in self.words.iter().enumerate() {
            let h = hash64(w);
            let mut i = h as usize & (cap - 1);
            while self.slots[i].1 != 0 {
                i = (i + 1) & (cap - 1);
            }
            self.slots[i] = (h, t as u32 + 1);
        }
    }

    #[inline]
    fn find(&self, w: &[u8]) -> Option<u32> {
        let h = hash64(w);
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            match self.slots[i] {
                (_, 0) => return None,
                (sh, st) if sh == h && self.words[st as usize - 1] == w => return Some(st - 1),
                _ => i = (i + 1) & mask,
            }
        }
    }
}

/// What one scanning thread produced from its slice of documents.
#[derive(Default)]
struct Slice {
    first_doc: u32,
    docs: u32,
    tokens: Vec<u32>,     // per token: phrase-term id + 1, or 0 for anything else
    doc_start: Vec<u32>,  // docs + 1 offsets into `tokens`
    pairs: Vec<u64>,      // term << 32 | doc, once per (term, doc)
    per_term: Vec<u32>,   // number of pairs per term
}

fn scan(d: &[u8], mut at: usize, end: usize, vocab: &Vocab, fold: &[u8; 256]) -> Slice {
    let mut s = Slice { per_term: vec![0; vocab.words.len()], doc_start: vec![0], ..Default::default() };
    s.tokens.reserve((end - at) / 6);
    let mut last_doc = vec![0u32; vocab.words.len()]; // doc + 1 of the last pair per term
    let mut word = [0u8; 32];
    let mut first = true;
    while at < end {
        let mut doc = 0u32;
        while d[at] != b'\t' {
            doc = doc * 10 + u32::from(d[at] - b'0');
            at += 1;
        }
        at += 1;
        if first {
            s.first_doc = doc;
            first = false;
        }
        let mut len = 0usize;
        loop {
            let c = d[at];
            at += 1;
            let f = fold[c as usize];
            if f != 0 {
                if len < 32 {
                    word[len] = f;
                }
                len += 1;
                continue;
            }
            if len != 0 {
                let mut tag = 0u32;
                if len <= 32 {
                    if let Some(t) = vocab.find(&word[..len]) {
                        if vocab.in_phrase[t as usize] {
                            tag = t + 1;
                        }
                        if last_doc[t as usize] != doc + 1 {
                            last_doc[t as usize] = doc + 1;
                            s.pairs.push(u64::from(t) << 32 | u64::from(doc));
                            s.per_term[t as usize] += 1;
                        }
                    }
                }
                s.tokens.push(tag);
                len = 0;
            }
            if c == b'\n' {
                break;
            }
        }
        s.doc_start.push(s.tokens.len() as u32);
        s.docs += 1;
    }
    s
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
    // ASCII-lowercase fold; 0 marks a separator byte.
    let mut fold = [0u8; 256];
    for c in b'0'..=b'9' {
        fold[c as usize] = c;
    }
    for c in b'a'..=b'z' {
        fold[c as usize] = c;
        fold[(c - 32) as usize] = c;
    }

    let Some(query_text) = map("/data/queries.txt") else { std::process::exit(1) };

    // 1. Queries and vocabulary.
    let mut vocab = Vocab { words: Vec::new(), in_phrase: Vec::new(), slots: Vec::new() };
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
                    vocab.in_phrase.push(false);
                    vocab.words.len() as u32 - 1
                });
                vocab.in_phrase[t as usize] |= phrase;
                terms.push(t);
            }
            queries.push(if phrase { Query::Phrase(terms) } else { Query::Bool { terms, ops } });
        }
    }
    vocab.index();
    let n_terms = vocab.words.len();

    // 2. Scan the documents.
    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);
    let slices: Vec<Slice> = match map("/data/docs.txt") {
        Some(d) => {
            let mut cut = vec![0usize; threads + 1];
            cut[threads] = d.len();
            for i in 1..threads {
                let guess = (d.len() * i / threads).max(cut[i - 1]);
                cut[i] = d[guess..].iter().position(|&c| c == b'\n').map_or(d.len(), |k| guess + k + 1);
            }
            thread::scope(|sc| {
                let (vocab, fold, cut) = (&vocab, &fold, &cut);
                let jobs: Vec<_> = (0..threads).map(|i| sc.spawn(move || scan(d, cut[i], cut[i + 1], vocab, fold))).collect();
                jobs.into_iter().map(|j| j.join().unwrap()).collect()
            })
        }
        None => vec![Slice { per_term: vec![0; n_terms], doc_start: vec![0], ..Default::default() }],
    };
    let doc_count: u32 = slices.iter().map(|s| s.docs).sum();
    let words_per_bitmap = (doc_count as usize).div_ceil(64);

    // 3. Posting lists. Slices are in document order, so each list comes out ascending.
    let mut list_start = vec![0usize; n_terms + 1];
    for t in 0..n_terms {
        list_start[t + 1] = list_start[t] + slices.iter().map(|s| s.per_term[t] as usize).sum::<usize>();
    }
    let mut postings = vec![0u32; list_start[n_terms]];
    {
        let dst = Disjoint(postings.as_mut_ptr());
        let mut cursors: Vec<Vec<usize>> = vec![vec![0; n_terms]; slices.len()];
        for t in 0..n_terms {
            let mut at = list_start[t];
            for (s, cur) in slices.iter().zip(cursors.iter_mut()) {
                cur[t] = at;
                at += s.per_term[t] as usize;
            }
        }
        thread::scope(|sc| {
            for (s, mut cur) in slices.iter().zip(cursors) {
                sc.spawn(move || {
                    let dst = dst;
                    for &p in &s.pairs {
                        let t = (p >> 32) as usize;
                        // SAFETY: each (term, slice) pair owns a private range of `postings`.
                        unsafe { *dst.0.add(cur[t]) = p as u32 };
                        cur[t] += 1;
                    }
                });
            }
        });
    }
    let mut slices = slices;
    slices.iter_mut().for_each(|s| s.pairs = Vec::new());

    // Frequent terms (a list at least twice as long as a bitmap has words) also get a bitmap.
    let mut bitmap_of = vec![u32::MAX; n_terms];
    let mut bitmaps_wanted = 0u32;
    for t in 0..n_terms {
        if words_per_bitmap > 0 && list_start[t + 1] - list_start[t] >= 2 * words_per_bitmap {
            bitmap_of[t] = bitmaps_wanted;
            bitmaps_wanted += 1;
        }
    }
    let mut bitmaps = vec![0u64; bitmaps_wanted as usize * words_per_bitmap];
    {
        let dst = Disjoint(bitmaps.as_mut_ptr());
        let next = AtomicUsize::new(0);
        let (list_start, postings, bitmap_of, next) = (&list_start, &postings, &bitmap_of, &next);
        thread::scope(|sc| {
            for _ in 0..threads {
                sc.spawn(move || {
                    let dst = dst;
                    loop {
                        let t = next.fetch_add(1, Relaxed);
                        if t >= n_terms {
                            break;
                        }
                        if bitmap_of[t] == u32::MAX {
                            continue;
                        }
                        // SAFETY: every frequent term owns its own `words_per_bitmap` words.
                        let row = unsafe { std::slice::from_raw_parts_mut(dst.0.add(bitmap_of[t] as usize * words_per_bitmap), words_per_bitmap) };
                        for &doc in &postings[list_start[t]..list_start[t + 1]] {
                            row[(doc >> 6) as usize] |= 1 << (doc & 63);
                        }
                    }
                });
            }
        });
    }

    // 4. Answer the queries.
    let slice_first_doc: Vec<u32> = slices.iter().map(|s| s.first_doc).collect();
    let next = AtomicUsize::new(0);
    let (queries, list_start, postings, bitmaps, bitmap_of, slices, slice_first_doc, next) =
        (&queries, &list_start, &postings, &bitmaps, &bitmap_of, &slices, &slice_first_doc, &next);
    let answered: Vec<Vec<(usize, Vec<u8>)>> = thread::scope(|sc| {
        let jobs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(move || {
                    let list = |t: u32| &postings[list_start[t as usize]..list_start[t as usize + 1]];
                    let bitmap = |t: u32| -> Option<&[u64]> {
                        let b = bitmap_of[t as usize];
                        (b != u32::MAX).then(|| &bitmaps[b as usize * words_per_bitmap..(b as usize + 1) * words_per_bitmap])
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
                                match bitmap(terms[0]) {
                                    Some(b) => run.copy_from_slice(b),
                                    None => {
                                        run.fill(0);
                                        for &doc in list(terms[0]) {
                                            run[(doc >> 6) as usize] |= 1 << (doc & 63);
                                        }
                                    }
                                }
                                for (&t, &op) in terms[1..].iter().zip(ops) {
                                    match (op, bitmap(t)) {
                                        (Op::And, Some(b)) => run.iter_mut().zip(b).for_each(|(r, b)| *r &= b),
                                        (Op::Or, Some(b)) => run.iter_mut().zip(b).for_each(|(r, b)| *r |= b),
                                        (Op::Not, Some(b)) => run.iter_mut().zip(b).for_each(|(r, b)| *r &= !b),
                                        (Op::And, None) => {
                                            keep.clear();
                                            keep.extend(list(t).iter().copied().filter(|&d| run[(d >> 6) as usize] >> (d & 63) & 1 != 0));
                                            run.fill(0);
                                            for &d in &keep {
                                                run[(d >> 6) as usize] |= 1 << (d & 63);
                                            }
                                        }
                                        (Op::Or, None) => {
                                            for &d in list(t) {
                                                run[(d >> 6) as usize] |= 1 << (d & 63);
                                            }
                                        }
                                        (Op::Not, None) => {
                                            for &d in list(t) {
                                                run[(d >> 6) as usize] &= !(1 << (d & 63));
                                            }
                                        }
                                    }
                                }
                                for (w, &word) in run.iter().enumerate() {
                                    let mut bits = word;
                                    while bits != 0 {
                                        ans.push(w as u32 * 64 + bits.trailing_zeros());
                                        bits &= bits - 1;
                                    }
                                }
                            }
                            Query::Phrase(terms) => {
                                let want: Vec<u32> = terms.iter().map(|t| t + 1).collect();
                                let k = want.len();
                                let rarest = *terms.iter().min_by_key(|&&t| list(t).len()).unwrap();
                                for &doc in list(rarest) {
                                    let s = &slices[slice_first_doc.partition_point(|&f| f <= doc) - 1];
                                    let local = (doc - s.first_doc) as usize;
                                    let tokens = &s.tokens[s.doc_start[local] as usize..s.doc_start[local + 1] as usize];
                                    if tokens.len() >= k && (0..=tokens.len() - k).any(|i| tokens[i] == want[0] && tokens[i + 1..i + k] == want[1..]) {
                                        ans.push(doc);
                                    }
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
    let mut f = File::create("/out/results.txt").expect("create output");
    for l in lines {
        f.write_all(l).expect("write output");
    }
}
