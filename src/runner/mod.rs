//! 命令提示（runner）：输入行、候选、计算器与各个模式的派发。
//!
//! 对标基线 `src/runner.c`。这一层只做状态与过滤，不碰 Wayland/窗口：
//! 应用与命令由 [`apps`] 扫描，窗口列表与 tag 名由合成器通过
//! [`Runner::set_windows`] / [`Runner::set_tags`] 注入，执行结果用 [`Outcome`]
//! 交回合成器（它才知道怎么 spawn、怎么切 tag）。
//!
//! 键盘处理顺序也照抄基线：Ctrl 组合 → 功能键 → 可打印字节；缓冲区变更后
//! 重新过滤，Tab 补全时列表保持不动。

pub mod apps;
pub mod calc;

use std::{env, path::PathBuf, time::SystemTime};

use apps::App;

/// 输入行上限（基线 `char runner_buf[256]`）。
pub const BUF_MAX: usize = 255;

/// 当前模式。`App` 是没前缀时的默认模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    App,
    Run,
    Term,
    Calc,
    Search,
    Window,
    Tag,
    File,
}

/// 模式前缀；空串表示关掉该模式。
#[derive(Debug, Clone)]
pub struct Prefixes {
    pub run: String,
    pub term: String,
    pub calc: String,
    pub search: String,
    pub window: String,
    pub tag: String,
}

impl Default for Prefixes {
    fn default() -> Self {
        Self {
            run: "!!".into(),
            term: "!".into(),
            calc: "=".into(),
            search: "?".into(),
            window: "@".into(),
            tag: "#".into(),
        }
    }
}

/// runner 的可配置项（对应基线 `config.h` 里的 `runner_*`）。
#[derive(Debug, Clone)]
pub struct Config {
    pub placeholder: String,
    /// 候选列表一次显示几条；0 表示不显示列表，只补全。
    pub max_candidates: usize,
    pub fuzzy: bool,
    pub history_size: usize,
    pub calc_precision: u32,
    pub shell: String,
    /// 空串表示没有终端（关掉 Term 模式）。
    pub terminal: String,
    /// 空串表示没有搜索引擎（关掉 Search 模式）。
    pub search_url: String,
    pub files: bool,
    /// 额外加入 `$PATH` 的目录（`~` 展开）。
    pub paths: String,
    pub prefixes: Prefixes,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            placeholder: "Run...".into(),
            max_candidates: 5,
            fuzzy: true,
            history_size: 100,
            calc_precision: 10,
            shell: "/bin/sh".into(),
            terminal: "foot".into(),
            search_url: "https://duckduckgo.com/?q=%s".into(),
            files: true,
            paths: String::new(),
            prefixes: Prefixes::default(),
        }
    }
}

/// 一次按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Escape,
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Char(char),
}

/// 按键的结局：合成器据此 spawn / 切 tag / 聚焦窗口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 提示还开着，继续。
    Continue,
    /// 收起提示，不做别的。
    Close,
    Exec {
        cmd: String,
        term: bool,
    },
    /// 下标指向 [`Runner::set_windows`] 传进来的标签数组。
    FocusWindow(usize),
    ViewTag(usize),
    Open(String),
    Search(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pool {
    Apps,
    Commands,
    Windows,
    Tags,
    Entries,
    Empty,
}

/// 提示的状态机。
pub struct Runner {
    config: Config,
    active: bool,
    buf: String,
    /// 光标在 `buf` 里的字节偏移，永远落在字符边界上。
    cursor: usize,
    mode: Mode,
    /// 查询串在 `buf` 里的起点（前缀 + 其后空白之后）。
    query_off: usize,
    sel: usize,
    cycling: bool,
    matches: Vec<usize>,

    apps: Vec<App>,
    commands: Vec<String>,
    windows: Vec<String>,
    tags: Vec<String>,
    entries: Vec<String>,
    entry_dir: Option<String>,
    entry_hidden: bool,
    apps_stamp: Option<SystemTime>,
    commands_stamp: Option<SystemTime>,

    history: Vec<String>,
    history_read: bool,
    histpos: Option<usize>,
    app_use: Vec<i32>,
    cmd_use: Vec<i32>,
}

impl Runner {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            active: false,
            buf: String::new(),
            cursor: 0,
            mode: Mode::App,
            query_off: 0,
            sel: 0,
            cycling: false,
            matches: Vec::new(),
            apps: Vec::new(),
            commands: Vec::new(),
            windows: Vec::new(),
            tags: Vec::new(),
            entries: Vec::new(),
            entry_dir: None,
            entry_hidden: false,
            apps_stamp: None,
            commands_stamp: None,
            history: Vec::new(),
            history_read: false,
            histpos: None,
            app_use: Vec::new(),
            cmd_use: Vec::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 热重载：只换配置，缓存保留（下次 open 时按 mtime 判断是否重扫）。
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
        self.history.truncate(self.config.history_size);
        self.weigh();
        if self.active {
            self.filter();
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn toggle(&mut self) {
        if self.active {
            self.active = false;
        } else {
            self.open();
        }
    }

    pub fn close(&mut self) {
        self.active = false;
    }

    /// 打开提示：重建可能在变的数据源，清空输入。
    pub fn open(&mut self) {
        self.refresh_sources();
        if !self.history_read && self.config.history_size > 0 {
            self.read_history();
        }
        self.history.truncate(self.config.history_size);
        self.weigh();
        self.buf.clear();
        self.cursor = 0;
        self.histpos = None;
        self.cycling = false;
        self.active = true;
        self.filter();
    }

    /// 打开提示前调用；目录 mtime 没变就跳过重扫。
    fn refresh_sources(&mut self) {
        let dirs = apps::search_paths(&self.config.paths);
        let stamp = newest_mtime(&dirs);
        if self.commands.is_empty() || stamp != self.commands_stamp {
            self.commands = apps::scan_commands(&dirs);
            self.commands_stamp = stamp;
        }

        let app_dirs = apps::desktop_dirs();
        let stamp = newest_mtime(&app_dirs);
        if self.apps.is_empty() || stamp != self.apps_stamp {
            self.apps = apps::scan_desktop(&app_dirs);
            self.apps_stamp = stamp;
        }
    }

    /// 合成器每次过滤前刷新：窗口顺序就是候选顺序。
    pub fn set_windows(&mut self, labels: Vec<String>) {
        self.windows = labels;
    }

    pub fn set_tags(&mut self, tags: Vec<String>) {
        self.tags = tags;
    }

    pub fn buf(&self) -> &str {
        &self.buf
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 去掉前缀与空白后的查询串。
    pub fn query(&self) -> &str {
        self.buf.get(self.query_off..).unwrap_or("")
    }

    pub fn selected(&self) -> usize {
        self.sel
    }

    /// 当前页第一条在候选里的下标。
    pub fn page_start(&self) -> usize {
        let per = self.config.max_candidates.max(1);
        self.sel / per * per
    }

    pub fn candidates(&self) -> &[usize] {
        &self.matches
    }

    /// 第 `i` 条候选的显示文本。
    pub fn candidate(&self, i: usize) -> Option<&str> {
        let index = *self.matches.get(i)?;
        match self.pool() {
            Pool::Apps => self.apps.get(index).map(|app| app.name.as_str()),
            Pool::Commands => self.commands.get(index).map(String::as_str),
            Pool::Windows => self.windows.get(index).map(String::as_str),
            Pool::Tags => self.tags.get(index).map(String::as_str),
            Pool::Entries => self.entries.get(index).map(String::as_str),
            Pool::Empty => None,
        }
    }

    /// 选中候选的补全文本（Tab 与 Enter 都用它）。
    pub fn suggest(&self) -> Option<&str> {
        if self.matches.is_empty() {
            None
        } else {
            self.candidate(self.sel)
        }
    }

    /// 第 `i` 条候选的图标名（只有 App 池有图标）。
    pub fn icon(&self, i: usize) -> Option<&str> {
        if self.pool() != Pool::Apps {
            return None;
        }
        let index = *self.matches.get(i)?;
        let app = self.apps.get(index)?;
        (!app.icon.is_empty()).then_some(app.icon.as_str())
    }

    /// App 模式下选中候选对应的图标名。
    pub fn selected_icon(&self) -> Option<&str> {
        if self.matches.is_empty() {
            return None;
        }
        self.icon(self.sel)
    }

    pub fn has_icons(&self) -> bool {
        self.pool() == Pool::Apps
    }

    /// 当前输入能不能直接算（App 模式要求带运算符）。
    pub fn calc_value(&self) -> Option<f64> {
        if !matches!(self.mode, Mode::App | Mode::Calc) {
            return None;
        }
        let query = self.query();
        if !calc::is_expr(query, self.mode == Mode::App) {
            return None;
        }
        calc::evaluate(query)
    }

    /// 计算结果的文本形式，供 bar 画 `= …`。
    pub fn calc_text(&self) -> Option<String> {
        self.calc_value()
            .map(|value| calc::format(value, self.config.calc_precision))
    }

    fn pool(&self) -> Pool {
        match self.mode {
            Mode::App => Pool::Apps,
            Mode::Run | Mode::Term => Pool::Commands,
            Mode::Window => Pool::Windows,
            Mode::Tag => Pool::Tags,
            Mode::File => Pool::Entries,
            Mode::Calc | Mode::Search => Pool::Empty,
        }
    }

    /// 基准列表的一份拷贝，过滤时不必和 `&mut self` 打架。
    fn pool_names(&self) -> Vec<String> {
        match self.pool() {
            Pool::Apps => self.apps.iter().map(|app| app.name.clone()).collect(),
            Pool::Commands => self.commands.clone(),
            Pool::Windows => self.windows.clone(),
            Pool::Tags => self.tags.clone(),
            Pool::Entries => self.entries.clone(),
            Pool::Empty => Vec::new(),
        }
    }

    /// 模式判定：最长前缀胜出；`/` 或 `~/` 开头进文件模式。
    fn mode_at(&self, text: &str) -> (Mode, usize) {
        let mut mode = Mode::App;
        let mut len = 0;
        let prefixes = [
            (Mode::Run, self.config.prefixes.run.as_str()),
            (Mode::Term, self.config.prefixes.term.as_str()),
            (Mode::Calc, self.config.prefixes.calc.as_str()),
            (Mode::Search, self.config.prefixes.search.as_str()),
            (Mode::Window, self.config.prefixes.window.as_str()),
            (Mode::Tag, self.config.prefixes.tag.as_str()),
        ];
        for (candidate, prefix) in prefixes {
            if prefix.is_empty() || prefix.len() <= len || !text.starts_with(prefix) {
                continue;
            }
            if candidate == Mode::Term && self.config.terminal.is_empty() {
                continue;
            }
            if candidate == Mode::Search && self.config.search_url.is_empty() {
                continue;
            }
            mode = candidate;
            len = prefix.len();
        }
        if mode == Mode::App && self.config.files && (text.starts_with('/') || is_home_path(text)) {
            let off = text.rfind('/').map(|i| i + 1).unwrap_or(0);
            return (Mode::File, off);
        }
        while text[len..].starts_with([' ', '\t']) {
            len += 1;
        }
        (mode, len)
    }

    /// 重新过滤（输入变化、窗口变化、配置变化后调用）。
    pub fn filter(&mut self) {
        let (mode, off) = self.mode_at(&self.buf);
        self.mode = mode;
        self.query_off = off;
        self.matches.clear();
        self.sel = 0;
        if mode == Mode::File {
            self.refresh_entries();
        }

        let names = self.pool_names();
        let query = self.query().to_string();
        let list_all = matches!(mode, Mode::Window | Mode::Tag | Mode::File);
        if query.is_empty() && list_all {
            self.matches.extend(0..names.len());
        }
        if query.is_empty() || self.calc_value().is_some() {
            return;
        }
        let fold = !query.chars().any(|ch| ch.is_ascii_uppercase());
        let uses: Vec<i32> = match self.pool() {
            Pool::Apps => self.app_use.clone(),
            Pool::Commands => self.cmd_use.clone(),
            _ => Vec::new(),
        };
        let alt: Vec<Option<&str>> = match self.pool() {
            Pool::Apps => self.apps.iter().map(|app| app.gname.as_deref()).collect(),
            _ => Vec::new(),
        };

        let mut scored: Vec<(i64, usize)> = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let mut best = candidate_score(name, &query, self.config.fuzzy, fold);
            if let Some(Some(other)) = alt.get(index) {
                best = best.max(candidate_score(other, &query, self.config.fuzzy, fold));
            }
            if best == 0 {
                continue;
            }
            let mut score = best;
            if let Some(use_count) = uses.get(index) {
                score += (*use_count).min(1 << 16) as i64 * 16;
            }
            scored.push((score, index));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
    }

    fn refresh_entries(&mut self) {
        let dir = expand_home(&self.buf[..self.query_off]);
        let hidden = self.buf[self.query_off..].starts_with('.');
        if self.entry_dir.as_deref() == Some(dir.as_str()) && self.entry_hidden == hidden {
            return;
        }
        self.entry_dir = Some(dir.clone());
        self.entry_hidden = hidden;
        self.entries.clear();
        if self.query_off == 0 {
            return;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !hidden {
                continue;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            self.entries
                .push(if is_dir { format!("{name}/") } else { name });
        }
        self.entries.sort();
    }

    // ---- 键盘 ----

    /// 处理一次按键。`ctrl`/`shift` 来自修饰键状态。
    pub fn key(&mut self, key: Key, ctrl: bool, shift: bool) -> Outcome {
        if !self.active {
            return Outcome::Close;
        }
        let before = self.buf.clone();
        let mut keep_list = false;

        if ctrl {
            if let Key::Char(ch) = key {
                match ch.to_ascii_lowercase() {
                    'c' => {
                        self.buf.clear();
                        self.cursor = 0;
                    }
                    'a' => self.cursor = 0,
                    'e' => self.cursor = self.buf.len(),
                    'f' => self.cursor = next_char(&self.buf, self.cursor),
                    'b' => self.cursor = prev_char(&self.buf, self.cursor),
                    'w' => {
                        let start = word_start(&self.buf, self.cursor);
                        self.buf.replace_range(start..self.cursor, "");
                        self.cursor = start;
                    }
                    _ => {}
                }
            }
            let changed = self.buf != before;
            if changed {
                self.cycling = false;
                self.filter();
            }
            return Outcome::Continue;
        }

        match key {
            Key::Escape => return Outcome::Close,
            Key::Enter => return self.enter(),
            Key::Backspace => {
                let start = prev_char(&self.buf, self.cursor);
                self.buf.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            Key::Delete => {
                let end = next_char(&self.buf, self.cursor);
                self.buf.replace_range(self.cursor..end, "");
            }
            Key::Left => self.cursor = prev_char(&self.buf, self.cursor),
            Key::Right => self.cursor = next_char(&self.buf, self.cursor),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.buf.len(),
            Key::Up => self.recall(-1),
            Key::Down => self.recall(1),
            Key::Tab | Key::BackTab => {
                let back = key == Key::BackTab || shift;
                if !self.matches.is_empty() && (self.cycling || back) {
                    self.select(if back { -1 } else { 1 });
                }
                match self.suggest().map(str::to_owned) {
                    Some(suggest) if self.query_off + suggest.len() < BUF_MAX => {
                        self.buf.truncate(self.query_off);
                        self.buf.push_str(&suggest);
                        self.cursor = self.buf.len();
                        self.cycling = true;
                        keep_list = true;
                    }
                    Some(_) => {}
                    None => {
                        self.calc_fill();
                    }
                }
            }
            Key::Char(ch) => {
                // Ctrl 组合产生的是控制字符，这里一律不收。
                if ch.is_control() {
                    return Outcome::Continue;
                }
                let mut encoded = [0u8; 4];
                let text = ch.encode_utf8(&mut encoded);
                if self.buf.len() + text.len() <= BUF_MAX {
                    self.buf.insert_str(self.cursor, text);
                    self.cursor += text.len();
                }
            }
        }

        if !keep_list && self.buf != before {
            self.cycling = false;
            self.filter();
        }
        Outcome::Continue
    }

    /// 把当前算式换成结果（Tab 或 Enter 时用），返回是否成功。
    pub fn calc_fill(&mut self) -> bool {
        let Some(text) = self.calc_text() else {
            return false;
        };
        let off = self.query_off;
        if off + text.len() >= BUF_MAX {
            return false;
        }
        self.buf.truncate(off);
        self.buf.push_str(&text);
        self.cursor = self.buf.len();
        true
    }

    fn enter(&mut self) -> Outcome {
        let suggest = self.suggest().map(str::to_owned);
        let query = self.query().to_string();
        let off = self.query_off;
        let mode = self.mode;
        let picked = suggest.clone().unwrap_or_else(|| query.clone());
        let line = format!("{}{picked}", &self.buf[..off]);

        let outcome = match mode {
            Mode::App => {
                let Some(index) = self.matches.get(self.sel).copied() else {
                    return Outcome::Close;
                };
                let Some(app) = self.apps.get(index) else {
                    return Outcome::Close;
                };
                Outcome::Exec {
                    cmd: apps::unescape_exec(&app.exec),
                    term: app.term,
                }
            }
            Mode::Run => {
                let cmd = suggest.unwrap_or(query);
                if cmd.is_empty() {
                    return Outcome::Close;
                }
                Outcome::Exec { cmd, term: false }
            }
            Mode::Term => {
                let cmd = suggest.unwrap_or(query);
                Outcome::Exec { cmd, term: true }
            }
            Mode::Calc => {
                let keep = self.calc_fill() || self.buf.len() > off;
                if !keep {
                    return Outcome::Close;
                }
                return Outcome::Continue;
            }
            Mode::Search => {
                let cmd = suggest.unwrap_or(query);
                if cmd.is_empty() {
                    return Outcome::Close;
                }
                Outcome::Search(cmd)
            }
            Mode::Window => {
                let Some(index) = self.matches.get(self.sel).copied() else {
                    return Outcome::Close;
                };
                Outcome::FocusWindow(index)
            }
            Mode::Tag => {
                let Some(index) = self.matches.get(self.sel).copied() else {
                    return Outcome::Close;
                };
                Outcome::ViewTag(index)
            }
            Mode::File => {
                let cmd = suggest.unwrap_or(query);
                Outcome::Open(format!("{}{cmd}", &self.buf[..off]))
            }
        };
        self.active = false;
        if !line.is_empty() {
            self.record(&line);
        }
        outcome
    }

    fn select(&mut self, step: i32) {
        let len = self.matches.len() as i32;
        if len == 0 {
            return;
        }
        self.sel = ((self.sel as i32 + step + len) % len) as usize;
    }

    /// 上下翻历史，与 shell 一样跳过重复项；往下翻过最新会清空输入。
    fn recall(&mut self, step: i32) {
        let shown = self
            .histpos
            .and_then(|pos| self.history.get(pos))
            .cloned()
            .unwrap_or_default();
        if self.histpos.is_none() && step > 0 {
            return;
        }
        let mut pos = match self.histpos {
            Some(pos) => pos as i32,
            None => self.history.len() as i32,
        };
        loop {
            pos += step;
            if pos < 0 || pos >= self.history.len() as i32 {
                break;
            }
            if self.history[pos as usize] != shown {
                break;
            }
        }
        if pos < 0 {
            return;
        }
        if pos >= self.history.len() as i32 {
            self.histpos = None;
            self.buf.clear();
        } else {
            self.histpos = Some(pos as usize);
            self.buf.clear();
            self.buf.push_str(&self.history[pos as usize]);
        }
        self.cursor = self.buf.len();
        self.filter();
    }

    // ---- 历史 ----

    fn read_history(&mut self) {
        self.history_read = true;
        let Some(path) = history_path() else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            if !line.is_empty() && line.len() < BUF_MAX {
                self.history.push(line.to_string());
            }
        }
        let max = self.config.history_size;
        if self.history.len() > max {
            self.history.drain(..self.history.len() - max);
        }
    }

    fn write_history(&self) {
        let Some(path) = history_path() else {
            return;
        };
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_err()
        {
            return;
        }
        let mut text = self.history.join("\n");
        text.push('\n');
        if let Err(err) = std::fs::write(&path, text) {
            tracing::warn!(path = %path.display(), %err, "写入 runner 历史失败");
        }
    }

    fn record(&mut self, line: &str) {
        if self.config.history_size == 0 {
            return;
        }
        self.history.push(line.to_string());
        let max = self.config.history_size;
        if self.history.len() > max {
            self.history.drain(..self.history.len() - max);
        }
        self.write_history();
        self.weigh();
    }

    /// 历史里出现过的应用/命令按新旧加权：最新的是最旧的最多 5 倍。
    fn weigh(&mut self) {
        self.app_use = vec![0; self.apps.len()];
        self.cmd_use = vec![0; self.commands.len()];
        let total = self.history.len().max(1) as i32;
        let history = self.history.clone();
        for (index, line) in history.iter().enumerate() {
            let weight = 1 + ((index as i32 + 1) * 4 / total);
            let (mode, off) = self.mode_at(line);
            let rest = &line[off.min(line.len())..];
            if mode == Mode::App
                && let Some(position) = self.apps.iter().position(|app| app.name == rest)
            {
                self.app_use[position] += weight;
            }
            if !matches!(mode, Mode::Run | Mode::Term) {
                continue;
            }
            let word = rest.split([' ', '\t']).next().unwrap_or("");
            if let Some(position) = self.commands.iter().position(|cmd| cmd == word) {
                self.cmd_use[position] += weight;
            }
        }
    }
}

/// 把查询词填进搜索 URL 的 `%s`；没有 `%s` 就追加在末尾。
///
/// 只放行字母数字与 `-_.~`，其余按字节百分号编码（基线 `runnersearch`）。
pub fn search_url(base: &str, query: &str) -> String {
    let mut encoded = String::with_capacity(query.len());
    for byte in query.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    match base.split_once("%s") {
        Some((head, tail)) => format!("{head}{encoded}{tail}"),
        None => format!("{base}{encoded}"),
    }
}

fn is_home_path(text: &str) -> bool {
    text == "~" || text.starts_with("~/")
}

fn expand_home(text: &str) -> String {
    if let Some(rest) = text.strip_prefix('~') {
        let home = env::var("HOME").unwrap_or_default();
        return format!("{home}{rest}");
    }
    text.to_string()
}

fn history_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_STATE_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join("r0wm/runner_history"));
    }
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/r0wm/runner_history"))
}

fn newest_mtime(dirs: &[PathBuf]) -> Option<SystemTime> {
    dirs.iter()
        .filter_map(|dir| std::fs::metadata(dir).ok()?.modified().ok())
        .max()
}

fn prev_char(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    if index == 0 {
        return 0;
    }
    index -= 1;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_char(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    if index >= text.len() {
        return text.len();
    }
    index += 1;
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Ctrl+W：跳过尾部空格，再删掉前面那一段非空格。
fn word_start(text: &str, cursor: usize) -> usize {
    let bytes = text.as_bytes();
    let mut start = cursor;
    while start > 0 && bytes[start - 1] == b' ' {
        start -= 1;
    }
    while start > 0 && bytes[start - 1] != b' ' {
        start -= 1;
    }
    start
}

/// 候选打分。完全相等最高；不模糊时只认前缀；模糊时要求字母按顺序出现，
/// 名字开头、词首、连续命中加分，越短的名字在同分时越靠前。
fn candidate_score(candidate: &str, query: &str, fuzzy: bool, fold: bool) -> i64 {
    if candidate == query {
        return 1 << 24;
    }
    if !fuzzy {
        return candidate.starts_with(query) as i64;
    }
    let query_chars: Vec<char> = query.chars().map(|ch| fold_char(ch, fold)).collect();
    let mut score = 0i64;
    let mut run = 0i64;
    let mut matched = 0usize;
    let mut examined = 0usize;
    let mut prev: Option<char> = None;
    for ch in candidate.chars() {
        if matched >= query_chars.len() {
            break;
        }
        examined += 1;
        if fold_char(ch, fold) == query_chars[matched] {
            score += 1 + 3 * run;
            if prev.is_none() {
                score += 8;
            } else if prev.is_some_and(|p| matches!(p, '-' | '_' | '.' | ' ')) {
                score += 4;
            }
            run += 1;
            matched += 1;
        } else {
            run = 0;
        }
        prev = Some(ch);
    }
    if matched < query_chars.len() {
        return 0;
    }
    // 一整段前缀都对上：额外加一点，让 "lso" 更愿意补成 "lsof" 而不是 "lsof-xyz"。
    if examined == query_chars.len() {
        score += 20;
    }
    // 同分时短名字在前。
    let clen = candidate.chars().count() as i64;
    let qlen = query_chars.len() as i64;
    (score * 8 - (clen - qlen)).max(1)
}

fn fold_char(ch: char, fold: bool) -> char {
    if fold { ch.to_ascii_lowercase() } else { ch }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> Runner {
        // 不要真去扫宿主的 PATH 与 applications。
        let config = Config {
            history_size: 0,
            ..Default::default()
        };
        let mut runner = Runner::new(config);
        runner.apps = vec![
            App {
                id: "a.desktop".into(),
                name: "Alpha".into(),
                gname: None,
                exec: "alpha %U".into(),
                icon: "alpha".into(),
                term: false,
                hide: false,
            },
            App {
                id: "b.desktop".into(),
                name: "Beta".into(),
                gname: Some("Second".into()),
                exec: "beta --flag".into(),
                icon: String::new(),
                term: true,
                hide: false,
            },
        ];
        runner.commands = vec!["ls".into(), "lsof".into(), "make".into()];
        runner.tags = (1..=3).map(|n| n.to_string()).collect();
        runner
    }

    #[test]
    fn app_mode_filters_and_suggests() {
        let mut runner = runner();
        runner.active = true;
        runner.filter();
        // App 模式空查询不列候选（基线只对窗口/tag/文件这么做）。
        assert!(runner.candidates().is_empty());
        runner.buf.push_str("al");
        runner.filter();
        assert_eq!(runner.suggest(), Some("Alpha"));
        // 未翻译的名字也能命中。
        runner.buf.clear();
        runner.buf.push_str("second");
        runner.filter();
        assert_eq!(runner.suggest(), Some("Beta"));
    }

    #[test]
    fn uppercase_makes_matching_case_sensitive() {
        let mut runner = runner();
        runner.active = true;
        // 有一个大写字母就变成大小写敏感（完全相等仍然算命中）。
        runner.buf.push_str("AL");
        runner.filter();
        assert!(runner.matches.is_empty());
        // 小写仍然忽略大小写。
        runner.buf = "alpha".into();
        runner.filter();
        assert_eq!(runner.suggest(), Some("Alpha"));
    }

    #[test]
    fn enter_app_runs_unescaped_exec() {
        let mut runner = runner();
        runner.open_buf("alp");
        assert_eq!(
            runner.enter(),
            Outcome::Exec {
                cmd: "alpha ".into(),
                term: false
            }
        );
        assert!(!runner.active());
    }

    #[test]
    fn term_mode_marks_terminal() {
        let mut runner = runner();
        // Term 与 Run 一样从命令里选，只是执行时套一层终端。
        runner.commands = vec!["btop".into()];
        runner.open_buf("!bto");
        assert_eq!(runner.mode(), Mode::Term);
        assert_eq!(
            runner.enter(),
            Outcome::Exec {
                cmd: "btop".into(),
                term: true
            }
        );
    }

    #[test]
    fn run_mode_uses_commands_and_raw_query() {
        let mut runner = runner();
        runner.open_buf("!!lso");
        assert_eq!(runner.mode(), Mode::Run);
        assert_eq!(runner.suggest(), Some("lsof"));
        // Tab 补全后回车跑的是补全后的命令。
        assert_eq!(runner.key(Key::Tab, false, false), Outcome::Continue);
        assert_eq!(runner.buf(), "!!lsof");
        assert_eq!(
            runner.enter(),
            Outcome::Exec {
                cmd: "lsof".into(),
                term: false
            }
        );
    }

    #[test]
    fn calc_mode_evaluates_and_fills() {
        let mut runner = runner();
        runner.open_buf("=1+2*3");
        assert_eq!(runner.calc_text().as_deref(), Some("7"));
        assert!(runner.calc_fill());
        assert_eq!(runner.buf(), "=7");
        // 算完还开着，可以接着算。
        assert_eq!(runner.enter(), Outcome::Continue);
    }

    #[test]
    fn app_mode_needs_an_operator_for_calc() {
        let mut runner = runner();
        runner.open_buf("42");
        assert!(runner.calc_value().is_none());
        runner.buf.clear();
        runner.buf.push_str("40+2");
        runner.filter();
        assert_eq!(runner.calc_text().as_deref(), Some("42"));
    }

    #[test]
    fn window_and_tag_modes_list_everything() {
        let mut runner = runner();
        runner.set_windows(vec!["foot: shell".into(), "firefox".into()]);
        runner.open_buf("@");
        assert_eq!(runner.mode(), Mode::Window);
        assert_eq!(runner.candidates().len(), 2);
        assert_eq!(runner.enter(), Outcome::FocusWindow(0));

        let mut runner = self::runner();
        runner.open_buf("#");
        assert_eq!(runner.mode(), Mode::Tag);
        assert_eq!(runner.candidates().len(), 3);
        runner.select(1);
        assert_eq!(runner.enter(), Outcome::ViewTag(1));
    }

    #[test]
    fn search_mode_builds_the_query() {
        let mut runner = runner();
        runner.open_buf("?foo");
        assert_eq!(runner.enter(), Outcome::Search("foo".into()));

        let mut runner = Runner::new(Config {
            search_url: String::new(),
            ..Default::default()
        });
        runner.commands.clear();
        runner.open_buf("?foo");
        // 没有搜索引擎时 `?` 不是前缀，整串落回 App 模式。
        assert_eq!(runner.mode(), Mode::App);
    }

    #[test]
    fn file_mode_lists_directory() {
        let dir = std::env::temp_dir().join(format!("r0wm-runner-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();

        let mut runner = runner();
        let base = format!("{}/", dir.display());
        runner.buf.push_str(&base);
        runner.filter();
        assert_eq!(runner.mode(), Mode::File);
        assert_eq!(runner.candidates().len(), 2, "{:?}", runner.entries);
        assert_eq!(runner.candidate(0), Some("notes.txt"));
        assert_eq!(runner.candidate(1), Some("sub/"));
        assert_eq!(
            runner.enter(),
            Outcome::Open(format!("{}/notes.txt", dir.display()))
        );
    }

    #[test]
    fn typing_edits_at_the_cursor_and_ctrl_shortcuts_work() {
        let mut runner = runner();
        runner.active = true;
        for ch in "abc".chars() {
            runner.key(Key::Char(ch), false, false);
        }
        assert_eq!(runner.cursor(), 3);
        runner.key(Key::Left, false, false);
        runner.key(Key::Char('X'), false, false);
        assert_eq!(runner.buf(), "abXc");
        runner.key(Key::Char('c'), true, false);
        assert_eq!(runner.buf(), "");
        runner.key(Key::Char('z'), true, false);
        assert_eq!(runner.buf(), "", "未知 Ctrl 组合不产生字符");
        for ch in "one two".chars() {
            runner.key(Key::Char(ch), false, false);
        }
        runner.key(Key::Char('w'), true, false);
        assert_eq!(runner.buf(), "one ");
        runner.key(Key::Backspace, false, false);
        assert_eq!(runner.buf(), "one");
        runner.key(Key::Home, false, false);
        runner.key(Key::Delete, false, false);
        assert_eq!(runner.buf(), "ne");
        assert_eq!(runner.key(Key::Escape, false, false), Outcome::Close);
    }

    #[test]
    fn history_recall_skips_duplicates() {
        let mut runner = runner();
        runner.history = vec!["!!ls".into(), "!!ls".into(), "!!make".into()];
        runner.active = true;
        runner.key(Key::Up, false, false);
        assert_eq!(runner.buf(), "!!make");
        runner.key(Key::Up, false, false);
        assert_eq!(runner.buf(), "!!ls");
        runner.key(Key::Down, false, false);
        assert_eq!(runner.buf(), "!!make");
        runner.key(Key::Down, false, false);
        assert_eq!(runner.buf(), "");
    }

    #[test]
    fn weights_prefer_recent_history() {
        let mut runner = runner();
        // 历史是「旧的在前」：Alpha 最旧、Beta 最新。
        runner.history = vec!["Alpha".into(), "Beta".into()];
        runner.weigh();
        assert_eq!(runner.app_use[0], 3);
        assert_eq!(runner.app_use[1], 5);
    }

    #[test]
    fn tab_cycles_candidates_without_refiltering() {
        let mut runner = runner();
        runner.commands = vec!["aa".into(), "ab".into()];
        runner.open_buf("!!a");
        assert_eq!(runner.candidates().len(), 2);
        assert_eq!(runner.key(Key::Tab, false, false), Outcome::Continue);
        assert_eq!(runner.buf(), "!!aa");
        runner.key(Key::Tab, false, false);
        assert_eq!(runner.buf(), "!!ab");
        runner.key(Key::BackTab, false, false);
        assert_eq!(runner.buf(), "!!aa");
    }

    #[test]
    fn suggestion_that_does_not_fit_is_ignored() {
        let mut runner = runner();
        runner.commands = vec!["a".repeat(BUF_MAX + 10)];
        runner.mode = Mode::Run;
        runner.query_off = 2;
        runner.buf = format!("!!{}", "a".repeat(BUF_MAX - 2));
        runner.cursor = runner.buf.len();
        runner.matches = vec![0];
        runner.active = true;
        let before = runner.buf().len();
        runner.key(Key::Tab, false, false);
        assert_eq!(runner.buf().len(), before);
    }

    #[test]
    fn search_url_fills_and_appends() {
        assert_eq!(
            search_url("https://example.com/?q=%s", "a b"),
            "https://example.com/?q=a%20b"
        );
        assert_eq!(
            search_url("https://example.com/?q=", "x/y"),
            "https://example.com/?q=x%2Fy"
        );
        assert_eq!(
            search_url("https://example.com/?q=%s&x=1", "-_.~"),
            "https://example.com/?q=-_.~&x=1"
        );
    }

    #[test]
    fn cursor_moves_over_whole_codepoints() {
        let text = "aé中";
        assert_eq!(prev_char(text, text.len()), text.len() - 3);
        assert_eq!(next_char(text, 1), 3);
        assert_eq!(prev_char(text, 0), 0);
        assert_eq!(next_char(text, text.len()), text.len());
    }

    // 便于测试的小工具：直接装填输入并过滤。
    impl Runner {
        fn open_buf(&mut self, text: &str) {
            self.active = true;
            self.buf.clear();
            self.buf.push_str(text);
            self.cursor = self.buf.len();
            self.filter();
        }
    }
}
