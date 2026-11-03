# v1 协议清单

按 ADR 0007 与该轮决策：只实现「基线已用 + 生态刚需」的协议，清单外留 v2。
实现由 Smithay 提供协议骨架，r0wm 负责焦点、生命周期与布局联动。

## 核心

| 协议 | 用途 |
|---|---|
| xdg-shell | 顶级窗口与 popup |
| xdg-decoration | 客户端自绘 / 服务端绘制的标题条协商 |
| xdg-activation | 应用请求激活窗口（通知点击、启动器） |
| xdg-output | 输出逻辑尺寸描述 |
| layer-shell | 面板、锁屏、壁纸类客户端（bar 自身也是 layer-shell 客户端） |

## 绘图与显示

| 协议 | 用途 |
|---|---|
| wayland shm | 基础缓冲 |
| linux-dmabuf | 零拷贝缓冲 |
| presentation-time | 帧呈现反馈 |
| viewporter | 缩放裁剪 |
| fractional-scale-v1 | 分数缩放 |
| wlr-screencopy / ext-image-copy | 截图与录屏 |
| wlr-output-management | 分辨率/刷新率/位置/缩放 |
| wlr-gamma-control | 色温调节（wlsunset/gammastep） |
| wlr-output-power-management | 显示器开关 |
| xdg-output-management（若 Smithay 已支持） | 新版输出管理 |

## 输入

| 协议 | 用途 |
|---|---|
| 键盘/指针/触摸 | seat 基础能力 |
| pointer-constraints / relative-pointer / pointer-warp | 游戏与远程桌面 |
| idle-inhibit / idle-notify | 阻止与响应空闲 |
| keyboard-shortcuts-inhibit | 全屏应用捕获快捷键（基线 inhibit） |
| text-input-v3 / input-method-v2 | 输入法（fcitx5、ibus） |
| tablet-v2 | 数位板 |
| data-device + primary-selection | 复制粘贴与拖放 |

## 会话与互操作

| 协议 | 用途 |
|---|---|
| session-lock | 锁屏 |
| foreign-toplevel | 外部工具/portal 枚举窗口 |
| xwayland-shell | X11 窗口（ADR：Xwayland 进 v1） |

## 明确留到 v2

- color-management-v1 / color-representation-v1 / HDR 输出
- 任何仍处于实验期、Smithay 尚无稳定实现的协议

## 当前进度（M4 起步）

已注册的全局：`wl_compositor` / `wl_subcompositor`、`wl_shm`、`wl_seat`（键盘 + 指针）、
`wl_output` + `zxdg_output_manager_v1`、`wl_data_device_manager`、
`xdg_wm_base`（toplevel/popup）、`wp_pointer_constraints`（仅骨架）、
`zxdg_decoration_manager_v1`、`xdg_activation_v1`、`zwlr_layer_shell_v1`、
`wp_viewporter`、`wp_presentation`、`zwp_linux_dmabuf_v1`（v3）、
`wp_fractional_scale_manager_v1`、`zwp_text_input_manager_v3`、
`zwp_input_method_manager_v2`、`zwp_virtual_keyboard_manager_v1`。

窗口生命周期已含 `toplevel_destroyed`、`title_changed` / `app_id_changed`（推 IPC 事件）。
layer-shell 已接入映射、exclusive zone → 工作区、初始 configure 与指针命中；
`Exclusive` 键盘在映射时拿焦点、`OnDemand` 在点击时拿焦点，layer 销毁或点到别处后
键盘还给窗口。xdg-decoration 固定协商服务端装饰，标题条由合成器自绘；xdg-activation 在 10 秒内的 token
会切到对应 tag 并聚焦。presentation-time 在每帧提交后回 `presented`。
popup 的显式 grab（菜单、下拉框）会同时上指针与键盘 grab：指针焦点跟到 popup 所在
客户端，点到别处 `PopupPointerGrab` 自动 dismiss 全部 popup；客户端销毁 popup 时
`PopupManager::cleanup` 收尾。客户端没请求 grab 的 popup 也有隐式「点外关闭」：
每次按下前先看指针是否落在任一 popup 的几何矩形里，不在就 `dismiss_popup` 全部 popup，
再照常把事件交给下面的 surface。popup 的边缘约束分两条：窗口根用窗口几何，layer 根用
layer 在输出里的几何，两者都扣掉 popup 链偏移后再交给 positioner。

linux-dmabuf 用后端渲染器的 `dmabuf_formats()` 注册，导入回调把 buffer 试导入
`GlesRenderer` 后回执成功/失败；`wl_shm` 的格式列表也来自同一个渲染器。
光标由 `src/ui/cursor.rs` 按 `XCURSOR_THEME` / `XCURSOR_SIZE` 渲染，客户端自绘光标
（`wl_pointer.set_cursor`）走同一条渲染路径。

分数缩放：输出把物理尺寸与缩放分开记（`Mode.size` 是物理像素，`Output::current_scale`
是缩放），工作区、layer zone 与窗口布局全部用逻辑像素；渲染时由 Smithay 按输出缩放
换算。客户端建 `wp_fractional_scale_v1` 时立刻收到 `preferred_scale`（单位 1/120），
输出缩放变化后 `refresh_surface_scales` 再广播给所有顶层与 layer。不支持该协议的
客户端按 `wl_output.scale` 的整数（向上取整）渲染。

输入法：`zwp_text_input_manager_v3` 给客户端，`zwp_input_method_manager_v2` 给
fcitx5/ibus；Smithay 在键盘焦点切换时同步 text-input 的 enter/leave，输入法的预编辑
窗口按普通 popup 跟踪、约束与渲染。另注册 `zwp_virtual_keyboard_manager_v1` 作为
不支持 text-input 客户端的兜底注入通道，按键直接转发给焦点客户端、不经过键绑定。

多输出：`Space` 按输出映射窗口；`State::outputs` 保存连接顺序，每个输出有自己的
`WorkspaceSet`、工作区、bar、标题条与 DRM 表面。指针按整张桌面的包围盒移动，
落在哪个输出就把动作目标切到哪个输出。

下一批（M4）：bar（layer-shell 客户端）与 tab 标题条、udev 真机后端。
