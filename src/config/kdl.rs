//! KDL 前端：把 niri 风格的配置文件映射到 [`super::Config`]。
//!
//! 只解析 r0wm 认识的节点；niri 有、r0wm 还没有的选项（gaps、border、
//! window-rule 等）会被忽略并打一条 debug 日志，避免用户为了试一个选项
//! 就得删掉整段配置。

use anyhow::Result;

use super::{
    BarPosition, Config, General, InputConfig, KeyBind, KeyboardConfig, LayoutConfig, LayoutName,
    OutputConfig, RunnerIcons,
};

type Doc = ::kdl::KdlDocument;
type Node = ::kdl::KdlNode;
type Value = ::kdl::KdlValue;

pub fn parse(text: &str) -> Result<Config> {
    let doc: Doc = text
        .parse()
        .map_err(|err| anyhow::anyhow!("KDL 语法错误：{err}"))?;
    let mut config = Config::default();

    for node in doc.nodes() {
        match node.name().value() {
            "general" => parse_general(node, &mut config.general)?,
            "layout" => parse_layout(node, &mut config.layout)?,
            "input" => parse_input(node, &mut config.input)?,
            "output" => {
                if let Some(output) = parse_output(node) {
                    config.outputs.push(output);
                }
            }
            "binds" => parse_binds(node, &mut config.binds)?,
            "spawn-at-startup" | "autostart" => {
                for value in positional_values(node) {
                    if let Some(command) = as_string(value) {
                        config.general.autostart.push(command);
                    }
                }
            }
            other => tracing::debug!("忽略未知的顶层配置节点：{other}"),
        }
    }

    Ok(config)
}

fn children(node: &Node) -> &[Node] {
    node.children().map(|doc| doc.nodes()).unwrap_or(&[])
}

fn positional_values(node: &Node) -> impl Iterator<Item = &Value> {
    node.entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .map(|entry| entry.value())
}

fn positional(node: &Node) -> Option<&Value> {
    positional_values(node).next()
}

fn property<'a>(node: &'a Node, name: &str) -> Option<&'a Value> {
    node.entries()
        .iter()
        .find(|entry| entry.name().is_some_and(|key| key.value() == name))
        .map(|entry| entry.value())
}

fn as_string(value: &Value) -> Option<String> {
    value
        .as_string()
        .map(str::to_owned)
        .or_else(|| value.as_integer().map(|number| number.to_string()))
}

fn as_bool(value: &Value) -> Option<bool> {
    value.as_bool()
}

fn as_i32(value: &Value) -> Option<i32> {
    value
        .as_integer()
        .and_then(|number| i32::try_from(number).ok())
        .or_else(|| value.as_float().map(|number| number as i32))
}

fn as_u32(value: &Value) -> Option<u32> {
    value
        .as_integer()
        .and_then(|number| u32::try_from(number).ok())
        .or_else(|| value.as_float().map(|number| number.max(0.0) as u32))
}

fn as_usize(value: &Value) -> Option<usize> {
    value
        .as_integer()
        .and_then(|number| usize::try_from(number).ok())
        .or_else(|| value.as_float().map(|number| number.max(0.0) as usize))
}

fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|number| number as f64))
}

fn bool_arg(node: &Node) -> Option<bool> {
    positional(node).and_then(as_bool)
}

// ---- general ----------------------------------------------------------------

fn parse_general(node: &Node, general: &mut General) -> Result<()> {
    for child in children(node) {
        match child.name().value() {
            "bar" => parse_bar(child, general),
            "titlebar" => parse_titlebar(child, general),
            "notifications" | "notify" => parse_notifications(child, general),
            "runner" => parse_runner(child, general),
            "systray" => parse_systray(child, general),
            "autostart" => {
                for value in positional_values(child) {
                    if let Some(command) = as_string(value) {
                        general.autostart.push(command);
                    }
                }
            }
            _ => apply_general_field(child, general),
        }
    }
    Ok(())
}

fn apply_general_field(node: &Node, general: &mut General) {
    let name = node.name().value().replace('-', "_");
    let value = positional(node);
    match name.as_str() {
        "terminal" => set_string(value, &mut general.terminal),
        "move_snap" => set_i32(value, &mut general.move_snap),
        "move_threshold" => set_f64(value, &mut general.move_threshold),
        "bar_height" => set_f32(value, &mut general.bar_height),
        "bar_position" => {
            if let Some(value) = value.and_then(as_string) {
                general.bar_position = parse_bar_position(&value);
            }
        }
        "bar_enabled" => set_bool(value, &mut general.bar_enabled),
        "font_size" => set_f32(value, &mut general.font_size),
        "font_family" => general.font_family = value.and_then(as_string),
        "titlebar" => set_bool(value, &mut general.titlebar),
        "title_padding" => set_i32(value, &mut general.title_padding),
        "title_center" => set_bool(value, &mut general.title_center),
        "title_close" => set_bool(value, &mut general.title_close),
        "title_close_color" => general.title_close_color = value.and_then(as_string),
        "bar_window_title" => set_bool(value, &mut general.bar_window_title),
        "show_notifications" => set_bool(value, &mut general.show_notifications),
        "notification_timeout" => set_u32(value, &mut general.notification_timeout),
        "notification_line_width" => set_u32(value, &mut general.notification_line_width),
        "notification_line_radius" => set_u32(value, &mut general.notification_line_radius),
        "notification_action_sign" => set_string(value, &mut general.notification_action_sign),
        "runner_placeholder" => set_string(value, &mut general.runner_placeholder),
        "runner_max_candidates" => set_usize(value, &mut general.runner_max_candidates),
        "runner_fuzzy" => set_bool(value, &mut general.runner_fuzzy),
        "runner_history_size" => set_usize(value, &mut general.runner_history_size),
        "runner_calc_precision" => set_u32(value, &mut general.runner_calc_precision),
        "runner_shell" => set_string(value, &mut general.runner_shell),
        "runner_terminal" => set_string(value, &mut general.runner_terminal),
        "runner_search_url" => set_string(value, &mut general.runner_search_url),
        "runner_files" => set_bool(value, &mut general.runner_files),
        "runner_paths" => set_string(value, &mut general.runner_paths),
        "runner_icons" => {
            if let Some(value) = value.and_then(as_string) {
                general.runner_icons = parse_runner_icons(&value);
            }
        }
        "runner_icon_size" => set_u32(value, &mut general.runner_icon_size),
        "runner_icon_round" => set_f32(value, &mut general.runner_icon_round),
        "runner_icon_border" => set_u32(value, &mut general.runner_icon_border),
        "runner_pill_height" => set_u32(value, &mut general.runner_pill_height),
        "runner_pill_padding" => set_u32(value, &mut general.runner_pill_padding),
        "runner_pill_round" => set_f32(value, &mut general.runner_pill_round),
        "runner_run_prefix" => set_string(value, &mut general.runner_run_prefix),
        "runner_term_prefix" => set_string(value, &mut general.runner_term_prefix),
        "runner_calc_prefix" => set_string(value, &mut general.runner_calc_prefix),
        "runner_search_prefix" => set_string(value, &mut general.runner_search_prefix),
        "runner_window_prefix" => set_string(value, &mut general.runner_window_prefix),
        "runner_tag_prefix" => set_string(value, &mut general.runner_tag_prefix),
        "show_systray" => set_bool(value, &mut general.show_systray),
        "systray_spacing" => set_i32(value, &mut general.systray_spacing),
        "systray_padding" => set_i32(value, &mut general.systray_padding),
        "systray_icon_size" => set_u32(value, &mut general.systray_icon_size),
        "status_command" => general.status_command = value.and_then(as_string),
        "autostart" => {
            for value in positional_values(node) {
                if let Some(command) = as_string(value) {
                    general.autostart.push(command);
                }
            }
        }
        _ => tracing::debug!("忽略 general 里的未知选项：{}", node.name().value()),
    }
}

fn parse_bar(node: &Node, general: &mut General) {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "enabled" => set_bool(positional(child), &mut general.bar_enabled),
            "height" => set_f32(positional(child), &mut general.bar_height),
            "position" => {
                if let Some(value) = positional(child).and_then(as_string) {
                    general.bar_position = parse_bar_position(&value);
                }
            }
            "font_size" => set_f32(positional(child), &mut general.font_size),
            "font_family" => general.font_family = positional(child).and_then(as_string),
            "window_title" => set_bool(positional(child), &mut general.bar_window_title),
            "status_command" => general.status_command = positional(child).and_then(as_string),
            other => tracing::debug!("忽略 bar 里的未知选项：{other}"),
        }
    }
}

fn parse_titlebar(node: &Node, general: &mut General) {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "enabled" => set_bool(positional(child), &mut general.titlebar),
            "padding" => set_i32(positional(child), &mut general.title_padding),
            "center" => set_bool(positional(child), &mut general.title_center),
            "close" => set_bool(positional(child), &mut general.title_close),
            "close_color" => general.title_close_color = positional(child).and_then(as_string),
            other => tracing::debug!("忽略 titlebar 里的未知选项：{other}"),
        }
    }
}

fn parse_notifications(node: &Node, general: &mut General) {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "enabled" => set_bool(positional(child), &mut general.show_notifications),
            "timeout" => set_u32(positional(child), &mut general.notification_timeout),
            "line_width" => set_u32(positional(child), &mut general.notification_line_width),
            "line_radius" => set_u32(positional(child), &mut general.notification_line_radius),
            "action_sign" => set_string(positional(child), &mut general.notification_action_sign),
            other => tracing::debug!("忽略 notifications 里的未知选项：{other}"),
        }
    }
}

fn parse_runner(node: &Node, general: &mut General) {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "placeholder" => set_string(positional(child), &mut general.runner_placeholder),
            "max_candidates" => set_usize(positional(child), &mut general.runner_max_candidates),
            "fuzzy" => set_bool(positional(child), &mut general.runner_fuzzy),
            "history_size" => set_usize(positional(child), &mut general.runner_history_size),
            "calc_precision" => set_u32(positional(child), &mut general.runner_calc_precision),
            "shell" => set_string(positional(child), &mut general.runner_shell),
            "terminal" => set_string(positional(child), &mut general.runner_terminal),
            "search_url" => set_string(positional(child), &mut general.runner_search_url),
            "files" => set_bool(positional(child), &mut general.runner_files),
            "paths" => set_string(positional(child), &mut general.runner_paths),
            "icons" => {
                if let Some(value) = positional(child).and_then(as_string) {
                    general.runner_icons = parse_runner_icons(&value);
                }
            }
            "icon_size" => set_u32(positional(child), &mut general.runner_icon_size),
            "icon_round" => set_f32(positional(child), &mut general.runner_icon_round),
            "icon_border" => set_u32(positional(child), &mut general.runner_icon_border),
            "pill_height" => set_u32(positional(child), &mut general.runner_pill_height),
            "pill_padding" => set_u32(positional(child), &mut general.runner_pill_padding),
            "pill_round" => set_f32(positional(child), &mut general.runner_pill_round),
            "run_prefix" => set_string(positional(child), &mut general.runner_run_prefix),
            "term_prefix" => set_string(positional(child), &mut general.runner_term_prefix),
            "calc_prefix" => set_string(positional(child), &mut general.runner_calc_prefix),
            "search_prefix" => set_string(positional(child), &mut general.runner_search_prefix),
            "window_prefix" => set_string(positional(child), &mut general.runner_window_prefix),
            "tag_prefix" => set_string(positional(child), &mut general.runner_tag_prefix),
            other => tracing::debug!("忽略 runner 里的未知选项：{other}"),
        }
    }
}

fn parse_systray(node: &Node, general: &mut General) {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "enabled" => set_bool(positional(child), &mut general.show_systray),
            "spacing" => set_i32(positional(child), &mut general.systray_spacing),
            "padding" => set_i32(positional(child), &mut general.systray_padding),
            "icon_size" => set_u32(positional(child), &mut general.systray_icon_size),
            other => tracing::debug!("忽略 systray 里的未知选项：{other}"),
        }
    }
}

fn parse_bar_position(value: &str) -> BarPosition {
    match value.to_ascii_lowercase().as_str() {
        "bottom" => BarPosition::Bottom,
        _ => BarPosition::Top,
    }
}

fn parse_runner_icons(value: &str) -> RunnerIcons {
    match value.to_ascii_lowercase().as_str() {
        "none" => RunnerIcons::None,
        "all" => RunnerIcons::All,
        _ => RunnerIcons::Selected,
    }
}

// ---- layout -----------------------------------------------------------------

fn parse_layout(node: &Node, layout: &mut LayoutConfig) -> Result<()> {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "default" | "layout" => {
                if let Some(name) = positional(child).and_then(as_string) {
                    layout.default = parse_layout_name(&name);
                }
            }
            "default_width" => {
                if let Some(value) = positional(child).and_then(as_f64) {
                    layout.default_width = value;
                }
            }
            "default_column_width" => {
                if let Some(value) = column_fraction(child) {
                    layout.default_width = value;
                }
            }
            "widths" => {
                let widths: Vec<f64> = positional_values(child).filter_map(as_f64).collect();
                if !widths.is_empty() {
                    layout.widths = widths;
                }
            }
            "preset_column_widths" => {
                let widths: Vec<f64> = children(child).iter().filter_map(column_fraction).collect();
                if !widths.is_empty() {
                    layout.widths = widths;
                }
            }
            "width" => {
                if let Some(value) = positional(child).and_then(as_f64) {
                    layout.widths.push(value);
                }
            }
            other => tracing::debug!("忽略 layout 里的未知选项：{other}"),
        }
    }
    Ok(())
}

fn column_fraction(node: &Node) -> Option<f64> {
    if node.name().value() == "proportion" {
        return positional(node).and_then(as_f64);
    }
    property(node, "proportion")
        .and_then(as_f64)
        .or_else(|| positional(node).and_then(as_f64))
        .or_else(|| children(node).first().and_then(column_fraction))
}

fn parse_layout_name(value: &str) -> LayoutName {
    match value.to_ascii_lowercase().as_str() {
        "scroll" => LayoutName::Scroll,
        "tabbed" => LayoutName::Tabbed,
        _ => LayoutName::Bsp,
    }
}

// ---- input ------------------------------------------------------------------

fn parse_input(node: &Node, input: &mut InputConfig) -> Result<()> {
    for child in children(node) {
        match child.name().value() {
            "keyboard" => parse_keyboard(child, &mut input.keyboard)?,
            other => tracing::debug!("忽略 input 里的未知节点：{other}"),
        }
    }
    Ok(())
}

fn parse_keyboard(node: &Node, keyboard: &mut KeyboardConfig) -> Result<()> {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "xkb" => parse_xkb(child, keyboard),
            "layout" => set_string(positional(child), &mut keyboard.layout),
            "variant" => set_string(positional(child), &mut keyboard.variant),
            "options" => set_string(positional(child), &mut keyboard.options),
            "model" => set_string(positional(child), &mut keyboard.model),
            "rules" => set_string(positional(child), &mut keyboard.rules),
            "repeat_delay" => set_i32(positional(child), &mut keyboard.repeat_delay),
            "repeat_rate" => set_i32(positional(child), &mut keyboard.repeat_rate),
            other => tracing::debug!("忽略 keyboard 里的未知选项：{other}"),
        }
    }
    Ok(())
}

fn parse_xkb(node: &Node, keyboard: &mut KeyboardConfig) {
    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "layout" => set_string(positional(child), &mut keyboard.layout),
            "variant" => set_string(positional(child), &mut keyboard.variant),
            "options" => set_string(positional(child), &mut keyboard.options),
            "model" => set_string(positional(child), &mut keyboard.model),
            "rules" => set_string(positional(child), &mut keyboard.rules),
            other => tracing::debug!("忽略 xkb 里的未知选项：{other}"),
        }
    }
}

// ---- output -----------------------------------------------------------------

fn parse_output(node: &Node) -> Option<OutputConfig> {
    let mut output = OutputConfig::default();
    let mut has_name = false;

    if let Some(name) = positional(node).and_then(as_string) {
        output.name = name;
        has_name = true;
    }

    for child in children(node) {
        match child.name().value().replace('-', "_").as_str() {
            "name" => {
                if let Some(name) = positional(child).and_then(as_string) {
                    output.name = name;
                    has_name = true;
                }
            }
            "enabled" => output.enabled = bool_arg(child).unwrap_or(true),
            "scale" => output.scale = positional(child).and_then(as_f64),
            "mode" => output.mode = positional(child).and_then(as_string),
            "transform" => output.transform = positional(child).and_then(as_string),
            "primary" => output.primary = bool_arg(child).unwrap_or(true),
            "position" => output.position = parse_position(child),
            other => tracing::debug!("忽略 output 里的未知选项：{other}"),
        }
    }

    has_name.then_some(output)
}

fn parse_position(node: &Node) -> Option<(i32, i32)> {
    let x = property(node, "x").and_then(as_i32);
    let y = property(node, "y").and_then(as_i32);
    if let (Some(x), Some(y)) = (x, y) {
        return Some((x, y));
    }

    let values: Vec<i32> = positional_values(node).filter_map(as_i32).collect();
    if values.len() >= 2 {
        return Some((values[0], values[1]));
    }
    if let Some(text) = positional(node).and_then(as_string) {
        let mut parts = text
            .split([',', 'x', ' '])
            .filter(|part| !part.trim().is_empty());
        let x = parts.next()?.trim().parse().ok()?;
        let y = parts.next()?.trim().parse().ok()?;
        return Some((x, y));
    }
    None
}

// ---- binds ------------------------------------------------------------------

fn parse_binds(node: &Node, binds: &mut Vec<KeyBind>) -> Result<()> {
    for bind in children(node) {
        let Some((mods, key)) = parse_key_combo(bind.name().value()) else {
            tracing::warn!(key = %bind.name().value(), "无法解析键绑定，跳过");
            continue;
        };

        let Some(action) = children(bind).first().and_then(parse_action) else {
            tracing::warn!(key = %bind.name().value(), "键绑定没有动作，跳过");
            continue;
        };

        binds.push(KeyBind {
            mods,
            key,
            action: action.0,
            arg: action.1,
        });
    }
    Ok(())
}

fn parse_key_combo(name: &str) -> Option<(Vec<String>, String)> {
    let mut parts: Vec<&str> = name
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let key = parts.pop()?.to_string();
    let mods = parts
        .into_iter()
        .map(|part| match part.to_ascii_lowercase().as_str() {
            "mod" | "super" | "logo" | "win" | "mod4" => "super".to_string(),
            "shift" => "shift".to_string(),
            "ctrl" | "control" => "ctrl".to_string(),
            "alt" | "mod1" => "alt".to_string(),
            other => other.to_string(),
        })
        .collect();
    Some((mods, key))
}

/// 把 niri 动作名翻译成动作表里的名字，并取第一个参数。
fn parse_action(node: &Node) -> Option<(String, Option<String>)> {
    let raw = node.name().value();
    let mut name = raw.replace('-', "_");
    let arg = positional(node).and_then(as_string);

    // niri 的命名与 r0wm 内部动作名并不总是一一对应。
    name = match name.as_str() {
        "spawn" => "spawn".into(),
        "close_window" | "close" | "kill" => "close".into(),
        "reload_config" | "reload" => "reload".into(),
        "focus_column_left" => "focus_column_left".into(),
        "focus_column_right" => "focus_column_right".into(),
        "focus_window_down" => "tab_next".into(),
        "focus_window_up" => "tab_prev".into(),
        "focus_workspace_up" => "focus_row_up".into(),
        "focus_workspace_down" => "focus_row_down".into(),
        "focus_output" | "focus_output_next" => "focus_output".into(),
        "focus_workspace" => "view".into(),
        "move_window_to_workspace" | "move_window_to_workspace_up" => "transfer_to".into(),
        "toggle_window_floating" | "toggle_floating" => "toggle_floating".into(),
        "consume_window_into_column" | "consume_window" | "consume" => "consume".into(),
        "expel_window_from_column" | "expel_window" | "expel" => "expel".into(),
        "fullscreen_window" | "toggle_window_fullscreen" | "toggle_fullscreen" | "fullscreen" => {
            "toggle_fullscreen".into()
        }
        "toggle_sticky" | "sticky" => "toggle_sticky".into(),
        "toggle_titlebar" => "toggle_titlebar".into(),
        "set_layout" => "set_layout".into(),
        "focus_next" => "focus_next".into(),
        "focus_prev" => "focus_prev".into(),
        "tab_next" => "tab_next".into(),
        "tab_prev" => "tab_prev".into(),
        "view" | "toggle_view" | "transfer_to" | "toggle_tag" | "toggleview" => name,
        "view_prev" | "viewprev" => "view_prev".into(),
        "shift_view" | "shiftview" => "shift_view".into(),
        "notify_open" | "notifyopen" => "notify_open".into(),
        "notify_dismiss" | "notifydismiss" => "notify_dismiss".into(),
        "notify_dismiss_all" | "notifydismissall" => "notify_dismiss_all".into(),
        "notify_next" | "notifynext" => "notify_next".into(),
        "notify_prev" | "notifyprev" => "notify_prev".into(),
        "notify_scroll" | "notifyscroll" => "notify_scroll".into(),
        "notify_actions" | "notifyactions" => "notify_actions".into(),
        "runner_toggle" | "runnertoggle" => "runner_toggle".into(),
        "quit" => "quit".into(),
        other => {
            tracing::warn!(action = other, "忽略 r0wm 还不支持的动作");
            return None;
        }
    };

    Some((name, arg))
}

// ---- 转换助手 ---------------------------------------------------------------

fn set_bool(value: Option<&Value>, target: &mut bool) {
    if let Some(value) = value.and_then(as_bool) {
        *target = value;
    }
}

fn set_i32(value: Option<&Value>, target: &mut i32) {
    if let Some(value) = value.and_then(as_i32) {
        *target = value;
    }
}

fn set_u32(value: Option<&Value>, target: &mut u32) {
    if let Some(value) = value.and_then(as_u32) {
        *target = value;
    }
}

fn set_usize(value: Option<&Value>, target: &mut usize) {
    if let Some(value) = value.and_then(as_usize) {
        *target = value;
    }
}

fn set_f32(value: Option<&Value>, target: &mut f32) {
    if let Some(value) = value.and_then(as_f64) {
        *target = value as f32;
    }
}

fn set_f64(value: Option<&Value>, target: &mut f64) {
    if let Some(value) = value.and_then(as_f64) {
        *target = value;
    }
}

fn set_string(value: Option<&Value>, target: &mut String) {
    if let Some(value) = value.and_then(as_string) {
        *target = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_general_names_work_too() {
        let config = parse(
            r#"
            general {
                terminal "kitty"
                bar-height 1.4
                runner-fuzzy false
            }
            "#,
        )
        .unwrap();
        assert_eq!(config.general.terminal, "kitty");
        assert_eq!(config.general.bar_height, 1.4);
        assert!(!config.general.runner_fuzzy);
    }

    #[test]
    fn unknown_nodes_are_ignored() {
        let config = parse(
            r#"
            layout { gaps 16; border { width 2; } }
            window-rule { match app-id="foo"; }
            binds { Mod+Return { spawn "foot"; } }
            "#,
        )
        .unwrap();
        assert_eq!(config.binds.len(), 1);
    }

    #[test]
    fn status_command_parses_in_bar_block() {
        let config = parse(
            r#"
            general {
                bar {
                    status-command "echo hi"
                }
            }
            "#,
        )
        .unwrap();
        assert_eq!(config.general.status_command.as_deref(), Some("echo hi"));
    }
}
