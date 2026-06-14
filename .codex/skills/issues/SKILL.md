---
name: issues
description: "GitHub Issue / PR / 评论 / label 的 gh 命令编排单源（ship/fix/pr-review 共用）：建/改/关 issue、贴 PR 评论并回显 comment URL、PR 双轴状态 label 流转、PR 冲突预检 + CI watch。另含「非任务 issue 状态核查」：查代码判断 issue 是否仍成立，只判不修，建议 /ship 或 close。当用户要建/改 issue、给 PR 留评论、切 PR 状态 label、核一个 issue 是否还成立时使用。"
argument-hint: "<#issue（状态核查）| create-issue | edit-issue | comment | pr-status | pr-precheck> [...]"
allowed-tools: [Read, Grep, Bash, Agent, AskUserQuestion]
---

See .claude/skills/issues/SKILL.md
