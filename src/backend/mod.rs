//! 后端装配。
//!
//! 两条路径共用同一份 `State`，差异只在调度与输出创建（共识：udev 是一等公民，
//! winit 用于开发与集成冒烟）。

#[cfg(feature = "udev")]
pub mod udev;
#[cfg(feature = "winit")]
pub mod winit;
