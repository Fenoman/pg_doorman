use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tokio::time::{Duration, Instant};

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

    pub async fn wait(&self) {
        let (s, r) = oneshot::channel::<()>();
        self.sender
            .send(Message { sender: s })
            .await
            .expect("unable to send to rate limit channel");
        r.await.expect("unable to read from rate limit channel");
    }
    fn spawn_receiver(mut receiver: Receiver<Message>, count: usize, duration: Duration) {
        tokio::spawn(async move {
            let mut queue = std::collections::VecDeque::with_capacity(count + 1);
            while let Some(message) = receiver.recv().await {
                let now = Instant::now();
                while queue.front().map_or(false, |&t| t <= now) {
                    queue.pop_front();
                }
                if queue.len() >= count {
                    if let Some(&alarm) = queue.front() {
                        sleep(alarm - Instant::now()).await;
                        queue.pop_front();
                    }
                }
                message
                    .sender
                    .send(())
                    .expect("unable to send to rate limiter client channel");
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
            limiter.wait().await;
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
                limiter.wait().await;
            }
        }
        let elapsed = start.elapsed();
        assert!(elapsed > Duration::from_secs(CHUNKS as u64 - 1));
    }
}
