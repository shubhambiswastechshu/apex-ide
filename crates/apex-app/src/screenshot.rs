//! `apex-ide --screenshot <out.png> <screen> [project] [file...]`
//!
//! Renders one screen of the real UI off-screen with Slint's software
//! renderer and saves it as a PNG, without opening any window. Used to check
//! the visual design on machines where popping up windows would disturb the
//! user. `screen` is `welcome`, `new-project` or `workspace`; for
//! `workspace`, `project` is the folder to open and each `file` (relative to
//! it) is opened in a tab, the last one active. All project folders are
//! expanded in the explorer.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, RepaintBufferType, SoftwareRenderer,
};
use slint::platform::{EventLoopProxy, Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, Rgb8Pixel};

use crate::app::App;
use crate::{AppWindow, WelcomeApi};

/// Logical size of the rendered window.
const WIDTH: u32 = 1440;
const HEIGHT: u32 = 880;

/// What to capture.
#[derive(Debug, Clone)]
pub struct Request {
    /// PNG file to write.
    pub out: PathBuf,
    /// `welcome`, `new-project` or `workspace`.
    pub screen: String,
    /// Project folder (workspace only).
    pub project: Option<PathBuf>,
    /// Files to open, relative to `project`.
    pub files: Vec<PathBuf>,
}

impl Request {
    /// Parses the arguments after `--screenshot`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let [out, screen, rest @ ..] = args else {
            return Err(
                "usage: --screenshot <out.png> <welcome|new-project|workspace> [project] [file...]"
                    .into(),
            );
        };
        if !matches!(screen.as_str(), "welcome" | "new-project" | "workspace") {
            return Err(format!("unknown screen {screen:?}"));
        }
        let (project, files) = match rest.split_first() {
            Some((p, files)) => (
                Some(std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p))),
                files.iter().map(PathBuf::from).collect(),
            ),
            None => (None, Vec::new()),
        };
        if screen == "workspace" && project.is_none() {
            return Err("the workspace screen needs a project folder".into());
        }
        Ok(Self {
            out: PathBuf::from(out),
            screen: screen.clone(),
            project,
            files,
        })
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Request>> = const { RefCell::new(None) };
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

type Task = Box<dyn FnOnce() + Send>;

struct Proxy(Arc<Mutex<VecDeque<Task>>>);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Task) -> Result<(), slint::EventLoopError> {
        self.0
            .lock()
            .map_err(|_| slint::EventLoopError::EventLoopTerminated)?
            .push_back(event);
        Ok(())
    }
}

struct OffscreenPlatform {
    window: Rc<MinimalSoftwareWindow>,
    queue: Arc<Mutex<VecDeque<Task>>>,
    start: Instant,
}

impl Platform for OffscreenPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.queue.clone())))
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let req = ACTIVE
            .with(|a| a.borrow().clone())
            .ok_or_else(|| slint::PlatformError::Other("no screenshot request".into()))?;
        self.window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
        let started = Instant::now();
        let mut acted = false;
        // Real key events need the element tree instantiated (conditional
        // elements are created when first drawn), so draw every frame then.
        let clicks = std::env::var("APEX_SCREENSHOT_CLICKS").ok();
        let live = std::env::var_os("APEX_SCREENSHOT_WINDOW_KEYS").is_some() || clicks.is_some();
        // Each scripted pointer step gets time to settle (popups animate in).
        let budget = Duration::from_millis(
            3500 + clicks.as_deref().map_or(0, |c| {
                350 * c.split(';').filter(|s| !s.is_empty()).count() as u64
            }),
        );
        let mut frame =
            vec![Rgb8Pixel::new(0, 0, 0); if live { (WIDTH * HEIGHT) as usize } else { 0 }];
        // Let workers (tree scan, interpreter discovery) report back, run
        // the scripted actions, then let animations settle.
        while started.elapsed() < budget {
            loop {
                let task = self.queue.lock().ok().and_then(|mut q| q.pop_front());
                match task {
                    Some(t) => t(),
                    None => break,
                }
            }
            slint::platform::update_timers_and_animations();
            if live {
                self.window.draw_if_needed(|renderer: &SoftwareRenderer| {
                    renderer.render(&mut frame, WIDTH as usize);
                });
            }
            if !acted && started.elapsed() > Duration::from_millis(1200) {
                acted = true;
                act(&req);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        slint::platform::update_timers_and_animations();
        let (w, h) = (WIDTH as usize, HEIGHT as usize);
        let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); w * h];
        self.window.request_redraw();
        self.window.draw_if_needed(|renderer: &SoftwareRenderer| {
            renderer.render(&mut buffer, w);
        });
        save_png(&req.out, &buffer, WIDTH, HEIGHT).map_err(|e| {
            slint::PlatformError::Other(format!("cannot save {}: {e}", req.out.display()))
        })?;
        println!("saved {}", req.out.display());
        // APEX_SCREENSHOT_HOLD_MS keeps the process alive (idle, project
        // open) so its memory can be measured from outside.
        if let Some(ms) = std::env::var("APEX_SCREENSHOT_HOLD_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            println!("holding {ms} ms (pid {})", std::process::id());
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                loop {
                    let task = self.queue.lock().ok().and_then(|mut q| q.pop_front());
                    match task {
                        Some(t) => t(),
                        None => break,
                    }
                }
                slint::platform::update_timers_and_animations();
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    }
}

/// Applies the scripted part of the request once the UI has settled.
fn act(req: &Request) {
    let Some(app) = APP.with(|a| a.borrow().clone()) else {
        return;
    };
    match req.screen.as_str() {
        "workspace" => {
            let Some(root) = req.project.clone() else {
                return;
            };
            if let Some(ws) = app.state.borrow_mut().workspace.as_mut() {
                if let Some(tree) = ws.tree.as_mut() {
                    let _ = tree.load_all();
                }
                if let Some(tree) = ws.tree.as_ref() {
                    let mut stack = vec![&tree.root];
                    while let Some(node) = stack.pop() {
                        if node.is_dir && node.path != tree.root.path {
                            ws.expanded.insert(node.path.clone());
                        }
                        stack.extend(node.children.iter());
                    }
                }
            }
            app.push_tree();
            for f in &req.files {
                app.open_file(&root.join(f));
            }
            // APEX_SCREENSHOT_KEYS types into the editor (see App::editor_script).
            if let Ok(keys) = std::env::var("APEX_SCREENSHOT_KEYS") {
                app.editor_script(&keys);
            }
            // APEX_SCREENSHOT_WINDOW_KEYS sends real key events to the window
            // (through focus, capture and bubbling; see `window_keys`).
            if let Ok(keys) = std::env::var("APEX_SCREENSHOT_WINDOW_KEYS") {
                app.editor_show_active();
                // Let the editor take the focus first.
                let app = app.clone();
                slint::Timer::single_shot(Duration::from_millis(300), move || {
                    window_keys(&app, &keys);
                });
            }
        }
        "new-project" => {}
        _ => {}
    }
    // APEX_SCREENSHOT_COMMAND=<id>[;<id>...] runs registry commands, and
    // APEX_SCREENSHOT_QUERY types into the palette / Find in Files box.
    if let Ok(ids) = std::env::var("APEX_SCREENSHOT_COMMAND") {
        for id in ids.split(';').filter(|s| !s.is_empty()) {
            app.run_command(id);
        }
    }
    if let Ok(q) = std::env::var("APEX_SCREENSHOT_QUERY") {
        app.screenshot_query(&q);
    }
    // APEX_SCREENSHOT_CLICKS="x,y;x,y" clicks the window at logical points,
    // one step every 350 ms (see `pointer_steps`).
    if let Ok(script) = std::env::var("APEX_SCREENSHOT_CLICKS") {
        let steps: Vec<PointerStep> = script.split(';').filter_map(PointerStep::parse).collect();
        let app = app.clone();
        slint::Timer::single_shot(Duration::from_millis(400), move || {
            pointer_steps(app, steps);
        });
    }
}

/// One scripted pointer action for `APEX_SCREENSHOT_CLICKS`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PointerStep {
    /// `x,y`: move there, press and release the left button.
    Click(f32, f32),
    /// `move:x,y`: hover only.
    Move(f32, f32),
    /// `wheel:x,y,dy`: scroll (with `ctrl+wheel:` holding Control).
    Wheel(f32, f32, f32, bool),
}

impl PointerStep {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let nums = |t: &str| -> Option<Vec<f32>> {
            t.split(',').map(|n| n.trim().parse::<f32>().ok()).collect()
        };
        if let Some(rest) = s.strip_prefix("move:") {
            let v = nums(rest)?;
            return (v.len() == 2).then(|| Self::Move(v[0], v[1]));
        }
        for (prefix, ctrl) in [("ctrl+wheel:", true), ("wheel:", false)] {
            if let Some(rest) = s.strip_prefix(prefix) {
                let v = nums(rest)?;
                return (v.len() == 3).then(|| Self::Wheel(v[0], v[1], v[2], ctrl));
            }
        }
        let v = nums(s)?;
        (v.len() == 2).then(|| Self::Click(v[0], v[1]))
    }
}

/// Dispatches the pointer steps one by one, 350 ms apart, through the real
/// window event path (hit testing, popups, focus), like a user would.
fn pointer_steps(app: Rc<App>, mut steps: Vec<PointerStep>) {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use slint::LogicalPosition;
    if steps.is_empty() {
        return;
    }
    let step = steps.remove(0);
    if let Some(ui) = app.ui.upgrade() {
        let window = ui.window();
        let send = |e: WindowEvent| {
            let _ = window.dispatch_event_with_result(e);
        };
        match step {
            PointerStep::Click(x, y) => {
                let position = LogicalPosition::new(x, y);
                send(WindowEvent::PointerMoved { position });
                send(WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                });
                send(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
            }
            PointerStep::Move(x, y) => send(WindowEvent::PointerMoved {
                position: LogicalPosition::new(x, y),
            }),
            PointerStep::Wheel(x, y, dy, ctrl) => {
                let position = LogicalPosition::new(x, y);
                if ctrl {
                    send(WindowEvent::KeyPressed {
                        text: Key::Control.into(),
                    });
                }
                send(WindowEvent::PointerMoved { position });
                send(WindowEvent::PointerScrolled {
                    position,
                    delta_x: 0.0,
                    delta_y: dy,
                });
                if ctrl {
                    send(WindowEvent::KeyReleased {
                        text: Key::Control.into(),
                    });
                }
            }
        }
        println!("pointer step {step:?}");
    }
    slint::Timer::single_shot(Duration::from_millis(350), move || {
        pointer_steps(app, steps);
    });
}

/// Dispatches `script` as window key events: plain chars are pressed and
/// released; `{ctrl}`, `{alt}`, `{shift}` press and hold a modifier until
/// `{/}` releases every held one; `{shift-tap}`, `{f10}`, `{enter}`,
/// `{tab}`, `{esc}`, `{up}`, `{down}`, `{pgup}`, `{pgdn}` and `{bs}` press
/// and release that key.
fn window_keys(app: &App, script: &str) {
    use slint::platform::{Key, WindowEvent};
    let Some(ui) = app.ui.upgrade() else { return };
    let window = ui.window();
    let _ = window.dispatch_event_with_result(WindowEvent::WindowActiveChanged(true));
    let send = |text: slint::SharedString, press: bool| {
        let event = if press {
            WindowEvent::KeyPressed { text }
        } else {
            WindowEvent::KeyReleased { text }
        };
        let _ = window.dispatch_event_with_result(event);
    };
    let mut held: Vec<Key> = Vec::new();
    let mut rest = script;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix('{') {
            let Some(end) = after.find('}') else { break };
            let name = &after[..end];
            rest = &after[end + 1..];
            let key = match name {
                "ctrl" => Key::Control,
                "alt" => Key::Alt,
                "shift" => Key::Shift,
                "shift-tap" | "f10" | "enter" | "tab" | "esc" | "up" | "down" | "pgup" | "pgdn"
                | "bs" => {
                    let k = match name {
                        "f10" => Key::F10,
                        "enter" => Key::Return,
                        "tab" => Key::Tab,
                        "esc" => Key::Escape,
                        "up" => Key::UpArrow,
                        "down" => Key::DownArrow,
                        "pgup" => Key::PageUp,
                        "pgdn" => Key::PageDown,
                        "bs" => Key::Backspace,
                        _ => Key::Shift,
                    };
                    send(k.into(), true);
                    send(k.into(), false);
                    continue;
                }
                "/" => {
                    for k in held.drain(..).rev() {
                        send(k.into(), false);
                    }
                    continue;
                }
                _ => continue,
            };
            send(key.into(), true);
            held.push(key);
        } else {
            rest = &rest[c.len_utf8()..];
            send(c.to_string().into(), true);
            send(c.to_string().into(), false);
        }
    }
}

/// Called by `app::run` once the UI is wired. Switches to the requested
/// screen when a screenshot is being taken.
pub fn on_ready(app: &Rc<App>, ui: &AppWindow) {
    let Some(req) = ACTIVE.with(|a| a.borrow().clone()) else {
        return;
    };
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    // Never touch the user's settings file (recent projects, layout).
    app.state.borrow_mut().settings_path =
        Some(std::env::temp_dir().join("apex-ide-screenshot-settings.toml"));
    // APEX_SCREENSHOT_THEME=<theme id> renders with that theme.
    if let Ok(id) = std::env::var("APEX_SCREENSHOT_THEME") {
        let index = app.state.borrow().theme_ids.iter().position(|t| *t == id);
        if let Some(i) = index.and_then(|i| i32::try_from(i).ok()) {
            ui.global::<crate::AppApi>().invoke_select_theme(i);
        }
    }
    if req.screen == "new-project" {
        ui.global::<WelcomeApi>().invoke_new_project();
    }
}

/// Installs the off-screen platform and runs the app for `req`.
pub fn run(req: Request) -> anyhow::Result<()> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let platform = OffscreenPlatform {
        window,
        queue: Arc::new(Mutex::new(VecDeque::new())),
        start: Instant::now(),
    };
    slint::platform::set_platform(Box::new(platform))
        .map_err(|e| anyhow::anyhow!("cannot install the off-screen platform: {e:?}"))?;
    let initial = if req.screen == "workspace" {
        req.project.clone()
    } else {
        None
    };
    ACTIVE.with(|a| *a.borrow_mut() = Some(req));
    crate::app::run(initial)
}

fn save_png(path: &Path, pixels: &[Rgb8Pixel], w: u32, h: u32) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    writer.write_image_data(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let r = Request::parse(&args(&["o.png", "welcome"])).unwrap();
        assert_eq!(r.screen, "welcome");
        assert!(r.project.is_none());
        let r = Request::parse(&args(&["o.png", "workspace", "proj", "main.py"])).unwrap();
        assert!(r.project.unwrap().is_absolute());
        assert_eq!(r.files, [PathBuf::from("main.py")]);
        assert!(Request::parse(&args(&["o.png", "workspace"])).is_err());
        assert!(Request::parse(&args(&["o.png", "nope"])).is_err());
        assert!(Request::parse(&args(&["o.png"])).is_err());
    }

    #[test]
    fn parses_pointer_steps() {
        assert_eq!(
            PointerStep::parse(" 10,20 "),
            Some(PointerStep::Click(10.0, 20.0))
        );
        assert_eq!(
            PointerStep::parse("move:1.5,2"),
            Some(PointerStep::Move(1.5, 2.0))
        );
        assert_eq!(
            PointerStep::parse("ctrl+wheel:5,6,-120"),
            Some(PointerStep::Wheel(5.0, 6.0, -120.0, true))
        );
        assert_eq!(PointerStep::parse("1,2,3"), None);
        assert_eq!(PointerStep::parse("x,y"), None);
    }
}
