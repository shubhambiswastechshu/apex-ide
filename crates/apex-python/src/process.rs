//! Small helpers for spawning child processes consistently.

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};

use crate::error::{PythonError, Result};

/// Windows `CREATE_NO_WINDOW` process creation flag.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Maximum number of stderr bytes kept in [`PythonError::CommandFailed`].
const MAX_STDERR_CHARS: usize = 8 * 1024;

/// Creates a [`Command`] that never pops up a console window on Windows.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    hide_console(&mut cmd);
    cmd
}

/// Sets `CREATE_NO_WINDOW` on Windows; no-op elsewhere.
pub(crate) fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Makes the spawned process the leader of a new process group on Unix, so
/// [`kill_process_tree`] can terminate everything it starts. No-op on Windows,
/// where the tree is found through parent process ids instead.
pub(crate) fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Forcefully terminates `child` and every process it started.
///
/// On Unix this signals the child's process group (see [`own_process_group`]),
/// which also reaches grandchildren after the child itself has exited. On
/// Windows it runs `taskkill /T /F`, which walks the tree from the child.
/// Succeeds if everything had already exited.
pub(crate) fn kill_process_tree(child: &mut Child) -> io::Result<()> {
    let alive = matches!(child.try_wait(), Ok(None));
    #[cfg(unix)]
    {
        if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: `killpg` has no memory-safety preconditions; a stale or
            // empty group only makes it fail with ESRCH, which is ignored.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        if alive {
            use std::os::windows::process::CommandExt;
            let _ = Command::new("taskkill")
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }
    if !alive {
        return Ok(());
    }
    match child.kill() {
        Ok(()) => Ok(()),
        // Lost a race with a normal exit.
        Err(_) if matches!(child.try_wait(), Ok(Some(_))) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Renders a command line for error messages.
pub(crate) fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        s.push(' ');
        s.push_str(&arg.to_string_lossy());
    }
    s
}

/// Runs `cmd` to completion, killing it after `timeout`.
///
/// Returns `None` if it could not be spawned or timed out. Used for quick
/// probes (version queries, tool listings) where failure is not an error.
pub(crate) fn output_with_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = thread::spawn(move || read_all(stdout));
    let err_reader = thread::spawn(move || read_all(stderr));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    status.map(|status| Output {
        status,
        stdout,
        stderr,
    })
}

fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

/// Default for [`ToolOptions::idle_timeout`]: generous, because uv can be
/// silent while it downloads a Python build.
pub const DEFAULT_TOOL_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often [`run_checked`] checks for cancellation and timeouts.
const TOOL_POLL: Duration = Duration::from_millis(50);

/// How long output may keep arriving after a tool exited (a child process
/// that inherited the pipes could otherwise hold them open forever).
const TOOL_OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// Cooperative cancellation for long-running tool commands such as
/// `uv venv` or `pip install`. Clones share one flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation; commands watching this token are killed.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether [`cancel`](Self::cancel) was called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// How a long-running tool command (uv, pip, venv) is supervised.
pub struct ToolOptions<'a> {
    /// Receives each line the command prints (stdout and stderr) as it arrives.
    pub on_output: Option<&'a mut dyn FnMut(&str)>,
    /// Kills the command (and the processes it started) once cancelled; the
    /// call then fails with [`PythonError::Cancelled`].
    pub cancel: Option<CancelToken>,
    /// Kills the command if it prints nothing for this long; the call then
    /// fails with [`PythonError::TimedOut`]. `None` waits forever.
    pub idle_timeout: Option<Duration>,
}

impl Default for ToolOptions<'_> {
    /// No output callback, no cancellation, [`DEFAULT_TOOL_IDLE_TIMEOUT`].
    fn default() -> Self {
        Self {
            on_output: None,
            cancel: None,
            idle_timeout: Some(DEFAULT_TOOL_IDLE_TIMEOUT),
        }
    }
}

impl std::fmt::Debug for ToolOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolOptions")
            .field("on_output", &self.on_output.is_some())
            .field("cancel", &self.cancel)
            .field("idle_timeout", &self.idle_timeout)
            .finish()
    }
}

/// Forwards lines from `pipe` as `(is_stderr, line)` until EOF.
fn spawn_line_reader(
    pipe: impl Read + Send + 'static,
    is_stderr: bool,
    tx: Sender<(bool, String)>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n' | b'\r')) {
                        buf.pop();
                    }
                    let _ = tx.send((is_stderr, String::from_utf8_lossy(&buf).into_owned()));
                }
            }
        }
    });
}

/// Appends `line` to `tail`, keeping roughly the last [`MAX_STDERR_CHARS`].
fn push_tail(tail: &mut String, line: &str) {
    tail.push_str(line);
    tail.push('\n');
    if tail.len() > 2 * MAX_STDERR_CHARS {
        *tail = truncate_tail(tail, MAX_STDERR_CHARS);
    }
}

/// Runs `cmd` to completion and fails unless it exits successfully.
///
/// Output is streamed to [`ToolOptions::on_output`] line by line; the
/// command is killed with its process tree when cancelled or idle too long.
pub(crate) fn run_checked(mut cmd: Command, options: &mut ToolOptions<'_>) -> Result<()> {
    let description = describe(&cmd);
    own_process_group(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|source| PythonError::Spawn {
        program: cmd.get_program().to_string_lossy().into_owned(),
        source,
    })?;
    let (tx, rx) = unbounded();
    if let Some(out) = child.stdout.take() {
        spawn_line_reader(out, false, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_line_reader(err, true, tx.clone());
    }
    drop(tx);

    let (mut out_tail, mut err_tail) = (String::new(), String::new());
    let mut last_activity = Instant::now();
    let mut exited: Option<(ExitStatus, Instant)> = None;
    let mut pipes_closed = false;
    loop {
        match rx.recv_timeout(TOOL_POLL) {
            Ok((is_stderr, line)) => {
                last_activity = Instant::now();
                if let Some(on_output) = options.on_output.as_mut() {
                    on_output(&line);
                }
                let tail = if is_stderr {
                    &mut err_tail
                } else {
                    &mut out_tail
                };
                push_tail(tail, &line);
            }
            Err(RecvTimeoutError::Disconnected) => {
                pipes_closed = true;
                if exited.is_none() {
                    thread::sleep(TOOL_POLL);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if options
            .cancel
            .as_ref()
            .is_some_and(CancelToken::is_cancelled)
        {
            stop(&mut child);
            return Err(PythonError::Cancelled {
                command: description,
            });
        }
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some((status, Instant::now())),
                Ok(None) => {}
                Err(e) => {
                    stop(&mut child);
                    return Err(PythonError::io(format!("waiting for `{description}`"), e));
                }
            }
        }
        match exited {
            Some((status, at)) => {
                if pipes_closed || at.elapsed() >= TOOL_OUTPUT_GRACE {
                    return finish(description, status, &out_tail, &err_tail);
                }
            }
            None => {
                if let Some(idle) = options.idle_timeout {
                    if last_activity.elapsed() >= idle {
                        stop(&mut child);
                        return Err(PythonError::TimedOut {
                            command: description,
                            idle,
                        });
                    }
                }
            }
        }
    }
}

/// Kills `child` and its tree, then reaps it.
fn stop(child: &mut Child) {
    if let Err(e) = kill_process_tree(child) {
        tracing::warn!(%e, pid = child.id(), "killing tool process");
    }
    let _ = child.wait();
}

fn finish(description: String, status: ExitStatus, out_tail: &str, err_tail: &str) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    let mut stderr = err_tail.trim();
    if stderr.is_empty() {
        stderr = out_tail.trim();
    }
    Err(PythonError::CommandFailed {
        command: description,
        code: status.code(),
        stderr: truncate_tail(stderr, MAX_STDERR_CHARS),
    })
}

/// Keeps the last `max` characters of `s` (the end of an error log matters most).
fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(count - max).collect();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_tail_keeps_end() {
        assert_eq!(truncate_tail("abcdef", 10), "abcdef");
        assert_eq!(truncate_tail("abcdef", 3), "...def");
    }

    #[test]
    fn describe_joins_args() {
        let mut cmd = Command::new("uv");
        cmd.args(["venv", "--python", "3.12"]);
        assert_eq!(describe(&cmd), "uv venv --python 3.12");
    }

    #[test]
    fn missing_program_is_tolerated_by_probe() {
        let cmd = command("definitely-not-a-real-program-apex");
        assert!(output_with_timeout(cmd, Duration::from_secs(1)).is_none());
    }

    #[test]
    fn missing_program_is_spawn_error() {
        let cmd = command("definitely-not-a-real-program-apex");
        assert!(matches!(
            run_checked(cmd, &mut ToolOptions::default()),
            Err(PythonError::Spawn { .. })
        ));
    }

    /// A working interpreter, or `None` to skip.
    fn python() -> Option<std::path::PathBuf> {
        let found = ["python3", "python"]
            .into_iter()
            .filter_map(|n| which::which(n).ok())
            .find(|p| crate::interpreter::inspect_interpreter(p).is_ok());
        if found.is_none() {
            eprintln!("skipping: no Python interpreter found");
        }
        found
    }

    fn python_code(python: &std::path::Path, code: &str) -> Command {
        let mut cmd = command(python);
        cmd.args(["-u", "-c", code]);
        cmd
    }

    #[test]
    fn streams_output_and_reports_failures() {
        let Some(py) = python() else { return };
        let mut lines = Vec::new();
        let mut on_output = |line: &str| lines.push(line.to_owned());
        let mut options = ToolOptions {
            on_output: Some(&mut on_output),
            ..ToolOptions::default()
        };
        run_checked(python_code(&py, "print('one'); print('two')"), &mut options).expect("success");
        drop(options);
        assert_eq!(lines, ["one", "two"]);

        let err = run_checked(
            python_code(
                &py,
                "import sys; print('bad', file=sys.stderr); sys.exit(4)",
            ),
            &mut ToolOptions::default(),
        )
        .expect_err("fails");
        assert!(
            matches!(&err, PythonError::CommandFailed { code: Some(4), stderr, .. } if stderr == "bad"),
            "{err:?}"
        );
    }

    #[test]
    fn idle_commands_time_out() {
        let Some(py) = python() else { return };
        let start = Instant::now();
        let err = run_checked(
            python_code(&py, "import time; time.sleep(60)"),
            &mut ToolOptions {
                idle_timeout: Some(Duration::from_millis(500)),
                ..ToolOptions::default()
            },
        )
        .expect_err("times out");
        assert!(matches!(err, PythonError::TimedOut { .. }), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn cancelled_commands_are_killed() {
        let Some(py) = python() else { return };
        let token = CancelToken::new();
        let canceller = token.clone();
        let mut on_output = move |_: &str| canceller.cancel();
        let start = Instant::now();
        let err = run_checked(
            python_code(&py, "import time; print('working'); time.sleep(60)"),
            &mut ToolOptions {
                on_output: Some(&mut on_output),
                cancel: Some(token.clone()),
                idle_timeout: None,
            },
        )
        .expect_err("cancelled");
        assert!(matches!(err, PythonError::Cancelled { .. }), "{err:?}");
        assert!(token.is_cancelled());
        assert!(start.elapsed() < Duration::from_secs(20));
    }
}
