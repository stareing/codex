use super::*;
use crate::common::Reasoning;
use crate::common::ResponsesApiTools;
use crate::requests::chat_tools::ChatToolKind;
use crate::requests::chat_tools::ChatToolTarget;
use codex_protocol::models::FunctionCallOutputPayload;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn request(input: Vec<ResponseItem>, tools: Option<Value>) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "meta/llama-3.3-70b-instruct".to_string(),
        instructions: "You are a coding agent.".to_string(),
        input,
        tools: tools.map(|tools| {
            let raw = serde_json::value::to_raw_value(&tools).expect("tools should serialize");
            ResponsesApiTools::from(Arc::from(raw))
        }),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: Some(Reasoning {
            effort: None,
            summary: None,
            context: None,
        }),
        store: false,
        stream: true,
        stream_options: None,
        include: vec!["reasoning.encrypted_content".to_string()],
        service_tier: Some("priority".to_string()),
        prompt_cache_key: Some("cache-key".to_string()),
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

fn message(role: &str, content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(
    call_id: &str,
    name: &str,
    namespace: Option<&str>,
    arguments: &str,
) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: namespace.map(str::to_string),
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_output(call_id: &str, output: FunctionCallOutputPayload) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn converts_conversation_with_tool_calls_and_images() {
    let input = vec![
        message(
            "developer",
            vec![ContentItem::InputText {
                text: "Sandbox: workspace-write".to_string(),
            }],
        ),
        message(
            "user",
            vec![
                ContentItem::InputText {
                    text: "What is in this screenshot?".to_string(),
                },
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,AAAA".to_string(),
                    },
                    detail: None,
                },
            ],
        ),
        ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("opaque".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        message(
            "assistant",
            vec![ContentItem::OutputText {
                text: "Let me look.".to_string(),
            }],
        ),
        function_call("call_1", "shell", None, r#"{"command":["ls"]}"#),
        function_call("call_2", "search", Some("mcp__docs__"), r#"{"q":"rust"}"#),
        function_output(
            "call_1",
            FunctionCallOutputPayload::from_text("a.txt".to_string()),
        ),
        function_output(
            "call_2",
            FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "found".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "https://example.com/a.png".to_string(),
                    },
                    detail: None,
                },
            ]),
        ),
        message(
            "assistant",
            vec![ContentItem::OutputText {
                text: "Done.".to_string(),
            }],
        ),
    ];
    let tools = json!([
        {
            "type": "function",
            "name": "shell",
            "description": "Run a command.",
            "strict": false,
            "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}
        },
        {"type": "web_search", "external_web_access": true},
        {
            "type": "namespace",
            "name": "mcp__docs__",
            "description": "Docs server.",
            "tools": [{
                "type": "function",
                "name": "search",
                "description": "Search docs.",
                "strict": false,
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
            }]
        }
    ]);

    let actual = build_chat_request(&request(input, Some(tools))).expect("request converts");

    let expected = ChatCompletionsRequest {
        body: json!({
            "model": "meta/llama-3.3-70b-instruct",
            "messages": [
                {"role": "system", "content": "You are a coding agent."},
                {"role": "system", "content": "Sandbox: workspace-write"},
                {"role": "user", "content": [
                    {"type": "text", "text": "What is in this screenshot?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]},
                {
                    "role": "assistant",
                    "content": "Let me look.",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "shell", "arguments": "{\"command\":[\"ls\"]}"}
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {"name": "mcp__docs__search", "arguments": "{\"q\":\"rust\"}"}
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "a.txt"},
                {"role": "tool", "tool_call_id": "call_2", "content": "found"},
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
                ]},
                {"role": "assistant", "content": "Done."}
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": [
                {"type": "function", "function": {
                    "name": "shell",
                    "description": "Run a command.",
                    "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}
                }},
                {"type": "function", "function": {
                    "name": "mcp__docs__search",
                    "description": "Search docs.",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
                }}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        }),
        tool_index: ChatToolIndex::from_targets([(
            "mcp__docs__search".to_string(),
            ChatToolTarget {
                namespace: Some("mcp__docs__".to_string()),
                name: "search".to_string(),
                kind: ChatToolKind::Function,
            },
        )]),
    };
    assert_eq!(actual, expected);
}

#[test]
fn converts_freeform_tools_and_custom_tool_history() {
    let input = vec![
        message(
            "user",
            vec![ContentItem::InputText {
                text: "Fix the typo.".to_string(),
            }],
        ),
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call_patch".to_string(),
            name: "apply_patch".to_string(),
            namespace: None,
            input: "*** Begin Patch\n*** End Patch".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "call_patch".to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_text("Success.".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let tools = json!([{
        "type": "custom",
        "name": "apply_patch",
        "description": "Edit files.",
        "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}
    }]);

    let actual = build_chat_request(&request(input, Some(tools))).expect("request converts");

    let expected = ChatCompletionsRequest {
        body: json!({
            "model": "meta/llama-3.3-70b-instruct",
            "messages": [
                {"role": "system", "content": "You are a coding agent."},
                {"role": "user", "content": "Fix the typo."},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_patch",
                        "type": "function",
                        "function": {
                            "name": "apply_patch",
                            "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": "call_patch", "content": "Success."}
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": [{"type": "function", "function": {
                "name": "apply_patch",
                "description": "Edit files.\n\nPass the complete raw tool input as the `input` string argument. The input must follow this lark grammar:\nstart: patch",
                "parameters": {
                    "type": "object",
                    "properties": {"input": {"type": "string", "description": "Raw tool input."}},
                    "required": ["input"],
                    "additionalProperties": false
                }
            }}],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        }),
        tool_index: ChatToolIndex::from_targets([(
            "apply_patch".to_string(),
            ChatToolTarget {
                namespace: None,
                name: "apply_patch".to_string(),
                kind: ChatToolKind::Freeform,
            },
        )]),
    };
    assert_eq!(actual, expected);
}

#[test]
fn omits_tool_fields_without_tools() {
    let input = vec![message(
        "user",
        vec![ContentItem::InputText {
            text: "hi".to_string(),
        }],
    )];

    let actual = build_chat_request(&request(input, /*tools*/ None)).expect("request converts");

    let expected = ChatCompletionsRequest {
        body: json!({
            "model": "meta/llama-3.3-70b-instruct",
            "messages": [
                {"role": "system", "content": "You are a coding agent."},
                {"role": "user", "content": "hi"}
            ],
            "stream": true,
            "stream_options": {"include_usage": true}
        }),
        tool_index: ChatToolIndex::default(),
    };
    assert_eq!(actual, expected);
}
