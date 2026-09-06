use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::json;

// Fuse 猜想已坐实(2026-09-05,本机二进制实证):**Codex 152 起这条路永远不通**。
//
// `--inspect` 走的是 Electron 的 **Node** inspector(端口 = CDP 端口 + 100),
// 和 `--remote-debugging-port`(Chromium DevTools)是两套东西。Electron 打包时
// 可以烧 fuse 把 `--inspect` 整个禁掉,烧过之后参数被静默忽略,端口永不监听。
//
// 实测:本机 Codex 26.901.5003.0 的 fuse wire 在 `app\chrome.dll` 里(**不在主 exe**,
// 它是 OpenAI 定制构建,Electron 编进了 chrome.dll),sentinel 后 wire = `010011001`,
// 第 4 位 `EnableNodeCliInspectArguments = 0` = 禁用。对照组 Xmind 读出官方默认
// `10110001`,证明偏移没读错。
//
// 本机日志 58 次启动的分界线干净得吓人:Chrome/151 世代 52 次里成功 45 次;
// Chrome/152 世代 6 次**全败**,清一色 connrefused。同一次启动里 CDP 9229 完全正常,
// 只有 inspector 9329 连不上 —— 参数到了,Electron 自己没开。
//
// 所以「慢机器上 inspector 就绪得比 10 秒晚」这个旧判断是错的:151 上确实有启动
// 竞态(attempt 1/2/3 就成),152 上则是永不监听,等多久都一样。把窗口从 10 秒
// 拉到 120 秒对后者毫无用处,只是每次启动多空转 110 秒。**保持 10 秒**:够覆盖
// 151 的竞态,又不会在 152 上白等。
//
// 不下线功能:151 世代用户还能用,而这条路跑在 `tokio::spawn` 里不挡启动。
// 等 151 彻底没人用了再摘。
const MENU_LOCALIZATION_RETRIES: usize = 20;
const MENU_LOCALIZATION_RETRY_DELAY: Duration = Duration::from_millis(500);

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
    // 带上「跑满了多少次、一共等了多久」:光看错误信息分不清是等满了才放弃,还是
    // 中途因为别的原因提前退出。和 launcher.ensure_injection_exhausted 的
    // attempts 字段对称。
    //
    // 用 as_millis 而不是 as_secs:延迟是 500ms,`as_secs()` 截断成 0,
    // 再乘多少次都还是 0 —— 上线后所有上报都写着「共等 0 秒」,读的人会以为
    // 重试压根没退避,照着这条去查一个不存在的问题(实测被带偏过一次)。
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("native menu localization failed"))
        .context(format!(
            "放弃于第 {MENU_LOCALIZATION_RETRIES} 次尝试(共等 {:.1} 秒)",
            (MENU_LOCALIZATION_RETRY_DELAY.as_millis() * MENU_LOCALIZATION_RETRIES as u128) as f64
                / 1000.0
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
