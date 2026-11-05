//! fsops 可执行入口：无参数或 `mcp` 走 MCP 服务，其余走 CLI。

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = if args.is_empty() {
        // 双击或宿主只给可执行路径时，默认当 MCP 服务（MCP 客户端就是这种调用方式）
        fsops::mcp::serve().map(|_| 0).unwrap_or(1)
    } else {
        fsops::cli::run(&args)
    };
    std::process::exit(code);
}
