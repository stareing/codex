//! `[lsp]` from the agent config → the client's runtime settings.

use std::time::Duration;

use codex_config::LspConfigToml;
use codex_lsp::LspLimits;
use codex_lsp::LspSettings;
use codex_lsp::ServerSpec;
use codex_lsp::config::DEFAULT_PRIORITY;

pub(crate) fn settings_from_config(config: &LspConfigToml) -> LspSettings {
    let defaults = LspLimits::default();
    let ms =
        |value: Option<u64>, default: Duration| value.map(Duration::from_millis).unwrap_or(default);
    let limits = LspLimits {
        request_timeout: ms(config.request_timeout_ms, defaults.request_timeout),
        startup_timeout: ms(config.startup_timeout_ms, defaults.startup_timeout),
        idle_shutdown: config
            .idle_shutdown_secs
            .map(Duration::from_secs)
            .unwrap_or(defaults.idle_shutdown),
        max_servers: config.max_servers.unwrap_or(defaults.max_servers).max(1),
        diagnostics_wait: ms(config.diagnostics_wait_ms, defaults.diagnostics_wait),
        diagnostics: codex_lsp::DiagnosticLimits {
            max_per_file: config
                .max_diagnostics_per_file
                .unwrap_or(defaults.diagnostics.max_per_file),
            max_total: config
                .max_diagnostics_total
                .unwrap_or(defaults.diagnostics.max_total),
            ..defaults.diagnostics.clone()
        },
        ..defaults
    };
    let servers = config
        .servers
        .iter()
        .map(|(name, server)| ServerSpec {
            name: name.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            extensions: server
                .extensions
                .iter()
                .map(|(ext, language)| (ServerSpec::normalized_extension(ext), language.clone()))
                .collect(),
            env: server.env.clone(),
            initialization_options: server.initialization_options.clone(),
            settings: server.settings.clone(),
            priority: server.priority.unwrap_or(DEFAULT_PRIORITY),
            optional: server.optional,
            source: server.source.clone(),
            startup_timeout: server.startup_timeout_ms.map(Duration::from_millis),
            workspace_folder: server
                .workspace_folder
                .as_ref()
                .map(std::path::PathBuf::from),
            max_restarts: server.max_restarts,
        })
        .collect();
    LspSettings {
        enabled: config.enabled.unwrap_or(false),
        servers,
        limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    #[test]
    fn off_by_default_and_limits_fall_back_to_defaults() {
        let settings = settings_from_config(&LspConfigToml::default());
        assert!(!settings.enabled);
        assert!(settings.servers.is_empty());
        assert_eq!(settings.limits, LspLimits::default());
    }

    #[test]
    fn parses_toml_servers_and_limits() {
        let toml = r#"
            enabled = true
            max_servers = 0
            request_timeout_ms = 1500
            max_diagnostics_per_file = 4
            [servers.rust]
            command = "rust-analyzer"
            extensions = { "RS" = "rust" }
            optional = true
            priority = 20
            initialization_options = { check = { command = "clippy" } }
        "#;
        let config: LspConfigToml = toml::from_str(toml).unwrap();
        let settings = settings_from_config(&config);
        assert!(settings.enabled);
        assert_eq!(settings.limits.max_servers, 1, "zero is raised to one");
        assert_eq!(settings.limits.request_timeout, Duration::from_millis(1500));
        assert_eq!(settings.limits.diagnostics.max_per_file, 4);
        let rust = &settings.servers[0];
        assert_eq!(
            rust.extensions,
            BTreeMap::from([(".rs".to_string(), "rust".to_string())])
        );
        assert_eq!((rust.priority, rust.optional), (20, true));
        assert_eq!(
            rust.initialization_options,
            Some(serde_json::json!({"check": {"command": "clippy"}}))
        );
    }
}
