//! Workspace conversation goal state. Commands and model evaluation live in the caller.

use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const DEFAULT_MAX_TURNS: u32 = 20;
pub(crate) const MAX_TURNS: u32 = 200;
pub(crate) const MAX_NO_PROGRESS: u32 = 3;
const STATE_VERSION: u64 = 1;
const MAX_OBJECTIVE_BYTES: usize = 16 * 1024;
const MAX_DETAIL_BYTES: usize = 4 * 1024;
const MAX_STATE_BYTES: usize = 256 * 1024;
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
    BudgetLimited,
}

impl GoalStatus {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
            Self::BudgetLimited => "budget-limited",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "complete" => Self::Complete,
            "budget-limited" => Self::BudgetLimited,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalState {
    pub(crate) objective: String,
    pub(crate) status: GoalStatus,
    pub(crate) max_turns: u32,
    pub(crate) turns_used: u32,
    pub(crate) no_progress_count: u32,
    pub(crate) in_flight: bool,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IdleGate {
    pub(crate) idle: bool,
    pub(crate) pending_user_work: bool,
    pub(crate) pending_approval: bool,
    pub(crate) plan_mode: bool,
    pub(crate) interrupted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnPermit {
    pub(crate) objective: String,
    pub(crate) turn: u32,
    pub(crate) remaining: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalVerdict {
    Progress { evidence: String },
    NoProgress { reason: String },
    Complete { evidence: String },
    Blocked { reason: String },
}

pub(crate) struct GoalStore {
    path: PathBuf,
    cwd: String,
    state: Option<GoalState>,
}

impl GoalStore {
    pub(crate) fn load(history_path: &Path, cwd: &str) -> Result<Self, String> {
        let path = history_path.with_extension("goal.json");
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    cwd: cwd.into(),
                    state: None,
                });
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let too_large = || format!("{}: goal state exceeds size limit", path.display());
        if file
            .metadata()
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len()
            > MAX_STATE_BYTES as u64
        {
            return Err(too_large());
        }
        let mut text = String::new();
        file.take((MAX_STATE_BYTES + 1) as u64)
            .read_to_string(&mut text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if text.len() > MAX_STATE_BYTES {
            return Err(too_large());
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("{}: invalid goal state: {error}", path.display()))?;
        if value["version"].as_u64() != Some(STATE_VERSION) {
            return Err(format!(
                "{}: unsupported goal state version",
                path.display()
            ));
        }
        if value["cwd"].as_str() != Some(cwd) {
            return Err(format!(
                "{}: goal belongs to another workspace",
                path.display()
            ));
        }
        let state = match value.get("goal") {
            Some(Value::Null) => None,
            Some(goal) => {
                Some(parse_goal(goal).map_err(|error| format!("{}: {error}", path.display()))?)
            }
            None => return Err(format!("{}: goal field missing", path.display())),
        };
        let mut store = Self {
            path,
            cwd: cwd.into(),
            state,
        };
        if let Some(mut goal) = store.state.clone()
            && (goal.status == GoalStatus::Active || goal.in_flight)
        {
            goal.status = GoalStatus::Paused;
            goal.in_flight = false;
            goal.detail = Some("Process restarted; use /goal resume to continue.".into());
            store.save(Some(goal))?;
        }
        Ok(store)
    }

    pub(crate) fn status(&self) -> Option<&GoalState> {
        self.state.as_ref()
    }

    pub(crate) fn set(&mut self, objective: &str, max_turns: Option<u32>) -> Result<(), String> {
        if self.state.as_ref().is_some_and(|goal| goal.in_flight) {
            return Err("finish or interrupt the current goal turn before replacing it".into());
        }
        let objective = checked_text(objective, MAX_OBJECTIVE_BYTES, "objective")?;
        let max_turns = checked_max_turns(max_turns.unwrap_or(DEFAULT_MAX_TURNS))?;
        self.save(Some(GoalState {
            objective,
            status: GoalStatus::Active,
            max_turns,
            turns_used: 0,
            no_progress_count: 0,
            in_flight: false,
            detail: None,
        }))
    }

    pub(crate) fn pause(&mut self, reason: &str) -> Result<(), String> {
        let mut goal = self.require_goal()?;
        if goal.status == GoalStatus::Complete {
            return Err("completed goal cannot be paused".into());
        }
        goal.status = GoalStatus::Paused;
        goal.in_flight = false;
        goal.detail = Some(checked_text(reason, MAX_DETAIL_BYTES, "pause reason")?);
        self.save(Some(goal))
    }

    pub(crate) fn resume(&mut self, max_turns: Option<u32>) -> Result<(), String> {
        let mut goal = self.require_goal()?;
        if !matches!(
            goal.status,
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::BudgetLimited
        ) {
            return Err("only a paused, blocked, or budget-limited goal can resume".into());
        }
        if let Some(max_turns) = max_turns {
            goal.max_turns = checked_max_turns(max_turns)?;
        }
        if goal.turns_used >= goal.max_turns {
            return Err("increase max turns before resuming this budget-limited goal".into());
        }
        goal.status = GoalStatus::Active;
        goal.in_flight = false;
        goal.no_progress_count = 0;
        goal.detail = None;
        self.save(Some(goal))
    }

    pub(crate) fn clear(&mut self) -> Result<(), String> {
        self.save(None)
    }

    pub(crate) fn reserve_turn(&mut self, gate: IdleGate) -> Result<Option<TurnPermit>, String> {
        if !gate.idle
            || gate.pending_user_work
            || gate.pending_approval
            || gate.plan_mode
            || gate.interrupted
        {
            return Ok(None);
        }
        let Some(mut goal) = self.state.clone() else {
            return Ok(None);
        };
        if goal.status != GoalStatus::Active || goal.in_flight {
            return Ok(None);
        }
        if goal.turns_used >= goal.max_turns {
            goal.status = GoalStatus::BudgetLimited;
            goal.detail = Some("Goal turn budget reached without verified completion.".into());
            self.save(Some(goal))?;
            return Ok(None);
        }
        goal.turns_used += 1;
        goal.in_flight = true;
        let permit = TurnPermit {
            objective: goal.objective.clone(),
            turn: goal.turns_used,
            remaining: goal.max_turns - goal.turns_used,
        };
        self.save(Some(goal))?;
        Ok(Some(permit))
    }

    pub(crate) fn finish_turn(
        &mut self,
        verdict: GoalVerdict,
        had_tool_activity: bool,
        automatic: bool,
    ) -> Result<(), String> {
        let mut goal = self.require_goal()?;
        if goal.status != GoalStatus::Active || !goal.in_flight {
            return Err("no active goal turn to finish".into());
        }
        let (status, detail) = match verdict {
            GoalVerdict::Progress { evidence } => {
                goal.no_progress_count = 0;
                (
                    GoalStatus::Active,
                    checked_text(&evidence, MAX_DETAIL_BYTES, "progress evidence")?,
                )
            }
            GoalVerdict::NoProgress { reason } => {
                goal.no_progress_count += 1;
                let status = if goal.no_progress_count >= MAX_NO_PROGRESS {
                    GoalStatus::Blocked
                } else {
                    GoalStatus::Active
                };
                (
                    status,
                    checked_text(&reason, MAX_DETAIL_BYTES, "no-progress reason")?,
                )
            }
            GoalVerdict::Complete { evidence } => (
                GoalStatus::Complete,
                checked_text(&evidence, MAX_DETAIL_BYTES, "completion evidence")?,
            ),
            GoalVerdict::Blocked { reason } => (
                GoalStatus::Blocked,
                checked_text(&reason, MAX_DETAIL_BYTES, "block reason")?,
            ),
        };
        goal.status = status;
        goal.detail = Some(detail);
        goal.in_flight = false;
        if automatic && !had_tool_activity && goal.status == GoalStatus::Active {
            goal.status = GoalStatus::Blocked;
            goal.detail =
                Some("Automatic continuation made no tool call; paused to avoid a loop.".into());
        }
        if goal.status == GoalStatus::Active && goal.turns_used >= goal.max_turns {
            goal.status = GoalStatus::BudgetLimited;
            goal.detail = Some("Goal turn budget reached without verified completion.".into());
        }
        self.save(Some(goal))
    }

    fn require_goal(&self) -> Result<GoalState, String> {
        self.state.clone().ok_or_else(|| "no goal is set".into())
    }

    fn save(&mut self, next: Option<GoalState>) -> Result<(), String> {
        let goal = next.as_ref().map(goal_json);
        let body = json!({"version": STATE_VERSION, "cwd": self.cwd, "goal": goal});
        let serialized = body.to_string();
        if serialized.len() > MAX_STATE_BYTES {
            return Err(format!(
                "{}: goal state exceeds size limit",
                self.path.display()
            ));
        }
        write_private(&self.path, serialized.as_bytes())
            .map_err(|error| format!("{}: {error}", self.path.display()))?;
        self.state = next;
        Ok(())
    }
}

fn checked_text(value: &str, max_bytes: usize, name: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} cannot be blank"));
    }
    if value.len() > max_bytes {
        return Err(format!("{name} exceeds {max_bytes} bytes"));
    }
    Ok(value.into())
}

fn checked_max_turns(value: u32) -> Result<u32, String> {
    if !(1..=MAX_TURNS).contains(&value) {
        return Err(format!("max turns must be 1..={MAX_TURNS}"));
    }
    Ok(value)
}

fn goal_json(goal: &GoalState) -> Value {
    json!({
        "objective": goal.objective,
        "status": goal.status.name(),
        "max_turns": goal.max_turns,
        "turns_used": goal.turns_used,
        "no_progress_count": goal.no_progress_count,
        "in_flight": goal.in_flight,
        "detail": goal.detail,
    })
}

fn parse_goal(value: &Value) -> Result<GoalState, String> {
    let objective = checked_text(
        value["objective"]
            .as_str()
            .ok_or("goal objective must be text")?,
        MAX_OBJECTIVE_BYTES,
        "objective",
    )?;
    let status = GoalStatus::parse(value["status"].as_str().ok_or("goal status must be text")?)
        .ok_or("unknown goal status")?;
    let max_turns = value["max_turns"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or("invalid goal max turns")?;
    checked_max_turns(max_turns)?;
    let turns_used = value["turns_used"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n <= max_turns)
        .ok_or("invalid goal turns used")?;
    let no_progress_count = value["no_progress_count"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n <= MAX_NO_PROGRESS)
        .ok_or("invalid goal no-progress count")?;
    let in_flight = value["in_flight"]
        .as_bool()
        .ok_or("invalid goal in-flight state")?;
    let detail = match value.get("detail") {
        Some(Value::Null) => None,
        Some(Value::String(text)) => Some(checked_text(text, MAX_DETAIL_BYTES, "goal detail")?),
        _ => return Err("invalid goal detail".into()),
    };
    Ok(GoalState {
        objective,
        status,
        max_turns,
        turns_used,
        no_progress_count,
        in_flight,
        detail,
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("goal path has no parent"))?;
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("goal.tmp-{}-{seq}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        fs::remove_file(&temp).ok();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, GoalStore) {
        let dir = std::env::temp_dir().join(format!(
            "revex-goals-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let history = dir.join("history-abc.json");
        let store = GoalStore::load(&history, "/work/one").unwrap();
        (history, store)
    }

    fn idle() -> IdleGate {
        IdleGate {
            idle: true,
            ..IdleGate::default()
        }
    }

    #[test]
    fn active_goal_is_private_and_reopens_paused_without_replay() {
        let (history, mut store) = fixture();
        store.set("Finish the parser", None).unwrap();
        assert_eq!(store.status().unwrap().max_turns, DEFAULT_MAX_TURNS);
        let first = store.reserve_turn(idle()).unwrap().unwrap();
        assert_eq!((first.turn, first.remaining), (1, 19));
        let reopened = GoalStore::load(&history, "/work/one").unwrap();
        let state = reopened.status().unwrap();
        assert_eq!(state.status, GoalStatus::Paused);
        assert_eq!(state.turns_used, 1);
        assert!(!state.in_flight);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&reopened.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(reopened.path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn escaped_maximum_objective_and_detail_survive_reload() {
        let (history, mut store) = fixture();
        let objective = "\"\\".repeat(MAX_OBJECTIVE_BYTES / 2);
        let detail = "\0".repeat(MAX_DETAIL_BYTES);
        store.set(&objective, None).unwrap();
        store.pause(&detail).unwrap();
        assert!(std::fs::metadata(&store.path).unwrap().len() > 24 * 1024);
        let reopened = GoalStore::load(&history, "/work/one").unwrap();
        let state = reopened.status().unwrap();
        assert_eq!(state.objective, objective);
        assert_eq!(state.detail.as_deref(), Some(detail.as_str()));
        assert_eq!(state.status, GoalStatus::Paused);
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn oversized_state_file_is_rejected_before_json_parsing() {
        let (history, store) = fixture();
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&store.path, vec![b' '; MAX_STATE_BYTES + 1]).unwrap();
        let error = GoalStore::load(&history, "/work/one").err().unwrap();
        assert!(error.contains("goal state exceeds size limit"), "{error}");
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn total_turn_budget_includes_initial_turn_and_stays_distinct_from_completion() {
        let (history, mut store) = fixture();
        store.set("Fix the bug", Some(2)).unwrap();
        assert_eq!(store.reserve_turn(idle()).unwrap().unwrap().turn, 1);
        store
            .finish_turn(
                GoalVerdict::Progress {
                    evidence: "test failed".into(),
                },
                true,
                false,
            )
            .unwrap();
        assert_eq!(store.reserve_turn(idle()).unwrap().unwrap().turn, 2);
        store
            .finish_turn(
                GoalVerdict::Progress {
                    evidence: "test passed".into(),
                },
                true,
                true,
            )
            .unwrap();
        assert_eq!(store.status().unwrap().status, GoalStatus::BudgetLimited);
        assert!(store.reserve_turn(idle()).unwrap().is_none());
        assert_ne!(store.status().unwrap().status, GoalStatus::Complete);
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn no_progress_and_autonomous_no_tool_turns_stop() {
        let (history, mut store) = fixture();
        store.set("Inspect failures", Some(10)).unwrap();
        for count in 1..=3 {
            store.reserve_turn(idle()).unwrap().unwrap();
            store
                .finish_turn(
                    GoalVerdict::NoProgress {
                        reason: "same failure".into(),
                    },
                    true,
                    count > 1,
                )
                .unwrap();
            assert_eq!(store.status().unwrap().no_progress_count, count);
        }
        assert_eq!(store.status().unwrap().status, GoalStatus::Blocked);

        store.set("Another task", None).unwrap();
        store.reserve_turn(idle()).unwrap().unwrap();
        store
            .finish_turn(
                GoalVerdict::Progress {
                    evidence: "summary only".into(),
                },
                false,
                true,
            )
            .unwrap();
        assert_eq!(store.status().unwrap().status, GoalStatus::Blocked);
        assert!(store.reserve_turn(idle()).unwrap().is_none());
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn completion_requires_evidence_and_user_can_resume_or_clear() {
        let (history, mut store) = fixture();
        store.set("Ship feature", Some(1)).unwrap();
        store.reserve_turn(idle()).unwrap().unwrap();
        assert!(
            store
                .finish_turn(
                    GoalVerdict::Complete {
                        evidence: " ".into()
                    },
                    false,
                    false,
                )
                .is_err()
        );
        store
            .finish_turn(
                GoalVerdict::Complete {
                    evidence: "passing regression test".into(),
                },
                false,
                false,
            )
            .unwrap();
        assert_eq!(store.status().unwrap().status, GoalStatus::Complete);
        assert_eq!(
            store.status().unwrap().detail.as_deref(),
            Some("passing regression test")
        );
        store.clear().unwrap();
        assert!(store.status().is_none());
        assert!(
            GoalStore::load(&history, "/work/one")
                .unwrap()
                .status()
                .is_none()
        );
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn idle_gate_blocks_pending_work_plan_and_interrupt_without_spending_budget() {
        let (history, mut store) = fixture();
        store.set("Keep going", None).unwrap();
        for gate in [
            IdleGate {
                idle: false,
                ..idle()
            },
            IdleGate {
                pending_user_work: true,
                ..idle()
            },
            IdleGate {
                pending_approval: true,
                ..idle()
            },
            IdleGate {
                plan_mode: true,
                ..idle()
            },
            IdleGate {
                interrupted: true,
                ..idle()
            },
        ] {
            assert!(store.reserve_turn(gate).unwrap().is_none());
        }
        assert_eq!(store.status().unwrap().turns_used, 0);
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn pause_resume_and_budget_extension_preserve_turn_count() {
        let (history, mut store) = fixture();
        assert!(store.set(" ", None).is_err());
        assert!(store.set("task", Some(MAX_TURNS + 1)).is_err());
        store.set("task", Some(1)).unwrap();
        store.reserve_turn(idle()).unwrap().unwrap();
        store.pause("user paused").unwrap();
        assert_eq!(store.status().unwrap().status, GoalStatus::Paused);
        assert!(store.resume(None).is_err());
        store.resume(Some(2)).unwrap();
        assert_eq!(store.reserve_turn(idle()).unwrap().unwrap().turn, 2);
        store
            .finish_turn(
                GoalVerdict::Blocked {
                    reason: "needs input".into(),
                },
                true,
                true,
            )
            .unwrap();
        assert_eq!(store.status().unwrap().status, GoalStatus::Blocked);
        store.resume(Some(3)).unwrap();
        assert_eq!(store.status().unwrap().turns_used, 2);
        assert_eq!(store.reserve_turn(idle()).unwrap().unwrap().turn, 3);
        store
            .finish_turn(
                GoalVerdict::Blocked {
                    reason: "still needs input".into(),
                },
                true,
                true,
            )
            .unwrap();
        assert_eq!(
            store.status().unwrap().detail.as_deref(),
            Some("still needs input")
        );
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_or_wrong_workspace_state_is_rejected() {
        let (history, mut store) = fixture();
        store.set("One", None).unwrap();
        assert!(GoalStore::load(&history, "/work/two").is_err());
        std::fs::write(&store.path, b"not json").unwrap();
        assert!(GoalStore::load(&history, "/work/one").is_err());
        std::fs::remove_dir_all(history.parent().unwrap()).unwrap();
    }
}
