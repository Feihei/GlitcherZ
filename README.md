# GlitcherZ

[中文说明见下方](#中文)

**GlitcherZ** is a free, open-source digital audio workstation (DAW) for Windows, macOS and Linux, inspired by the workflow of LMMS.

Make music entirely on your computer: compose melodies, arrange beats, mix tracks, and shape your sound — all in one app.

## Highlights

- **Built-in instruments & effects** — synths, EQ, compressor, delay, reverb and more, ready out of the box
- **Piano roll & arrangement timeline** — write melodies and arrange clips into full songs
- **Modular patch board** — connect instruments, effects and mixers freely with patch cables, like a virtual modular studio
- **Multi-track mixer** — balance levels, pan, mute and solo every track
- **Plugin support** — load CLAP and VST3 instruments and effects from your favorite plugin makers
- **Automation** — draw curves to bring your mix to life over time
- **Cross-platform & project files** — works on all three major desktop systems, and your projects are saved in an open, readable format

## Install

Grab the archive for your system from the release page.

**macOS** — the download is a `.zip` containing `GlitcherZ.app`. Unzip it, drag the app into
`/Applications`, and launch it. The builds are **not notarized** (there is no Apple Developer
certificate), so Gatekeeper blocks the very first launch:

1. Open **System Settings → Privacy & Security**.
2. Scroll to the message about *GlitcherZ* and click **Open Anyway**.
3. Confirm with **Open**.

On older macOS versions you can also right-click the app in Finder and choose *Open*. Terminal
fallback if the dialog refuses to appear:

```bash
xattr -dr com.apple.quarantine /Applications/GlitcherZ.app
```

This is a one-time step and nothing leaves your machine. The command-line interface
(`--render`, `--ops`, `--mcp`) is inside the bundle:
`/Applications/GlitcherZ.app/Contents/MacOS/glitcherz`.

**Windows** — unzip and run `glitcherz.exe`. If SmartScreen warns, click *More info* →
*Run anyway*.

**Linux** — extract the `tar.gz` and run `./glitcherz`. ALSA, OpenGL and an X11 or Wayland session
are required (see the release notes for the Debian/Ubuntu package list).

## Documentation

The user manual lives in [`docs/`](docs/README.md): the
[manual](docs/manual.md), a [shortcut cheat sheet](docs/shortcuts.md) and
[project files & disk layout](docs/project-files.md). Developer notes are in
`.agents/`.

---

## 中文

[English version above](#glitcherz)

**GlitcherZ** 是一款免费、开源的数字音频工作站（DAW），支持 Windows / macOS / Linux，工作流参考 LMMS。

无需任何外部设备，在你的电脑上就能完成编曲、配器、混音全流程。

## 主要特性

- **内置乐器与效果器** — 合成器、EQ、压缩、延迟、混响等，开箱即用
- **钢琴卷帘与编曲时间轴** — 编写旋律，把片段排成完整乐曲
- **模块化接线板（Patch）** — 用连线自由组合乐器、效果器与混音器，像搭一台虚拟模块合成器
- **多轨混音台** — 每个轨道可独立调音量、声像、静音与独奏
- **插件支持** — 加载你喜欢的 CLAP / VST3 乐器与效果器插件
- **自动化** — 绘制曲线，让参数随时间自动变化，让音乐动起来
- **跨平台 + 开放的项目文件格式** — 三大桌面系统通用，项目文件开放可读

## 下载与安装

到 Releases 页面下载对应系统的归档。

**macOS** — 下载的是含 `GlitcherZ.app` 的 `.zip`。解压后把 App 拖进 `/Applications` 再启动。构建
**没有经过公证**（没有 Apple 开发者证书），所以 Gatekeeper 会拦住第一次启动：

1. 打开 **系统设置 → 隐私与安全性**。
2. 向下找到关于 *GlitcherZ* 的提示，点 **仍要打开**。
3. 在确认框里再点 **打开**。

较老的 macOS 也可以直接在 Finder 里右键 App → *打开*。如果对话框始终不出现，可在终端执行：

```bash
xattr -dr com.apple.quarantine /Applications/GlitcherZ.app
```

只需放行一次，整个过程不会上传任何数据。命令行接口（`--render`、`--ops`、`--mcp`）在 App 包内：
`/Applications/GlitcherZ.app/Contents/MacOS/glitcherz`。

**Windows** — 解压后运行 `glitcherz.exe`；SmartScreen 提示时点 *更多信息* → *仍要运行*。

**Linux** — 解压 `tar.gz` 后运行 `./glitcherz`，需要 ALSA、OpenGL 以及 X11 或 Wayland 会话（依赖包
清单见发布页）。

## 文档

用户手册在 [`docs/`](docs/README.md)：[主手册](docs/manual.md)、
[快捷键速查](docs/shortcuts.md)、[工程文件与磁盘布局](docs/project-files.md)。
开发向文档在 `.agents/`。

---

## License / 许可证

GlitcherZ is licensed under **MIT OR Apache-2.0**. The executable embeds a
subset of **Noto Sans SC** (SIL Open Font License 1.1, © 2014-2021 Adobe);
the notice and full OFL text ship with every release archive in
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

GlitcherZ 本体按 **MIT OR Apache-2.0** 授权；可执行文件内嵌的 **思源黑体同源
Noto Sans SC** 子集按 SIL Open Font License 1.1 使用，声明与许可证全文见
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)，并随每个发布归档一同分发。
