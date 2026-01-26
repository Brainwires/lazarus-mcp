use anyhow::{Context, Result};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// Manages the lifecycle of the wrapped MCP server process
pub struct ProcessManager {
    /// The command to run
    command: String,
    /// Arguments for the command
    args: Vec<String>,
    /// Server name (for logging)
    name: String,
    /// The child process handle
    child: Arc<Mutex<Option<Child>>>,
    /// When the current process was started
    start_time: Arc<Mutex<Instant>>,
    /// Number of restarts
    restart_count: AtomicU32,
    /// Channel to send lines from child stdout
    stdout_tx: mpsc::Sender<String>,
    /// Channel to send lines to child stdin
    stdin_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    /// Sender for stdin (kept for cloning)
    #[allow(dead_code)]
    stdin_tx: mpsc::Sender<String>,
}

impl ProcessManager {
    /// Create a new process manager
    pub fn new(
        name: String,
        command: String,
        args: Vec<String>,
        stdout_tx: mpsc::Sender<String>,
    ) -> (Self, mpsc::Sender<String>) {
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(100);

        (
            Self {
                command,
                args,
                name,
                child: Arc::new(Mutex::new(None)),
                start_time: Arc::new(Mutex::new(Instant::now())),
                restart_count: AtomicU32::new(0),
                stdout_tx,
                stdin_rx: Arc::new(Mutex::new(stdin_rx)),
                stdin_tx: stdin_tx.clone(),
            },
            stdin_tx,
        )
    }

    /// Spawn the wrapped server process
    pub async fn spawn(&self) -> Result<()> {
        info!(
            name = %self.name,
            command = %self.command,
            args = ?self.args,
            "Spawning wrapped MCP server"
        );

        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // Pass stderr through for debugging
            .spawn()
            .with_context(|| format!("Failed to spawn command: {}", self.command))?;

        let pid = child.id().unwrap_or(0);
        info!(name = %self.name, pid = pid, "Wrapped server started");

        // Take ownership of stdin/stdout
        let child_stdin = child.stdin.take().expect("Failed to get child stdin");
        let child_stdout = child.stdout.take().expect("Failed to get child stdout");

        // Store the child
        *self.child.lock().await = Some(child);
        *self.start_time.lock().await = Instant::now();

        // Spawn task to read from child stdout and forward
        let stdout_tx = self.stdout_tx.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(child_stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!(server = %name, "Child stdout: {}", line);
                if stdout_tx.send(line).await.is_err() {
                    break;
                }
            }
            debug!(server = %name, "Child stdout reader finished");
        });

        // Spawn task to write to child stdin
        let stdin_rx = Arc::clone(&self.stdin_rx);
        let name = self.name.clone();
        tokio::spawn(async move {
            let mut stdin = child_stdin;
            let mut rx = stdin_rx.lock().await;
            while let Some(line) = rx.recv().await {
                debug!(server = %name, "Writing to child stdin: {}", line);
                if let Err(e) = stdin.write_all(line.as_bytes()).await {
                    error!(server = %name, error = %e, "Failed to write to child stdin");
                    break;
                }
                if let Err(e) = stdin.write_all(b"\n").await {
                    error!(server = %name, error = %e, "Failed to write newline to child stdin");
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    error!(server = %name, error = %e, "Failed to flush child stdin");
                    break;
                }
            }
            debug!(server = %name, "Child stdin writer finished");
        });

        Ok(())
    }

    /// Kill the current process gracefully
    pub async fn kill(&self) -> Result<()> {
        let mut child_guard = self.child.lock().await;
        if let Some(ref mut child) = *child_guard {
            let pid = child.id().unwrap_or(0);
            info!(name = %self.name, pid = pid, "Stopping wrapped server");

            // Try graceful shutdown first (SIGTERM on Unix)
            #[cfg(unix)]
            {
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid;

                if let Some(pid) = child.id() {
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }

            // Wait up to 5 seconds for graceful shutdown
            let timeout = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

            match timeout {
                Ok(Ok(status)) => {
                    info!(name = %self.name, status = ?status, "Server stopped gracefully");
                }
                Ok(Err(e)) => {
                    warn!(name = %self.name, error = %e, "Error waiting for server");
                }
                Err(_) => {
                    // Timeout - force kill
                    warn!(name = %self.name, "Graceful shutdown timed out, force killing");
                    let _ = child.kill().await;
                }
            }

            *child_guard = None;
        }
        Ok(())
    }

    /// Restart the wrapped server
    pub async fn restart(&self, reason: Option<&str>) -> Result<()> {
        let count = self.restart_count.fetch_add(1, Ordering::SeqCst) + 1;
        info!(
            name = %self.name,
            restart_count = count,
            reason = reason.unwrap_or("not specified"),
            "Restarting wrapped server"
        );

        self.kill().await?;

        // Brief pause to allow cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        self.spawn().await?;

        Ok(())
    }

    /// Get server status
    pub async fn status(&self) -> ServerStatus {
        let child_guard = self.child.lock().await;
        let running = child_guard.is_some();
        let pid = child_guard
            .as_ref()
            .and_then(|c| c.id());

        let start_time = self.start_time.lock().await;
        let uptime_secs = start_time.elapsed().as_secs();

        ServerStatus {
            running,
            pid,
            uptime_secs,
            restart_count: self.restart_count.load(Ordering::SeqCst),
            server_name: self.name.clone(),
        }
    }

}

/// Server status information
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    pub restart_count: u32,
    pub server_name: String,
}

