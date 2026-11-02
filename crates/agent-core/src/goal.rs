use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle state of a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Paused => write!(f, "paused"),
            Self::Blocked => write!(f, "blocked"),
            Self::Complete => write!(f, "complete"),
        }
    }
}

/// Optional spending limits for a goal.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GoalBudgets {
    pub token_budget: Option<u64>,
    pub turn_budget: Option<u32>,
    pub wall_clock_budget_ms: Option<u64>,
}

/// A single user-defined objective.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub goal_id: String,
    pub objective: String,
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u32,
    pub tokens_used: u64,
    pub wall_clock_ms: u64,
    pub budgets: GoalBudgets,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
}

impl Goal {
    fn new(objective: impl Into<String>, completion_criterion: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            goal_id: format!("goal-{}", ulid::Ulid::generate().to_string().to_lowercase()),
            objective: objective.into(),
            completion_criterion,
            status: GoalStatus::Active,
            turns_used: 0,
            tokens_used: 0,
            wall_clock_ms: 0,
            budgets: GoalBudgets::default(),
            created_at: now,
            started_at: Some(now),
        }
    }

    /// Human-readable summary of the goal and its current spend.
    pub fn summary(&self) -> String {
        let criterion = self
            .completion_criterion
            .as_deref()
            .unwrap_or("none provided");
        format!(
            "{} [{}]: {}\n  completion criterion: {}\n  turns: {}, tokens: {}, time: {}ms",
            self.goal_id,
            self.status,
            self.objective,
            criterion,
            self.turns_used,
            self.tokens_used,
            self.wall_clock_ms
        )
    }

    /// Apply a status transition. Returns `false` if the transition is a no-op.
    pub fn set_status(&mut self, status: GoalStatus) -> bool {
        if self.status == status {
            return false;
        }
        self.status = status;
        if status == GoalStatus::Active && self.started_at.is_none() {
            self.started_at = Some(Utc::now());
        }
        true
    }
}

/// In-memory manager for goals, with optional disk persistence.
#[derive(Debug, Clone, Default)]
pub struct GoalManager {
    goals: HashMap<String, Goal>,
    persist_path: Option<std::path::PathBuf>,
}

impl GoalManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self {
            goals: HashMap::new(),
            persist_path: None,
        }
    }

    /// Set the path used to load/save goals.
    pub fn set_persist_path(&mut self, path: std::path::PathBuf) {
        self.persist_path = Some(path);
    }

    /// Load saved goals from `path` and configure persistence for future changes.
    pub fn load(path: std::path::PathBuf) -> Result<Self, String> {
        let mut manager = Self::new();
        manager.persist_path = Some(path.clone());
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read goals file: {e}"))?;
        let goals: Vec<Goal> =
            serde_json::from_str(&contents).map_err(|e| format!("invalid goals file: {e}"))?;
        manager.goals = goals.into_iter().map(|g| (g.goal_id.clone(), g)).collect();
        Ok(manager)
    }

    /// Create a new active goal, optionally replacing any existing active goal.
    pub fn create(
        &mut self,
        objective: impl Into<String>,
        completion_criterion: Option<String>,
        replace: bool,
    ) -> Result<String, String> {
        if !replace && self.active_goal().is_some() {
            return Err(
                "an active goal already exists; set replace=true to overwrite it".to_string(),
            );
        }
        if replace {
            // Mark any currently active goal as blocked so it is no longer driving turns.
            for goal in self.goals.values_mut() {
                if goal.status == GoalStatus::Active {
                    goal.status = GoalStatus::Blocked;
                }
            }
        }
        let goal = Goal::new(objective, completion_criterion);
        let id = goal.goal_id.clone();
        self.goals.insert(id.clone(), goal);
        self.persist()?;
        Ok(id)
    }

    /// Update the active goal's status.
    pub fn update(&mut self, status: GoalStatus) -> Result<String, String> {
        let id = self
            .active_goal_mut()
            .map(|goal| {
                goal.set_status(status);
                goal.goal_id.clone()
            })
            .ok_or("no active goal to update")?;
        self.persist()?;
        Ok(id)
    }

    /// Return the active goal, if any.
    pub fn active_goal(&self) -> Option<&Goal> {
        self.goals.values().find(|g| g.status == GoalStatus::Active)
    }

    fn active_goal_mut(&mut self) -> Option<&mut Goal> {
        self.goals
            .values_mut()
            .find(|g| g.status == GoalStatus::Active)
    }

    /// Return a formatted summary of the active goal.
    pub fn active_summary(&self) -> Option<String> {
        self.active_goal().map(Goal::summary)
    }

    /// Set budgets on the active goal.
    pub fn set_budgets(&mut self, budgets: GoalBudgets) -> Result<String, String> {
        let id = self
            .active_goal_mut()
            .map(|goal| {
                goal.budgets = budgets;
                goal.goal_id.clone()
            })
            .ok_or("no active goal to set budgets on")?;
        self.persist()?;
        Ok(id)
    }

    /// Record one turn's consumption against the active goal and enforce budgets.
    /// Returns `Some(reason)` if the goal was blocked by a budget.
    pub fn record_turn(&mut self, tokens: u64, elapsed_ms: u64) -> Option<String> {
        let reason = {
            let goal = self.active_goal_mut()?;
            goal.turns_used = goal.turns_used.saturating_add(1);
            goal.tokens_used = goal.tokens_used.saturating_add(tokens);
            goal.wall_clock_ms = goal.wall_clock_ms.saturating_add(elapsed_ms);

            if let Some(limit) = goal.budgets.turn_budget
                && goal.turns_used > limit
            {
                goal.status = GoalStatus::Blocked;
                return Some(format!(
                    "turn budget exceeded ({} > {})",
                    goal.turns_used, limit
                ));
            }
            if let Some(limit) = goal.budgets.token_budget
                && goal.tokens_used > limit
            {
                goal.status = GoalStatus::Blocked;
                return Some(format!(
                    "token budget exceeded ({} > {})",
                    goal.tokens_used, limit
                ));
            }
            if let Some(limit) = goal.budgets.wall_clock_budget_ms
                && goal.wall_clock_ms > limit
            {
                goal.status = GoalStatus::Blocked;
                return Some(format!(
                    "time budget exceeded ({}ms > {}ms)",
                    goal.wall_clock_ms, limit
                ));
            }
            None
        };
        let _ = self.persist();
        reason
    }

    /// Decide whether a tool should be offered to the model in the current state.
    pub fn should_offer_tool(&self, name: &str) -> bool {
        let has_active = self.active_goal().is_some();
        match name {
            "create_goal" => !has_active,
            "update_goal" | "get_goal" | "set_goal_budget" => has_active,
            _ => true,
        }
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create goals directory: {e}"))?;
        }
        let goals: Vec<&Goal> = self.goals.values().collect();
        let file =
            std::fs::File::create(path).map_err(|e| format!("failed to create goals file: {e}"))?;
        serde_json::to_writer_pretty(file, &goals)
            .map_err(|e| format!("failed to write goals file: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_update() {
        let mut manager = GoalManager::new();
        let id = manager.create("refactor parser", None, false).unwrap();
        assert!(manager.active_goal().is_some());
        manager.update(GoalStatus::Complete).unwrap();
        assert!(manager.active_goal().is_none());
        assert_eq!(manager.get(&id).unwrap().status, GoalStatus::Complete);
    }

    #[test]
    fn test_tool_visibility() {
        let mut manager = GoalManager::new();
        assert!(manager.should_offer_tool("create_goal"));
        assert!(!manager.should_offer_tool("update_goal"));
        manager.create("x", None, false).unwrap();
        assert!(!manager.should_offer_tool("create_goal"));
        assert!(manager.should_offer_tool("update_goal"));
    }

    #[test]
    fn test_budget_enforcement() {
        let mut manager = GoalManager::new();
        let id = manager.create("x", None, false).unwrap();
        manager
            .set_budgets(GoalBudgets {
                turn_budget: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert!(manager.record_turn(1, 1).is_none());
        assert!(manager.record_turn(1, 1).is_none());
        let reason = manager.record_turn(1, 1);
        assert!(reason.is_some());
        assert_eq!(manager.get(&id).unwrap().status, GoalStatus::Blocked);
    }

    impl GoalManager {
        fn get(&self, id: &str) -> Option<&Goal> {
            self.goals.get(id)
        }
    }
}
