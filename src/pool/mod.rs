//! Agent Pool
//!
//! Manages a pool of background task agents with spawn, monitor, and coordinate capabilities.

mod agent;
mod locks;
mod task;

pub use agent::{AgentConfig, AgentHandle, AgentStatus};
pub use locks::{FileLockManager, LockInfo, LockType};
pub use task::{Task, TaskPriority, TaskResult};

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Statistics about the agent pool
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Maximum number of agents allowed
    pub max_agents: usize,
    /// Number of agents currently in the pool
    pub total_agents: usize,
    /// Number of actively running agents
    pub running: usize,
    /// Number of completed agents (waiting for cleanup)
    pub completed: usize,
    /// Number of failed agents
    pub failed: usize,
}

/// Manages a pool of background task agents
pub struct AgentPool {
    /// Maximum number of concurrent agents
    max_agents: usize,
    /// Running agents
    agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    /// Shared file lock manager
    lock_manager: Arc<FileLockManager>,
    /// Agent configurations by type
    agent_configs: HashMap<String, AgentConfig>,
}

impl AgentPool {
    /// Create a new agent pool
    pub fn new(max_agents: usize) -> Self {
        Self {
            max_agents,
            agents: Arc::new(RwLock::new(HashMap::new())),
            lock_manager: Arc::new(FileLockManager::new()),
            agent_configs: Self::default_agent_configs(),
        }
    }

    /// Get default agent configurations
    fn default_agent_configs() -> HashMap<String, AgentConfig> {
        let mut configs = HashMap::new();

        // Try to find Claude
        if let Some(path) = Self::find_agent_executable("claude") {
            configs.insert(
                "claude".to_string(),
                AgentConfig {
                    executable: path,
                    args: vec![],
                    skip_permissions_flag: Some("--dangerously-skip-permissions".to_string()),
                },
            );
        }

        // Try to find Aider
        if let Some(path) = Self::find_agent_executable("aider") {
            configs.insert(
                "aider".to_string(),
                AgentConfig {
                    executable: path,
                    args: vec![],
                    skip_permissions_flag: Some("--yes".to_string()),
                },
            );
        }

        // Try to find Cursor
        if let Some(path) = Self::find_agent_executable("cursor") {
            configs.insert(
                "cursor".to_string(),
                AgentConfig {
                    executable: path,
                    args: vec![],
                    skip_permissions_flag: None,
                },
            );
        }

        configs
    }

    /// Find an agent executable
    fn find_agent_executable(name: &str) -> Option<PathBuf> {
        // Try which first
        if let Ok(path) = which::which(name) {
            return Some(path);
        }

        // Try common locations
        let candidates = [
            PathBuf::from(format!("/usr/local/bin/{}", name)),
            PathBuf::from(format!("/usr/bin/{}", name)),
        ];

        // Add home directory locations
        let home_candidates = if let Some(home) = dirs::home_dir() {
            vec![
                home.join(format!(".local/bin/{}", name)),
                home.join(format!(".local/share/{}/{}", name, name)),
            ]
        } else {
            vec![]
        };

        for candidate in candidates.iter().chain(home_candidates.iter()) {
            if candidate.exists() && candidate.is_file() {
                return Some(candidate.clone());
            }
        }

        None
    }

    /// Spawn a new background agent
    ///
    /// Returns the agent ID if successful.
    pub async fn spawn(&self, task: Task) -> Result<String> {
        let agents = self.agents.read().await;
        if agents.len() >= self.max_agents {
            return Err(anyhow!(
                "Agent pool is full ({}/{})",
                agents.len(),
                self.max_agents
            ));
        }
        drop(agents);

        // Get the agent config
        let config = self
            .agent_configs
            .get(&task.agent_type)
            .ok_or_else(|| anyhow!("Unknown agent type: {}", task.agent_type))?
            .clone();

        let agent_id = format!("agent-{}", uuid::Uuid::new_v4());
        let mut handle = AgentHandle::new(
            agent_id.clone(),
            task,
            Arc::clone(&self.lock_manager),
        );

        // Start the agent process
        handle.start(&config).await?;

        // Add to pool
        let mut agents = self.agents.write().await;
        agents.insert(agent_id.clone(), handle);

        info!("Spawned agent {}", agent_id);
        Ok(agent_id)
    }

    /// Get the status of an agent
    pub async fn status(&self, agent_id: &str) -> Option<AgentStatus> {
        let agents = self.agents.read().await;
        if let Some(handle) = agents.get(agent_id) {
            Some(handle.status().await)
        } else {
            None
        }
    }

    /// List all agents with their status
    pub async fn list(&self) -> Vec<(String, AgentStatus)> {
        let agents = self.agents.read().await;
        let mut result = Vec::with_capacity(agents.len());

        for (id, handle) in agents.iter() {
            result.push((id.clone(), handle.status().await));
        }

        result
    }

    /// Stop an agent
    pub async fn stop(&self, agent_id: &str) -> Result<()> {
        let mut agents = self.agents.write().await;
        if let Some(mut handle) = agents.remove(agent_id) {
            handle.stop().await?;
            Ok(())
        } else {
            Err(anyhow!("Agent {} not found", agent_id))
        }
    }

    /// Wait for an agent to complete
    pub async fn await_completion(&self, agent_id: &str) -> Result<TaskResult> {
        loop {
            // Check if agent exists and poll it
            {
                let mut agents = self.agents.write().await;
                if let Some(handle) = agents.get_mut(agent_id) {
                    if let Some(result) = handle.poll().await {
                        // Agent completed, remove from pool
                        agents.remove(agent_id);
                        return Ok(result);
                    }
                } else {
                    return Err(anyhow!("Agent {} not found", agent_id));
                }
            }

            // Wait a bit before polling again
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    /// Wait for an agent with timeout
    pub async fn await_completion_timeout(
        &self,
        agent_id: &str,
        timeout: std::time::Duration,
    ) -> Result<TaskResult> {
        tokio::time::timeout(timeout, self.await_completion(agent_id))
            .await
            .map_err(|_| anyhow!("Timeout waiting for agent {}", agent_id))?
    }

    /// Get pool statistics
    pub async fn stats(&self) -> PoolStats {
        let agents = self.agents.read().await;
        let mut running = 0;
        let mut completed = 0;
        let mut failed = 0;

        for (_, handle) in agents.iter() {
            match handle.status().await {
                AgentStatus::Running { .. } | AgentStatus::Starting => running += 1,
                AgentStatus::Completed { .. } => completed += 1,
                AgentStatus::Failed { .. } => failed += 1,
                AgentStatus::Stopped => {}
            }
        }

        PoolStats {
            max_agents: self.max_agents,
            total_agents: agents.len(),
            running,
            completed,
            failed,
        }
    }

    /// Get the file lock manager
    pub fn lock_manager(&self) -> Arc<FileLockManager> {
        Arc::clone(&self.lock_manager)
    }

    /// Cleanup completed agents
    pub async fn cleanup_completed(&self) -> Vec<(String, TaskResult)> {
        let mut completed = Vec::new();
        let mut to_remove = Vec::new();

        // First identify completed agents
        {
            let mut agents = self.agents.write().await;
            for (id, handle) in agents.iter_mut() {
                if let Some(result) = handle.poll().await {
                    completed.push((id.clone(), result));
                    to_remove.push(id.clone());
                }
            }

            // Remove them
            for id in to_remove {
                agents.remove(&id);
            }
        }

        completed
    }

    /// Shutdown the pool, stopping all agents
    pub async fn shutdown(&self) {
        info!("Shutting down agent pool");
        let mut agents = self.agents.write().await;
        for (id, mut handle) in agents.drain() {
            debug!("Stopping agent {}", id);
            let _ = handle.stop().await;
        }
    }

    /// Check if an agent is running
    pub async fn is_running(&self, agent_id: &str) -> bool {
        let agents = self.agents.read().await;
        if let Some(handle) = agents.get(agent_id) {
            handle.is_running()
        } else {
            false
        }
    }

    /// Get the number of active agents
    pub async fn active_count(&self) -> usize {
        self.agents.read().await.len()
    }
}

impl Default for AgentPool {
    fn default() -> Self {
        Self::new(5) // Default to 5 concurrent agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = AgentPool::new(10);
        assert_eq!(pool.max_agents, 10);
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn test_pool_stats() {
        let pool = AgentPool::new(10);
        let stats = pool.stats().await;
        assert_eq!(stats.max_agents, 10);
        assert_eq!(stats.total_agents, 0);
        assert_eq!(stats.running, 0);
    }

    #[tokio::test]
    async fn test_pool_default() {
        let pool = AgentPool::default();
        assert_eq!(pool.max_agents, 5);
    }
}
