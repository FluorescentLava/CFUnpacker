<p align="center"><img src="Assets/AppIcon.png" width="128" alt="CFUnpacker 图标" /></p>

# CFUnpacker

解包工具作者：熔萤FluorescentLava

[下载 Windows x64 版](https://github.com/FluorescentLava/CFUnpacker/releases/latest)

使用 **Rust + 原生 WinUI 3** 实现的保卫萝卜系列 APK 解包工具。界面通过 WinRT 投影直接调用 WinUI 3，解包核心直接链接进 GUI；正式程序没有 C#、.NET 或解包子进程。

本工具的格式分析、key 验证、算法整理、程序实现与测试借助了 AI 工具。

## 功能与使用

- 支持保卫萝卜、保卫萝卜2、保卫萝卜3、保卫萝卜4（含国际版 Carrot Fantasy）、保卫萝卜阿波之旅。
- 支持 `.apk`、`.apks`、`.xapk`，以及 `.apk.1`、`.xapk.2` 等单个数字后缀。分包优先读取 `base.apk`，再合并资源分包，跳过 `split_config.*.apk`。
- 保留 WinUI 3 控件、Mica 背景、绿色主题、游戏选择、APK 拖放、系统目录选择器、进度和取消对话框、运行日志及打开输出目录。
- 默认自动识别，依据加密头、可验证 key 和目录特征；四代也结合 `assets/res` 资源目录识别。手动选择不匹配时可切换流程、强制执行或取消。
- 可选择包含安装包的文件夹、填写文件路径，或拖入安装包。同一文件夹有多个安装包时，拖入指定文件。
- 支持安全覆盖同名输出：成功后提交新目录；取消或失败时保留旧输出并清理暂存。
- 支持 CCZ/PVR、明文 plist、三代/阿波 czzf plist、四代自定义 plist 和加密 PNG atlas；可识别四代早期明文 plist 与沿用旧 key 的资源。
- 读取含 Git 合并冲突标记的 plist 时跳过标记行与没有对应 key 的额外值，保留完整的键值内容；正常 plist 仍按结构校验。
- 提取资源保留在输出根目录；独立加密 PNG 会在原资源路径替换为明文 PNG。`Unpacked_PNG` 仅包含图集拆分的最终 PNG，不生成整张 atlas PNG 或 `_atlases`、`_pvr` 中间目录。
- Windows 不允许的文件名字符、末尾点和空格按 `%HH` 编码保留资源；编码后重名会报错，避免覆盖另一份资源。

完整解压发行 ZIP，运行 CFUnpacker.exe，选择游戏、输入和输出位置，再点击“开始解包”。最终目录使用安装包主文件名，例如 `game.xapk.2` 输出到 `game`。请只处理自己合法持有并有权使用的安装包与资源。

## 系统运行时

使用系统已安装的 **x64 Windows App Runtime 2.3.1 或更新的兼容 2.x 版本**。它并非所有 Windows 都预装；缺少时微软引导器会自动显示安装提示。选择“是”会打开官方获取页面，安装后重新启动程序。程序不静默下载安装运行时。

发行包不含 .NET、私带 WinUI 3 运行时或 runtime 目录。旁边的 Microsoft.WindowsAppRuntime.Bootstrap.dll 是约 385 KiB 的微软官方引导器，用于发现运行时和显示缺失提示。Rust 静态链接 CRT，无需额外安装 VC++ 运行库。

发布脚本从固定版本微软组件包提取引导器，并校验其 SHA-256 与数字签名。

## 核心优化

并行 ZIP 提取和图集处理、缓存长钥流、借用压缩输入、通道缩放查表、RGBA8888 快速复制、复用帧缓冲。PNG 使用 zlib-rs 快速无损压缩、批量扫描线与流式 IDAT，避免整图中间文件及二次读盘。默认并行度为逻辑处理器数减二，限制在 1–12 之间。

自定义封装校验长度和 CRC32，并限制实际膨胀大小。提取拒绝 ZIP 路径穿越、大小写重名、符号链接及 Windows 保留路径。输出提交通过同目录暂存和备份回滚完成。GUI 在独立线程解包，用消息更新界面；取消或关闭窗口会等待资源关闭和暂存清理。

## key

- 保卫萝卜 PVR/CCZ：`0x89427C80, 0xF850A6D9, 0x14FB3BA0, 0x3A557437`
- 保卫萝卜2 PVR/CCZ：`0x67AA748A, 0xFB868651, 0xE8243360, 0x34062D80`
- 保卫萝卜3 / 阿波之旅 PVR/CCZ：`0xA8AC7F50, 0x63F379E7, 0x06AE82BA, 0x5405FE14`
- 保卫萝卜3 / 阿波之旅 `czzf` plist：`0x26AB1359, 0x1C2485A3, 0xF2B34691, 0xAA172AF6`
- 保卫萝卜4 PVR/CCZ：`0x3CE64A05, 0x81E437A2, 0x37DB91EC, 0x65FA03B8`
- 保卫萝卜4 `ff db ff ee 66` plist / PNG atlas：`0x43F21B68, 0x9A0F3610, 0x3AC65312, 0xB8926AA3`

补充：一代 `Themes/scene/mainscene-hd.pvr.ccz` 使用二代兼容 key；四代少量 PVR 资源沿用三代/阿波之旅 key。

## 构建

需要 Windows、Rust stable MSVC x64、Visual Studio C++ Build Tools 和 Windows SDK。**正式构建不需要 .NET SDK。**

```powershell
cargo test --lib
.\Packaging\BuildRelease.ps1
```

生成 发布/win-x64 与 发布/CFUnpacker-win-x64.zip，附带第三方许可。发布脚本使用 Cargo.lock 固定依赖，从固定版本微软组件包提取引导器并核对哈希与签名。

调试运行：

```powershell
.\Packaging\PrepareBootstrap.ps1 -DestinationDirectory "$PWD\target\debug"
cargo run --bin CFUnpacker
```

命令行工具调用与 GUI 相同的 Rust 库，无需 Windows App Runtime，仅用于批处理和测试：

```powershell
cargo build --release --bin cfunpacker-core
.\target\release\cfunpacker-core.exe detect sample.apks --json
.\target\release\cfunpacker-core.exe unpack sample.xapk --output output --game auto
```

`unpack --overwrite` 开启安全覆盖；显式指定 `--game cf1/cf2/cf3/cf4/abo` 时使用指定流程。CLI 支持 Ctrl+C，或向标准输入发送一行 `cancel`，之后等待清理。

## 验证

```powershell
cargo test --release --lib
cargo build --release --bin cfunpacker-core
python tests/integration.py
```

Rust 单元测试覆盖解密、CRC/长度、像素格式、alpha、旋转、偏移、分包、路径规则和输入目录保护。端到端脚本仅使用 Python 3.11+ 标准库，自动生成小型安装包并验证识别、预期裁剪/旋转/留白像素、资源提取、覆盖与取消；所有输入和输出位于临时目录，结束后清理。
