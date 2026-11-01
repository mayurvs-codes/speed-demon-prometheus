// Track D: Traverse.
// Parallel parse of graph.txt into a CSR adjacency, then one early-stopping Dijkstra per
// query. Queries are shared between threads through an atomic counter.
use std::arch::x86_64::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

#[repr(C)]
struct RUsage {
    ru_utime: [u64; 2],
    ru_stime: [u64; 2],
    ru_maxrss: i64,
    ru_ixrss: i64,
    ru_idrss: i64,
    ru_isrss: i64,
    ru_minflt: i64,
    ru_majflt: i64,
    ru_nswap: i64,
    ru_inblock: i64,
    ru_oublock: i64,
    ru_msgsnd: i64,
    ru_msgrcv: i64,
    ru_nsignals: i64,
    ru_nvcsw: i64,
    ru_nivcsw: i64,
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

/// Reads one unsigned decimal at `*i` and skips the single separator byte after it.
#[inline(always)]
fn num(d: &[u8], i: &mut usize) -> u64 {
    let mut v = 0u64;
    while d[*i] >= b'0' {
        v = v * 10 + u64::from(d[*i] - b'0');
        *i += 1;
    }
    *i += 1;
    v
}

#[derive(Clone, Copy, Default)]
struct Arc {
    head: u32,
    w: u32,
}



fn main() {
    let t0 = std::time::Instant::now();
    let Some(g) = map("/data/graph.txt") else { std::process::exit(1) };
    let mut at = 0usize;
    let n = num(g, &mut at) as usize;
    let m = num(g, &mut at) as usize;
    let body = at;

    let threads = thread::available_parallelism().map_or(1, |t| t.get()).clamp(1, 12);

    // Split the edge list at line boundaries.
    let mut cut = vec![body; threads + 1];
    cut[threads] = g.len();
    for i in 1..threads {
        let guess = (body + (g.len() - body) * i / threads).max(cut[i - 1]);
        cut[i] = g[guess..].iter().position(|&c| c == b'\n').map_or(g.len(), |k| guess + k + 1);
    }

    // Pass 1: parse edges, counting out-degree per tail and in-degree per head (self-loops excluded).
    let out_deg: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let in_deg: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let parsed: Vec<Vec<[u32; 3]>> = thread::scope(|sc| {
        let (out_deg, in_deg, cut) = (&out_deg, &in_deg, &cut);
        let jobs: Vec<_> = (0..threads)
            .map(|t| {
                sc.spawn(move || {
                    let mut edges = Vec::with_capacity(m / threads + m / threads / 8 + 64);
                    let (mut i, end) = (cut[t], cut[t + 1]);
                    let mut released = i;
                    while i < end {
                        // Behind us the pages are finished with: hand whole pages back (MADV_DONTNEED drops only this process's
                        // mapping; the file stays in the page cache), so the input never sits fully resident.
                        if i - released >= 1 << 22 {
                            let (lo, hi) = ((released + 4095) & !4095, i & !4095);
                            unsafe { madvise(g.as_ptr().add(lo) as *mut u8, hi - lo, 4) };
                            released = i;
                        }
                        let u = num(g, &mut i) as u32;
                        let v = num(g, &mut i) as u32;
                        let w = num(g, &mut i) as u32;
                        edges.push([u, v, w]);
                        out_deg[u as usize].fetch_add(1, Relaxed);
                        if u != v {
                            in_deg[v as usize].fetch_add(1, Relaxed);
                        }
                    }
                    edges
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    // Pass 2: prefix sums, then scatter every edge into its tail's slice of the arc array.
    let mut first = vec![0u32; n + 1];
    for v in 0..n {
        first[v + 1] = first[v] + out_deg[v].load(Relaxed);
        out_deg[v].store(first[v], Relaxed); // becomes the fill cursor
    }
    let mut arcs = vec![Arc::default(); first[n] as usize];
    {
        let dst = Disjoint(arcs.as_mut_ptr());
        thread::scope(|sc| {
            let cursor = &out_deg;
            for edges in parsed {
                sc.spawn(move || {
                    let dst = dst;
                    for &[u, v, w] in &edges {
                        let slot = cursor[u as usize].fetch_add(1, Relaxed) as usize;
                        // SAFETY: fetch_add gives every edge its own slot inside its tail's range.
                        unsafe { *dst.0.add(slot) = Arc { head: v, w } };
                    }
                });
            }
        });
    }

    // The node ids in the file are a random relabelling, so every edge hop lands somewhere far away in memory.
    // Renumber the nodes in breadth-first order: neighbours then get nearby ids, and a search walks nearby memory.
    // The visited set is a bitset (218 KB, stays in cache); the old-to-new map is filled afterwards in one pass.
    let mut seen = vec![0u64; n.div_ceil(64)];
    let mut old_of: Vec<u32> = Vec::with_capacity(n); // old_of[new id] = old id; doubles as the BFS queue
    for root in 0..n {
        if seen[root >> 6] >> (root & 63) & 1 != 0 {
            continue;
        }
        seen[root >> 6] |= 1 << (root & 63);
        old_of.push(root as u32);
        let mut head = old_of.len() - 1;
        while head < old_of.len() {
            let v = old_of[head] as usize;
            // The queue says which nodes come next, so fetch their data early: offsets 16 nodes ahead, arcs 8 ahead.
            if let Some(&w) = old_of.get(head + 16) {
                unsafe { _mm_prefetch::<{ _MM_HINT_T0 }>(first.as_ptr().add(w as usize) as *const i8) };
            }
            if let Some(&u) = old_of.get(head + 8) {
                unsafe { _mm_prefetch::<{ _MM_HINT_T0 }>(arcs.as_ptr().add(first[u as usize] as usize) as *const i8) };
            }
            head += 1;
            for a in &arcs[first[v] as usize..first[v + 1] as usize] {
                let h = a.head as usize;
                if seen[h >> 6] >> (h & 63) & 1 == 0 {
                    seen[h >> 6] |= 1 << (h & 63);
                    old_of.push(a.head);
                }
            }
        }
    }
    drop(seen);
    let mut new_id = vec![0u32; n];
    for (i, &v) in old_of.iter().enumerate() {
        new_id[v as usize] = i as u32;
    }
    let mut first2 = vec![0u32; n + 1];
    for i in 0..n {
        let v = old_of[i] as usize;
        first2[i + 1] = first2[i] + (first[v + 1] - first[v]);
    }
    let mut arcs2 = vec![Arc::default(); arcs.len()];
    thread::scope(|sc| {
        let mut rest: &mut [Arc] = &mut arcs2;
        for t in 0..threads {
            let (lo, hi) = (n * t / threads, n * (t + 1) / threads);
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut((first2[hi] - first2[lo]) as usize);
            rest = tail;
            let (old_of, new_id, first, arcs) = (&old_of, &new_id, &first, &arcs);
            sc.spawn(move || {
                let mut at = 0;
                for &v in &old_of[lo..hi] {
                    for a in &arcs[first[v as usize] as usize..first[v as usize + 1] as usize] {
                        mine[at] = Arc { head: new_id[a.head as usize], w: a.w };
                        at += 1;
                    }
                }
            });
        }
    });
    first = first2; // the old arrays are freed by these assignments
    arcs = arcs2;
    drop(old_of);

    let mut queries: Vec<(u32, u32)> = Vec::new();
    if let Some(q) = map("/data/queries.txt") {
        let mut i = 0;
        while i < q.len() {
            let s = num(q, &mut i) as u32;
            let t = num(q, &mut i) as u32;
            queries.push((s, t));
        }
    }



    // Answer queries; each thread owns its label array and heap.
    let next = AtomicUsize::new(0);
    let (queries_ref, first, arcs, in_deg, next, new_id) = (&queries, &first, &arcs, &in_deg, &next, &new_id);
    let answered: Vec<Vec<(usize, i64)>> = thread::scope(|sc| {
        let jobs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(move || {
                    let mut dist = vec![u64::MAX; n];
                    let mut touched: Vec<u32> = Vec::with_capacity(16384);
                    let mut heap: BinaryHeap<Reverse<u128>> = BinaryHeap::new(); // key = distance << 32 | node
                    let mut done = Vec::new();
                    loop {
                        let qi = next.fetch_add(1, Relaxed);
                        let Some(&(s, t)) = queries_ref.get(qi) else { break };
                        if s == t {
                            done.push((qi, 0));
                            continue;
                        }
                        // Nothing but self-loops (or nothing at all) enters t: unreachable.
                        if in_deg[t as usize].load(Relaxed) == 0 {
                            done.push((qi, -1));
                            continue;
                        }
                        let (s, t) = (new_id[s as usize], new_id[t as usize]);
                        heap.clear();
                        dist[s as usize] = 0;
                        touched.push(s);
                        heap.push(Reverse(u128::from(s)));
                        let mut answer = -1i64;
                        while let Some(Reverse(key)) = heap.pop() {
                            let (d, v) = ((key >> 32) as u64, key as u32);
                            if d > dist[v as usize] {
                                continue; // stale entry
                            }
                            if v == t {
                                answer = d as i64;
                                break;
                            }
                            let (lo, hi) = (first[v as usize] as usize, first[v as usize + 1] as usize);
                            for a in &arcs[lo..hi] {
                                let nd = d + u64::from(a.w);
                                let h = a.head as usize;
                                if nd < dist[h] {
                                    if dist[h] == u64::MAX {
                                        touched.push(a.head);
                                    }
                                    dist[h] = nd;
                                    heap.push(Reverse(u128::from(nd) << 32 | u128::from(a.head)));
                                }
                            }
                        }
                        for &v in &touched {
                            dist[v as usize] = u64::MAX;
                        }
                        touched.clear();
                        done.push((qi, answer));
                    }
                    done
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });

    let mut result = vec![0i64; queries.len()];
    for (qi, a) in answered.into_iter().flatten() {
        result[qi] = a;
    }
    let mut text = String::with_capacity(queries.len() * 12);
    for a in result {
        text.push_str(&a.to_string());
        text.push('\n');
    }
    File::create("/out/distances.txt").expect("create /out/distances.txt").write_all(text.as_bytes()).expect("write output");
    let elapsed = t0.elapsed();
    let mut ru = std::mem::MaybeUninit::<RUsage>::zeroed();
    unsafe { getrusage(0, ru.as_mut_ptr()) };
    let rss = unsafe { ru.assume_init().ru_maxrss };
    eprintln!("SD_METRICS time={:.4} rss={}", elapsed.as_secs_f64(), rss);
}
