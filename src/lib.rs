//! r0wm —— 以 g0wm 为对标基线的 Rust 版 Wayland 合成器（见 `docs/adr/0001`）。
//!
//! 架构约束（`docs/adr/0002`、`docs/adr/0007`）：
//!
//! - 底座是 Smithay 0.7（git、commit 固定），无 C 依赖；
//! - `layout` 与 `config` **不依赖 Smithay**，只吃纯数据、吐目标矩形，
//!   因此可以脱离合成器做单元测试；
//! - 键绑定、IPC 命令、bar 点击共用同一张动作表。

pub mod action;
pub mod backend;
pub mod cli;
pub mod compositor;
pub mod config;
pub mod input;
pub mod ipc;
pub mod layout;
pub mod notify;
pub mod runner;
pub mod shell;
pub mod systray;
pub mod ui;
