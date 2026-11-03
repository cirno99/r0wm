# ADR 0001 — r0wm 定位为自足的单二进制桌面环境

## 状态
已采纳（2026-11-03）

## 背景
r0wm 以 `.consult/G0wm`（C + wlroots 0.20，约 17.9k 行，dwl 衍生）为对标基线，
改用 Rust 重构，并新增 PaperWM/niri 式 scroll 布局。g0wm 的「DE 感」集中在 bar 一族：
状态栏、通知、runner/计算器、systray、tabbed 复用同一根 bar，且不依赖外部组件。

开工前必须先决定：r0wm 是复刻这套一体化体验，还是只做 compositor 内核、把
bar/通知/启动器交给 waybar/mako/fuzzel 之类的生态组件。

## 决策
r0wm 定位为**自足的单二进制桌面环境**。v1 的功能目标是 g0wm 全等（状态栏、通知、
runner、systray、tabbed、wallpaper、Xwayland、配置热重载）再叠加 scroll 布局；
运行时不依赖第三方 bar、通知守护或启动器。

可以分阶段交付，但「功能缺失」只能记为相对基线的欠账，**不允许改由外部组件补齐**。

## 权衡
- 备选 A（分阶段自足 + 前期用外部组件过渡）：v1 周期最短，但开箱观感在过渡期断裂，
  且后期把 waybar 换成内置 bar 等于同一件事做两遍。
- 备选 C（纯内核 + IPC，bar 交给 waybar）：实现最省，但丢掉 g0wm 最有辨识度的
  一体化体验，与「重构 g0wm」的动机不符。
- 选 B 的代价：v1 周期约为 A 的 3~5 倍；Rust 生态没有现成的 bar/通知栈可抄，
  `bar.c`(1874) + `notify.c`(1025) + `runner.c`(1496) + `systray/`（约 1.6k）
  都需要等价重写。
- 选 B 的收益：单进程模型——bar 与布局共享同一份状态，点击/悬停/拖拽无需 IPC 往返；
  也没有外部组件的版本耦合。
