//! A minimal scoped work queue (std only) for decoding slices and filtering planes in parallel.

use std::sync::Mutex;

/// Runs `f` on every task with up to `threads` threads, the calling thread included, and
/// returns the results in task order. Threads take the tasks in order from a shared queue, so
/// put the largest tasks first. With `threads <= 1` (or a single task) everything runs on the
/// calling thread, in order. A panic in a task is passed on to the caller.
pub(crate) fn par_map<T: Send, R: Send>(tasks: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = threads.min(tasks.len());
    if threads <= 1 {
        return tasks.into_iter().map(f).collect();
    }
    let n = tasks.len();
    let queue = Mutex::new(tasks.into_iter().enumerate());
    let worker = || {
        let mut done = Vec::new();
        loop {
            // The guard is dropped at the end of this statement, before the task runs.
            let task = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
            let Some((i, task)) = task else { break };
            done.push((i, f(task)));
        }
        done
    };
    let mut results = std::thread::scope(|s| {
        let others: Vec<_> = (1..threads).map(|_| s.spawn(worker)).collect();
        let mut all = worker();
        for t in others {
            all.extend(t.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
        }
        all
    });
    debug_assert_eq!(results.len(), n);
    results.sort_unstable_by_key(|&(i, _)| i);
    results.into_iter().map(|(_, r)| r).collect()
}

/// Splits `[start, end)` into at most `parts` contiguous, non-empty, nearly equal ranges and
/// returns their ends (the last one is `end`). An empty range gives no ends.
pub(crate) fn split_ends(start: usize, end: usize, parts: usize) -> Vec<usize> {
    let len = end.saturating_sub(start);
    let parts = parts.clamp(1, len.max(1));
    (1..=parts)
        .map(|j| start + len * j / parts)
        .filter(|&e| e > start)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_map_keeps_order() {
        for threads in [0, 1, 2, 5, 64] {
            let out = par_map((0..37).collect(), threads, |i: u32| i * i);
            assert_eq!(out, (0..37).map(|i| i * i).collect::<Vec<_>>(), "{threads} threads");
        }
        assert!(par_map(Vec::<u8>::new(), 4, |x| x).is_empty());
    }

    #[test]
    fn split_ends_covers_the_range() {
        assert_eq!(split_ends(0, 10, 3), vec![3, 6, 10]);
        assert_eq!(split_ends(1, 4, 8), vec![2, 3, 4]);
        assert_eq!(split_ends(5, 6, 1), vec![6]);
        assert_eq!(split_ends(5, 5, 4), Vec::<usize>::new());
    }
}
