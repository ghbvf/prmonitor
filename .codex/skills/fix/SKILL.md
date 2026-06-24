---
name: fix
description: "问题诊断与修复: 验证+根因+复杂度分级+修复方案+backlog登记。当用户说'这个问题存在吗''帮我分析这个bug''诊断一下这个模块''修复这个问题'时触发。输入优先 PR 号（自动读 PR 评论），也支持 文件:行号 / 自然语言；多 findings 自动批量。issue 号不再受理——issue triage 走 `issues` 技能（建议 /ship 或 close）。"
argument-hint: "<#PR | 文件:行号 | 问题描述>"
allowed-tools: [Read, Write, Edit, Glob, Grep, Bash, Agent, AskUserQuestion]
---

See .claude/skills/fix/SKILL.md
