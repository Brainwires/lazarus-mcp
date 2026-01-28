//! Task Definition
//!
//! Defines the Task struct that represents work to be done by an agent.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Priority level for tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskPriority {
    Low,
    Normal,
    High,
    Urgent,
}

impl Default for TaskPriority {
    fn default() -> Self {
        TaskPriority::Normal
    }
}

/// A task to be executed by an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task ID
    pub id: String,
    /// Description of what the task should accomplish
    pub description: String,
    /// Priority level
    pub priority: TaskPriority,
    /// Working directory for the agent
    pub working_directory: Option<PathBuf>,
    /// Maximum iterations before giving up
    pub max_iterations: u32,
    /// Type of agent to use (claude, aider, cursor)
    pub agent_type: String,
}

impl Task {
    /// Create a new task with default settings
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            description: description.into(),
            priority: TaskPriority::Normal,
            working_directory: None,
            max_iterations: 50,
            agent_type: "claude".to_string(),
        }
    }

    /// Set the working directory
    pub fn with_working_directory(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// Set the maximum iterations
    pub fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }

    /// Set the agent type
    pub fn with_agent_type(mut self, agent_type: impl Into<String>) -> Self {
        self.agent_type = agent_type.into();
        self
    }

    /// Set the priority
    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }
}

/// Result of a completed task
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Task ID
    pub task_id: String,
    /// Whether the task completed successfully
    pub success: bool,
    /// Summary of what was accomplished
    pub summary: String,
    /// Number of iterations used
    pub iterations: u32,
    /// Any error message if failed
    pub error: Option<String>,
}

impl TaskResult {
    /// Create a successful result
    pub fn success(task_id: String, summary: String, iterations: u32) -> Self {
        Self {
            task_id,
            success: true,
            summary,
            iterations,
            error: None,
        }
    }

    /// Create a failure result
    pub fn failure(task_id: String, error: String, iterations: u32) -> Self {
        Self {
            task_id,
            success: false,
            summary: String::new(),
            iterations,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_creation() {
        let task = Task::new("Test task");
        assert!(!task.id.is_empty());
        assert_eq!(task.description, "Test task");
        assert_eq!(task.priority, TaskPriority::Normal);
        assert_eq!(task.max_iterations, 50);
    }

    #[test]
    fn test_task_builder() {
        let task = Task::new("Complex task")
            .with_working_directory("/tmp/test")
            .with_max_iterations(100)
            .with_agent_type("aider")
            .with_priority(TaskPriority::High);

        assert_eq!(task.working_directory, Some(PathBuf::from("/tmp/test")));
        assert_eq!(task.max_iterations, 100);
        assert_eq!(task.agent_type, "aider");
        assert_eq!(task.priority, TaskPriority::High);
    }

    #[test]
    fn test_task_result() {
        let success = TaskResult::success("task-1".to_string(), "Done".to_string(), 5);
        assert!(success.success);
        assert!(success.error.is_none());

        let failure = TaskResult::failure("task-2".to_string(), "Failed".to_string(), 10);
        assert!(!failure.success);
        assert_eq!(failure.error, Some("Failed".to_string()));
    }

    #[test]
    fn test_priority_ordering() {
        assert!(TaskPriority::Low < TaskPriority::Normal);
        assert!(TaskPriority::Normal < TaskPriority::High);
        assert!(TaskPriority::High < TaskPriority::Urgent);
    }
}
