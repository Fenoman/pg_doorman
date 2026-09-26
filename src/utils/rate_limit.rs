use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::time::{sleep_until, Duration, Instant};

#[derive(Debug)]
struct Message {
    sender: oneshot::Sender<()>,
}

#[derive(Clone, Debug)]
pub struct RateLimiter {
    sender: Sender<Message>,
    /// The receiver until its task starts: at once when `new` runs inside a
    /// tokio runtime, otherwise on the first `wait()`. The server builds the
    /// limiter before it starts the runtime.
    idle_receiver: Arc<Mutex<Option<Receiver<Message>>>>,
    count: usize,
    duration: Duration,
}

impl RateLimiter {
    pub fn new(count: usize, duration_in_ms: u64) -> Self {
        let duration = Duration::from_millis(duration_in_ms);
        let (sender, receiver) = channel(count);
        let limiter = Self {
            sender,
            idle_receiver: Arc::new(Mutex::new(Some(receiver))),
            count,
            duration,
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            limiter.start();
        }
        limiter
    }

    fn start(&self) {
        if let Some(receiver) = self.idle_receiver.lock().take() {
            RateLimiter::spawn_receiver(receiver, self.count, self.duration);
        }
    }

    /// two `.expect()` calls - if the spawned receiver
    /// task ever exited (panic in `spawn_receiver` body, future tokio
    /// bug closing the channel) every TLS handshake panicked. With the
    /// the panic hook (no more process exit), the per-client task would
    /// still die without a useful error message to the client. Now
    /// `wait()` returns `Result`; callers decide whether to fail the
    /// handshake gracefully or panic.
    pub async fn wait(&self) -> Result<(), &'static str> {
        self.start();
        let (s, r) = oneshot::channel::<()>();
        self.sender
            .send(Message { sender: s })
            .await
            .map_err(|_| "rate limit channel closed")?;
        r.await.map_err(|_| "rate limit oneshot closed")?;
        Ok(())
    }
    fn spawn_receiver(mut receiver: Receiver<Message>, count: usize, duration: Duration) {
        tokio::spawn(async move {
            // iServ backport: VecDeque turns the O(n) `Vec::remove(0)` calls
            // below into O(1) pop_front. Capacity is `count + 1` because the
            // length transiently equals `count` between front-evict and
            // back-push within one iteration.
            let mut queue: VecDeque<Instant> = VecDeque::with_capacity(count + 1);
            while let Some(message) = receiver.recv().await {
                let now = Instant::now();
                // Drop alarms whose time has already passed; freeze `now` so
                // the loop terminates instead of chasing tail-end items.
                while queue.front().is_some_and(|&t| t <= now) {
                    queue.pop_front();
                }
                // Off-by-one fix vs the original `> count`: when `queue.len()
                // == count` the next push would already break the contract,
                // so wait for the oldest alarm and pop it before admitting.
                if queue.len() >= count {
                    if let Some(&alarm) = queue.front() {
                        sleep_until(alarm).await;
                        queue.pop_front();
                        // Drain: scheduler latency may have overshot the
                        // alarm by enough that several additional entries are
                        // now also expired. Drain them too so the next
                        // iteration's drain isn't doing redundant work and
                        // we don't admit at a stale-throttled pace.
                        let now = Instant::now();
                        while queue.front().is_some_and(|&t| t <= now) {
                            queue.pop_front();
                        }
                    }
                }
                // The previous `.expect(...)` panicked the worker
                // when a caller dropped its oneshot receiver (cancellation,
                // shutdown). The panic killed this task, the mpsc never
                // closed from the sender side, and every subsequent `wait()`
                // blocked forever - entire rate limiter frozen for the
                // process lifetime. Ignoring a dropped peer is correct:
                // the slot is "wasted" on a no-show client but the limiter
                // keeps functioning for everyone else.
                let _ = message.sender.send(());
                queue.push_back(Instant::now() + duration);
            }
        });
    }
}

#[cfg(test)]
mod test {
    use super::RateLimiter;
    use std::time::Duration;
    use tokio::time::Instant;

    #[tokio::test]
    async fn up_to_limit_execute_quickly() {
        const COUNT: usize = 10;
        let limiter = RateLimiter::new(COUNT, 60000);
        let start = Instant::now();
        for _ in 0..COUNT {
            limiter.wait().await.expect("rate limiter healthy in test");
        }
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(10));
    }

    #[tokio::test]
    async fn over_limit_execute_proportionally() {
        const COUNT: usize = 10;
        const CHUNKS: usize = 3;
        let limiter = RateLimiter::new(COUNT, 1000);
        let start = Instant::now();
        for _ in 0..CHUNKS {
            for _ in 0..COUNT {
                limiter.wait().await.expect("rate limiter healthy in test");
            }
        }
        let elapsed = start.elapsed();
        assert!(elapsed > Duration::from_secs(CHUNKS as u64 - 1));
    }

    /// The server builds its TLS state, the limiter included, before it
    /// starts the tokio runtime. The limiter then works in that runtime.
    #[test]
    fn a_limiter_built_before_the_runtime_works_in_it() {
        let limiter = RateLimiter::new(2, 60000);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(5), limiter.wait())
                    .await
                    .expect("the limiter admits within its rate")
                    .expect("rate limiter healthy in test");
            }
        });
    }
}
