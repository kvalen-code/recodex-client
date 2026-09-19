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

/// 删除我们在 `%LOCALAPPDATA%` / `%APPDATA%` 下建的 `ReCodex` 目录。
///
/// `remove_owned_data()` 只清 `~/.recodex`,这两个目录一直漏在外面:
///   - `%LOCALAPPDATA%\ReCodex\device-id` —— **这个留着最要命**:
///     卸载时服务端已经把该设备吊销了,重装后却还拿同一个 ID 去登录;
///   - `%LOCALAPPDATA%\ReCodex\official-mode.json` —— 官方模式快照;
///   - `%APPDATA%\ReCodex\user_scripts` —— 用户脚本。
///
/// 返回被删掉的目录数,供卸载结果里给用户一句交代。
fn remove_appdata_dirs(warnings: &mut Vec<String>) -> usize {
    let mut removed = 0;
    for key in ["LOCALAPPDATA", "APPDATA"] {
        let Some(base) = std::env::var_os(key) else {
            continue;
        };
        let dir = PathBuf::from(base).join("ReCodex");
        if !dir.exists() {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(error) => warnings.push(format!("删除 {} 失败:{error}", dir.display())),
        }
    }
    removed
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
    schedule_delete_after_exit(exe, &[], None)
}

/// 清理脚本本体(纯文本,单测覆盖)。先删 `exe`(删不掉 = 还在运行,隔一秒重试,最多约 60 秒),
/// 再删 `extra`;给了 `remove_dir` 就在最后把它(非递归,空了才删得掉)一并删掉。
///
/// 最后一行把「删脚本自己」与 rmdir 写在**同一行**:cmd 按行读批处理,删掉自己之后
/// 下一行就读不到了 —— 同一行里的命令在删之前已经解析完,照样执行。
pub fn build_cleanup_script(exe: &Path, extra: &[PathBuf], remove_dir: Option<&Path>) -> String {
    let exe_str = exe.to_string_lossy();
    let mut script = format!(
        "@echo off\r\n\
         for /l %%i in (1,1,60) do (\r\n\
         \x20 del /f /q \"{exe_str}\" >nul 2>&1\r\n\
         \x20 if not exist \"{exe_str}\" goto done\r\n\
         \x20 ping -n 2 127.0.0.1 >nul\r\n\
         )\r\n\
         :done\r\n"
    );
    for file in extra {
        script.push_str(&format!("del /f /q \"{}\" >nul 2>&1\r\n", file.to_string_lossy()));
    }
    match remove_dir {
        Some(dir) => script.push_str(&format!(
            "del /f /q \"%~f0\" >nul 2>&1 & rmdir \"{}\" >nul 2>&1\r\n",
            dir.to_string_lossy()
        )),
        None => script.push_str("del /f /q \"%~f0\" >nul 2>&1\r\n"),
    }
    script
}

/// 要删目录时脚本放到临时目录、工作目录也设到临时目录 —— 否则脚本自己和 cmd 的
/// 当前目录都会占着那个目录,rmdir 永远失败。
#[cfg(windows)]
fn schedule_delete_after_exit(exe: &Path, extra: &[PathBuf], remove_dir: Option<&Path>) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let bat = match remove_dir {
        Some(_) => std::env::temp_dir().join(format!("recodex-cleanup-{}.bat", std::process::id())),
        None => exe.with_extension("cleanup.bat"),
    };
    std::fs::write(&bat, build_cleanup_script(exe, extra, remove_dir))?;
    std::process::Command::new("cmd")
        .args(["/c", &bat.to_string_lossy()])
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

/// `recodex.exe --legacy-uninstall`:「程序和功能」里卸载 1.3.4 之前装的 ReCodex。
///
/// 卸载项的 UninstallString 由 legacy_install 在迁移时改指这里(见
/// `legacy_install::legacy_uninstall_redirect`)。做的事:
///   1. 原地跑老的 `uninstall.exe`(`_?=<目录>`:NSIS 默认会把自己拷到临时目录后立刻返回,
///      带上它才会在原处跑、并等它结束)—— 确认页、删快捷方式和注册表都照旧由它做;
///   2. 卸载项还在 = 用户在确认页点了取消(或它失败了):什么都不删,原样退出;
///   3. 卸载项没了:停掉其它还在跑的 ReCodex、停手机远程守护进程并撤开机自启,
///      再安排删除 recodex.exe、老卸载程序与安装目录(非递归,里面还有别的东西就留着)。
///
/// 老卸载程序已经不在了(被人手动删过)时直接走第 3 步,并替它删掉快捷方式与卸载项 ——
/// 用户是在「程序和功能」里点了卸载的,不能让他卡在一个卸不掉的条目上。
#[cfg(windows)]
pub fn run_legacy_uninstall() -> anyhow::Result<()> {
    const UNINSTALL_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex";
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法定位安装目录"))?
        .to_path_buf();
    let uninstaller = dir.join("uninstall.exe");
    let mut detail = json!({ "uninstaller_present": uninstaller.is_file() });
    if uninstaller.is_file() {
        let status = std::process::Command::new(&uninstaller)
            .arg(format!("_?={}", dir.display()))
            .current_dir(std::env::temp_dir())
            .status()?;
        detail["uninstaller_exit"] = json!(status.code());
        let still_registered =
            crate::windows_integration::read_current_user_string_values(UNINSTALL_SUBKEY)
                .is_ok_and(|values| !values.is_empty());
        if still_registered {
            detail["cancelled"] = json!(true);
            let _ = crate::diagnostic_log::append_diagnostic_log("launcher.legacy_uninstall", detail);
            return Ok(());
        }
    } else {
        let result =
            crate::install::uninstall_entrypoints(&crate::install::InstallOptions::default());
        detail["entrypoints"] = json!(result.status);
        let _ = crate::windows_integration::delete_current_user_key(UNINSTALL_SUBKEY);
        let _ = crate::windows_integration::delete_current_user_key(r"Software\ReCodex");
    }
    crate::watcher::stop_launcher_processes_and_wait();
    detail["remote"] = json!(crate::phone_remote::uninstall_cleanup());
    // 与 ReCodex.nsi 卸载段同一张单子:自更新(.old/.new)与旧名接班(.migrating)的残留,
    // 不清的话最后那步删不掉安装目录
    let extra = [
        uninstaller,
        dir.join("recodex.exe.old"),
        dir.join("recodex.exe.new"),
        dir.join("recodex.exe.migrating"),
        dir.join("codex-plus-plus.exe"),
        dir.join("codex-plus-plus.exe.old"),
        dir.join("codex-plus-plus.exe.new"),
        dir.join("codex-plus-plus.exe.migrating"),
    ];
    let scheduled = schedule_delete_after_exit(&exe, &extra, Some(&dir));
    if let Err(error) = &scheduled {
        detail["error"] = json!(error.to_string());
    }
    let _ = crate::diagnostic_log::append_diagnostic_log("launcher.legacy_uninstall", detail);
    scheduled.map_err(Into::into)
}

#[cfg(not(windows))]
pub fn run_legacy_uninstall() -> anyhow::Result<()> {
    anyhow::bail!("--legacy-uninstall 只用于 Windows")
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

    // 手机远程:先停守护进程、撤开机自启 —— 下一步会删掉 ~/.recodex(运行时就在里面),
    // 不停的话 Windows 上运行时文件被占着删不掉,开机自启项也会指向一个不存在的程序。
    warnings.extend(crate::phone_remote::uninstall_cleanup());

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
    // 设备 ID 必须删掉:服务端刚把这台设备吊销了,留着它重装后会拿一个已吊销的身份去登录
    if remove_appdata_dirs(&mut warnings) > 0 {
        warnings.push("已删除设备标识与用户脚本目录".to_string());
    }
    // 以前出货过的管理工具留下的 WebView 数据目录(%LOCALAPPDATA%\com.bigpizzav3.codexplusplus.manager)。
    // 名字与上游 Codex++ 共用:机器上还装着上游、或目录里有 WebView 缓存以外的东西,就不碰。
    // 结果会显示给用户,不带目录名(那个名字就是上游品牌)。
    if crate::legacy_install::remove_legacy_manager_data_for_uninstall().is_some() {
        warnings.push("已清除旧版管理工具的缓存".to_string());
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
    fn source_without_comments(source: &str) -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **调用点**守卫:下面那两条单测只验了清理函数本身。
    /// 把 `perform_uninstall` 里的调用删掉,它们照样绿 —— 实测过,189 项全过。
    /// 清理函数写对了却没人调,和没写是一样的。
    #[test]
    fn cleanup_helpers_are_actually_called_by_perform_uninstall() {
        let body = source_without_comments(include_str!("uninstall.rs"));
        let perform = body
            .split("pub fn perform_uninstall")
            .nth(1)
            .expect("perform_uninstall 应存在");
        let perform = &perform[..perform.find("\n#[cfg(test)]").unwrap_or(perform.len())];

        assert!(
            perform.contains("remove_appdata_dirs(&mut warnings)"),
            "必须清理 %LOCALAPPDATA%/%APPDATA% 下的 ReCodex 目录 —— 设备 ID 留着重装会用已吊销的身份"
        );
        assert!(
            perform.contains("uninstall_watcher_pointing_at("),
            "必须清理指向本 exe 的开机自启项,否则卸完每次开机都去拉一个已删除的 exe"
        );
        assert!(
            perform.contains("remove_codex_owned_dir()"),
            "必须清理 ~/.codex/recodex"
        );
        assert!(
            perform.contains("phone_remote::uninstall_cleanup()"),
            "必须停手机远程守护进程并撤开机自启,否则卸完每次开机都去拉一个已删除的运行时"
        );
    }

    #[test]
    fn cleanup_script_removes_extra_files_and_the_directory_on_one_final_line() {
        let dir = PathBuf::from(r"C:\Users\u\AppData\Local\Programs\ReCodex");
        let script = build_cleanup_script(
            &dir.join("recodex.exe"),
            &[dir.join("uninstall.exe")],
            Some(&dir),
        );
        assert!(script.starts_with("@echo off\r\n"));
        assert!(script.contains("del /f /q \"C:\\Users\\u\\AppData\\Local\\Programs\\ReCodex\\recodex.exe\""));
        assert!(script.contains("del /f /q \"C:\\Users\\u\\AppData\\Local\\Programs\\ReCodex\\uninstall.exe\" >nul 2>&1\r\n"));
        // 删脚本自己与 rmdir 必须在同一行(删了自己之后 cmd 读不到下一行)
        let last = script.trim_end().lines().last().unwrap();
        assert_eq!(
            last,
            "del /f /q \"%~f0\" >nul 2>&1 & rmdir \"C:\\Users\\u\\AppData\\Local\\Programs\\ReCodex\" >nul 2>&1"
        );
        // 不递归删目录:用户放在里面的东西不能跟着没了
        assert!(!script.contains("/s"));
        let plain = build_cleanup_script(&dir.join("recodex.exe"), &[], None);
        assert!(plain.trim_end().ends_with("del /f /q \"%~f0\" >nul 2>&1"));
        assert!(!plain.contains("rmdir"));
    }

    /// 设备 ID 留在磁盘上,重装后会拿一个**已被服务端吊销**的身份去登录。
    /// 这条断言把两个一直漏掉的目录钉住。
    #[test]
    fn appdata_dirs_including_device_id_are_removed() {
        let sandbox = std::env::temp_dir().join(format!("recodex-appdata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sandbox);
        let local = sandbox.join("local");
        let roaming = sandbox.join("roaming");
        std::fs::create_dir_all(local.join("ReCodex")).unwrap();
        std::fs::create_dir_all(roaming.join("ReCodex").join("user_scripts")).unwrap();
        std::fs::write(local.join("ReCodex").join("device-id"), b"rcd_test").unwrap();
        std::fs::write(local.join("ReCodex").join("official-mode.json"), b"{}").unwrap();

        // SAFETY:改的是进程环境,本测试用完立刻恢复。
        let saved_local = std::env::var_os("LOCALAPPDATA");
        let saved_roaming = std::env::var_os("APPDATA");
        unsafe {
            std::env::set_var("LOCALAPPDATA", &local);
            std::env::set_var("APPDATA", &roaming);
        }

        let mut warnings = Vec::new();
        let removed = remove_appdata_dirs(&mut warnings);

        unsafe {
            match saved_local {
                Some(value) => std::env::set_var("LOCALAPPDATA", value),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
            match saved_roaming {
                Some(value) => std::env::set_var("APPDATA", value),
                None => std::env::remove_var("APPDATA"),
            }
        }

        assert_eq!(removed, 2, "两个目录都应被删除:{warnings:?}");
        assert!(!local.join("ReCodex").exists(), "设备 ID 目录应已删除");
        assert!(!roaming.join("ReCodex").exists(), "用户脚本目录应已删除");
        assert!(warnings.is_empty(), "不该有失败:{warnings:?}");

        let _ = std::fs::remove_dir_all(&sandbox);
    }

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
