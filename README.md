## rexec - command execution aggregator for AI agents

```bash
rexec --help|-h

# check if the host is running
# prints: HOST RUNNING/HOST NOT FOUND
# exits: 0/127
rexec --check-host|-c

# run a command with logging and explicit working directory.
# optionally with environment variables.
# prints: whatever the command prints,
# stdout/stderr aggregated to stdout.
# if the command is not found, exits 127 with
# "$0: not found" to stderr.
# if the host is not running, exits 127 with
# "HOST NOT FOUND"
# to stderr. otherwise, the command's exit code is used.
# command's output is filtered (by host), so ANSI colors are not printed,
# and "\r" is replaced with "\n".
# --whoami and --dir are both REQUIRED.
rexec --whoami Codex --dir /path/to/working/directory [ --env VAR=val ... ] -- my-command --and arguments --as-is

# start host (^C to interrupt)
rexec --start-host|-s

# list recent N transcripts (N is required)
rexec --list <N>
# prints lines like: `2026-05-20-10:10:09 commands=19`
# where commands= is the count of command executions.

# show transcript in the same format as `--start-host`
# ANSI colors are already lost, so not displayed.
rexec --print|-p [--follow|-f] 2026-05-20-10:10:09
```

host owns a socket at `/tmp/.rexec-$UID`.
It refuses to start when the socket is listened by another host.

It accepts command execution requests in JSON. Printing is
serialized: commands are executed in the order they arrived, possibly concurrently, but printing is one-by-one, waiting for
another running commands to finish, before printing the arguments,
etc. to the human console.

JSON requests:
```json
{"whoami":"Claude Code","dir":"/path/to/directory","envs":{},"exec":["grep","-v","foo","bar.txt"]}
```

JSON responses:

```json
{"exit":0,"output":"foobar\n"}
```

The client side opens a new socket each time, so it is not ambiguous.

host is started by the human developer at the start of a session.
It prints something like for them:

```
[2026-05-11T10:57:44Z] Claude Code:/path/to/dir $ grep -v foo bar.txt
foobar
# (extra new line)
```

Host's console outputs include ANSI colors as-is.

The host opens a JSONL transcript file (creating it) at the
start of the session at `~/.rexec/YYYY-MM-DD-hh:mm:ss.jsonl`.
It contains something like:

```json
{"whoami":"Claude Code","dir":"/path/to/directory","envs":{},"exec":["grep","-v","foo","bar.txt"],"exit":0,"output":"foobar\n"}
{"whoami":"Codex","dir":"/path/to/directory","envs":{},"exec":["id","-un"],"exit":0,"output":"1000\n"}
```

Host executes each command inside a properly configured fresh
PTY. PTY outputs are collected, and displayed at the host
console, and sent back to the client.
