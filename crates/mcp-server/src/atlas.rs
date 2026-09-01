//! Compatibility layer for deployments that previously supplied premium
//! product providers from a separate implementation crate.
//!
//! The open-source server deliberately ships only the no-op implementation.
//! Hosted acceleration is wired through [`crate::acceleration`] and the
//! MongoDB-free `mcp-acceleration-products` crate.

use mcp_types::atlas_layer::AtlasLayer;

/// Build the compatibility layer used by the public binary.
pub fn build_atlas_layer() -> AtlasLayer {
    mcp_types::atlas_layer::noop_layer()
}

/// Short label for startup diagnostics.
pub fn layer_summary(_layer: &AtlasLayer) -> String {
    "noop (premium provider implementation not included)".to_string()
}
