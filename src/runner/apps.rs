//! runner 的数据源：`.desktop` 应用与 `$PATH` 里的命令。
//!
//! 对标基线 `src/runner.c` 的 `runnerbuildapps()` / `runnerbuildcache()`：
//! 启动提示时扫一遍并缓存，之后每次按键只过滤内存里的数组。

use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
};

/// 一条 `.desktop` 应用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// 桌面文件 id（相对 `applications/` 的路径，`/` 换成 `-`）。
    pub id: String,
    /// 按当前 locale 选出的显示名。
    pub name: String,
    /// 未翻译的 `Name`（跟 `name` 不同时用于拼音/英文匹配）。
    pub gname: Option<String>,
    pub exec: String,
    pub icon: String,
    pub term: bool,
    /// `NoDisplay` / `Hidden` / 不是 Application / 缺 Name 或 Exec。
    pub hide: bool,
}

/// 会话 locale：基线的 `LC_ALL` → `LC_MESSAGES` → `LANG`。
pub fn session_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .find(|value| !value.is_empty())
}

fn trim(text: &str) -> &str {
    text.trim_matches([' ', '\t'])
}

/// `Name[xx]` 跟 locale 的贴合程度：完全一致 3、只看语言 2、只有 `Name` 1。
fn name_rank(key: &str, locale: Option<&str>) -> u8 {
    if key == "Name" {
        return 1;
    }
    let Some(rest) = key.strip_prefix("Name[") else {
        return 0;
    };
    if rest.is_empty() {
        return 0;
    }
    let Some(locale) = locale else {
        return 0;
    };
    let bytes = locale.as_bytes();
    let rank = |cut: fn(u8) -> bool| {
        let end = bytes.iter().position(|b| cut(*b)).unwrap_or(bytes.len());
        rest.len() == end + 1 && rest.as_bytes().starts_with(&bytes[..end])
    };
    if rank(|b| b == b'.' || b == b'@') {
        3
    } else if rank(|b| matches!(b, b'_' | b'.' | b'@')) {
        2
    } else {
        0
    }
}

/// 解析一个 `.desktop` 文件。`[Desktop Entry]` 之外的组一律不看。
pub fn parse_desktop(content: &str, id: &str, locale: Option<&str>) -> App {
    let mut app = App {
        id: id.to_string(),
        name: String::new(),
        gname: None,
        exec: String::new(),
        icon: String::new(),
        term: false,
        hide: false,
    };
    let mut in_entry = false;
    let mut is_app = false;
    let mut name = String::new();
    let mut rank = 0u8;

    for line in content.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (trim(key), trim(value));
        match key {
            "Type" => is_app = value == "Application",
            "Exec" if app.exec.is_empty() => app.exec = value.to_string(),
            "Icon" if app.icon.is_empty() => app.icon = value.to_string(),
            "Terminal" => app.term = value == "true",
            "NoDisplay" | "Hidden" => app.hide |= value == "true",
            _ => {
                let r = name_rank(key, locale);
                if r > rank {
                    name.clear();
                    name.push_str(value);
                    rank = r;
                }
            }
        }
        if key == "Name" && app.gname.is_none() {
            app.gname = Some(value.to_string());
        }
    }

    app.name = name;
    if app.gname.as_deref() == Some(app.name.as_str()) {
        app.gname = None;
    }
    app.hide |= !is_app || app.name.is_empty() || app.exec.is_empty();
    app
}

/// `Exec` 里 `\s \n \t \\` 还原，`%f` 之类字段码丢掉（这里没有文件/URL 可给）。
pub fn unescape_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('s') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            '%' => match chars.peek() {
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                Some(_) => {
                    chars.next();
                }
                None => out.push('%'),
            },
            _ => out.push(ch),
        }
    }
    out
}

fn data_home() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/share")
}

fn data_dirs() -> Vec<PathBuf> {
    match env::var_os("XDG_DATA_DIRS").filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => env::split_paths(&dirs).collect(),
        None => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    }
}

/// `$XDG_DATA_*` 下的 `applications` 目录，按优先级排列。
pub fn desktop_dirs() -> Vec<PathBuf> {
    std::iter::once(data_home())
        .chain(data_dirs())
        .map(|base| base.join("applications"))
        .collect()
}

fn scan_dir(dir: &Path, prefix: &str, order: &mut usize, out: &mut Vec<App>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let path = dir.join(&name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let id = format!("{prefix}{name}");
        if meta.is_dir() {
            scan_dir(&path, &format!("{id}-"), order, out);
        } else if id.len() > ".desktop".len() && id.ends_with(".desktop") {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let app = parse_desktop(&content, &id, session_locale().as_deref());
            *order += 1;
            out.push(app);
        }
    }
}

/// 扫所有 `applications` 目录：同 id 取优先级高的，隐藏的丢掉，按名字排序。
pub fn scan_desktop(dirs: &[PathBuf]) -> Vec<App> {
    let mut order = 0usize;
    let mut apps = Vec::new();
    for dir in dirs {
        scan_dir(dir, "", &mut order, &mut apps);
    }
    // 扫描顺序就是优先级；稳定排序后同 id 的第一个胜出。
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = HashSet::new();
    let mut visible: Vec<App> = Vec::with_capacity(apps.len());
    for app in apps {
        if !seen.insert(app.id.clone()) || app.hide {
            continue;
        }
        visible.push(app);
    }
    visible.sort_by(|a, b| a.name.cmp(&b.name));
    visible
}

/// `$PATH` 加上 `runner_paths`（`~` 展开）。
pub fn search_paths(extra: &str) -> Vec<PathBuf> {
    let home = env::var_os("HOME").unwrap_or_default();
    let mut out: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    for entry in extra.split(':').filter(|entry| !entry.is_empty()) {
        if let Some(rest) = entry.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with('/'))
        {
            out.push(PathBuf::from(&home).join(rest.trim_start_matches('/')));
        } else {
            out.push(PathBuf::from(entry));
        }
    }
    out
}

/// PATH 目录里的可执行普通文件（目录即使可进入也不算命令）。
pub fn scan_commands(dirs: &[PathBuf]) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;

    let mut names = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = std::fs::metadata(dir.join(&name)) else {
                continue;
            };
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "r0wm-runner-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn parses_desktop_entry() {
        let app = parse_desktop(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Files\n\
             Name[zh_CN]=文件\n\
             Exec=nautilus %U\n\
             Icon=system-file-manager\n\
             Terminal=false\n",
            "org.gnome.Nautilus.desktop",
            Some("zh_CN.UTF-8"),
        );
        assert_eq!(app.name, "文件");
        assert_eq!(app.gname.as_deref(), Some("Files"));
        assert_eq!(app.exec, "nautilus %U");
        assert_eq!(app.icon, "system-file-manager");
        assert!(!app.hide);
    }

    #[test]
    fn locale_rank_prefers_exact_then_language() {
        assert_eq!(name_rank("Name", Some("zh_CN.UTF-8")), 1);
        assert_eq!(name_rank("Name[zh_CN]", Some("zh_CN.UTF-8")), 3);
        assert_eq!(name_rank("Name[zh]", Some("zh_CN.UTF-8")), 2);
        assert_eq!(name_rank("Name[de]", Some("zh_CN.UTF-8")), 0);
        assert_eq!(name_rank("Name[zh_CN]", None), 0);
    }

    #[test]
    fn hidden_and_non_application_entries_are_dropped() {
        let hidden = parse_desktop(
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n",
            "x.desktop",
            None,
        );
        assert!(hidden.hide);
        let link = parse_desktop(
            "[Desktop Entry]\nType=Link\nName=X\nExec=x\n",
            "x.desktop",
            None,
        );
        assert!(link.hide);
    }

    #[test]
    fn only_desktop_entry_group_is_read() {
        let app = parse_desktop(
            "[Desktop Action new]\nName=Wrong\nExec=wrong\n\
             [Desktop Entry]\nType=Application\nName=Right\nExec=right\n",
            "r.desktop",
            None,
        );
        assert_eq!(app.name, "Right");
        assert_eq!(app.exec, "right");
    }

    #[test]
    fn exec_escapes_and_field_codes() {
        assert_eq!(unescape_exec("foo\\sbar %U"), "foo bar ");
        assert_eq!(unescape_exec("100%% done"), "100% done");
        assert_eq!(unescape_exec("a\\tb\\nc"), "a\tb\nc");
        assert_eq!(unescape_exec("plain"), "plain");
    }

    #[test]
    fn scans_recursively_and_dedupes_by_id() {
        let high = temp_dir("high");
        let low = temp_dir("low");
        write(
            &high.join("a.desktop"),
            "[Desktop Entry]\nType=Application\nName=FromHigh\nExec=high\n",
        );
        write(
            &high.join("devel/b.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bee\nExec=b\n",
        );
        write(
            &low.join("a.desktop"),
            "[Desktop Entry]\nType=Application\nName=FromLow\nExec=low\n",
        );
        let apps = scan_desktop(&[high.clone(), low.clone()]);
        let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
        assert_eq!(names, ["Bee", "FromHigh"]);
        assert_eq!(apps[0].id, "devel-b.desktop");
    }

    #[test]
    fn scans_executables_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("path");
        write(&dir.join("runme"), "#!/bin/sh\n");
        std::fs::set_permissions(dir.join("runme"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        write(&dir.join("plain"), "x");
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let names = scan_commands(&[dir, PathBuf::from("/nonexistent-r0wm")]);
        assert_eq!(names, ["runme"]);
    }

    #[test]
    fn search_paths_appends_extra_and_expands_tilde() {
        let home = std::env::var("HOME").unwrap_or_default();
        let paths = search_paths("~/bin:relative");
        assert!(paths.contains(&PathBuf::from(format!("{home}/bin"))));
        assert!(paths.contains(&PathBuf::from("relative")));
    }
}
