//! 第二个 launcher 的「把已经在跑的 Codex 叫到前台」。
//!
//! 走到这里的前提:单实例锁被别的 launcher(主实例)持有。主实例拥有 Codex 的整个
//! 生命周期 —— 拉起、注入、看门狗、退出清理。第二个 launcher 只是用户多点了一下
//! 图标,它能做的**只有**激活窗口。
//!
//! 以前这里复用主实例的 `launch_codex`,而那里有一段「Codex 在跑但 CDP 探不通 →
//! 先杀掉再带调试端口拉起」。第二实例探 CDP 的条件远不可靠:主实例刚拉起 Codex、
//! CDP 还没监听;Codex 忙到 /json 超过 300ms;页面刷新中没有主页面 target;
//! latest-status.json 被写坏后退回了请求端口 —— 任何一种都会把用户正在用的 Codex
//! 杀掉。所以这条路径**不探 CDP、不杀任何进程、不拉起新的 Codex**:
//!   - 有 Codex 进程 → 前置它的窗口;窗口一直找不到(还在建/最小化到托盘)才退到
//!     系统激活(AUMID / `open`),此时 Electron 单实例锁会把激活转交给老进程;
//!   - 没有 Codex 进程 → 主实例多半正在拉起(商店激活带退避重试,最长约 20 秒),
//!     等它;等不到也只报告,绝不自己去拉 —— 自己拉的那个没有调试端口,反过来会被
//!     主实例当成「无 CDP 的 Codex」杀掉。
//!
//! 等待期间**每一轮都试着接手单实例锁**:用户刚关掉 Codex 又马上点图标时,主实例
//! 还要 4~6 秒才确认 Codex 退出并放锁(wait_for_codex_exit 每 2 秒轮询、连续三次
//! 查不到才算退出)。不重试的话,这几秒里点开的第二实例会空等到超时,最后什么都没
//! 打开。拿到锁就转成主实例,走正常启动。

use std::path::Path;
use std::time::Duration;

/// 第二实例激活路径能碰到的全部外部副作用。故意**没有**任何结束进程/拉起进程的方法:
/// 这条路径不需要,也就不该有入口。
pub trait ExistingInstanceEnv: Send + Sync {
    fn codex_process_ids(&self) -> Vec<u32>;
    fn activate_process_window(&self, process_id: u32) -> bool;
    /// 这个进程名下还有没有窗口(隐藏到托盘的算)。「有进程、没有任何窗口」= Codex
    /// 正在退出的残留,此时系统激活会**新起**一个不带调试端口的 Codex。
    fn process_has_window(&self, process_id: u32) -> bool;
    /// 试着接手单实例锁(不等待)。拿到了就由调用方转为主实例。
    fn try_take_over(&self) -> bool;
    /// 系统级激活(Windows 商店版走 AUMID、不带任何参数;macOS 走 `open`)。
    /// 只在已有 Codex 进程时调用。
    fn activate_app(&self, app_dir: &Path) -> anyhow::Result<()>;
    fn sleep(&self, duration: Duration);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingActivationPolicy {
    pub poll_interval: Duration,
    /// 一个 Codex 进程都没有时最多等多久(覆盖主实例商店激活的退避重试)。
    pub wait_for_process: Duration,
    /// 有进程但窗口一直前置不了,等多久再退到系统激活。
    pub window_grace: Duration,
}

impl Default for ExistingActivationPolicy {
    fn default() -> Self {
        let retry_total_ms: u64 = crate::launcher::PACKAGED_ACTIVATION_RETRY_DELAYS_MS
            .iter()
            .sum();
        Self {
            poll_interval: Duration::from_millis(500),
            // 主实例的激活重试总长 + 余量(托管配置同步、进程枚举等)。
            wait_for_process: Duration::from_millis(retry_total_ms + 5_000),
            // 只有 Windows 能按进程前置窗口;别的平台直接走系统激活,不必空等。
            window_grace: if cfg!(windows) {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingActivation {
    /// 找到窗口并前置了。
    WindowActivated { process_ids: Vec<u32> },
    /// 窗口前置不了,交给系统激活(转交给已在跑的实例)。
    AppActivated { process_ids: Vec<u32> },
    AppActivationFailed { process_ids: Vec<u32>, error: String },
    /// 等满 `wait_for_process` 也没有 Codex 进程。什么都没做。
    NoCodexProcess,
    /// 等待期间主实例放开了单实例锁(通常是它刚确认 Codex 退出),本进程接手当主实例。
    BecamePrimary,
}

impl ExistingActivation {
    pub fn label(&self) -> &'static str {
        match self {
            Self::WindowActivated { .. } => "window_activated",
            Self::AppActivated { .. } => "app_activated",
            Self::AppActivationFailed { .. } => "app_activation_failed",
            Self::NoCodexProcess => "no_codex_process",
            Self::BecamePrimary => "became_primary",
        }
    }
}

/// 同步执行(内部会 sleep),调用方放进阻塞线程。
pub fn activate_existing_codex(
    env: &dyn ExistingInstanceEnv,
    app_dir: &Path,
    policy: &ExistingActivationPolicy,
) -> ExistingActivation {
    let interval = policy.poll_interval.max(Duration::from_millis(1));
    let polls_for = |total: Duration| (total.as_millis() / interval.as_millis()) as u64;
    let max_process_polls = polls_for(policy.wait_for_process);
    let max_window_polls = polls_for(policy.window_grace);
    let mut process_polls = 0u64;
    let mut window_polls = 0u64;
    loop {
        let process_ids = env.codex_process_ids();
        if !process_ids.is_empty() {
            if process_ids
                .iter()
                .any(|process_id| env.activate_process_window(*process_id))
            {
                return ExistingActivation::WindowActivated { process_ids };
            }
            // 只有「进程名下真有窗口」才值得等、才可以做系统激活。只剩没有窗口的
            // 残留进程时,Electron 单实例锁多半已经放开,激活会新起一个没有调试
            // 端口的 Codex —— 按「没有进程」处理:接着等,并试着接手锁。
            if process_ids
                .iter()
                .any(|process_id| env.process_has_window(*process_id))
            {
                if window_polls < max_window_polls {
                    window_polls += 1;
                    env.sleep(interval);
                    continue;
                }
                // 宽限期到了:激活前再查一次,窗口不能是刚刚消失的。
                let current = env.codex_process_ids();
                if current
                    .iter()
                    .any(|process_id| env.process_has_window(*process_id))
                {
                    return match env.activate_app(app_dir) {
                        Ok(()) => ExistingActivation::AppActivated {
                            process_ids: current,
                        },
                        Err(error) => ExistingActivation::AppActivationFailed {
                            process_ids: current,
                            error: format!("{error:#}"),
                        },
                    };
                }
            }
        }
        // 没有进程,或只剩没有窗口的残留进程。
        if env.try_take_over() {
            return ExistingActivation::BecamePrimary;
        }
        if process_polls >= max_process_polls {
            return ExistingActivation::NoCodexProcess;
        }
        process_polls += 1;
        env.sleep(interval);
    }
}

/// 真实系统上的实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemExistingInstanceEnv;

impl ExistingInstanceEnv for SystemExistingInstanceEnv {
    fn codex_process_ids(&self) -> Vec<u32> {
        crate::watcher::find_codex_processes()
    }

    fn activate_process_window(&self, process_id: u32) -> bool {
        #[cfg(windows)]
        {
            crate::windows_activate_process_window(process_id)
        }
        #[cfg(not(windows))]
        {
            let _ = process_id;
            false
        }
    }

    fn process_has_window(&self, process_id: u32) -> bool {
        #[cfg(windows)]
        {
            crate::windows_process_has_window(process_id)
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 上没有按进程查窗口的办法;macOS 的 `open` 交给 LaunchServices,
            // 有进程就当它有窗口。
            let _ = process_id;
            true
        }
    }

    /// 接手要由**持有 guard 的那一层**做(guard 必须活到进程结束),所以系统实现
    /// 在这里永远返回 false;launcher 用自己的实现覆盖它。
    fn try_take_over(&self) -> bool {
        false
    }

    fn activate_app(&self, app_dir: &Path) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            let aumid = crate::app_paths::packaged_app_user_model_id(app_dir).ok_or_else(|| {
                anyhow::anyhow!("not a packaged Codex install; window activation only")
            })?;
            // 不带任何参数:已有实例会收到 second-instance 并前置自己。
            crate::launcher::activate_packaged_app_sync(&aumid, "").map(|_| ())
        }
        #[cfg(target_os = "macos")]
        {
            let status = std::process::Command::new("open").arg(app_dir).status()?;
            anyhow::ensure!(status.success(), "open exited with {status}");
            Ok(())
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = app_dir;
            anyhow::bail!("app activation is not supported on this platform")
        }
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 按轮次返回进程列表;记录所有调用。
    struct FakeEnv {
        process_rounds: Mutex<Vec<Vec<u32>>>,
        window_ok_after: Mutex<Option<usize>>,
        /// 名下有窗口的进程;空集合 = 只剩正在退出的残留进程。
        windowed: Mutex<Vec<u32>>,
        /// 第几轮起能拿到单实例锁(None = 一直拿不到)。
        take_over_after: Mutex<Option<usize>>,
        app_result: Result<(), String>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeEnv {
        fn new(process_rounds: Vec<Vec<u32>>, window_ok_after: Option<usize>) -> Self {
            let windowed = process_rounds.iter().flatten().copied().collect();
            Self {
                process_rounds: Mutex::new(process_rounds),
                window_ok_after: Mutex::new(window_ok_after),
                windowed: Mutex::new(windowed),
                take_over_after: Mutex::new(None),
                app_result: Ok(()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn without_windows(self) -> Self {
            *self.windowed.lock().unwrap() = Vec::new();
            self
        }
        fn taking_over_after(self, rounds: usize) -> Self {
            *self.take_over_after.lock().unwrap() = Some(rounds);
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ExistingInstanceEnv for FakeEnv {
        fn codex_process_ids(&self) -> Vec<u32> {
            self.calls.lock().unwrap().push("processes".into());
            let mut rounds = self.process_rounds.lock().unwrap();
            if rounds.len() > 1 {
                rounds.remove(0)
            } else {
                rounds.first().cloned().unwrap_or_default()
            }
        }
        fn activate_process_window(&self, process_id: u32) -> bool {
            self.calls.lock().unwrap().push(format!("window:{process_id}"));
            let mut remaining = self.window_ok_after.lock().unwrap();
            match remaining.as_mut() {
                Some(0) => true,
                Some(n) => {
                    *n -= 1;
                    false
                }
                None => false,
            }
        }
        fn process_has_window(&self, process_id: u32) -> bool {
            self.calls.lock().unwrap().push(format!("has_window:{process_id}"));
            self.windowed.lock().unwrap().contains(&process_id)
        }
        fn try_take_over(&self) -> bool {
            self.calls.lock().unwrap().push("take_over".into());
            let mut remaining = self.take_over_after.lock().unwrap();
            match remaining.as_mut() {
                Some(0) => true,
                Some(n) => {
                    *n -= 1;
                    false
                }
                None => false,
            }
        }
        fn activate_app(&self, _app_dir: &Path) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("app".into());
            self.app_result.clone().map_err(anyhow::Error::msg)
        }
        fn sleep(&self, _duration: Duration) {
            self.calls.lock().unwrap().push("sleep".into());
        }
    }

    fn policy() -> ExistingActivationPolicy {
        ExistingActivationPolicy {
            poll_interval: Duration::from_millis(100),
            wait_for_process: Duration::from_millis(300),
            window_grace: Duration::from_millis(200),
        }
    }

    #[test]
    fn running_codex_window_is_brought_forward_without_system_activation() {
        let env = FakeEnv::new(vec![vec![42]], Some(0));
        let outcome = activate_existing_codex(&env, Path::new("C:/codex/app"), &policy());
        assert_eq!(
            outcome,
            ExistingActivation::WindowActivated {
                process_ids: vec![42]
            }
        );
        assert_eq!(env.calls(), vec!["processes", "window:42"]);
    }

    #[test]
    fn waits_for_the_primary_to_bring_codex_up_instead_of_launching_one() {
        // 前两轮没有进程(主实例的激活重试还在进行),第三轮出现。
        let env = FakeEnv::new(vec![vec![], vec![], vec![7]], Some(0));
        let outcome = activate_existing_codex(&env, Path::new("C:/codex/app"), &policy());
        assert_eq!(
            outcome,
            ExistingActivation::WindowActivated {
                process_ids: vec![7]
            }
        );
        assert!(!env.calls().contains(&"app".to_string()));
    }

    #[test]
    fn gives_up_quietly_when_no_codex_ever_appears() {
        let env = FakeEnv::new(vec![vec![]], None);
        let outcome = activate_existing_codex(&env, Path::new("C:/codex/app"), &policy());
        assert_eq!(outcome, ExistingActivation::NoCodexProcess);
        // 没有进程时绝不做系统激活:那会拉起一个不带调试端口的 Codex。
        assert!(env
            .calls()
            .iter()
            .all(|call| call == "processes" || call == "sleep" || call == "take_over"));
    }

    /// 应修 2:关掉 Codex 后马上再点 —— 主实例还要几秒才放锁,等待期间每轮都试着
    /// 接手;拿到锁就转成主实例,而不是空等到超时再报错。
    #[test]
    fn takes_over_the_instance_lock_once_the_primary_releases_it() {
        let env = FakeEnv::new(vec![vec![]], None).taking_over_after(2);
        let outcome = activate_existing_codex(&env, Path::new("C:/codex/app"), &policy());
        assert_eq!(outcome, ExistingActivation::BecamePrimary);
        let calls = env.calls();
        assert_eq!(calls.iter().filter(|call| *call == "take_over").count(), 3);
        assert!(!calls.contains(&"app".to_string()));
    }

    /// 建议 A:只剩正在退出、名下没有窗口的残留进程时,不做系统激活(会新起一个
    /// 不带调试端口的 Codex),按「没有进程」处理并接手锁。
    #[test]
    fn windowless_dying_processes_are_treated_as_no_codex() {
        let env = FakeEnv::new(vec![vec![4242]], None)
            .without_windows()
            .taking_over_after(1);
        let outcome = activate_existing_codex(&env, Path::new("C:/codex/app"), &policy());
        assert_eq!(outcome, ExistingActivation::BecamePrimary);
        assert!(!env.calls().contains(&"app".to_string()));
    }

    /// 宽限期到点那一刻窗口刚消失:不激活,退回等待/接手。
    #[test]
    fn window_disappearing_at_the_grace_deadline_cancels_system_activation() {
        let env = FakeEnv::new(vec![vec![9]], None);
        // 前两轮有窗口(消耗宽限期),之后窗口没了。
        {
            let mut windowed = env.windowed.lock().unwrap();
            *windowed = vec![9];
        }
        let outcome = {
            let policy = ExistingActivationPolicy {
                poll_interval: Duration::from_millis(100),
                wait_for_process: Duration::from_millis(100),
                window_grace: Duration::from_millis(100),
            };
            // 宽限期到点前把窗口拿掉:模拟 Codex 正好退完。
            struct Vanishing<'a>(&'a FakeEnv);
            impl ExistingInstanceEnv for Vanishing<'_> {
                fn codex_process_ids(&self) -> Vec<u32> {
                    self.0.codex_process_ids()
                }
                fn activate_process_window(&self, process_id: u32) -> bool {
                    let activated = self.0.activate_process_window(process_id);
                    // 第一次尝试之后窗口消失。
                    *self.0.windowed.lock().unwrap() = Vec::new();
                    activated
                }
                fn process_has_window(&self, process_id: u32) -> bool {
                    self.0.process_has_window(process_id)
                }
                fn try_take_over(&self) -> bool {
                    self.0.try_take_over()
                }
                fn activate_app(&self, app_dir: &Path) -> anyhow::Result<()> {
                    self.0.activate_app(app_dir)
                }
                fn sleep(&self, duration: Duration) {
                    self.0.sleep(duration)
                }
            }
            activate_existing_codex(&Vanishing(&env), Path::new("C:/codex/app"), &policy)
        };
        assert_eq!(outcome, ExistingActivation::NoCodexProcess);
        assert!(!env.calls().contains(&"app".to_string()));
    }

    #[test]
    fn falls_back_to_system_activation_only_after_the_window_grace() {
        let env = FakeEnv::new(vec![vec![9]], None);
        let outcome = activate_existing_codex(&env, Path::new("C:/codex/app"), &policy());
        assert_eq!(
            outcome,
            ExistingActivation::AppActivated {
                process_ids: vec![9]
            }
        );
        let calls = env.calls();
        assert_eq!(calls.iter().filter(|call| *call == "app").count(), 1);
        assert_eq!(calls.last().map(String::as_str), Some("app"));
        assert!(calls.iter().filter(|call| *call == "window:9").count() >= 2);
    }
}
