# rexec

Command-execution aggregator for AI coding agents.

`rexec` runs a small per-user host that several coding agents (Claude Code,
Codex, Gemini CLI, ...) can share. Agents call a thin client which forwards
the command to the host; the host runs it inside a fresh PTY, streams raw
output to the human's console with a one-line banner, sends ANSI-stripped
output back to the calling agent, and journals every run to a JSONL
transcript.

The design assumes one human supervisor and several agents working
concurrently in the same project. They get a single, ordered, human-readable
log of what was executed, and the supervisor can interrupt or replay anything
after the fact.

## Features

- **One shared console.** Output from concurrent agents is serialised one
  command at a time, so the human's screen stays readable.
- **Sanitised output to agents.** ANSI escape sequences are stripped and CR is
  normalised to LF — agents see clean text, not progress-bar redraws.
- **Live raw output to the human.** The host console preserves ANSI colours
  and any TTY behaviour.
- **Fresh PTY per command.** Each command runs as if from a small (80x24)
  terminal with sane termios.
- **Verified, reusable connections.** Every client connection begins with a
  required ping/pong handshake, every command is immediately preceded by a
  fresh ping/pong, and each socket can carry multiple sequential commands.
- **Resilient concurrent MCP.** The stdio server pools verified connections,
  runs overlapping tool calls concurrently, and reconnects forever after host
  outages without placing its connection deadline on command execution.
- **JSONL transcripts.** Every run is appended to
  `~/.rexec/YYYY-MM-DD-HH:MM:SS.jsonl` and can be listed, printed, or
  followed live.
- **Cooperative abort.** The CLI sends `{"action":"abort"}` on catchable
  termination (Ctrl-C, SIGTERM, panic, or a dropped request guard); an actual
  socket disconnect is detected as EOF. The host SIGTERMs the spawned process
  group then SIGKILLs after a brief grace.
- **Optional user service.** `rexec --install` installs and starts a per-user
  systemd service. The host can still run directly as `rexec --start-host`;
  ^C cleans it up. Single static binary, no Python in the build graph, builds
  cleanly for `musl` targets.

## Install

From crates.io:

```bash
cargo install rexec
```

From source:

```bash
git clone https://github.com/metastable-void/rexec.git
cd rexec
cargo install --path .
```

Unix only. Tested on Linux (glibc and musl). BSDs and macOS should work.

### Upgrading to 0.5

Version 0.5 makes the ping/pong handshake mandatory and keeps verified sockets
open for multiple commands. The wire protocol is therefore not compatible with
0.4 clients or hosts. Upgrade both ends together and restart any running host;
after installing a new binary with `rexec --install`, restart the user service
so it is not still serving the old protocol.

```bash
rexec --install
systemctl --user restart rexec.service
```

For a directly started foreground host, stop the old process and launch
`rexec --start-host` again before using a 0.5 client.

## Quick start

On a systemd Linux host, install and start the user service:

```bash
rexec --install
```

The service runs the host silently. Use `rexec --attach` whenever you want a
live console view.

Alternatively, start the host directly in one terminal:

```bash
rexec --start-host
```

It prints the socket path and transcript file:

```
rexec host listening on /tmp/.rexec-1000
rexec transcript: ~/.rexec/2026-05-21-09:42:18.jsonl
```

From an agent (or any other shell), run commands through it:

```bash
rexec --whoami "Claude Code" --dir "$PWD" -- grep -v foo bar.txt
```

The host prints a banner and the command's raw output to its own console;
the client receives the ANSI-stripped output on stdout and exits with the
command's exit code. The banner includes `TO=0` for unlimited execution or
`TO=<seconds>` for a requested command timeout.

## CLI

```text
rexec --help | -h
rexec --check-host | -c
rexec --start-host | -s
rexec --start-host --silent
rexec --list | -l [N]
rexec --print | -p [--follow | -f] <transcript-name>
rexec --attach [--no-color | --force-color]
rexec --install
rexec --mcp-stdio | -m --whoami <NAME>
rexec --whoami <NAME> --dir <DIR> [--env VAR=VAL ...] [--read-stdin] [--timeout SECONDS] -- <command> [args...]
```

### Run a command

```bash
rexec --whoami Codex --dir /path/to/repo --env RUST_LOG=debug -- cargo test --workspace
```

| Flag           | Required | Description |
|----------------|----------|-------------|
| `--whoami`     | yes      | Identifier of the calling agent. Appears in the host banner and transcript. |
| `--dir`        | yes      | Working directory for the child. The host `chdir`s here. |
| `--env`        | no       | `VAR=VAL` pairs, repeatable. Added to (not replacing) the host's environment. |
| `--read-stdin` | no       | Read the client's stdin to EOF (must be valid UTF-8) and forward it to the child. The host attaches a pipe to the child's fd 0 and closes it after writing, so the child sees a real EOF. Without this flag fd 0 is `/dev/null`. |
| `--timeout`    | no       | Kill the command's PTY process group after this many seconds. Defaults to `0`, which disables the timeout. |
| `--`           | yes      | Separator; everything after is the command to execute. |

`argv[0]` is resolved via `PATH` (`execvp` semantics). Output to stdout is
the command's combined stdout+stderr, ANSI-stripped, CR-normalised. The
live and replayed transcript command line includes `TO=<seconds>` for the
requested command timeout; `TO=0` denotes unlimited execution. The
client's exit code is:

| Code  | Meaning |
|-------|---------|
| *N*   | The command's exit code. |
| 128+N | The command was killed by signal *N*. |
| 127   | Host not running (`HOST NOT FOUND` on stderr), command not found (`<arg0>: not found` on stderr), spawn failure, or a transport error. |
| 124   | The command exceeded `--timeout`. |
| 2     | CLI usage error. |

### Check whether the host is up

```bash
rexec --check-host
```

Prints `HOST RUNNING` (exit 0) or `HOST NOT FOUND` (exit 127).
The check succeeds only after the host returns a valid pong; merely connecting
to a process that owns the socket is not considered a healthy host.

### Start the host

```bash
rexec --start-host
```

Foreground; ^C to stop. Refuses to start if another host already owns the
per-user socket. On exit the socket file is removed. Add `--silent` to suppress
command banners/output and routine lifecycle notices; errors and diagnostics
remain visible on stderr. `-s` remains the shorthand for `--start-host`.

### List transcripts

```bash
rexec --list
rexec -l 25
```

Lists up to 10 recent transcripts by default, newest first. Each transcript's
name is its creation date. Supply a count to change the maximum:

```
2026-05-21-09:42:18 commands=19
2026-05-20-17:03:55 commands=4
```

### Attach to the host

```bash
rexec --attach
rexec --attach --no-color
rexec --attach --force-color
```

Follows transcript entries completed after attachment until the host exits or
the attach client is terminated. It does not replay entries that existed before
attachment. It exits 127 with `HOST NOT FOUND` on stderr when no host is
running. New entries receive raw pre-filter PTY output when color is enabled, so
command colors are preserved without storing a second raw transcript. Color is
enabled when stdout is a terminal, disabled for redirected output or by
`--no-color`, and forced by `--force-color`.

### Install the user service

```bash
rexec --install
```

Writes `rexec.service` under `$XDG_CONFIG_HOME/systemd/user` (or
`~/.config/systemd/user`), runs `systemctl --user daemon-reload`, then enables
and starts the service. The unit uses the absolute path of the `rexec`
executable that performed the installation and starts the host with
`--start-host --silent`.

### Print a transcript

```bash
rexec --print 2026-05-21-09:42:18
rexec --print --follow 2026-05-21-09:42:18
```

Renders the transcript in the same format the host prints to its console.
`--follow` (`-f`) streams new entries as they arrive.

### Run as a stdio MCP server

```bash
rexec --mcp-stdio --whoami "Claude Code"
```

Speaks the Model Context Protocol (MCP) over stdio. The agent launches `rexec
--mcp-stdio --whoami <NAME>` as a subprocess. The stdio server maintains a pool
of ping/pong-verified host connections and reuses idle connections for later
tool calls. When no verified connection is available and no command owns one,
the background reconnect loop retries forever with one-second intervals. Idle
pooled sockets are checked for disconnects once per second. `--whoami` is fixed
for the session.

Each MCP tool call waits at most 15 seconds to acquire a verified connection if
the host is unavailable. If the host has not returned, `exec` reports `HOST NOT
FOUND` as an MCP error and `check_host` returns `HOST NOT FOUND`; the background
loop continues retrying after that individual call finishes. The 15-second
connection deadline ends as soon as a connection is acquired. It never limits
the command itself: execution is unlimited when the tool's `timeout` is `0`, or
is limited only by that requested command timeout.

Immediately before sending each command request, the client sends another ping
and requires pong. A failed pre-command ping invalidates that pooled socket and
is safely retried within the same connection window because no command request
has yet been sent. Once a request is sent, it is never replayed automatically.
Every tool call can also establish a connection itself, so recovery does not
depend solely on the background worker and a worker delay cannot permanently
strand a live stdio MCP process.

Overlapping MCP `exec` calls run concurrently. Idle verified connections are
reused; when all pooled connections are executing, the server opens another
ping/pong-verified connection instead of waiting for an unrelated command.
Socket I/O runs on Tokio's blocking pool, so a long command does not block the
stdio transport. Active connections are never probed, timed out, or replaced
by the reconnect loop merely because their commands take a long time.

Two tools are exposed:

| Tool         | Purpose |
|--------------|---------|
| `exec`       | Run a command via the host. Arguments: `dir` (string, required), `argv` (array of strings, required), `envs` (array of `"VAR=VAL"` strings, optional), `stdin` (UTF-8 string, optional), and `timeout` (seconds, optional, defaults to `0`/disabled). Returns a JSON object with `exit`, `output`, and an optional `error` field; `isError` is set when the command exited non-zero or could not be found. |
| `check_host` | Acquires a ping/pong-verified pooled connection, waiting up to 15 seconds if the host is unavailable. Returns `"HOST RUNNING"` or `"HOST NOT FOUND"`. |

The MCP server itself does no work other than forwarding — a host started
directly or through the user service must be running (or return within the
15-second connection window) for `exec` calls to succeed.

Configuration example (Claude Code's `mcp_servers` block, similar shape for
other MCP clients):

```json
{
  "mcpServers": {
    "rexec": {
      "command": "rexec",
      "args": ["--mcp-stdio", "--whoami", "Claude Code"]
    }
  }
}
```

## Architecture

The host owns a Unix domain socket at `/tmp/.rexec-$UID` (mode `0600`, owner
only). Every client connection begins with a required ping/pong handshake and
may carry multiple sequential commands. One-shot CLI invocations close their
verified connection after one operation; the stdio MCP server keeps and reuses
its pooled connections until either side disconnects.

```
+----------------+              +---------------------------+              +---------------+
|  agent / shell |              |          host             |    forkpty   |    child      |
|  rexec client  | ---JSONL---> |  accept; per-conn worker  | -----------> | (fresh PTY)   |
|                | <--JSONL---- |  PTY -> host stdout (raw) |              +---------------+
+----------------+              |  PTY -> client (filtered) |
                                |  append transcript line   |
                                +---------------------------+
```

- **Concurrency vs. ordering.** Commands run concurrently, but *printing to
  the host console* is serialised. The Nth-arriving request gets sequence
  number N and its console worker waits its turn to print the banner and
  output. If the console is occupied, later raw output is buffered; command
  completion and the CLI/MCP response do not wait for that printing turn, so
  a slow command never blocks a fast one from completing on the client side.
- **Connections.** A per-connection host worker requires ping/pong first and
  then processes command requests sequentially until EOF, requiring another
  ping/pong immediately before each request. Concurrency uses multiple
  connections. The MCP server retains idle verified connections in a pool and
  creates another when all pooled connections are active.
- **PTY.** Each command runs under a fresh 80x24 PTY with sane termios
  (B38400, `CS8`, no input/output processing). This gives realistic TTY
  behaviour for tools that detect a terminal, without leaking the host's
  controlling terminal.
- **Input.** When `stdin` is supplied, fd 0 is a pipe that is closed after the
  supplied bytes are written (including when the supplied buffer is empty), so
  the child sees EOF. Otherwise fd 0 is `/dev/null`; the PTY remains the
  command's controlling terminal but cannot accidentally act as stdin.
- **Environment.** The child inherits the host's environment, with anything
  passed via `--env` added or overriding. `HOME`, `PATH`, etc. come from the
  host process unless the request supplies them.
- **Filtering.** Output sent back to the client passes through an
  ANSI-stripping filter: CSI sequences, OSC strings (terminated by BEL or
  ST), and single-character ESC sequences are removed; CR becomes LF so
  redraws appear as separate lines. The host's own console sees the raw
  PTY bytes.

## Protocol

Clients and the host exchange JSONL: one JSON object per line. A connection has
the following lifecycle:

1. The client sends **Ping** as the mandatory first line.
2. The host confirms **Pong** and keeps the connection open.
3. The verified connection either carries sequential **Ping → Pong → Request →
   Response** command exchanges, or switches to the long-lived **Attach** event
   stream.
4. While a request is active, the client may send **Abort**. The next request
   must not be sent until the current response arrives; command pipelining on a
   single connection is not supported. Use separate connections for concurrent
   commands, as the MCP pool does.

If ping is missing, pong is invalid or absent, or either side disconnects, the
individual connection is discarded. This does not require restarting an MCP
session: each later tool call can establish and verify a replacement even if
the background reconnect worker is unavailable. The CLI closes its verified
connection after its one operation. MCP retains healthy idle connections for
subsequent tool calls. The host rejects any Request that was not immediately
authorized by a fresh ping/pong exchange.

### 1. Ping / Pong handshake (client ↔ host)

Every connection begins with:

```json
{"action":"ping"}
```

The host must reply:

```json
{"result":"pong"}
```

The host then keeps the connection open. A command client sends this same
ping/pong exchange again immediately before every Request; an attach client may
send its attach action after the opening handshake, and `--check-host` simply
closes its connection after confirming the opening pong. A successful socket
connect without a valid pong is not accepted as proof of host health.

### 2. Request (client → host)

After the opening handshake and a fresh pre-command ping/pong, send a request
line:

```json
{"whoami":"Claude Code","dir":"/path/to/repo","envs":{"RUST_LOG":"debug"},"exec":["grep","-v","foo","bar.txt"],"timeout":30}
```

| Field    | Type                  | Description |
|----------|-----------------------|-------------|
| `whoami` | string                | Identifier of the calling agent. |
| `dir`    | string                | Working directory; the host `chdir`s the child here. |
| `envs`   | object<string,string> | Environment variables added to the child. Omittable. |
| `exec`   | array<string>         | `argv[0]` is the program (resolved via `PATH`); rest are arguments. Must be non-empty. |
| `stdin`  | string (optional)     | If present, the host attaches a pipe to the child's fd 0, writes these bytes (UTF-8), and closes the write end so the child sees EOF. If absent, fd 0 is `/dev/null`. |
| `timeout` | integer (optional)   | Maximum runtime in seconds. Defaults to `0`, which disables the timeout. On expiry the host terminates the command's PTY process group. |

After its response is received, another request may be sent on the same
connection. Requests on one connection are strictly sequential.

### 3. Response (host → client)

The host writes one line back when the command completes:

```json
{"exit":0,"output":"foobar\n"}
```

| Field    | Type   | Description |
|----------|--------|-------------|
| `exit`   | int    | Exit code. `128+N` if the child was killed by signal *N*; `127` if not found or spawn failed. |
| `output` | string | Filtered combined stdout+stderr (ANSI-stripped, CR→LF). |
| `error`  | string (optional) | Tag describing why the run did not complete normally. See below. |

`error` values currently defined:

| Tag            | Meaning |
|----------------|---------|
| `not_found`    | `execvp` reported `ENOENT` (or similar) for `argv[0]`. |
| `spawn_failed` | `chdir`, `setenv`, or `fork` failed before exec. |
| `aborted`      | The host killed the child because the client sent `abort` or disconnected. |
| `timeout`      | The command exceeded its requested timeout; the response exit code is 124. |

### 4. Abort (client → host, optional)

At any point after the request, the client may send:

```json
{"action":"abort"}
```

The one-shot CLI client sends this automatically on any catchable termination
(SIGINT, SIGTERM, SIGHUP, panic, or a dropped request guard before the response
is read). On receipt the host signals the child's process group with SIGTERM,
then SIGKILL after a 200 ms grace, and tags the transcript entry with
`"error":"aborted"`. Clients that do not send Abort are still safe: EOF while
a command is active is treated as an abort. After a normal response, the
connection remains available for another request.

### 5. Attach events (host → client)

After completing the ping/pong handshake, an attach client sends
`{"action":"attach","ansi":true}`. The connection then becomes an attach-only
event stream; it does not also carry command requests. The host sends each
entry completed after attachment as a JSONL event:

```json
{"event":"transcript","entry":{"whoami":"Codex","dir":"/tmp","envs":{},"exec":["printf","ok\\n"],"exit":0,"output":"ok\\n","time":"2026-05-21T09:42:24Z"}}
```

Entries completed before attachment are not sent. When `ansi` is true, new
events carry raw PTY output captured before filtering; it is not persisted.
When the host shuts down it closes all attach streams.

## Host console output

Per command, the host prints:

```
[2026-05-21T09:42:18Z] Claude Code:/path/to/repo $ TO=0 grep -v foo bar.txt
foobar
                                  <- trailing blank line separates commands
```

Output between the banner and the trailing blank line is the raw PTY
stream, including any ANSI colour and cursor control the command produced.

## Transcript format

`~/.rexec/YYYY-MM-DD-HH:MM:SS.jsonl` is JSONL with one object per executed
command, in arrival order:

```json
{"whoami":"Claude Code","dir":"/path/to/repo","envs":{},"exec":["grep","-v","foo","bar.txt"],"exit":0,"output":"foobar\n","time":"2026-05-21T09:42:18Z"}
{"whoami":"Codex","dir":"/path/to/repo","envs":{},"exec":["id","-un"],"exit":0,"output":"alice\n","time":"2026-05-21T09:42:24Z"}
```

The file is opened with `O_CREAT | O_EXCL`; the host refuses to start if a
transcript with the same name already exists. Entries are flushed after
every append, so the transcript is durable up to the last completed
command. The stored `timeout` field is omitted when it is zero for backward
readability, but live console banners and `--print`/`--attach` rendering always
show `TO=<seconds>`, including `TO=0` for unlimited commands.

## Security notes

- The socket is created at mode `0600` and lives in `/tmp/.rexec-$UID`,
  i.e. it is accessible only to the owning user. Anyone with that user's
  privileges can run arbitrary commands through the host; treat the host
  as equivalent to a shell running as you.
- The host does not authenticate clients beyond filesystem permissions on
  the socket. Do not start a host as `root` unless you want any process
  running as that user to be able to execute anything.
- Output from the child is rendered raw on the host console. A hostile
  command can emit terminal escape sequences against the human's terminal;
  this matches `bash`'s default behaviour and is preserved deliberately so
  the human sees what the command actually produced.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- Mozilla Public License, Version 2.0 ([LICENSE-MPL](LICENSE-MPL))

at your option.
