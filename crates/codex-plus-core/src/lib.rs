pub mod app_paths;
pub mod assets;
pub mod bridge;
pub mod cdp;
pub mod codex_app_state;
pub mod codex_home;
pub mod codex_local_storage;
pub mod codex_sqlite;
pub mod connect;
pub mod diagnostic_log;
pub mod http_client;
pub mod install;
pub mod launcher;
pub mod model_catalog;
pub mod model_suffix;
pub mod models;
pub mod native_menu;
pub mod paths;
pub mod plugin_marketplace;
pub mod ports;
pub mod protocol_proxy;
pub mod relay_config;
pub mod relay_rotation;
pub mod remote_control_recovery;
pub mod routes;
pub mod script_market;
pub mod selfupdate; // recodex-overlay: 自更新
pub mod settings;
pub mod status;
// recodex-overlay: Stepwise 已下线并删除
pub mod uninstall; // recodex-overlay: 卸载流程
pub mod update;
pub mod upstream_worktree;
pub mod user_scripts;
pub mod version;
pub mod vision;
pub mod watcher;
#[cfg(windows)]
mod windows_integration;
pub mod zed_remote;

#[cfg(windows)]
pub fn windows_create_no_window() -> u32 {
    windows_integration::CREATE_NO_WINDOW
}

#[cfg(windows)]
pub fn windows_open_url(url: &str) -> anyhow::Result<()> {
    windows_integration::open_url(url)
}

#[cfg(windows)]
pub fn windows_activate_process_window(process_id: u32) -> bool {
    windows_integration::activate_process_window(process_id)
}

#[cfg(windows)]
pub fn windows_apply_codexplusplus_icon_to_process_window(
    process_id: u32,
    icon_resource_path: std::path::PathBuf,
) -> bool {
    windows_integration::apply_codexplusplus_icon_to_process_window(process_id, icon_resource_path)
}

#[cfg(windows)]
pub fn windows_enumerate_processes() -> Vec<windows_integration::WindowsProcessInfo> {
    windows_integration::enumerate_processes()
}
