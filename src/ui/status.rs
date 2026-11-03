//! bar 右侧状态文本的解析。
//!
//! 状态行沿用基线 `src/bar.c` 的 `statusescape`：脚本把一整行文本写到合成器
//! stdin，行内的 `^` 转义控制颜色与点击区，解析结果交给 [`crate::ui::bar`] 绘制。
//!
//! | 转义 | 含义 |
//! |---|---|
//! | `^c#RRGGBB^` / `^c#RRGGBBAA^` | 之后文字的前景色（8 位带透明度） |
//! | `^b#RRGGBB^` / `^b#RRGGBBAA^` | 之后文字的背景色 |
//! | `^d^` | 前景色、背景色都回到默认 |
//! | `^sN!命令^` | 打开按钮 N 的点击区（1 左键、2 右键、3 中键、4 滚轮上、5 滚轮下） |
//! | `^s!命令^` | 同上，等价按钮 1 |
//! | `^e^` / `^e!^` | 关闭所有点击区 |
//! | `^^` | 一个字面的 `^` |
//!
//! 不是合法转义的 `^` 会原样保留（基线同样把无法识别的转义当普通文字画出来）。
//! 按钮上的命令按 `/bin/sh -c` 执行。

/// 可点击按钮的数量：1 左键、2 右键、3 中键、4 滚轮上、5 滚轮下。
pub const BUTTONS: u8 = 5;

/// RGBA 颜色。
pub type Color = [u8; 4];

/// 一段同样式、同点击区的文字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    /// 前景色；`None` 表示用 bar 的默认文字色。
    pub fg: Option<Color>,
    /// 背景色；`None` 表示不铺底（露出 bar 底色）。
    pub bg: Option<Color>,
    /// 这段文字属于哪些点击区（[`Status::areas`] 的下标）。
    pub areas: Vec<usize>,
}

/// 一个按钮上的点击区。命令作用在打开它的 `^sN!...^` 之后、`^e^` 之前的文字上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Area {
    pub button: u8,
    pub command: String,
}

/// 解析后的状态行。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub items: Vec<Item>,
    pub areas: Vec<Area>,
}

impl Status {
    /// 解析一整行状态文本。
    pub fn parse(text: &str) -> Self {
        let mut status = Status::default();
        let mut buf = String::new();
        let mut fg = None;
        let mut bg = None;
        // 下标是按钮号；同时开启的多个按钮各自记一份。
        let mut open: [Option<usize>; BUTTONS as usize + 1] = [None; BUTTONS as usize + 1];

        let mut rest = text;
        loop {
            let Some(pos) = rest.find('^') else {
                buf.push_str(rest);
                break;
            };
            buf.push_str(&rest[..pos]);
            rest = &rest[pos..];

            if let Some(after) = rest.strip_prefix("^^") {
                buf.push('^');
                rest = after;
                continue;
            }

            let Some((len, effect)) = escape(rest) else {
                // 不是转义：保留这个 `^`，继续往后找。
                buf.push('^');
                rest = &rest[1..];
                continue;
            };

            flush(&mut status, &mut buf, fg, bg, &open);
            match effect {
                Effect::Fg(color) => fg = Some(color),
                Effect::Bg(color) => bg = Some(color),
                Effect::Reset => {
                    fg = None;
                    bg = None;
                }
                Effect::Close => open.fill(None),
                Effect::Open { button, command } => {
                    status.areas.push(Area {
                        button,
                        command: command.to_string(),
                    });
                    open[button as usize] = Some(status.areas.len() - 1);
                }
            }
            rest = &rest[len..];
        }
        flush(&mut status, &mut buf, fg, bg, &open);
        status
    }

    /// 第 `item` 段文字在 `button` 上有没有命令。
    pub fn command(&self, item: usize, button: u8) -> Option<&str> {
        let item = self.items.get(item)?;
        item.areas
            .iter()
            .filter_map(|&index| self.areas.get(index))
            .find(|area| area.button == button)
            .map(|area| area.command.as_str())
    }
}

fn flush(
    status: &mut Status,
    buf: &mut String,
    fg: Option<Color>,
    bg: Option<Color>,
    open: &[Option<usize>; BUTTONS as usize + 1],
) {
    if buf.is_empty() {
        return;
    }
    let areas = open.iter().flatten().copied().collect();
    status.items.push(Item {
        text: std::mem::take(buf),
        fg,
        bg,
        areas,
    });
}

enum Effect<'a> {
    Fg(Color),
    Bg(Color),
    Reset,
    Close,
    Open { button: u8, command: &'a str },
}

/// 解析 `s` 开头的一个转义；返回占用的字节数和效果。
fn escape(s: &str) -> Option<(usize, Effect<'_>)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'^') {
        return None;
    }
    match *bytes.get(1)? {
        b'd' if bytes.get(2) == Some(&b'^') => Some((3, Effect::Reset)),
        b'e' => {
            if bytes.get(2) == Some(&b'^') {
                Some((3, Effect::Close))
            } else if bytes.get(2) == Some(&b'!') && bytes.get(3) == Some(&b'^') {
                Some((4, Effect::Close))
            } else {
                None
            }
        }
        kind @ (b'c' | b'b') => {
            if bytes.get(2) != Some(&b'#') {
                return None;
            }
            let (color, len) = hex_color(&s[3..])?;
            let effect = if kind == b'c' {
                Effect::Fg(color)
            } else {
                Effect::Bg(color)
            };
            Some((len + 4, effect))
        }
        b's' => {
            // `^sN!命令^`；`N` 省略时是按钮 1。
            let digit = bytes.get(2).copied().unwrap_or(0);
            let (button, mut i) = if (b'1'..=b'9').contains(&digit) && digit - b'0' <= BUTTONS {
                (digit - b'0', 3)
            } else {
                (1, 2)
            };
            if bytes.get(i) != Some(&b'!') {
                return None;
            }
            i += 1;
            // 命令不能为空，也不能再含 `^`。
            if bytes.get(i).is_none() || bytes.get(i) == Some(&b'^') {
                return None;
            }
            let rest = &s[i..];
            let end = rest.find('^')?;
            if end == 0 {
                return None;
            }
            Some((
                i + end + 1,
                Effect::Open {
                    button,
                    command: &rest[..end],
                },
            ))
        }
        _ => None,
    }
}

/// 解析 `#` 之后的 6 位或 8 位十六进制；返回颜色与消耗的字符数（不含结尾 `^`）。
fn hex_color(s: &str) -> Option<(Color, usize)> {
    let mut value = 0u32;
    let mut digits = 0;
    for byte in s.bytes() {
        if digits == 8 {
            break;
        }
        let Some(digit) = (byte as char).to_digit(16) else {
            break;
        };
        value = (value << 4) | digit;
        digits += 1;
    }
    if s.as_bytes().get(digits) != Some(&b'^') {
        return None;
    }
    match digits {
        6 => Some((
            [(value >> 16) as u8, (value >> 8) as u8, value as u8, 0xff],
            digits,
        )),
        8 => Some((
            [
                (value >> 24) as u8,
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ],
            digits,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(status: &Status) -> Vec<&str> {
        status.items.iter().map(|item| item.text.as_str()).collect()
    }

    #[test]
    fn plain_text_is_one_item() {
        let status = Status::parse("12:34");
        assert_eq!(texts(&status), ["12:34"]);
        assert!(status.areas.is_empty());
        assert_eq!(status.items[0].fg, None);
        assert!(status.items[0].areas.is_empty());
    }

    #[test]
    fn colors_apply_to_following_runs_and_reset() {
        let status = Status::parse("^c#ff0000^red^d^normal");
        assert_eq!(texts(&status), ["red", "normal"]);
        assert_eq!(status.items[0].fg, Some([0xff, 0x00, 0x00, 0xff]));
        assert_eq!(status.items[1].fg, None);

        let status = Status::parse("^b#11223344^x");
        assert_eq!(status.items[0].bg, Some([0x11, 0x22, 0x33, 0x44]));
        assert_eq!(status.items[0].fg, None);
    }

    #[test]
    fn double_caret_is_literal() {
        let status = Status::parse("a^^b");
        assert_eq!(texts(&status), ["a^b"]);
    }

    #[test]
    fn click_area_covers_text_until_close() {
        let status = Status::parse("^s!echo hi^click^e^ tail");
        assert_eq!(texts(&status), ["click", " tail"]);
        assert_eq!(status.areas.len(), 1);
        assert_eq!(status.areas[0].button, 1);
        assert_eq!(status.areas[0].command, "echo hi");
        assert_eq!(status.command(0, 1), Some("echo hi"));
        assert_eq!(status.command(0, 2), None);
        assert_eq!(status.command(1, 1), None, "`^e^` 之后的文字不可点");
    }

    #[test]
    fn numbered_and_overlapping_buttons() {
        let status = Status::parse("^s2!right^A^s4!up^B");
        assert_eq!(texts(&status), ["A", "B"]);
        assert_eq!(status.command(0, 2), Some("right"));
        assert_eq!(status.command(0, 4), None);
        assert_eq!(status.command(1, 2), Some("right"), "按钮 2 的区域仍然开着");
        assert_eq!(status.command(1, 4), Some("up"));
    }

    #[test]
    fn malformed_escapes_stay_literal() {
        // 位数不对、没有结尾 `^`、按钮号越界、转义不完整，都按普通文字处理。
        for text in ["^c#fff^", "^c#ff0000", "^sq!x^", "^s!^", "^q", "^"] {
            let status = Status::parse(text);
            assert_eq!(texts(&status), [text], "{text} 应当原样保留");
            assert!(status.items[0].areas.is_empty());
        }
    }
}
