pub const SUMMARIZATION_PROMPT: &str = include_str!("../templates/compact/prompt.md");
/// The prompt for a partial compaction ("summarize from here"): only the recent part is
/// summarized, the earlier part is kept verbatim.
pub const PARTIAL_SUMMARIZATION_PROMPT: &str =
    include_str!("../templates/compact/partial_prompt.md");
pub const SUMMARY_PREFIX: &str = include_str!("../templates/compact/summary_prefix.md");
