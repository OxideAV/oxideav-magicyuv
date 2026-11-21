//! Scoped fan-out over independent units of work (slices).
//!
//! Slices are independent on both sides of the codec (`spec/04` §5.2:
//! no prediction across a slice boundary; one Huffman book per plane,
//! fixed before any slice is coded), so a frame's slices can be handed
//! to several workers. Nothing here chooses a thread count: the caller
//! grants one, and `1` (the default everywhere) runs the jobs inline on
//! the calling thread. The results are returned in job order, so the
//! output of a frame does not depend on the grant.

use std::sync::Mutex;

/// Run `work` on every job and return the results in job order, using
/// at most `threads` workers (the calling thread is one of them). Each
/// worker owns one `init()` state for scratch storage.
pub(crate) fn for_each_job<J, S, R>(
    jobs: Vec<J>,
    threads: usize,
    init: impl Fn() -> S + Sync,
    work: impl Fn(&mut S, J) -> R + Sync,
) -> Vec<R>
where
    J: Send,
    R: Send,
{
    let workers = threads.min(jobs.len());
    if workers <= 1 {
        let mut state = init();
        return jobs.into_iter().map(|job| work(&mut state, job)).collect();
    }
    let total = jobs.len();
    let queue = Mutex::new(jobs.into_iter().enumerate());
    let done: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(total));
    let run = || {
        let mut state = init();
        let mut local: Vec<(usize, R)> = Vec::new();
        loop {
            // The guard is dropped before the job runs.
            let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
            let Some((index, job)) = next else { break };
            local.push((index, work(&mut state, job)));
        }
        done.lock().unwrap_or_else(|e| e.into_inner()).extend(local);
    };
    std::thread::scope(|scope| {
        for _ in 1..workers {
            scope.spawn(run);
        }
        run();
    });
    let mut done = done.into_inner().unwrap_or_else(|e| e.into_inner());
    done.sort_unstable_by_key(|&(index, _)| index);
    debug_assert_eq!(done.len(), total);
    done.into_iter().map(|(_, r)| r).collect()
}

/// Split `data` into `parts` nearly equal pieces and run `work` on each
/// alongside the matching piece of `other`, on up to `parts` workers.
pub(crate) fn zip_chunks<T: Send, U: Sync>(
    data: &mut [T],
    other: &[U],
    parts: usize,
    work: impl Fn(&mut [T], &[U]) + Sync,
) {
    debug_assert_eq!(data.len(), other.len());
    let parts = parts.max(1);
    if parts == 1 || data.len() < parts {
        work(data, other);
        return;
    }
    let chunk = data.len().div_ceil(parts);
    std::thread::scope(|scope| {
        let mut pieces = data.chunks_mut(chunk).zip(other.chunks(chunk));
        let first = pieces.next();
        for (d, o) in pieces {
            let work = &work;
            scope.spawn(move || work(d, o));
        }
        if let Some((d, o)) = first {
            work(d, o);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_job_order_at_every_grant() {
        let jobs: Vec<u64> = (0..97).collect();
        let want: Vec<u64> = jobs.iter().map(|j| j * j + 1).collect();
        for threads in [0usize, 1, 2, 3, 8, 200] {
            let got = for_each_job(jobs.clone(), threads, || 1u64, |one, j| j * j + *one);
            assert_eq!(got, want, "threads={threads}");
        }
        let none: Vec<u8> = for_each_job(Vec::<u8>::new(), 4, || (), |_, j| j);
        assert!(none.is_empty());
    }

    #[test]
    fn jobs_may_borrow_disjoint_mutable_chunks() {
        let mut data = vec![0u32; 1000];
        let jobs: Vec<(usize, &mut [u32])> = data.chunks_mut(7).enumerate().collect();
        for_each_job(jobs, 4, || (), |_, (i, chunk)| chunk.fill(i as u32));
        assert!(data.iter().enumerate().all(|(k, &v)| v == (k / 7) as u32));
    }

    #[test]
    fn zip_chunks_covers_everything_once() {
        for parts in [1usize, 2, 3, 7, 64] {
            let mut a: Vec<u32> = (0..1001).collect();
            let b: Vec<u32> = (0..1001).map(|x| x * 3).collect();
            zip_chunks(&mut a, &b, parts, |x, y| {
                for (p, q) in x.iter_mut().zip(y) {
                    *p += q;
                }
            });
            assert!(a.iter().enumerate().all(|(k, &v)| v == 4 * k as u32));
        }
    }
}
