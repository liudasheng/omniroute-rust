//! Probes for the routing dispatch layer: the rate limiter, the breaker, and
//! the model-level cooldown. Each prints a verdict so a claim is checked
//! against the real code, not against prose.
//!
//! Run: cargo test --test router_dispatch_probe -- --nocapture

use omniroute_rust::router::circuit::CircuitStore;
use omniroute_rust::router::rate::RateLimiter;
use omniroute_rust::registry::AuthType;
use omniroute_rust::errors::FailureKind;

/// R1: the min-interval must be respected, but must not starve the second
/// request for longer than the interval itself.
#[tokio::test]
async fn r1_rate_limiter_min_interval() {
    let rl = RateLimiter::with_limits(10_000, 50);
    assert!(rl.allow("p"), "the first request must be admitted");
    assert!(!rl.allow("p"), "the second must wait for the min interval");
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(rl.allow("p"), "after the interval the request is admitted");
    println!("R1: min interval honoured");
}

/// R2: the RPM budget must be applied per provider, not globally.
#[tokio::test]
async fn r2_rate_limiter_is_per_provider() {
    let rl = RateLimiter::with_limits(2, 0);
    assert!(rl.allow("a"));
    assert!(rl.allow("a"));
    assert!(!rl.allow("a"), "the RPM budget is exhausted for provider a");
    assert!(rl.allow("b"), "provider b has its own budget");
    println!("R2: budgets are per provider");
}

/// R3: a provider that failed repeatedly must open its breaker, and a success
/// must close it again.
#[test]
fn r3_breaker_opens_and_closes() {
    let store = CircuitStore::new();
    let profile = omniroute_rust::router::circuit::BreakerProfile::effective(AuthType::ApiKey);
    for _ in 0..profile.breaker_failure_threshold {
        store.record_failure("p", FailureKind::Server, false, AuthType::ApiKey);
    }
    assert!(
        !store.is_available("p"),
        "the breaker must be open after the failure threshold"
    );
    store.record_success("p");
    assert!(
        store.is_available("p"),
        "a success must close the breaker and clear the cooldown"
    );
    println!("R3: breaker opens on repeated failures and closes on success");
}

/// R4: a model-level failure must not freeze the provider, and the model must
/// become usable again after its cooldown.
#[test]
fn r4_model_failure_does_not_freeze_the_provider() {
    let store = CircuitStore::new();
    store.record_model_failure("p", "broken-model", FailureKind::Server, false);
    assert!(store.is_model_cooling("p", "broken-model"));
    assert!(
        store.is_available("p"),
        "the provider must stay usable while one model cools down"
    );
    assert!(
        !store.is_model_cooling("p", "healthy-model"),
        "a sibling model must not be affected"
    );
    // The cooldown is short by design; after it the id is eligible again.
    std::thread::sleep(std::time::Duration::from_millis(31_000));
    assert!(
        !store.is_model_cooling("p", "broken-model"),
        "the model-level cooldown must expire on its own"
    );
    println!("R4: model cooldown is scoped and self-expiring");
}

/// R5: the concurrency limit must reject a saturated provider but must not
/// report a cooldown that will never expire.
#[test]
fn r5_saturation_has_no_cooldown() {
    let store = CircuitStore::with_limits(2);
    let _a = store.begin_request_guard("p");
    let _b = store.begin_request_guard("p");
    assert!(!store.is_available("p"), "the provider is saturated");
    assert_eq!(
        store.cooldown_remaining_ms("p"),
        0,
        "a saturated provider reports no cooldown — the client must know the wait is the in-flight count"
    );
    drop(_a);
    assert!(store.is_available("p"), "one slot frees up");
    println!("R5: saturation is not a cooldown");
}
