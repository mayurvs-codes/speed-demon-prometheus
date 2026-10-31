// Track D: Traverse.
// Parallel parse of graph.txt into a CSR adjacency, then one early-stopping Dijkstra per
// query. Queries are shared between threads through an atomic counter.
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
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

/// Dijkstra state for one node: best known distance and the query that wrote it.
#[derive(Clone, Copy, Default)]
struct Label {
    dist: u64,
    seen: u32,
}

fn main() {
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
                    while i < end {
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
    let (queries_ref, first, arcs, in_deg, next) = (&queries, &first, &arcs, &in_deg, &next);
    let answered: Vec<Vec<(usize, i64)>> = thread::scope(|sc| {
        let jobs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(move || {
                    let mut label = vec![Label::default(); n];
                    let mut epoch = 0u32;
                    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();
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
                        epoch += 1;
                        heap.clear();
                        label[s as usize] = Label { dist: 0, seen: epoch };
                        heap.push(Reverse((0, s)));
                        let mut answer = -1i64;
                        while let Some(Reverse((d, v))) = heap.pop() {
                            if d > label[v as usize].dist {
                                continue; // stale entry
                            }
                            if v == t {
                                answer = d as i64;
                                break;
                            }
                            let (lo, hi) = (first[v as usize] as usize, first[v as usize + 1] as usize);
                            for a in &arcs[lo..hi] {
                                let nd = d + u64::from(a.w);
                                let l = &mut label[a.head as usize];
                                if l.seen != epoch || nd < l.dist {
                                    *l = Label { dist: nd, seen: epoch };
                                    heap.push(Reverse((nd, a.head)));
                                }
                            }
                        }
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
}
