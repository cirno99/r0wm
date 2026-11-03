//! 命令行入口。
//!
//! 只支持一个配置文件与一个后端选择，不需要通用参数解析库。

use std::path::PathBuf;

/// 运行后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// 嵌套进现有 Wayland/X11 会话，用于开发与集成冒烟。
    Winit,
    /// 真机：DRM + libinput + libseat。
    Udev,
}

/// 启动参数。
#[derive(Debug, Clone)]
pub struct Options {
    pub backend: Backend,
    /// 默认 `$XDG_CONFIG_HOME/r0wm/config.kdl`。
    pub config: PathBuf,
}

#[derive(Debug)]
pub enum CliError {
    /// 参数未知或缺少取值。
    Usage(String),
    /// 请求了编译期未启用的后端。
    BackendNotBuilt(Backend),
    /// 用户请求帮助。
    Help,
}

impl Options {
    /// 解析命令行参数（不含程序名）。
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Self, CliError> {
        let mut backend = default_backend();
        let mut config = default_config_path();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Err(CliError::Help),
                "-c" | "--config" => {
                    let path = args
                        .next()
                        .ok_or_else(|| CliError::Usage("--config 需要一个路径".into()))?;
                    config = PathBuf::from(path);
                }
                "--backend" => {
                    let name = args
                        .next()
                        .ok_or_else(|| CliError::Usage("--backend 需要取值".into()))?;
                    backend = match name.as_str() {
                        "winit" => Backend::Winit,
                        "udev" => Backend::Udev,
                        other => {
                            return Err(CliError::Usage(format!("未知后端：{other}")));
                        }
                    };
                }
                other => return Err(CliError::Usage(format!("未知参数：{other}"))),
            }
        }

        Ok(Self { backend, config })
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Usage(msg) => write!(f, "{msg}"),
            CliError::BackendNotBuilt(backend) => write!(
                f,
                "后端 {backend:?} 未编入本二进制（请用 --features {} 重新构建）",
                match backend {
                    Backend::Winit => "winit",
                    Backend::Udev => "udev",
                }
            ),
            CliError::Help => write!(f, "{}", HELP),
        }
    }
}

const HELP: &str = "\
r0wm —— 以 g0wm 为对标基线的 Wayland 合成器

用法：r0wm [选项]

  -h, --help            显示本帮助
  -c, --config <路径>   配置文件（默认 $XDG_CONFIG_HOME/r0wm/config.kdl）
      --backend <名字>  winit（嵌套，默认）| udev（真机 DRM）

配置文件使用 niri 风格的 KDL，示例见 examples/config.kdl。";

fn default_backend() -> Backend {
    if cfg!(feature = "winit") {
        Backend::Winit
    } else {
        Backend::Udev
    }
}

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("r0wm").join("config.kdl")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_backend_and_config() {
        let opts = Options::from_args(args(&["--backend", "udev", "-c", "/tmp/a.toml"])).unwrap();
        assert_eq!(opts.backend, Backend::Udev);
        assert_eq!(opts.config, PathBuf::from("/tmp/a.toml"));
    }

    #[test]
    fn unknown_arg_is_usage_error() {
        assert!(matches!(
            Options::from_args(args(&["--nope"])),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn help_is_its_own_branch() {
        assert!(matches!(
            Options::from_args(args(&["-h"])),
            Err(CliError::Help)
        ));
    }
}
