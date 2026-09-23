//! recodex-overlay: 发布流水线用 —— 捆绑租约直连 sidecar 之前先验 minisign 签名。
//!
//! 用法：verify_minisign <文件> <签名文件>，公钥从环境变量 RECODEX_UPDATE_PUBLIC_KEY 读
//! （与命令行自更新、手机远程组件同一把）。验签通过退出 0，否则非 0。
//!
//! 为什么要它：流水线原来只对同一目录下的 SHA256SUMS。拿到 OSS 写权限的人能把二进制和
//! SHA256SUMS 一起换掉，而流水线会在 runner 上执行它、再把它捆进安装包、写进自更新清单 ——
//! 桌面端这条链路反而比命令行（自更新验签）更弱。签名私钥不在 OSS 上，换不了。
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: verify_minisign <file> <signature>");
        std::process::exit(2);
    }
    let key = std::env::var("RECODEX_UPDATE_PUBLIC_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("RECODEX_UPDATE_PUBLIC_KEY is not set");
        std::process::exit(2);
    }
    let message = match std::fs::read(&args[1]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("read file: {err}");
            std::process::exit(2);
        }
    };
    let signature = match std::fs::read(&args[2]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("read signature: {err}");
            std::process::exit(2);
        }
    };
    match codex_plus_core::phone_remote::manifest::verify_minisign(&message, &signature, &key) {
        Ok(()) => println!("signature ok"),
        Err(err) => {
            eprintln!("signature verification failed: {err}");
            std::process::exit(1);
        }
    }
}
