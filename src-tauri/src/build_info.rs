//! Build identity shared by desktop diagnostics and the gateway status endpoint.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: &'static str,
    pub built_at: &'static str,
}

#[tauri::command]
pub fn get_build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        commit: option_env!("GATEWAY_BUILD_COMMIT").unwrap_or("unknown"),
        built_at: option_env!("GATEWAY_BUILD_TIME").unwrap_or("unknown"),
    }
}
