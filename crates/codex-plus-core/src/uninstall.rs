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
/// 安排一个分离进程,等 exe 不再被占用后删除它。
///
/// 这段实现踩了四个坑,逐条记下来免得后人重蹈:
///
/// 1. **不能轮询 PID**。原本用 `tasklist /fi "PID eq N" | find "N"` 判断主进程是否退出,
///    实测对活着的进程也返回「未找到」,于是文件在主进程还在跑时就被删了。
///    改成直接删:Windows 会锁住运行中的 exe 映像,**删不掉本身就说明进程还在**。
/// 2. **不能用 `timeout` 延时**。它依赖控制台,而清理进程是分离启动的(无控制台),
///    会立刻报错返回,导致循环瞬间跑完。用 `ping -n 2 127.0.0.1` 代替。
/// 3. **不能把长脚本塞进 `cmd /c "…"`**。脚本里含引号路径时,cmd 的 /c 引号剥离规则
///    会把命令解析坏 —— 实测整个循环一次都没执行(心跳文件为空)。
///    改成**先写 .bat 再执行**,彻底避开引号歧义。
/// 4. **`DETACHED_PROCESS` 会让 cmd 直接退出**。只用 `CREATE_NO_WINDOW` 即可隐藏窗口,
///    进程能正常存活到删除完成。
#[cfg(windows)]
fn schedule_self_delete(exe: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe_str = exe.to_string_lossy().to_string();
    let bat = exe.with_extension("cleanup.bat");
    // 最多重试 60 次(约 60 秒);删成功就跳出,最后把 .bat 自己也删掉
    let script = format!(
        "@echo off\r\n\
         for /l %%i in (1,1,60) do (\r\n\
         \x20 del /f /q \"{exe_str}\" >nul 2>&1\r\n\
         \x20 if not exist \"{exe_str}\" goto done\r\n\
         \x20 ping -n 2 127.0.0.1 >nul\r\n\
         )\r\n\
         :done\r\n\
         del /f /q \"%~f0\" >nul 2>&1\r\n"
    );
    std::fs::write(&bat, script)?;
    std::process::Command::new("cmd")
        .args(["/c", &bat.to_string_lossy()])
        .creation_flags(CREATE_NO_WINDOW)
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

    // 3) 清理指向本 exe 的开机自启项,再安排 exe 自删
    let exe: Option<PathBuf> = std::env::current_exe().ok();
    match exe {
        Some(path) => {
            // 从 Codex++ 迁移过来的用户,注册表 Run 里可能留着 CodexPlusPlusWatcher。
            // 不清的话,卸载之后每次开机都会去拉一个已经被删掉的 exe。
            if crate::watcher::uninstall_watcher_pointing_at(&path) {
                warnings.push("已一并清除开机自启项(旧版遗留)".to_string());
            }
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

    /// 卸载成功后面板必须调 `/quit` 而不是 `/restart-codex`。
    ///
    /// 这条曾经是错的:`/restart-codex` 会拉一个接班 launcher,把刚安排自删的 exe
    /// 重新锁住,清理脚本重试 60 次全部失败后放弃 —— 用户点了卸载,配置还了、
    /// 设备吊销了、快捷方式删了,程序却还在跑、exe 还躺在磁盘上。
    /// 这个断言直接钉住注入脚本,免得以后有人图省事改回去。
    #[test]
    fn panel_quits_instead_of_restarting_after_uninstall() {
        let panel = include_str!("../../../assets/inject/recodex-panel-inject.js");
        let uninstall_block = panel
            .split("function confirmUninstall")
            .nth(1)
            .expect("面板里应有 confirmUninstall");
        // 只看卸载这一段:文件别处用 /restart-codex 是正常的(切换运行模式等)
        let uninstall_block = &uninstall_block[..uninstall_block
            .find("function ")
            .unwrap_or(uninstall_block.len())];
        assert!(
            uninstall_block.contains("bridge(\"/quit\""),
            "卸载后必须调 /quit"
        );
        assert!(
            // 只匹配真正的桥调用 —— 注释里提到 /restart-codex 是在解释为什么不能用它
            !uninstall_block.contains("bridge(\"/restart-codex\""),
            "卸载后不能调 /restart-codex —— 接班进程会锁住待删的 exe"
        );
    }

    /// 开机自启只清**指向本 exe** 的那一条。
    ///
    /// 用户可能还单独装着 Codex++,那是别人的自启项 —— 卸我们的东西不该顺手删它。
    /// 这里用一个绝不可能出现在真实 Run 值里的路径,断言我们不会误伤。
    #[cfg(windows)]
    #[test]
    fn autostart_cleanup_only_touches_entries_pointing_at_us() {
        let bogus = std::env::temp_dir().join("recodex-not-installed-anywhere-12345.exe");
        assert!(
            !crate::watcher::uninstall_watcher_pointing_at(&bogus),
            "Run 值里没提到这个 exe,就不该动任何东西"
        );
    }

    #[cfg(windows)]
    #[test]
    fn self_delete_retries_until_the_running_binary_exits() {
        // 注意:Rust 的 File 句柄默认允许 delete 共享,**不能**用来模拟 exe 锁。
        // 真实的锁来自映像加载器,所以这里必须跑一个真的 exe。
        let dir = std::env::temp_dir().join(format!("recodex-selfdel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.exe");
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        std::fs::copy(format!(r"{system_root}\System32\cmd.exe"), &victim).unwrap();

        let mut child = std::process::Command::new(&victim)
            .args(["/c", "ping -n 5 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        schedule_self_delete(&victim).unwrap();

        // 进程在跑 -> 映像被锁 -> 删不掉
        std::thread::sleep(std::time::Duration::from_millis(2000));
        assert!(victim.exists(), "程序还在运行时不应被删除");

        child.wait().unwrap();
        let mut gone = false;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if !victim.exists() {
                gone = true;
                break;
            }
        }
        assert!(gone, "程序退出后应被清理");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
