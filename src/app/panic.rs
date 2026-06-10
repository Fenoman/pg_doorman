//! Process-wide panic hook.
//!
//! A panic in a single Tokio worker task must not take down the whole pooler:
//! escalating every panic to `std::process::exit(1)` would turn any `.unwrap()`
//! on protocol-controlled input into a process-killing DoS vector, dropping
//! every other client connection.
//!
//! The hook logs the panic with full context (panic message,
//! location, thread name, backtrace if available) and lets the default
//! unwinding take over. Tokio's default panic policy aborts only the panicking
//! task; the rest of the runtime continues. A panic on the main thread is
//! still process-fatal because the runtime is anchored there - that matches
//! standard Rust semantics and is the correct behaviour for unrecoverable
//! startup-time failures.
//!
//! Per-task panics are counted via [`WORKER_PANIC_COUNT`] so operators can
//! alert on a non-zero rate via the admin SHOW STATS / Prometheus surface.

use log::error;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of panics observed in any thread since process start.
///
/// Surfaces a "something panicked but the process survived" signal to
/// operators. The counter is process-global and monotonic.
pub static WORKER_PANIC_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn install_panic_hook() {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        WORKER_PANIC_COUNT.fetch_add(1, Ordering::Relaxed);

        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&'static str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        error!(
            "panic in thread '{thread_name}' at {location}: {payload} (total panics: {})",
            WORKER_PANIC_COUNT.load(Ordering::Relaxed)
        );

        // Delegate to the default hook so the panic is fully reported on
        // stderr with a backtrace (when RUST_BACKTRACE=1) and tokio's
        // task-isolation can unwind the panicking task without bringing
        // down the entire runtime.
        default_panic(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// verify C1 contract - panic in a tokio task does NOT
    /// terminate the process, WORKER_PANIC_COUNT increments, the
    /// surrounding runtime continues. A future refactor that
    /// reintroduces `exit(1)` would break this test.
    #[tokio::test]
    #[serial_test::serial]
    async fn panic_in_task_does_not_terminate_process_and_counts() {
        install_panic_hook();

        let before = WORKER_PANIC_COUNT.load(Ordering::Relaxed);

        // Spawn a task that panics. tokio task isolation should
        // contain the panic; the JoinHandle yields Err but the test
        // continues running.
        let h = tokio::spawn(async {
            panic!("intentional test panic - C1 contract verification");
        });
        let res = h.await;
        assert!(res.is_err(), "panicking task should yield JoinError");

        // Give the panic hook time to run (set_hook synchronously
        // invokes our closure during unwind).
        let after = WORKER_PANIC_COUNT.load(Ordering::Relaxed);
        assert!(
            after > before,
            "WORKER_PANIC_COUNT must increment on task panic (before={before}, after={after})"
        );

        // Process is still alive: assert we can do other work.
        let v: i32 = tokio::spawn(async { 42 }).await.unwrap();
        assert_eq!(v, 42);
    }
}
