use std::process::ExitCode;

use rexec::cli::{self, Mode};
use rexec::{client, host, mcp, service};

fn main() -> ExitCode {
    let mode = match cli::parse() {
        Ok(m) => m,
        Err(err) => {
            eprintln!("rexec: {err}");
            return ExitCode::from(2);
        }
    };

    let code = match mode {
        Mode::Help => 0,
        Mode::CheckHost => client::check_host(),
        Mode::StartHost { silent, add_path } => match host::run_with_path_options(silent, add_path)
        {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("rexec: host: {err}");
                127
            }
        },
        Mode::List(n) => client::list(n),
        Mode::Print { name, follow } => client::print(&name, follow),
        Mode::Attach { color } => client::attach(color),
        Mode::Install => match service::install() {
            Ok(path) => {
                println!("installed and started {}", path.display());
                0
            }
            Err(err) => {
                eprintln!("rexec: install: {err}");
                127
            }
        },
        Mode::Run(args) => client::run(args),
        Mode::McpStdio { whoami } => mcp::run(whoami),
    };

    if (0..=255).contains(&code) {
        ExitCode::from(code as u8)
    } else {
        ExitCode::from((code & 0xFF) as u8)
    }
}
