//! Session management for the ContextStream MCP server.
//!
//! This crate provides session tracking, auto-initialization,
//! and context pressure detection.

pub mod auto_init;
pub mod checkout_identity;
pub mod grounding_state;
pub mod manager;
pub mod session_model_cache;

pub use checkout_identity::{
    current_checkout_fingerprint, current_repository_canonical_url, current_repository_fingerprint,
    current_repository_remote_identity, validate_checkout_binding, CheckoutId,
    CheckoutIdentityError, CheckoutIdentityKind, RepositoryFingerprint, RepositoryRemoteIdentity,
    ValidatedCheckoutBinding,
};
pub use manager::{ChildProjectInfo, ProjectRelationKind, RelatedProjectInfo, SessionManager};
