---
name: issues
description: "激活 forge 的 issue/work-item tracker + 看板项目管理单源技能（GitHub Issues+Project v2 / Azure Boards / GitLab issues，经 forge.sh 适配）。Part A：epic 拆解 + wave 实施顺序评论；Part B：issue/PR 原子操作（建/改 backlog issue、area/type/pri label、PR 双轴状态 label 流转、统一 PR 评论格式 + 冲突预检/CI watch 跟进，ship/fix 共用）。非 epic issue 号 → 查代码判状态（只判不修，建议 /ship 或 close）。"
argument-hint: "<epic #N | #issue（非epic→状态核查）| create-issue | edit-labels | pr-status | comment> [...]"
allowed-tools: [Read, Grep, Bash, Agent, AskUserQuestion]
---

See .claude/skills/issues/SKILL.md
