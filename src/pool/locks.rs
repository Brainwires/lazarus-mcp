//! File Lock Manager
//!
//! Prevents concurrent file edits by multiple agents.
//! Supports read/write lock types with agent-scoped locks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Type of lock held on a file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    /// Multiple readers allowed
    Read,
    /// Exclusive write access
    Write,
}

/// Information about a held lock
#[derive(Debug, Clone)]
pub struct LockInfo {
    /// Agent holding the lock
    pub agent_id: String,
    /// Type of lock
    pub lock_type: LockType,
}

/// Manages file locks across all agents
#[derive(Debug)]
pub struct FileLockManager {
    /// Map from file path to lock info
    locks: Arc<RwLock<HashMap<PathBuf, LockInfo>>>,
}

impl FileLockManager {
    /// Create a new file lock manager
    pub fn new() -> Self {
        Self {
            locks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Try to acquire a lock on a file
    ///
    /// Returns true if the lock was acquired, false if blocked.
    pub async fn try_acquire(
        &self,
        path: impl AsRef<Path>,
        agent_id: &str,
        lock_type: LockType,
    ) -> bool {
        let path = path.as_ref().to_path_buf();
        let mut locks = self.locks.write().await;

        if let Some(existing) = locks.get(&path) {
            // Check if the existing lock blocks this request
            match (existing.lock_type, lock_type) {
                // Multiple readers allowed
                (LockType::Read, LockType::Read) => return true,
                // Same agent can upgrade/downgrade
                _ if existing.agent_id == agent_id => {
                    locks.insert(
                        path,
                        LockInfo {
                            agent_id: agent_id.to_string(),
                            lock_type,
                        },
                    );
                    return true;
                }
                // Blocked by existing lock
                _ => return false,
            }
        }

        // No existing lock, acquire it
        locks.insert(
            path,
            LockInfo {
                agent_id: agent_id.to_string(),
                lock_type,
            },
        );
        true
    }

    /// Release a lock on a file
    pub async fn release(&self, path: impl AsRef<Path>, agent_id: &str) -> bool {
        let path = path.as_ref().to_path_buf();
        let mut locks = self.locks.write().await;

        if let Some(info) = locks.get(&path) {
            if info.agent_id == agent_id {
                locks.remove(&path);
                return true;
            }
        }
        false
    }

    /// Release all locks held by an agent
    pub async fn release_all(&self, agent_id: &str) {
        let mut locks = self.locks.write().await;
        locks.retain(|_, info| info.agent_id != agent_id);
    }

    /// List all currently held locks
    pub async fn list_locks(&self) -> Vec<(PathBuf, LockInfo)> {
        let locks = self.locks.read().await;
        locks
            .iter()
            .map(|(path, info)| (path.clone(), info.clone()))
            .collect()
    }

    /// Get the lock info for a specific file
    pub async fn get_lock_info(&self, path: impl AsRef<Path>) -> Option<LockInfo> {
        let locks = self.locks.read().await;
        locks.get(path.as_ref()).cloned()
    }

    /// Check if an agent holds a lock on a file
    pub async fn is_locked_by(&self, path: impl AsRef<Path>, agent_id: &str) -> bool {
        let locks = self.locks.read().await;
        locks
            .get(path.as_ref())
            .map(|info| info.agent_id == agent_id)
            .unwrap_or(false)
    }

    /// Get all locks held by a specific agent
    pub async fn locks_held_by(&self, agent_id: &str) -> Vec<(PathBuf, LockType)> {
        let locks = self.locks.read().await;
        locks
            .iter()
            .filter(|(_, info)| info.agent_id == agent_id)
            .map(|(path, info)| (path.clone(), info.lock_type))
            .collect()
    }
}

impl Default for FileLockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_acquire_release() {
        let manager = FileLockManager::new();

        // Acquire a write lock
        assert!(manager.try_acquire("/tmp/test.txt", "agent-1", LockType::Write).await);

        // Same agent can re-acquire
        assert!(manager.try_acquire("/tmp/test.txt", "agent-1", LockType::Write).await);

        // Different agent is blocked
        assert!(!manager.try_acquire("/tmp/test.txt", "agent-2", LockType::Write).await);
        assert!(!manager.try_acquire("/tmp/test.txt", "agent-2", LockType::Read).await);

        // Release and verify
        assert!(manager.release("/tmp/test.txt", "agent-1").await);

        // Now agent-2 can acquire
        assert!(manager.try_acquire("/tmp/test.txt", "agent-2", LockType::Write).await);
    }

    #[tokio::test]
    async fn test_multiple_readers() {
        let manager = FileLockManager::new();

        // Multiple read locks allowed
        assert!(manager.try_acquire("/tmp/test.txt", "agent-1", LockType::Read).await);
        assert!(manager.try_acquire("/tmp/test.txt", "agent-2", LockType::Read).await);

        // But write is blocked
        assert!(!manager.try_acquire("/tmp/test.txt", "agent-3", LockType::Write).await);
    }

    #[tokio::test]
    async fn test_release_all() {
        let manager = FileLockManager::new();

        manager.try_acquire("/tmp/a.txt", "agent-1", LockType::Write).await;
        manager.try_acquire("/tmp/b.txt", "agent-1", LockType::Write).await;
        manager.try_acquire("/tmp/c.txt", "agent-2", LockType::Write).await;

        // Release all for agent-1
        manager.release_all("agent-1").await;

        // agent-1's locks should be released
        assert!(manager.try_acquire("/tmp/a.txt", "agent-3", LockType::Write).await);
        assert!(manager.try_acquire("/tmp/b.txt", "agent-3", LockType::Write).await);

        // agent-2's lock should still be held
        assert!(!manager.try_acquire("/tmp/c.txt", "agent-3", LockType::Write).await);
    }

    #[tokio::test]
    async fn test_list_locks() {
        let manager = FileLockManager::new();

        manager.try_acquire("/tmp/a.txt", "agent-1", LockType::Write).await;
        manager.try_acquire("/tmp/b.txt", "agent-2", LockType::Read).await;

        let locks = manager.list_locks().await;
        assert_eq!(locks.len(), 2);
    }

    #[tokio::test]
    async fn test_locks_held_by() {
        let manager = FileLockManager::new();

        manager.try_acquire("/tmp/a.txt", "agent-1", LockType::Write).await;
        manager.try_acquire("/tmp/b.txt", "agent-1", LockType::Read).await;
        manager.try_acquire("/tmp/c.txt", "agent-2", LockType::Write).await;

        let agent1_locks = manager.locks_held_by("agent-1").await;
        assert_eq!(agent1_locks.len(), 2);

        let agent2_locks = manager.locks_held_by("agent-2").await;
        assert_eq!(agent2_locks.len(), 1);
    }
}
