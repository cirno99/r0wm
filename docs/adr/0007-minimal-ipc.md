# ADR 0007 — 自研最小 IPC，不复用 i3/sway 协议

## 状态
已采纳（2026-11-03）

## 背景
基线 g0wm 没有 IPC，外部只能靠键绑定与读标准输入的状态脚本；它的 ROADMAP 把
「IPC socket」列为待办。`scroll` 因为继承 sway，自带 i3 IPC，`swaymsg`、`waybar`
等工具能直接驱动它。r0wm 有内置 bar，但脚本化控制与外部状态跟随仍是刚需。

## 决策
实现**自研最小 IPC**：Unix socket 位于 `$XDG_RUNTIME_DIR/r0wm.<WAYLAND_DISPLAY>.sock`
（0600），行分隔 JSON。三类消息：

- 命令：走**和键绑定同一张动作表**，不存在第二套解析器；
- 查询：`clients` / `tags` / `outputs` / `focused`，返回 JSON；
- 订阅：`subscribe` 推送事件（focus / tag / title / urgent / layout / output），
  每个订阅者有有界缓冲，读得慢就断开，绝不阻塞合成器。

事件格式从第一天带版本号。

## 权衡
- 复用 i3/sway IPC：生态工具白拿，但协议面很大（workspace 容器树、marks、窗口
  事件），要把 tag / 行 / 列 / tab 组硬翻译成 i3 的容器树，翻译损耗大；而 bar 是
  内置的，主要受益者是第三方脚本。
- 不做 IPC：最省，但「脚本控制 + 外部跟随状态」早晚要补，晚补的代价是命令表被
  键绑定锁死。
- 选自研的代价：需要自己维护协议文档与版本兼容；也放弃 `swaymsg` 类现成客户端，
  要配一个最小 CLI。
