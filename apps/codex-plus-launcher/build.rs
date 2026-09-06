fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        // recodex-overlay: 用上游原图标(ChatGPT 风格),名称仍是 ReCodex。
        // 不要指向 manager 的 icons/icon.ico —— 那个会被 overlay 替换成 Rx 图标。
        resource.set_icon("../../assets/images/recodex.ico");
        resource.set_manifest(include_str!(
            "../codex-plus-manager/src-tauri/windows-app-manifest.xml"
        ));
        resource.compile().expect("compile launcher icon resource");
    }
}
