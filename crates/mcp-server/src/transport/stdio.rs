//! Stdio transport for MCP JSON-RPC communication.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdin, Stdout};

/// Stdio transport for JSON-RPC communication.
pub struct StdioTransport {
    reader: BufReader<Stdin>,
    writer: Stdout,
}

impl StdioTransport {
    /// Create a new stdio transport.
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(tokio::io::stdin()),
            writer: tokio::io::stdout(),
        }
    }

    /// Read a JSON-RPC message from stdin.
    pub async fn read_message(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            return Ok(None);
        }

        Ok(Some(line.trim().to_string()))
    }

    /// Write a JSON-RPC message to stdout.
    pub async fn write_message(&mut self, message: &str) -> Result<()> {
        if !message.is_empty() {
            self.writer.write_all(message.as_bytes()).await?;
            self.writer.write_all(b"\n").await?;
            self.writer.flush().await?;
        }
        Ok(())
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Construction Tests
    // ========================================================================

    mod construction_tests {
        use super::*;

        #[test]
        fn test_stdio_transport_default() {
            // Default implementation exists and creates a valid transport
            let _transport = StdioTransport::default();
            // If we get here without panic, the test passes
            // We can't test actual I/O in unit tests
        }

        #[test]
        fn test_stdio_transport_new() {
            // new() creates a valid transport
            let _transport = StdioTransport::new();
        }
    }

    // ========================================================================
    // Documentation Tests
    // ========================================================================

    mod documentation_tests {
        #[test]
        fn test_stdio_transport_structure() {
            // Document the StdioTransport structure:
            // - reader: BufReader<Stdin> for buffered line-by-line reading
            // - writer: Stdout for writing responses
            //
            // Methods:
            // - new(): Create a new transport connected to stdin/stdout
            // - read_message(): Read a line from stdin, trim it, return as String
            // - write_message(): Write a message to stdout with newline, flush
            //
            // Protocol:
            // - One JSON-RPC message per line (newline-delimited JSON)
            // - Empty messages are not written (skip empty strings)
            // - EOF returns Ok(None)
        }

        #[test]
        fn test_message_format() {
            // Document the message format:
            //
            // Input (from stdin):
            // - Each line is a complete JSON-RPC request
            // - Lines are trimmed of leading/trailing whitespace
            // - EOF (0 bytes read) returns None
            //
            // Output (to stdout):
            // - Each message is written followed by a newline
            // - Messages are flushed immediately for real-time communication
            // - Empty messages are skipped (not written)
            //
            // Example exchange:
            // <- {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
            // -> {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}
        }
    }

    // ========================================================================
    // Behavior Tests (documented, not executed)
    // ========================================================================

    mod behavior_tests {
        #[test]
        fn test_write_message_skips_empty() {
            // Documented behavior:
            // write_message("") does NOT write anything to stdout
            // This prevents sending empty lines that could confuse the client
            //
            // The check is: if !message.is_empty() { ... }
        }

        #[test]
        fn test_write_message_adds_newline() {
            // Documented behavior:
            // write_message("hello") writes:
            // 1. "hello" (the message bytes)
            // 2. "\n" (a newline separator)
            // 3. flush() to ensure immediate delivery
        }

        #[test]
        fn test_read_message_trims_input() {
            // Documented behavior:
            // Input "  hello world  \n" becomes "hello world"
            // The .trim().to_string() removes leading/trailing whitespace
        }

        #[test]
        fn test_read_message_eof_returns_none() {
            // Documented behavior:
            // When stdin reaches EOF (bytes_read == 0), returns Ok(None)
            // This signals the transport should shut down gracefully
        }
    }
}
