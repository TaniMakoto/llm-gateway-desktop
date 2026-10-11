//! Prepare, persist and publish a complete configuration under the runtime lock.
use super::*;

pub(super) fn registry_backup_path(path: &std::path::Path) -> PathBuf {
    path.with_extension("previous.json")
}

struct RegistryPublication {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    bytes: Vec<u8>,
    file: ModelRegistryOverridesFile,
    published: bool,
    committed: bool,
}

impl RegistryPublication {
    fn prepare(
        path: PathBuf,
        config: &GatewayConfig,
        committed_config: Option<&GatewayConfig>,
    ) -> Result<Self, AppError> {
        let mut previous = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(AppError::io(&path, error)),
        };
        if let Some(committed) = committed_config {
            let file_revision = previous
                .as_deref()
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
                .and_then(|value| value.get("configRevision").and_then(Value::as_u64));
            if file_revision.is_some_and(|revision| revision != committed.revision) {
                // A previous interrupted publication may have left a future main
                // file. Never replace the last valid backup with that future copy.
                previous = Some(
                    serde_json::to_vec_pretty(&ModelRegistryOverridesFile {
                        version: MODEL_REGISTRY_OVERRIDES_VERSION,
                        config_revision: Some(committed.revision),
                        overrides: committed.model_registry_overrides.clone(),
                    })
                    .map_err(|source| AppError::JsonSerialize { source })?,
                );
            }
        }
        let file = ModelRegistryOverridesFile {
            version: MODEL_REGISTRY_OVERRIDES_VERSION,
            config_revision: Some(config.revision),
            overrides: normalize_registry_overrides(config.model_registry_overrides.clone()),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|source| AppError::JsonSerialize { source })?;
        Ok(Self {
            path,
            previous,
            bytes,
            file,
            published: false,
            committed: false,
        })
    }

    fn publish(&mut self) -> Result<(), AppError> {
        // A persistent previous copy also handles process termination between
        // publishing the file and committing SQLite, before Drop can run.
        if let Some(previous) = &self.previous {
            crate::config::atomic_write(&registry_backup_path(&self.path), previous)?;
        }
        self.published = true;
        crate::config::atomic_write(&self.path, &self.bytes)
    }

    fn accept(&mut self) {
        self.committed = true;
        let mut cache = model_registry_overrides_cache()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.path = Some(self.path.clone());
        cache.loaded = true;
        cache.overrides = Some(self.file.clone());
    }
}

impl Drop for RegistryPublication {
    fn drop(&mut self) {
        if !self.published || self.committed {
            return;
        }
        let restored = match &self.previous {
            Some(bytes) => crate::config::atomic_write(&self.path, bytes),
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(AppError::io(&self.path, error)),
            },
        };
        if let Err(error) = restored {
            log::error!("恢复模型覆盖文件失败，读取时仍以已提交配置版本为准: {error}");
        }
    }
}

fn generated_providers(config: &GatewayConfig) -> Vec<(String, Provider)> {
    let combos = iter_materialized_combos(config);
    ["claude", "codex"]
        .into_iter()
        .flat_map(|app| {
            combos
                .iter()
                .enumerate()
                .map(move |(index, (_, provider, format))| {
                    (
                        app.to_string(),
                        materialize_provider(
                            provider,
                            *format,
                            app,
                            index,
                            &config.model_registry_overrides,
                            Some(config.revision),
                        ),
                    )
                })
        })
        .collect()
}

fn max_retries(config: &GatewayConfig) -> u32 {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for provider in config.providers.iter().filter(|provider| provider.enabled) {
        for model in provider.models.iter().filter(|model| model.enabled) {
            *counts.entry(model.alias.trim()).or_default() += 1;
        }
    }
    counts
        .values()
        .copied()
        .max()
        .unwrap_or(1)
        .saturating_sub(1)
        .min(u32::MAX as usize) as u32
}

pub(super) fn sync_providers(db: &Database, config: &GatewayConfig) -> Result<(), AppError> {
    let proxy = crate::proxy::ProxyConfig {
        listen_address: config.listen_address.clone(),
        listen_port: config.listen_port,
        enable_logging: config.enable_logging,
        ..Default::default()
    };
    db.commit_gateway_config(
        CONFIG_KEY,
        None,
        Some(config.revision),
        &proxy,
        GENERATED_CATEGORY,
        &generated_providers(config),
        max_retries(config),
        || Ok(()),
    )
}

async fn proxy_config(
    state: &AppState,
    config: &GatewayConfig,
) -> Result<crate::proxy::ProxyConfig, String> {
    let mut proxy = state.gateway_runtime.get_config().await?;
    proxy.listen_address = config.listen_address.clone();
    proxy.listen_port = config.listen_port;
    proxy.enable_logging = config.enable_logging;
    Ok(proxy)
}

pub(super) async fn apply(state: &AppState, config: &GatewayConfig) -> Result<(), String> {
    let proxy = proxy_config(state, config).await?;
    let providers = generated_providers(config);
    state
        .gateway_runtime
        .commit_reconfiguration(&proxy, || {
            state
                .db
                .commit_gateway_config(
                    CONFIG_KEY,
                    None,
                    Some(config.revision),
                    &proxy,
                    GENERATED_CATEGORY,
                    &providers,
                    max_retries(config),
                    || Ok(()),
                )
                .map_err(|error| error.to_string())
        })
        .await
}

pub(super) async fn save(
    state: &AppState,
    mut config: GatewayConfig,
) -> Result<GatewayConfig, String> {
    // Load the previous file into the cache before staging publication. During
    // the transaction readers see either the old complete revision or the new one.
    let previous_config = load_config(&state.db).map_err(|error| error.to_string())?;
    let expected_revision = config.revision;
    config.revision = expected_revision.checked_add(1).ok_or("配置版本号已耗尽")?;
    let proxy = proxy_config(state, &config).await?;
    let providers = generated_providers(&config);
    // Embed overrides as a durable recovery copy; a matching user-editable file
    // remains authoritative. Stale managed files cannot overwrite newer config.
    let serialized = serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?;
    let registry = RegistryPublication::prepare(
        model_registry_overrides_path(),
        &config,
        Some(&previous_config),
    )
    .map_err(|error| error.to_string())?;
    state
        .gateway_runtime
        .commit_reconfiguration(&proxy, || {
            // Restore a rejected publication before releasing the runtime lock; a
            // concurrent save must not be overwritten by this revision's Drop.
            let mut registry = registry;
            state
                .db
                .commit_gateway_config(
                    CONFIG_KEY,
                    Some(&serialized),
                    Some(expected_revision),
                    &proxy,
                    GENERATED_CATEGORY,
                    &providers,
                    max_retries(&config),
                    || registry.publish(),
                )
                .map_err(|error| error.to_string())?;
            registry.accept();
            Ok(())
        })
        .await?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncommitted_registry_publication_restores_the_previous_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("overrides.json");
        let previous = br#"{"version":1,"configRevision":3,"overrides":{}}"#;
        std::fs::write(&path, previous).unwrap();
        {
            let mut staged = RegistryPublication::prepare(
                path.clone(),
                &GatewayConfig {
                    revision: 4,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            staged.publish().unwrap();
            assert_eq!(
                read_model_registry_overrides_file(&path)
                    .unwrap()
                    .unwrap()
                    .config_revision,
                Some(4)
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert_eq!(
            std::fs::read(registry_backup_path(&path)).unwrap(),
            previous
        );
    }

    #[test]
    fn retry_after_interrupted_publication_keeps_the_committed_backup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("overrides.json");
        std::fs::write(&path, br#"{"version":1,"configRevision":4,"overrides":{}}"#).unwrap();
        let committed = GatewayConfig {
            revision: 3,
            ..Default::default()
        };
        {
            let mut staged = RegistryPublication::prepare(
                path.clone(),
                &GatewayConfig {
                    revision: 4,
                    ..Default::default()
                },
                Some(&committed),
            )
            .unwrap();
            staged.publish().unwrap();
        }
        assert_eq!(
            read_model_registry_overrides_file(&path)
                .unwrap()
                .unwrap()
                .config_revision,
            Some(3)
        );
        assert_eq!(
            read_model_registry_overrides_file(&registry_backup_path(&path))
                .unwrap()
                .unwrap()
                .config_revision,
            Some(3)
        );
    }
}
