---
name: ship
description: "全流程实施：探索→计划→worktree→实施→PR→内置 review→自动修小问题→人工确认。L1(跳过探索,1 reviewer)/L2(单 agent 探索,1 reviewer)/L3(默认,三 agent 探索,按 diff 行数 1/2/3/6 reviewer 自动)。"
argument-hint: "[--level=L1|L2|L3] <#issue-number 或任务描述>"
allowed-tools: [Read, Write, Edit, Glob, Grep, Bash, Agent, AskUserQuestion]
---

See .claude/skills/ship/SKILL.md
