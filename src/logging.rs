//! Logging back ends: colored stderr (foreground), a file or syslog
//! (background), or the TUI's log pane.

use std::ffi::CString;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

use crossbeam_channel::Sender;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::events::UiEvent;

/// `RUST_LOG` wins when set; otherwise xcheckfs logs at `level` and its
/// dependencies at warn.
///
/// `fuser::reply` is silenced: fuser answers every FUSE_INTERRUPT (sent when
/// an application blocked in the file system is signalled) with ENOSYS, and
/// when the interrupted request has already completed, the normal case, the
/// kernel rejects that reply with ENOENT and fuser logs "Failed to send FUSE
/// reply" at error level. Processes that signal each other constantly (a
/// database server's backends) produce thousands of these; none is
/// actionable. `fuser::mnt` warns when its own cleanup unmounts a mount that
/// is already gone (unmounted externally); xcheckfs logs unmount problems
/// itself.
fn filter(level: Level) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("warn,fuser::reply=off,fuser::mnt=error,xcheckfs={}", level.as_str().to_lowercase()))
    })
}

pub fn init_stderr(level: Level, color: bool) {
    tracing_subscriber::fmt()
        .with_env_filter(filter(level))
        .with_writer(std::io::stderr)
        .with_ansi(color)
        .with_target(false)
        .with_thread_names(level >= Level::DEBUG)
        .init();
}

pub fn init_file(path: &Path, level: Level) -> anyhow::Result<()> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("log file {}: {e}", path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter(level))
        .with_writer(Mutex::new(f))
        .with_ansi(false)
        .with_target(false)
        .init();
    Ok(())
}

pub fn init_syslog(level: Level) {
    let ident = CString::new("xcheckfs").unwrap();
    // openlog keeps the pointer: leak the identity string.
    let ident = Box::leak(ident.into_boxed_c_str());
    // SAFETY: valid, 'static C string.
    unsafe { libc::openlog(ident.as_ptr(), libc::LOG_PID | libc::LOG_NDELAY, libc::LOG_DAEMON) };
    tracing_subscriber::registry().with(filter(level)).with(SyslogLayer).init();
}

/// Routes log lines to the TUI. Never blocks.
pub fn init_tui(level: Level, tx: Sender<UiEvent>) {
    tracing_subscriber::registry().with(filter(level)).with(ChannelLayer { tx }).init();
}

#[derive(Default)]
struct MessageVisitor {
    msg: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.msg, "{value:?}");
        } else {
            let _ = write!(self.msg, " {}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.msg.push_str(value);
        } else {
            let _ = write!(self.msg, " {}={value}", field.name());
        }
    }
}

fn message(event: &Event<'_>) -> String {
    let mut v = MessageVisitor::default();
    event.record(&mut v);
    v.msg
}

struct SyslogLayer;

impl<S: Subscriber> Layer<S> for SyslogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let prio = match *event.metadata().level() {
            Level::ERROR => libc::LOG_ERR,
            Level::WARN => libc::LOG_WARNING,
            Level::INFO => libc::LOG_INFO,
            _ => libc::LOG_DEBUG,
        };
        if let Ok(c) = CString::new(message(event).replace('\0', " ")) {
            // SAFETY: constant format string, valid C string argument.
            unsafe { libc::syslog(prio, c"%s".as_ptr(), c.as_ptr()) };
        }
    }
}

struct ChannelLayer {
    tx: Sender<UiEvent>,
}

impl<S: Subscriber> Layer<S> for ChannelLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let _ = self.tx.try_send(UiEvent::Log { level: *event.metadata().level(), message: message(event) });
    }
}
