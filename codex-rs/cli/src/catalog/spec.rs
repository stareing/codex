//! What the site's catalog says about an item: the reference a user types and the install
//! description (`spec`) the site's `agent_catalog/install_spec.py` (store) and `tools_spec.py`
//! (the open-source tool directory) produce (docs/agent-codex/PLUGINS.md §9 and §10 in the site
//! repo). Everything the server sends is checked here
//! before it reaches the config file or the disk.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use url::Url;

/// Upper bound on a skill bundle, compressed or not (PLUGINS.md §9 "CLI 写入").
pub(crate) const MAX_BUNDLE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_SLUG_CHARS: usize = 128;
const MAX_ENV_NAME_CHARS: usize = 128;
const MAX_ENV_ENTRIES: usize = 64;
const MAX_ARGS: usize = 64;
const MAX_TEXT_CHARS: usize = 4096;
pub(crate) const MAX_ALIAS_CHARS: usize = 64;
/// The site's limits on a tool directory key: two parts of at most 150 characters, 200 in all
/// (`agent_catalog/refs.py`).
const MAX_TOOL_KEY_PART_CHARS: usize = 150;
const MAX_TOOL_KEY_CHARS: usize = 200;
/// The value `trust` has for an entry the site reviewed; anything else is unreviewed.
pub(crate) const TRUST_REVIEWED: &str = "reviewed";
/// Upper bound on a skill sent as text (`skill.skill_md`).
pub(crate) const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Mcp,
    Skill,
    /// A command-line tool from the tool directory: never installed, only pointed to.
    Cli,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Mcp => "mcp",
            Kind::Skill => "skill",
            Kind::Cli => "cli",
        }
    }

    /// The kinds a `store:` reference names.
    fn parse_store(raw: &str) -> Option<Self> {
        match raw {
            "mcp" => Some(Kind::Mcp),
            "skill" => Some(Kind::Skill),
            _ => None,
        }
    }

    /// The kinds a `tools:` reference names.
    fn parse_tools(raw: &str) -> Option<Self> {
        match raw {
            "cli" => Some(Kind::Cli),
            other => Self::parse_store(other),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a reference points: the site's reviewed store, or the open-source tool directory
/// (unreviewed entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Store,
    Tools,
}

/// `store:mcp/<slug>`, `store:skill/<slug>`, a bare `<slug>` (the site picks the kind, or
/// answers 409 `ambiguous_ref`), or `tools:<mcp|skill|cli>/<key>` (`key` is the directory's
/// source key, e.g. `io.github.acme/weather`; it is sent to the site exactly as typed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogRef {
    pub(crate) source: Source,
    pub(crate) kind: Option<Kind>,
    /// The store slug, or the tool directory key.
    pub(crate) slug: String,
}

impl CatalogRef {
    pub(crate) fn store(kind: Kind, slug: &str) -> Self {
        Self {
            source: Source::Store,
            kind: Some(kind),
            slug: slug.to_string(),
        }
    }

    pub(crate) fn tools(kind: Kind, key: &str) -> Self {
        Self {
            source: Source::Tools,
            kind: Some(kind),
            slug: key.to_string(),
        }
    }
}

impl fmt::Display for CatalogRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.source, self.kind) {
            (Source::Tools, Some(kind)) => write!(f, "tools:{kind}/{}", self.slug),
            (Source::Store, Some(kind)) => write!(f, "store:{kind}/{}", self.slug),
            (_, None) => f.write_str(&self.slug),
        }
    }
}

/// Letters, digits, `.`, `_`, `-`, starting with a letter or digit: safe as a directory name and a
/// URL segment.
pub(crate) fn valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    slug.len() <= MAX_SLUG_CHARS
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A local MCP server name (`[mcp_servers.<alias>]`).
pub(crate) fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= MAX_ALIAS_CHARS
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// One part of a tool directory key: letters, digits, `.`, `_`, `-` (the registry's name rule;
/// `..` is allowed there, and the key only ever travels as a query value or JSON string).
fn valid_tool_key_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= MAX_TOOL_KEY_PART_CHARS
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `<namespace>/<name>`: exactly one `/` between two valid parts.
pub(crate) fn valid_tool_key(key: &str) -> bool {
    key.len() <= MAX_TOOL_KEY_CHARS
        && key
            .split_once('/')
            .is_some_and(|(left, right)| valid_tool_key_part(left) && valid_tool_key_part(right))
}

pub(crate) fn parse_ref(raw: &str) -> Result<CatalogRef, String> {
    let raw = raw.trim();
    let bad = || {
        format!(
            "`{raw}` is not a catalog reference; use store:mcp/<slug>, store:skill/<slug>, \
             tools:<mcp|skill|cli>/<key> or a slug"
        )
    };
    match raw.split_once(':') {
        None if valid_slug(raw) => Ok(CatalogRef {
            source: Source::Store,
            kind: None,
            slug: raw.to_string(),
        }),
        Some(("store", rest)) => {
            let (kind, slug) = rest.split_once('/').ok_or_else(bad)?;
            let kind = Kind::parse_store(kind).ok_or_else(bad)?;
            if !valid_slug(slug) {
                return Err(bad());
            }
            Ok(CatalogRef::store(kind, slug))
        }
        Some(("tools", rest)) => {
            let (kind, key) = rest.split_once('/').ok_or_else(bad)?;
            let kind = Kind::parse_tools(kind).ok_or_else(bad)?;
            if !valid_tool_key(key) {
                return Err(bad());
            }
            Ok(CatalogRef::tools(kind, key))
        }
        _ => Err(bad()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Transport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct McpSpec {
    pub(crate) transport: Transport,
    #[serde(default)]
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    /// Environment variables whose values the user supplies on this computer.
    #[serde(default)]
    pub(crate) secret_env: Vec<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    /// Literal HTTP headers (a remote server); absent in older answers.
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    /// HTTP header name → name of the environment variable holding its value.
    #[serde(default)]
    pub(crate) secret_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct SkillSpec {
    pub(crate) name: String,
    /// The whole SKILL.md, for skills the store keeps as text only (then there is no bundle).
    #[serde(default)]
    pub(crate) skill_md: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct BundleSpec {
    pub(crate) sha256: String,
    pub(crate) size: u64,
    /// One-time download link; the contract puts it on the `install/` answer, this is accepted too.
    #[serde(default)]
    pub(crate) download_url: Option<String>,
}

/// Where to read how to install something this CLI does not install (a command-line tool).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct InstallInstructions {
    #[serde(default)]
    pub(crate) url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct InstallSpec {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) kind: Kind,
    pub(crate) slug: String,
    pub(crate) name: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) trust: String,
    #[serde(default)]
    pub(crate) source_url: Option<String>,
    #[serde(default)]
    pub(crate) mcp: Option<McpSpec>,
    #[serde(default)]
    pub(crate) skill: Option<SkillSpec>,
    #[serde(default)]
    pub(crate) bundle: Option<BundleSpec>,
    /// Set for command-line tools only; `null` (or absent, in older answers) otherwise.
    #[serde(default)]
    pub(crate) install_instructions: Option<InstallInstructions>,
}

pub(crate) fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= MAX_ENV_NAME_CHARS
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An HTTP header name (RFC 9110 token).
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ENV_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
}

fn plain_text(text: &str) -> bool {
    text.len() <= MAX_TEXT_CHARS && !text.chars().any(char::is_control)
}

/// HTTPS, or HTTP to this computer only.
pub(crate) fn acceptable_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    url.scheme() == "https" || (url.scheme() == "http" && loopback)
}

pub(crate) fn valid_sha256(sha: &str) -> bool {
    sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// A web page to show the user: http(s), one line.
fn showable_url(raw: &str) -> bool {
    plain_text(raw) && Url::parse(raw).is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
}

impl InstallSpec {
    pub(crate) fn reviewed(&self) -> bool {
        self.trust == TRUST_REVIEWED
    }

    /// The reference as the site names it (what the ledger and the site's other endpoints use).
    /// `None` when the site sent something that is not a reference of this spec's kind.
    pub(crate) fn catalog_ref(&self) -> Option<CatalogRef> {
        let parsed = parse_ref(&self.reference).ok()?;
        match parsed.source {
            Source::Store if parsed == CatalogRef::store(self.kind, &self.slug) => Some(parsed),
            Source::Tools if parsed.kind == Some(self.kind) => Some(parsed),
            _ => None,
        }
    }

    /// Whether this spec describes what `requested` asked for. A store reference is matched by
    /// kind and slug as before; a tool directory reference must name the same kind and key,
    /// ignoring ASCII case (GitHub keys are case-insensitive and the site sends them
    /// lower-cased).
    pub(crate) fn answers(&self, requested: &CatalogRef) -> bool {
        let Some(own) = self.catalog_ref() else {
            return false;
        };
        match requested.source {
            Source::Store => {
                own.source == Source::Store
                    && requested.kind.is_none_or(|kind| kind == self.kind)
                    && requested.slug == self.slug
            }
            Source::Tools => {
                own.source == Source::Tools
                    && requested.kind == own.kind
                    && requested.slug.eq_ignore_ascii_case(&own.slug)
            }
        }
    }

    /// Refuses anything this CLI would not write or run as described.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let Some(own) = self.catalog_ref() else {
            return Err(format!(
                "the site sent reference {:?} for a {} named {:?}",
                self.reference, self.kind, self.slug
            ));
        };
        // A tool directory slug is the server's default local name (`[a-z0-9_-]`).
        let usable_slug = match own.source {
            Source::Store => valid_slug(&self.slug),
            Source::Tools => valid_alias(&self.slug),
        };
        if !usable_slug {
            return Err(format!("the site sent an unusable slug {:?}", self.slug));
        }
        for (label, text) in [("name", &self.name), ("version", &self.version)] {
            if text.trim().is_empty() || !plain_text(text) {
                return Err(format!("the site sent an unusable {label}"));
            }
        }
        if let Some(source) = &self.source_url
            && !showable_url(source)
        {
            return Err("the site sent an unusable source address".to_string());
        }
        match (self.kind, &self.mcp, &self.skill) {
            (Kind::Mcp, Some(mcp), None) => validate_mcp(mcp),
            (Kind::Skill, None, Some(skill)) => self.validate_skill(skill),
            (Kind::Cli, None, None) => self.validate_instructions(),
            _ => Err(format!(
                "the site's description of this {} does not match its kind",
                self.kind
            )),
        }
    }

    fn validate_instructions(&self) -> Result<(), String> {
        let Some(instructions) = &self.install_instructions else {
            return Err("the site sent a command-line tool without instructions".to_string());
        };
        match &instructions.url {
            Some(url) if !showable_url(url) => {
                Err("the site sent an unusable instructions address".to_string())
            }
            _ => Ok(()),
        }
    }

    fn validate_skill(&self, skill: &SkillSpec) -> Result<(), String> {
        if skill.name.trim().is_empty() || !plain_text(&skill.name) {
            return Err("the site sent an unusable skill name".to_string());
        }
        let bundle = match (&self.bundle, &skill.skill_md) {
            (Some(bundle), None) => bundle,
            (None, Some(text)) if !text.trim().is_empty() => {
                return if text.len() as u64 <= MAX_SKILL_MD_BYTES {
                    Ok(())
                } else {
                    Err("the skill's SKILL.md is too large".to_string())
                };
            }
            _ => {
                return Err(
                    "the site sent a skill with neither a bundle nor a SKILL.md (or both)"
                        .to_string(),
                );
            }
        };
        if !valid_sha256(&bundle.sha256) {
            return Err("the site sent a bundle without a valid sha256".to_string());
        }
        if bundle.size == 0 || bundle.size > MAX_BUNDLE_BYTES {
            return Err(format!(
                "the skill bundle is {} bytes; at most {MAX_BUNDLE_BYTES} are accepted",
                bundle.size
            ));
        }
        Ok(())
    }
}

fn validate_mcp(mcp: &McpSpec) -> Result<(), String> {
    match (mcp.transport, &mcp.command, &mcp.url) {
        (Transport::Stdio, Some(command), None) if !command.trim().is_empty() => {
            if !plain_text(command) || mcp.args.len() > MAX_ARGS {
                return Err("the site sent an unusable command".to_string());
            }
            if mcp.args.iter().any(|arg| !plain_text(arg)) {
                return Err("the site sent an unusable command argument".to_string());
            }
            if !mcp.secret_headers.is_empty() || !mcp.headers.is_empty() {
                return Err("a local (stdio) server cannot take HTTP headers".to_string());
            }
        }
        (Transport::Http, None, Some(url)) => {
            if !acceptable_url(url) {
                return Err(format!("the server address {url:?} is not HTTPS"));
            }
            if !mcp.env.is_empty() {
                return Err("a remote (http) server cannot take environment variables".to_string());
            }
        }
        _ => {
            return Err(
                "the site's server description has neither a command nor an address".to_string(),
            );
        }
    }
    if mcp.env.len() + mcp.secret_env.len() > MAX_ENV_ENTRIES
        || mcp.secret_headers.len() + mcp.headers.len() > MAX_ENV_ENTRIES
    {
        return Err("the site sent too many settings".to_string());
    }
    for (name, value) in &mcp.env {
        if !valid_env_name(name) || !plain_text(value) {
            return Err(format!("the site sent an unusable variable {name:?}"));
        }
    }
    if let Some(name) = mcp.secret_env.iter().find(|name| !valid_env_name(name)) {
        return Err(format!("the site sent an unusable secret name {name:?}"));
    }
    let mut texts: Vec<&String> = mcp.args.iter().collect();
    texts.extend(mcp.url.iter());
    for text in texts {
        if let Some(name) = placeholders(text)
            .into_iter()
            .find(|name| !mcp.secret_env.iter().any(|secret| secret == name))
        {
            return Err(format!(
                "the site's server description uses ${{{name}}} without asking for it"
            ));
        }
    }
    for (header, variable) in &mcp.secret_headers {
        if !valid_header_name(header) || !valid_env_name(variable) {
            return Err(format!("the site sent an unusable header {header:?}"));
        }
    }
    for (header, value) in &mcp.headers {
        if !valid_header_name(header)
            || !plain_text(value)
            || mcp.secret_headers.contains_key(header)
        {
            return Err(format!("the site sent an unusable header {header:?}"));
        }
    }
    Ok(())
}

/// The `KEY`s of the `${KEY}` placeholders in `text`, in order.
pub(crate) fn placeholders(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            break;
        };
        let name = &after[..end];
        if valid_env_name(name) {
            names.push(name.to_string());
            rest = &after[end + 1..];
        } else {
            rest = after;
        }
    }
    names
}

/// `text` with each `${KEY}` that `values` has replaced by its value (single pass: a value that
/// itself contains `${…}` is not expanded again).
pub(crate) fn substitute(text: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after
            .find('}')
            .and_then(|end| values.get(&after[..end]).map(|value| (end, value)))
        {
            Some((end, value)) => {
                out.push_str(value);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
