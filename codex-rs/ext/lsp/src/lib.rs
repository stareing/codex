//! Language servers for the agent: the `lsp` code-navigation tool and diagnostics after edits.
//!
//! Off unless `[lsp] enabled = true` and at least one server is configured (and, for `optional` servers, installed).
//! Servers come from `[lsp.servers]` and from installed plugins that declare `lspServers` ([`plugins`]).
//! Servers are shared by every conversation in the process (`codex_lsp::LspManager::global`). After the agent writes
//! a file (an applied patch), the file is synced to its server; the next model step waits briefly for the server's
//! diagnostics and adds new ones to the model context as a `<new-diagnostics>` section.

pub mod plugins;
mod settings;
mod sync;
mod tool;
mod world_state;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolContributor;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_lsp::DiagnosticSink;
use codex_lsp::LspContext;
use codex_lsp::LspManager;
use codex_lsp::ServerSpec;
use codex_lsp::ServerTable;
use codex_protocol::items::TurnItem;

use crate::settings::settings_from_config;

/// One conversation's view of the language servers.
pub(crate) struct LspThread {
    pub(crate) enabled: bool,
    pub(crate) ctx: LspContext,
    pub(crate) manager: Arc<LspManager>,
    /// Servers read from plugins when the conversation started (kept across configuration changes).
    pub(crate) plugin_servers: Arc<Vec<ServerSpec>>,
}

impl LspThread {
    fn from_config(
        config: &Config,
        sink: Option<Arc<DiagnosticSink>>,
        manager: Arc<LspManager>,
        plugin_servers: Arc<Vec<ServerSpec>>,
    ) -> Self {
        let settings = settings_from_config(&config.lsp);
        let cwd: PathBuf = config.cwd.to_path_buf();
        let table = if settings.enabled {
            let servers: Vec<ServerSpec> = settings
                .servers
                .iter()
                .chain(plugin_servers.iter())
                .cloned()
                .collect();
            ServerTable::new(&servers, std::env::var_os("PATH"), &cwd)
        } else {
            ServerTable::default()
        };
        let sink = sink
            .unwrap_or_else(|| Arc::new(DiagnosticSink::new(settings.limits.diagnostics.clone())));
        Self {
            enabled: settings.enabled && !table.is_empty(),
            ctx: LspContext {
                table: Arc::new(table),
                limits: settings.limits,
                cwd,
                sink,
            },
            manager,
            plugin_servers,
        }
    }

    /// The thread's state when language servers are on.
    pub(crate) fn active(thread_store: &ExtensionData) -> Option<Arc<Self>> {
        thread_store.get::<Self>().filter(|thread| thread.enabled)
    }
}

/// Where plugin folders come from (the agent's plugin manager in production).
pub trait PluginSource: Send + Sync {
    /// The installed, enabled plugins for this configuration.
    fn plugin_dirs<'a>(
        &'a self,
        config: &'a Config,
    ) -> ExtensionFuture<'a, Vec<plugins::PluginDir>>;
}

/// [`PluginSource`] backed by the thread manager's plugin manager.
pub struct ThreadManagerPlugins(pub Weak<ThreadManager>);

impl PluginSource for ThreadManagerPlugins {
    fn plugin_dirs<'a>(
        &'a self,
        config: &'a Config,
    ) -> ExtensionFuture<'a, Vec<plugins::PluginDir>> {
        Box::pin(async move {
            let Some(thread_manager) = self.0.upgrade() else {
                return Vec::new();
            };
            let outcome = thread_manager
                .plugins_manager()
                .plugins_for_config(&config.plugins_config_input())
                .await;
            let store = match codex_core_plugins::store::PluginStore::try_new(
                config.codex_home.to_path_buf(),
            ) {
                Ok(store) => store,
                Err(err) => {
                    tracing::warn!("lsp: plugin folders unavailable: {err}");
                    return Vec::new();
                }
            };
            outcome
                .plugins()
                .iter()
                .filter(|plugin| plugin.is_active())
                .filter_map(|plugin| {
                    let id =
                        codex_core_plugin_common::plugin_id::PluginId::parse(&plugin.config_name)
                            .ok()?;
                    Some(plugins::PluginDir {
                        name: plugin.display_name().to_string(),
                        root: plugin.root.to_path_buf(),
                        data_root: store.plugin_data_root(&id).to_path_buf(),
                    })
                })
                .collect()
        })
    }
}

struct LspExtension {
    manager: Arc<LspManager>,
    plugins: Option<Arc<dyn PluginSource>>,
}

impl LspExtension {
    /// Plugin servers, read only when the feature is on.
    async fn plugin_servers(&self, config: &Config) -> Arc<Vec<ServerSpec>> {
        let (Some(source), Some(true)) = (self.plugins.as_ref(), config.lsp.enabled) else {
            return Arc::default();
        };
        let dirs = source.plugin_dirs(config).await;
        let loaded = plugins::load(&dirs, &|name| std::env::var(name).ok());
        for error in &loaded.errors {
            tracing::warn!("lsp: {error}");
        }
        Arc::new(loaded.specs)
    }
}

impl ThreadLifecycleContributor<Config> for LspExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let plugin_servers = self.plugin_servers(input.config).await;
            input.thread_store.insert(LspThread::from_config(
                input.config,
                None,
                self.manager.clone(),
                plugin_servers,
            ));
        })
    }
}

impl ConfigContributor<Config> for LspExtension {
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        _previous_config: &Config,
        new_config: &Config,
    ) {
        // Keep the conversation's diagnostics (what was already delivered) across configuration changes.
        let previous = thread_store.get::<LspThread>();
        let sink = previous.as_ref().map(|thread| thread.ctx.sink.clone());
        let plugin_servers = previous
            .map(|thread| thread.plugin_servers.clone())
            .unwrap_or_default();
        thread_store.insert(LspThread::from_config(
            new_config,
            sink,
            self.manager.clone(),
            plugin_servers,
        ));
    }
}

impl ToolContributor for LspExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<
        Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>,
    > {
        match LspThread::active(thread_store) {
            Some(thread) => vec![Arc::new(tool::LspTool { thread })],
            None => Vec::new(),
        }
    }
}

impl TurnLifecycleContributor for LspExtension {
    fn on_item_completed<'a>(
        &'a self,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a TurnItem,
    ) -> ExtensionFuture<'a, ()> {
        if let (TurnItem::FileChange(change), Some(thread)) =
            (item, LspThread::active(thread_store))
        {
            sync::sync_file_change(&thread, change);
        }
        Box::pin(std::future::ready(()))
    }
}

impl ContextContributor for LspExtension {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let Some(thread) = LspThread::active(input.thread_store) else {
                return Vec::new();
            };
            vec![world_state::diagnostics_section(&thread).await]
        })
    }
}

/// Installs the LSP tool, edit sync and diagnostics into the registry; `plugins` supplies plugin folders.
pub fn install(
    registry: &mut ExtensionRegistryBuilder<Config>,
    plugins: Option<Arc<dyn PluginSource>>,
) {
    install_with_manager(registry, LspManager::global(), plugins);
}

/// As [`install`], with a manager of the caller's (tests).
pub fn install_with_manager(
    registry: &mut ExtensionRegistryBuilder<Config>,
    manager: Arc<LspManager>,
    plugins: Option<Arc<dyn PluginSource>>,
) {
    let extension = Arc::new(LspExtension { manager, plugins });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.tool_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.prompt_contributor(extension);
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
