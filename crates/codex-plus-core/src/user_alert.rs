//! 把「增强功能已经挂了」这件事真正推到用户眼前。
//!
//! 线上教训:付费客户的桥断了 9 小时,期间只有本地诊断日志和服务端遥测有记录,
//! 客户端本身一声不吭 —— 客户以为一切正常,我们靠后台报表才发现。状态文件
//! (`running_degraded`)只有我们自己会读,对用户等于不存在。
//!
//! 平台差异是有意的:Windows 用 `MessageBoxW`(`Win32_UI_WindowsAndMessaging`
//! 已在依赖里,零新依赖),macOS 用 `osascript` 通知中心(仓库里已经在用
//! osascript,同样零新依赖)。默认不阻塞调用方;只有「提示完就退出」的致命路径
//! 走 `alert_once_blocking`,否则 Windows 上的弹窗会随进程一起消失。

use std::sync::atomic::{AtomicBool, Ordering};

/// 全进程只提醒一次。桥挂掉时看门狗每隔几十秒就会再失败一次,反复弹窗比不弹更糟。
static ALERTED: AtomicBool = AtomicBool::new(false);

/// **默认关闭**,只有真正的启动器进程调 [`enable`] 之后才会弹窗。
///
/// 为什么必须默认关:集成测试链接的是 dev 编译的这个 lib,`#[cfg(test)]` 对它
/// 是 **false**,挡不住任何东西。而测试里本来就有故意触发错误路径的用例
/// (`launch_lifecycle_cleans_helper_and_codex_when_status_save_fails` 就是把
/// status 目录做成文件来逼出失败)—— 于是每跑一次测试就往开发者桌面上弹一个框,
/// 阻塞版还会一直等人点确定,把测试进程拖成「挂死」。2026-09-05 真的这么弹了一下午。
///
/// 「默认安全 + 主程序显式开启」而不是「默认开 + 测试里记得关」:后者靠每个测试
/// 作者自觉,这个仓库已经在 `RECODEX_ENV_SANDBOX` 上吃过一次那种亏。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 允许本进程弹窗。只该由启动器的 `main` 调用一次。
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// 当前进程是否允许弹窗。
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// 本进程内第一次调用时把提示推给用户;之后的调用直接返回 `false`。
///
/// 不阻塞:Windows 起一条独立线程等用户点确定,macOS 直接 spawn 一个短命子进程。
///
/// **选它还是选 `alert_once_blocking`,判据只有一条:提示之后这个进程还活不活着。**
/// 不是「错误严重不严重」—— 严重程度和弹窗能不能被看见没有关系。进程马上要退出
/// 的路径(启动失败、激活已有实例后直接 return)必须用阻塞版,否则 Windows 上
/// 弹窗线程会随进程一起被掐掉,对话框一闪而过等于没提示。
pub fn alert_once(title: &str, body: &str) -> bool {
    alert_once_inner(title, body, false)
}

/// 同 `alert_once`,但**等用户点掉**才返回。
///
/// 专给「提示完这个进程就没了」的路径用:进程一退,Windows 上那条弹窗线程会被直接
/// 掐掉,对话框一闪而过 —— 等于没提示。启动彻底失败恰恰是最需要用户看见的那次,
/// 所以这里宁可挡住即将结束的启动流程,也要把话说完。
pub fn alert_once_blocking(title: &str, body: &str) -> bool {
    alert_once_inner(title, body, true)
}

fn alert_once_inner(title: &str, body: &str, wait: bool) -> bool {
    if !is_enabled() {
        // 没开就只留一条日志。诊断仍然拿得到「这里本该提示用户」这个事实。
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "launcher.user_alert_suppressed",
            serde_json::json!({ "title": title }),
        );
        return false;
    }
    if !claim_alert_slot() {
        return false;
    }
    // 只记标题,不记正文:正文里嵌的是 error.to_string(),而那串已经由
    // launcher.failed / running_degraded 传过一遍了。再传一次既是重复载荷,
    // 又把错误信息里的路径(含用户名)多送出去一份。标题已经足够区分是哪种提示。
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "launcher.user_alert",
        serde_json::json!({ "title": title, "blocking": wait }),
    );
    show_alert(title, body, wait);
    true
}

/// 抢占「本进程唯一一次提醒」的名额,抢到返回 `true`。
///
/// 单独抽出来是为了能测:直接测 `alert_once` 会在 Windows 上真弹一个对话框,
/// 跑 `cargo test` 的人得手动点掉。
fn claim_alert_slot() -> bool {
    !ALERTED.swap(true, Ordering::SeqCst)
}

/// 构造 macOS 通知中心的 osascript 命令。
///
/// 单独抽出来是为了能测转义:标题/正文里出现双引号或反斜杠会把 AppleScript
/// 字符串截断,拼出一条语法错误的脚本 —— 那样通知就静默丢了。
///
/// 不用 `pub`:测试就在本模块内,外面没有第二个消费者。
fn build_macos_alert_command(title: &str, body: &str) -> Vec<String> {
    vec![
        "osascript".to_string(),
        "-e".to_string(),
        format!(
            r#"display notification "{}" with title "{}""#,
            escape_applescript(body),
            escape_applescript(title)
        ),
    ]
}

/// AppleScript 的字符串字面量**不能跨行**:正文里有一个真实换行,拼出来的脚本
/// 就是语法错误,osascript 报错退出,通知静默消失 —— 和 plist 没转义 `&` 是同一
/// 类病:没有任何报错,只是「提示没出现」。而我们的提示文案恰恰都是多行的。
///
/// 顺序不能换:必须先把反斜杠翻倍,再处理引号和换行,否则会把刚生成的转义序列
/// 再转义一遍。
fn escape_applescript(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

#[cfg(windows)]
fn show_alert(title: &str, body: &str, wait: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    use windows::core::HSTRING;

    let title = HSTRING::from(title);
    let body = HSTRING::from(body);
    // 置顶 + 抢焦点只给「用户此刻正在等结果」的那一次。
    //
    // 判据和 wait 是同一个:wait=true 的路径是用户刚双击了图标、进程马上要退出,
    // 他就盯着屏幕等启动 —— 这时候不抢到最前面等于没提示。
    //
    // wait=false 的路径(注入降级、看门狗发现桥断)则相反:Codex 已经开着,
    // 用户多半正在别的窗口里干活。MB_TOPMOST 会把框直接盖到他手上的事情上面,
    // 而「增强功能挂了」并不紧急到值得打断 —— 功能消失他自己看得见。
    // 去掉这两个标志之后,Windows 会把非前台进程的弹窗降级成任务栏闪烁:
    // 提示还在,只是等用户自己回头看。
    let flags = if wait {
        MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST
    } else {
        MB_OK | MB_ICONWARNING
    };
    let show = move || unsafe {
        MessageBoxW(None, &body, &title, flags);
    };
    if wait {
        // 调用方马上要退出了,挡住它反而是对的 —— 不等的话对话框会随进程一起消失。
        show();
        return;
    }
    // 独立线程:MessageBoxW 会一直等到用户点确定,绝不能挡住启动流程或看门狗。
    std::thread::spawn(show);
}

/// macOS 不需要区分 `wait`:`osascript` 是**独立子进程**,我们退出之后它照样把
/// 通知送到通知中心,不像 Windows 的弹窗线程会跟着进程一起没。
#[cfg(target_os = "macos")]
fn show_alert(title: &str, body: &str, _wait: bool) {
    let command = build_macos_alert_command(title, body);
    let Some((program, args)) = command.split_first() else {
        return;
    };
    // spawn 不 wait:osascript 自己会退出,回收交给系统(启动器是短命进程,
    // 不值得为一条通知多养一个 waiter 任务)。
    let _ = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(any(windows, target_os = "macos")))]
fn show_alert(_title: &str, _body: &str, _wait: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_alert_command_escapes_quotes_and_backslashes() {
        let command = build_macos_alert_command("Re\"Codex", "path C:\\a \"b\"");
        assert_eq!(command[0], "osascript");
        assert_eq!(command[1], "-e");
        assert_eq!(
            command[2],
            r#"display notification "path C:\\a \"b\"" with title "Re\"Codex""#
        );
    }

    /// 我们的提示文案全是多行的。AppleScript 字符串不能跨行 —— 不转义换行,
    /// 拼出来的就是语法错误的脚本,osascript 报错退出、通知一声不响地没了。
    #[test]
    fn macos_alert_command_escapes_newlines_so_the_script_stays_one_line() {
        let command = build_macos_alert_command("ReCodex 增强功能未启动", "第一行\n第二行\r\n第三行");
        let script = &command[2];

        assert!(
            !script.contains('\n') && !script.contains('\r'),
            "脚本里还有真实换行,osascript 会当成语法错误:\n{script}"
        );
        assert!(script.contains("第一行\\n第二行\\r\\n第三行"), "{script}");
    }

    /// 反斜杠必须先翻倍,否则会把随后生成的 `\n` 再转义一遍,变成字面量 `\n`。
    #[test]
    fn macos_alert_escaping_does_not_double_escape_itself() {
        let command = build_macos_alert_command("t", "C:\\dir\n下一行");
        assert!(command[2].contains("C:\\\\dir\\n下一行"), "{}", command[2]);
    }

    #[test]
    fn alert_slot_is_claimable_exactly_once() {
        // 静态 latch 是全进程的,这个用例独占它 —— 本文件不要再写第二个抢名额的用例。
        assert!(claim_alert_slot());
        assert!(!claim_alert_slot());
        assert!(!claim_alert_slot());
    }
}
