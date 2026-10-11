//! Atomic persistence for one complete gateway configuration revision.
use super::{lock_conn, Database};
use crate::{error::AppError, provider::Provider, proxy::ProxyConfig};
use rusqlite::{params, OptionalExtension};
use std::collections::HashSet;

impl Database {
    pub(crate) fn commit_gateway_config(
        &self,
        config_key: &str,
        serialized: Option<&str>,
        expected_revision: Option<u64>,
        config: &ProxyConfig,
        generated_category: &str,
        providers: &[(String, Provider)],
        max_retries: u32,
        publish_registry: impl FnOnce() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction().map_err(database_error)?;
        if let Some(expected) = expected_revision {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    [config_key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(database_error)?;
            let revision = previous
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .map_err(|error| AppError::Config(error.to_string()))?
                .and_then(|value| value.get("revision").and_then(serde_json::Value::as_u64))
                .unwrap_or(0);
            if revision != expected {
                return Err(AppError::Config(
                    "配置已被其他操作更新，请重新读取后保存".into(),
                ));
            }
        }
        let wanted: HashSet<_> = providers
            .iter()
            .map(|(app, provider)| (app.as_str(), provider.id.as_str()))
            .collect();
        let existing = {
            let mut statement = tx.prepare("SELECT app_type, id FROM providers WHERE category = ?1 AND app_type IN ('claude','codex')").map_err(database_error)?;
            let rows = statement
                .query_map([generated_category], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(database_error)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(database_error)?
        };
        for (app, id) in existing {
            if !wanted.contains(&(app.as_str(), id.as_str())) {
                tx.execute(
                    "DELETE FROM providers WHERE id = ?1 AND app_type = ?2",
                    params![id, app],
                )
                .map_err(database_error)?;
            }
        }
        for (app, provider) in providers {
            Self::save_provider_in_transaction(&tx, app, provider)?;
        }
        // Keep the existing per-protocol timeout/health settings; only gateway-owned
        // fields are mirrored into this legacy storage during the transition.
        tx.execute("UPDATE proxy_config SET listen_address = ?1, listen_port = ?2, enable_logging = ?3, updated_at = datetime('now')",
            params![config.listen_address, config.listen_port, config.enable_logging]).map_err(database_error)?;
        tx.execute("UPDATE proxy_config SET auto_failover_enabled = 1, max_retries = ?1 WHERE app_type IN ('claude','codex')", [max_retries]).map_err(database_error)?;
        if let Some(serialized) = serialized {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![config_key, serialized],
            )
            .map_err(database_error)?;
        }
        // No configuration reader can observe the staged rows while this lock is
        // held. The caller restores the registry file if publication/commit fails.
        publish_registry()?;
        tx.commit().map_err(database_error)
    }
}

fn database_error(error: rusqlite::Error) -> AppError {
    AppError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publication_failure_rolls_back_providers_settings_and_listener_config() {
        let db = Database::memory().unwrap();
        db.set_setting("gateway-test", r#"{"revision":4}"#).unwrap();
        let provider = Provider::with_id("new".into(), "new".into(), json!({}), None);
        let next = ProxyConfig {
            listen_port: 23456,
            ..Default::default()
        };
        let result = db.commit_gateway_config(
            "gateway-test",
            Some(r#"{"revision":5}"#),
            Some(4),
            &next,
            "unified_gateway",
            &[("claude".into(), provider)],
            9,
            || Err(AppError::Config("registry write rejected".into())),
        );
        assert!(result.is_err());
        assert_eq!(
            db.get_setting("gateway-test").unwrap().unwrap(),
            r#"{"revision":4}"#
        );
        assert!(db.get_provider_by_id("new", "claude").unwrap().is_none());
        let conn = db.conn.lock().unwrap();
        let port: u16 = conn
            .query_row(
                "SELECT listen_port FROM proxy_config WHERE app_type='claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(port, next.listen_port);
    }

    #[test]
    fn stale_revision_is_rejected_before_any_publication() {
        let db = Database::memory().unwrap();
        db.set_setting("gateway-test", r#"{"revision":4}"#).unwrap();
        let result = db.commit_gateway_config(
            "gateway-test",
            Some(r#"{"revision":5}"#),
            Some(3),
            &ProxyConfig::default(),
            "unified_gateway",
            &[],
            0,
            || panic!("stale configuration must not publish a file"),
        );
        assert!(result.is_err());
        assert_eq!(
            db.get_setting("gateway-test").unwrap().unwrap(),
            r#"{"revision":4}"#
        );
    }

    #[test]
    fn committed_revision_replaces_only_generated_providers() {
        let db = Database::memory().unwrap();
        db.set_setting("gateway-test", r#"{"revision":4}"#).unwrap();
        let mut obsolete = Provider::with_id("obsolete".into(), "old".into(), json!({}), None);
        obsolete.category = Some("unified_gateway".into());
        db.save_provider("claude", &obsolete).unwrap();
        let manual = Provider::with_id("manual".into(), "user".into(), json!({}), None);
        db.save_provider("claude", &manual).unwrap();
        let mut next = Provider::with_id("next".into(), "next".into(), json!({}), None);
        next.category = Some("unified_gateway".into());
        let proxy = ProxyConfig {
            listen_port: 23456,
            ..Default::default()
        };
        db.commit_gateway_config(
            "gateway-test",
            Some(r#"{"revision":5}"#),
            Some(4),
            &proxy,
            "unified_gateway",
            &[("claude".into(), next)],
            2,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(
            db.get_setting("gateway-test").unwrap().as_deref(),
            Some(r#"{"revision":5}"#)
        );
        assert!(db
            .get_provider_by_id("obsolete", "claude")
            .unwrap()
            .is_none());
        assert!(db.get_provider_by_id("manual", "claude").unwrap().is_some());
        assert!(db.get_provider_by_id("next", "claude").unwrap().is_some());
        let conn = db.conn.lock().unwrap();
        let (port, retries): (u16, u32) = conn
            .query_row(
                "SELECT listen_port, max_retries FROM proxy_config WHERE app_type='claude'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((port, retries), (23456, 2));
    }
}
