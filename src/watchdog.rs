//! Watchdog module for process health monitoring
//!
//! Detects unresponsive child processes and handles lockup recovery.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Default heartbeat timeout (60 seconds)
pub const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default check interval (1 second)
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Action to take when a process is detected as unresponsive
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockupAction {
    /// Log a warning but take no action
    Warn,
    /// Automatically restart the agent
    Restart,
    /// Restart with exponential backoff
    RestartWithBackoff,
    /// Kill the process and don't restart
    Kill,
    /// Notify and wait for user decision (for TUI mode)
    NotifyAndWait,
}

impl Default for LockupAction {
    fn default() -> Self {
        Self::Restart
    }
}

/// Watchdog configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Whether watchdog is enabled
    pub enabled: bool,
    /// Maximum time without activity before considering process hung
    #[serde(with = "humantime_serde")]
    pub heartbeat_timeout: Duration,
    /// How often to check process health
    #[serde(with = "humantime_serde")]
    pub check_interval: Duration,
    /// Maximum memory usage in MB (None = unlimited)
    pub max_memory_mb: Option<u64>,
    /// Maximum CPU percentage (None = unlimited)
    pub max_cpu_percent: Option<f32>,
    /// Action to take on lockup
    pub lockup_action: LockupAction,
    /// Number of consecutive unresponsive checks before action
    pub unresponsive_threshold: u32,
}

mod humantime_serde {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            check_interval: DEFAULT_CHECK_INTERVAL,
            max_memory_mb: None,
            max_cpu_percent: None,
            lockup_action: LockupAction::Restart,
            unresponsive_threshold: 3,
        }
    }
}

/// Current state of a monitored process
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// Process is starting up
    Starting,
    /// Process has recent activity
    Active,
    /// Process is idle but responsive (no activity, but within timeout)
    Idle,
    /// Process hasn't had activity past threshold
    Unresponsive,
    /// Process is using excessive resources
    HighResource,
    /// Process has exited
    Exited,
}

/// Activity timestamps for a monitored process
#[derive(Debug)]
pub struct ProcessActivity {
    /// Process ID
    pub pid: u32,
    /// When the process was started
    pub started_at: Instant,
    /// Last stdout activity
    pub last_stdout: Option<Instant>,
    /// Last stderr activity
    pub last_stderr: Option<Instant>,
    /// Last MCP tool call
    pub last_mcp_call: Option<Instant>,
    /// Last file I/O activity
    pub last_file_activity: Option<Instant>,
    /// Manual ping from the agent
    pub last_ping: Option<Instant>,
    /// Current computed state
    pub current_state: ProcessState,
    /// Consecutive unresponsive checks
    pub unresponsive_count: u32,
    /// Current memory usage in MB
    pub memory_mb: u64,
    /// Current CPU percentage
    pub cpu_percent: f32,
}

impl ProcessActivity {
    /// Create new activity tracker for a process
    pub fn new(pid: u32) -> Self {
        let now = Instant::now();
        Self {
            pid,
            started_at: now,
            last_stdout: Some(now), // Consider startup as activity
            last_stderr: None,
            last_mcp_call: None,
            last_file_activity: None,
            last_ping: None,
            current_state: ProcessState::Starting,
            unresponsive_count: 0,
            memory_mb: 0,
            cpu_percent: 0.0,
        }
    }

    /// Record stdout activity
    pub fn record_stdout(&mut self) {
        self.last_stdout = Some(Instant::now());
    }

    /// Record stderr activity
    pub fn record_stderr(&mut self) {
        self.last_stderr = Some(Instant::now());
    }

    /// Record MCP tool call
    pub fn record_mcp_call(&mut self) {
        self.last_mcp_call = Some(Instant::now());
    }

    /// Record file I/O activity
    pub fn record_file_activity(&mut self) {
        self.last_file_activity = Some(Instant::now());
    }

    /// Record manual ping from agent
    pub fn record_ping(&mut self) {
        self.last_ping = Some(Instant::now());
    }

    /// Get the most recent activity timestamp
    pub fn last_activity(&self) -> Instant {
        [
            self.last_stdout,
            self.last_stderr,
            self.last_mcp_call,
            self.last_file_activity,
            self.last_ping,
        ]
        .iter()
        .filter_map(|t| *t)
        .max()
        .unwrap_or(self.started_at)
    }

    /// Get time since last activity
    pub fn time_since_activity(&self) -> Duration {
        self.last_activity().elapsed()
    }

    /// Get uptime
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }
}

/// Health check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub state: ProcessState,
    pub uptime_secs: u64,
    pub last_activity_secs: u64,
    pub memory_mb: u64,
    pub cpu_percent: f32,
    pub unresponsive_count: u32,
    pub action_pending: Option<LockupAction>,
}

/// Watchdog manager for monitoring process health (sync version)
pub struct Watchdog {
    config: RwLock<WatchdogConfig>,
    activity: Mutex<Option<ProcessActivity>>,
    system: Mutex<System>,
    /// Shared counter for activity updates from other threads
    activity_counter: AtomicU64,
    /// Flag indicating watchdog is temporarily disabled
    disabled_until: Mutex<Option<Instant>>,
}

impl Watchdog {
    /// Create a new watchdog with default configuration
    pub fn new() -> Self {
        Self {
            config: RwLock::new(WatchdogConfig::default()),
            activity: Mutex::new(None),
            system: Mutex::new(System::new()),
            activity_counter: AtomicU64::new(0),
            disabled_until: Mutex::new(None),
        }
    }

    /// Create a new watchdog with custom configuration
    pub fn with_config(config: WatchdogConfig) -> Self {
        Self {
            config: RwLock::new(config),
            activity: Mutex::new(None),
            system: Mutex::new(System::new()),
            activity_counter: AtomicU64::new(0),
            disabled_until: Mutex::new(None),
        }
    }

    /// Start monitoring a process
    pub fn start_monitoring(&self, pid: u32) {
        let mut activity = self.activity.lock().unwrap();
        *activity = Some(ProcessActivity::new(pid));
    }

    /// Stop monitoring
    pub fn stop_monitoring(&self) {
        let mut activity = self.activity.lock().unwrap();
        *activity = None;
    }

    /// Record activity (thread-safe, non-blocking)
    pub fn record_activity(&self) {
        self.activity_counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Update configuration
    pub fn configure(&self, config: WatchdogConfig) {
        let mut cfg = self.config.write().unwrap();
        *cfg = config;
    }

    /// Get current configuration
    pub fn get_config(&self) -> WatchdogConfig {
        self.config.read().unwrap().clone()
    }

    /// Temporarily disable watchdog
    pub fn disable_for(&self, duration: Duration) {
        let mut disabled = self.disabled_until.lock().unwrap();
        *disabled = Some(Instant::now() + duration);
    }

    /// Re-enable watchdog
    pub fn enable(&self) {
        let mut disabled = self.disabled_until.lock().unwrap();
        *disabled = None;
    }

    /// Check if watchdog is currently disabled
    pub fn is_disabled(&self) -> bool {
        let disabled = self.disabled_until.lock().unwrap();
        if let Some(until) = *disabled {
            if Instant::now() < until {
                return true;
            }
        }
        false
    }

    /// Record stdout activity
    pub fn record_stdout(&self) {
        if let Some(ref mut activity) = *self.activity.lock().unwrap() {
            activity.record_stdout();
        }
    }

    /// Record stderr activity
    pub fn record_stderr(&self) {
        if let Some(ref mut activity) = *self.activity.lock().unwrap() {
            activity.record_stderr();
        }
    }

    /// Record MCP call activity
    pub fn record_mcp_call(&self) {
        if let Some(ref mut activity) = *self.activity.lock().unwrap() {
            activity.record_mcp_call();
        }
    }

    /// Record ping from agent
    pub fn record_ping(&self) {
        if let Some(ref mut activity) = *self.activity.lock().unwrap() {
            activity.record_ping();
        }
    }

    /// Perform health check and return status
    pub fn check_health(&self) -> Option<HealthStatus> {
        let config = self.config.read().unwrap();
        if !config.enabled {
            return None;
        }

        // Check if disabled
        if self.is_disabled() {
            return None;
        }

        let mut activity = self.activity.lock().unwrap();
        let activity = activity.as_mut()?;

        // Process any queued activity updates
        let counter = self.activity_counter.swap(0, Ordering::Relaxed);
        if counter > 0 {
            activity.last_stdout = Some(Instant::now());
        }

        // Update system info for this process
        {
            let mut sys = self.system.lock().unwrap();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[Pid::from_u32(activity.pid)]),
                true,
                ProcessRefreshKind::everything(),
            );

            if let Some(proc) = sys.process(Pid::from_u32(activity.pid)) {
                activity.memory_mb = proc.memory() / (1024 * 1024);
                activity.cpu_percent = proc.cpu_usage();
            }
        }

        // Determine current state
        let time_since = activity.time_since_activity();
        let mut action_pending = None;

        // Check for high resource usage
        if let Some(max_mem) = config.max_memory_mb {
            if activity.memory_mb > max_mem {
                activity.current_state = ProcessState::HighResource;
                action_pending = Some(config.lockup_action);
            }
        }
        if let Some(max_cpu) = config.max_cpu_percent {
            if activity.cpu_percent > max_cpu {
                activity.current_state = ProcessState::HighResource;
                action_pending = Some(config.lockup_action);
            }
        }

        // Check for unresponsive
        if activity.current_state != ProcessState::HighResource {
            if time_since > config.heartbeat_timeout {
                activity.unresponsive_count += 1;
                activity.current_state = ProcessState::Unresponsive;

                if activity.unresponsive_count >= config.unresponsive_threshold {
                    action_pending = Some(config.lockup_action);
                }
            } else if time_since > config.heartbeat_timeout / 2 {
                activity.current_state = ProcessState::Idle;
                activity.unresponsive_count = 0;
            } else {
                activity.current_state = ProcessState::Active;
                activity.unresponsive_count = 0;
            }
        }

        Some(HealthStatus {
            state: activity.current_state,
            uptime_secs: activity.uptime().as_secs(),
            last_activity_secs: time_since.as_secs(),
            memory_mb: activity.memory_mb,
            cpu_percent: activity.cpu_percent,
            unresponsive_count: activity.unresponsive_count,
            action_pending,
        })
    }

    /// Get current health status without triggering actions
    pub fn get_status(&self) -> Option<HealthStatus> {
        let config = self.config.read().unwrap();
        let activity = self.activity.lock().unwrap();
        let activity = activity.as_ref()?;

        let time_since = activity.time_since_activity();

        Some(HealthStatus {
            state: activity.current_state,
            uptime_secs: activity.uptime().as_secs(),
            last_activity_secs: time_since.as_secs(),
            memory_mb: activity.memory_mb,
            cpu_percent: activity.cpu_percent,
            unresponsive_count: activity.unresponsive_count,
            action_pending: if activity.unresponsive_count >= config.unresponsive_threshold {
                Some(config.lockup_action)
            } else {
                None
            },
        })
    }

    /// Get the monitored PID
    pub fn get_pid(&self) -> Option<u32> {
        self.activity.lock().unwrap().as_ref().map(|a| a.pid)
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared watchdog instance
pub type SharedWatchdog = Arc<Watchdog>;

/// Create a new shared watchdog
pub fn create_watchdog() -> SharedWatchdog {
    Arc::new(Watchdog::new())
}

/// Create a new shared watchdog with config
pub fn create_watchdog_with_config(config: WatchdogConfig) -> SharedWatchdog {
    Arc::new(Watchdog::with_config(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watchdog_basic() {
        let watchdog = Watchdog::new();
        watchdog.start_monitoring(1234);

        // Should be active initially
        let status = watchdog.get_status();
        assert!(status.is_some());
        let status = status.unwrap();
        assert!(matches!(
            status.state,
            ProcessState::Starting | ProcessState::Active
        ));
    }

    #[test]
    fn test_watchdog_activity_recording() {
        let watchdog = Watchdog::new();
        watchdog.start_monitoring(1234);

        watchdog.record_stdout();
        watchdog.record_mcp_call();
        watchdog.record_ping();

        let status = watchdog.get_status().unwrap();
        assert_eq!(status.last_activity_secs, 0);
    }

    #[test]
    fn test_watchdog_disable() {
        let watchdog = Watchdog::new();
        watchdog.start_monitoring(1234);

        watchdog.disable_for(Duration::from_secs(60));
        assert!(watchdog.is_disabled());

        watchdog.enable();
        assert!(!watchdog.is_disabled());
    }
}
