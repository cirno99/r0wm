# ADR 0002 — 以 Smithay 0.7 作为实现底座，纯 Rust 重写

## 状态
已采纳（2026-11-03）

## 背景
r0wm 要重构 g0wm（C + wlroots 0.20）。底座有三条路：Smithay（纯 Rust 库）、
自建 wlroots 0.20 的 Rust FFI 绑定、裸 wayland-server 手写协议栈。

事实：crates.io 上的 `wlroots` crate 最后版本停在 2019 年的 0.4.0，没有任何对应
wlroots 0.18/0.20 的维护版本；`scroll` 的做法是 fork sway 并把改过的 wlroots
静态编进源码树。Smithay 0.7.0（2025-06）仍在活跃开发，niri、cosmic-comp 均基于它。

## 决策
用 **Smithay + calloop** 重写，全项目无 C 依赖（Xwayland 通过协议对接系统
Xwayland 二进制，不链接 libwlroots）。g0wm 的 C 代码只作为**行为参考**，不作为
架构参考。

依赖版本策略：**跟随 Smithay git 上游，用 commit hash 固定**，不走 crates.io 的
版本号。初版钉在 `c7a26ade404dd7593d92514fb571922dccd1050a`（2026-11-02 并入）。
升级 Smithay 是一次显式动作（改 `Cargo.toml` 里的 `rev`），而非 `cargo update`
的副作用。

## 权衡
- 自建 wlroots FFI：能白拿 scene graph、可逐函数对照移植，但需要手写 0.20 的
  bindgen 绑定，并在所有回调里跨 FFI 边界处理对象生命周期（Rust 借用检查在边界
  处失效），wlroots 升级需整体重做。收益不足以抵消长期成本。
- 裸 wayland-server：控制力最强、教学价值最高，但 scroll 布局 + DE 级功能的目标
  下等于从零做 wlroots 的活，v1 周期以年计。
- 选 Smithay 的代价：dwl 的「全局变量 + 链表 + 每输出 tagset 位掩码」模型整体
  换成 `State` trait + 所有权模型；`bar.c` 那一套 scene-graph 直接摆矩形节点的
  做法要改写成基于 `RenderElement` 的自研 UI 层，没有现成 crate 可抄。
