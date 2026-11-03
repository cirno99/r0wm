//! 动作执行：键绑定、IPC 命令与 bar 点击共用这一处入口（`docs/adr/0007`）。
//!
//! 所有动作最终都落到三件事：改 `Shell` 的层/tag/布局状态、改焦点栈、
//! 调 [`State::arrange`] 重排。布局几何本身在 `crate::layout` 里，不碰 Smithay。

use std::process::Command;

use crate::{
    action::Action,
    config::Config,
    layout::{Direction, TiledLayout},
    shell::Layer,
};

use super::state::State;

impl State {
    pub fn execute(&mut self, action: Action) {
        tracing::debug!(?action, "执行动作");
        match action {
            Action::Quit => {
                tracing::info!("收到退出请求");
                self.loop_signal.stop();
            }
            Action::Spawn(argv) => spawn_command(&argv),
            Action::Close => {
                if let Some(id) = self.shell.focus() {
                    self.close_window(id);
                }
            }
            Action::FocusNext => self.cycle_focus(1),
            Action::FocusPrev => self.cycle_focus(-1),
            Action::ToggleFloating => self.toggle_floating(),
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            Action::ToggleSticky => self.toggle_sticky(),
            Action::Consume(direction) => self.consume_column(direction),
            Action::Expel => self.expel_window(),
            Action::ToggleTitlebar => self.toggle_titlebar(),
            Action::NotifyOpen => self.notify_open(),
            Action::NotifyDismiss => self.notify_dismiss(),
            Action::NotifyDismissAll => self.notify_dismiss_all(),
            Action::NotifyNext => self.notify_next(),
            Action::NotifyPrev => self.notify_prev(),
            Action::NotifyScroll => self.notify_scroll(),
            Action::NotifyActions => self.notify_actions(),
            Action::RunnerToggle => self.runner_toggle(),
            Action::Reload => self.reload_config(),
            Action::View(mask) => {
                let output = self.focused_output_name();
                self.shell.set_visible_tags(&output, mask);
                self.after_focus_change();
            }
            Action::ToggleView(mask) => {
                let output = self.focused_output_name();
                self.shell.toggle_visible_tags(&output, mask);
                self.after_focus_change();
            }
            Action::TransferTo(mask) => self.transfer_focus(mask),
            Action::ToggleTag(mask) => self.toggle_tag(mask),
            Action::ViewPrev => {
                let output = self.focused_output_name();
                if self.shell.swap_visible_tags(&output) {
                    self.after_focus_change();
                }
            }
            Action::ShiftView(step) => {
                let output = self.focused_output_name();
                if self.shell.shift_view(&output, step) {
                    self.after_focus_change();
                }
            }
            Action::SetLayout(layout) => {
                let output = self.focused_output_name();
                self.shell.set_layout(&output, layout);
                tracing::info!(?layout, "切换平铺布局");
                self.after_focus_change();
            }
            Action::FocusColumnLeft => self.focus_direction(Direction::Left),
            Action::FocusColumnRight => self.focus_direction(Direction::Right),
            Action::FocusRowUp => self.focus_vertical(-1),
            Action::FocusRowDown => self.focus_vertical(1),
            Action::TabNext => self.cycle_tab(1),
            Action::TabPrev => self.cycle_tab(-1),
            Action::FocusOutput => self.focus_next_output(),
        }
    }

    /// scroll 布局：把焦点窗口并入相邻列，成为该列 tab 组的新栈顶。
    fn consume_column(&mut self, direction: Direction) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        if self.shell.consume_tiled(id, direction) {
            self.shell.set_focus(id);
            self.after_focus_change();
            tracing::info!(id, ?direction, "窗口并入相邻列");
        } else {
            tracing::debug!(id, ?direction, "当前布局/列不支持并入");
        }
    }

    /// scroll 布局：把焦点 tab 从当前列拆出来，放到右侧新列。
    fn expel_window(&mut self) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        if self.shell.expel_tiled(id) {
            self.shell.set_focus(id);
            self.after_focus_change();
            tracing::info!(id, "窗口拆到新列");
        }
    }

    /// 按最近使用顺序循环焦点。
    fn cycle_focus(&mut self, step: isize) {
        let output = self.focused_output_name();
        let Some(id) = self.shell.cycle_focus(&output, step) else {
            return;
        };
        self.shell.set_focus(id);
        self.after_focus_change();
    }

    /// 焦点窗口进入/退出全屏。全屏时 bar 隐藏，窗口占满整个输出。
    fn toggle_fullscreen(&mut self) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        let Some(entry) = self.shell.find(id) else {
            return;
        };
        let layer = entry.layer;
        let output = entry.output.clone();
        let restore = entry.fullscreen_restore.clone();

        if layer == Layer::Fullscreen {
            let (target, location) = restore.unwrap_or((Layer::Tiled, None));
            if !self.shell.set_layer(id, target) {
                return;
            }
            if let Some(entry) = self.shell.find_mut(id) {
                entry.fullscreen_restore = None;
                entry.floating_loc = location;
                entry.configured_size = None;
            }
            tracing::info!(id, ?target, "退出全屏");
        } else {
            let area = self
                .outputs
                .iter()
                .find(|candidate| candidate.name() == output)
                .and_then(|output| self.space.output_geometry(output))
                .map(|geo| (geo.loc.x, geo.loc.y, geo.size.w, geo.size.h))
                .unwrap_or((
                    self.work_area.x,
                    self.work_area.y,
                    self.work_area.width,
                    self.work_area.height,
                ));
            let location = self.shell.find(id).and_then(|entry| entry.floating_loc);
            if !self.shell.set_layer(id, Layer::Fullscreen) {
                return;
            }
            if let Some(entry) = self.shell.find_mut(id) {
                entry.fullscreen_restore = Some((layer, location));
                entry.floating_loc = Some((area.0, area.1));
                entry.configured_size = None;
            }
            tracing::info!(id, "进入全屏");
        }
        self.shell.set_focus(id);
        self.arrange();
    }

    /// 浮动层与平铺层之间切换。
    fn toggle_floating(&mut self) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        let Some(entry) = self.shell.find(id) else {
            return;
        };
        let target = match entry.layer {
            Layer::Tiled | Layer::Fullscreen => Layer::Floating,
            Layer::Floating => Layer::Tiled,
        };

        if !self.shell.set_layer(id, target) {
            return;
        }
        if target == Layer::Floating {
            self.place_floating(id);
        }
        self.shell.set_focus(id);
        self.after_focus_change();
        tracing::info!(id, ?target, "切换窗口层");
    }

    /// 浮动窗口置顶（sticky）：永远可见且永远在最上层。
    fn toggle_sticky(&mut self) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        match self.shell.toggle_sticky(id) {
            Some(sticky) => {
                tracing::info!(id, sticky, "切换 sticky");
                self.arrange();
            }
            None => tracing::warn!(id, "只有浮动窗口可以置顶（sticky）"),
        }
    }

    /// 显示/隐藏每窗口标题条（基线 `toggletitlebar`）。
    fn toggle_titlebar(&mut self) {
        let enabled = !self
            .title_bars
            .values()
            .next()
            .is_some_and(|titles| titles.is_enabled());
        for titles in self.title_bars.values_mut() {
            titles.set_enabled(enabled);
        }
        tracing::info!(enabled, "切换窗口标题条");
        self.arrange();
    }

    fn focus_direction(&mut self, direction: Direction) {
        let output = self.focused_output_name();
        let area = self.focused_work_area();
        if self.shell.focus_column(&output, direction, area).is_some() {
            self.after_focus_change();
        }
    }

    /// 上下方向导航：bsp 先按屏幕几何找上方/下方的邻居；只有在没有几何
    /// 邻居时才回退到「切换可见行」。scroll/tabbed 仍然只按行切换。
    fn focus_vertical(&mut self, step: isize) {
        let output = self.focused_output_name();
        let tag = self.shell.focused_tag(&output);
        if self.shell.layout_of(&output, tag) == TiledLayout::Bsp {
            let direction = if step < 0 {
                Direction::Up
            } else {
                Direction::Down
            };
            let area = self.focused_work_area();
            if self.shell.focus_column(&output, direction, area).is_some() {
                self.after_focus_change();
                return;
            }
        }
        if self.shell.focus_row(&output, step).is_some() {
            self.after_focus_change();
        }
    }

    /// 焦点按连接顺序循环到下一个输出，并把该输出上的窗口设为当前焦点。
    fn focus_next_output(&mut self) {
        if self.outputs.len() < 2 {
            return;
        }
        let current = self.focused_output_name();
        let index = self
            .outputs
            .iter()
            .position(|output| output.name() == current)
            .unwrap_or(0);
        let next = (index + 1) % self.outputs.len();
        let name = self.outputs[next].name();
        self.focus_output(&name);

        let visible = self.shell.visible_tags(&name);
        match self.shell.fallback_focus(&name, visible) {
            Some(id) => self.shell.set_focus(id),
            None => self.shell.clear_focus(),
        }
        self.after_focus_change();
        tracing::info!(output = %name, "切换输出焦点");
    }

    fn cycle_tab(&mut self, step: isize) {
        let output = self.focused_output_name();
        if self.shell.cycle_tab(&output, step).is_some() {
            self.after_focus_change();
        }
    }

    fn transfer_focus(&mut self, mask: u32) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        if self.shell.transfer(id, mask).is_some() {
            self.after_focus_change();
        }
    }

    /// 切换焦点窗口的 tag 归属（`toggletag`）。
    fn toggle_tag(&mut self, mask: u32) {
        let Some(id) = self.shell.focus() else {
            return;
        };
        if self.shell.toggle_tags(id, mask) {
            self.after_focus_change();
        }
    }

    /// 焦点可能已经变了：同步 tag 行与滚动位置，再重排。
    pub(crate) fn after_focus_change(&mut self) {
        let output = self.focused_output_name();
        let area = self.focused_work_area();
        self.shell.note_focus(&output, area);
        self.arrange();
    }

    /// 热重载配置：失败时保留上一份有效配置（ADR 0006）。
    fn reload_config(&mut self) {
        match Config::load(&self.config_path) {
            Ok(config) => {
                self.binds = crate::input::Binds::from_config(&config);
                self.shell.set_default_layout(config.layout.default.into());
                for titles in self.title_bars.values_mut() {
                    titles.reconfigure(&config.general);
                }
                for bar in self.bars.values_mut() {
                    bar.reconfigure(&config.general);
                }
                self.runner.set_config(config.general.runner_config());
                if let Some(handle) = self.notify.as_ref() {
                    handle.set_timeout(config.general.notification_timeout);
                }
                if let Some(handle) = self.tray.as_ref() {
                    // 主题图标按物理像素解码；bar 绘制时还会再缩进格子。
                    let scale = self
                        .output
                        .as_ref()
                        .map(|output| output.current_scale().fractional_scale())
                        .unwrap_or(1.0);
                    handle.set_icon_size(
                        (config.general.systray_icon_size.max(16) as f64 * scale).round() as u32,
                    );
                }
                self.config = config;
                tracing::info!(path = %self.config_path.display(), "配置已重载");
                self.reapply_output_config();
                self.arrange();
            }
            Err(err) => {
                tracing::warn!(path = %self.config_path.display(), "配置重载失败，保留旧配置：{err:#}");
            }
        }
    }
}

/// 启动时执行配置里的 autostart。
pub fn run_autostart(state: &State) {
    for line in &state.config.general.autostart {
        let mut parts = line.split_whitespace();
        let Some(program) = parts.next() else {
            continue;
        };
        let args: Vec<&str> = parts.collect();
        spawn_reaped(Command::new(program).args(&args), line);
    }
}

/// 按 argv 直接启动，不经过 shell（动作表约定）。
pub fn spawn_command(argv: &[String]) {
    let Some((program, rest)) = argv.split_first() else {
        return;
    };
    spawn_reaped(Command::new(program).args(rest), program);
}

/// 交给 `/bin/sh -c` 执行（状态文本点击区的命令是 shell 行）。
pub fn spawn_shell(command: &str) {
    spawn_reaped(Command::new("/bin/sh").arg("-c").arg(command), command);
}

/// 用配置里的 shell 执行一行命令；`terminal` 有值时套一层终端。
///
/// 与基线 `runnerexec` 一致：终端模式下命令作为 `-c` 的参数传给终端，
/// 而不是拼进 shell 字符串，所以命令里的引号不会被二次解释。
pub fn spawn_line(shell: &str, command: &str, terminal: Option<&str>, path: Option<&str>) {
    let mut child = Command::new(if shell.is_empty() { "/bin/sh" } else { shell });
    match terminal {
        Some(terminal) if !command.is_empty() => {
            child
                .arg("-c")
                .arg(format!("exec {terminal} \"$0\" -c \"$1\""))
                .arg(shell)
                .arg(command);
        }
        Some(terminal) => {
            child.arg("-c").arg(format!("exec {terminal}"));
        }
        None => {
            child.arg("-c").arg(command);
        }
    }
    if let Some(path) = path {
        child.env("PATH", path);
    }
    spawn_reaped(&mut child, command);
}

/// 用系统默认程序打开文件/URL（基线 `runneropen`）。
pub fn spawn_open(target: &str) {
    spawn_reaped(Command::new("xdg-open").arg(target), target);
}

/// 启动子进程并在后台线程里 `wait`，避免留下僵尸进程。
fn spawn_reaped(command: &mut Command, what: &str) {
    match command.spawn() {
        Ok(mut child) => {
            tracing::info!(what, "已启动命令");
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(what, "命令启动失败：{err}"),
    }
}
