//! 自绘 UI：bar、通知、runner、systray。
//!
//! 约定（共识）：三者全部寄生在同一条 bar 上；bar 挤占工作区（不是浮层），
//! 但**不是** layer-shell 客户端——它是合成器自己光栅化的位图，直接作为渲染元素
//! 叠在窗口之上，工作区由合成器预留。这样 bar 不依赖任何外部进程，也符合
//! `docs/adr/0001` 的定位 B。
//!
//! 排版顺序（基线）：tags | 布局符号 | 标题/tab 区 | 状态文本 | systray。
//!
//! 光标不寄生在 bar 上：它是合成器自己的渲染元素（`cursor`）；托盘右键菜单
//! 是另一块内存位图（[`tray`]），叠在 bar 与窗口之上、光标之下。
//!
//! 通知由 `notify` 模块、命令提示由 `runner`、托盘与 dbusmenu 由 `systray` 接管，
//! bar 只负责把它们的快照画出来；图标查找与解码在 [`icon`] 里，几家共用。

pub mod bar;
pub mod cursor;
pub mod icon;
pub mod paint;
pub mod status;
pub mod title;
pub mod tray;

use smithay::{
    backend::renderer::{ImportAll, ImportMem, element::memory::MemoryRenderBufferRenderElement},
    render_elements,
};

render_elements! {
    /// 叠在窗口之上的合成器自绘元素（bar、标题条与光标）。
    pub OverlayElement<R> where R: ImportAll + ImportMem;
    Cursor=cursor::CursorRenderElement<R>,
    /// bar 与标题条都是同一种内存位图元素。
    Bar=MemoryRenderBufferRenderElement<R>,
}
