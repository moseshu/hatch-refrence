//! BFCL agent 级评估：把 BFCL 题目走一遍真实的 agent 回合。
//!
//! 只做 Phase A（单轮）：simple_python、multiple、irrelevance。
//! 每个题目函数变成一个 stub 工具（见 [`stub`]），agent 在进程内跑一轮，
//! 调用从会话 transcript 里取出来，写成 BFCL 格式的结果文件，
//! 再用官方 `bfcl evaluate` 评分。评分器不由这里实现。
//!
//! 约定：
//! * 每题一个全新 session + 全新内存 DB，题目之间完全隔离；
//! * Registry 只含该题的函数（clean room），agent 自带工具不混入；
//! * stub 调用无副作用，审批走 workspace harness 的 decide() 模式自动放行。

pub mod dataset;
pub mod results;
pub mod runner;
pub mod stub;

pub use runner::{run_bfcl_eval, BfclOpts, BfclSummary, CategorySummary};
