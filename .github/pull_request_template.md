## Summary

<!-- 一句话描述这个 PR 做了什么 -->

## Why / 背景

<!-- 为什么改：要解决的问题 / 触发原因 / 预期结果（让 reviewer 不用翻 issue 也能懂动机）-->

## Refs

<!-- 关联 issue / plan，如 Closes #NNN；对标参考 ref: 文件 -->

## Risk / 兼容性

<!-- 破坏性变更 / 配置或数据迁移 / 跨切片契约（model.rs ↔ types.ts）影响 / 需同步的消费方；无则写「无」 -->

## Test plan

- [ ] `pnpm build` 本地通过（vue-tsc 类型检查 + vite build）
- [ ] `cargo build --manifest-path src-tauri/Cargo.toml --locked` 本地通过
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings` 0 告警
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check` 干净
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` 通过（涉及逻辑变更时）
