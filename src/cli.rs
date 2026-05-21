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
    #[arg(short = 'c', long = "check-host", conflicts_with_all = ["start_host", "list", "print"])]
    pub check_host: bool,

    /// Start a host (foreground; ^C to stop).
    #[arg(short = 's', long = "start-host", conflicts_with_all = ["check_host", "list", "print"])]
    pub start_host: bool,

    /// List the N most recent transcripts.
    #[arg(long = "list", value_name = "N", conflicts_with_all = ["check_host", "start_host", "print"])]
    pub list: Option<usize>,

    /// Show a transcript by its name (YYYY-MM-DD-hh:mm:ss).
    #[arg(short = 'p', long = "print", conflicts_with_all = ["check_host", "start_host", "list"])]
    pub print: bool,

    /// With --print, follow the transcript as new entries arrive.
    #[arg(short = 'f', long = "follow", requires = "print")]
    pub follow: bool,

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
    /// child's stdin. Without this flag the child's stdin is the PTY slave
    /// (and reads will block, as nothing is written to it).
    #[arg(long = "read-stdin")]
    pub read_stdin: bool,

    /// Positional arguments. For run mode: the command and its arguments (use `--`).
    /// For --print: the transcript name.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug)]
pub enum Mode {
    Help,
    CheckHost,
    StartHost,
    List(usize),
    Print { name: String, follow: bool },
    Run(RunArgs),
}

#[derive(Debug)]
pub struct RunArgs {
    pub whoami: String,
    pub dir: PathBuf,
    pub envs: Vec<(String, String)>,
    pub argv: Vec<String>,
    pub read_stdin: bool,
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
        return Ok(Mode::StartHost);
    }
    if let Some(n) = cli.list {
        return Ok(Mode::List(n));
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
    }))
}
