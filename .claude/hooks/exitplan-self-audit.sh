#!/usr/bin/env bash
# PreToolUse(ExitPlanMode) hook：本会话首次退出 plan mode → deny 回喂；状态按 session_id 隔离，每会话仅一次。
# 迫使重审计划后重新提交即放行。
set -euo pipefail

command -v jq >/dev/null 2>&1 || { echo "[exitplan-self-audit] jq 缺失，自检 hook 跳过" >&2; exit 0; }

input=$(cat)
sid=$(printf '%s' "$input" | jq -r '.session_id // "default"')
sid=$(printf '%s' "$sid" | tr -cd 'A-Za-z0-9-')   # 消毒：杜绝路径穿越
[ -n "$sid" ] || sid="default"
state="${TMPDIR:-/tmp}/claude-exitplan-audited-${sid}"

[ -f "$state" ] && exit 0   # 已自检过 → 放行

# 先 deny 后打标记：jq 失败则标记不落盘，避免 hook 被永久旁路
jq -nc '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: "本计划符合 彻底 / 不向后兼容 / 优雅简洁 / 对标参考 四原则吗？（见 ship 方案与计划原则）"
  }
}'
touch "$state"
