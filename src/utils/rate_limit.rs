use std::collections::VecDeque;
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
}

impl RateLimiter {
    pub fn new(count: usize, duration_in_ms: u64) -> Self {
        let duration = Duration::from_millis(duration_in_ms);
        let (sender, receiver) = channel(count);
        RateLimiter::spawn_receiver(receiver, count, duration);
        Self { sender }
    }

    /// two `.expect()` calls - if the spawned receiver
    /// task ever exited (panic in `spawn_receiver` body, future tokio
    /// bug closing the channel) every TLS handshake panicked. With the
    /// the panic hook (no more process exit), the per-client task would
    /// still die without a useful error message to the client. Now
    /// `wait()` returns `Result`; callers decide whether to fail the
    /// handshake gracefully or panic.
    pub async fn wait(&self) -> Result<(), &'static str> {
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
}
