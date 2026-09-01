//! Error types for the MCP server.

use std::fmt;
use thiserror::Error;

/// Result type alias using our Error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for the MCP server.
#[derive(Debug, Error)]
pub enum Error {
    /// HTTP request error
    #[error("HTTP error ({status}): {message}")]
    Http {
        status: u16,
        message: String,
        code: ErrorCode,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Network error (connection failed, timeout, etc.)
    #[error("Network error: {0}")]
    Network(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Missing credentials
    #[error(
        "Missing credentials: Set CONTEXTSTREAM_API_KEY or CONTEXTSTREAM_JWT for authentication"
    )]
    MissingCredentials,

    /// Validation error
    #[error("Validation error: {0}")]
    Validation(String),

    /// Tool execution error
    #[error("Tool error: {0}")]
    Tool(String),

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// UUID parsing error
    #[error("Invalid UUID: {0}")]
    InvalidUuid(#[from] uuid::Error),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Rate limited
    #[error("Rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<u64>,
    },

    /// Insufficient credits to perform the operation
    #[error("{message}")]
    InsufficientCredits {
        message: String,
        required: Option<i32>,
        available: Option<i32>,
    },

    /// Feature not available on current plan
    #[error("Feature not available: {0}")]
    PlanRestriction(String),

    /// Integration not connected
    #[error("Integration not connected: {0}")]
    IntegrationNotConnected(String),

    /// Request cancelled
    #[error("Request cancelled")]
    Cancelled,

    /// Request timeout
    #[error("Request timeout after {0} seconds")]
    Timeout(u64),
}

/// Whether a text message describes the known non-blocking Tree-sitter parser
/// failure emitted by the API during best-effort startup/index work.
pub fn is_non_blocking_parser_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let mentions_parser_error = message.contains("ParserError")
        || lower.contains("parser_error")
        || (lower.contains("parser error") && lower.contains("tree-sitter"));
    let marked_non_blocking = lower.contains("non-blocking")
        || lower.contains("non_blocking")
        || lower.contains("nonblocking");

    mentions_parser_error && marked_non_blocking
}

impl Error {
    /// Create an HTTP error from status and message.
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        let code = ErrorCode::from_status(status);
        Self::Http {
            status,
            message,
            code,
            source: None,
        }
    }

    /// Create an HTTP error with a specific code.
    pub fn http_with_code(status: u16, message: impl Into<String>, code: ErrorCode) -> Self {
        Self::Http {
            status,
            message: message.into(),
            code,
            source: None,
        }
    }

    /// Get the error code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Http { code, .. } => *code,
            Self::Network(_) => ErrorCode::NetworkError,
            Self::Config(_) => ErrorCode::BadRequest,
            Self::MissingCredentials => ErrorCode::Unauthorized,
            Self::Validation(_) => ErrorCode::ValidationError,
            Self::Tool(_) => ErrorCode::InternalError,
            Self::Serialization(_) => ErrorCode::BadRequest,
            Self::InvalidUuid(_) => ErrorCode::ValidationError,
            Self::Io(_) => ErrorCode::InternalError,
            Self::RateLimited { .. } => ErrorCode::RateLimited,
            Self::InsufficientCredits { .. } => ErrorCode::PaymentRequired,
            Self::PlanRestriction(_) => ErrorCode::Forbidden,
            Self::IntegrationNotConnected(_) => ErrorCode::NotFound,
            Self::Cancelled => ErrorCode::NetworkError,
            Self::Timeout(_) => ErrorCode::GatewayTimeout,
        }
    }

    /// Check if this error is retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http { status, .. } => matches!(status, 408 | 429 | 500 | 502 | 503 | 504),
            Self::Network(_) => true,
            Self::Timeout(_) => true,
            Self::RateLimited { .. } => true,
            _ => false,
        }
    }

    /// Whether this error is a known non-blocking Tree-sitter parser failure
    /// reported by the API during best-effort startup/index work.
    ///
    /// These parser failures are useful in server-side logs but should not be
    /// surfaced verbatim to MCP clients because they are not actionable and do
    /// not prevent the session from continuing.
    pub fn is_non_blocking_parser_error(&self) -> bool {
        let message = match self {
            Self::Http { message, .. }
            | Self::Network(message)
            | Self::Config(message)
            | Self::Validation(message)
            | Self::Tool(message)
            | Self::PlanRestriction(message)
            | Self::IntegrationNotConnected(message)
            | Self::RateLimited { message, .. }
            | Self::InsufficientCredits { message, .. } => message.as_str(),
            Self::MissingCredentials
            | Self::Serialization(_)
            | Self::InvalidUuid(_)
            | Self::Io(_)
            | Self::Cancelled
            | Self::Timeout(_) => return false,
        };

        is_non_blocking_parser_error_message(message)
    }

    /// User-facing error text with known internal/non-blocking implementation
    /// details removed. The original error should still be sent to tracing logs
    /// before this sanitized message is returned to a client.
    pub fn user_facing_message(&self) -> String {
        if self.is_non_blocking_parser_error() {
            return "ContextStream completed with an internal parse warning. The session can continue."
                .to_string();
        }

        self.to_string()
    }
}

/// Error codes matching the TypeScript implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NetworkError,
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    ValidationError,
    RateLimited,
    PaymentRequired,
    InternalError,
    BadGateway,
    ServiceUnavailable,
    GatewayTimeout,
    UnknownError,
}

impl ErrorCode {
    /// Convert HTTP status to error code.
    pub fn from_status(status: u16) -> Self {
        match status {
            0 => Self::NetworkError,
            400 => Self::BadRequest,
            401 => Self::Unauthorized,
            402 => Self::PaymentRequired,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            409 => Self::Conflict,
            422 => Self::ValidationError,
            429 => Self::RateLimited,
            500 => Self::InternalError,
            502 => Self::BadGateway,
            503 => Self::ServiceUnavailable,
            504 => Self::GatewayTimeout,
            _ => Self::UnknownError,
        }
    }

    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NetworkError => "NETWORK_ERROR",
            Self::BadRequest => "BAD_REQUEST",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::Conflict => "CONFLICT",
            Self::ValidationError => "VALIDATION_ERROR",
            Self::RateLimited => "RATE_LIMITED",
            Self::PaymentRequired => "PAYMENT_REQUIRED",
            Self::InternalError => "INTERNAL_ERROR",
            Self::BadGateway => "BAD_GATEWAY",
            Self::ServiceUnavailable => "SERVICE_UNAVAILABLE",
            Self::GatewayTimeout => "GATEWAY_TIMEOUT",
            Self::UnknownError => "UNKNOWN_ERROR",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_code_from_status() {
        assert_eq!(ErrorCode::from_status(400), ErrorCode::BadRequest);
        assert_eq!(ErrorCode::from_status(401), ErrorCode::Unauthorized);
        assert_eq!(ErrorCode::from_status(429), ErrorCode::RateLimited);
        assert_eq!(ErrorCode::from_status(999), ErrorCode::UnknownError);
    }

    #[test]
    fn test_error_retryable() {
        assert!(Error::http(429, "rate limited").is_retryable());
        assert!(Error::http(503, "service unavailable").is_retryable());
        assert!(Error::Timeout(30).is_retryable());
        assert!(Error::Network("connection reset".to_string()).is_retryable());
        assert!(!Error::http(400, "bad request").is_retryable());
        assert!(!Error::http(401, "unauthorized").is_retryable());
    }

    #[test]
    fn test_non_blocking_parser_error_is_sanitized_for_users() {
        let err = Error::http(
            500,
            "Failed with non-blocking status code: ParserError: failed to parse AST",
        );

        assert!(err.is_non_blocking_parser_error());
        assert!(!err.user_facing_message().contains("ParserError"));
        assert!(!err
            .user_facing_message()
            .contains("non-blocking status code"));
    }

    #[test]
    fn test_regular_parser_error_context_is_not_suppressed() {
        let err = Error::Validation("parser error in user input".to_string());

        assert!(!err.is_non_blocking_parser_error());
        assert_eq!(err.user_facing_message(), err.to_string());
    }
}
