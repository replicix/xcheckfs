//! Interactive terminal UI (ratatui). See `run`.
//!
//! `model` holds the pure data logic (sample history, rates, filters),
//! `app` the state and key handling, `ui` the rendering.

mod app;
mod model;
mod ui;

use std::io::{Stdout, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::config::{CheckLevel, EngineConfig};
use crate::events::UiEvent;
use crate::policy::Policy;
use crate::stats::Stats;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Static facts about the mount, shown in the header.
#[derive(Clone, Debug)]
pub struct MountInfo {
    pub mountpoint: PathBuf,
    pub primary: PathBuf,
    pub secondary: PathBuf,
    pub check: CheckLevel,
    pub engine: EngineConfig,
    pub rules_path: Option<PathBuf>,
    pub control_socket: Option<PathBuf>,
}

pub struct TuiContext {
    pub stats: Arc<Stats>,
    pub policy: Arc<Policy>,
    /// Op events, mismatches and log lines (tracing output is routed here
    /// while the TUI owns the terminal).
    pub events: Receiver<UiEvent>,
    /// Maximum number of op-log lines kept for scrolling.
    pub history: usize,
    pub info: MountInfo,
    /// Set by the main thread when the file system went away (unmounted
    /// externally); the TUI must then exit promptly.
    pub shutdown: Arc<AtomicBool>,
}

/// Redraw / input poll interval (~10 fps).
const TICK: Duration = Duration::from_millis(100);

/// True while the terminal is in TUI mode, so the panic hook knows whether
/// it has anything to restore.
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
}

/// Puts the terminal in raw mode on the alternate screen; the guard
/// restores it when dropped (normal exit, error return or unwinding).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<(Terminal<CrosstermBackend<Stdout>>, TerminalGuard)> {
        install_panic_hook();
        enable_raw_mode()?;
        let guard = TerminalGuard;
        ACTIVE.store(true, Ordering::SeqCst);
        execute!(stdout(), EnterAlternateScreen)?;
        let mut term = Terminal::new(CrosstermBackend::new(stdout()))?;
        term.clear()?;
        Ok((term, guard))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if ACTIVE.swap(false, Ordering::SeqCst) {
            restore_terminal();
        }
    }
}

/// Restores the terminal before the previous panic hook prints its message
/// (it would be lost on the alternate screen otherwise).
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if ACTIVE.swap(false, Ordering::SeqCst) {
            restore_terminal();
        }
        prev(info);
    }));
}

/// Runs the TUI until the user quits or `shutdown` is set. Restores the
/// terminal on exit (also on error).
pub fn run(ctx: TuiContext) -> anyhow::Result<()> {
    let mut app = app::App::new(ctx);
    let (mut terminal, _guard) = TerminalGuard::enter()?;
    while !app.shutting_down() {
        let backlog = app.drain_events();
        app.tick();
        terminal.draw(|f| ui::draw(f, &mut app))?;
        // Handle all queued input before the next redraw; skip waiting when
        // the event channel still has a backlog.
        let mut wait = if backlog { Duration::ZERO } else { TICK };
        while event::poll(wait)? {
            if let Event::Key(k) = event::read()? {
                app.on_key(k);
            }
            wait = Duration::ZERO;
        }
    }
    Ok(())
}
