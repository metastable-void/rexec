use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "rexec",
    version,
    about = "command execution aggregator for AI agents",
    long_about = None,
    disable_help_flag = false,
    arg_required_else_help = true,
)]
pub struct Cli {
    /// Check whether a host is running for this user.
    #[arg(short = 'c', long = "check-host", conflicts_with_all = ["start_host", "list", "print", "attach", "install"])]
    pub check_host: bool,

    /// Start a host (foreground; ^C to stop).
    #[arg(short = 's', long = "start-host", conflicts_with_all = ["check_host", "list", "print", "attach", "install"])]
    pub start_host: bool,

    /// Disable host command output while retaining error diagnostics.
    #[arg(long = "silent", requires = "start_host")]
    pub silent: bool,

    /// List the N most recent transcripts (default: 10).
    #[arg(
        short = 'l',
        long = "list",
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "10",
        conflicts_with_all = ["check_host", "start_host", "print", "attach", "install"]
    )]
    pub list: Option<Option<usize>>,

    /// Show a transcript by its name (YYYY-MM-DD-hh:mm:ss).
    #[arg(short = 'p', long = "print", conflicts_with_all = ["check_host", "start_host", "list", "attach", "install"])]
    pub print: bool,

    /// With --print, follow the transcript as new entries arrive.
    #[arg(short = 'f', long = "follow", requires = "print")]
    pub follow: bool,

    /// Attach to the running host and render new transcripts live.
    #[arg(long = "attach", conflicts_with_all = ["check_host", "start_host", "list", "print", "install", "mcp_stdio"])]
    pub attach: bool,

    /// Disable colors in --attach output.
    #[arg(long = "no-color", requires = "attach", conflicts_with = "force_color")]
    pub no_color: bool,

    /// Use colors in --attach output even when stdout is not a terminal.
    #[arg(long = "force-color", requires = "attach", conflicts_with = "no_color")]
    pub force_color: bool,

    /// Install, enable, and start the per-user systemd service.
    #[arg(long = "install", conflicts_with_all = ["check_host", "start_host", "list", "print", "attach", "mcp_stdio"])]
    pub install: bool,

    /// Run a stdio MCP server that forwards tool calls to the rexec host.
    /// Requires --whoami.
    #[arg(short = 'm', long = "mcp-stdio", conflicts_with_all = ["check_host", "start_host", "list", "print", "attach", "install"])]
    pub mcp_stdio: bool,

    /// Identifier of the calling agent (required when running a command).
    #[arg(long = "whoami")]
    pub whoami: Option<String>,

    /// Working directory the command should run in (required when running a command).
    #[arg(long = "dir")]
    pub dir: Option<PathBuf>,

    /// Environment overrides, in VAR=value form. Repeatable.
    #[arg(long = "env", value_name = "VAR=VAL")]
    pub env: Vec<String>,

    /// Read the client's stdin to EOF and send it to the host to be fed to the
    /// child's stdin. Without this flag the child's stdin is /dev/null.
    #[arg(long = "read-stdin")]
    pub read_stdin: bool,

    /// Kill the command if it is still running after this many seconds.
    /// Zero disables the timeout.
    #[arg(long, value_name = "SECONDS", default_value_t = 0)]
    pub timeout: u64,

    /// Positional arguments. For run mode: the command and its arguments (use `--`).
    /// For --print: the transcript name.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug)]
pub enum Mode {
    Help,
    CheckHost,
    StartHost { silent: bool },
    List(usize),
    Print { name: String, follow: bool },
    Attach { color: ColorChoice },
    Install,
    Run(RunArgs),
    McpStdio { whoami: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Never,
    Always,
}

#[derive(Debug)]
pub struct RunArgs {
    pub whoami: String,
    pub dir: PathBuf,
    pub envs: Vec<(String, String)>,
    pub argv: Vec<String>,
    pub read_stdin: bool,
    pub timeout: u64,
}

pub fn parse() -> Result<Mode, String> {
    let cli = Cli::parse();
    dispatch(cli)
}

fn dispatch(cli: Cli) -> Result<Mode, String> {
    if cli.check_host {
        return Ok(Mode::CheckHost);
    }
    if cli.start_host {
        return Ok(Mode::StartHost { silent: cli.silent });
    }
    if let Some(limit) = cli.list {
        return Ok(Mode::List(limit.unwrap_or(10)));
    }
    if cli.attach {
        let color = if cli.no_color {
            ColorChoice::Never
        } else if cli.force_color {
            ColorChoice::Always
        } else {
            ColorChoice::Auto
        };
        return Ok(Mode::Attach { color });
    }
    if cli.install {
        return Ok(Mode::Install);
    }
    if cli.print {
        if cli.args.len() != 1 {
            return Err("--print requires exactly one transcript name".into());
        }
        return Ok(Mode::Print {
            name: cli.args.into_iter().next().unwrap(),
            follow: cli.follow,
        });
    }
    if cli.mcp_stdio {
        let whoami = cli
            .whoami
            .ok_or_else(|| "--whoami is required with --mcp-stdio".to_string())?;
        if !cli.args.is_empty() {
            return Err("--mcp-stdio takes no positional arguments".into());
        }
        return Ok(Mode::McpStdio { whoami });
    }

    let whoami = cli
        .whoami
        .ok_or_else(|| "--whoami is required when running a command".to_string())?;
    let dir = cli
        .dir
        .ok_or_else(|| "--dir is required when running a command".to_string())?;
    if cli.args.is_empty() {
        return Err("no command to run (pass it after `--`)".into());
    }
    let mut envs = Vec::with_capacity(cli.env.len());
    for entry in cli.env {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| format!("--env requires VAR=value form, got: {entry}"))?;
        if k.is_empty() {
            return Err(format!("--env name is empty: {entry}"));
        }
        envs.push((k.to_string(), v.to_string()));
    }
    Ok(Mode::Run(RunArgs {
        whoami,
        dir,
        envs,
        argv: cli.args,
        read_stdin: cli.read_stdin,
        timeout: cli.timeout,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_timeout_defaults_to_zero() {
        let cli = Cli::try_parse_from(["rexec", "--whoami", "test", "--dir", "/tmp", "--", "true"])
            .unwrap();
        let Mode::Run(args) = dispatch(cli).unwrap() else {
            panic!("expected run mode");
        };
        assert_eq!(args.timeout, 0);
    }

    #[test]
    fn parses_run_timeout_seconds() {
        let cli = Cli::try_parse_from([
            "rexec",
            "--whoami",
            "test",
            "--dir",
            "/tmp",
            "--timeout",
            "17",
            "--",
            "true",
        ])
        .unwrap();
        let Mode::Run(args) = dispatch(cli).unwrap() else {
            panic!("expected run mode");
        };
        assert_eq!(args.timeout, 17);
    }

    #[test]
    fn list_defaults_to_ten() {
        let cli = Cli::try_parse_from(["rexec", "--list"]).unwrap();
        assert!(matches!(dispatch(cli).unwrap(), Mode::List(10)));
    }

    #[test]
    fn list_count_can_be_overridden() {
        let cli = Cli::try_parse_from(["rexec", "-l", "23"]).unwrap();
        assert!(matches!(dispatch(cli).unwrap(), Mode::List(23)));
    }

    #[test]
    fn long_list_value_still_works() {
        let cli = Cli::try_parse_from(["rexec", "--list", "7"]).unwrap();
        assert!(matches!(dispatch(cli).unwrap(), Mode::List(7)));
    }

    #[test]
    fn short_s_still_means_start_host() {
        let cli = Cli::try_parse_from(["rexec", "-s"]).unwrap();
        assert!(matches!(
            dispatch(cli).unwrap(),
            Mode::StartHost { silent: false }
        ));
    }

    #[test]
    fn silent_is_a_long_only_host_option() {
        let cli = Cli::try_parse_from(["rexec", "--start-host", "--silent"]).unwrap();
        assert!(matches!(
            dispatch(cli).unwrap(),
            Mode::StartHost { silent: true }
        ));
    }

    #[test]
    fn attach_color_flags_are_mapped() {
        let cli = Cli::try_parse_from(["rexec", "--attach", "--force-color"]).unwrap();
        assert!(matches!(
            dispatch(cli).unwrap(),
            Mode::Attach {
                color: ColorChoice::Always
            }
        ));
    }
}
