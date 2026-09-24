//! Deadlines for operations on hot paths that are usually ready at once.
//!
//! Creating a tokio timer clones the runtime handle, a reference count shared
//! by every worker thread. Polling the operation once first avoids that when
//! it completes immediately; only a pending operation gets the deadline.

use std::future::Future;
use std::task::Poll;
use std::time::Duration;

use tokio::time::error::Elapsed;
use tokio::time::Instant;

/// `tokio::time::timeout` that creates its timer only if `future` is pending.
pub async fn timeout_unless_ready<F>(duration: Duration, future: F) -> Result<F::Output, Elapsed>
where
    F: Future,
{
    let mut future = std::pin::pin!(future);
    if let Poll::Ready(output) = futures::poll!(future.as_mut()) {
        return Ok(output);
    }
    tokio::time::timeout(duration, future).await
}

/// `tokio::time::timeout_at` that creates its timer only if `future` is pending.
pub async fn timeout_at_unless_ready<F>(deadline: Instant, future: F) -> Result<F::Output, Elapsed>
where
    F: Future,
{
    let mut future = std::pin::pin!(future);
    if let Poll::Ready(output) = futures::poll!(future.as_mut()) {
        return Ok(output);
    }
    tokio::time::timeout_at(deadline, future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_future_completes_without_waiting() {
        assert_eq!(
            timeout_unless_ready(Duration::ZERO, async { 7 }).await,
            Ok(7)
        );
        assert_eq!(
            timeout_at_unless_ready(Instant::now(), async { 7 }).await,
            Ok(7)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_future_gets_the_deadline() {
        let never = std::future::pending::<()>();
        assert!(timeout_unless_ready(Duration::from_millis(50), never)
            .await
            .is_err());
        let never = std::future::pending::<()>();
        assert!(
            timeout_at_unless_ready(Instant::now() + Duration::from_millis(50), never)
                .await
                .is_err()
        );
    }
}
