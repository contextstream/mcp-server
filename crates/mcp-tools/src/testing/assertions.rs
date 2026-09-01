//! Assertion helpers for testing tool results.

use mcp_types::tool::{ContentItem, ToolResult};
use serde_json::Value;

/// Extension trait for ToolResult assertions.
pub trait ToolResultAssert {
    /// Assert the result is not an error.
    fn assert_success(&self) -> &Self;

    /// Assert the result is an error.
    fn assert_error(&self) -> &Self;

    /// Assert the result contains text matching the pattern.
    fn assert_text_contains(&self, pattern: &str) -> &Self;

    /// Assert the result text equals the expected value.
    fn assert_text_equals(&self, expected: &str) -> &Self;

    /// Assert the result has structured content.
    fn assert_has_structured(&self) -> &Self;

    /// Assert structured content matches expected JSON.
    fn assert_structured_equals(&self, expected: &Value) -> &Self;

    /// Assert structured content contains a key.
    fn assert_structured_has_key(&self, key: &str) -> &Self;

    /// Assert structured content key equals value.
    fn assert_structured_key_equals(&self, key: &str, expected: &Value) -> &Self;

    /// Get the text content as a string.
    fn text(&self) -> String;

    /// Get the structured content if present.
    fn structured(&self) -> Option<&Value>;

    /// Parse text content as JSON.
    fn text_as_json(&self) -> Option<Value>;
}

impl ToolResultAssert for ToolResult {
    fn assert_success(&self) -> &Self {
        assert!(
            !self.is_error,
            "Expected success but got error: {}",
            self.text()
        );
        self
    }

    fn assert_error(&self) -> &Self {
        assert!(self.is_error, "Expected error but got success");
        self
    }

    fn assert_text_contains(&self, pattern: &str) -> &Self {
        let text = self.text();
        assert!(
            text.contains(pattern),
            "Expected text to contain '{}', but got: {}",
            pattern,
            text
        );
        self
    }

    fn assert_text_equals(&self, expected: &str) -> &Self {
        let text = self.text();
        assert_eq!(
            text, expected,
            "Expected text '{}', but got: {}",
            expected, text
        );
        self
    }

    fn assert_has_structured(&self) -> &Self {
        assert!(
            self.structured_content.is_some(),
            "Expected structured content but got none"
        );
        self
    }

    fn assert_structured_equals(&self, expected: &Value) -> &Self {
        let structured = self
            .structured_content
            .as_ref()
            .expect("Expected structured content");
        assert_eq!(structured, expected, "Structured content mismatch");
        self
    }

    fn assert_structured_has_key(&self, key: &str) -> &Self {
        let structured = self
            .structured_content
            .as_ref()
            .expect("Expected structured content");
        assert!(
            structured.get(key).is_some(),
            "Expected structured content to have key '{}', but keys are: {:?}",
            key,
            structured.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        self
    }

    fn assert_structured_key_equals(&self, key: &str, expected: &Value) -> &Self {
        let structured = self
            .structured_content
            .as_ref()
            .expect("Expected structured content");
        let value = structured
            .get(key)
            .unwrap_or_else(|| panic!("Expected structured content to have key '{}'", key));
        assert_eq!(
            value, expected,
            "Expected key '{}' to equal {:?}, but got {:?}",
            key, expected, value
        );
        self
    }

    fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|item| {
                if let ContentItem::Text { text } = item {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn structured(&self) -> Option<&Value> {
        self.structured_content.as_ref()
    }

    fn text_as_json(&self) -> Option<Value> {
        serde_json::from_str(&self.text()).ok()
    }
}

/// Assert that two JSON values are equal, ignoring key order.
pub fn assert_json_eq(actual: &Value, expected: &Value) {
    assert_eq!(
        actual,
        expected,
        "JSON mismatch:\nActual: {}\nExpected: {}",
        serde_json::to_string_pretty(actual).unwrap(),
        serde_json::to_string_pretty(expected).unwrap()
    );
}

/// Assert that a JSON value contains expected keys.
pub fn assert_json_has_keys(value: &Value, keys: &[&str]) {
    let obj = value.as_object().expect("Expected JSON object");
    for key in keys {
        assert!(
            obj.contains_key(*key),
            "Expected JSON to have key '{}', but keys are: {:?}",
            key,
            obj.keys().collect::<Vec<_>>()
        );
    }
}

/// Assert that a JSON array has expected length.
pub fn assert_json_array_len(value: &Value, expected_len: usize) {
    let arr = value.as_array().expect("Expected JSON array");
    assert_eq!(
        arr.len(),
        expected_len,
        "Expected array length {}, got {}",
        expected_len,
        arr.len()
    );
}

/// Assert that a JSON value matches a path and expected value.
///
/// Path uses dot notation, e.g., "user.profile.name".
pub fn assert_json_path(value: &Value, path: &str, expected: &Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;

    for part in &parts {
        current = current
            .get(*part)
            .unwrap_or_else(|| panic!("Path '{}' not found at '{}'", path, part));
    }

    assert_eq!(
        current, expected,
        "Expected path '{}' to equal {:?}, but got {:?}",
        path, expected, current
    );
}

/// Create a successful text result for comparison.
pub fn text_result(text: &str) -> ToolResult {
    ToolResult::text(text)
}

/// Create an error result for comparison.
pub fn error_result(message: &str) -> ToolResult {
    ToolResult::error(message)
}

/// Create a result with structured content.
pub fn structured_result(text: &str, structured: Value) -> ToolResult {
    ToolResult {
        content: vec![ContentItem::Text {
            text: text.to_string(),
        }],
        is_error: false,
        structured_content: Some(structured),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_assert_success() {
        let result = ToolResult::text("Hello");
        result.assert_success();
    }

    #[test]
    #[should_panic(expected = "Expected success but got error")]
    fn test_assert_success_fails_on_error() {
        let result = ToolResult::error("Something went wrong");
        result.assert_success();
    }

    #[test]
    fn test_assert_error() {
        let result = ToolResult::error("Error message");
        result.assert_error();
    }

    #[test]
    fn test_assert_text_contains() {
        let result = ToolResult::text("Hello, World!");
        result.assert_text_contains("World");
    }

    #[test]
    fn test_assert_structured() {
        let result = structured_result("Result", json!({"key": "value", "count": 42}));
        result
            .assert_success()
            .assert_has_structured()
            .assert_structured_has_key("key")
            .assert_structured_key_equals("count", &json!(42));
    }

    #[test]
    fn test_text_as_json() {
        let result = ToolResult::text(r#"{"status": "ok"}"#);
        let json = result.text_as_json().expect("Should parse as JSON");
        assert_eq!(json["status"], "ok");
    }

    #[test]
    fn test_assert_json_path() {
        let value = json!({
            "user": {
                "profile": {
                    "name": "Alice"
                }
            }
        });
        assert_json_path(&value, "user.profile.name", &json!("Alice"));
    }

    #[test]
    fn test_assert_json_has_keys() {
        let value = json!({"a": 1, "b": 2, "c": 3});
        assert_json_has_keys(&value, &["a", "b"]);
    }

    #[test]
    fn test_assert_json_array_len() {
        let value = json!([1, 2, 3, 4, 5]);
        assert_json_array_len(&value, 5);
    }
}
