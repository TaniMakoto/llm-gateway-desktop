use crate::error::AppError;
use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use std::path::{Path, PathBuf};

pub const AUTO_START_ARG: &str = "--autostart";
const APP_NAME: &str = "LLM Gateway Desktop";

/// alpha.8 旧版 macOS AppleScript 登录项使用 .app bundle 路径。
#[cfg(target_os = "macos")]
fn legacy_macos_app_bundle_path(exe_path: &Path) -> Option<PathBuf> {
    let path_str = exe_path.to_string_lossy();
    path_str.find(".app/Contents/MacOS/").map(|app_pos| {
        let app_bundle_end = app_pos + 4;
        PathBuf::from(&path_str[..app_bundle_end])
    })
}

fn current_executable_path() -> Result<PathBuf, AppError> {
    std::env::current_exe().map_err(|e| AppError::Message(format!("无法获取应用路径: {e}")))
}

fn normalized_path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `auto-launch 0.5` 在 Windows 注册表和 Linux desktop 文件中不会自动
/// 为包含空格的可执行文件路径加引号。这里仅对写入系统启动项的路径做转义；
/// 设置中仍保存未加引号的真实绝对路径，便于移动检测。
#[cfg(target_os = "windows")]
fn system_launch_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

#[cfg(target_os = "linux")]
fn system_launch_path(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('\"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "macos")]
fn system_launch_path(path: &Path) -> String {
    normalized_path_string(path)
}

#[cfg(target_os = "windows")]
fn paths_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(target_os = "windows"))]
fn paths_equal(left: &str, right: &str) -> bool {
    left == right
}

/// 当前进程是否由系统登录启动项拉起。
pub fn launched_by_auto_start() -> bool {
    std::env::args_os().any(|arg| arg.as_os_str() == std::ffi::OsStr::new(AUTO_START_ARG))
}

/// 当前可执行文件路径，供设置持久化和便携版移动检测使用。
pub fn current_executable_path_string() -> Result<String, AppError> {
    Ok(normalized_path_string(&current_executable_path()?))
}

/// 初始化 AutoLaunch 实例。
///
/// 安装版和便携版使用同一套注册方式：系统启动项始终指向当前正在运行的
/// 可执行文件，并携带 `--autostart`，从而能区分手动启动与登录启动。
fn get_auto_launch() -> Result<AutoLaunch, AppError> {
    let exe_path = current_executable_path()?;
    let launch_path = system_launch_path(&exe_path);
    let mut builder = AutoLaunchBuilder::new();
    builder
        .set_app_name(APP_NAME)
        .set_app_path(&launch_path)
        .set_args(&[AUTO_START_ARG]);

    // macOS 必须使用 LaunchAgent 才能把自定义参数传给可执行文件；
    // AppleScript 登录项只识别 --hidden/--minimized，无法可靠区分启动来源。
    #[cfg(target_os = "macos")]
    builder.set_use_launch_agent(true);

    builder
        .build()
        .map_err(|e| AppError::Message(format!("创建 AutoLaunch 失败: {e}")))
}

/// 构造 alpha.8 旧注册方式，用于升级时清理无参数启动项。
fn get_legacy_auto_launch() -> Result<AutoLaunch, AppError> {
    let exe_path = current_executable_path()?;

    #[cfg(target_os = "macos")]
    let app_path = legacy_macos_app_bundle_path(&exe_path).unwrap_or(exe_path);

    #[cfg(not(target_os = "macos"))]
    let app_path = exe_path;

    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&app_path.to_string_lossy())
        .build()
        .map_err(|e| AppError::Message(format!("创建旧版 AutoLaunch 失败: {e}")))
}

fn remove_legacy_auto_launch_best_effort() {
    let Ok(legacy) = get_legacy_auto_launch() else {
        return;
    };

    match legacy.is_enabled() {
        Ok(true) => {
            if let Err(error) = legacy.disable() {
                log::warn!("清理旧版开机启动项失败，将继续写入新启动项: {error}");
            } else {
                log::info!("已清理旧版无参数开机启动项");
            }
        }
        Ok(false) => {}
        Err(error) => log::debug!("检查旧版开机启动项失败: {error}"),
    }
}

/// 启用开机自启。
pub fn enable_auto_launch() -> Result<(), AppError> {
    // 先清理旧版无参数/AppleScript 注册，避免升级后双重启动。
    remove_legacy_auto_launch_best_effort();

    let auto_launch = get_auto_launch()?;
    auto_launch
        .enable()
        .map_err(|e| AppError::Message(format!("启用开机自启失败: {e}")))?;
    log::info!("已启用开机自启（启动参数: {AUTO_START_ARG}）");
    Ok(())
}

/// 禁用开机自启。
pub fn disable_auto_launch() -> Result<(), AppError> {
    let auto_launch = get_auto_launch()?;
    let current_result = auto_launch.disable();

    // 同时清理 alpha.8 旧注册方式。两个注册中任意一个成功删除即可认为
    // 禁用操作已完成；若两边都失败，再返回当前注册方式的错误。
    let legacy_result = get_legacy_auto_launch().and_then(|legacy| {
        legacy
            .disable()
            .map_err(|e| AppError::Message(format!("禁用旧版开机自启失败: {e}")))
    });

    match (current_result, legacy_result) {
        (Ok(()), _) | (_, Ok(())) => {
            log::info!("已禁用开机自启");
            Ok(())
        }
        (Err(current_error), Err(legacy_error)) => Err(AppError::Message(format!(
            "禁用开机自启失败: {current_error}; 旧版启动项清理也失败: {legacy_error}"
        ))),
    }
}

/// 检查当前格式的系统启动项是否已启用。
pub fn is_auto_launch_enabled() -> Result<bool, AppError> {
    let auto_launch = get_auto_launch()?;
    auto_launch
        .is_enabled()
        .map_err(|e| AppError::Message(format!("检查开机自启状态失败: {e}")))
}

/// 检查当前格式或 alpha.8 旧格式的启动项是否已启用。
///
/// Windows/Linux 的新旧注册共用同一个系统入口，当前格式的检查已经能够
/// 覆盖；macOS 旧版使用 AppleScript 登录项，需要额外检测。
pub fn is_any_auto_launch_enabled() -> Result<bool, AppError> {
    if is_auto_launch_enabled()? {
        return Ok(true);
    }

    #[cfg(target_os = "macos")]
    {
        let legacy = get_legacy_auto_launch()?;
        return legacy
            .is_enabled()
            .map_err(|e| AppError::Message(format!("检查旧版开机自启状态失败: {e}")));
    }

    #[cfg(not(target_os = "macos"))]
    Ok(false)
}

/// 便携版（以及安装路径发生变化的安装版）启动后修复系统启动项路径。
///
/// 仅在本地仍声明“开机自启动”且系统入口仍存在时迁移旧格式或修复路径。
/// 若用户在任务管理器/系统登录项中手动关闭启动项，则同步关闭本地设置，
/// 不会偷偷重新开启。
pub fn repair_auto_launch_path_if_needed() -> Result<bool, AppError> {
    let settings = crate::settings::get_settings();
    if !settings.launch_on_startup {
        return Ok(false);
    }

    let current_enabled = is_auto_launch_enabled()?;
    let any_enabled = is_any_auto_launch_enabled()?;

    // 尊重用户在任务管理器/系统登录项设置中的手动禁用，不偷偷重新开启。
    if !any_enabled {
        crate::settings::set_auto_launch_preference(false, None)?;
        log::info!("系统中的开机启动项已被关闭，已同步本地设置");
        return Ok(false);
    }

    let current_path = current_executable_path_string()?;
    let path_changed = settings
        .auto_launch_executable_path
        .as_deref()
        .map(|stored| !paths_equal(stored, &current_path))
        .unwrap_or(true);

    if !path_changed && current_enabled {
        return Ok(false);
    }

    // 路径变化，或检测到 macOS 旧版 AppleScript 登录项时，重写为当前
    // 可执行文件路径并携带 --autostart。
    enable_auto_launch()?;
    crate::settings::set_auto_launch_preference(true, Some(current_path.clone()))?;
    log::info!("已迁移或修复开机启动项: {current_path}");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_start_argument_is_stable() {
        assert_eq!(AUTO_START_ARG, "--autostart");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_launch_path_is_quoted() {
        assert_eq!(
            system_launch_path(Path::new(r"C:\Program Files\LLM Gateway Desktop.exe")),
            r#""C:\Program Files\LLM Gateway Desktop.exe""#
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_launch_path_is_quoted() {
        assert_eq!(
            system_launch_path(Path::new("/opt/LLM Gateway Desktop/app")),
            "\"/opt/LLM Gateway Desktop/app\""
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_macos_bundle_path_is_detected() {
        let path = Path::new(
            "/Applications/LLM Gateway Desktop.app/Contents/MacOS/LLM Gateway Desktop",
        );
        assert_eq!(
            legacy_macos_app_bundle_path(path),
            Some(PathBuf::from("/Applications/LLM Gateway Desktop.app"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_macos_bundle_path_supports_spaces() {
        let path = Path::new(
            "/Users/test/My Apps/LLM Gateway Desktop.app/Contents/MacOS/LLM Gateway Desktop",
        );
        assert_eq!(
            legacy_macos_app_bundle_path(path),
            Some(PathBuf::from(
                "/Users/test/My Apps/LLM Gateway Desktop.app"
            ))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_macos_bundle_path_ignores_standalone_binary() {
        assert_eq!(
            legacy_macos_app_bundle_path(Path::new("/usr/local/bin/llm-gateway")),
            None
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_path_comparison_is_case_insensitive() {
        assert!(paths_equal(
            r"C:\\Apps\\LLM Gateway Desktop.exe",
            r"c:\\apps\\llm gateway desktop.exe"
        ));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_path_comparison_is_case_sensitive() {
        assert!(!paths_equal("/Applications/App", "/applications/app"));
    }
}
