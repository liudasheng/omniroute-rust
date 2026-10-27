//! Which providers can report real upstream usage, and how to ask them.
//!
//! A provider's *credential* is the unit that carries a subscription: two
//! provider ids registered with the same key (`opencode-go` and `opencode-zen`
//! on one OpenCode subscription) share one quota, so readings are stored and
//! presented per credential rather than per provider id.
//!
//! Adding a provider is one implementation of [`UsageProbe`] plus an entry in
//! [`probes`] — the dashboard picks it up automatically. Nothing here knows
//! about HTTP; [`crate::opencode_quota`] owns the request/parse machinery so the
//! routing gate and the dashboard keep reading the same numbers.

/// One family of providers that share a usage endpoint.
pub trait UsageProbe: Send + Sync {
    /// Stable id used as the grouping key in the dashboard response.
    fn id(&self) -> &'static str;

    /// Does this probe cover `provider`?
    fn supports(&self, provider: &str) -> bool;

    /// URL to GET, given the provider's resolved base URL.
    fn usage_url(&self, base: &str) -> String;
}

/// The OpenCode subscription tiers.
///
/// `opencode`, `opencode-zen` and `opencode-go` all authenticate against the
/// same account pool and answer from `/usage` on the Go base. Live-checked
/// 2026-10-27: a Zen key returns the same payload shape as a Go key, and two
/// connections holding the same key report identical numbers — which is exactly
/// why the dashboard merges them.
pub struct OpencodeProbe;

impl UsageProbe for OpencodeProbe {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn supports(&self, provider: &str) -> bool {
        matches!(provider, "opencode" | "opencode-zen" | "opencode-go")
    }

    fn usage_url(&self, base: &str) -> String {
        crate::opencode_quota::usage_url(base)
    }
}

/// Every probe the gateway knows about.
pub fn probes() -> &'static [&'static dyn UsageProbe] {
    static PROBES: &[&dyn UsageProbe] = &[&OpencodeProbe];
    PROBES
}

/// The probe covering `provider`, if any.
pub fn probe_for(provider: &str) -> Option<&'static dyn UsageProbe> {
    probes().iter().copied().find(|p| p.supports(provider))
}

/// Stable, non-reversible id for a credential.
///
/// Readings are keyed by this instead of the raw key: the dashboard needs to
/// know *which connections share a quota*, not what the secret is, and a
/// fingerprint cannot leak one.
pub fn credential_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opencode_probe_covers_every_opencode_alias() {
        let probe = probe_for("opencode-go").expect("go tier");
        assert_eq!(probe.id(), "opencode");
        for alias in ["opencode", "opencode-zen", "opencode-go"] {
            assert!(probe.supports(alias), "{alias} must be covered");
        }
        assert!(probe_for("openrouter").is_none(), "no probe for api-key providers yet");
        assert!(probe_for("openai-compatible-local").is_none());
    }

    /// The fingerprint must be stable (so it can key a map across refreshes) and
    /// must not carry the secret.
    #[test]
    fn credential_ids_are_stable_and_opaque() {
        let key = "example-credential-value";
        let id = credential_id(key);
        assert_eq!(id, credential_id(key), "the same key must hash the same way");
        assert_ne!(id, credential_id("example-credential-valuE"));
        assert_eq!(id.len(), 16);
        assert!(!id.contains("example-credential"), "the fingerprint must not embed the key");
    }
}
