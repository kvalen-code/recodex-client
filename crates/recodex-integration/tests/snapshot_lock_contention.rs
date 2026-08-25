//! 面板轮询不该因为「有人正在刷新」而报错。
//!
//! 快照锁本身是必要的(snapshot 末尾要合并缓存,并发合并会搅乱状态)。
//! 但从前轮询和刷新共用同一个 try_lock:用户点一次刷新要打上游、可能几秒,
//! 这期间面板每隔几秒的轮询全部撞锁,于是屏幕上刷出一串
//! 「A ReCodex status refresh is already in progress」——
//! 一次无害的重叠被变成了可见故障,而面板上本来就有数据可以继续显示。
//!
//! 现在:轮询等一会儿(有上限,不会挂死),刷新仍然立即返回「正在刷新」。

use std::sync::{Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

/// 复刻被测的取锁策略:refresh 立即失败,轮询有上限地等。
fn acquire(lock: &Mutex<()>, refresh: bool) -> Option<std::sync::MutexGuard<'_, ()>> {
    if refresh {
        return lock.try_lock().ok();
    }
    for _ in 0..20 {
        match lock.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(100)),
            Err(TryLockError::Poisoned(_)) => return None,
        }
    }
    lock.try_lock().ok()
}

#[test]
fn polling_waits_out_a_short_refresh_instead_of_failing() {
    let lock = Mutex::new(());
    let started_holding = Mutex::new(false);
    thread::scope(|scope| {
        // 模拟一次 300ms 的刷新占着锁(MutexGuard 不是 Send,只能在线程内部加锁)
        scope.spawn(|| {
            let _held = lock.lock().unwrap();
            *started_holding.lock().unwrap() = true;
            thread::sleep(Duration::from_millis(300));
        });
        // 等它真的拿到锁再开始断言
        while !*started_holding.lock().unwrap() {
            thread::sleep(Duration::from_millis(10));
        }

        // 刷新期间再点刷新:如实告诉用户「正在刷新」,这是对的
        assert!(
            acquire(&lock, true).is_none(),
            "刷新期间再点刷新,应当立即返回「正在刷新」"
        );

        // 但普通轮询必须等出来,而不是报错
        let started = Instant::now();
        assert!(
            acquire(&lock, false).is_some(),
            "轮询不该因为别人在刷新就失败 —— 面板上会刷出一串可见错误"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "等待必须有上限,不能挂死"
        );
    });
}
