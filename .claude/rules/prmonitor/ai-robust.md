# AI-robust 治理章程

> 第一性原理：prmonitor 主要实施者是 AI（Claude Code）。新增 / 修改 enforcement 机制必须让违反**不可表达**，至少做到**机器可判定**；纯口头约定不可作为新增 enforcement。
>
> 本文件是约束 enforcement 的权威真值源。原则在此终结，不向 ADR / 代码 "详见"；落地实例活在代码 + 测试 + lint 配置，本文不复制也不指向。

## 适用范围

本章程**仅**适用于「新增 / 修改约束 enforcement 机制」，锚定 prmonitor 的真实治理面：

- **垂直切片边界**：三切片 `config` / `pr` / `review` 自包含，跨切片契约只走 `src-tauri/src/model.rs`（`Candidate` / `PullRequestView`）。其中 `Candidate` 是 cross-Rust-slice 契约但 backend-internal（由 `PrSource::discover` 返回、未经 Tauri command 暴露给前端），故 `src/types.ts` 不镜像它属设计预期、非契约缺口；只有 `PullRequestView` 是前后端契约类型
- **trait seam**：PR 来源 `pr/source.rs` 的 `PrSource`、review 引擎 `review/engine.rs` 的 `ReviewEngine`——新增实现而非改调用方
- **serde camelCase 契约**：所有序列化给前端的 Rust 类型 ↔ TS 的 wire 形状对齐——`model.rs`（`PullRequestView`）、`events.rs`（`ReviewEvent`）↔ `src/types.ts`（共享跨切片契约），`config/model.rs`（`AppConfig`）↔ `src/config/types.ts`（切片私有）
- **Tauri command 注册**：组装根 `src-tauri/src/lib.rs` 的 command 暴露面
- **错误漏斗**：`AppError` / `AppResult`（`src-tauri/src/error.rs`）的统一错误出口
- **事件 union**：`events.rs` 的 `ReviewEvent`（tagged `kind` camelCase）与 `src/types.ts` 的 discriminated union

**不在范围**：日常业务（加切片 / 加字段 / 修 bug / refactor）；常规 lint / test / build；review finding 中的纯 bug 修复类。不把 bug 修复本身包装成新治理机制。

## AI-robust 三档分级

| 档 | 定义 | 典型载体 | AI 可绕过性 |
|---|---|---|---|
| **Hard** | 违反不可表达 | Rust type system：newtype / 私有字段 + typed constructor / `#[non_exhaustive]` / sealed enum；codegen + `git diff --exit-code` 单源派生；TS branded type / discriminated union / `as const` | **0** |
| **Medium** | 违反可表达，但 CI 由 type-aware scan / golden / runtime guard 抓住 | serde golden / snapshot 测试锁 wire 形状；`assertNever` 穷尽性；clippy `#[deny(...)]`；vitest 契约一致性断言 | 低 |
| **Soft** | 依赖人记住 / 注释 / 命名 convention / 手维护清单 | review 凭眼力 / `// TODO` 注释 / README 段落 / 手镜像字段——**禁止作为新增机制** | **高** |

## 载体决策原则

新增 enforcement 按下列优先级选载体：

1. **codegen + `git diff --exit-code`**——单源（`model.rs` / schema）派生执行体，CI 跑 diff 校验未手改（**Hard**）
2. **Rust / TS type system**——newtype / 私有字段 + typed constructor / sealed enum / `#[non_exhaustive]`；TS branded type / discriminated union / `as const`，让违反不可表达（**Hard**）
3. **golden / snapshot 锁 serde wire 形状**——`serde_json::to_value` 断言键名集合，防字段漂移（**Medium**）
4. **runtime guard / `assertNever` 边界穷尽**——类型穷尽 / 数据形状在边界校验，fail-fast（**Medium**）
5. **clippy / `#[deny(...)]` lint 兜底**——type-aware scan 抓住可表达的违反（**Medium**）

**Soft 不立项。**

### 立项硬门槛

**≥ Medium。Soft 形态严禁立项**——纯靠口头规则 / 手维护清单的约束不立。要么有 Hard / Medium 兜底，要么不写。

## 审查要求

涉及 enforcement 的 finding 必须给出 **Hard / Medium / Soft** 评级 + 载体：

- **Hard**：保留；符号和证明写入对应**代码注释**。
- **Medium**：保留；若有低成本 Hard 化路径，登记 GitHub Issue。
- **Soft**：新增时 **reject**；既有 Soft 优先升级到 Medium 或 Hard。

**Funnel 类约束必须分别说明上游与下游强度——只锁 callsite 不是闭环 funnel。** 上游锁住、下游开口的 funnel 不算闭环，须明确指出开口侧。

> 本规则**禁止维护落地实例清单**：实例、符号、评级证明写在对应代码注释或 PR，本文不复制也不指向具体实例。

## 评级范例（acceptance）

以 `src-tauri/src/model.rs` ↔ `src/types.ts` 的 **serde camelCase 契约**为例：

- **现状**：仅靠人工镜像两侧字段（Rust struct 字段 ⇄ TS `interface`）→ **Soft**。
- **Funnel 上下游分析**：
  - **上游** = Rust `#[serde(rename_all = "camelCase")]`，已锁 Rust 侧 wire 产出。
  - **下游** = TS `src/types.ts` 的 `interface`，手维护、无机器校验。
  - → **只锁了上游，下游开口 = 非闭环 funnel**。
- **规定载体**：serde golden / snapshot 测试（**Medium**）——已随本 PR 落地于 `src-tauri/src/model.rs` 的 `#[cfg(test)] mod tests`，锁 wire 形状（断言 camelCase 键存在、snake_case 键缺席），防 Rust 侧字段漂移。
- **进一步 Hard 路径**（future）：从 `model.rs` codegen 派生 `src/types.ts` + `git diff --exit-code`，使 Rust↔TS 漂移在 CI 不可表达地失败。

评级证明见该测试模块的注释，本文不复述。
