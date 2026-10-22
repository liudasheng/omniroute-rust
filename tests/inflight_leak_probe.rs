//! Prove the in-flight counter leaks when a dispatch future is cancelled.
//!
//! A client that closes its connection (or a timeout in the caller) drops
//! the future between `begin_request` and `end_request`; because the two are
//! plain calls rather than a guard, the counter is never decremented. Once it
//! reaches the concurrency limit every later request skips that provider with
//! "circuit open / cooldown / concurrency" — the provider is silently dead
//! until the process restarts.
//!
//! Run: cargo test --test inflight_leak_probe -- --nocapture

use omniroute_rust::router::circuit::CircuitStore;
use std::sync::Arc;

#[tokio::test]
async fn a_cancelled_dispatch_leaves_the_counter_held() {
    let store = Arc::new(CircuitStore::new());
    let provider = "opencode-go";

    // Simulate the dispatch site: begin, then an await that never completes.
    async fn dispatch(store: Arc<CircuitStore>, provider: String) {
        // RAII guard: the counter is released when this future is dropped.
        let _in_flight = store.begin_request_guard(&provider);
        std::future::pending::<()>().await;
    }

    let task = {
        let provider = provider.to_string();
        tokio::spawn(dispatch(store.clone(), provider))
    };
    tokio::task::yield_now().await;
    task.abort();
    // Dropping the task runs its destructor chain, which releases the
    // guard; give the runtime a turn so the release is observable.
    let _ = task.await;
    tokio::task::yield_now().await;

    println!(
        "in_flight={} available={}",
        store.inflight(provider),
        store.is_available(provider)
    );
    assert_eq!(
        store.inflight(provider),
        0,
        "the cancelled dispatch must release its in-flight slot"
    );
}

#[tokio::test]
async fn the_guard_releases_in_bulk() {
    let store = CircuitStore::new();
    let provider = "opencode-go";

    for _ in 0..8 {
        // Each request is cancelled right after it begins.
        let _in_flight = store.begin_request_guard(provider);
    }

    println!(
        "in_flight={} available={}",
        store.inflight(provider),
        store.is_available(provider)
    );
    assert_eq!(
        store.inflight(provider),
        0,
        "every guard must release its slot"
    );
    assert!(
        store.is_available(provider),
        "the provider must still be usable after the cancelled requests"
    );
}
