# r0wm

以 [g0wm](.consult/G0wm) 为**对标基线**的 Rust 版 Wayland 合成器，自带 PaperWM/niri
风格的 **scroll 布局**。

- 底座：Smithay（git 依赖，commit 固定，无 C 依赖）
- 定位：自足的单二进制桌面环境——bar、通知、runner、systray 都是自己的，不依赖
  waybar/mako/fuzzel
- 布局：`bsp`（默认）/ `scroll` / `tabbed` 三套可切换平铺布局，外加浮动层
- 配置：niri 风格 KDL；控制：Unix socket + JSON 的行分隔 IPC
- 许可证：GPL-3.0-or-later

## 状态

里程碑 M3：**IPC 与协议补齐**。控制套接字支持命令 / 查询 / 订阅三类消息；
新增 xdg-decoration、xdg-activation、layer-shell、wp-viewporter、presentation-time
五个全局。M2 的平铺布局（bsp/scroll/tabbed、tag、多行同屏、方向导航）与 M1 的
winit 嵌套后端保持可用；M2 收尾补上了浮动窗口的交互缩放与拖动吸附，以及
`toggle_tag` / `view_prev` / 多 tag 归属。M4 起步补上了 **linux-dmabuf**、
**光标主题渲染**、**layer-shell 独占/按需键盘焦点**、**分数缩放（fractional-scale-v1）**、
**popup grab**（显式 grab + 点外关闭，含 layer 上的 popup 约束）与 IPC 的 `output` 事件
（渲染器已搬进 `State`，后续渲染类功能都走这条路）。M4 还接上了 **自绘 bar** 与
**每窗口标题条**：tags、布局名、状态文本（状态行沿用基线，从合成器
stdin 读，支持 `^c#/^b#/^sN!命令^` 转义与可点击区），bar 高度从工作区里扣除，点击
tag/布局名直接走动作表；每个窗口标题条从内容上方占一条，tab 组的标题并排平分成
可点击的 tab，悬停出现关闭按钮，点标题可拖动窗口，滚轮切焦点；另新增 **sticky
浮动窗口**（置顶且在所有 tag 可见）。M4 续作接上了 **通知**：r0wm 自己实现
`org.freedesktop.Notifications`（GetCapabilities / Notify / CloseNotification /
GetServerInformation，以及 NotificationClosed、ActionInvoked、ActivationToken 信号），
通知画在 bar 中间，支持紧急程度配色、倒计时进度条、图标（`image-data`，以及 `image-path`/app_icon 的
路径或名字，查 `hicolor` 主题）、排队、历史浏览、动作提示与选择器。M6 第一刀接上了
**命令提示（runner）**：`super+r` 打开，应用（`.desktop`）、命令（`$PATH`）、窗口、
tag、算式、文件各一个模式前缀，带模糊匹配、历史与使用频次加权、实时补全；
候选行按基线画圆角胶囊与图标（`runner_pill*` / `runner_icon*`）。M6 续作
接上了 **systray**：r0wm 自己充当 `org.kde.StatusNotifierWatcher`，图标画在 bar 右端，
左键 `Activate`、右键拉取 dbusmenu 在光标处弹出菜单（子菜单、勾选项、隐藏/禁用过滤
都照基线），没有图标时用 `Id` 的首字母占位。M1 的最后一块也补上了：**udev 真机后端**
（libseat 会话 + DRM/KMS 直出 + libinput 输入，`--backend udev` 或 `--features udev`），
与 winit 后端共用同一份 `State`、同一套 bar/标题条/光标叠加层。最新一轮补齐了
**中文输入法**（text-input-v3 + input-method-v2，fcitx5/ibus 可直接连接）、
**多输出**（每个输出独立 tagset、工作区、bar 与标题条，指针可跨屏移动）、
**niri 风格 KDL 配置**（`input` / `output` / `layout` / `general` / `binds`），
以及 udev 空帧也发送 frame callback 的渲染修复。进度见
[`docs/parity.md`](docs/parity.md)。

当前限制（都会在后续里程碑补齐）：

- 托盘菜单的圆角/边框细节比基线简化（功能对齐）；图标只查 `hicolor`、
  不认 `.xpm`（与基线同源的取舍）；
- wlr-screencopy 等输出类协议待后续；
- udev 后端支持一块 GPU 上的多个输出；跨 GPU 仍需要 `MultiRenderer` 拷贝，
  没有接入 EDID 解析，`wl_output` 的厂商/型号是 `Unknown`。

## 构建

工具链默认 **nightly**（见 `rust-toolchain.toml`）；需要可复现构建时把其中的
`channel` 换成具体日期。

```sh
cargo build                    # 默认 feature：winit 嵌套后端
cargo run --bin r0wm -- --backend winit

cargo build --features udev    # 真机：DRM + libinput + libseat
cargo build --features udev,xwayland
```

真机后端需要系统开发库（`pkg-config` 可见）：`libdrm`、`libgbm`、`libinput`、
`libseat`。默认（winit）构建只需要 `libxkbcommon` 与 `libwayland-client`。

真机运行需要能拿到 DRM 设备的会话（`seatd` 或 systemd-logind 之一）：

```sh
# 从 TTY 启动（logind 会自动把当前 VT 交给它）
cargo run --features udev -- --backend udev -c /tmp/config.kdl
# 用 seatd 时先确保 seatd 在跑，且当前用户在 video/input 组里
```

输出（连接器）是热插拔的：插上显示器会建 `wl_output` 并开始扫描，拔掉会撤销。
`RUST_LOG=r0wm=debug` 可以看到连接器、模式和 vblank 相关日志。

NixOS 上 `shell.nix` 已把这些依赖和运行期 `LD_LIBRARY_PATH` 准备好：

```sh
nix-shell --run 'cargo run --bin r0wm -- -c /tmp/config.kdl'
```

## IPC

套接字位于 `$XDG_RUNTIME_DIR/r0wm.<WAYLAND_DISPLAY>.sock`（0600），行分隔 JSON。
`r0wmctl` 是最小客户端：

```sh
WAYLAND_DISPLAY=wayland-2 r0wmctl query focused   # 当前窗口与 tag
WAYLAND_DISPLAY=wayland-2 r0wmctl query tags      # 9 个 tag 的可见性/布局/窗口数
WAYLAND_DISPLAY=wayland-2 r0wmctl query clients   # 窗口列表（标题、app_id、层、几何）
WAYLAND_DISPLAY=wayland-2 r0wmctl query outputs   # 输出与工作区
WAYLAND_DISPLAY=wayland-2 r0wmctl cmd view 3      # 任何动作表里的动作
WAYLAND_DISPLAY=wayland-2 r0wmctl subscribe focus tag layout output
```

命令/查询是一问一答（服务端回完即关连接）；订阅保持长连接，先收到带版本号的
`hello` 与当前状态快照，之后是 `focus` / `tag` / `title` / `layout` / `output` 事件。
读得太慢的连接会被直接断开，不会阻塞合成器。

## 状态行

bar 右侧的状态文本沿用基线：把脚本的输出接到合成器 stdin，最后一行生效。行内可以用
`^` 转义控制颜色和点击区：

| 转义 | 含义 |
|---|---|
| `^c#RRGGBB^` / `^c#RRGGBBAA^` | 之后文字的前景色（8 位带透明度） |
| `^b#RRGGBB^` / `^b#RRGGBBAA^` | 之后文字的背景色 |
| `^d^` | 颜色复位 |
| `^sN!命令^` … `^e^` | 打开按钮 N 的点击区（1 左键、2 右键、3 中键、4/5 滚轮上/下） |
| `^s!命令^` | 同上，等价按钮 1 |
| `^^` | 字面的 `^` |

点击或滚轮落在点击区时，命令交给 `/bin/sh -c` 执行。无法识别的 `^` 原样显示。

```sh
# 每分钟刷新时间，左键点 "vol" 打开一个叫 mixer 的终端
( while :; do
    date '+^c#ffcc00^%H:%M^d^ ^s!footclient -T mixer^vol^e^'
    sleep 60
  done ) | nix-shell --run './target/debug/r0wm'
```

## 运行嵌套会话

配置使用 **niri 风格的 KDL**，默认路径 `~/.config/r0wm/config.kdl`，
完整可运行示例见 [`examples/config.kdl`](examples/config.kdl)：

```kdl
input {
    keyboard {
        xkb { layout "us,cn"; variant ","; options "grp:alt_shift_toggle"; }
    }
}

output "eDP-1" { scale 1.5; position x=0 y=0; primary }

layout {
    default "bsp"        // bsp | scroll | tabbed
    default-column-width { proportion 0.5; }
}

general {
    terminal "foot"
    autostart "foot"
    bar {
        enabled true; position "top"; font-size 12;
        // status-command "~/.local/bin/scroll-status"
    }
    titlebar { enabled true; padding 6; }
}

binds {
    Mod+Return { spawn "foot"; }
    Mod+Q { close-window; }
    Mod+1 { focus-workspace 1; }
}
```

`binds` 为空时使用内置表；写了自己的绑定就完全替换默认表。
KDL 里同时接受 `default-column-width` 和 `default_width` 这类写法，
niri 有而 r0wm 还没有的节点（gaps、border、window-rule）会被忽略。

通知默认 5 秒后消失（`critical` 不自动过期，客户端可在 1ms–60s 之间覆盖），
其余排队；关掉的通知留在 32 条历史里。总线不可用、或名字已被 mako/dunst 占用时
不抢占，只记一条警告，合成器照常运行。

```sh
nix-shell --run 'cargo run --bin r0wm -- -c /tmp/config.kdl'
```

r0wm 会打印自己创建的 `WAYLAND_DISPLAY`，并把子进程连到它上面；宿主会话里可以看到
一个初始 1280x800 的窗口（可随宿主拖拽缩放），里面是 r0wm 的工作区。

内置键绑定（配置 `binds` 为空时生效）：`super+Return` 启动终端、`super+h/l`
在行内切换列（bsp 下是几何邻居）、`super+j/k` 在 bsp 下按屏幕几何上下切换窗口、
其它布局下在可见行之间切换、`super+o` 循环切换显示器/输出焦点、
`super+Tab` 按最近使用顺序切焦点、`super+b` / `super+q` 切到 bsp / scroll 布局、
`super+v` 平铺/浮动互转、`super+f` 进/出全屏（全屏时 bar 自动隐藏）、
`super+s` 浮动窗口置顶（sticky：所有 tag 可见且永远在最上）、
`super+1` 只看 tag 1、**`super+左键` 在窗口任意位置拖动**（平铺窗口拖到别的格子
会交换位置，浮动窗口自由移动）、**`super+右键` / `super+中键` 从指针最近的角缩放**
（平铺窗口保持原布局：bsp 改分割比例，scroll 改列宽）、
scroll 布局下把多个窗口放进同一列：`super+shift+h` / `super+shift+l` 把焦点窗口
并入左/右相邻列，`super+shift+c` 再把 tab 拆成新列；`super+shift+t`
显示/隐藏窗口标题条、`super+shift+e` 重载配置、`super+shift+q` 退出；通知用
`super+n` 翻正文、`super+shift+n` 打开（触发 `default` 动作）、`super+ctrl+n` /
`super+alt+n` 在历史里前后走、`super+ctrl+shift+n` 全部关掉、`super+a` 展开动作选择器。
配置里写了 `binds` 就完全替换默认表；`tab_next`/`tab_prev`、`transfer_to`、
`toggle_tag`、`view_prev`、`shift_view`、`focus_output`、`toggle_sticky`、
`toggle_titlebar`、`close`（别名 `kill`，请求关闭焦点窗口）、`notify_open`、
`notify_dismiss` 等动作可在配置里自行绑定。

窗口标题条：无修饰左键点关闭按钮（悬停时出现在右端）关窗口，点标题其余位置拖动
窗口；tab 组里每个 tab 各占一条，点一下就切过去；标题上滚轮按最近使用顺序切焦点。

通知区（有通知时占据 bar 中间）：左键打开/触发默认动作，右键关掉；纵向滚轮在历史里
前后走，横向滚轮滚动长正文；`[r]` 提示左键展开动作选择器（左右/Tab 选择、回车执行、
Esc 关闭、1-9 直选）。历史浏览 5 秒没有输入会自动回到实时通知。

命令提示（`super+r`）：不写前缀时在 `.desktop` 应用里搜，`!!` 找 `$PATH` 里的命令、
`!` 在终端里跑、`=` 算数学式、`?` 打开搜索引擎、`@` 切窗口、`#` 切 tag、
`/` 或 `~/` 列目录。Tab 补全（再按一次换下一条，Shift+Tab 反向），上下键翻历史，
Ctrl+A/E/B/F/W/C 是 readline 风格的编辑；回车执行、Esc 收起；算式结果还留在提示里
可以接着算。候选按「匹配分 + 历史使用次数」排序，大写字母会让匹配变成大小写敏感。

托盘（bar 最右端）：左键触发 item 的 `Activate`，右键拉取它的 dbusmenu 在光标处
弹出；菜单支持子菜单（选中后原地换成下一层）、勾选项（`✓`/`☐`）、隐藏与禁用条目
过滤，点行外或 Esc 收起。没有提供图标的 item 用 `Id` 的首字母占位。
`org.kde.StatusNotifierWatcher` 被其它宿主占用时只记警告，不抢占。

bar 上的鼠标绑定沿用基线：左键 `view`、右键 `toggle_view`、`super`+左键把焦点窗口
移到该 tag、`super`+右键切换它的归属；tag 区的滚轮前后切换 tag，状态文本的滚轮走
该片段配置的命令。栏名（`[bsp]`）左键循环切换平铺布局。

## 文档

| 文件 | 内容 |
|---|---|
| [`CONTEXT.md`](CONTEXT.md) | 术语表（行、列、tab 组、行偏移、空间导航……） |
| [`docs/adr/`](docs/adr/) | 架构决策记录 |
| [`docs/parity.md`](docs/parity.md) | 相对基线的功能台账 |
| [`docs/protocols.md`](docs/protocols.md) | v1 实现的协议清单 |

阅读顺序建议：ADR 0001（定位）→ 0002（底座）→ 0003/0004/0005（布局模型）→
0009（通知的线程模型）→ CONTEXT.md 的术语。
