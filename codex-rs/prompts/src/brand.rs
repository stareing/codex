//! The agent's name in its instructions. Our releases ship this agent as Onigio's (`cli/src/brand.rs`), so the
//! model introduces itself as Onigio Agent rather than as the upstream CLI (user 11-07: it answered
//! "我是 Codex CLI"). Only the self-descriptions change; the rest of each template stays as upstream wrote it.

/// The name the model uses for itself.
pub const AGENT_NAME: &str = "Onigio Agent";

/// `(upstream text, ours)`, applied in order. The upstream claims "based on GPT-5" are dropped: the site serves
/// other models under these templates.
const REPLACEMENTS: &[(&str, &str)] = &[
    (
        "a coding agent running in the Codex CLI, a terminal-based coding assistant. Codex CLI is an open source project led by OpenAI.",
        "Onigio Agent, the coding agent of Onigio (onigio.com), running on the user's computer and used from the Onigio desktop app or website.",
    ),
    (
        "running in the Codex CLI, a terminal-based coding assistant. Codex CLI is an open source project led by OpenAI.",
        "running as Onigio Agent, the coding agent of Onigio (onigio.com), on the user's computer.",
    ),
    (
        "Within this context, Codex refers to the open-source agentic coding interface (not the old Codex language model built by OpenAI).",
        "Within this context, Onigio Agent refers to this agentic coding interface; when asked who you are, say you are Onigio Agent.",
    ),
    (
        "You are Codex, based on GPT-5. You are running as a coding agent in the Codex CLI on a user's computer.",
        "You are Onigio Agent, the coding agent of Onigio (onigio.com), running on the user's computer.",
    ),
    (
        "You are Codex, an agent based on GPT-5.",
        "You are Onigio Agent, the coding agent of Onigio (onigio.com).",
    ),
    (
        "You are Codex, an agent based on GPT-6.",
        "You are Onigio Agent, the coding agent of Onigio (onigio.com).",
    ),
    (
        "You are Codex, a coding agent based on GPT-5.",
        "You are Onigio Agent, the coding agent of Onigio (onigio.com).",
    ),
    ("As Codex, you are", "As Onigio Agent, you are"),
    ("inner life as Codex:", "inner life as Onigio Agent:"),
];

/// The instructions with the agent's self-descriptions renamed (unchanged when none appears).
pub fn brand_instructions(text: &str) -> String {
    REPLACEMENTS
        .iter()
        .fold(text.to_string(), |out, (from, to)| out.replace(from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What must not survive in any bundled template: the model calling itself Codex.
    const UPSTREAM_SELF: &[&str] = &[
        "You are Codex",
        "As Codex,",
        "as Codex:",
        "Codex CLI",
        "led by OpenAI",
    ];

    fn assert_branded(name: &str, text: &str) {
        let branded = brand_instructions(text);
        for upstream in UPSTREAM_SELF {
            assert!(
                !branded.contains(upstream),
                "{name}: still says {upstream:?}"
            );
        }
    }

    #[test]
    fn the_bundled_instructions_name_the_agent_onigio() {
        assert_branded(
            "models-manager/prompt.md",
            include_str!("../../models-manager/prompt.md"),
        );
        assert_branded(
            "protocol default.md",
            include_str!("../../protocol/src/prompts/base_instructions/default.md"),
        );
        // Every model's templates (JSON-escaped, the sentences are the same).
        assert_branded(
            "models-manager/models.json",
            include_str!("../../models-manager/models.json"),
        );
        let default = brand_instructions(include_str!(
            "../../protocol/src/prompts/base_instructions/default.md"
        ));
        assert!(
            default.starts_with("You are Onigio Agent, the coding agent of Onigio (onigio.com)"),
            "{}",
            &default[..120]
        );
    }

    #[test]
    fn other_text_is_left_alone() {
        let text = "Run `codex --help`; files live in ~/.codex. Codex Auto Review stays.";
        assert_eq!(brand_instructions(text), text);
        assert_eq!(
            brand_instructions(
                "You are GPT-5.2 running in the Codex CLI, a terminal-based coding assistant. Codex CLI is an open source project led by OpenAI. Be precise."
            ),
            "You are GPT-5.2 running as Onigio Agent, the coding agent of Onigio (onigio.com), on the user's computer. Be precise."
        );
    }
}
