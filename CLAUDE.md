# prmonitor

PR 监控 + AI review 桌面应用（Tauri v2 + Vue 3 + Rust）。仓库：github.com/ghbvf/prmonitor；Azure DevOps 主远端：`https://dev.azure.com/shengming0923/prmonitor/_git/prmonitor`；base 分支 `develop`。

> 本文件是 Claude Code 与 codex 的**共享指令入口**（codex 经 `AGENTS.md` 指向此）。只做索引指针——章程与技能正文是单源，本文件不复制。

## 治理章程

新增 / 修改约束 enforcement 机制前，先读 [.claude/rules/prmonitor/ai-robust.md](.claude/rules/prmonitor/ai-robust.md)：违反须**不可表达**、至少**机器可判定**；Hard / Medium / Soft 分级，最低 Medium，Soft 不立项。Claude Code 经 `.claude/rules/**` 自动加载本章程；codex 经本链路读取。

## 项目管理与开发工作流

issue / PR 真源 = 激活 forge 的 issue/work-item tracker + 看板（当前默认 Azure Boards），body / 评论 / label / 流程模板单源在 [.github/project-template/](.github/project-template/)；命令编排经 [hack/automation/forge.sh](hack/automation/forge.sh)。不再维护 markdown backlog 副本。

## 开发工作流技能（`.claude/skills/`）

- **ship**（`.claude/skills/ship/SKILL.md`）— 全流程实施：探索→计划→worktree→实施→PR→内置 review→修→收尾。
- **fix**（`.claude/skills/fix/SKILL.md`）— 问题诊断与修复：验证 + 根因 + 复杂度分级 + 修复 + 收尾。
- **pr-review**（`.claude/skills/pr-review/SKILL.md`）— 对 PR 跑六维度分级 review；`--check` 验证上一轮修复。
- **pr-monitor**（`.claude/skills/pr-monitor/SKILL.md`）— ship/fix 收尾后的 PR 状态自动接力检查器。
- **issues**（`.claude/skills/issues/SKILL.md`）— forge issue/work-item / PR / 评论 / label 的命令编排单源。
- **app-build-run**（`.claude/skills/app-build-run/SKILL.md`）— 本地启动 Tauri App，编译 Apple/macOS 与 Windows x64 桌面包。

技能正文是单源，本文件只索引、不复制。codex 侧入口见 `.codex/skills/`（薄引用，正文仍指向 `.claude/skills/`）。
