# 对立标基线（g0wm）的功能台账

按 ADR 0001：v1 目标是基线全等 + scroll 布局。**缺失只能记为欠账，不许改由外部组件补齐。**
状态取值：`未开始` / `部分` / `完成`。验证方式：`单测` / `嵌套冒烟` / `真机`。

## 基线功能

| # | 功能 | 基线出处 | 状态 | 验证方式 |
|---|---|---|---|---|
| 1 | KDL 配置 + 热重载（基线为 scfg） | `src/settings.c` | 部分 | 单测 |
| 2 | 键绑定与动作表 | `src/settings.c` / `config.h` | 部分 | 单测 |
| 3 | tag 模型（9 tag、位掩码、多选可见、toggle_tag、view_prev、shift_view） | `src/layout.c` | 完成 | 单测 + 嵌套冒烟 |
| 4 | 窗口圆角 / squircle 边框 | `src/corner.c` | 未开始 | 真机 |
| 5 | 窗口不透明度 / 模糊 | `src/opacity.c` | 未开始 | 真机 |
| 6 | 每窗口标题条 + 悬停关闭按钮 | `src/bar.c` | 完成 | 单测 + 嵌套冒烟 |
| 7 | tab 组（基线 tabbed 布局） | `src/layout.c` | 完成 | 单测 + 嵌套冒烟 |
| 8 | 拖动窗口 + 平铺交换 + 边缘吸附（move_snap） + 交互缩放 | `src/client.c` / `input.c` | 完成 | 单测 + 嵌套冒烟 |
| 9 | 状态栏（tags/布局符号/标题/状态文本与转义） | `src/bar.c` | 部分 | 单测 + 嵌套冒烟 |
| 10 | 状态文本点击/滚动执行命令 | `src/bar.c` | 完成 | 单测 |
| 11 | 通知（图标/优先级/动作） | `src/notify.c` | 部分 | 单测 + 嵌套冒烟 |
| 12 | runner（搜索/开关窗口/启动应用 + 计算器） | `src/runner.c` | 完成 | 单测 + 嵌套冒烟 |
| 13 | systray + 菜单 | `src/systray/*` | 部分 | 单测 + 嵌套冒烟 |
| 14 | 壁纸绘制 | `src/g0wm.c` | 未开始 | 真机 |
| 15 | Xwayland | `src/xwayland.c` | 未开始 | 真机 |
| 16 | text-input / input-method（IME） | `src/textinput.c` | 完成 | 真机 |
| 17 | 触摸板手势（三指切 tag） | `src/input.c` | 未开始 | 真机 |
| 18 | 放大镜（含捏合缩放） | `src/magnifier.c` | 未开始 | 真机 |
| 19 | 会话锁 | `src/lock.c` | 未开始 | 真机 |
| 20 | 输出管理：gamma / VRR / 电源 | `src/monitor.c` | 未开始 | 真机 |
| 21 | 自启动命令 | `src/g0wm.c` | 完成 | 嵌套冒烟 |
| 22 | 输入抑制（inhibit 快捷键）与打字时隐藏光标 | `src/input.c` | 未开始 | 真机 |
| 23 | 光标（XCursor 主题 / 客户端自绘） | `src/input.c`（wlroots 光标） | 完成 | 单测 + 嵌套冒烟 |
| 24 | 分数缩放（输出 scale + fractional-scale-v1） | `src/monitor.c` | 完成 | 嵌套冒烟 |
| 25 | 多输出（每输出独立 tagset / 工作区 / bar / 标题条） | `src/monitor.c` | 完成 | 真机 |

## 相对基线的欠账（基线有、r0wm 不做或不完全等价）

| 项 | 说明 |
|---|---|
| dwl `tile` 布局（master/stack + `mfact`/`nmaster`） | 由 bsp 取代，观感可逼近但不完全等价（见 ADR 0003 修订） |
| 图标的 `.xpm` 与主题继承 | `image-data`/`icon_data`、`image-path`/app_icon 的路径与名字都支持；但图标查找只走 `hicolor`（与基线一致），`.xpm` 不能解码（基线靠 gdk-pixbuf），`index.theme` 继承也没做。 |
| systray 的视觉细节 | 图标、fallback 首字母、点击与 dbusmenu 子菜单/勾选/过滤都已对齐；菜单框的圆角（`cornerpx`）与可配置边框厚度还没做，`IconName` 的 `.xpm` 也不解码（与图标查找同一条取舍）。 |
| 多 GPU | 多输出已经支持（同一块 GPU 上多个连接器各自独立）；跨 GPU 仍需要 `MultiRenderer` 拷贝，暂未实现。 |
| EDID 的厂商 / 型号 / 序列号 | 没有接 `libdisplay-info`（避免 C 依赖），`wl_output` 的这几个字段是 `Unknown`；物理尺寸与 subpixel 来自连接器本身，仍然正确。 |

## 新增（基线没有）

| 项 | 说明 |
|---|---|
| scroll 布局（行/列/tab 组/行偏移） | 见 ADR 0004、0005 |
| bsp 布局（默认平铺布局） | 见 ADR 0003 修订 |
| 多行同屏（一个 tag 一行，垂直等分） | 见 ADR 0004 |
| 方向邻居导航（bsp 几何邻居） | 见 `CONTEXT.md`「方向邻居」 |
| 多 tag 窗口的归属行规则 | 窗口只归属一个输出；同一窗口的多个 tag 仍只在一个输出的一行里显示，见 `CONTEXT.md`「归属行」 |
| IPC（命令/查询/订阅） | 见 ADR 0007，基线仅在 ROADMAP 中计划；M3 已可用，嵌套冒烟验证 |
| sticky 浮动窗口（置顶且全 tag 可见） | 基线没有；`toggle_sticky` 双向切换，只对浮动层生效，见 `CONTEXT.md`「sticky」 |
