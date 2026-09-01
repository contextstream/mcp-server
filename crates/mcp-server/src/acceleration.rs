//! Acceleration layer construction for the remote HTTP gateway.
//!
//! The default local stdio build returns a no-op layer and never pulls
//! MongoDB-specific provider crates. The `remote-acceleration` build
//! wires MongoDB-free providers that call ContextStream server APIs.

use mcp_types::acceleration_layer::AccelerationLayer;

#[cfg(feature = "remote-acceleration")]
pub fn build_acceleration_layer() -> AccelerationLayer {
    mcp_acceleration_products::McpRemoteAccelerationLayer::from_env()
}

#[cfg(not(feature = "remote-acceleration"))]
pub fn build_acceleration_layer() -> AccelerationLayer {
    mcp_types::acceleration_layer::noop_acceleration_layer()
}

pub fn layer_summary(layer: &AccelerationLayer) -> String {
    if layer.is_enabled() {
        "enabled".to_string()
    } else if layer.has_connection() {
        "configured but disabled".to_string()
    } else {
        "noop".to_string()
    }
}
