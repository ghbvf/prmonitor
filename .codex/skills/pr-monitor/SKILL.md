---
name: pr-monitor
description: "PR 状态自动接力检查器：ship/fix 收尾约 15min 后必须启动；读取外部 app/review 已产生的 label + 最新机器块，过 handoff 机器门（fresh canonical block + verdict + same-head + next 一致）才接力 /fix——Cx/scope 判定下放 /fix。pr-monitor 自身不贴评论、不切 label。"
argument-hint: "<PR#> --mode=auto [--role fix|review]"
allowed-tools: [Bash, Read, Skill, Agent]
---

See .claude/skills/pr-monitor/SKILL.md
