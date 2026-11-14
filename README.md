# Foxes Token Watch

Windows 本地 AI 编程助手用量与会话状态监控工具。作者：**Foxes**。

本项目公开源码，采用 **PolyForm Noncommercial 1.0.0** 许可。允许在许可范围内进行非商业使用、修改和分享，须保留 Foxes 署名、[许可文本](LICENSE)和[声明](NOTICE)。**未经另外授权，不允许商用**。第三方依赖仍适用各自的[许可证](THIRD_PARTY_NOTICES.txt)。

## 使用前请注意：MCP 网关仍是实验支持

**MCP 网关的用量与会话状态识别目前不稳定，这部分实现比较粗糙，不保证开箱即用。大概率需要借助 Codex 或 Claude 自行调试、修复后才能正常使用。**

不同网关、插件和版本的记录格式可能不同，可能出现识别不到会话、用量估算偏差或状态更新异常。请把这部分当作可供修改的实验实现，不要把它视为成熟、通用的 MCP 网关监控方案。

## 功能

- 读取 Claude Code、Codex、Deepseek Harness、Antigravity、Kiro 和 Cursor 的本机会话记录；Cursor 包含支持的网页 MCP 对话来源。
- 显示 Token 用量、模型分布、请求记录以及运行中、等待中和已结束等会话状态。
- 提供桌面灵动岛、边缘停靠、系统托盘和可选开机自启。
- 支持深色、浅色主题，以及 K/M/B 和万/亿数字单位。
- 对没有完整 Token 记录的来源显示估算用量；估算结果不等同于服务商账单。

## 下载与安装

在本仓库的 **Releases** 页面下载 `Foxes-Token-Watch-0.2.1-x64-Setup.exe`，运行简体中文安装向导。

当前发行目标为 **Windows 10/11 x64**。界面依赖 Microsoft Edge WebView2；缺少该组件时，安装器会联网下载微软的运行时安装程序。统计过程在本机完成，不向项目作者上传对话或用量数据。

关闭主窗口后软件继续在托盘运行；彻底退出请使用设置页或托盘中的退出按钮。

## 数据与空间占用

软件只读访问各 Agent 的来源记录。自己的设置、用量数据库和压缩缓存默认保存在：

```text
%APPDATA%\com.foxes.watch\
```

其中 `settings.json` 保存设置，`agent-usage.sqlite` 保存部分来源的用量记录，`cache/gateway/*.json.zst` 保存用于持续估算的历史元数据。缓存保存时间、大小和工具标识等信息，不保存对话正文、工具参数或工具响应正文。

当前正式程序约 13 MB，安装包约 3.3 MB。历史数据会随使用增长，不设置固定的 30 MB 上限。`target`、`node_modules`、`tmp` 等目录用于开发和构建，正常使用安装版不会生成这些开发目录。

本地统计可能包含会话标识、来源路径和工具活动等元数据；反馈问题时请先移除私人信息，不要直接上传应用数据目录。

## 从源码构建

准备 Node.js 24、Rust 稳定版 MSVC 工具链，以及带 Windows SDK 的 Visual Studio C++ Build Tools。

```powershell
npm ci
npm run package:windows
```

安装包输出到 `installers/`。打包脚本完成后会清理开发依赖和构建缓存；如需保留以便继续开发：

```powershell
npm run package:windows -- -KeepBuildFiles
```

开发桌面程序：

```powershell
npm ci
npm run tauri -- dev
```

只预览前端时使用 `npm run dev`，浏览器中显示的是模拟数据。

核心测试：

```powershell
cargo test --locked -p foxes-core --lib
```

手动清理可重建的开发文件：

```powershell
npm run clean:dev
```

该命令保留当前便携程序、`installers/`、源码、Git 历史和应用数据；清理后再次构建需要恢复依赖并重新编译。

## 反馈

欢迎通过 Issues 提交问题。请附上软件版本、Windows 版本、涉及的 Agent、复现步骤以及经过脱敏的错误信息。

商业授权需另行取得 Foxes 的许可。本仓库公开源码不代表授予商业使用权；详细条款以 [LICENSE](LICENSE) 为准。
