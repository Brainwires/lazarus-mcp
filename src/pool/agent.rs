//! Agent Handle
//!
//! Manages the lifecycle of individual background agents.

use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use super::locks::FileLockManager;
use super::task::{Task, TaskResult};

/// Status of a running agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Agent is starting up
    Starting,
    /// Agent is actively working
    Running {
        /// Current iteration
        iteration: u32,
        /// Description of current activity
        activity: String,
    },
    /// Agent completed successfully
    Completed {
        /// Summary of what was done
        summary: String,
    },
    /// Agent failed
    Failed {
        /// Error message
        error: String,
    },
    /// Agent was stopped
    Stopped,
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentStatus::Starting => write!(f, "Starting"),
            AgentStatus::Running { iteration, activity } => {
                write!(f, "Running (iteration {}: {})", iteration, activity)
            }
            AgentStatus::Completed { summary } => write!(f, "Completed: {}", summary),
            AgentStatus::Failed { error } => write!(f, "Failed: {}", error),
            AgentStatus::Stopped => write!(f, "Stopped"),
        }
    }
}

/// Configuration for an agent
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Path to the agent executable
    pub executable: PathBuf,
    /// Additional arguments
    pub args: Vec<String>,
    /// Skip permissions flag (if supported)
    pub skip_permissions_flag: Option<String>,
}

/// Handle to a running background agent
pub struct AgentHandle {
    /// Unique agent ID
    pub id: String,
    /// The task being executed
    task: Task,
    /// Current status
    status: Arc<RwLock<AgentStatus>>,
    /// Child process (if running)
    child: Option<Child>,
    /// Start time
    start_time: Instant,
    /// Reference to the file lock manager
    lock_manager: Arc<FileLockManager>,
}

impl AgentHandle {
    /// Create a new agent handle
    pub fn new(id: String, task: Task, lock_manager: Arc<FileLockManager>) -> Self {
        Self {
            id,
            task,
            status: Arc::new(RwLock::new(AgentStatus::Starting)),
            child: None,
            start_time: Instant::now(),
            lock_manager,
        }
    }

    /// Get the current status
    pub async fn status(&self) -> AgentStatus {
        self.status.read().await.clone()
    }

    /// Get the task
    pub fn task(&self) -> &Task {
        &self.task
    }

    /// Get elapsed time
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Start the agent process
    pub async fn start(&mut self, config: &AgentConfig) -> Result<()> {
        info!("Starting agent {} for task: {}", self.id, self.task.description);

        let mut cmd = Command::new(&config.executable);

        // Add skip permissions flag if available
        if let Some(flag) = &config.skip_permissions_flag {
            cmd.arg(flag);
        }

        // Add any additional args
        cmd.args(&config.args);

        // Set working directory if specified
        if let Some(dir) = &self.task.working_directory {
            cmd.current_dir(dir);
        }

        // Add the task as a prompt argument
        // For Claude, this would be passed via -p flag
        cmd.arg("-p").arg(&self.task.description);

        // Capture stdout/stderr for monitoring
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd.spawn().context("Failed to spawn agent process")?;
        self.child = Some(child);

        *self.status.write().await = AgentStatus::Running {
            iteration: 0,
            activity: "Starting".to_string(),
        };

        Ok(())
    }

    /// Check if the agent is still running
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Poll the agent for completion
    ///
    /// Returns Some(result) if completed, None if still running
    pub async fn poll(&mut self) -> Option<TaskResult> {
        let child = self.child.as_mut()?;

        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(1);
                self.child = None;

                // Release all locks held by this agent
                self.lock_manager.release_all(&self.id).await;

                if code == 0 {
                    let result = TaskResult::success(
                        self.task.id.clone(),
                        "Task completed".to_string(),
                        self.task.max_iterations,
                    );
                    *self.status.write().await = AgentStatus::Completed {
                        summary: result.summary.clone(),
                    };
                    Some(result)
                } else {
                    let result = TaskResult::failure(
                        self.task.id.clone(),
                        format!("Agent exited with code {}", code),
                        self.task.max_iterations,
                    );
                    *self.status.write().await = AgentStatus::Failed {
                        error: result.error.clone().unwrap_or_default(),
                    };
                    Some(result)
                }
            }
            Ok(None) => None, // Still running
            Err(e) => {
                error!("Error polling agent {}: {}", self.id, e);
                self.child = None;
                self.lock_manager.release_all(&self.id).await;

                let result = TaskResult::failure(
                    self.task.id.clone(),
                    format!("Error polling agent: {}", e),
                    0,
                );
                // Need to use async block properly
                let status = AgentStatus::Failed {
                    error: result.error.clone().unwrap_or_default(),
                };
                *self.status.write().await = status;
                Some(result)
            }
        }
    }

    /// Stop the agent gracefully
    pub async fn stop(&mut self) -> Result<()> {
        info!("Stopping agent {}", self.id);

        if let Some(child) = &self.child {
            let pid = Pid::from_raw(child.id() as i32);

            // Try SIGINT first
            let _ = signal::kill(pid, Signal::SIGINT);

            // Wait with timeout escalation
            let start = Instant::now();
            loop {
                match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
                    Ok(WaitStatus::StillAlive) => {
                        if start.elapsed() > Duration::from_secs(3) {
                            warn!("Agent {} not responding to SIGINT, sending SIGTERM", self.id);
                            let _ = signal::kill(pid, Signal::SIGTERM);
                        }
                        if start.elapsed() > Duration::from_secs(5) {
                            warn!("Agent {} not responding to SIGTERM, sending SIGKILL", self.id);
                            let _ = signal::kill(pid, Signal::SIGKILL);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    _ => break,
                }
            }
        }

        self.child = None;
        self.lock_manager.release_all(&self.id).await;
        *self.status.write().await = AgentStatus::Stopped;

        Ok(())
    }

    /// Update the agent's activity status
    pub async fn set_activity(&self, iteration: u32, activity: impl Into<String>) {
        *self.status.write().await = AgentStatus::Running {
            iteration,
            activity: activity.into(),
        };
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        // Try to kill the child process if still running
        if let Some(mut child) = self.child.take() {
            debug!("AgentHandle dropped, killing child process");
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agent_status_display() {
        let status = AgentStatus::Running {
            iteration: 5,
            activity: "Writing code".to_string(),
        };
        assert!(status.to_string().contains("iteration 5"));
        assert!(status.to_string().contains("Writing code"));
    }

    #[tokio::test]
    async fn test_agent_handle_creation() {
        let lock_manager = Arc::new(FileLockManager::new());
        let task = Task::new("Test task");
        let handle = AgentHandle::new("agent-1".to_string(), task, lock_manager);

        assert_eq!(handle.id, "agent-1");
        matches!(handle.status().await, AgentStatus::Starting);
    }
}
