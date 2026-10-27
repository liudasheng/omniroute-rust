//! Which providers can report real upstream usage, and how to ask them.
//!
//! Two shapes exist in the wild and both must be supported:
//!
//!  * **windowed** — the subscription reports rolling/weekly/monthly windows as
//!    a percentage used (OpenCode). Routing can gate on an exhausted window.
//!  * **balance** — a prepaid account reports money left (`total_credits -
//!    total_usage`, `/user/balance`). There is no reset time; the account simply
//!    stops working when it hits zero.
//!
//! A provider's *credential* is the unit that carries the subscription: two
//! provider ids registered with the same key share one quota, so readings are
//! stored and presented per credential rather than per provider id.
//!
//! Adding a provider is one [`UsageProbe`] implementation plus an entry in
//! [`probes`] — the dashboard picks it up automatically.

/// How an upstream reports usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageShape {
    /// Rolling/weekly/monthly windows, as a percentage already used.
    Windowed,
    /// A prepaid balance: money left, or money spent out of a grant.
    Balance,
}

impl UsageShape {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Windowed => "windowed",
            Self::Balance => "balance",
        }
    }
}

/// One family of providers that share a usage endpoint.
pub trait UsageProbe: Send + Sync {
    /// Stable id used as the grouping key in the dashboard response.
    fn id(&self) -> &'static str;

    /// Does this probe cover `provider`?
    fn supports(&self, provider: &str) -> bool;

    /// What kind of numbers this endpoint returns.
    fn shape(&self) -> UsageShape;

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

    fn shape(&self) -> UsageShape {
        UsageShape::Windowed
    }

    fn usage_url(&self, base: &str) -> String {
        crate::opencode_quota::usage_url(base)
    }
}

/// OpenRouter: prepaid credits.
///
/// Live-checked 2026-10-27 — `{"data":{"total_credits":10,"total_usage":9.57}}`,
/// so the balance is the difference. The host is fixed rather than derived from
/// the chat base (`/api/v1` vs `/api/v1/chat/completions`).
pub struct OpenRouterProbe;

impl UsageProbe for OpenRouterProbe {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn supports(&self, provider: &str) -> bool {
        provider == "openrouter"
    }

    fn shape(&self) -> UsageShape {
        UsageShape::Balance
    }

    fn usage_url(&self, _base: &str) -> String {
        "https://openrouter.ai/api/v1/credits".to_string()
    }
}

/// DeepSeek: prepaid balance.
///
/// `GET /user/balance` answers 401 without a key (endpoint exists) and returns
/// `{"balance_infos":[{"total_balance":"12.34","currency":"CNY"}]}`.
pub struct DeepSeekProbe;

impl UsageProbe for DeepSeekProbe {
    fn id(&self) -> &'static str {
        "deepseek"
    }

    fn supports(&self, provider: &str) -> bool {
        provider == "deepseek"
    }

    fn shape(&self) -> UsageShape {
        UsageShape::Balance
    }

    fn usage_url(&self, _base: &str) -> String {
        "https://api.deepseek.com/user/balance".to_string()
    }
}

/// Moonshot / Kimi: `{"data":{"available_balance":12.34,...}}`.
pub struct MoonshotProbe;

impl UsageProbe for MoonshotProbe {
    fn id(&self) -> &'static str {
        "moonshot"
    }

    fn supports(&self, provider: &str) -> bool {
        matches!(provider, "moonshot" | "moonshot-cn" | "kimi")
    }

    fn shape(&self) -> UsageShape {
        UsageShape::Balance
    }

    fn usage_url(&self, _base: &str) -> String {
        "https://api.moonshot.cn/v1/users/me/balance".to_string()
    }
}

/// SiliconFlow: `{"data":{"balance":"12.34","totalBalance":"..."}}`.
pub struct SiliconFlowProbe;

impl UsageProbe for SiliconFlowProbe {
    fn id(&self) -> &'static str {
        "siliconflow"
    }

    fn supports(&self, provider: &str) -> bool {
        matches!(provider, "siliconflow" | "siliconflow-cn")
    }

    fn shape(&self) -> UsageShape {
        UsageShape::Balance
    }

    fn usage_url(&self, _base: &str) -> String {
        "https://api.siliconflow.cn/v1/user/info".to_string()
    }
}

/// Every probe the gateway knows about.
///
/// Order matters only for grouping; [`probes_do_not_overlap`] keeps each
/// provider matching exactly one.
pub fn probes() -> &'static [&'static dyn UsageProbe] {
    static PROBES: &[&dyn UsageProbe] = &[
        &OpencodeProbe,
        &OpenRouterProbe,
        &DeepSeekProbe,
        &MoonshotProbe,
        &SiliconFlowProbe,
    ];
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
        assert_eq!(probe.shape(), UsageShape::Windowed);
        for alias in ["opencode", "opencode-zen", "opencode-go"] {
            assert!(probe.supports(alias), "{alias} must be covered");
        }
    }

    /// Balance reporters must be distinguishable: the dashboard renders
    /// "12.34 USD" or "62% used" depending on this.
    #[test]
    fn balance_probes_are_marked_as_such() {
        for (provider, id) in [
            ("openrouter", "openrouter"),
            ("deepseek", "deepseek"),
            ("moonshot", "moonshot"),
            ("siliconflow", "siliconflow"),
        ] {
            let probe = probe_for(provider).unwrap_or_else(|| panic!("{provider} must have a probe"));
            assert_eq!(probe.id(), id);
            assert_eq!(probe.shape(), UsageShape::Balance, "{provider}");
        }
    }

    /// A provider whose usage we cannot read must not advertise a probe; the
    /// dashboard then falls back to manual values instead of inventing numbers.
    #[test]
    fn providers_without_an_endpoint_have_no_probe() {
        for provider in [
            "openai-compatible-local",
            "openai-compatible-dev-one-blog-top",
            "qwen-cloud-token-plan",
            "groq",
            "mistral",
        ] {
            assert!(probe_for(provider).is_none(), "{provider} must not advertise usage");
        }
    }

    /// Each provider resolves to exactly one probe, never two.
    #[test]
    fn probes_do_not_overlap() {
        for provider in [
            "opencode", "opencode-zen", "opencode-go", "openrouter", "deepseek",
            "moonshot", "moonshot-cn", "kimi", "siliconflow", "siliconflow-cn",
        ] {
            let hits = probes().iter().filter(|p| p.supports(provider)).count();
            assert_eq!(hits, 1, "{provider} must match exactly one probe, saw {hits}");
        }
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
