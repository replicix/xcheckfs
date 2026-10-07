//! TUI state and input handling. Drawing lives in `ui`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::Receiver;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::model::{
    History, LogEntry, LogFilter, Nav, OpEntry, OpRow, Ring, Scroll, WINDOW_SECS, compute_rows, fmt_clock,
    sanitize,
};
use super::{MountInfo, TuiContext};
use crate::config::MismatchMode;
use crate::events::UiEvent;
use crate::policy::{Action, Mismatch, Policy, Rule};
use crate::stats::{InFlightOp, Stats};

/// Bound on events handled per UI tick, so a flood cannot starve redraws.
pub const MAX_EVENTS_PER_TICK: usize = 10_000;
/// Capacity of the (tracing) log pane.
const LOG_LINES: usize = 1000;
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const STATUS_TTL: Duration = Duration::from_secs(5);

/// Panes that can take the keyboard focus (scrolling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    OpLog,
    Stats,
    Inflight,
    Mismatches,
    Logs,
}

/// What currently captures the keyboard besides the panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    /// Typing the operations-log filter.
    Filter,
    ConfirmQuit,
    ConfirmDetach,
    /// Choosing the scope of an allow rule for the frozen mismatch.
    AllowChoice,
    /// Full details of the selected mismatch in the mismatches pane.
    MismatchDetail,
}

pub struct Status {
    pub text: String,
    pub error: bool,
    at: Instant,
}

pub struct App {
    pub stats: Arc<Stats>,
    pub policy: Arc<Policy>,
    events: Receiver<UiEvent>,
    shutdown: Arc<AtomicBool>,
    pub info: MountInfo,

    pub history: History,
    last_sample: Option<Instant>,
    pub rows: Vec<OpRow>,
    /// Latency columns over the last window (true) or since start.
    pub windowed: bool,

    pub oplog: Ring<OpEntry>,
    pub filter: LogFilter,
    /// The raw (not lower-cased) text being edited.
    pub filter_input: String,
    /// Keep the view at the newest entry.
    pub follow: bool,
    pub log_scroll: Scroll,
    pub logs: Ring<LogEntry>,
    pub logs_scroll: Scroll,
    pub stats_scroll: Scroll,
    pub inflight_scroll: Scroll,
    /// Selected mismatch; `None` tracks the newest one.
    pub mm_sel: Option<u64>,

    /// Refreshed every tick.
    pub mismatches: Vec<Arc<Mismatch>>,
    pub pending: Vec<Arc<Mismatch>>,
    pub inflight: Vec<InFlightOp>,
    pub rules: usize,
    /// Index into `pending` shown by the freeze modal.
    pub pend_sel: usize,

    pub focus: Focus,
    pub overlay: Overlay,
    /// Mismatch an open allow / detach dialog refers to.
    pub target: Option<Arc<Mismatch>>,
    pub status: Option<Status>,
    pub quit: bool,
    /// Set by the renderer: the panes currently visible (Tab order) and the
    /// number of rows of the focused pane (page size).
    pub cycle: Vec<Focus>,
    pub page: usize,
}

impl App {
    pub fn new(ctx: TuiContext) -> App {
        App {
            stats: ctx.stats,
            policy: ctx.policy,
            events: ctx.events,
            shutdown: ctx.shutdown,
            info: ctx.info,
            history: History::default(),
            last_sample: None,
            rows: Vec::new(),
            windowed: true,
            oplog: Ring::new(ctx.history),
            filter: LogFilter::default(),
            filter_input: String::new(),
            follow: true,
            log_scroll: Scroll::default(),
            logs: Ring::new(LOG_LINES),
            logs_scroll: Scroll::default(),
            stats_scroll: Scroll::default(),
            inflight_scroll: Scroll::default(),
            mm_sel: None,
            mismatches: Vec::new(),
            pending: Vec::new(),
            inflight: Vec::new(),
            rules: 0,
            pend_sel: 0,
            focus: Focus::OpLog,
            overlay: Overlay::None,
            target: None,
            status: None,
            quit: false,
            cycle: vec![Focus::OpLog],
            page: 10,
        }
    }

    pub fn shutting_down(&self) -> bool {
        self.quit || self.shutdown.load(Relaxed)
    }

    pub fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    /// The status message while it is fresh.
    pub fn live_status(&self) -> Option<&Status> {
        self.status.as_ref().filter(|s| s.at.elapsed() < STATUS_TTL)
    }

    /// Drains the event channel (bounded). Returns true if more is waiting.
    pub fn drain_events(&mut self) -> bool {
        for _ in 0..MAX_EVENTS_PER_TICK {
            match self.events.try_recv() {
                Ok(ev) => self.ingest(ev),
                Err(_) => return false,
            }
        }
        true
    }

    fn ingest(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Op(e) => {
                let entry = OpEntry::from_event(&e);
                // A paused view must not drift while new rows arrive.
                if !self.follow && self.filter.matches(&entry) {
                    self.log_scroll.off = self.log_scroll.off.saturating_add(1);
                }
                self.oplog.push(entry);
            }
            UiEvent::Mismatch(m) => {
                self.set_status(
                    format!("MISMATCH #{} {} {}: {}", m.id, m.op.name(), m.kind.name(), m.path),
                    true,
                );
            }
            UiEvent::Log { level, message } => {
                if self.logs_scroll.off > 0 {
                    self.logs_scroll.off = self.logs_scroll.off.saturating_add(1);
                }
                self.logs.push(LogEntry {
                    ts: fmt_clock(SystemTime::now()),
                    level,
                    message: sanitize(&message),
                });
            }
        }
    }

    /// Takes the once-a-second snapshot when due and refreshes the cached
    /// views of policy and in-flight state.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if self
            .last_sample
            .is_none_or(|t| now.duration_since(t) >= SAMPLE_EVERY)
        {
            self.last_sample = Some(now);
            self.history.push(self.stats.snapshot(), now);
            self.recompute_rows();
        }
        // Newest first.
        self.mismatches = self.policy.history().into_iter().rev().collect();
        self.pending = self
            .policy
            .pending()
            .into_iter()
            .map(|p| p.mismatch.clone())
            .collect();
        self.pend_sel = self.pend_sel.min(self.pending.len().saturating_sub(1));
        self.inflight = self.stats.inflight.snapshot();
        self.rules = self.policy.rules().len();
        if self.pending.is_empty() && matches!(self.overlay, Overlay::AllowChoice) {
            self.overlay = Overlay::None;
        }
    }

    fn recompute_rows(&mut self) {
        self.rows = match self.history.window(WINDOW_SECS) {
            Some(w) => compute_rows(&w, self.windowed),
            None => Vec::new(),
        };
    }

    /// The mismatch the freeze modal currently shows.
    pub fn current_pending(&self) -> Option<&Arc<Mismatch>> {
        self.pending.get(self.pend_sel)
    }

    /// Index of the selected mismatch in `mismatches` (newest first).
    pub fn mm_index(&self) -> usize {
        self.mm_sel
            .and_then(|id| self.mismatches.iter().position(|m| m.id == id))
            .unwrap_or(0)
    }

    pub fn selected_mismatch(&self) -> Option<&Arc<Mismatch>> {
        self.mismatches.get(self.mm_index())
    }

    // ---- input ----

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            // A second Ctrl-C confirms.
            if self.overlay == Overlay::ConfirmQuit {
                self.quit_now();
            } else {
                self.overlay = Overlay::ConfirmQuit;
            }
            return;
        }
        match self.overlay {
            Overlay::None => self.on_key_normal(k),
            Overlay::Help => self.overlay = Overlay::None,
            Overlay::MismatchDetail => {
                if matches!(k.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.overlay = Overlay::None;
                }
            }
            Overlay::Filter => self.on_key_filter(k),
            Overlay::ConfirmQuit => match k.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.quit_now(),
                _ => self.overlay = Overlay::None,
            },
            Overlay::ConfirmDetach => match k.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.detach_now(),
                _ => self.overlay = Overlay::None,
            },
            Overlay::AllowChoice => self.on_key_allow(k),
        }
    }

    /// Leaves the UI; frozen operations are released first, otherwise the
    /// caller's unmount would hang on the blocked file system threads.
    fn quit_now(&mut self) {
        for m in std::mem::take(&mut self.pending) {
            self.policy.resolve(m.id, Action::Continue);
        }
        self.quit = true;
    }

    fn detach_now(&mut self) {
        self.overlay = Overlay::None;
        self.target = None;
        self.policy.detach();
        // Frozen operations would otherwise wait for a decision forever.
        for m in &self.pending {
            self.policy.resolve(m.id, Action::Detach);
        }
        self.set_status("secondary detached: pass-through to the primary only", false);
    }

    fn on_key_filter(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                self.filter_input.clear();
                self.filter.text.clear();
                self.overlay = Overlay::None;
            }
            KeyCode::Enter => self.overlay = Overlay::None,
            KeyCode::Backspace => {
                self.filter_input.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => self.filter_input.clear(),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.filter_input.push(c),
            _ => {}
        }
        self.filter.text = self.filter_input.to_lowercase();
        self.log_scroll.off = 0;
    }

    fn on_key_allow(&mut self, k: KeyEvent) {
        let with_path = match k.code {
            KeyCode::Char('1') => false,
            KeyCode::Char('2') => true,
            _ => {
                self.overlay = Overlay::None;
                return;
            }
        };
        self.overlay = Overlay::None;
        if let Some(m) = self.target.take() {
            let rule = Rule::from_mismatch(&m, with_path);
            let scope = if with_path { "this path" } else { "everywhere" };
            self.resolve(&m, Action::Allow(rule), &format!("allowed ({scope})"));
        }
    }

    fn resolve(&mut self, m: &Mismatch, action: Action, what: &str) {
        if self.policy.resolve(m.id, action) {
            self.set_status(format!("#{}: {what}", m.id), false);
        } else {
            self.set_status(format!("#{} is no longer pending", m.id), true);
        }
    }

    /// Keys of the freeze modal. Returns true when the key was consumed.
    fn on_key_modal(&mut self, k: KeyEvent) -> bool {
        let Some(m) = self.current_pending().cloned() else {
            return false;
        };
        let n = self.pending.len();
        match k.code {
            KeyCode::Left => self.pend_sel = (self.pend_sel + n - 1) % n,
            KeyCode::Right => self.pend_sel = (self.pend_sel + 1) % n,
            KeyCode::Char('c') => self.resolve(&m, Action::Continue, "continue"),
            KeyCode::Char('a') => {
                self.target = Some(m);
                self.overlay = Overlay::AllowChoice;
            }
            KeyCode::Char('r') if m.retryable => self.resolve(&m, Action::Retry, "retry"),
            KeyCode::Char('r') => self.set_status("this operation is not retryable", true),
            KeyCode::Char('s') if m.resyncable => self.resolve(&m, Action::Resync, "resync from primary"),
            KeyCode::Char('s') => self.set_status("this object cannot be resynced", true),
            KeyCode::Char('e') => self.resolve(&m, Action::Fail, "fail with EIO"),
            KeyCode::Char('d') => {
                self.target = Some(m);
                self.overlay = Overlay::ConfirmDetach;
            }
            _ => return false,
        }
        true
    }

    fn on_key_normal(&mut self, k: KeyEvent) {
        if !self.pending.is_empty() && self.on_key_modal(k) {
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.overlay = Overlay::ConfirmQuit,
            KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Overlay::Help,
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Char('m') => self.cycle_mode(),
            KeyCode::Char('D') => {
                if self.stats.detached.load(Relaxed) {
                    self.set_status("already detached", true);
                } else {
                    self.target = None;
                    self.overlay = Overlay::ConfirmDetach;
                }
            }
            KeyCode::Char('w') => {
                self.windowed = !self.windowed;
                self.recompute_rows();
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.log_scroll.off = 0;
                }
            }
            KeyCode::Char('e') => {
                self.filter.errors_only = !self.filter.errors_only;
                self.log_scroll.off = 0;
            }
            KeyCode::Char('/') => self.overlay = Overlay::Filter,
            KeyCode::Esc => {
                self.filter_input.clear();
                self.filter.text.clear();
                self.filter.errors_only = false;
            }
            KeyCode::Enter if self.focus == Focus::Mismatches && !self.mismatches.is_empty() => {
                self.overlay = Overlay::MismatchDetail;
            }
            KeyCode::Up | KeyCode::Char('k') => self.navigate(Nav::Up),
            KeyCode::Down | KeyCode::Char('j') => self.navigate(Nav::Down),
            KeyCode::PageUp => self.navigate(Nav::PageUp),
            KeyCode::PageDown => self.navigate(Nav::PageDown),
            KeyCode::Home | KeyCode::Char('g') => self.navigate(Nav::Top),
            KeyCode::End | KeyCode::Char('G') => self.navigate(Nav::Bottom),
            _ => {}
        }
    }

    fn cycle_focus(&mut self, dir: isize) {
        let n = self.cycle.len() as isize;
        if n == 0 {
            return;
        }
        // A focus that is no longer visible continues from either end.
        let start = if dir > 0 { -1 } else { 0 };
        let cur = self
            .cycle
            .iter()
            .position(|f| *f == self.focus)
            .map_or(start, |i| i as isize);
        let next = (cur + dir).rem_euclid(n) as usize;
        self.focus = self.cycle[next];
    }

    fn cycle_mode(&mut self) {
        let next = match self.policy.mode() {
            MismatchMode::Resync => MismatchMode::Log,
            MismatchMode::Log => MismatchMode::Fail,
            MismatchMode::Fail => MismatchMode::Freeze,
            MismatchMode::Freeze | MismatchMode::Detach => MismatchMode::Resync,
        };
        self.policy.set_mode(next);
        self.set_status(format!("mismatch mode: {}", next.name()), false);
    }

    fn navigate(&mut self, nav: Nav) {
        let page = self.page;
        match self.focus {
            Focus::OpLog => {
                self.log_scroll.nav(nav, page, true);
                // Scrolling back pauses the tail; returning to it resumes.
                self.follow = self.log_scroll.off == 0 && !matches!(nav, Nav::Up | Nav::PageUp | Nav::Top);
                if self.follow {
                    self.log_scroll.off = 0;
                }
            }
            Focus::Stats => self.stats_scroll.nav(nav, page, false),
            Focus::Inflight => self.inflight_scroll.nav(nav, page, false),
            Focus::Logs => self.logs_scroll.nav(nav, page, true),
            Focus::Mismatches => self.mm_nav(nav, page),
        }
    }

    fn mm_nav(&mut self, nav: Nav, page: usize) {
        if self.mismatches.is_empty() {
            return;
        }
        let last = self.mismatches.len() - 1;
        let cur = self.mm_index();
        let idx = match nav {
            Nav::Up => cur.saturating_sub(1),
            Nav::Down => (cur + 1).min(last),
            Nav::PageUp => cur.saturating_sub(page),
            Nav::PageDown => (cur + page).min(last),
            Nav::Top => 0,
            Nav::Bottom => last,
        };
        self.mm_sel = if idx == 0 {
            None
        } else {
            Some(self.mismatches[idx].id)
        };
    }
}
