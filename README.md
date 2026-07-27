# CFUnpacker

解包工具作者：熔萤FluorescentLava

本工具的格式分析、key 验证、算法整理、WinUI 3 程序实现与测试借助了 AI 工具。

## 功能

- 解包对象：保卫萝卜、保卫萝卜2、保卫萝卜3、保卫萝卜4、保卫萝卜阿波之旅。
- 默认使用“自动识别”，按资源加密头、可验证 key 和目录特征选择流程，不依赖 APK 版本号。
- 手动选择与识别结果不一致时，会询问切换到识别版本、按当前版本强制执行或取消。
- 输入仅接受 `.apk`；可输入地址、使用文件选择器或直接拖入，拖入后会自动填写地址。
- 输出位置可输入或选择。最终目录名与 APK 文件名一致。
- 支持 CCZ/PVR、明文 plist、三代/阿波之旅 `czzf` plist、四代 `ff db ff ee 66` plist 和加密 PNG atlas。
- `Unpacked_PNG` 只保存按 plist 拆出的最终 PNG，不生成或保留整张 atlas PNG、`_atlases`、`_pvr` 等中间结果。
- 取消、失败和成功后都会清理 `.unpacking-*` 暂存目录；覆盖模式完成后会删除替换用备份目录。

## 使用

1. 运行 `CFUnpacker.exe`。
2. 顶部默认选择“自动识别”；也可手动指定游戏。
3. 选择或拖入 APK，再选择输出文件夹。
4. 如需替换已存在的同名目录，打开“覆盖同名输出目录”。
5. 点击“开始解包”。完成后可直接打开输出目录。

自动识别失败时可手动指定版本；强制执行不匹配流程可能因 key、CRC32 或资源格式不同而跳过相关图集。请只处理自己合法持有并有权使用的 APK 与资源。

## key

- 保卫萝卜 PVR/CCZ：`0x89427C80, 0xF850A6D9, 0x14FB3BA0, 0x3A557437`
- 保卫萝卜2 PVR/CCZ：`0x67AA748A, 0xFB868651, 0xE8243360, 0x34062D80`
- 保卫萝卜3 / 阿波之旅 PVR/CCZ：`0xA8AC7F50, 0x63F379E7, 0x06AE82BA, 0x5405FE14`
- 保卫萝卜3 / 阿波之旅 `czzf` plist：`0x26AB1359, 0x1C2485A3, 0xF2B34691, 0xAA172AF6`
- 保卫萝卜4 PVR/CCZ：`0x3CE64A05, 0x81E437A2, 0x37DB91EC, 0x65FA03B8`
- 保卫萝卜4 `ff db ff ee 66` plist / PNG atlas：`0x43F21B68, 0x9A0F3610, 0x3AC65312, 0xB8926AA3`

补充：一代 `Themes/scene/mainscene-hd.pvr.ccz` 使用二代兼容 key；四代少量 PVR 资源沿用三代/阿波之旅 key。

## 核心流程

1. 校验 APK/ZIP 后，仅并行提取 `assets`，并阻止 ZIP 路径越界。
2. `CCZp` 使用 1024 词长钥流解密首 512 词，再每隔 64 词处理 1 词；`CCZ!` 直接 zlib 解压。
3. `czzf` 与四代自定义封装连续处理首尾各 512 词，中段每 4 词处理 1 词；三代/阿波之旅使用 6 轮，四代使用 8 轮，并校验解包长度及 CRC32。
4. PVR v2 按 header mask 解码 RGBA4444 等格式，PVR v3 按通道描述解码；三代 Android 地图图集的 `rgb/565` BGR 存储会自动校正通道；按游戏处理预乘 alpha。
5. 图集只在内存中存在，按 plist 坐标、偏移和旋转信息直接写出最终帧。

提取阶段按处理器数量使用独立 ZIP 读取器，拆图阶段最多并行处理 12 个图集；长钥流按 key/轮数缓存。PNG 使用快速无损压缩、池化帧缓冲与批量扫描线输入，避免生成整图和二次读盘。

## 验证

- 真实样本逐像素回归：五个游戏选项、两种四代纹理路径、旋转帧与一代兼容 key 均通过。
- 端到端最小 APK：提取、拆图、README、落盘和暂存清理均通过。
- APK 自动识别：一至四代、阿波之旅及未知 APK 均通过回归测试。
- 全量现存资源扫描：4,951 个 plist、4,934 个图集、4,468 个 PVR、471 个四代加密 PNG 图集均通过解密和解码。
- Release x64 构建：0 警告、0 错误。

## 构建

需要 Visual Studio 2022/Build Tools、.NET 10 SDK 和 Windows App SDK 工作负载。

```powershell
dotnet restore CFUnpacker.csproj -r win-x64 -p:Platform=x64
dotnet build CFUnpacker.csproj -c Release -p:Platform=x64 --no-restore
.\Packaging\BuildRelease.ps1 -Configuration Release
```

发布脚本会生成 `发布\CFUnpacker-win-x64.zip`。解压后根目录只包含
`CFUnpacker.exe`、`.dll`、`.deps.json`、`.pri`、`.runtimeconfig.json`
和 `runtime` 文件夹；这五个启动文件不会在 `runtime` 内重复，其余运行依赖均位于 `runtime`。
