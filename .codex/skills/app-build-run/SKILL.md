---
name: app-build-run
description: "prmonitor Tauri App 本地启动与打包：启动开发版桌面 app，编译 Apple/macOS app/dmg，交叉编译 Windows x64 exe；正式发版 Windows 为 portable zip（见 release.yml --no-bundle），本地交叉编仍可能涉及 cargo-xwin/makensis。"
argument-hint: "<dev | apple | windows-x64>"
allowed-tools: [Read, Grep, Bash]
---

See .claude/skills/app-build-run/SKILL.md
