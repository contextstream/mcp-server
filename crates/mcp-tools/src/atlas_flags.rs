//! Gating for the legacy `atlas_remote_layer` capability surface.
//!
//! The public build retains this module for wire compatibility. Decisions combine
//! the server capability response with an optional per-product runtime override;
//! the public compatibility provider itself is always a no-op.

use mcp_types::atlas_layer::{AtlasProductId, AtlasRemoteCapabilities};

/// Result of gating a product for a given call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtlasProductGate {
    /// Product is available — safe to call.
    Allowed,
    /// Server-declared unavailable for this workspace (tier too low,
    /// or the server explicitly gated off via
    /// `plan.features.atlas_remote_layer.products.<name>=false`).
    DeniedByTier {
        /// Minimum plan name that includes this product, when known
        /// (`pro`, `elite`, `enterprise`).
        tier_required: Option<String>,
    },
    /// Operator env-var override is explicitly off (cheaper / safer
    /// kill-switch than redeploying). Not a user-visible tier issue.
    DeniedByEnvFlag,
    /// Server handshake is absent — we can't confirm tier entitlement
    /// at all. Tools should degrade silently (same as DeniedByTier
    /// from a UX perspective).
    NoHandshake,
}

impl AtlasProductGate {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Short human-readable reason for logs and tool-output markers.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::DeniedByTier { .. } => "denied_by_tier",
            Self::DeniedByEnvFlag => "denied_by_env_flag",
            Self::NoHandshake => "no_handshake",
        }
    }
}

/// Preferred env-var name for a given acceleration product. e.g.
/// `AtlasProductId::Search` → `CONTEXTSTREAM_ACCELERATION_SEARCH_ENABLED`.
pub fn env_flag_name(product: AtlasProductId) -> String {
    format!(
        "CONTEXTSTREAM_ACCELERATION_{}_ENABLED",
        product.as_str().to_ascii_uppercase()
    )
}

/// Deprecated MongoDB Atlas env-var alias. Accepted during the migration
/// window only when the preferred acceleration flag is absent.
pub fn legacy_atlas_env_flag_name(product: AtlasProductId) -> String {
    format!(
        "CONTEXTSTREAM_ATLAS_{}_ENABLED",
        product.as_str().to_ascii_uppercase()
    )
}

/// Parse an env-var value into an optional bool. Recognised truthy
/// values: `1`, `true`, `yes`, `on`, `enabled`. Falsy: `0`, `false`,
/// `no`, `off`, `disabled`. Empty string or anything else → None
/// (i.e., defer to the handshake).
pub fn parse_flag_value(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

/// Read the per-product env override for a product, returning
/// `Some(true|false)` if set to a recognised value, `None` otherwise.
pub fn read_env_flag(product: AtlasProductId) -> Option<bool> {
    let preferred = env_flag_name(product);
    if let Some(value) = std::env::var(&preferred)
        .ok()
        .and_then(|raw| parse_flag_value(&raw))
    {
        return Some(value);
    }

    let legacy = legacy_atlas_env_flag_name(product);
    std::env::var(&legacy)
        .ok()
        .and_then(|raw| parse_flag_value(&raw))
}

/// Compute the gate decision for a product. Callers pass the
/// capability snapshot from `SessionState.atlas_remote_capabilities`
/// (or `None` if unset). This function does NOT read the env twice
/// — it calls `read_env_flag` once internally.
pub fn gate_decision(
    product: AtlasProductId,
    capabilities: Option<&AtlasRemoteCapabilities>,
) -> AtlasProductGate {
    // Env kill-switch — `off` wins over everything else.
    if let Some(false) = read_env_flag(product) {
        return AtlasProductGate::DeniedByEnvFlag;
    }

    match capabilities {
        None => AtlasProductGate::NoHandshake,
        Some(caps) => {
            // Env `on` can also *force-enable* a product the server
            // says is unavailable — useful for staged rollouts where
            // a pod is promoted ahead of a tier-upgrade.
            if let Some(true) = read_env_flag(product) {
                return AtlasProductGate::Allowed;
            }
            match caps.product_available(product) {
                Some(true) => AtlasProductGate::Allowed,
                Some(false) | None => {
                    let tier_required = caps
                        .products
                        .iter()
                        .find(|p| p.name == product.as_str())
                        .map(|p| p.tier_required.clone());
                    AtlasProductGate::DeniedByTier { tier_required }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::atlas_layer::AtlasRemoteProductInfo;

    fn caps(enabled: bool, products: Vec<(&str, bool, &str)>) -> AtlasRemoteCapabilities {
        AtlasRemoteCapabilities {
            enabled,
            products: products
                .into_iter()
                .map(|(n, a, t)| AtlasRemoteProductInfo {
                    name: n.to_string(),
                    available: a,
                    tier_required: t.to_string(),
                })
                .collect(),
        }
    }

    fn clear_env(product: AtlasProductId) {
        std::env::remove_var(env_flag_name(product));
    }

    #[test]
    fn env_flag_name_uppercases_product() {
        assert_eq!(
            env_flag_name(AtlasProductId::Search),
            "CONTEXTSTREAM_ACCELERATION_SEARCH_ENABLED"
        );
        assert_eq!(
            env_flag_name(AtlasProductId::Vector),
            "CONTEXTSTREAM_ACCELERATION_VECTOR_ENABLED"
        );
        assert_eq!(
            env_flag_name(AtlasProductId::Functions),
            "CONTEXTSTREAM_ACCELERATION_FUNCTIONS_ENABLED"
        );
    }

    #[test]
    fn legacy_env_flag_name_keeps_atlas_alias() {
        assert_eq!(
            legacy_atlas_env_flag_name(AtlasProductId::Search),
            "CONTEXTSTREAM_ATLAS_SEARCH_ENABLED"
        );
    }

    #[test]
    fn parse_flag_value_accepts_common_forms() {
        for on in &["1", "true", "yes", "on", "ENABLED", "TrUe"] {
            assert_eq!(parse_flag_value(on), Some(true), "expected on for {}", on);
        }
        for off in &["0", "false", "no", "off", "DISABLED", "OFF"] {
            assert_eq!(
                parse_flag_value(off),
                Some(false),
                "expected off for {}",
                off
            );
        }
        for neither in &["", "maybe", "2", "please", " "] {
            assert_eq!(parse_flag_value(neither), None);
        }
    }

    #[test]
    fn no_handshake_yields_no_handshake_gate() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_env(AtlasProductId::Search);
        let gate = gate_decision(AtlasProductId::Search, None);
        assert!(matches!(gate, AtlasProductGate::NoHandshake));
        assert!(!gate.is_allowed());
    }

    #[test]
    fn available_product_is_allowed() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_env(AtlasProductId::Search);
        let c = caps(
            true,
            vec![("search", true, "pro"), ("vector", false, "pro")],
        );
        let gate = gate_decision(AtlasProductId::Search, Some(&c));
        assert!(gate.is_allowed());
    }

    #[test]
    fn unavailable_product_is_denied_by_tier() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_env(AtlasProductId::Vector);
        let c = caps(
            true,
            vec![("search", true, "pro"), ("vector", false, "pro")],
        );
        let gate = gate_decision(AtlasProductId::Vector, Some(&c));
        assert!(matches!(
            gate,
            AtlasProductGate::DeniedByTier { tier_required: Some(ref t) } if t == "pro"
        ));
    }

    #[test]
    fn disabled_layer_denies_every_product() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_env(AtlasProductId::Search);
        let c = caps(false, vec![("search", false, "pro")]);
        let gate = gate_decision(AtlasProductId::Search, Some(&c));
        assert!(!gate.is_allowed());
    }

    #[test]
    fn env_flag_off_wins_over_server_allow() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let product = AtlasProductId::Stream;
        std::env::set_var(env_flag_name(product), "off");
        let c = caps(true, vec![("stream", true, "elite")]);
        let gate = gate_decision(product, Some(&c));
        std::env::remove_var(env_flag_name(product));
        assert!(matches!(gate, AtlasProductGate::DeniedByEnvFlag));
    }

    #[test]
    fn env_flag_on_can_force_enable_over_server_deny() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let product = AtlasProductId::Functions;
        std::env::set_var(env_flag_name(product), "on");
        let c = caps(true, vec![("functions", false, "elite")]);
        let gate = gate_decision(product, Some(&c));
        std::env::remove_var(env_flag_name(product));
        assert!(gate.is_allowed());
    }

    #[test]
    fn env_flag_requires_handshake_to_force_on() {
        // Without a handshake, we can't know workspace tier — env
        // "on" alone shouldn't flip the gate, otherwise a free-tier
        // user with a misconfigured pod could bypass billing.
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let product = AtlasProductId::Vector;
        std::env::set_var(env_flag_name(product), "on");
        let gate = gate_decision(product, None);
        std::env::remove_var(env_flag_name(product));
        assert!(matches!(gate, AtlasProductGate::NoHandshake));
    }
}
