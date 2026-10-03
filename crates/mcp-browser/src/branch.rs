//! Ephemeral Browser Context Forking & Speculative Branching.
//!
//! Enables agents to fork a browser tab's state into isolated background contexts
//! (via CDP `Target.createBrowserContext`), test speculative or ambiguous paths concurrently,
//! commit winning branch outcomes back to the visible tab, or cleanly discard failed branches.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Error related to branch lifecycle operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchError {
    AlreadyExists(String),
    NotFound(String),
    AlreadyCommitted(String),
    AlreadyDiscarded(String),
    /// The cap on simultaneously active branches was reached.
    LimitReached(usize),
    Failed(String),
}

impl std::fmt::Display for BranchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BranchError::AlreadyExists(id) => {
                write!(f, "branch '{id}' already exists and is active")
            }
            BranchError::NotFound(id) => write!(f, "branch '{id}' not found"),
            BranchError::AlreadyCommitted(id) => {
                write!(f, "branch '{id}' has already been committed")
            }
            BranchError::AlreadyDiscarded(id) => {
                write!(f, "branch '{id}' has already been discarded")
            }
            BranchError::LimitReached(max) => write!(
                f,
                "too many active branches (limit {max}); commit or discard one first, \
                 or raise AGENTCTL_MAX_BRANCHES"
            ),
            BranchError::Failed(msg) => write!(f, "branch operation failed: {msg}"),
        }
    }
}

impl std::error::Error for BranchError {}

/// Lifecycle status of a speculative branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    Active,
    Committed,
    Discarded,
}

/// Metadata and state tracking for an active or finalized branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub branch_id: String,
    pub parent_target_id: String,
    pub branch_target_id: String,
    pub browser_context_id: Option<String>,
    pub browser_host: String,
    pub browser_port: u16,
    pub initial_url: String,
    pub status: BranchStatus,
    pub created_at_ms: u64,
}

/// Thread-safe in-memory manager for active and historical speculative branches.
#[derive(Debug)]
pub struct BranchManager {
    branches: HashMap<String, Branch>,
    max_active: usize,
}

impl Default for BranchManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BranchManager {
    /// Default cap on simultaneously active branches. Every branch is a real
    /// tab in its own browser context, so an unbounded count is a resource leak.
    pub const DEFAULT_MAX_ACTIVE: usize = 8;

    pub fn new() -> Self {
        Self::with_max_active(Self::DEFAULT_MAX_ACTIVE)
    }

    pub fn with_max_active(max_active: usize) -> Self {
        Self {
            branches: HashMap::new(),
            max_active,
        }
    }

    pub fn max_active(&self) -> usize {
        self.max_active
    }

    /// Whether another branch named `branch_id` could be created right now.
    pub fn check_capacity(&self, branch_id: &str) -> Result<(), BranchError> {
        if let Some(existing) = self.branches.get(branch_id) {
            if existing.status == BranchStatus::Active {
                return Err(BranchError::AlreadyExists(branch_id.to_string()));
            }
        }
        if self.active_branches().len() >= self.max_active {
            return Err(BranchError::LimitReached(self.max_active));
        }
        Ok(())
    }

    /// The branch, if it exists and is still active (not committed/discarded).
    pub fn ensure_active(&self, branch_id: &str) -> Result<Branch, BranchError> {
        let b = self
            .branches
            .get(branch_id)
            .ok_or_else(|| BranchError::NotFound(branch_id.to_string()))?;
        match b.status {
            BranchStatus::Active => Ok(b.clone()),
            BranchStatus::Committed => Err(BranchError::AlreadyCommitted(branch_id.to_string())),
            BranchStatus::Discarded => Err(BranchError::AlreadyDiscarded(branch_id.to_string())),
        }
    }

    pub fn insert(&mut self, branch: Branch) -> Result<(), BranchError> {
        self.check_capacity(&branch.branch_id)?;
        self.branches.insert(branch.branch_id.clone(), branch);
        Ok(())
    }

    pub fn get(&self, branch_id: &str) -> Option<&Branch> {
        self.branches.get(branch_id)
    }

    pub fn get_mut(&mut self, branch_id: &str) -> Option<&mut Branch> {
        self.branches.get_mut(branch_id)
    }

    pub fn list(&self, target_id: Option<&str>) -> Vec<Branch> {
        let mut list: Vec<Branch> = self
            .branches
            .values()
            .filter(|b| match target_id {
                Some(tid) => b.parent_target_id == tid || b.branch_target_id == tid,
                None => true,
            })
            .cloned()
            .collect();
        list.sort_by_key(|b| b.created_at_ms);
        list
    }

    pub fn mark_committed(&mut self, branch_id: &str) -> Result<Branch, BranchError> {
        let b = self
            .branches
            .get_mut(branch_id)
            .ok_or_else(|| BranchError::NotFound(branch_id.to_string()))?;
        if b.status == BranchStatus::Committed {
            return Err(BranchError::AlreadyCommitted(branch_id.to_string()));
        }
        if b.status == BranchStatus::Discarded {
            return Err(BranchError::AlreadyDiscarded(branch_id.to_string()));
        }
        b.status = BranchStatus::Committed;
        Ok(b.clone())
    }

    pub fn mark_discarded(&mut self, branch_id: &str) -> Result<Branch, BranchError> {
        let b = self
            .branches
            .get_mut(branch_id)
            .ok_or_else(|| BranchError::NotFound(branch_id.to_string()))?;
        if b.status == BranchStatus::Discarded {
            return Err(BranchError::AlreadyDiscarded(branch_id.to_string()));
        }
        if b.status == BranchStatus::Committed {
            return Err(BranchError::AlreadyCommitted(branch_id.to_string()));
        }
        b.status = BranchStatus::Discarded;
        Ok(b.clone())
    }

    pub fn active_branches(&self) -> Vec<Branch> {
        self.branches
            .values()
            .filter(|b| b.status == BranchStatus::Active)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_lifecycle() {
        let mut mgr = BranchManager::new();
        let b = Branch {
            branch_id: "test_branch".into(),
            parent_target_id: "parent_1".into(),
            branch_target_id: "branch_1".into(),
            browser_context_id: Some("ctx_1".into()),
            browser_host: "127.0.0.1".into(),
            browser_port: 9222,
            initial_url: "https://example.com".into(),
            status: BranchStatus::Active,
            created_at_ms: 1000,
        };

        assert!(mgr.insert(b.clone()).is_ok());
        // Inserting duplicate active fails
        assert_eq!(
            mgr.insert(b.clone()),
            Err(BranchError::AlreadyExists("test_branch".into()))
        );

        // List filtering
        assert_eq!(mgr.list(Some("parent_1")).len(), 1);
        assert_eq!(mgr.list(Some("other")).len(), 0);

        // Commit branch
        let committed = mgr.mark_committed("test_branch").unwrap();
        assert_eq!(committed.status, BranchStatus::Committed);

        // Cannot discard committed branch
        assert_eq!(
            mgr.mark_discarded("test_branch"),
            Err(BranchError::AlreadyCommitted("test_branch".into()))
        );
    }

    fn sample(id: &str) -> Branch {
        Branch {
            branch_id: id.into(),
            parent_target_id: "p".into(),
            branch_target_id: format!("t_{id}"),
            browser_context_id: None,
            browser_host: "127.0.0.1".into(),
            browser_port: 9222,
            initial_url: "about:blank".into(),
            status: BranchStatus::Active,
            created_at_ms: 1,
        }
    }

    #[test]
    fn active_branch_cap_is_enforced_and_freed_by_finalizing() {
        let mut mgr = BranchManager::with_max_active(2);
        mgr.insert(sample("a")).unwrap();
        mgr.insert(sample("b")).unwrap();
        assert_eq!(mgr.insert(sample("c")), Err(BranchError::LimitReached(2)));
        assert_eq!(mgr.check_capacity("c"), Err(BranchError::LimitReached(2)));
        mgr.mark_discarded("a").unwrap();
        mgr.insert(sample("c")).unwrap();
        assert_eq!(mgr.active_branches().len(), 2);
    }

    #[test]
    fn ensure_active_rejects_finalized_and_unknown() {
        let mut mgr = BranchManager::new();
        assert_eq!(
            mgr.ensure_active("x"),
            Err(BranchError::NotFound("x".into()))
        );
        mgr.insert(sample("x")).unwrap();
        assert!(mgr.ensure_active("x").is_ok());
        mgr.mark_committed("x").unwrap();
        assert_eq!(
            mgr.ensure_active("x"),
            Err(BranchError::AlreadyCommitted("x".into()))
        );
    }
}
