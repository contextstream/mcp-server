//! Stable content-hash plumbing for ContextStream rules files.
//!
//! Background: locally-installed rules files (CLAUDE.md, .cursorrules,
//! AGENTS.md, etc.) are written by the MCP binary and need to be
//! refreshed when the binary's bundled rule content changes. The cloud
//! server emits `[RULES_NOTICE]` when `rules_version` (= `CARGO_PKG_VERSION`)
//! moves forward, which works for releases but misses content-only edits
//! where we don't bump the package version.
//!
//! This module gives every crate access to two small, deterministic
//! primitives so the staleness check can be content-aware instead of
//! version-aware:
//!
//! * [`fnv1a_64_hex`] — 64-bit FNV-1a hash rendered as 16 hex chars. Stable
//!   across runs and Rust versions, no extra deps. Fine for staleness
//!   detection (collision-resistant for our purposes; not cryptographic).
//! * [`HASH_MARKER_PREFIX`] / [`extract_hash_marker`] — the comment
//!   marker embedded into every `<contextstream>` block so the binary can
//!   later read which canonical teaching bundle wrote the file.
//!
//! Producers (`mcp-server`) call [`set_canonical_rules_hash`] at startup
//! with a deterministic fingerprint covering every supported editor's
//! bundled teaching surfaces. Consumers (`mcp-client`, `mcp-tools`) read it
//! via [`canonical_rules_hash`] to send up to the server and compare against
//! locally-installed files. A single bundle fingerprint is intentional:
//! editor formatting and workspace identity must not make a just-written
//! file disagree with the process-global staleness check.

use std::path::Path;
use std::sync::OnceLock;

/// Filenames where ContextStream rules blocks live, in priority order.
/// Kept in sync with the editor surfaces in `mcp-server::setup::editors`,
/// but lives here so consumers (`mcp-tools`) can scan a project root
/// without taking a circular dep on `mcp-server`.
///
/// The list intentionally omits sub-directory rules paths
/// (`.cursor/rules/*.mdc`, `.github/copilot-instructions.md`, etc.) —
/// when the marker shows up in any one of these top-level files we
/// already know the binary's rules content was applied; finding it in
/// every editor's surface is unnecessary.
const KNOWN_RULES_FILENAMES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    ".rooignore",
    ".kilocoderules",
    ".aider.conf.yml",
    ".github/copilot-instructions.md",
    ".contextstream/rules.md",
    "contextstream.md",
];

/// Comment marker used inside the `<contextstream>` block of locally-
/// generated rules files to record the canonical teaching-bundle
/// fingerprint that wrote them. Reading this back lets the MCP binary
/// detect when bundled rule content has drifted from what the user has
/// installed without needing a `Cargo.toml` version bump.
pub const HASH_MARKER_PREFIX: &str = "<!-- contextstream-rules-hash: ";
const HASH_MARKER_SUFFIX: &str = " -->";

/// 64-bit FNV-1a hash, rendered as a 16-character lowercase hex string.
/// Deterministic across runs and Rust versions, no allocations beyond
/// the returned `String`. We use this instead of `DefaultHasher` (which
/// is randomly seeded per process) and instead of pulling in a crypto
/// dep — collision resistance is plenty for staleness detection and the
/// hash never leaves the user's machine in a security-sensitive context.
pub fn fnv1a_64_hex(data: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

/// Build the full marker line for embedding in a rules file. Includes a
/// trailing newline so callers can splice it directly after the
/// `<contextstream>` opening tag.
pub fn format_hash_marker(hash: &str) -> String {
    format!("{}{}{}\n", HASH_MARKER_PREFIX, hash, HASH_MARKER_SUFFIX)
}

/// Pull the embedded rules-content hash out of a file's text, if any.
/// Returns `None` when the marker is absent (older binary wrote the
/// file, or the file was hand-edited and the marker stripped).
pub fn extract_hash_marker(text: &str) -> Option<String> {
    let start = text.find(HASH_MARKER_PREFIX)?;
    let after_prefix = &text[start + HASH_MARKER_PREFIX.len()..];
    let end = after_prefix.find(HASH_MARKER_SUFFIX)?;
    let hash = after_prefix[..end].trim();
    if hash.is_empty() {
        return None;
    }
    Some(hash.to_string())
}

/// Strip a hash marker line from rules text (used when comparing the
/// pre-marker body of two files, or when re-writing a file with a fresh
/// marker). Removes the entire line including the trailing newline.
pub fn strip_hash_marker(text: &str) -> String {
    let Some(start) = text.find(HASH_MARKER_PREFIX) else {
        return text.to_string();
    };
    let after_prefix = &text[start + HASH_MARKER_PREFIX.len()..];
    let Some(rel_end) = after_prefix.find(HASH_MARKER_SUFFIX) else {
        return text.to_string();
    };
    let marker_end = start + HASH_MARKER_PREFIX.len() + rel_end + HASH_MARKER_SUFFIX.len();
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[marker_end..]
        .find('\n')
        .map_or(text.len(), |index| marker_end + index + 1);
    let prefix = &text[line_start..start];
    let suffix = &text[marker_end..line_end];

    // Most surfaces use a bare HTML-comment line. Aider's YAML pointer uses
    // `# <!-- ... -->`; remove that entire standalone comment line too.
    // Leaving the `# ` prefix behind would add another comment prefix on every
    // re-stamp. Preserve prefixes/tails when the marker is embedded in a line
    // containing user text.
    let marker_is_standalone = (prefix.chars().all(char::is_whitespace) || prefix.trim() == "#")
        && suffix.trim().is_empty();
    let (remove_start, remove_end) = if marker_is_standalone {
        (line_start, line_end)
    } else {
        let mut end = marker_end;
        if text.as_bytes().get(end) == Some(&b'\n') {
            end += 1;
        }
        (start, end)
    };

    let mut out = String::with_capacity(text.len() - (remove_end - remove_start));
    out.push_str(&text[..remove_start]);
    out.push_str(&text[remove_end..]);
    out
}

static CANONICAL_RULES_HASH: OnceLock<String> = OnceLock::new();

/// Record the canonical teaching-bundle fingerprint this binary would
/// produce. Idempotent — first call wins; subsequent calls are no-ops.
/// Producers should call this once at startup so every other crate (and
/// every later turn) sees the same fingerprint.
pub fn set_canonical_rules_hash(hash: impl Into<String>) {
    let _ = CANONICAL_RULES_HASH.set(hash.into());
}

/// Read back the canonical rules hash recorded by
/// [`set_canonical_rules_hash`], if any. Returns `None` before the
/// startup hook has run (e.g. in unit tests that exercise types
/// without bringing up the server).
pub fn canonical_rules_hash() -> Option<&'static str> {
    CANONICAL_RULES_HASH.get().map(String::as_str)
}

/// Scan `project_root` for the first known rules file that carries an
/// embedded `<!-- contextstream-rules-hash: ... -->` marker, and return
/// the hash. Returns `None` when no rules file is present, or when the
/// present file was written by an older binary that didn't embed a
/// marker (the staleness check then falls back to "unknown" — the
/// caller decides how to interpret).
///
/// Reads at most one file (the first hit). Errors on individual files
/// are swallowed so a single permission-denied path doesn't break the
/// scan; the next file in the list gets a chance.
pub fn read_local_rules_hash(project_root: &Path) -> Option<String> {
    for filename in KNOWN_RULES_FILENAMES {
        let path = project_root.join(filename);
        if !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(hash) = extract_hash_marker(&text) {
            return Some(hash);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_stable_and_distinct_across_inputs() {
        // Stability: same input → same hash on every call. Without this
        // guarantee the staleness check would false-fire on every restart.
        assert_eq!(fnv1a_64_hex(b"hello"), fnv1a_64_hex(b"hello"));
        // Distinctness: meaningfully different content yields different
        // hashes. (We're not asserting absence of every possible collision,
        // just that obvious changes register.)
        assert_ne!(fnv1a_64_hex(b"hello"), fnv1a_64_hex(b"hello!"));
        // Length: always 16 hex chars (64 bits / 4 bits per char).
        assert_eq!(fnv1a_64_hex(b"").len(), 16);
        assert_eq!(
            fnv1a_64_hex(b"a long input that exceeds short-buffer paths").len(),
            16
        );
    }

    #[test]
    fn extract_hash_marker_finds_embedded_hash() {
        let text = "<contextstream>\n<!-- contextstream-rules-hash: abc123 -->\n# Workspace\n";
        assert_eq!(extract_hash_marker(text), Some("abc123".to_string()));

        // Missing marker: returns None instead of erroring.
        assert!(extract_hash_marker("<contextstream>\nhello\n").is_none());

        // Marker with only whitespace inside: also None — we don't accept
        // empty hashes as a legitimate "this file is up to date" signal.
        assert!(extract_hash_marker("<!-- contextstream-rules-hash:    -->").is_none());
    }

    #[test]
    fn strip_hash_marker_removes_marker_line_cleanly() {
        // Round-trip safety: writing-then-stripping must yield the original
        // body, including no leftover blank lines. Rules files get rewritten
        // every `generate_rules()` call, so any drift would compound fast.
        let body = "<contextstream>\n# Workspace: Engineering\n</contextstream>\n";
        let with_marker = format!(
            "<contextstream>\n{}# Workspace: Engineering\n</contextstream>\n",
            format_hash_marker("deadbeefcafebabe")
        );
        assert_eq!(strip_hash_marker(&with_marker), body);

        // No marker present: pass-through unchanged.
        assert_eq!(strip_hash_marker(body), body);
    }

    #[test]
    fn strip_hash_marker_removes_standalone_yaml_comment_and_crlf_line() {
        let yaml = concat!(
            "# <contextstream>\r\n",
            "# <!-- contextstream-rules-hash: abc123 -->\r\n",
            "# ContextStream managed rules reference\r\n",
            "# </contextstream>\r\n",
        );
        assert_eq!(
            strip_hash_marker(yaml),
            concat!(
                "# <contextstream>\r\n",
                "# ContextStream managed rules reference\r\n",
                "# </contextstream>\r\n",
            )
        );
    }

    #[test]
    fn strip_hash_marker_preserves_user_text_around_inline_marker() {
        let inline = "prefix <!-- contextstream-rules-hash: abc123 --> suffix\nnext line\n";
        assert_eq!(strip_hash_marker(inline), "prefix  suffix\nnext line\n");
    }

    #[test]
    fn format_and_extract_round_trip() {
        // The format/extract pair has to agree exactly — drift here
        // breaks the staleness check silently. Pin the round-trip in a
        // test so format changes can't slip past.
        let h = "0123456789abcdef";
        let line = format_hash_marker(h);
        // Embed in a realistic surrounding block.
        let file = format!("<contextstream>\n{}# rest of the file\n", line);
        assert_eq!(extract_hash_marker(&file).as_deref(), Some(h));
    }

    #[test]
    fn canonical_rules_hash_is_set_once_and_readable() {
        // Test isolation note: OnceLock is process-global, so we use a
        // value distinct from anything a parallel test might set. If this
        // test runs before any production setter, it locks in the value;
        // if it runs after, the first-write-wins semantics mean our
        // assertion still passes because we're just checking *some*
        // value is present.
        let probe = "ffffffffffffffff_test";
        set_canonical_rules_hash(probe);
        let got = canonical_rules_hash().unwrap();
        assert!(!got.is_empty(), "canonical_rules_hash must be set");
        // Either we set it first (got == probe) or someone else did
        // (got is whatever they set). Both are acceptable here.
    }
}
