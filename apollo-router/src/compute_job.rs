use std::future::Future;
use std::panic::UnwindSafe;
use std::sync::OnceLock;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::metrics::ObservableGauge;
use tokio::sync::oneshot;

use crate::ageing_priority_queue::AgeingPriorityQueue;
pub(crate) use crate::ageing_priority_queue::Priority;
use crate::metrics::meter_provider;

/// We generate backpressure in tower `poll_ready` when the number of queued jobs
/// reaches `QUEUE_SOFT_CAPACITY_PER_THREAD * thread_pool_size()`
const QUEUE_SOFT_CAPACITY_PER_THREAD: usize = 20;

/// Leave a fraction of CPU cores free to run Tokio threads even if this thread pool is very busy:
///
/// available: 1     pool size: 1
/// available: 2     pool size: 1
/// available: 3     pool size: 2
/// available: 4     pool size: 3
/// available: 5     pool size: 4
/// ...
/// available: 8     pool size: 7
/// available: 9     pool size: 7
/// ...
/// available: 16    pool size: 14
/// available: 17    pool size: 14
/// ...
/// available: 32    pool size: 28
fn thread_pool_size() -> usize {
    let available = std::thread::available_parallelism()
        .expect("available_parallelism() failed")
        .get();
    thread_poll_size_for_available_parallelism(available)
}

fn thread_poll_size_for_available_parallelism(available: usize) -> usize {
    let reserved = available.div_ceil(8);
    (available - reserved).max(1)
}

type Job = Box<dyn FnOnce() + Send + 'static>;

fn queue() -> &'static AgeingPriorityQueue<Job> {
    static QUEUE: OnceLock<AgeingPriorityQueue<Job>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let pool_size = thread_pool_size();
        for _ in 0..pool_size {
            std::thread::spawn(|| {
                // This looks like we need the queue before creating the queue,
                // but it happens in a child thread where OnceLock will block
                // until `get_or_init` in the parent thread is finished
                // and the parent is *not* blocked on the child thread making progress.
                let queue = queue();

                let mut receiver = queue.receiver();
                loop {
                    let job = receiver.blocking_recv();
                    job();
                }
            });
        }
        AgeingPriorityQueue::soft_bounded(QUEUE_SOFT_CAPACITY_PER_THREAD * pool_size)
    })
}

/// Returns a future that resolves to a `Result` that is `Ok` if `f` returned or `Err` if it panicked.
pub(crate) fn execute<T, F>(
    priority: Priority,
    job: F,
) -> impl Future<Output = std::thread::Result<T>>
where
    F: FnOnce() -> T + Send + UnwindSafe + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let job = Box::new(move || {
        // Ignore the error if the oneshot receiver was dropped
        let _ = tx.send(std::panic::catch_unwind(job));
    });
    queue().send(priority, job);
    async { rx.await.expect("channel disconnected") }
}

pub(crate) fn is_full() -> bool {
    queue().is_full()
}

pub(crate) fn create_queue_size_gauge() -> ObservableGauge<u64> {
    meter_provider()
        .meter("apollo/router")
        .u64_observable_gauge("apollo.router.compute_jobs.queued")
        .with_description(
            "Number of computation jobs (parsing, planning, …) waiting to be scheduled",
        )
        .with_callback(move |m| m.observe(queue().queued_count() as u64, &[]))
        .init()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn test_executes_on_different_thread() {
        let test_thread = std::thread::current().id();
        let job_thread = execute(Priority::P4, || std::thread::current().id())
            .await
            .unwrap();
        assert_ne!(job_thread, test_thread)
    }

    #[tokio::test]
    async fn test_parallelism() {
        if thread_pool_size() < 2 {
            return;
        }
        let start = Instant::now();
        let one = execute(Priority::P8, || {
            std::thread::sleep(Duration::from_millis(1_000));
            1
        });
        let two = execute(Priority::P8, || {
            std::thread::sleep(Duration::from_millis(1_000));
            1 + 1
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(one.await.unwrap(), 1);
        assert_eq!(two.await.unwrap(), 2);
        // Evidence of fearless parallel sleep:
        assert!(start.elapsed() < Duration::from_millis(1_400));
    }

    #[test]
    fn pool_size() {
        assert_eq!(thread_poll_size_for_available_parallelism(1), 1);
        assert_eq!(thread_poll_size_for_available_parallelism(2), 1);
        assert_eq!(thread_poll_size_for_available_parallelism(3), 2);
        assert_eq!(thread_poll_size_for_available_parallelism(4), 3);
        assert_eq!(thread_poll_size_for_available_parallelism(31), 27);
        assert_eq!(thread_poll_size_for_available_parallelism(32), 28);
    }
}
