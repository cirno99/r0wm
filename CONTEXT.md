# CONTEXT

## 术语
- **对标基线（parity baseline）**：`.consult/G0wm` 的功能与交互集合，作为 r0wm v1
  「功能全等」的验收对象。区别于「参考实现」——基线是度量标准，不是抄写对象；
  r0wm 未实现的部分一律记为相对基线的欠账。
- **平铺布局（tiled layout）**：可切换的几何策略，决定平铺窗口如何摆放。r0wm 有
  `bsp`、`scroll`、`tabbed` 三个（ADR 0003 修订：默认 bsp，取代基线的 master/stack）。
  区别于**模式（mode）**——mode 只是 scroll 布局内部的方向（horizontal / vertical），
  不是布局，切换 mode 不换布局槽位。
- **tag**：dwl 式的归属标签，客户端用位掩码表示自己属于哪些 tag。一个 tag 对应
  一个**行**。区别于 workspace——r0wm 不引入独立于 tag 的工作区概念，说「工作区」
  就是指某个 tag。
- **归属行（home row）**：多 tag 窗口同一时刻只在一行里参与布局——当前行仍在它的
  掩码里就留在原行，否则取掩码里最低的可见 tag；一个都不可见时保留原行，切回来
  再显示。这样「一个窗口占两个 tag」不会在同一屏出现两份几何。
- **可见集合交替（view prev）**：`view`/`toggle_view` 每次替换可见集合时把旧集合
  存进第二个槽位，`view_prev` 在两个槽位间交换，等价基线的 `tagset[2]`。
- **行（row）**：一个 tag 的整条滚动带，沿主轴排列若干**列**。行是可滚动的，
  不保证内容全部可见。行按**输出**分组——每个输出为自己的 tagset 维护一套行状态
  （列序与行偏移），客户端只归属一个输出。
- **可见行（visible row）**：当前 `tagset` 中被选中的 tag 所对应的行。多个可见行
  在屏幕上垂直堆叠。
- **列（column）**：行内的水平单元，占行宽的一个比例，内含一个**tab 组**。列是
  scroll 布局唯一的滚动单位。
- **tab 组**：一列里的窗口集合，同位置重叠，只显示栈顶，标题条列出全部窗口。
  区别于**堆叠**——r0wm 没有并排的垂直堆叠，列内多窗口只能通过切换 tab 见到。
- **栈顶（top）**：tab 组里当前显示的那个窗口。
- **行偏移（row offset）**：一行自己的水平滚动位置，**每行独立**，切换 tag 后不丢失。
  滚动规则是最小滚动——焦点列已完整可见就不动，否则只滚刚好够的距离，贴边对齐。
- **空间导航（spatial navigation）**：方向键只解释为「该方向上的下一个东西」。
  `left`/`right` 在行内跨列（不跨行、不环绕），`up`/`down` 在可见行之间移动，
  `Tab`/`Shift+Tab` 在列内 tab 之间循环。区别于基线的**线性焦点链**——那条链遍历
  「行 → 列 → tab」，方向感由链的顺序决定，而不是由屏幕布局决定。
- **方向邻居（directional neighbor）**：bsp 没有列的概念，方向键退化为「目标矩形里，
  该方向上最近、且在另一轴有重叠的窗口」；没有正对邻居时按中心点方向取欧氏距离
  最近者。scroll 的左右仍按列移动；tabbed 是单列，左右不动。
- **平铺层 / 浮动层**：窗口分属两个层。平铺层的窗口参与行、列、tab 组与行偏移；
  浮动层的窗口有自持的位置尺寸，**不参与行列、不被行偏移带走**，绘制在平铺层之上、
  全屏窗口之下。
- **浮动（floating）**：窗口属于浮动层的状态。新窗口默认不平铺浮动，`togglefloating`
  双向切换；窗口进出浮动层时保留自身尺寸比例，回到平铺时按当前布局重新入列。
- **平铺交换（swap）**：拖动平铺窗口不改变它的层，而是与指针下的同 tag 窗口交换布局
  位置（i3/sway 式）；只能在同一行内交换。浮动窗口拖动则是自由移动 + 吸附。
- **冻结（freeze）**：焦点落到浮动窗口时，平铺侧的行焦点与行偏保持原样不动，
  方向键在浮动窗口上不动作；离开浮动层时原样恢复。
- **sticky（置顶浮动窗口）**：浮动窗口的一个状态——不属于任何可见集合，在**所有 tag**
  都映射，并且永远排在其他窗口之上（包括全屏）。`toggle_sticky` 双向切换；窗口回到
  平铺层时 sticky 自动取消。与**冻结**的区别：冻结说的是焦点导航，sticky 说的是
  可见性与层序。
- **bar（状态栏）**：合成器自己绘制的、**挤占工作区**的条（不是浮层），每个输出一
  根，可在输出顶部或底部。它不是 layer-shell 客户端：文本与色块在合成器内光栅化成
  一块位图，作为渲染元素叠在窗口之上；工作区由合成器自己预留。
- **状态行转义（status escapes）**：bar 右侧状态文本的行内语法，沿用基线的
  `statusescape`。`^c#RRGGBB(AA)^` / `^b#RRGGBB(AA)^` 设置之后文字的前景/背景色，
  `^d^` 复位，`^sN!命令^` 打开按钮 N 的点击区（`^e^` 关闭，`^^` 表示字面的 `^`）。
  点击或滚轮落在点击区时，命令交给 `/bin/sh -c`；无法识别的 `^` 原样显示。
- **标题条（title bar）**：合成器为每个窗口自绘的一条位图，贴在窗口内容上方。
  平铺窗口从目标矩形顶部扣掉一条，浮动窗口整体下移一条，全屏窗口没有。tab 组把
  这一条横向平分给组内每个窗口（基线 tabbed 的「标题并排当 tab」），选中 tab 用
  高亮底色与左边缘指示条；点一个 tab 就聚焦它并把它提到栈顶。指针悬停时右端出现
  关闭按钮，左键点击关闭窗口，点标题其余位置开始拖动（平铺窗口拖到别的格子会交换）。
  标题条的高度与配色不随 `bar_enabled` 变化，由 `titlebar` / `title_*` 配置控制。
- **行可用高（row area）**：输出高度扣掉 bar 之后的剩余高度；多行同时可见时，
  这个高度再在行之间等分。所有「比例」都以行可用高/行可用宽为基准。
- **动作表（action table）**：一张把「动作名 + 参数」映射到具体操作的登记表。键绑定、
  IPC 命令、bar 点击全部走它，不存在第二套解析器。
- **目标矩形（target rect）**：布局计算的唯一产物——窗口/列/bar 应当在屏幕上占据的
  矩形，纯数据、不接触渲染。当前是否要插值动画是**渲染层**的问题，布局层不感知。
- **欠账（parity gap）**：`docs/parity.md` 里状态不是「完成」的基线功能条目。欠账
  只能靠 r0wm 自己补齐，不允许引入外部组件抵消。
- **比例（fraction）**：窗口或列尺寸的表达方式，相对其所在行/列的**可用空间**
  （不是相对屏幕），取值 (0, 1]。多行同屏时比例语义不变。
- **档位（size step）**：配置里给出的一组常用比例（默认 1/3、1/2、2/3、1），
  `cycle_size` 在档位间循环；`resize` 可以产生不落在档位上的自由比例。
- **焦点栈（focus stack）**：窗口的最近使用顺序，栈顶即当前焦点。浮动窗口没有行列
  位置可导航，`focus_next`/`focus_prev` 按这个顺序循环；平铺窗口的焦点变化也会把它
  推到栈顶，所以从浮动回到平铺不会丢上下文。
- **级联（cascade）**：新浮动窗口的初始摆放方式——工作区居中后按固定步长（32 像素，
  走满 5 步回绕）依次错开，避免多个未平铺窗口完全重叠。
- **layer 键盘焦点（Exclusive / OnDemand）**：layer-shell surface 的两档键盘交互。
  `Exclusive` 在映射时直接拿键盘（锁屏、菜单）；`OnDemand` 在指针点击它时拿键盘，
  点到别处归还。持有期间窗口焦点变化（重排、切 tag）不会把键盘抢回去。
- **光标热点（hotspot）**：光标位图里「真正的指针位置」那一点。渲染时要先把热点从
  位图坐标缩回逻辑坐标，再从指针位置里扣掉，否则光标会整体偏移。
- **分数缩放（fractional scale）**：输出的缩放系数不是整数（1.5、1.25）。布局、窗口
  几何与指针坐标始终用逻辑像素；`wl_output` 只能报整数（向上取整），支持
  `wp_fractional_scale_v1` 的客户端从 `preferred_scale` 拿到精确的 120 分之一单位值。
  每个 surface 的缩放由 `State::send_fractional_scale` 决定，输出缩放变化时广播给所有
  顶层与 layer。`preferred_scale` 的单位是 1/120：1.5 倍 = 180，1.0 倍 = 120。
- **popup grab**：菜单/下拉框这类 xdg_popup 的输入接管。客户端显式请求
  （`xdg_popup.grab`）时，指针与键盘 grab 一起上，点到 popup 之外或 popup 被销毁就
  dismiss；客户端没请求时，r0wm 在每次按下前看指针是否在 popup 的几何矩形内，不在就
  把该根 surface 的 popup 全部 dismiss（wlroots 的隐式 grab）。
- **通知（notification）**：r0wm 在会话总线上实现 `org.freedesktop.Notifications`，
  一次只在 bar 中间显示一条；其余排队，关闭（含过期）的进最多 32 条历史。状态机
  `notify::Store` 是纯数据（时间由调用者传入），D-Bus 连接与它在独立线程里，通过
  calloop channel 把快照推给渲染线程。客户端 id、`replaces_id` 与
  `x-dunst-stack-tag`（同 app + 同 tag）都指向同一槽位，原地更新；`critical` 不自动
  过期并会抢占屏幕；`CloseNotification` 不进历史，其余关闭都进；`transient` 哪都不进。
  「倒计时开始」= `shown_ms`，历史浏览与动作选择器（hold）都暂停它。
- **图标查找（icon lookup）**：通知的 `image-path`/`image_path` 与 `app_icon` 可以是
  绝对路径、`file://` URI 或图标名。名字按基线的 `iconfile()` 顺序查 `hicolor` 主题
  （`<data>/icons/hicolor/<N>x<N>/apps/<name>.png`，尺寸顺序 24/32/22/48/64/16/128/256，
  再退到 `scalable/apps/<name>.svg` 与 `<data>/pixmaps/<name>.png|svg`），不解析
  `index.theme`、不做主题继承。解码后缩到方框内（文件图标允许放大，`image-data` 只缩
  不放），结果（含 miss）缓存在 `ui::icon`。
- **动作选择器（action picker）**：有动作的通知右侧显示 `[r]`，展开后动作标签排成
  可点的芯片，同时通知进入 hold（计时暂停、新通知只排队）。它独占键盘：左右/Tab
  移动、回车/空格执行、Esc 关闭、1-9 直选；点通知正文其他位置也会先关掉它。
- **历史浏览（notification history view）**：在实时通知上按 `notify_prev` 打开最新
  一条历史，`notify_next` 向新走、`notify_dismiss` 删掉当前条目；5 秒没有输入（或
  实时通知换了）自动回到实时视图。浏览期间进度条画成半透明的满格。
- **命令提示（runner）**：`super+r` 打开、独占键盘的输入行，占据 bar 中间（优先级高于
  通知）。打开时才按目录 mtime 决定是否重扫 `.desktop` 与 `$PATH`，之后每次按键只过滤
  内存里的数组；输入行变更就重新过滤，Tab 补全时列表不动。
- **模式前缀（mode prefix）**：`!!` 命令、`!` 终端、`=` 算式、`?` 搜索、`@` 窗口、
  `#` tag，空串即关掉该模式；最长匹配胜出，认到前缀后跳过其后的空白。没有前缀时是
  应用模式；`/` 或 `~/` 开头直接进文件模式（路径支持 `~` 展开）。
- **候选打分（candidate score）**：完全相等最高；`runner_fuzzy` 打开时要求查询的字母
  按顺序出现，名字开头、词首（`-_ .` 之后）、连续命中加分，同分时短名字靠前；查询里
  有一个大写字母就变成大小写敏感。分数再加上「历史使用次数 × 16」，历史越新权重越大
  （最新是最旧的 5 倍），应用按显示名计、命令按第一个词计。
- **历史（runner history）**：`$XDG_STATE_HOME/r0wm/runner_history`（或
  `~/.local/state/...`），旧的在文件前面；上下键像 shell 一样翻并跳过重复项，翻过最新
  一条会清空输入。
- **候选胶囊（candidate pill）**：选中候选的圆角底色，几何照基线 `drawcandidates`：
  高度默认 4/3 行高（`runner_pill_height`）、圆角是半高乘比例（`runner_pill_round`，
  负数按全圆）、左右内边距 `runner_pill_padding` 会自动放大到避开圆弧；非选中候选
  用灰字，不画底色。
- **图标占位（icon holder）**：`runner_icons = "all"` 时每条候选左侧都留图标位；
  候选没有图标、但应用池里别处有图标时画一个圆角方块（边框与底色照基线的
  `SchemeRunnerIcon`，圆角比例 `runner_icon_round`、边框 `runner_icon_border`）。
- **托盘（systray）**：r0wm 在会话总线上实现 `org.kde.StatusNotifierWatcher`，
  item 通过 `RegisterStatusNotifierItem` 注册（参数可以是对象路径、唯一名或
  well-known name，后两者用 `/StatusNotifierItem` 作默认路径）。名字从总线上消失
  （`NameOwnerChanged` 的 new_owner 为空）就移除对应图标。图标优先取 `IconPixmap`
  里第一个宽 ≥22 的位图（网络字节序 ARGB32，换成 RGBA），没有就按
  `IconThemePath`/`IconName` 查 hicolor；都没有时画 `Id` 的首字母。图标在 bar
  最右端、状态文本的左/右侧留白之外。
- **托盘菜单（dbusmenu）**：右键图标时先 `AboutToShow(0)` 再 `GetLayout(0, -1, [])`，
  把 `(id, a{sv}, av)` 递归解析成节点；隐藏、禁用、无 label 的条目丢弃，勾选项加
  `☐`/`✓` 前缀，子菜单追加 `→`，标签去掉 mnemonics 下划线。菜单在光标处弹出并夹
  进输出范围；点行发 `Event(id, "clicked", v"", 时间戳)`，点子菜单则原地换成下一层，
  点行外或 Esc 发 `-1` 取消。
- **真机后端（udev）**：用 libseat 拿到会话（`seatd` 或 systemd-logind），按主 GPU
  打开 `/dev/dri/card*`，用 GBM + EGL 建渲染器，为第一个连上的连接器建 `DrmOutput`
  （KMS 直出：能直扫就直扫，不能就合成后送 primary plane）。输入走 libinput，
  与 winit 共用 `State::process_input_event`。输出热插拔由 `smithay-drm-extras`
  的连接器扫描发现；会话挂起时 `pause`、恢复后 `activate` 并重画。
- **输出（output）**：一台显示器。每个输出持有自己的 `WorkspaceSet`（可见 tag 与每行偏移）、
  工作区、bar 与标题条；窗口通过 `ManagedWindow::output` 只归属一个输出。指针跨输出
  移动时，键盘/动作的默认目标（`State::output`）跟着指针切换。
- **输入法（IME）**：合成器同时实现 `zwp_text_input_manager_v3`（客户端侧）与
  `zwp_input_method_manager_v2`（输入法侧）。键盘焦点切换时由 Smithay 自动同步
  text-input 的 enter/leave；输入法的预编辑窗口按普通 popup 跟踪、约束与渲染。
  中文输入法（fcitx5/ibus）作为普通 Wayland 客户端连接即可，不需要外部合成器组件。
- **KDL 配置**：niri 风格的 `input` / `output` / `layout` / `general` / `binds` 节点；
  默认路径 `~/.config/r0wm/config.kdl`。r0wm 不认识的节点忽略并记 debug 日志，
  解析失败保留上一份有效配置。
