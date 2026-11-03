//! r0wm 二进制入口。

use std::process::ExitCode;

use r0wm::cli::{CliError, Options};
use r0wm::compositor;

fn main() -> ExitCode {
    init_logging();

    let options = match Options::from_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(CliError::Help) => {
            // `Display` 里带完整帮助文本。
            println!("{}", CliError::Help);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("r0wm: {err}");
            return ExitCode::from(2);
        }
    };

    tracing::info!(
        backend = ?options.backend,
        config = %options.config.display(),
        "启动 r0wm"
    );

    match compositor::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("r0wm=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
