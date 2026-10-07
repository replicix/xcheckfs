//! What to do when the file systems disagree: allow rules, de-duplication,
//! and the log / fail / freeze / detach modes.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::time::SystemTime;

use globset::{Glob, GlobMatcher};
use parking_lot::{Condvar, Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::config::MismatchMode;
use crate::events::{EventSink, UiEvent};
use crate::stats::{OpKind, Stats};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MismatchKind {
    /// Success/errno differs.
    Result,
    /// A returned attribute differs (`field` names it).
    Attr,
    /// Returned data differs.
    Data,
    /// Returned length / count differs (read, write, copy_file_range, lseek).
    Length,
    /// Directory listings differ.
    Readdir,
    /// Symlink targets differ.
    Readlink,
    /// Extended attribute values or lists differ.
    Xattr,
    /// Hard-link structure differs: the primary says two names are the same
    /// inode, the secondary disagrees (or vice versa).
    Identity,
    /// A read-back after a mutation (thorough mode) shows the mutation was
    /// not applied as requested.
    Verify,
    /// Whole-file content comparison on close (paranoid mode).
    Content,
    /// Lock grant/deny or conflicting-lock result differs.
    Lock,
}

impl MismatchKind {
    pub fn name(&self) -> &'static str {
        match self {
            MismatchKind::Result => "result",
            MismatchKind::Attr => "attr",
            MismatchKind::Data => "data",
            MismatchKind::Length => "length",
            MismatchKind::Readdir => "readdir",
            MismatchKind::Readlink => "readlink",
            MismatchKind::Xattr => "xattr",
            MismatchKind::Identity => "identity",
            MismatchKind::Verify => "verify",
            MismatchKind::Content => "content",
            MismatchKind::Lock => "lock",
        }
    }
}

/// One disagreement between the file systems.
#[derive(Clone, Debug, Serialize)]
pub struct Mismatch {
    pub id: u64,
    #[serde(with = "systime")]
    pub time: SystemTime,
    pub op: OpKind,
    pub kind: MismatchKind,
    pub ino: u64,
    /// Path relative to the mount root ("/a/b"), best effort.
    pub path: String,
    /// Attribute name, verification step, xattr name, ...
    pub field: Option<String>,
    /// Short machine-readable values, matched by rules (e.g. errno names).
    pub primary: String,
    pub secondary: String,
    /// Human-readable explanation.
    pub detail: String,
    /// The operation is read-only and can be re-executed on both sides.
    pub retryable: bool,
    /// The object can be re-synchronised from the primary to the secondary.
    pub resyncable: bool,
}

mod systime {
    use std::time::{SystemTime, UNIX_EPOCH};
    pub fn serialize<S: serde::Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0))
    }
}

impl Mismatch {
    pub fn summary(&self) -> String {
        format!("#{} {}", self.id, self.describe())
    }

    /// The summary without the id.
    pub fn describe(&self) -> String {
        let field = self.field.as_deref().map(|f| format!(" {f}")).unwrap_or_default();
        format!(
            "{} {}{} on {} (ino {}): primary={} secondary={}{}",
            self.op.name(),
            self.kind.name(),
            field,
            if self.path.is_empty() { "?" } else { &self.path },
            self.ino,
            self.primary,
            self.secondary,
            if self.detail.is_empty() { String::new() } else { format!(" — {}", self.detail) }
        )
    }
}

/// An allow rule. Every given criterion must match; omitted ones match
/// anything. Stored as `[[allow]]` tables in the rules file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MismatchKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Glob on the path relative to the mount root, e.g. `/var/cache/**`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Rule {
    /// A rule matching this mismatch's op, kind, field and values anywhere.
    pub fn from_mismatch(m: &Mismatch, with_path: bool) -> Rule {
        Rule {
            op: Some(m.op.name().to_string()),
            kind: Some(m.kind),
            field: m.field.clone(),
            path: if with_path && !m.path.is_empty() { Some(m.path.clone()) } else { None },
            primary: if m.kind == MismatchKind::Result { Some(m.primary.clone()) } else { None },
            secondary: if m.kind == MismatchKind::Result { Some(m.secondary.clone()) } else { None },
            note: Some(format!("added interactively for mismatch #{}", m.id)),
        }
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.op {
            parts.push(format!("op={v}"));
        }
        if let Some(v) = &self.kind {
            parts.push(format!("kind={}", v.name()));
        }
        if let Some(v) = &self.field {
            parts.push(format!("field={v}"));
        }
        if let Some(v) = &self.path {
            parts.push(format!("path={v}"));
        }
        if let Some(v) = &self.primary {
            parts.push(format!("primary={v}"));
        }
        if let Some(v) = &self.secondary {
            parts.push(format!("secondary={v}"));
        }
        if parts.is_empty() { "<matches everything>".into() } else { parts.join(" ") }
    }
}

struct CompiledRule {
    rule: Rule,
    path: Option<GlobMatcher>,
}

impl CompiledRule {
    fn new(rule: Rule) -> anyhow::Result<CompiledRule> {
        if let Some(op) = &rule.op {
            if op != "*" && OpKind::from_name(op).is_none() {
                anyhow::bail!("unknown op {op:?} in rule");
            }
        }
        let path = match &rule.path {
            Some(p) => Some(Glob::new(p)?.compile_matcher()),
            None => None,
        };
        Ok(CompiledRule { rule, path })
    }

    fn matches(&self, m: &Mismatch) -> bool {
        let r = &self.rule;
        r.op.as_deref().is_none_or(|o| o == "*" || o == m.op.name())
            && r.kind.is_none_or(|k| k == m.kind)
            && r.field.as_deref().is_none_or(|f| m.field.as_deref() == Some(f))
            && self.path.as_ref().is_none_or(|g| g.is_match(&m.path))
            && r.primary.as_deref().is_none_or(|v| v.eq_ignore_ascii_case(&m.primary))
            && r.secondary.as_deref().is_none_or(|v| v.eq_ignore_ascii_case(&m.secondary))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RulesFile {
    #[serde(default)]
    pub allow: Vec<Rule>,
}

impl RulesFile {
    pub fn load(path: &Path) -> anyhow::Result<RulesFile> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RulesFile::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let body = format!(
            "# xcheckfs allow rules. Every given field must match; omitted fields match anything.\n\
             # kinds: result attr data length readdir readlink xattr identity verify content lock\n\n{}",
            toml::to_string_pretty(self)?
        );
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Operator decision for a frozen mismatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Return the primary's result; further identical mismatches on this
    /// object are only counted.
    Continue,
    /// Add an allow rule (persisted when possible) and continue.
    Allow(Rule),
    /// Re-execute the (read-only) operation on both file systems.
    Retry,
    /// Copy the object's state from the primary to the secondary, continue.
    Resync,
    /// Return EIO for this operation.
    Fail,
    /// Stop mirroring for the rest of the session.
    Detach,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "continue" | "c" => Action::Continue,
            "retry" | "r" => Action::Retry,
            "resync" | "s" => Action::Resync,
            "fail" | "e" => Action::Fail,
            "detach" | "d" => Action::Detach,
            _ => return None,
        })
    }
}

/// What the engine must do after reporting a mismatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    Fail,
    Retry,
    Resync,
}

pub struct Pending {
    pub mismatch: Arc<Mismatch>,
    decision: Mutex<Option<Action>>,
    cv: Condvar,
}

/// De-duplication key: the same kind of problem on the same object. Name-based mismatches (lookup, create,
/// unlink, ...) are reported against the directory's inode, so the path (which includes the name) is part of the
/// key: otherwise one bad name would hide every other bad name in that directory.
type SeenKey = (u64, OpKind, MismatchKind, Option<String>, String);

fn seen_key(m: &Mismatch) -> SeenKey {
    (m.ino, m.op, m.kind, m.field.clone(), m.path.clone())
}

pub struct Policy {
    mode: RwLock<MismatchMode>,
    rules: RwLock<Vec<CompiledRule>>,
    rules_path: Option<PathBuf>,
    /// False when the rules file lives inside a mirrored tree: writing it
    /// from the daemon would bypass the mirror.
    persist: bool,
    seen: Mutex<HashSet<SeenKey>>,
    pending: Mutex<Vec<Arc<Pending>>>,
    frozen: AtomicBool,
    gate: Mutex<()>,
    gate_cv: Condvar,
    next_id: AtomicU64,
    history: Mutex<VecDeque<Arc<Mismatch>>>,
    history_cap: usize,
    events: EventSink,
    stats: Arc<Stats>,
}

impl Policy {
    pub fn new(
        mode: MismatchMode,
        rules: Vec<Rule>,
        rules_path: Option<PathBuf>,
        persist: bool,
        events: EventSink,
        stats: Arc<Stats>,
    ) -> anyhow::Result<Policy> {
        let rules = rules.into_iter().map(CompiledRule::new).collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Policy {
            mode: RwLock::new(mode),
            rules: RwLock::new(rules),
            rules_path,
            persist,
            seen: Mutex::new(HashSet::new()),
            pending: Mutex::new(Vec::new()),
            frozen: AtomicBool::new(false),
            gate: Mutex::new(()),
            gate_cv: Condvar::new(),
            next_id: AtomicU64::new(1),
            history: Mutex::new(VecDeque::new()),
            history_cap: 1000,
            events,
            stats,
        })
    }

    pub fn mode(&self) -> MismatchMode {
        *self.mode.read()
    }

    pub fn set_mode(&self, m: MismatchMode) {
        *self.mode.write() = m;
        if m != MismatchMode::Freeze {
            for p in self.pending() {
                self.resolve(p.mismatch.id, Action::Continue);
            }
        }
        tracing::warn!("mismatch mode set to {}", m.name());
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.read().iter().map(|r| r.rule.clone()).collect()
    }

    pub fn rules_path(&self) -> Option<&Path> {
        self.rules_path.as_deref()
    }

    pub fn can_persist(&self) -> bool {
        self.persist && self.rules_path.is_some()
    }

    /// Adds a rule; persists it to the rules file when possible.
    pub fn add_rule(&self, rule: Rule, persist: bool) -> anyhow::Result<()> {
        let compiled = CompiledRule::new(rule.clone())?;
        self.rules.write().push(compiled);
        tracing::warn!("allow rule added: {}", rule.describe());
        if persist {
            if !self.persist {
                anyhow::bail!("rules file is inside a mirrored tree; rule kept for this session only");
            }
            if let Some(p) = &self.rules_path {
                let mut f = RulesFile::load(p)?;
                f.allow.push(rule);
                f.save(p)?;
            }
        }
        Ok(())
    }

    pub fn history(&self) -> Vec<Arc<Mismatch>> {
        self.history.lock().iter().cloned().collect()
    }

    pub fn pending(&self) -> Vec<Arc<Pending>> {
        self.pending.lock().clone()
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(SeqCst)
    }

    /// Called at the start of every operation: blocks while frozen.
    #[inline]
    pub fn gate(&self) {
        if !self.frozen.load(SeqCst) {
            return;
        }
        let mut g = self.gate.lock();
        while self.frozen.load(SeqCst) {
            self.gate_cv.wait(&mut g);
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Relaxed)
    }

    /// Reports a mismatch and decides how the engine continues.
    pub fn report(&self, mut m: Mismatch) -> Verdict {
        if self.stats.detached.load(Relaxed) {
            return Verdict::Continue;
        }
        if self.rules.read().iter().any(|r| r.matches(&m)) {
            self.stats.allowed.fetch_add(1, Relaxed);
            tracing::debug!("allowed mismatch: {}", m.summary());
            return Verdict::Continue;
        }
        let mode = self.mode();
        let key = seen_key(&m);
        if !self.seen.lock().insert(key) {
            self.stats.repeats.fetch_add(1, Relaxed);
            tracing::debug!("repeated mismatch: {}", m.summary());
            return match mode {
                MismatchMode::Fail => Verdict::Fail,
                // A repeat after a repair: the bug struck again, repair again
                // (the engine limits repairs per object).
                MismatchMode::Resync if m.resyncable => Verdict::Resync,
                _ => Verdict::Continue,
            };
        }
        m.id = self.next_id();
        let m = Arc::new(m);
        self.stats.mismatches.fetch_add(1, Relaxed);
        self.stats.op(m.op).mismatches.fetch_add(1, Relaxed);
        tracing::error!(target: "xcheckfs::mismatch", "MISMATCH {}", m.summary());
        {
            let mut h = self.history.lock();
            if h.len() == self.history_cap {
                h.pop_front();
            }
            h.push_back(m.clone());
        }
        self.events.send(UiEvent::Mismatch(m.clone()), &self.stats);
        match mode {
            MismatchMode::Resync if m.resyncable => Verdict::Resync,
            MismatchMode::Resync | MismatchMode::Log => Verdict::Continue,
            MismatchMode::Fail => Verdict::Fail,
            MismatchMode::Detach => {
                self.detach();
                Verdict::Continue
            }
            MismatchMode::Freeze => self.freeze(m),
        }
    }

    pub fn detach(&self) {
        if !self.stats.detached.swap(true, SeqCst) {
            tracing::error!(
                "secondary DETACHED: from now on operations go to the primary only, nothing is compared"
            );
        }
    }

    fn freeze(&self, m: Arc<Mismatch>) -> Verdict {
        let p = Arc::new(Pending { mismatch: m.clone(), decision: Mutex::new(None), cv: Condvar::new() });
        {
            let mut pend = self.pending.lock();
            pend.push(p.clone());
            self.frozen.store(true, SeqCst);
        }
        tracing::error!(
            "FROZEN on mismatch #{}: waiting for an operator decision (TUI, or `xcheckfs ctl`)",
            m.id
        );
        let action = {
            let mut d = p.decision.lock();
            while d.is_none() {
                p.cv.wait(&mut d);
            }
            d.take().unwrap()
        };
        {
            let mut pend = self.pending.lock();
            pend.retain(|x| !Arc::ptr_eq(x, &p));
            if pend.is_empty() {
                let _g = self.gate.lock();
                self.frozen.store(false, SeqCst);
                self.gate_cv.notify_all();
            }
        }
        tracing::warn!("mismatch #{} resolved: {:?}", m.id, action);
        match action {
            Action::Continue => Verdict::Continue,
            Action::Allow(rule) => {
                if let Err(e) = self.add_rule(rule, true) {
                    tracing::warn!("{e}");
                }
                Verdict::Continue
            }
            Action::Retry if m.retryable => {
                // A retry must be able to freeze again on the same object.
                self.seen.lock().remove(&seen_key(&m));
                Verdict::Retry
            }
            Action::Resync if m.resyncable => Verdict::Resync,
            Action::Retry | Action::Resync => Verdict::Continue,
            Action::Fail => Verdict::Fail,
            Action::Detach => {
                self.detach();
                Verdict::Continue
            }
        }
    }

    /// Resolves a pending (frozen) mismatch. Returns false if `id` is not
    /// pending.
    pub fn resolve(&self, id: u64, action: Action) -> bool {
        let p = self.pending.lock().iter().find(|p| p.mismatch.id == id).cloned();
        match p {
            Some(p) => {
                *p.decision.lock() = Some(action);
                p.cv.notify_all();
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mm(op: OpKind, kind: MismatchKind, field: Option<&str>, path: &str) -> Mismatch {
        Mismatch {
            id: 0,
            time: SystemTime::now(),
            op,
            kind,
            ino: 5,
            path: path.into(),
            field: field.map(Into::into),
            primary: "ENOTSUP".into(),
            secondary: "OK".into(),
            detail: String::new(),
            retryable: false,
            resyncable: false,
        }
    }

    #[test]
    fn rules_match() {
        let r = CompiledRule::new(Rule {
            op: Some("getattr".into()),
            kind: Some(MismatchKind::Attr),
            field: Some("nlink".into()),
            path: Some("/var/**".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(r.matches(&mm(OpKind::Getattr, MismatchKind::Attr, Some("nlink"), "/var/a/b")));
        assert!(!r.matches(&mm(OpKind::Getattr, MismatchKind::Attr, Some("nlink"), "/etc/a")));
        assert!(!r.matches(&mm(OpKind::Lookup, MismatchKind::Attr, Some("nlink"), "/var/a")));
        assert!(!r.matches(&mm(OpKind::Getattr, MismatchKind::Attr, Some("mode"), "/var/a")));
        let any = CompiledRule::new(Rule { primary: Some("enotsup".into()), ..Default::default() }).unwrap();
        assert!(any.matches(&mm(OpKind::Setxattr, MismatchKind::Result, None, "/x")));
    }

    #[test]
    fn rules_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rules.toml");
        let f = RulesFile {
            allow: vec![Rule {
                op: Some("setxattr".into()),
                kind: Some(MismatchKind::Result),
                primary: Some("OK".into()),
                secondary: Some("EOPNOTSUPP".into()),
                note: Some("no xattrs".into()),
                ..Default::default()
            }],
        };
        f.save(&p).unwrap();
        let g = RulesFile::load(&p).unwrap();
        assert_eq!(f.allow, g.allow);
    }

    #[test]
    fn dedup_and_fail_mode() {
        let stats = Arc::new(Stats::default());
        let p = Policy::new(MismatchMode::Fail, vec![], None, false, EventSink::disabled(), stats.clone())
            .unwrap();
        let m = mm(OpKind::Read, MismatchKind::Data, None, "/f");
        assert_eq!(p.report(m.clone()), Verdict::Fail);
        assert_eq!(p.report(m), Verdict::Fail);
        assert_eq!(stats.mismatches.load(Relaxed), 1);
        assert_eq!(stats.repeats.load(Relaxed), 1);
    }

    #[test]
    fn freeze_and_resolve() {
        let stats = Arc::new(Stats::default());
        let p = Arc::new(
            Policy::new(MismatchMode::Freeze, vec![], None, false, EventSink::disabled(), stats).unwrap(),
        );
        let p2 = p.clone();
        let t = std::thread::spawn(move || {
            let mut m = mm(OpKind::Read, MismatchKind::Data, None, "/f");
            m.retryable = true;
            p2.report(m)
        });
        let id = loop {
            if let Some(x) = p.pending().first() {
                break x.mismatch.id;
            }
            std::thread::yield_now();
        };
        assert!(p.is_frozen());
        assert!(p.resolve(id, Action::Retry));
        assert_eq!(t.join().unwrap(), Verdict::Retry);
        assert!(!p.is_frozen());
    }
}
