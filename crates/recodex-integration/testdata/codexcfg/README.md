# codexcfg 行为对照语料

`~/.codex/config.toml` 由**三方**写入:ReCodex 命令行(Go,`internal/clientcfg`)、
ReCodex 桌面端(Rust,本 crate),以及 Codex++ 自己(它会重新序列化整份文件、丢掉注释)。

两个客户端各有一份实现,历史上它们**一致地错**过一次:托管块往顶层塞 `model_provider`,
而用户可能已经有一个 —— TOML 顶层重复键让 Codex 连整份文件都解析不了
(`duplicate key model_provider in document root`),用户看到的却是
「Model provider 'custom' not found」这种完全指错方向的报错。

这批语料就是两侧的共同契约:同一份 input,install 和 remove 的结果必须逐字节相同。
Go 测试读 `desktop/recodex-integration/testdata/codexcfg/`,Rust 测试读镜像到
crate 里的同一批文件,`apply.ps1 -AdapterOnly` 保证两份一致。行为一旦分叉就是红测试,
而不是用户机器上一份打不开的配置。

- `body.toml` —— 所有用例共用的托管块内容(等同 `render_sub2api_block` 的输出)
- `<用例>/input.toml` —— 输入
- `<用例>/installed.toml` —— `install_block(input, body)` 的期望输出
- `<用例>/removed.toml` —— `remove_block(installed)` 的期望输出

`removed.toml` 不必与 `input.toml` 逐字节相同:被接管的 `model_provider` 会还回来,
但会落在第一个表头之前,未必回到原来那一行。TOML 语义等价即可。
