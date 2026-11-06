//! fsops：给 AI Agent 用的 Windows 原生文件操作层。
//!
//! 目标不是"再写一个文件工具"，而是把模型在 Windows 上最容易踩的四类坑
//! （路径、换行、编码、文件占用）收进内核，并把返回体压到最小：
//! 同样的任务，工具输出越短，轮次越少，token 越省。

pub mod adopt;
pub mod cli;
pub mod documents;
pub mod edit;
pub mod harden;
pub mod io;
pub mod mcp;
pub mod paths;
pub mod render;
pub mod search;
pub mod text;
pub mod tools;
pub mod walk;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
