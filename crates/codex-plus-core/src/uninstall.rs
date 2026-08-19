//! recodex-overlay: 卸载 ReCodex。
//!
//! 这是**不可逆**操作,面板侧必须二次确认后才会调到这里。顺序是刻意排的:
//!   1. 先还原用户的 Codex 配置(config.toml / auth.json / RECODEX_KEY)——
//!      这一步失败就中止,不能把用户扔在"配置被改过但程序已删"的状态;
//!   2. 再删我们自己的数据目录与快捷方式;
//!   3. 最后安排 exe 自删(Windows 上运行中的 exe 删不掉自己,交给分离的清理进程)。
//!
//! 服务端吊销设备 + 清 Windows 凭据由调用方(desktop 桥的 logout)先做,
//! 因为那需要持有 ReCodexState。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// 删除我们在 `~/.codex` 下建的目录(不碰用户自己的东西)。
fn remove_codex_owned_dir() -> std::io::Result<()> {
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return Ok(());
    };
    let dir = home.join(".codex").join("recodex");
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// 安排一个分离的清理进程:等本进程退出后删掉 exe 及其所在目录里的残留。
///
/// 运行中的 exe 无法删除自身,所以只能交给外部进程。用 `cmd /c` 起一个隐藏窗口,
/// 先轮询等待进程退出(而不是死等固定秒数),再删文件。
#[cfg(windows)]
fn schedule_self_delete(exe: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let pid = std::process::id();
    let exe_str = exe.to_string_lossy().to_string();
    // 最多等 30 次 × 1s;进程还在就继续等,退出后再删,避免删不掉又静默失败。
    let script = format!(
        "for /l %i in (1,1,30) do (tasklist /fi \"PID eq {pid}\" | find \"{pid}\" >nul || (del /f /q \"{exe_str}\" & exit)) & timeout /t 1 /nobreak >nul"
    );
    std::process::Command::new("cmd")
        .args(["/c", &script])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn schedule_self_delete(exe: &Path) -> std::io::Result<()> {
    // 类 Unix 上可以直接删掉正在运行的可执行文件
    std::fs::remove_file(exe)
}

/// 执行卸载。`restore` 由调用方注入(还原 Codex 配置的闭包),
/// 这样 core 不必依赖 recodex-integration。
pub fn perform_uninstall<F>(restore: F) -> Value
where
    F: FnOnce() -> Result<(), String>,
{
    // 1) 还原用户配置 —— 失败就停,别让用户配置回不去
    if let Err(message) = restore() {
        return json!({
            "status": "failed",
            "message": format!("还原 Codex 配置失败,已中止卸载:{message}")
        });
    }

    let mut warnings: Vec<String> = Vec::new();

    // 2) 删我们的数据目录与快捷方式
    if let Err(error) = remove_codex_owned_dir() {
        warnings.push(format!("删除 ~/.codex/recodex 失败:{error}"));
    }
    let options = crate::install::InstallOptions {
        remove_owned_data: true,
        ..Default::default()
    };
    let result = crate::install::uninstall_entrypoints(&options);
    if result.status != "ok" {
        warnings.push(format!("卸载快捷方式:{}", result.message));
    }

    // 3) 安排 exe 自删
    let exe: Option<PathBuf> = std::env::current_exe().ok();
    match exe {
        Some(path) => {
            if let Err(error) = schedule_self_delete(&path) {
                warnings.push(format!("安排删除程序文件失败:{error}"));
            }
        }
        None => warnings.push("无法定位程序文件,请手动删除".to_string()),
    }

    json!({
        "status": "ok",
        "message": "ReCodex 已卸载,程序将在退出后自动删除",
        "warnings": warnings
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_failure_aborts_before_touching_anything() {
        // 配置还原失败必须中止 —— 否则用户会落在"配置被改过但程序没了"的状态
        let value = perform_uninstall(|| Err("boom".to_string()));
        assert_eq!(value["status"], "failed");
        assert!(
            value["message"].as_str().unwrap_or_default().contains("boom"),
            "错误原因要透传给用户,而不是吞掉"
        );
    }
}
