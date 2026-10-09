//! Control socket: lets `xcheckfs ctl` inspect a running mount and resolve
//! frozen mismatches without the TUI (background mode, CI, scripts).
//!
//! Protocol: one command per line, one JSON object per response line.
//! Commands: `status`, `stats`, `mismatches [N]`, `pending`,
//! `resolve <id> <continue|retry|resync|fail|detach|allow|allow-path>`,
//! `mode <resync|log|fail|freeze|detach>`, `detach`, `rules`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::MismatchMode;
use crate::policy::{Action, Policy, Rule};
use crate::stats::{OpKind, Stats};

/// Default socket path for a mount point (lexical, never touches the mount,
/// so it works while the file system is frozen).
pub fn default_socket(mountpoint: &Path) -> PathBuf {
    let abs = std::path::absolute(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let mut parts: Vec<String> = Vec::new();
    for c in abs.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let full = format!("/{}", parts.join("/"));
    // sun_path is ~108 bytes: last component (shortened) + hash of the path.
    let last: String = parts.last().map(|l| l.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).take(24).collect()).unwrap_or_else(|| "root".into());
    let name = format!("{last}-{:016x}", xxhash_rust::xxh3::xxh3_64(full.as_bytes()));
    let dir = if crate::sys::is_root() {
        PathBuf::from("/run/xcheckfs")
    } else if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("xcheckfs")
    } else {
        // SAFETY: trivial syscall.
        PathBuf::from(format!("/tmp/xcheckfs-{}", unsafe { libc::getuid() }))
    };
    dir.join(format!("{name}.sock"))
}

pub struct ControlServer {
    path: PathBuf,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct Shared {
    pub stats: Arc<Stats>,
    pub policy: Arc<Policy>,
    pub info: Value,
}

pub fn serve(path: &Path, shared: Arc<Shared>) -> anyhow::Result<ControlServer> {
    // A directory xcheckfs creates is private (0700); an existing one (say
    // /run) is left as it is.
    if let Some(dir) = path.parent()
        && !dir.exists()
    {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            anyhow::bail!("control socket {} is in use by another xcheckfs", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path).map_err(|e| anyhow::anyhow!("control socket {}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    std::thread::Builder::new().name("xcheckfs-ctl".into()).spawn(move || {
        for conn in listener.incoming().flatten() {
            let shared = shared.clone();
            let _ = std::thread::Builder::new().name("xcheckfs-ctl-conn".into()).spawn(move || handle(conn, &shared));
        }
    })?;
    Ok(ControlServer { path: path.to_path_buf() })
}

fn handle(conn: UnixStream, sh: &Shared) {
    let Ok(mut w) = conn.try_clone() else { return };
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { return };
        let resp = command(sh, line.trim());
        if writeln!(w, "{resp}").is_err() {
            return;
        }
    }
}

fn status(sh: &Shared) -> Value {
    let s = sh.stats.snapshot();
    json!({
        "ok": true,
        "info": sh.info,
        "state": if s.detached { "detached" } else if sh.policy.is_frozen() { "frozen" } else { "running" },
        "mode": sh.policy.mode().name(),
        "uptime_secs": s.uptime_secs,
        "ops": s.total_ops(),
        "mismatches": s.mismatches,
        "allowed": s.allowed,
        "repeats": s.repeats,
        "pending": sh.policy.pending().len(),
        "secondary_skipped": s.secondary_skipped,
        "verifications": s.verifications,
        "concurrent_data_ops": s.concurrent_data_ops,
        "range_waits": s.range_waits,
        "attr_time_skipped": s.attr_time_skipped,
        "resyncs": s.resyncs,
        "aligned_mtimes": s.aligned_mtimes,
        "resync_failures": s.resync_failures,
        "resync_giveups": s.resync_giveups,
        "quarantined": s.quarantined,
        "bytes_read": s.bytes_read,
        "bytes_written": s.bytes_written,
        "nodes": s.nodes,
        "open_files": s.open_files,
        "open_dirs": s.open_dirs,
        "lock_waiters": s.lock_waiters,
        "fds_held": crate::engine::fds::held(),
        "fds_budget": s.fd_budget,
        "fd_refusals": s.fd_refusals,
    })
}

fn command(sh: &Shared, line: &str) -> Value {
    let args: Vec<&str> = line.split_whitespace().collect();
    let err = |m: String| json!({"ok": false, "error": m});
    match args.as_slice() {
        ["status"] | [] => status(sh),
        ["stats"] => {
            let s = sh.stats.snapshot();
            let ops: Vec<Value> = s
                .ops
                .iter()
                .filter(|(_, o)| o.count > 0)
                .map(|(k, o)| {
                    json!({
                        "op": k.name(), "count": o.count, "errno_results": o.errors, "mismatches": o.mismatches,
                        "bytes": o.bytes,
                        "p50_ns": o.total.percentile(50.0), "p99_ns": o.total.percentile(99.0), "max_ns": o.total.max,
                        "primary_p50_ns": o.primary.percentile(50.0), "secondary_p50_ns": o.secondary.percentile(50.0),
                        "primary_p99_ns": o.primary.percentile(99.0), "secondary_p99_ns": o.secondary.percentile(99.0),
                    })
                })
                .collect();
            json!({"ok": true, "ops": ops, "status": status(sh)})
        }
        ["mismatches", rest @ ..] => {
            let n: usize = rest.first().and_then(|x| x.parse().ok()).unwrap_or(50);
            let h = sh.policy.history();
            let v: Vec<Value> = h.iter().rev().take(n).map(|m| serde_json::to_value(&**m).unwrap_or(Value::Null)).collect();
            json!({"ok": true, "mismatches": v})
        }
        ["pending"] => {
            let v: Vec<Value> = sh
                .policy
                .pending()
                .iter()
                .map(|p| serde_json::to_value(&*p.mismatch).unwrap_or(Value::Null))
                .collect();
            json!({"ok": true, "pending": v})
        }
        ["resolve", id, action] => {
            let Ok(id) = id.parse::<u64>() else { return err(format!("bad id {id}")) };
            let action = match *action {
                "allow" | "allow-path" => {
                    let Some(p) = sh.policy.pending().into_iter().find(|p| p.mismatch.id == id) else {
                        return err(format!("mismatch #{id} is not pending"));
                    };
                    Action::Allow(Rule::from_mismatch(&p.mismatch, *action == "allow-path"))
                }
                a => match Action::parse(a) {
                    Some(a) => a,
                    None => return err(format!("unknown action {a}")),
                },
            };
            if sh.policy.resolve(id, action) {
                json!({"ok": true})
            } else {
                err(format!("mismatch #{id} is not pending"))
            }
        }
        ["mode", m] => {
            let mode = match *m {
                "resync" => MismatchMode::Resync,
                "log" => MismatchMode::Log,
                "fail" => MismatchMode::Fail,
                "freeze" => MismatchMode::Freeze,
                "detach" => MismatchMode::Detach,
                _ => return err(format!("unknown mode {m}")),
            };
            sh.policy.set_mode(mode);
            json!({"ok": true, "mode": mode.name()})
        }
        ["detach"] => {
            sh.policy.detach();
            for p in sh.policy.pending() {
                sh.policy.resolve(p.mismatch.id, Action::Continue);
            }
            json!({"ok": true})
        }
        ["rules"] => json!({
            "ok": true,
            "path": sh.policy.rules_path().map(|p| p.display().to_string()),
            "persistent": sh.policy.can_persist(),
            "rules": sh.policy.rules(),
        }),
        _ => err(format!("unknown command {line:?}")),
    }
}

/// Client side: sends one command and returns the parsed response.
pub fn request(path: &Path, cmd: &str) -> anyhow::Result<Value> {
    let mut s = UnixStream::connect(path).map_err(|e| anyhow::anyhow!("connect {}: {e}", path.display()))?;
    writeln!(s, "{cmd}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Human-oriented one-line summary for exit / SIGUSR1.
pub fn summary(stats: &Stats) -> String {
    let s = stats.snapshot();
    let mut ops: Vec<(OpKind, u64)> = s.ops.iter().filter(|(_, o)| o.count > 0).map(|(k, o)| (*k, o.count)).collect();
    ops.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    let top: Vec<String> = ops.iter().take(8).map(|(k, c)| format!("{}={c}", k.name())).collect();
    format!(
        "{} ops in {:.0}s ({}), {} mismatches ({} allowed, {} repeats), {} repairs ({} failed, {} given up), read {}, written {}, {} verifications, {} secondary halves skipped",
        s.total_ops(),
        s.uptime_secs,
        top.join(" "),
        s.mismatches,
        s.allowed,
        s.repeats,
        s.resyncs,
        s.resync_failures,
        s.resync_giveups,
        crate::stats::fmt_bytes(s.bytes_read as f64),
        crate::stats::fmt_bytes(s.bytes_written as f64),
        s.verifications,
        s.secondary_skipped,
    )
}
