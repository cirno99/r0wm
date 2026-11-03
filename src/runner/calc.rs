//! 命令提示里的计算器。
//!
//! 对标基线 `src/runner.c` 的 `runnercalc*`：只认 `+ - * / %`、括号、小数点与
//! 空白，递归下降求值；深度上限 64，输入长度受输入行限制，所以不会爆栈。
//! 不做任何字符串拼接，因此畸形表达式最多是「解析失败」。

/// 输入里最长能带回结果的长度（基线 `char calcbuf[48]`）。
pub const RESULT_MAX: usize = 48;

struct State<'a> {
    s: &'a [u8],
    pos: usize,
    failed: bool,
    depth: u32,
}

impl State<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.bump();
        }
    }

    fn enter(&mut self) -> bool {
        self.depth += 1;
        if self.depth > 64 {
            self.failed = true;
            return false;
        }
        true
    }

    fn number(&mut self) -> f64 {
        let mut value = 0.0f64;
        let mut any = false;
        while let Some(d) = self.peek()
            && d.is_ascii_digit()
        {
            value = value * 10.0 + (d - b'0') as f64;
            self.bump();
            any = true;
        }
        if self.peek() == Some(b'.') {
            self.bump();
            let mut frac = 0.1;
            while let Some(d) = self.peek()
                && d.is_ascii_digit()
            {
                value += (d - b'0') as f64 * frac;
                frac *= 0.1;
                self.bump();
                any = true;
            }
        }
        if !any {
            self.failed = true;
        }
        value
    }

    fn atom(&mut self) -> f64 {
        if !self.enter() {
            return 0.0;
        }
        self.skip_blanks();
        let value = match self.peek() {
            Some(b'-') => {
                self.bump();
                -self.atom()
            }
            Some(b'+') => {
                self.bump();
                self.atom()
            }
            Some(b'(') => {
                self.bump();
                let value = self.expr();
                self.skip_blanks();
                if self.peek() != Some(b')') {
                    self.failed = true;
                    value
                } else {
                    self.bump();
                    value
                }
            }
            _ => self.number(),
        };
        self.depth -= 1;
        value
    }

    fn term(&mut self) -> f64 {
        let mut value = self.atom();
        loop {
            self.skip_blanks();
            match self.peek() {
                Some(b'*') => {
                    self.bump();
                    value *= self.atom();
                }
                Some(b'/') | Some(b'%') => {
                    let modulo = self.peek() == Some(b'%');
                    self.bump();
                    let rhs = self.atom();
                    if rhs == 0.0 {
                        self.failed = true;
                        return 0.0;
                    }
                    value = if modulo { value % rhs } else { value / rhs };
                }
                _ => break,
            }
        }
        value
    }

    fn expr(&mut self) -> f64 {
        let mut value = self.term();
        loop {
            self.skip_blanks();
            match self.peek() {
                Some(b'+') => {
                    self.bump();
                    value += self.term();
                }
                Some(b'-') => {
                    self.bump();
                    value -= self.term();
                }
                _ => break,
            }
        }
        value
    }
}

/// 求值整串；不完整 / 除零 / 非有限值都返回 `None`。
pub fn evaluate(text: &str) -> Option<f64> {
    let mut state = State {
        s: text.as_bytes(),
        pos: 0,
        failed: false,
        depth: 0,
    };
    let value = state.expr();
    state.skip_blanks();
    if state.failed || state.pos != state.s.len() || !value.is_finite() {
        return None;
    }
    Some(value)
}

/// 看起来像不像一个表达式。`need_op` 为真时（App 模式）必须带运算符，
/// 这样光敲一个数字不会被当成算式。
pub fn is_expr(text: &str, need_op: bool) -> bool {
    let mut digit = false;
    let mut op = !need_op;
    for ch in text.chars() {
        match ch {
            '0'..='9' => digit = true,
            '+' | '-' | '*' | '/' | '%' => op = true,
            '(' | ')' | '.' | ' ' | '\t' => {}
            _ => return false,
        }
    }
    digit && op
}

/// `%.*g` 的等价物（C 的 `%g`：按精度选定点或科学计数法，去掉尾随零）。
pub fn format(value: f64, precision: u32) -> String {
    let precision = precision.clamp(1, 17) as i32;
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let exponent = value.abs().log10().floor() as i32;
    if exponent < -4 || exponent >= precision {
        let text = format!("{:.*e}", (precision - 1) as usize, value);
        // Rust 的 `e` 是 `1.5e3`，C 是 `1.5e+03`。
        let (mantissa, exp) = text.split_once('e').unwrap_or((text.as_str(), "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let mantissa = trim_zeros(mantissa.to_string());
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (precision - 1 - exponent).max(0) as usize;
        trim_zeros(format!("{value:.decimals$}"))
    }
}

fn trim_zeros(mut text: String) -> String {
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_parens() {
        assert_eq!(evaluate("1+2*3"), Some(7.0));
        assert_eq!(evaluate("(1+2)*3"), Some(9.0));
        assert_eq!(evaluate(" 10 / 4 "), Some(2.5));
        assert_eq!(evaluate("7%3"), Some(1.0));
        assert_eq!(evaluate("-3 + +4"), Some(1.0));
    }

    #[test]
    fn rejects_malformed_and_zero_division() {
        assert_eq!(evaluate("1+"), None);
        assert_eq!(evaluate("1/0"), None);
        assert_eq!(evaluate("(1+2"), None);
        assert_eq!(evaluate("1 + x"), None);
        assert_eq!(evaluate(""), None);
    }

    #[test]
    fn is_expr_needs_operator_outside_calc_mode() {
        assert!(is_expr("1+1", true));
        assert!(!is_expr("11", true));
        assert!(is_expr("11", false));
        assert!(!is_expr("ls -l", false));
    }

    #[test]
    fn format_follows_g() {
        assert_eq!(format(5.0, 10), "5");
        assert_eq!(format(1.5, 10), "1.5");
        assert_eq!(format(1.0 / 3.0, 10), "0.3333333333");
        assert_eq!(format(1_000_000.0, 10), "1000000");
        assert_eq!(format(1e-5, 10), "1e-05");
        assert_eq!(format(123456789012.0, 10), "1.23456789e+11");
        assert_eq!(format(-2.5, 3), "-2.5");
        assert_eq!(format(0.0, 10), "0");
    }
}
