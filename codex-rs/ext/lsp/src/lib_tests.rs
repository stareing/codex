use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// Plugin folders from a list, counting lookups.
struct FakePlugins {
    dirs: Vec<plugins::PluginDir>,
    calls: AtomicUsize,
}

impl PluginSource for FakePlugins {
    fn plugin_dirs<'a>(
        &'a self,
        _config: &'a Config,
    ) -> ExtensionFuture<'a, Vec<plugins::PluginDir>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(self.dirs.clone()))
    }
}

async fn config(home: &std::path::Path, enabled: bool) -> Config {
    Config::load_default_with_cli_overrides_for_codex_home(
        home.to_path_buf(),
        vec![("lsp.enabled".to_string(), toml::Value::Boolean(enabled))],
    )
    .await
    .expect("config")
}

/// A plugin whose server is this test program (an absolute path that exists, so it counts as installed).
fn plugin_with_server(dir: &std::path::Path) -> plugins::PluginDir {
    let root = dir.join("p");
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    let program = std::env::current_exe().unwrap();
    let manifest = serde_json::json!({
        "name": "p",
        "lspServers": {"s": {"command": program, "extensionToLanguage": {".zz": "zz"}}}
    });
    std::fs::write(
        root.join(".claude-plugin/plugin.json"),
        manifest.to_string(),
    )
    .unwrap();
    plugins::PluginDir {
        name: "p".into(),
        root,
        data_root: dir.join("data"),
    }
}

fn extension(source: Option<Arc<FakePlugins>>) -> LspExtension {
    LspExtension {
        manager: LspManager::new(),
        plugins: source.map(|s| s as Arc<dyn PluginSource>),
    }
}

#[tokio::test]
async fn a_plugins_server_turns_the_tool_on_and_survives_config_changes() {
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(FakePlugins {
        dirs: vec![plugin_with_server(dir.path())],
        calls: AtomicUsize::new(0),
    });
    let ext = extension(Some(source.clone()));
    let config = config(dir.path(), true).await;
    let servers = ext.plugin_servers(&config).await;
    let thread = LspThread::from_config(&config, None, ext.manager.clone(), servers.clone());
    assert!(
        thread.enabled,
        "the plugin's server is the only one, and it enables the tool"
    );
    assert!(
        thread
            .ctx
            .table
            .describe()
            .iter()
            .any(|line| line.contains("plugin:p:s")),
        "{:?}",
        thread.ctx.table.describe()
    );
    // A configuration change keeps the plugin's servers.
    let again = LspThread::from_config(
        &config,
        None,
        ext.manager.clone(),
        thread.plugin_servers.clone(),
    );
    assert!(again.enabled);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn plugins_are_not_read_while_the_feature_is_off_and_nothing_means_no_tool() {
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(FakePlugins {
        dirs: vec![plugin_with_server(dir.path())],
        calls: AtomicUsize::new(0),
    });
    let ext = extension(Some(source.clone()));
    let off = config(dir.path(), false).await;
    let servers = ext.plugin_servers(&off).await;
    assert!(servers.is_empty());
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    assert!(!LspThread::from_config(&off, None, ext.manager.clone(), servers).enabled);
    // On, but no plugin and no configured server: still no tool.
    let on = config(dir.path(), true).await;
    let ext = extension(None);
    let servers = ext.plugin_servers(&on).await;
    assert!(!LspThread::from_config(&on, None, ext.manager.clone(), servers).enabled);
}
