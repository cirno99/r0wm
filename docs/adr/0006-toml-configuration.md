# ADR 0006 — 配置文件用 TOML 格式

## 状态
已采纳（2026-11-03）

## 背景
基线 `g0wm` 用 scfg 格式（libscfg）：`~/.config/g0wm/config`，`settings.c` 约 1877 行
做逐键解析，键绑定写成「修饰键 + 键名 + 动作 + 参数」的四元组，`MODKEY+Shift+R`
热重载。r0wm 是重写，配置格式可以重新选，不需要背历史包袱。

## 决策
r0wm 的配置文件使用 **TOML**：`~/.config/r0wm/config.toml`，用 `serde` 反序列化到
强类型结构体，再从中构建动作表与键绑定表；不做 scfg 解析器。

## 权衡
- 沿用 scfg：可以直接吃基线的配置文件、便于逐项对照迁移；但 scfg 只有「键值 +
  块」两样东西，没有类型、没有数组嵌套、没有注释规范，`settings.c` 里大量代码
  花在手工校验和默认值回填上，这部分在 Rust 里属于白白重写。
- TOML 的代价：基线配置**不能直接复用**，需要一份迁移对照表；嵌套表/数组的语义
  要自己定（例如每个 monitor 规则是一张表、每个 keybinding 是一个内联表）；
  解析失败必须**保留上一份有效配置**而不是让合成器崩掉，热重载的错误反馈要自己设计。
- TOML 的收益：`serde` 给出类型校验、缺省值、字段级错误定位；配置结构与 Rust 结构体
  一一对应，`settings.c` 的手工解析代码整体消失。

## 修订（2026-11-03）：改用 niri 风格 KDL

TOML 实现完成后，用户明确要求配置风格向 niri 靠拢。niri 的配置语言是 KDL，
`input {}` / `output "eDP-1" {}` / `layout {}` / `binds { Mod+Return { spawn "foot"; } }`
这套结构比 TOML 的扁平表更贴近本项目的使用习惯，也便于直接复用 niri 用户的
配置片段。

因此 r0wm 现在只解析 KDL（`src/config/kdl.rs`），默认路径变为
`$XDG_CONFIG_HOME/r0wm/config.kdl`。原来的 TOML 前端与 `toml` 依赖已移除；
ADR 正文里的 TOML 决策仅作为历史记录保留。KDL 解析失败仍然遵循「保留上一份
有效配置」的约定，niri 有而 r0wm 没有的选项会被忽略而不是让整份配置失效。
