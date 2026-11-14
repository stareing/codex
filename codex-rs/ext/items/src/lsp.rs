use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Display item for one call of the `lsp` code-navigation tool, owned by the LSP extension.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct LspItem {
    pub id: String,
    /// `goToDefinition`, `findReferences`, `hover`, ...
    pub operation: String,
    /// The file as the model named it.
    pub file_path: String,
    /// One-based line and character the model asked about.
    pub line: u32,
    pub character: u32,
    pub status: LspItemStatus,
    /// The language server that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub file_count: Option<u32>,
    /// The text the model received (or the error), shortened for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum LspItemStatus {
    InProgress,
    Completed,
    Failed,
}
