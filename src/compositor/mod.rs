//! 合成器主体：把各子系统拼起来。
//!
//! M1 目标（见共识里的切片）：winit 嵌套启动、单输出、GL 渲染、xdg-shell、
//! 键鼠输入、浮动窗口能开能关能拖。平铺与 IPC 从 M2 起叠上来。

pub mod state;

mod actions;
mod focus;
mod grabs;
mod handlers;
mod input;

#[cfg(feature = "xwayland")]
mod xwayland;

pub use actions::run_autostart;
pub use focus::KeyboardFocusTarget;
pub use state::State;

use crate::cli::{Backend, Options};

/// 读 stdin 的状态文本，喂给 bar 右侧。
///
/// 基线 g0wm 的 bar 状态行就是合成器 stdin 上的最后一行（`src/bar.c` 的
/// `checkstatus`）。这里用线程 + calloop channel，避免把 stdin 设成非阻塞。
/// stdin 是终端时不启用，免得抢走交互输入。
pub fn init_status_reader(event_loop: &mut calloop::EventLoop<'static, State>) {
    use std::io::{BufRead, IsTerminal};

    if std::io::stdin().is_terminal() {
        return;
    }

    let (sender, receiver) = calloop::channel::channel::<String>();
    if event_loop
        .handle()
        .insert_source(receiver, |event, _, state| {
            if let calloop::channel::Event::Msg(line) = event {
                if state.status_text != line {
                    state.status_text = line;
                    state.request_redraw();
                }
            }
        })
        .is_err()
    {
        tracing::warn!("状态事件源注册失败，bar 状态文本不可用");
        return;
    }

    let spawned = std::thread::Builder::new()
        .name("r0wm-status".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("状态读取线程启动失败：{err}");
    }
}

/// 运行 bar 的 `status_command`，把 stdout 的每一行喂给右侧状态文本。
///
/// 与 stdin 版本二选一即可；配置了命令就以命令为准（配置更明确）。
pub fn init_status_command(event_loop: &mut calloop::EventLoop<'static, State>, command: &str) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let (sender, receiver) = calloop::channel::channel::<String>();
    if event_loop
        .handle()
        .insert_source(receiver, |event, _, state| {
            if let calloop::channel::Event::Msg(line) = event {
                if state.status_text != line {
                    state.status_text = line;
                    state.request_redraw();
                }
            }
        })
        .is_err()
    {
        tracing::warn!("状态命令事件源注册失败，bar 状态文本不可用");
        return;
    }

    let command = command.to_string();
    let spawned = std::thread::Builder::new()
        .name("r0wm-status-command".into())
        .spawn(move || {
            let child = Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(err) => {
                    tracing::warn!(%command, "启动 bar 状态命令失败：{err}");
                    return;
                }
            };
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // 协议头与流框架行返回 None，直接跳过。
                let Some(line) = normalize_status_line(&line) else {
                    continue;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
            let _ = child.wait();
        });
    if let Err(err) = spawned {
        tracing::warn!("状态命令线程启动失败：{err}");
    }
}

/// 兼容 i3bar/sway 的 `status_command` 输出。
///
/// 很多现成脚本（包括 scroll-status）输出的是 i3bar 的 JSON 数组；这里把它
/// 转成 r0wm bar 认识的颜色转义文本。不是 JSON 的行原样透传。
///
/// 需要额外容忍 scroll-status 的写法：协议头之后单独写一行 `[`，并且给每个数组
/// 加尾逗号（它把输出当成一个持续写入的外层数组）。这些行不是状态内容，返回
/// `None` 跳过；否则 bar 会把协议头或一整个 JSON 数组当成正文画出来。
fn normalize_status_line(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Block {
        full_text: String,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        background: Option<String>,
    }

    let trimmed = line.trim();

    // i3bar 协议头，以及 scroll-status 包住整个流的外层框架行。
    if is_i3bar_header(trimmed) || matches!(trimmed, "[" | "]" | "],") {
        return None;
    }

    // scroll-status 给每个数组加了尾逗号，解析前先去掉。
    let payload = trimmed.strip_suffix(',').unwrap_or(trimmed);
    if !(payload.starts_with('[') && payload.ends_with(']')) {
        return Some(line.to_string());
    }
    let Ok(blocks) = serde_json::from_str::<Vec<Block>>(payload) else {
        return Some(line.to_string());
    };

    let mut out = String::new();
    for (index, block) in blocks.iter().enumerate() {
        // 前景色与背景色可以同时存在，都要带上。
        if let Some(color) = block.color.as_deref() {
            out.push_str(&format!("^c{color}^"));
        }
        if let Some(background) = block.background.as_deref() {
            out.push_str(&format!("^b{background}^"));
        }
        out.push_str(&block.full_text.replace('^', "^^"));
        out.push_str("^d^");
        if index + 1 < blocks.len() {
            out.push_str("  ");
        }
    }
    Some(out)
}

/// 判断一行是不是 i3bar 的 JSON 协议头（`{"version":1,...}`）。
fn is_i3bar_header(line: &str) -> bool {
    if !line.starts_with('{') {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .map(|object| object.contains_key("version"))
        })
        .unwrap_or(false)
}

/// 启动合成器。阻塞运行事件循环，直到退出。
pub fn run(options: Options) -> anyhow::Result<()> {
    match options.backend {
        Backend::Winit => run_nested(options),
        Backend::Udev => run_udev(options),
    }
}

#[cfg(feature = "winit")]
fn run_nested(options: Options) -> anyhow::Result<()> {
    crate::backend::winit::run(options)
}

#[cfg(not(feature = "winit"))]
fn run_nested(_options: Options) -> anyhow::Result<()> {
    anyhow::bail!("本二进制未编入 winit 嵌套后端（--features winit）")
}

#[cfg(feature = "udev")]
fn run_udev(options: Options) -> anyhow::Result<()> {
    crate::backend::udev::run(options)
}

#[cfg(not(feature = "udev"))]
fn run_udev(_options: Options) -> anyhow::Result<()> {
    anyhow::bail!("本二进制未编入 udev 后端（--features udev）")
}

#[cfg(test)]
mod tests {
    use super::normalize_status_line;

    #[test]
    fn skips_i3bar_header_and_stream_framing() {
        assert_eq!(
            normalize_status_line(r#"{"version":1,"click_events":false}"#),
            None
        );
        assert_eq!(normalize_status_line("["), None);
        assert_eq!(normalize_status_line("],"), None);
        assert_eq!(normalize_status_line("]"), None);
    }

    #[test]
    fn parses_scroll_status_blocks_with_trailing_comma() {
        let line = concat!(
            r##"[{"full_text":" CPU ","color":"#a5adcb"},"##,
            r##"{"full_text":"BAT 97%","color":"#ffffff","background":"#c6a0f6e6"}],"##
        );
        assert_eq!(
            normalize_status_line(line).as_deref(),
            Some("^c#a5adcb^ CPU ^d^  ^c#ffffff^^b#c6a0f6e6^BAT 97%^d^")
        );
    }

    #[test]
    fn keeps_plain_text_and_escapes_carets() {
        assert_eq!(normalize_status_line("hello"), Some("hello".to_string()));
        assert_eq!(
            normalize_status_line(r#"[{"full_text":"a^b"}]"#).as_deref(),
            Some("a^^b^d^")
        );
    }
}
