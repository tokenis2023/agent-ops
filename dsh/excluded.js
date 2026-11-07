/**
 * DSH 插件形态刻意不暴露的 `fs_exec` 后台任务参数。
 *
 * 为什么单独成模块：这份清单同时被
 *   - `scripts/gen_dsh_specs.mjs`（生成 schema 时剔除参数、改写描述）
 *   - `dsh/index.js`（运行时显式拒绝，避免「schema 说没有、运行时却能穿透」）
 * 使用。分开写两份就是下一个漂移点——两处一旦不一致，模型会拿到与文档冲突的行为。
 */

/** 工具名 -> 插件形态不支持的参数名。 */
export const EXCLUDED = {
  fs_exec: ['background', 'job', 'list', 'wait_ms', 'kill'],
}

/** 该工具在插件形态特有的描述（覆盖 Rust 侧面向 MCP/CLI 的原文）。 */
export const DESCRIPTION_PATCHES = {
  fs_exec: (description) => {
    const cut = description.indexOf('启动任务必须给 program')
    const head = cut >= 0 ? description.slice(0, cut) : description
    return (
      head +
      '启动任务必须给 program。DSH 插件形态不提供后台任务参数（background/job/list/wait_ms/kill）；' +
      '需要后台任务请用 MCP 形态 `fsops mcp`。'
    )
  },
}
