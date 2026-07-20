//! Portable-mode detection and path preparation.
//!
//! When a `portable.flag` file exists next to the executable, application-owned
//! persistent data is stored in the sibling `data` directory. This keeps the
//! portable ZIP self-contained while leaving installed builds unchanged.

use std::path::{Path, PathBuf};

const PORTABLE_FLAG_FILE: &str = "portable.flag";
const PORTABLE_DATA_DIR: &str = "data";

/// Pure helper used by tests and by runtime detection.
fn portable_root_from_executable(executable: &Path) -> Option<PathBuf> {
    let root = executable.parent()?;
    root.join(PORTABLE_FLAG_FILE)
        .is_file()
        .then(|| root.to_path_buf())
}

/// Returns the portable package root when portable mode is enabled.
///
/// `LLM_GATEWAY_PORTABLE_ROOT` is intentionally supported for automated tests
/// and advanced launchers. Normal users enable portable mode with
/// `portable.flag` next to the executable.
pub fn root_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("LLM_GATEWAY_PORTABLE_ROOT") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }

    let executable = std::env::current_exe().ok()?;
    portable_root_from_executable(&executable)
}

/// Returns `<portable root>/data` when portable mode is enabled.
pub fn data_dir() -> Option<PathBuf> {
    root_dir().map(|root| root.join(PORTABLE_DATA_DIR))
}

pub fn is_portable() -> bool {
    root_dir().is_some()
}

/// Creates portable directories and redirects Windows WebView2 browser data.
///
/// This must run before the Tauri builder creates its webview.
pub fn prepare_runtime() -> std::io::Result<Option<PathBuf>> {
    let Some(data_dir) = data_dir() else {
        return Ok(None);
    };

    std::fs::create_dir_all(&data_dir)?;

    #[cfg(target_os = "windows")]
    {
        let webview_dir = data_dir.join("webview2");
        std::fs::create_dir_all(&webview_dir)?;
        // WebView2 reads this process-scoped override while creating the
        // environment. Keeping the UDF here avoids browser cache/profile data
        // being written to the user's LocalAppData directory.
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &webview_dir);
    }

    Ok(Some(data_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_portable_detects_flag_next_to_executable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exe = temp.path().join("LLM Gateway Desktop.exe");
        std::fs::write(&exe, b"").expect("fake executable");
        std::fs::write(temp.path().join(PORTABLE_FLAG_FILE), b"").expect("portable flag");

        assert_eq!(
            portable_root_from_executable(&exe),
            Some(temp.path().to_path_buf())
        );
    }

    #[test]
    fn gateway_portable_ignores_executable_without_flag() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exe = temp.path().join("LLM Gateway Desktop.exe");
        std::fs::write(&exe, b"").expect("fake executable");

        assert_eq!(portable_root_from_executable(&exe), None);
    }
}