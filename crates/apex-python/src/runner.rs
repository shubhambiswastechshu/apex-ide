//! Running Python programs with streamed output.
//!
//! [`Runner::spawn`] starts the interpreter and returns a [`RunHandle`] whose
//! channel yields [`RunEvent`]s from background threads: output lines as they
//! arrive, then exactly one [`RunEvent::Exited`], always last.
//!
//! On Unix the program runs in its own process group, so [`RunHandle::kill`]
//! also stops the processes it started (such as Django's autoreloader child);
//! on Windows the process tree is killed with `taskkill /T`.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::error::{PythonError, Result};
use crate::process::{command, kill_process_tree, own_process_group};

/// How long output may keep arriving after the program itself exited.
///
/// A grandchild that inherited stdout/stderr keeps the pipes open; after this
/// grace period [`RunEvent::Exited`] is sent anyway and later output dropped.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// What to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTarget {
    /// `python <script>`.
    Script(PathBuf),
    /// `python -m <module>`.
    Module(String),
    /// `python -c <code>`.
    Code(String),
}

/// A request to run Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The interpreter to use (usually the project venv's).
    pub python: PathBuf,
    /// The script, module or code to run.
    pub target: RunTarget,
    /// Arguments passed to the program (after the target).
    pub args: Vec<String>,
    /// Working directory; inherits the IDE's when `None`.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables, applied after the defaults
    /// (`PYTHONUNBUFFERED=1`, `PYTHONIOENCODING=utf-8`) so they can override them.
    pub env: Vec<(String, String)>,
}

impl RunRequest {
    /// A request with no arguments, working directory or extra environment.
    pub fn new(python: impl Into<PathBuf>, target: RunTarget) -> Self {
        Self {
            python: python.into(),
            target,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }
}

/// Something that happened in a running program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    /// One line of standard output, without the line terminator.
    Stdout(String),
    /// One line of standard error, without the line terminator.
    Stderr(String),
    /// The process ended; `None` if it was terminated by a signal. Always
    /// the last event.
    Exited(Option<i32>),
}

/// The event sender, shared by the output readers and the exit watcher. The
/// watcher takes it out when it sends [`RunEvent::Exited`], so nothing can be
/// sent after that.
type EventSlot = Arc<Mutex<Option<Sender<RunEvent>>>>;

fn emit(slot: &Mutex<Option<Sender<RunEvent>>>, event: RunEvent) {
    if let Some(tx) = lock(slot).as_ref() {
        let _ = tx.send(event);
    }
}

/// Starts Python processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Runner;

impl Runner {
    /// Spawns `request` and starts streaming its output.
    pub fn spawn(request: RunRequest) -> Result<RunHandle> {
        let mut cmd = command(&request.python);
        own_process_group(&mut cmd);
        match &request.target {
            RunTarget::Script(path) => {
                cmd.arg(path);
            }
            RunTarget::Module(module) => {
                cmd.arg("-m").arg(module);
            }
            RunTarget::Code(code) => {
                cmd.arg("-c").arg(code);
            }
        }
        cmd.args(&request.args)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .envs(request.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &request.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd.spawn().map_err(|source| PythonError::Spawn {
            program: request.python.display().to_string(),
            source,
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().map(spawn_stdin_writer);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, rx) = unbounded();
        let slot: EventSlot = Arc::new(Mutex::new(Some(tx)));
        let readers = [
            stdout.map(|out| spawn_reader(out, Arc::clone(&slot), RunEvent::Stdout)),
            stderr.map(|err| spawn_reader(err, Arc::clone(&slot), RunEvent::Stderr)),
        ];

        let child = Arc::new(Mutex::new(child));
        let exited = Arc::new(AtomicBool::new(false));
        {
            let child = Arc::clone(&child);
            let exited = Arc::clone(&exited);
            thread::spawn(move || {
                let code = wait_for_exit(&child);
                let readers: Vec<_> = readers.into_iter().flatten().collect();
                let deadline = Instant::now() + OUTPUT_GRACE;
                while readers.iter().any(|r| !r.is_finished()) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                for reader in readers {
                    if reader.is_finished() {
                        let _ = reader.join();
                    } else {
                        tracing::debug!(pid, "output pipe still held by a child process");
                    }
                }
                // Hold the slot while marking the exit so no reader can slip a
                // line in after `Exited`.
                let mut slot = lock(&slot);
                exited.store(true, Ordering::SeqCst);
                if let Some(tx) = slot.take() {
                    let _ = tx.send(RunEvent::Exited(code));
                }
            });
        }

        Ok(RunHandle {
            pid,
            child,
            stdin,
            exited,
            events: rx,
        })
    }
}

/// Reads lines from `pipe` and forwards them until EOF. Keeps draining even
/// after the events are closed, so a child never blocks on a full pipe.
fn spawn_reader(
    pipe: impl Read + Send + 'static,
    slot: EventSlot,
    wrap: fn(String) -> RunEvent,
) -> thread::JoinHandle<()> {
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
                    emit(&slot, wrap(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
        }
    })
}

/// Writes queued input to the child on its own thread, so a program that is
/// not reading stdin can never block the caller. The thread ends (closing the
/// pipe) once the sender is dropped and the queue is written, or a write fails.
fn spawn_stdin_writer(mut pipe: ChildStdin) -> Sender<Vec<u8>> {
    let (tx, rx) = unbounded::<Vec<u8>>();
    thread::spawn(move || {
        for chunk in rx {
            if let Err(e) = pipe.write_all(&chunk).and_then(|()| pipe.flush()) {
                tracing::debug!(%e, "writing to program stdin");
                break;
            }
        }
    });
    tx
}

/// Polls the child so the lock is never held while blocking, letting
/// [`RunHandle::kill`] in at any time.
fn wait_for_exit(child: &Mutex<Child>) -> Option<i32> {
    loop {
        match lock(child).try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {}
            Err(_) => return None,
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running (or finished) Python process.
///
/// Dropping the handle does **not** kill the process; call [`kill`](Self::kill).
#[derive(Debug)]
pub struct RunHandle {
    pid: u32,
    child: Arc<Mutex<Child>>,
    stdin: Option<Sender<Vec<u8>>>,
    exited: Arc<AtomicBool>,
    events: Receiver<RunEvent>,
}

impl RunHandle {
    /// The channel of output and exit events. Clone it to consume elsewhere.
    pub fn events(&self) -> &Receiver<RunEvent> {
        &self.events
    }

    /// The operating-system process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the run is still in progress (becomes `false` just before
    /// [`RunEvent::Exited`] is sent).
    pub fn is_running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    /// Forcefully terminates the process and the processes it started.
    /// Succeeds if everything already exited.
    pub fn kill(&self) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        kill_process_tree(&mut lock(&self.child))
            .map_err(|e| PythonError::io(format!("killing process {}", self.pid), e))
    }

    /// Queues text for the program's standard input (e.g. for `input()`).
    ///
    /// Never blocks: a background thread does the writing. Fails once stdin
    /// was closed or an earlier write failed (for example, the program exited).
    pub fn write_stdin(&self, text: &str) -> Result<()> {
        let closed = || {
            PythonError::io(
                format!("writing to process {}", self.pid),
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin is closed"),
            )
        };
        let stdin = self.stdin.as_ref().ok_or_else(closed)?;
        stdin.send(text.as_bytes().to_vec()).map_err(|_| closed())
    }

    /// Closes standard input once queued input is written, signalling
    /// end-of-file to the program.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    const TIMEOUT: Duration = Duration::from_secs(30);

    /// A working interpreter, or `None` to skip.
    fn python() -> Option<PathBuf> {
        let found = ["python3", "python"]
            .into_iter()
            .filter_map(|n| which::which(n).ok())
            .find(|p| crate::interpreter::inspect_interpreter(p).is_ok());
        if found.is_none() {
            eprintln!("skipping: no Python interpreter found");
        }
        found
    }

    /// Collects events until `Exited`.
    fn collect(handle: &RunHandle) -> (Vec<String>, Vec<String>, Option<i32>) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match handle
                .events()
                .recv_timeout(left)
                .expect("event before timeout")
            {
                RunEvent::Stdout(l) => out.push(l),
                RunEvent::Stderr(l) => err.push(l),
                RunEvent::Exited(code) => return (out, err, code),
            }
        }
    }

    #[test]
    fn streams_stdout_stderr_and_exit_code() {
        let Some(py) = python() else { return };
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("prog.py");
        std::fs::write(
            &script,
            "import os, sys\nprint('hello')\nprint('ünïcode ✓')\nprint('oops', file=sys.stderr)\n\
             print(sys.argv[1:])\nprint(os.environ['APEX_TEST'])\nprint(os.environ['PYTHONUNBUFFERED'])\n\
             print(os.path.basename(os.getcwd()))\nsys.exit(3)\n",
        )
        .expect("write");
        let workdir = tmp.path().join("work dir");
        std::fs::create_dir(&workdir).expect("mkdir");

        let mut req = RunRequest::new(&py, RunTarget::Script(script));
        req.args = vec!["a b".into(), "c".into()];
        req.cwd = Some(workdir);
        req.env = vec![("APEX_TEST".into(), "42".into())];
        let handle = Runner::spawn(req).expect("spawn");
        let (out, err, code) = collect(&handle);

        assert_eq!(
            out,
            ["hello", "ünïcode ✓", "['a b', 'c']", "42", "1", "work dir"]
        );
        assert_eq!(err, ["oops"]);
        assert_eq!(code, Some(3));
        assert!(!handle.is_running());
        assert!(handle.kill().is_ok(), "kill after exit is a no-op");
    }

    #[test]
    fn runs_code_and_modules_with_stdin() {
        let Some(py) = python() else { return };
        let mut handle = Runner::spawn(RunRequest::new(
            &py,
            RunTarget::Code("name = input()\nprint('hi ' + name)".into()),
        ))
        .expect("spawn");
        handle.write_stdin("apex\n").expect("stdin");
        handle.close_stdin();
        let (out, _, code) = collect(&handle);
        assert_eq!(out, ["hi apex"]);
        assert_eq!(code, Some(0));

        let handle = Runner::spawn(RunRequest::new(
            &py,
            RunTarget::Module("apex_no_such_module".into()),
        ))
        .expect("spawn");
        let (_, err, code) = collect(&handle);
        assert_ne!(code, Some(0));
        assert!(
            err.iter().any(|l| l.contains("apex_no_such_module")),
            "{err:?}"
        );

        let handle = Runner::spawn(RunRequest {
            args: vec!["-c".into(), "print(1 + 1)".into()],
            ..RunRequest::new(&py, RunTarget::Module("timeit".into()))
        })
        .expect("spawn");
        let (out, _, code) = collect(&handle);
        assert_eq!(code, Some(0));
        assert!(out.iter().any(|l| l.contains("loop")), "{out:?}");
    }

    #[test]
    fn kill_stops_a_long_running_program() {
        let Some(py) = python() else { return };
        let handle = Runner::spawn(RunRequest::new(
            &py,
            RunTarget::Code("import time\nprint('started')\ntime.sleep(60)".into()),
        ))
        .expect("spawn");
        let first = handle.events().recv_timeout(TIMEOUT).expect("first line");
        assert_eq!(first, RunEvent::Stdout("started".into()));
        assert!(handle.is_running());

        let start = Instant::now();
        handle.kill().expect("kill");
        let (_, _, code) = collect(&handle);
        assert!(start.elapsed() < Duration::from_secs(20));
        assert_ne!(code, Some(0));
        assert!(!handle.is_running());
    }

    /// Python code that starts a sleeping grandchild sharing our stdout and
    /// stderr (like Django's autoreloader), then prints `started <its pid>`.
    const SPAWN_GRANDCHILD: &str = "import subprocess, sys, time\n\
         p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n\
         print('started', p.pid)\n";

    fn grandchild_pid(line: &str) -> u32 {
        line.strip_prefix("started ")
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("unexpected line {line:?}"))
    }

    fn process_alive(pid: u32) -> bool {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("tasklist");
            c.args(["/NH", "/FI", &format!("PID eq {pid}")]);
            c
        } else {
            let mut c = std::process::Command::new("kill");
            c.args(["-0", &pid.to_string()]);
            c
        };
        match cmd.stderr(Stdio::null()).output() {
            Ok(out) if cfg!(windows) => {
                String::from_utf8_lossy(&out.stdout).contains(&format!(" {pid} "))
            }
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }

    #[test]
    fn kill_stops_child_processes_too() {
        let Some(py) = python() else { return };
        let code = format!("{SPAWN_GRANDCHILD}time.sleep(60)\n");
        let handle = Runner::spawn(RunRequest::new(&py, RunTarget::Code(code))).expect("spawn");
        let RunEvent::Stdout(first) = handle.events().recv_timeout(TIMEOUT).expect("first line")
        else {
            panic!("expected output first");
        };
        let grandchild = grandchild_pid(&first);
        assert!(process_alive(grandchild));

        handle.kill().expect("kill");
        let (_, _, code) = collect(&handle);
        assert_ne!(code, Some(0));
        assert!(!handle.is_running());
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_alive(grandchild) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(grandchild), "grandchild survived the kill");
    }

    #[test]
    fn exit_is_reported_while_a_grandchild_holds_the_pipes() {
        let Some(py) = python() else { return };
        let handle = Runner::spawn(RunRequest::new(
            &py,
            RunTarget::Code(SPAWN_GRANDCHILD.into()),
        ))
        .expect("spawn");
        let start = Instant::now();
        let (out, _, code) = collect(&handle);
        assert_eq!(out.len(), 1, "{out:?}");
        let grandchild = grandchild_pid(&out[0]);
        assert_eq!(code, Some(0));
        assert!(start.elapsed() < Duration::from_secs(20));
        assert!(
            handle.events().try_recv().is_err(),
            "nothing may follow Exited"
        );
        // Clean up the orphaned grandchild.
        let _ = if cfg!(windows) {
            std::process::Command::new("taskkill")
                .args(["/F", "/PID", &grandchild.to_string()])
                .output()
        } else {
            std::process::Command::new("kill")
                .args(["-9", &grandchild.to_string()])
                .output()
        };
    }

    #[test]
    fn stdin_writes_never_block_and_fail_after_close() {
        let Some(py) = python() else { return };
        // The program never reads stdin, so writing 1 MiB directly would block.
        let mut handle = Runner::spawn(RunRequest::new(
            &py,
            RunTarget::Code("import time\nprint('up')\ntime.sleep(60)".into()),
        ))
        .expect("spawn");
        let start = Instant::now();
        handle
            .write_stdin(&"x".repeat(1024 * 1024))
            .expect("queued");
        assert!(start.elapsed() < Duration::from_secs(5));
        handle.close_stdin();
        assert!(handle.write_stdin("late").is_err());
        handle.kill().expect("kill");
        collect(&handle);
    }

    #[test]
    fn missing_interpreter_is_a_spawn_error() {
        let req = RunRequest::new(
            Path::new("no-such-python-apex"),
            RunTarget::Code("pass".into()),
        );
        assert!(matches!(Runner::spawn(req), Err(PythonError::Spawn { .. })));
    }
}
