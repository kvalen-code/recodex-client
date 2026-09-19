fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        // recodex-overlay: 用上游原图标(ChatGPT 风格),名称仍是 ReCodex。
        // 不要指向 manager 的 icons/icon.ico —— 那个会被 overlay 替换成 Rx 图标。
        resource.set_icon("../../assets/images/recodex.ico");
        // 版本信息里的名字用户看得见:任务管理器「名称」列、属性 → 详细信息、
        // 防火墙 / 杀软弹窗都显示 FileDescription。winresource 默认取 cargo 包名,
        // 于是全都写着 codex-plus-launcher。
        //
        // FileVersion / ProductVersion 不在这里设:保留 winresource 的默认值
        // (= Cargo.toml 的 version),发布 CI 靠它校验「exe 自报版本 == 发布版本」。
        resource.set("ProductName", "ReCodex");
        resource.set("FileDescription", "ReCodex");
        resource.set("InternalName", "recodex");
        resource.set("OriginalFilename", "recodex.exe");
        resource.set("LegalCopyright", "AGPL-3.0-only");
        resource.set_manifest(include_str!(
            "../codex-plus-manager/src-tauri/windows-app-manifest.xml"
        ));
        resource.compile().expect("compile launcher icon resource");
    }
}
