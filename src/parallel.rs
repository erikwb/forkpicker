//! Bounded work scheduling; completion order does not change report order.
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default for local Git work; preserve the CLI's existing 32-worker ceiling.
pub fn local_jobs() -> u16 {
    std::thread::available_parallelism()
        .map(|cpus| cpus.get().min(32) as u16)
        .unwrap_or(1)
}

pub fn map<T: Sync, R: Send>(
    items: &[T],
    jobs: usize,
    work: impl Fn(usize, &T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.max(1).min(items.len()))
            .map(|_| {
                let work = &work;
                let next = &next;
                scope.spawn(move || {
                    let mut results = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        results.push((i, work(i, item)));
                    }
                    results
                })
            })
            .collect();
        let mut results: Vec<_> = workers
            .into_iter()
            .flat_map(|w| w.join().expect("scan worker panicked"))
            .collect();
        results.sort_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, value)| value).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, time::Duration};

    #[test]
    fn bounded_workers_overlap_and_preserve_input_order() {
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let barrier = Barrier::new(3);
        let results = map(&[0, 1, 2, 3, 4, 5], 3, |i, _| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            if i < 3 {
                barrier.wait();
            }
            std::thread::sleep(Duration::from_millis((6 - i) as u64));
            active.fetch_sub(1, Ordering::SeqCst);
            i
        });
        assert_eq!(results, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert!(map::<usize, usize>(&[], 0, |_, x| *x).is_empty());
    }
}
