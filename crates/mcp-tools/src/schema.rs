//! JSON Schema generation for tool inputs.

use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Generate a JSON Schema for a type.
pub fn generate_schema<T: JsonSchema>() -> Value {
    let schema = schema_for!(T);
    serde_json::to_value(schema).unwrap_or_default()
}

// Common parameter types.

/// UUID parameter.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "UUID identifier")]
pub struct UuidParam(pub String);

/// Workspace ID parameter.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceIdParam {
    #[schemars(description = "Workspace ID (UUID). If omitted, uses the default workspace.")]
    pub workspace_id: Option<String>,
}

/// Project ID parameter.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectIdParam {
    #[schemars(description = "Project ID (UUID). If omitted, uses the default project.")]
    pub project_id: Option<String>,
}

/// Common workspace + project parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceProjectParams {
    #[schemars(description = "Workspace ID (UUID). If omitted, uses the default workspace.")]
    pub workspace_id: Option<String>,

    #[schemars(description = "Project ID (UUID). If omitted, uses the default project.")]
    pub project_id: Option<String>,
}

/// Pagination parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PaginationParams {
    #[schemars(description = "Page number (1-indexed)")]
    pub page: Option<i64>,

    #[schemars(description = "Results per page")]
    pub page_size: Option<i64>,
}

/// Query parameter for search operations.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueryParam {
    #[schemars(description = "Search query string")]
    pub query: String,
}

/// Limit parameter for limiting results.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LimitParam {
    #[schemars(description = "Maximum number of results to return")]
    pub limit: Option<i64>,
}

/// Schema builder for creating tool input schemas.
pub struct SchemaBuilder {
    properties: serde_json::Map<String, Value>,
    required: Vec<String>,
    description: Option<String>,
}

impl SchemaBuilder {
    pub fn new() -> Self {
        Self {
            properties: serde_json::Map::new(),
            required: Vec::new(),
            description: None,
        }
    }

    pub fn description(mut self, desc: &str) -> Self {
        self.description = Some(desc.to_string());
        self
    }

    pub fn property(mut self, name: &str, schema: Value, required: bool) -> Self {
        self.properties.insert(name.to_string(), schema);
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    pub fn string(self, name: &str, description: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "string",
            "description": description
        });
        self.property(name, schema, required)
    }

    pub fn uuid(self, name: &str, description: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "string",
            "format": "uuid",
            "description": description
        });
        self.property(name, schema, required)
    }

    pub fn integer(self, name: &str, description: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "integer",
            "description": description
        });
        self.property(name, schema, required)
    }

    pub fn number(self, name: &str, description: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "number",
            "description": description
        });
        self.property(name, schema, required)
    }

    pub fn boolean(self, name: &str, description: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "boolean",
            "description": description
        });
        self.property(name, schema, required)
    }

    pub fn string_enum(
        self,
        name: &str,
        description: &str,
        values: &[&str],
        required: bool,
    ) -> Self {
        let schema = serde_json::json!({
            "type": "string",
            "description": description,
            "enum": values
        });
        self.property(name, schema, required)
    }

    pub fn array(self, name: &str, description: &str, item_type: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "array",
            "description": description,
            "items": { "type": item_type }
        });
        self.property(name, schema, required)
    }

    pub fn object(self, name: &str, description: &str, required: bool) -> Self {
        let schema = serde_json::json!({
            "type": "object",
            "description": description,
            "additionalProperties": {}
        });
        self.property(name, schema, required)
    }

    pub fn build(self) -> Value {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": self.properties,
        });

        if !self.required.is_empty() {
            schema["required"] = serde_json::json!(self.required);
        }

        if let Some(desc) = self.description {
            schema["description"] = serde_json::json!(desc);
        }

        schema
    }
}

impl Default for SchemaBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Common schema patterns.
pub mod patterns {
    use super::SchemaBuilder;
    use serde_json::Value;

    /// Schema for workspace_id + project_id params.
    pub fn workspace_project_schema() -> Value {
        SchemaBuilder::new()
            .uuid(
                "workspace_id",
                "Workspace ID (UUID). If omitted, uses default.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID (UUID). If omitted, uses default.",
                false,
            )
            .build()
    }

    /// Schema for search operations.
    pub fn search_schema() -> Value {
        SchemaBuilder::new()
            .string("query", "Search query string", true)
            .uuid("workspace_id", "Workspace ID (UUID)", false)
            .uuid("project_id", "Project ID (UUID)", false)
            .integer("limit", "Maximum results to return", false)
            .build()
    }

    /// Schema for paginated list operations.
    pub fn paginated_list_schema() -> Value {
        SchemaBuilder::new()
            .uuid("workspace_id", "Workspace ID (UUID)", false)
            .integer("page", "Page number", false)
            .integer("page_size", "Results per page", false)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_builder() {
        let schema = SchemaBuilder::new()
            .description("Test schema")
            .string("name", "The name", true)
            .integer("count", "The count", false)
            .build();

        assert!(schema.get("properties").is_some());
        assert!(schema.get("required").is_some());
    }
}
