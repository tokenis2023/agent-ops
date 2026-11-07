/**
 * DSH 插件形态的参数裁剪与描述覆写（单一真源）。
 *
 * 同时被 scripts/gen_dsh_specs.mjs（生成 schema）与 dsh/index.js（运行时拒绝）使用。
 * 分开写两份就是下一个漂移点——两处不一致时，模型会拿到与文档冲突的行为。
 */

/**
 * 工具名 -> 插件形态不支持的参数名。
 *
 * v0.9.2 起 fs_exec 的后台任务参数已放开：长驻会话本来就能承载后台任务，
 * 之前的屏蔽让长命令（构建、测试）必然占住整个回合。
 * 保留这个机制本身，供将来确实无法在插件形态支持的参数使用。
 */
export const EXCLUDED = {}

/** 该工具在插件形态特有的描述（覆盖 Rust 侧面向 MCP/CLI 的原文）。 */
export const DESCRIPTION_PATCHES = {
  fs_exec: (description) => {
    const cut = description.indexOf('启动任务必须给 program')
    const head = cut >= 0 ? description.slice(0, cut) : description
    return (
      head +
      '启动任务必须给 program。DSH 插件形态支持后台任务（background/job/list/wait_ms/kill）：' +
      '长命令用 background=true 立即拿 job，再用 job=<id> wait_ms=<毫秒> 一次等到退出，' +
      '期间可以继续做别的调用。'
    )
  },
}
