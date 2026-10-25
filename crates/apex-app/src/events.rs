//! Messages from worker threads to the UI thread.
//!
//! Workers never touch Slint objects. They push an [`AppEvent`] through an
//! [`EventSink`]; the sink wakes the Slint event loop with
//! `slint::invoke_from_event_loop`, and the UI thread drains the queue in
//! batches (see [`EventQueue::drain`]). Many sends between two wake-ups
//! cost a single wake-up, so chatty producers such as a running program's
//! output do not flood the event loop.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use apex_core::FileTree;
use apex_lsp::{LspClient, LspEvent};
use apex_python::{Interpreter, RunEvent};
use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::db_panel::DbReply;
use crate::project_flow::{CreatedProject, Progress};

/// How a status message should look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// Neutral information.
    Info = 0,
    /// Something needs attention.
    Warning = 1,
    /// Something failed.
    Error = 2,
    /// Something finished well.
    Success = 3,
}

/// Something a worker thread wants the UI thread to know.
pub enum AppEvent {
    /// Interpreter discovery finished.
    Interpreters {
        /// Interpreters found, best first.
        found: Vec<Interpreter>,
        /// Where uv is, if installed.
        uv: Option<PathBuf>,
    },
    /// A New Project step reported progress.
    CreateProgress(Progress),
    /// The New Project pipeline ended.
    CreateFinished(Result<CreatedProject, String>),
    /// A project tree finished building.
    TreeBuilt {
        /// Which open project it belongs to.
        generation: u64,
        /// The tree, or why it could not be built.
        result: Result<FileTree, String>,
    },
    /// Output or exit of a running program.
    Run {
        /// Which run session it belongs to.
        run_id: u64,
        /// What happened.
        event: RunEvent,
    },
    /// The language server finished starting (or could not start).
    LspStarted {
        /// Which open project it belongs to.
        generation: u64,
        /// The ready client and its display name.
        result: Result<(Arc<LspClient>, String), crate::lsp::LspStartError>,
    },
    /// A notification from the language server.
    Lsp {
        /// Which open project it belongs to.
        generation: u64,
        /// The event.
        event: LspEvent,
    },
    /// A reply from the database worker.
    Db(DbReply),
    /// Which recent-project folders no longer exist (`true` = missing).
    RecentChecked(Vec<(PathBuf, bool)>),
    /// A message for the status bar.
    Notice(NoticeKind, String),
    /// Interpreters found for the open project's drop-down.
    ProjectInterpreters {
        /// Which open project it belongs to.
        generation: u64,
        /// Interpreters found, best first.
        found: Vec<Interpreter>,
    },
    /// Completion replies and package installs for the editor.
    Editor(crate::editor::EditorEvent),
}

/// The sending half, cheap to clone into worker threads.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<AppEvent>,
    scheduled: Arc<AtomicBool>,
}

/// The receiving half, owned by the UI thread.
pub struct EventQueue {
    rx: Receiver<AppEvent>,
    scheduled: Arc<AtomicBool>,
}

/// Creates a connected sink and queue.
pub fn channel() -> (EventSink, EventQueue) {
    let (tx, rx) = unbounded();
    let scheduled = Arc::new(AtomicBool::new(false));
    (
        EventSink {
            tx,
            scheduled: scheduled.clone(),
        },
        EventQueue { rx, scheduled },
    )
}

/// Upper bound of events handled per wake-up, so a flood of output cannot
/// starve rendering; the rest is handled on the next turn of the loop.
const MAX_EVENTS_PER_TICK: usize = 2_000;

impl EventSink {
    /// Queues an event and makes sure the UI thread will look at the queue.
    /// Does nothing once the UI has shut down.
    pub fn send(&self, event: AppEvent) {
        if self.tx.send(event).is_err() {
            return;
        }
        self.wake();
    }

    /// Shorthand for a status bar notice.
    pub fn notice(&self, kind: NoticeKind, text: impl Into<String>) {
        self.send(AppEvent::Notice(kind, text.into()));
    }

    fn wake(&self) {
        if !self.scheduled.swap(true, Ordering::AcqRel)
            && slint::invoke_from_event_loop(dispatch_pending).is_err()
        {
            // The event loop is gone (shutting down); nothing will drain.
            self.scheduled.store(false, Ordering::Release);
        }
    }
}

impl EventQueue {
    /// Takes up to one tick's worth of events. If more are waiting, another
    /// wake-up is scheduled through `sink`.
    pub fn drain(&self, sink: &EventSink) -> Vec<AppEvent> {
        self.scheduled.store(false, Ordering::Release);
        let events: Vec<AppEvent> = self.rx.try_iter().take(MAX_EVENTS_PER_TICK).collect();
        if !self.rx.is_empty() {
            sink.wake();
        }
        events
    }
}

thread_local! {
    static DISPATCHER: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
}

/// Installs the UI-thread function that drains the queue. Must be called on
/// the thread that runs the Slint event loop.
pub fn set_dispatcher(dispatch: impl Fn() + 'static) {
    DISPATCHER.with(|d| *d.borrow_mut() = Some(Box::new(dispatch)));
}

/// Removes the dispatcher (breaking the reference it holds) at shutdown.
pub fn clear_dispatcher() {
    DISPATCHER.with(|d| d.borrow_mut().take());
}

fn dispatch_pending() {
    DISPATCHER.with(|d| {
        if let Some(dispatch) = d.borrow().as_ref() {
            dispatch();
        }
    });
}
