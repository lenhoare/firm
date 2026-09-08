# MCO v0.1.2 — Reliable Processes and Model-Friendly Terminals

Status: implemented and locally validated; live ChatGPT/Qwen acceptance pending  
Date: 13 August 2026

## Purpose

Version 0.1.2 makes long-running commands and interactive terminal programs safe to
operate through ChatGPT and OpenAI Secure MCP Tunnel.

The release has two complementary execution paths:

- short synchronous commands through `exec`;
- recoverable sessions for long-running pipe processes and interactive PTYs.

No individual child process or PTY worker may make the MCO supervisor or unrelated
MCP tools unusable.

## Incident and corrected diagnosis

The first Qwen test initially appeared to show that Qwen's full-screen TUI had
poisoned MCO. The audit log established a more precise sequence:

1. interactive `qwen` started successfully in a PTY;
2. terminal input succeeded;
3. an independent `qwen --help` execution succeeded while the PTY was alive;
4. a separate `qwen -p ...` synchronous execution reached MCO's 120-second limit;
5. ChatGPT then surfaced a Python `ExceptionGroup` for later calls;
6. the later `write_file` request never reached MCO.

MCO is Rust and does not use Python `TaskGroup`s. The visible `ExceptionGroup` was
therefore an upstream wrapper failure, probably associated with the long request's
timeout boundary, rather than an exception created by MCO's PTY reader.

OpenAI documents how Secure MCP Tunnel forwards queued JSON-RPC work and returns
responses, but does not document a guaranteed maximum plugin tool-call duration.
MCO must consequently avoid holding tunnel-facing calls open for long-running work.

Reference:
[OpenAI Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)

## Design principles

1. **MCP calls are short control-plane operations.** Child-process lifetime is
   independent of the call that starts it.
2. **Pipes and PTYs are different interfaces.** Non-interactive agents and builds
   use pipes; interactive shells and TUIs use PTYs.
3. **A TUI is a screen, not a useful text log.** MCO maintains a virtual terminal
   screen as well as bounded raw output.
4. **Failures belong to one session.** Worker errors become session state and data,
   never a manager-wide panic.
5. **Cleanup is bounded and decisive.** A wedged process can always be unregistered
   and its process group is terminated as far as the operating system permits.
6. **Responses are deliberately small.** Large retained buffers must never imply
   large MCP payloads.
7. **Absence from the audit log is diagnostic evidence.** Every tool request records
   enough lifecycle information to determine whether it reached MCO.

## Execution model

### Synchronous `exec`

`exec` remains the convenient route for quick, non-interactive commands.

- default and hard maximum duration: 30 seconds;
- stdout and stderr remain separately captured and bounded;
- the entire process group is killed on timeout or completion so descendants cannot
  retain pipes;
- the tool description directs callers to process sessions for commands that may
  outlive 30 seconds.

Examples suited to `exec` include `pwd`, `git status`, small file inspections and
quick test commands. Agent runs, servers, watchers, downloads and uncertain builds
belong in process sessions.

### Persistent pipe processes

Version 0.1.2 adds six tools:

- `start_process`
- `read_process`
- `write_process`
- `signal_process`
- `close_process`
- `list_processes`

`start_process` launches the configured shell with `-lc`, creates a new process
group, arranges piped stdin/stdout/stderr, registers the session and returns its ID
immediately. It does not wait for the command to finish.

The input contains:

- `command`;
- optional `cwd`;
- an optional human-readable label.

The output contains:

- `session_id`;
- resolved `cwd`;
- redacted command summary;
- process ID and process-group ID where available;
- current lifecycle state;
- post-action advisory.

`read_process` accepts separate stdout and stderr cursors, a bounded `max_bytes`, and
an optional wait of at most five seconds. It returns separate output chunks, updated
cursors, loss/truncation flags and current process state. Polling does not affect the
child's lifetime.

`write_process` either writes bounded UTF-8 data to stdin or closes stdin. Input
content is not written to the audit log.

`signal_process` sends one named process-group signal: `interrupt`, `terminate`,
`kill`, `hangup`, `stop` or `continue`.

`close_process` performs bounded cleanup and unregisters the session. It closes
stdin, requests normal termination, waits briefly, sends `SIGKILL` if required, and
waits briefly again. Cleanup warnings are returned separately from the guarantee
that the manager has dropped the session.

`list_processes` takes a state snapshot. One failed session cannot cause the whole
listing to fail.

The intended Qwen delegation flow becomes:

```text
start_process: qwen -p "implement and verify the requested change" -o text
       -> session ID immediately
read_process: poll with short waits
       -> exit status and bounded stdout/stderr
inspect files and run independent tests
start another Qwen process with a follow-up prompt when necessary
close_process after results have been collected
```

### Interactive PTYs

The existing terminal tools remain:

- `start_terminal`
- `read_terminal`
- `write_terminal`
- `resize_terminal`
- `close_terminal`
- `list_terminals`

They are rebuilt on the same lifecycle and cleanup foundations as pipe processes.

`read_terminal` gains a `view` option:

- `screen` returns a normalized snapshot of the currently visible terminal screen;
- `stream` returns a cursor-based bounded portion of raw terminal output.

`screen` is the default because it is the useful representation for full-screen
programs such as Qwen, Vim, `top` and installers. A screen response contains the
terminal dimensions, screen generation, whether it changed since the supplied
generation, plain visible text, cursor position and current session state. It does
not contain ANSI control sequences.

`stream` remains available for line-oriented shells and diagnostics. It preserves
the existing byte-cursor, lost-output and truncation semantics.

MCO retains both a bounded raw ring buffer and a virtual terminal screen. Terminal
resize updates the PTY and virtual screen consistently. Alternate-screen buffers,
cursor movement, erasure, colour sequences and ordinary UTF-8 output must be handled
without allowing malformed escape sequences to panic a worker.

Starting a terminal returns session metadata promptly. Initial output is obtained
with `read_terminal`, avoiding a large or unpredictable `start_terminal` response.

## Session lifecycle and isolation

Pipe and PTY sessions use the following public states:

```text
starting
  -> running
  -> exited | failed | cancelled
  -> cleaned
```

Each session stores:

- session ID and kind (`process` or `terminal`);
- PID and process-group ID where available;
- command summary and working directory;
- creation and last-client-activity times;
- stdout/stderr or PTY cursors;
- exit code and signal;
- worker failure details;
- cleanup outcome and warnings.

Implementation requirements:

- runtime lock poisoning must not be handled with `.expect(...)` or an escaping
  panic;
- reader, writer and waiter errors transition only their own session to `failed`;
- a worker panic is caught at the worker boundary and recorded as a session failure;
- list operations read stored snapshots and do not poll every child fallibly;
- insertion and concurrency-limit checking are atomic from the manager's point of
  view;
- a failed start rolls back all resources and never leaves an undiscoverable
  registered session;
- the manager never waits without a deadline while holding its sessions lock;
- cleanup removes the session even if signalling, waiting or closing a descriptor
  reports an error;
- MCO shutdown attempts the same bounded process-group cleanup for every session.
