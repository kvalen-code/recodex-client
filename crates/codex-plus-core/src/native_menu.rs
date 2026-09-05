use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::json;

// ⚠️ 未证实的怀疑:这个功能可能**根本跑不通**,而不只是慢。
//
// `--inspect` 走的是 Electron 的 **Node** inspector,和 `--remote-debugging-port`
// (Chromium DevTools)是两套东西。两个参数在 `build_codex_arguments_with_native_menu_inspector`
// 里是**同一批**传给 Codex 的 —— 现场 CDP 通、inspector 不通,说明参数到达了,
// 是 Electron 自己没开。最可能是打包时烧了 Fuse `EnableNodeCliInspectArguments=false`
// (处理用户凭据的应用这么做很合理),那样等多久都没用。
//
// 吻合的旁证:6 台上报设备**全部**失败、终态清一色 `failed to query CDP targets`
// (连不上,不是执行失败)、Windows 和 macOS 都有 —— fuse 是打包烧录的,跨平台一致。
//
// 没有直接证据(本机没装 Codex,查不了它二进制里的 fuse wire),所以先不下线功能。
// **发版后看 `native_menu.localization_failed` 里新加的 `inspector_port_free`**:
// 端口空着=没人监听=坐实 fuse;端口被占=另一回事。坐实了就该把这个功能摘掉,
// 而不是继续每次启动空转两分钟。
//
// 等的和 `ensure_injection` 是**同一个 Codex 进程**开出来的两个端口
// (CDP 9229 / 菜单 inspector 9329),没道理一个等 120 秒、一个只等 10 秒。
//
// 线上实测(2026-09-05):20×500ms=10 秒的老配置下,6 台设备报重试失败、3 台跑到
// 终态彻底失败,其中 desktop-61ac49b6… 这台**桥完全正常、只有菜单汉化挂了** ——
// 说明不是 Codex 没起来,是慢机器上 inspector 就绪得比 10 秒晚。attempt 分布
// (1/2/3/5/18 都有)也印证:多数机器重试几次就成,少数跑满。
//
// 拉长不花钱:这条路跑在 `tokio::spawn` 的后台任务里,不挡启动;失败日志已经按
// 2 的幂次采样(120 次最多留 7 条)。
const MENU_LOCALIZATION_RETRIES: usize = 120;
const MENU_LOCALIZATION_RETRY_DELAY: Duration = Duration::from_secs(1);

const MENU_LABEL_TRANSLATIONS: &[(&str, &str)] = &[
    ("File", "文件"),
    ("Edit", "编辑"),
    ("View", "视图"),
    ("Window", "窗口"),
    ("Help", "帮助"),
    ("Undo", "撤销"),
    ("Redo", "重做"),
    ("Cut", "剪切"),
    ("Copy", "复制"),
    ("Paste", "粘贴"),
    ("Delete", "删除"),
    ("Select All", "全选"),
    ("Copy conversation path", "复制对话路径"),
    ("Copy deeplink", "复制深度链接"),
    ("Copy session id", "复制会话 ID"),
    ("Copy working directory", "复制工作目录"),
    ("Close Tab", "关闭标签页"),
    ("Close", "关闭"),
    ("Reload Browser Page", "重新加载浏览器页面"),
    ("Force Reload Browser Page", "强制重新加载浏览器页面"),
    ("New Window", "新建窗口"),
    ("Open command menu", "打开命令菜单"),
    ("Search Chats…", "搜索对话..."),
    ("Search Files…", "搜索文件..."),
    ("Rename chat", "重命名对话"),
    ("Toggle File Tree", "切换文件树"),
    ("Start Trace Recording", "开始跟踪录制"),
    ("New Chat", "新建对话"),
    ("Quick Chat", "快速对话"),
    ("Open in New Window", "在新窗口中打开"),
    ("Archive chat", "归档对话"),
    ("Pin/unpin chat", "固定/取消固定对话"),
    ("Dictation", "听写"),
    ("Wake Pet", "唤醒助手"),
    ("Previous Chat", "上一个对话"),
    ("Next Chat", "下一个对话"),
    ("Settings…", "设置..."),
    ("Keyboard Shortcuts", "键盘快捷键"),
    ("Process Manager", "进程管理器"),
    ("Open Folder…", "打开文件夹..."),
    ("Toggle Sidebar", "切换边栏"),
    ("Toggle Bottom Panel", "切换底部面板"),
    ("Toggle Pinned Summary", "切换固定摘要"),
    ("Open Terminal", "打开终端"),
    ("Open Browser Tab", "打开浏览器标签页"),
    ("Toggle Browser Panel", "切换浏览器面板"),
    ("Toggle Side Panel", "切换侧边面板"),
    ("Find", "查找"),
    ("Focus Browser Address Bar", "聚焦浏览器地址栏"),
    ("Back", "后退"),
    ("Forward", "前进"),
    ("Go to Chat 1", "转到对话 1"),
    ("Go to Chat 2", "转到对话 2"),
    ("Go to Chat 3", "转到对话 3"),
    ("Go to Chat 4", "转到对话 4"),
    ("Go to Chat 5", "转到对话 5"),
    ("Go to Chat 6", "转到对话 6"),
    ("Go to Chat 7", "转到对话 7"),
    ("Go to Chat 8", "转到对话 8"),
    ("Go to Chat 9", "转到对话 9"),
    ("Log Out", "退出登录"),
    ("Reload Window", "重新加载窗口"),
    ("Zoom In", "放大"),
    ("Zoom Out", "缩小"),
    ("Actual Size", "实际大小"),
    ("Toggle Full Screen", "切换全屏"),
    ("Codex Documentation", "Codex 文档"),
    ("What's new", "更新内容"),
    ("Automations", "自动化"),
    ("Local Environments", "本地环境"),
    ("Worktrees", "工作树"),
    ("Skills", "技能"),
    ("Model Context Protocol", "模型上下文协议"),
    ("Troubleshooting", "故障排查"),
    ("Send Feedback", "发送反馈"),
    ("Check for Updates…", "检查更新..."),
    ("Updates Unavailable", "更新不可用"),
    ("Toggle Debug Menu", "切换调试菜单"),
    ("Open Deeplink from Clipboard", "从剪贴板打开深度链接"),
    ("Toggle Query Devtools", "切换查询 DevTools"),
    ("Toggle React Scan", "切换 React Scan"),
];

pub async fn install_native_menu_localizer(inspector_port: u16) -> anyhow::Result<()> {
    let mut last_error = None;
    for attempt in 1..=MENU_LOCALIZATION_RETRIES {
        match try_install_native_menu_localizer(inspector_port).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                // 同 ensure_injection:CDP 不可达时 20 次全会失败且同因,
                // 线上见过 attempt 18。采样留第 1、2、4、8、16 次。
                if crate::diagnostic_log::should_log_retry_attempt(attempt as u32) {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "native_menu.localization_retry_failed",
                        json!({
                            "inspector_port": inspector_port,
                            "attempt": attempt,
                            "message": last_error.as_ref().map(ToString::to_string).unwrap_or_default()
                        }),
                    );
                }
                tokio::time::sleep(MENU_LOCALIZATION_RETRY_DELAY).await;
            }
        }
    }
    // 带上「跑满了多少次」:光看错误信息分不清是等满了才放弃,还是中途因为别的
    // 原因提前退出。和 launcher.ensure_injection_exhausted 的 attempts 字段对称。
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("native menu localization failed"))
        .context(format!(
            "放弃于第 {MENU_LOCALIZATION_RETRIES} 次尝试(共等 {} 秒)",
            MENU_LOCALIZATION_RETRIES as u64 * MENU_LOCALIZATION_RETRY_DELAY.as_secs()
        )))
}

pub fn native_menu_localizer_script() -> anyhow::Result<String> {
    let translations =
        serde_json::to_string(&MENU_LABEL_TRANSLATIONS.iter().copied().collect::<Vec<_>>())?;
    Ok(format!(
        r#"
(() => {{
  const translations = new Map({translations});
  const electron = process.mainModule?.require?.("electron");
  if (!electron?.Menu) return JSON.stringify({{ status: "skipped", reason: "electron-menu-unavailable" }});
  const Menu = electron.Menu;
  let changed = 0;
  const translateItem = (item) => {{
    if (!item) return;
    const nextLabel = translations.get(item.label);
    if (nextLabel && item.label !== nextLabel) {{
      item.label = nextLabel;
      changed += 1;
    }}
    if (item.submenu?.items) {{
      for (const child of item.submenu.items) translateItem(child);
    }}
  }};
  const translateMenu = (menu) => {{
    if (!menu?.items) return menu;
    for (const item of menu.items) translateItem(item);
    return menu;
  }};
  if (!globalThis.__codexPlusNativeMenuLocalizerInstalled) {{
    globalThis.__codexPlusNativeMenuLocalizerInstalled = true;
    const originalSetApplicationMenu = Menu.setApplicationMenu.bind(Menu);
    Menu.setApplicationMenu = (menu) => {{
      try {{ translateMenu(menu); }} catch {{}}
      return originalSetApplicationMenu(menu);
    }};
  }}
  const menu = Menu.getApplicationMenu();
  if (menu) {{
    translateMenu(menu);
    Menu.setApplicationMenu(menu);
  }}
  return JSON.stringify({{
    status: "ok",
    changed,
    topLabels: menu?.items?.map((item) => item.label) ?? []
  }});
}})()
"#
    ))
}

async fn try_install_native_menu_localizer(inspector_port: u16) -> anyhow::Result<()> {
    let targets = crate::cdp::list_targets(inspector_port).await?;
    let target = targets
        .iter()
        .find(|target| {
            target
                .web_socket_debugger_url
                .as_deref()
                .is_some_and(|url| !url.is_empty())
                && target.target_type == "node"
        })
        .or_else(|| {
            targets.iter().find(|target| {
                target
                    .web_socket_debugger_url
                    .as_deref()
                    .is_some_and(|url| !url.is_empty())
            })
        })
        .context("No Electron main-process inspector target found")?;
    let websocket_url = target
        .web_socket_debugger_url
        .as_deref()
        .context("selected inspector target has no websocket URL")?;
    let script = native_menu_localizer_script()?;
    let result = crate::bridge::evaluate_script_with_await_promise(websocket_url, &script, true)
        .await
        .context("failed to evaluate native menu localizer")?;
    if let Some(exception) = result
        .get("result")
        .and_then(|value| value.get("exceptionDetails"))
    {
        bail!("native menu localizer threw: {exception}");
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "native_menu.localization_installed",
        json!({
            "inspector_port": inspector_port,
            "target_type": target.target_type,
            "target_title": target.title,
            "result": result
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_menu_localizer_script_uses_runtime_menu_patch() {
        let script = native_menu_localizer_script().unwrap();

        assert!(script.contains("Menu.setApplicationMenu"));
        assert!(script.contains("Toggle Sidebar"));
        assert!(script.contains("切换边栏"));
        assert!(!script.contains("app.asar"));
    }
}
