//! Turns server results into the text the model reads, with counts for the UI.
//!
//! Positions are shown one-based, paths relative to the working directory when shorter. Results are parsed leniently:
//! entries without a URI or range are skipped instead of failing the whole answer.

use std::collections::BTreeSet;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::Value;

use crate::operation::Operation;
use crate::uri::display_path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formatted {
    pub text: String,
    pub result_count: usize,
    pub file_count: usize,
}

const NO_DEFINITION: &str = "No definition found. The position may not be on a symbol, or the definition is in code the language server has not indexed.";
const NO_REFERENCES: &str = "No references found. The symbol may be unused, or the language server may not have finished indexing the workspace.";
const NO_HOVER: &str = "No hover information at this position. It may not be on a symbol, or the language server may not have finished indexing the file.";
const NO_DOCUMENT_SYMBOLS: &str = "No symbols found in this file. It may be empty, unsupported by the language server, or not indexed yet.";
const NO_WORKSPACE_SYMBOLS: &str = "No symbols found in the workspace. It may be empty, or the language server may not have finished indexing it.";
const NO_CALL_ITEM: &str = "No call hierarchy item at this position";
const NO_INCOMING: &str = "No incoming calls found (nothing calls this function)";
const NO_OUTGOING: &str = "No outgoing calls found (this function calls nothing)";

#[derive(Clone, Copy)]
struct Pos {
    line: u64,
    character: u64,
}

impl Pos {
    fn of(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        Some(Self {
            line: value.get("line")?.as_u64()?,
            character: value.get("character")?.as_u64()?,
        })
    }

    fn shown(self) -> String {
        format!("{}:{}", self.line + 1, self.character + 1)
    }
}

fn range_start(range: Option<&Value>) -> Option<Pos> {
    Pos::of(range?.get("start"))
}

/// A `Location` or `LocationLink` as `(uri, start)`.
fn location(value: &Value) -> Option<(String, Pos)> {
    if let Some(target) = value.get("targetUri").and_then(Value::as_str) {
        let range = value
            .get("targetSelectionRange")
            .or_else(|| value.get("targetRange"));
        return Some((target.to_string(), range_start(range)?));
    }
    let uri = value.get("uri")?.as_str()?;
    if uri.is_empty() {
        return None;
    }
    Some((uri.to_string(), range_start(value.get("range"))?))
}

/// A single result or an array, as a list.
fn as_list(result: &Value) -> Vec<&Value> {
    match result {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

fn symbol_kind(kind: Option<u64>) -> &'static str {
    match kind {
        Some(1) => "File",
        Some(2) => "Module",
        Some(3) => "Namespace",
        Some(4) => "Package",
        Some(5) => "Class",
        Some(6) => "Method",
        Some(7) => "Property",
        Some(8) => "Field",
        Some(9) => "Constructor",
        Some(10) => "Enum",
        Some(11) => "Interface",
        Some(12) => "Function",
        Some(13) => "Variable",
        Some(14) => "Constant",
        Some(15) => "String",
        Some(16) => "Number",
        Some(17) => "Boolean",
        Some(18) => "Array",
        Some(19) => "Object",
        Some(20) => "Key",
        Some(21) => "Null",
        Some(22) => "EnumMember",
        Some(23) => "Struct",
        Some(24) => "Event",
        Some(25) => "Operator",
        Some(26) => "TypeParameter",
        _ => "Unknown",
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Formats `result` of `operation` (for call operations: the result of the follow-up request).
pub fn format_result(operation: Operation, result: &Value, cwd: Option<&Path>) -> Formatted {
    match operation {
        Operation::GoToDefinition | Operation::GoToImplementation => definitions(result, cwd),
        Operation::FindReferences => references(result, cwd),
        Operation::Hover => hover(result),
        Operation::DocumentSymbol => document_symbols(result, cwd),
        Operation::WorkspaceSymbol => workspace_symbols(result, cwd),
        Operation::PrepareCallHierarchy => call_items(result, cwd),
        Operation::IncomingCalls => calls(
            result,
            cwd,
            "from",
            "calls at",
            NO_INCOMING,
            "incoming call",
        ),
        Operation::OutgoingCalls => calls(
            result,
            cwd,
            "to",
            "called from",
            NO_OUTGOING,
            "outgoing call",
        ),
    }
}

fn empty(text: &str) -> Formatted {
    Formatted {
        text: text.to_string(),
        result_count: 0,
        file_count: 0,
    }
}

fn definitions(result: &Value, cwd: Option<&Path>) -> Formatted {
    let locations: Vec<(String, Pos)> = as_list(result).into_iter().filter_map(location).collect();
    let files = locations
        .iter()
        .map(|(uri, _)| uri.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let shown = |(uri, pos): &(String, Pos)| format!("{}:{}", display_path(uri, cwd), pos.shown());
    let text = match locations.as_slice() {
        [] => return empty(NO_DEFINITION),
        [only] => format!("Defined in {}", shown(only)),
        many => {
            let lines: Vec<String> = many.iter().map(|l| format!("  {}", shown(l))).collect();
            format!("Found {} definitions:\n{}", many.len(), lines.join("\n"))
        }
    };
    Formatted {
        text,
        result_count: locations.len(),
        file_count: files,
    }
}

fn references(result: &Value, cwd: Option<&Path>) -> Formatted {
    let locations: Vec<(String, Pos)> = as_list(result).into_iter().filter_map(location).collect();
    if locations.is_empty() {
        return empty(NO_REFERENCES);
    }
    if let [(uri, pos)] = locations.as_slice() {
        return Formatted {
            text: format!(
                "Found 1 reference:\n  {}:{}",
                display_path(uri, cwd),
                pos.shown()
            ),
            result_count: 1,
            file_count: 1,
        };
    }
    let mut by_file: IndexMap<String, Vec<Pos>> = IndexMap::new();
    for (uri, pos) in &locations {
        by_file
            .entry(display_path(uri, cwd))
            .or_default()
            .push(*pos);
    }
    let mut lines = vec![format!(
        "Found {} references across {}:",
        locations.len(),
        plural(by_file.len(), "file")
    )];
    for (file, positions) in &by_file {
        lines.push(format!("\n{file}:"));
        lines.extend(
            positions
                .iter()
                .map(|pos| format!("  Line {}", pos.shown())),
        );
    }
    Formatted {
        text: lines.join("\n"),
        result_count: locations.len(),
        file_count: by_file.len(),
    }
}

fn markup_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(markup_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn hover(result: &Value) -> Formatted {
    let Some(contents) = result.get("contents") else {
        return empty(NO_HOVER);
    };
    let content = markup_text(contents);
    if content.trim().is_empty() {
        return empty(NO_HOVER);
    }
    let text = match range_start(result.get("range")) {
        Some(pos) => format!("Hover info at {}:\n\n{content}", pos.shown()),
        None => content,
    };
    Formatted {
        text,
        result_count: 1,
        file_count: 1,
    }
}

fn document_symbol_lines(symbol: &Value, depth: usize, lines: &mut Vec<String>) -> usize {
    let name = str_field(symbol, "name").unwrap_or("<unnamed>");
    let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64));
    let detail = str_field(symbol, "detail")
        .map(|d| format!(" {d}"))
        .unwrap_or_default();
    let line = range_start(symbol.get("range"))
        .map(|pos| format!(" - Line {}", pos.line + 1))
        .unwrap_or_default();
    lines.push(format!(
        "{}{name} ({kind}){detail}{line}",
        "  ".repeat(depth)
    ));
    let mut count = 1;
    if let Some(children) = symbol.get("children").and_then(Value::as_array) {
        for child in children {
            count += document_symbol_lines(child, depth + 1, lines);
        }
    }
    count
}

fn document_symbols(result: &Value, cwd: Option<&Path>) -> Formatted {
    let symbols = as_list(result);
    let Some(first) = symbols.first() else {
        return empty(NO_DOCUMENT_SYMBOLS);
    };
    // `SymbolInformation[]` (flat, with a location) is formatted like a workspace search.
    if first.get("location").is_some() {
        let mut formatted = workspace_symbols(result, cwd);
        formatted.file_count = usize::from(formatted.result_count > 0);
        return formatted;
    }
    let mut lines = vec!["Document symbols:".to_string()];
    let count: usize = symbols
        .iter()
        .map(|s| document_symbol_lines(s, 0, &mut lines))
        .sum();
    Formatted {
        text: lines.join("\n"),
        result_count: count,
        file_count: 1,
    }
}

fn workspace_symbols(result: &Value, cwd: Option<&Path>) -> Formatted {
    let symbols: Vec<(&Value, String, Pos)> = as_list(result)
        .into_iter()
        .filter_map(|symbol| {
            let (uri, pos) = location(symbol.get("location")?)?;
            Some((symbol, uri, pos))
        })
        .collect();
    if symbols.is_empty() {
        return empty(NO_WORKSPACE_SYMBOLS);
    }
    let mut by_file: IndexMap<String, Vec<String>> = IndexMap::new();
    for (symbol, uri, pos) in &symbols {
        let name = str_field(symbol, "name").unwrap_or("<unnamed>");
        let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64));
        let container = str_field(symbol, "containerName")
            .map(|c| format!(" in {c}"))
            .unwrap_or_default();
        by_file
            .entry(display_path(uri, cwd))
            .or_default()
            .push(format!(
                "  {name} ({kind}) - Line {}{container}",
                pos.line + 1
            ));
    }
    let mut lines = vec![format!(
        "Found {} in workspace:",
        plural(symbols.len(), "symbol")
    )];
    for (file, entries) in &by_file {
        lines.push(format!("\n{file}:"));
        lines.extend(entries.iter().cloned());
    }
    Formatted {
        text: lines.join("\n"),
        result_count: symbols.len(),
        file_count: by_file.len(),
    }
}

fn call_item_line(item: &Value, cwd: Option<&Path>) -> String {
    let name = str_field(item, "name").unwrap_or("<unnamed>");
    let kind = symbol_kind(item.get("kind").and_then(Value::as_u64));
    let place = match (str_field(item, "uri"), range_start(item.get("range"))) {
        (Some(uri), Some(pos)) => format!("{}:{}", display_path(uri, cwd), pos.line + 1),
        _ => crate::uri::UNKNOWN_LOCATION.to_string(),
    };
    let detail = str_field(item, "detail")
        .map(|d| format!(" [{d}]"))
        .unwrap_or_default();
    format!("{name} ({kind}) - {place}{detail}")
}

fn call_items(result: &Value, cwd: Option<&Path>) -> Formatted {
    let items = as_list(result);
    let files = items
        .iter()
        .filter_map(|item| str_field(item, "uri"))
        .collect::<BTreeSet<_>>()
        .len();
    let text = match items.as_slice() {
        [] => return empty(NO_CALL_ITEM),
        [only] => format!("Call hierarchy item: {}", call_item_line(only, cwd)),
        many => {
            let lines: Vec<String> = many
                .iter()
                .map(|i| format!("  {}", call_item_line(i, cwd)))
                .collect();
            format!(
                "Found {} call hierarchy items:\n{}",
                many.len(),
                lines.join("\n")
            )
        }
    };
    Formatted {
        text,
        result_count: items.len(),
        file_count: files,
    }
}

fn calls(
    result: &Value,
    cwd: Option<&Path>,
    side: &str,
    sites_label: &str,
    none: &str,
    noun: &str,
) -> Formatted {
    let entries = as_list(result);
    if entries.is_empty() {
        return empty(none);
    }
    let mut by_file: IndexMap<String, Vec<String>> = IndexMap::new();
    for entry in &entries {
        let Some(item) = entry.get(side) else {
            continue;
        };
        let file = str_field(item, "uri")
            .map(|uri| display_path(uri, cwd))
            .unwrap_or_else(|| crate::uri::UNKNOWN_LOCATION.to_string());
        let name = str_field(item, "name").unwrap_or("<unnamed>");
        let kind = symbol_kind(item.get("kind").and_then(Value::as_u64));
        let line = range_start(item.get("range"))
            .map(|pos| format!(" - Line {}", pos.line + 1))
            .unwrap_or_default();
        let sites: Vec<String> = entry
            .get("fromRanges")
            .and_then(Value::as_array)
            .map(|ranges| {
                ranges
                    .iter()
                    .filter_map(|r| range_start(Some(r)))
                    .map(Pos::shown)
                    .collect()
            })
            .unwrap_or_default();
        let sites = if sites.is_empty() {
            String::new()
        } else {
            format!(" [{sites_label}: {}]", sites.join(", "))
        };
        by_file
            .entry(file)
            .or_default()
            .push(format!("  {name} ({kind}){line}{sites}"));
    }
    let mut lines = vec![format!("Found {}:", plural(entries.len(), noun))];
    for (file, items) in &by_file {
        lines.push(format!("\n{file}:"));
        lines.extend(items.iter().cloned());
    }
    Formatted {
        text: lines.join("\n"),
        result_count: entries.len(),
        file_count: by_file.len(),
    }
}

/// URIs a location-style result points at (for the `.gitignore` filter).
pub fn result_uris(operation: Operation, result: &Value) -> Vec<String> {
    if !operation.filters_ignored_locations() {
        return Vec::new();
    }
    let mut uris: Vec<String> = as_list(result)
        .into_iter()
        .filter_map(|item| match operation {
            Operation::WorkspaceSymbol => location(item.get("location")?),
            _ => location(item),
        })
        .map(|(uri, _)| uri)
        .collect();
    uris.sort();
    uris.dedup();
    uris
}

/// `result` without the entries whose URI is in `ignored`.
pub fn without_uris(operation: Operation, result: Value, ignored: &BTreeSet<String>) -> Value {
    if ignored.is_empty() || !operation.filters_ignored_locations() {
        return result;
    }
    let keep = |item: &Value| {
        let found = match operation {
            Operation::WorkspaceSymbol => item.get("location").and_then(location),
            _ => location(item),
        };
        found.is_none_or(|(uri, _)| !ignored.contains(&uri))
    };
    match result {
        Value::Array(items) => Value::Array(items.into_iter().filter(|item| keep(item)).collect()),
        single if keep(&single) => single,
        _ => Value::Null,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const CWD: &str = "/w";

    fn loc(uri: &str, line: u64, character: u64) -> Value {
        json!({"uri": uri, "range": {"start": {"line": line, "character": character}, "end": {"line": line, "character": character}}})
    }

    fn run(op: Operation, result: Value) -> Formatted {
        format_result(op, &result, Some(Path::new(CWD)))
    }

    #[test]
    fn definitions_accept_locations_links_single_and_none() {
        assert_eq!(
            run(Operation::GoToDefinition, Value::Null).text,
            NO_DEFINITION
        );
        assert_eq!(
            run(Operation::GoToDefinition, loc("file:///w/src/a.rs", 9, 4)).text,
            "Defined in src/a.rs:10:5"
        );
        let link = json!({"targetUri": "file:///w/b.rs", "targetRange": {"start": {"line": 0, "character": 0}},
            "targetSelectionRange": {"start": {"line": 2, "character": 3}}});
        let both = run(
            Operation::GoToImplementation,
            json!([loc("file:///w/src/a.rs", 0, 0), link, {"range": {}}]),
        );
        assert_eq!(
            both.text,
            "Found 2 definitions:\n  src/a.rs:1:1\n  b.rs:3:4"
        );
        assert_eq!((both.result_count, both.file_count), (2, 2));
    }

    #[test]
    fn references_group_by_file() {
        let result = json!([
            loc("file:///w/a.rs", 0, 0),
            loc("file:///w/b.rs", 4, 1),
            loc("file:///w/a.rs", 7, 2)
        ]);
        let formatted = run(Operation::FindReferences, result);
        assert_eq!(
            formatted.text,
            "Found 3 references across 2 files:\n\na.rs:\n  Line 1:1\n  Line 8:3\n\nb.rs:\n  Line 5:2"
        );
        assert_eq!((formatted.result_count, formatted.file_count), (3, 2));
        assert_eq!(
            run(Operation::FindReferences, json!([])).text,
            NO_REFERENCES
        );
        assert_eq!(
            run(
                Operation::FindReferences,
                json!([loc("file:///w/a.rs", 1, 1)])
            )
            .text,
            "Found 1 reference:\n  a.rs:2:2"
        );
    }

    #[test]
    fn hover_handles_markup_strings_and_arrays() {
        let markup = json!({"contents": {"kind": "markdown", "value": "fn main()"},
            "range": {"start": {"line": 0, "character": 3}}});
        assert_eq!(
            run(Operation::Hover, markup).text,
            "Hover info at 1:4:\n\nfn main()"
        );
        let marked = json!({"contents": ["a", {"language": "rust", "value": "b"}]});
        assert_eq!(run(Operation::Hover, marked).text, "a\n\nb");
        assert_eq!(run(Operation::Hover, Value::Null).text, NO_HOVER);
    }

    #[test]
    fn document_symbols_nest_and_count_children() {
        let result = json!([{"name": "S", "kind": 23, "detail": "struct", "range": {"start": {"line": 0, "character": 0}},
            "children": [{"name": "f", "kind": 8, "range": {"start": {"line": 1, "character": 4}}}]}]);
        let formatted = run(Operation::DocumentSymbol, result);
        assert_eq!(
            formatted.text,
            "Document symbols:\nS (Struct) struct - Line 1\n  f (Field) - Line 2"
        );
        assert_eq!(formatted.result_count, 2);
        let flat = json!([{"name": "x", "kind": 13, "location": loc("file:///w/a.rs", 3, 0)}]);
        assert_eq!(
            run(Operation::DocumentSymbol, flat).text,
            "Found 1 symbol in workspace:\n\na.rs:\n  x (Variable) - Line 4"
        );
    }

    #[test]
    fn workspace_symbols_and_call_hierarchy() {
        let symbols = json!([
            {"name": "a", "kind": 12, "containerName": "m", "location": loc("file:///w/a.rs", 0, 0)},
            {"name": "b", "kind": 99, "location": loc("file:///w/a.rs", 5, 0)}
        ]);
        assert_eq!(
            run(Operation::WorkspaceSymbol, symbols).text,
            "Found 2 symbols in workspace:\n\na.rs:\n  a (Function) - Line 1 in m\n  b (Unknown) - Line 6"
        );
        let item = json!({"name": "main", "kind": 12, "uri": "file:///w/a.rs", "detail": "fn()",
            "range": {"start": {"line": 2, "character": 0}}});
        assert_eq!(
            run(Operation::PrepareCallHierarchy, json!([item.clone()])).text,
            "Call hierarchy item: main (Function) - a.rs:3 [fn()]"
        );
        let incoming =
            json!([{"from": item.clone(), "fromRanges": [{"start": {"line": 7, "character": 1}}]}]);
        assert_eq!(
            run(Operation::IncomingCalls, incoming).text,
            "Found 1 incoming call:\n\na.rs:\n  main (Function) - Line 3 [calls at: 8:2]"
        );
        let outgoing = json!([{"to": item, "fromRanges": []}]);
        assert_eq!(
            run(Operation::OutgoingCalls, outgoing).text,
            "Found 1 outgoing call:\n\na.rs:\n  main (Function) - Line 3"
        );
        assert_eq!(run(Operation::IncomingCalls, Value::Null).text, NO_INCOMING);
    }

    #[test]
    fn ignored_uris_are_removed_from_location_results() {
        let result = json!([
            loc("file:///w/a.rs", 0, 0),
            loc("file:///w/target/b.rs", 0, 0)
        ]);
        let uris = result_uris(Operation::FindReferences, &result);
        assert_eq!(uris, ["file:///w/a.rs", "file:///w/target/b.rs"]);
        let ignored = BTreeSet::from(["file:///w/target/b.rs".to_string()]);
        let kept = without_uris(Operation::FindReferences, result.clone(), &ignored);
        assert_eq!(kept, json!([loc("file:///w/a.rs", 0, 0)]));
        assert_eq!(
            without_uris(Operation::Hover, result.clone(), &ignored),
            result
        );
        assert!(result_uris(Operation::Hover, &result).is_empty());
    }
}
