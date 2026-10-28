use std::future::Future;

use super::types::ToolAccesses;

/// Runs a batch of tool tasks concurrently when possible, preserving provider order.
pub struct ToolScheduler;

impl ToolScheduler {
    pub fn new() -> Self {
        Self
    }

    pub async fn run_batch<Fut, Result>(&self, tasks: Vec<(ToolAccesses, Fut)>) -> Vec<Result>
    where
        Fut: Future<Output = Result> + Send + 'static,
        Result: Send + 'static,
    {
        let mut indexed_tasks: Vec<(usize, ToolAccesses, Fut)> = tasks
            .into_iter()
            .enumerate()
            .map(|(i, (a, t))| (i, a, t))
            .collect();
        let mut results: Vec<(usize, Result)> = Vec::new();

        while !indexed_tasks.is_empty() {
            let mut batch: Vec<(usize, ToolAccesses, Fut)> = Vec::new();
            let mut remaining: Vec<(usize, ToolAccesses, Fut)> = Vec::new();

            for (i, accesses, task) in indexed_tasks {
                let conflicts = batch
                    .iter()
                    .any(|(_, a, _)| ToolAccesses::conflicts(a, &accesses));
                if conflicts {
                    remaining.push((i, accesses, task));
                } else {
                    batch.push((i, accesses, task));
                }
            }

            let handles: Vec<_> = batch
                .into_iter()
                .map(|(i, _, t)| tokio::spawn(async move { (i, t.await) }))
                .collect();

            let batch_results = futures::future::join_all(handles).await;
            for r in batch_results {
                let (i, result) = r.expect("tool task panicked");
                results.push((i, result));
            }

            indexed_tasks = remaining;
        }

        results.sort_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, r)| r).collect()
    }
}

impl Default for ToolScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;

    type BoxedTask<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

    fn spawn_task<F, T>(f: F) -> BoxedTask<T>
    where
        F: std::future::Future<Output = T> + Send + 'static,
    {
        Box::pin(f)
    }

    #[tokio::test]
    async fn test_independent_reads_run_concurrently() {
        let scheduler = ToolScheduler::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));

        let mut tasks: Vec<(ToolAccesses, BoxedTask<usize>)> = Vec::new();
        for i in 0..3 {
            let counter = counter.clone();
            let max_concurrent = max_concurrent.clone();
            let active = active.clone();
            let task = spawn_task(async move {
                let prev = active.fetch_add(1, Ordering::SeqCst);
                max_concurrent.fetch_max(prev + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                counter.fetch_add(1, Ordering::SeqCst);
                i
            });
            tasks.push((ToolAccesses::read_file(format!("/tmp/read{i}.txt")), task));
        }

        let start = Instant::now();
        let results = scheduler.run_batch(tasks).await;
        let elapsed = start.elapsed();

        assert_eq!(results, vec![0, 1, 2]);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert!(max_concurrent.load(Ordering::SeqCst) >= 2);
        assert!(elapsed < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn test_conflicting_read_and_write_run_sequentially() {
        let scheduler = ToolScheduler::new();
        let path = PathBuf::from("/tmp/shared.txt");
        let order = Arc::new(Mutex::new(Vec::new()));

        let order1 = order.clone();
        let task1 = spawn_task(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            order1.lock().unwrap().push(1);
            1
        });

        let order2 = order.clone();
        let task2 = spawn_task(async move {
            order2.lock().unwrap().push(2);
            2
        });

        let tasks = vec![
            (ToolAccesses::read_file(path.clone()), task1),
            (ToolAccesses::write_file(path), task2),
        ];

        let results = scheduler.run_batch(tasks).await;
        assert_eq!(results, vec![1, 2]);
        // Task 1 starts first but sleeps; task 2 should wait for it due to conflict.
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn test_preserves_declaration_order() {
        let scheduler = ToolScheduler::new();
        let tasks: Vec<(ToolAccesses, BoxedTask<&'static str>)> = vec![
            (ToolAccesses::none(), spawn_task(async { "a" })),
            (ToolAccesses::none(), spawn_task(async { "b" })),
            (ToolAccesses::none(), spawn_task(async { "c" })),
        ];
        let results = scheduler.run_batch(tasks).await;
        assert_eq!(results, vec!["a", "b", "c"]);
    }
}
