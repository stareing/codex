//! The nine operations the model can ask for, and the LSP requests behind them.
//!
//! Positions arrive one-based (as editors show them) and are sent zero-based. `incomingCalls` / `outgoingCalls`
//! take two requests: `textDocument/prepareCallHierarchy`, then the calls of the first item it returns.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl Operation {
    pub const ALL: [Operation; 9] = [
        Self::GoToDefinition,
        Self::FindReferences,
        Self::Hover,
        Self::DocumentSymbol,
        Self::WorkspaceSymbol,
        Self::GoToImplementation,
        Self::PrepareCallHierarchy,
        Self::IncomingCalls,
        Self::OutgoingCalls,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GoToDefinition => "goToDefinition",
            Self::FindReferences => "findReferences",
            Self::Hover => "hover",
            Self::DocumentSymbol => "documentSymbol",
            Self::WorkspaceSymbol => "workspaceSymbol",
            Self::GoToImplementation => "goToImplementation",
            Self::PrepareCallHierarchy => "prepareCallHierarchy",
            Self::IncomingCalls => "incomingCalls",
            Self::OutgoingCalls => "outgoingCalls",
        }
    }

    /// The second request of a call-hierarchy operation.
    pub fn follow_up_method(self) -> Option<&'static str> {
        match self {
            Self::IncomingCalls => Some("callHierarchy/incomingCalls"),
            Self::OutgoingCalls => Some("callHierarchy/outgoingCalls"),
            _ => None,
        }
    }

    /// Results that point at files and are filtered against `.gitignore`.
    pub fn filters_ignored_locations(self) -> bool {
        matches!(
            self,
            Self::GoToDefinition
                | Self::FindReferences
                | Self::GoToImplementation
                | Self::WorkspaceSymbol
        )
    }
}

/// A one-based position from the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditorPosition {
    pub line: u32,
    pub character: u32,
}

impl EditorPosition {
    /// `None` when either number is zero (positions are one-based).
    pub fn new(line: u32, character: u32) -> Option<Self> {
        (line >= 1 && character >= 1).then_some(Self { line, character })
    }

    pub fn to_lsp(self) -> Value {
        json!({"line": self.line - 1, "character": self.character - 1})
    }
}

/// The first request: method and params.
pub fn request_for(
    operation: Operation,
    uri: &str,
    position: EditorPosition,
) -> (&'static str, Value) {
    let document = json!({"uri": uri});
    let at = |method: &'static str| {
        (
            method,
            json!({"textDocument": document, "position": position.to_lsp()}),
        )
    };
    match operation {
        Operation::GoToDefinition => at("textDocument/definition"),
        Operation::FindReferences => (
            "textDocument/references",
            json!({
                "textDocument": document,
                "position": position.to_lsp(),
                "context": {"includeDeclaration": true},
            }),
        ),
        Operation::Hover => at("textDocument/hover"),
        Operation::DocumentSymbol => (
            "textDocument/documentSymbol",
            json!({"textDocument": document}),
        ),
        // An empty query asks for every symbol, as the reference agent does.
        Operation::WorkspaceSymbol => ("workspace/symbol", json!({"query": ""})),
        Operation::GoToImplementation => at("textDocument/implementation"),
        Operation::PrepareCallHierarchy | Operation::IncomingCalls | Operation::OutgoingCalls => {
            at("textDocument/prepareCallHierarchy")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn positions_are_one_based_in_and_zero_based_out() {
        assert_eq!(EditorPosition::new(0, 1), None);
        assert_eq!(EditorPosition::new(1, 0), None);
        let position = EditorPosition::new(10, 5).unwrap();
        let (method, params) = request_for(Operation::GoToDefinition, "file:///a.rs", position);
        assert_eq!(method, "textDocument/definition");
        assert_eq!(params["position"], json!({"line": 9, "character": 4}));
        assert_eq!(params["textDocument"]["uri"], "file:///a.rs");
    }

    #[test]
    fn every_operation_maps_to_a_request_and_names_round_trip() {
        let position = EditorPosition::new(1, 1).unwrap();
        let methods: Vec<&str> = Operation::ALL
            .iter()
            .map(|op| request_for(*op, "file:///a.rs", position).0)
            .collect();
        assert_eq!(
            methods,
            [
                "textDocument/definition",
                "textDocument/references",
                "textDocument/hover",
                "textDocument/documentSymbol",
                "workspace/symbol",
                "textDocument/implementation",
                "textDocument/prepareCallHierarchy",
                "textDocument/prepareCallHierarchy",
                "textDocument/prepareCallHierarchy",
            ]
        );
        for op in Operation::ALL {
            let parsed: Operation = serde_json::from_value(json!(op.as_str())).unwrap();
            assert_eq!(parsed, op);
        }
        let (_, refs) = request_for(Operation::FindReferences, "file:///a.rs", position);
        assert_eq!(refs["context"]["includeDeclaration"], true);
        assert_eq!(
            Operation::IncomingCalls.follow_up_method(),
            Some("callHierarchy/incomingCalls")
        );
        assert_eq!(Operation::Hover.follow_up_method(), None);
    }
}
