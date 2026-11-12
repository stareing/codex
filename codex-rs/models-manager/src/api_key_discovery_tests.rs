//! API-key discovery opt-outs must gate network requests and cached catalog authority.

use super::*;
use codex_protocol::openai_models::ModelOrigin;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn api_key_discovery_disabled_preserves_command_auth_discovery_and_merging() {
    let models = vec![remote_model(
        "command-auth",
        "Command Auth",
        /*priority*/ 0,
    )];
    let endpoint = Arc::new(TestModelsEndpoint {
        has_command_auth: true,
        responses: Mutex::new(vec![Ok(models.clone())].into()),
        ..TestModelsEndpoint::default()
    });
    let manager = OpenAiModelsManager::new_without_cache(
        endpoint.clone(),
        Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
            "test-key",
        ))),
    );
    let mut merged = load_remote_models_from_file().unwrap();
    merged.extend(models.clone());
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        merged
    );
    assert_eq!(endpoint.fetch_count(), 1);
}

#[tokio::test]
async fn api_key_discovery_flag_controls_fetches_and_cached_catalogs() {
    let home = tempdir().unwrap();
    let models = vec![remote_model("dynamic", "Dynamic", /*priority*/ 0)];
    let endpoint = TestModelsEndpoint::without_refresh(vec![models.clone()]);
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test-key"));
    let manager =
        OpenAiModelsManager::new(home.path().into(), endpoint.clone(), Some(auth.clone()));
    let bundled = load_remote_models_from_file().unwrap();
    let config = ModelsManagerConfig::default();

    for strategy in [
        RefreshStrategy::Online,
        RefreshStrategy::OnlineIfUncached,
        RefreshStrategy::Offline,
    ] {
        assert_eq!(
            manager
                .raw_model_catalog(strategy, DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .models,
            bundled
        );
    }
    assert_eq!(endpoint.fetch_count(), 0);

    let manager =
        OpenAiModelsManager::new(home.path().into(), endpoint.clone(), Some(auth.clone()));
    manager.set_api_key_model_discovery_enabled(/*enabled*/ true);
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        models
    );
    assert_eq!(endpoint.fetch_count(), 1);
    assert_eq!(
        manager.get_model_info("dynamic", &config).await,
        CatalogModel {
            model: models[0].clone().into(),
            origin: ModelOrigin::Provider
        }
    );

    // A runtime rollback must also suppress the already-loaded catalog.
    manager.set_api_key_model_discovery_enabled(/*enabled*/ false);
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        bundled
    );
    assert_eq!(manager.try_get_remote_models().unwrap(), bundled);
    assert_eq!(
        manager.get_model_info("dynamic", &config).await.origin,
        ModelOrigin::Synthetic
    );
    assert_eq!(endpoint.fetch_count(), 1);

    // A new session must honor its startup flag even when a matching disk cache exists.
    for enabled in [false, true] {
        let restarted =
            OpenAiModelsManager::new(home.path().into(), endpoint.clone(), Some(auth.clone()));
        restarted.set_api_key_model_discovery_enabled(enabled);
        let expected = if enabled { &models } else { &bundled };
        for strategy in [RefreshStrategy::Offline, RefreshStrategy::OnlineIfUncached] {
            assert_eq!(
                &restarted
                    .raw_model_catalog(strategy, DEFAULT_HTTP_CLIENT_FACTORY)
                    .await
                    .models,
                expected
            );
        }
        assert_eq!(&restarted.try_get_remote_models().unwrap(), expected);
    }
    assert_eq!(endpoint.fetch_count(), 1);
}

#[tokio::test]
async fn own_catalog_replaces_bundled_presets_and_fetches_without_the_feature_flag() {
    let fallback = vec![remote_model("suggested", "Suggested", /*priority*/ 0)];
    let served = vec![
        remote_model("site-a", "Site A", /*priority*/ 0),
        remote_model("site-b", "Site B", /*priority*/ 1),
    ];
    let endpoint = TestModelsEndpoint::without_refresh(vec![served.clone()]);
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("device-token"));
    let manager = OpenAiModelsManager::new_without_cache(endpoint.clone(), Some(auth))
        .with_own_catalog(fallback.clone());

    // Before any fetch only the suggested model is known — never the upstream presets.
    assert_eq!(manager.try_get_remote_models().unwrap(), fallback);
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Offline, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        fallback
    );
    assert_eq!(endpoint.fetch_count(), 0);

    // The provider's list is fetched although the API-key discovery feature is off, and is
    // authoritative (no bundled or fallback entries merged in).
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        served
    );
    assert_eq!(endpoint.fetch_count(), 1);
}

#[tokio::test]
async fn own_catalog_is_fetched_when_the_transport_alone_authenticates() {
    // The cloud sandbox reaches the site's gateway over a per-tenant socket: no key, no auth.
    let served = vec![remote_model("gateway-a", "Gateway A", /*priority*/ 0)];
    let endpoint = TestModelsEndpoint::without_refresh(vec![served.clone()]);
    let manager =
        OpenAiModelsManager::new_without_cache(endpoint.clone(), None).with_own_catalog(Vec::new());
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        served
    );
    assert_eq!(endpoint.fetch_count(), 1);
}

#[tokio::test]
async fn own_catalog_merges_a_hidden_only_list_into_the_fallback_not_the_bundled_presets() {
    let fallback = vec![remote_model("suggested", "Suggested", /*priority*/ 0)];
    let hidden = vec![remote_model_with_visibility(
        "hidden", "Hidden", /*priority*/ 1, "hide",
    )];
    let endpoint = TestModelsEndpoint::without_refresh(vec![hidden.clone()]);
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("device-token"));
    let manager = OpenAiModelsManager::new_without_cache(endpoint, Some(auth))
        .with_own_catalog(fallback.clone());
    let mut expected = fallback;
    expected.extend(hidden);
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models,
        expected
    );
}
