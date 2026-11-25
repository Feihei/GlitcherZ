# F-051 · macOS `.app` 打包与未签名安装指引

> ID: F-051 · 域: IO · 状态: **已交付**（2026-11-25）

## 背景

发布归档原本是一个 `tar.gz`，里面是裸的 universal 二进制：Finder 里双击不了，
用户要么 `chmod +x` 再开终端，要么先放行 quarantine。对没有技术背景的用户来说，
这比 Windows 的 `.zip` 还难。同时，macOS 的 Gatekeeper 说明只写了"右键 → 打开"，
在 macOS 15+ 已不再可靠。

## 决策

| # | 决策 | 理由 |
| :--- | :--- | :--- |
| D1 | macOS 产物改成 `GlitcherZ.app`，装进 `glitcherz-$VERSION-macos-universal.zip` | 双击即启动、可拖进 `/Applications`；`ditto` 打包保留权限与签名 |
| D2 | Bundle 内 `Contents/MacOS/glitcherz` 就是现有 universal 二进制 | 不复制构建逻辑；CLI（`--render` / `--ops` / `--mcp`）仍可从该路径调用 |
| D3 | 用 ad-hoc 签名（`codesign --force --sign -`）并校验 | `lipo` 合并会破坏链接器给每个 slice 的签名；Apple Silicon 拒绝执行未签名代码。没有 Apple Developer 证书，不做 notarization |
| D4 | 版本号数字部分进 `Info.plist`（`0.1.0-preview.1` → `0.1.0`） | Apple 只接受数字点分版本；tag 的 pre-release 后缀保留在发布标题里 |
| D5 | 文档与 `THIRD-PARTY-NOTICES.md` 放进 `Contents/Resources/` | OFL 1.1 要求声明随二进制分发；用户点"显示包内容"也能找到手册 |
| D6 | README 与发布页都写明 Gatekeeper 放行步骤 | 未签名是常态，不能假设用户知道 System Settings 路径 |
| D7 | 不画 `.icns`，先用系统通用图标 | 图标设计不在本特性范围；bundle 没有 `CFBundleIconFile` 也能正常启动 |

## 不做

- Developer ID 签名、notarization、公证 stapling（需要付费账号与密钥）。
- DMG 封装、自动更新（Sparkle）、Windows MSI、Linux AppImage/deb。
- 自定义应用图标与 `.glz` 文件关联（Finder 双击打开）。

## 验收

1. `release.yml` 的 macOS job 产出 `glitcherz-<tag>-macos-universal.zip`，解压后顶层是 `GlitcherZ.app`。
2. `plutil -lint` 通过；`codesign --verify --deep --strict` 通过；`lipo -archs` 仍为 `x86_64 arm64`。
3. 首次双击被 Gatekeeper 拦下后，按 README 的"System Settings → Privacy & Security → Open Anyway"可启动。
4. CLI 路径 `/Applications/GlitcherZ.app/Contents/MacOS/glitcherz --render …` 可用。
5. Windows / Linux 归档与 `sha256sums.txt` 布局不变。
