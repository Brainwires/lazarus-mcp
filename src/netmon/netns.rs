//! Network Namespace Support
//!
//! Provides network isolation via Linux network namespaces.
//! This module requires root privileges to create and manage namespaces.

use anyhow::{anyhow, Context, Result};
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

/// Network namespace configuration
#[derive(Debug, Clone)]
pub struct NetworkNamespace {
    /// Name of the namespace (e.g., "aegis-12345")
    pub name: String,
    /// Host-side veth interface name
    pub veth_host: String,
    /// Agent-side veth interface name (inside namespace)
    pub veth_agent: String,
    /// Host-side IP address
    pub host_ip: String,
    /// Agent-side IP address
    pub agent_ip: String,
    /// Whether the namespace is currently active
    pub active: bool,
}

impl NetworkNamespace {
    /// Create a new network namespace for the given PID
    pub fn create(pid: u32) -> Result<Self> {
        // Verify we're running as root
        if !nix::unistd::Uid::effective().is_root() {
            return Err(anyhow!(
                "Network namespace mode requires root privileges. Run with sudo or use --netmon=preload instead."
            ));
        }

        let name = format!("aegis-{}", pid);
        let veth_host = format!("veth-aegis-{}", pid);
        let veth_agent = format!("veth-agent-{}", pid);
        // Use 10.200.x.x range to avoid conflicts
        let subnet_id = (pid % 250) + 1; // 1-250
        let host_ip = format!("10.200.{}.1", subnet_id);
        let agent_ip = format!("10.200.{}.2", subnet_id);

        let mut ns = Self {
            name,
            veth_host,
            veth_agent,
            host_ip,
            agent_ip,
            active: false,
        };

        ns.setup()?;
        ns.active = true;

        Ok(ns)
    }

    /// Set up the network namespace and veth pair
    fn setup(&self) -> Result<()> {
        info!("Creating network namespace: {}", self.name);

        // Create the network namespace
        run_cmd("ip", &["netns", "add", &self.name])
            .context("Failed to create network namespace")?;

        // Create veth pair
        run_cmd(
            "ip",
            &[
                "link",
                "add",
                &self.veth_host,
                "type",
                "veth",
                "peer",
                "name",
                &self.veth_agent,
            ],
        )
        .context("Failed to create veth pair")?;

        // Move agent-side veth into namespace
        run_cmd(
            "ip",
            &[
                "link",
                "set",
                &self.veth_agent,
                "netns",
                &self.name,
            ],
        )
        .context("Failed to move veth to namespace")?;

        // Configure host-side veth
        run_cmd(
            "ip",
            &[
                "addr",
                "add",
                &format!("{}/24", self.host_ip),
                "dev",
                &self.veth_host,
            ],
        )
        .context("Failed to configure host veth IP")?;

        run_cmd("ip", &["link", "set", &self.veth_host, "up"])
            .context("Failed to bring up host veth")?;

        // Configure agent-side veth (inside namespace)
        run_cmd_in_netns(
            &self.name,
            "ip",
            &[
                "addr",
                "add",
                &format!("{}/24", self.agent_ip),
                "dev",
                &self.veth_agent,
            ],
        )
        .context("Failed to configure agent veth IP")?;

        run_cmd_in_netns(&self.name, "ip", &["link", "set", &self.veth_agent, "up"])
            .context("Failed to bring up agent veth")?;

        // Bring up loopback in namespace
        run_cmd_in_netns(&self.name, "ip", &["link", "set", "lo", "up"])
            .context("Failed to bring up loopback")?;

        // Set default route in namespace to go through host veth
        run_cmd_in_netns(
            &self.name,
            "ip",
            &["route", "add", "default", "via", &self.host_ip],
        )
        .context("Failed to set default route in namespace")?;

        info!("Network namespace {} created successfully", self.name);
        Ok(())
    }

    /// Set up NAT/masquerading for the namespace
    /// This allows the agent to access the internet through the host
    pub fn setup_nat(&self) -> Result<()> {
        info!("Setting up NAT for namespace {}", self.name);

        // Enable IP forwarding
        std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
            .context("Failed to enable IP forwarding")?;

        // Add iptables masquerade rule
        let subnet = format!("10.200.{}.0/24", self.subnet_id());

        // Check if rule already exists
        let check = Command::new("iptables")
            .args(["-t", "nat", "-C", "POSTROUTING", "-s", &subnet, "-j", "MASQUERADE"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if check.map(|s| !s.success()).unwrap_or(true) {
            run_cmd(
                "iptables",
                &["-t", "nat", "-A", "POSTROUTING", "-s", &subnet, "-j", "MASQUERADE"],
            )
            .context("Failed to add NAT masquerade rule")?;
        }

        // Allow forwarding for this subnet
        let check_fwd = Command::new("iptables")
            .args(["-C", "FORWARD", "-s", &subnet, "-j", "ACCEPT"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if check_fwd.map(|s| !s.success()).unwrap_or(true) {
            run_cmd(
                "iptables",
                &["-A", "FORWARD", "-s", &subnet, "-j", "ACCEPT"],
            )
            .context("Failed to add forward rule for subnet")?;

            run_cmd(
                "iptables",
                &["-A", "FORWARD", "-d", &subnet, "-j", "ACCEPT"],
            )
            .context("Failed to add forward rule to subnet")?;
        }

        info!("NAT configured for {}", self.name);
        Ok(())
    }

    /// Get the subnet ID for this namespace
    fn subnet_id(&self) -> u32 {
        self.agent_ip
            .split('.')
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    /// Run a command inside this network namespace
    pub fn run_in_namespace(&self, program: &str, args: &[&str]) -> Result<std::process::Output> {
        run_cmd_in_netns_output(&self.name, program, args)
    }

    /// Get the command prefix to run a process in this namespace
    pub fn namespace_exec_args(&self) -> Vec<String> {
        vec![
            "ip".to_string(),
            "netns".to_string(),
            "exec".to_string(),
            self.name.clone(),
        ]
    }

    /// Clean up the network namespace
    pub fn cleanup(&self) -> Result<()> {
        if !self.active {
            return Ok(());
        }

        info!("Cleaning up network namespace: {}", self.name);

        // Remove NAT rules (ignore errors, they may not exist)
        let subnet = format!("10.200.{}.0/24", self.subnet_id());
        let _ = Command::new("iptables")
            .args(["-t", "nat", "-D", "POSTROUTING", "-s", &subnet, "-j", "MASQUERADE"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let _ = Command::new("iptables")
            .args(["-D", "FORWARD", "-s", &subnet, "-j", "ACCEPT"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let _ = Command::new("iptables")
            .args(["-D", "FORWARD", "-d", &subnet, "-j", "ACCEPT"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        // Delete veth pair (deleting one side deletes both)
        let _ = Command::new("ip")
            .args(["link", "delete", &self.veth_host])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        // Delete network namespace
        let _ = run_cmd("ip", &["netns", "delete", &self.name]);

        info!("Network namespace {} cleaned up", self.name);
        Ok(())
    }

    /// Check if the namespace exists
    pub fn exists(&self) -> bool {
        let output = Command::new("ip")
            .args(["netns", "list"])
            .output()
            .ok();

        output
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&self.name))
            .unwrap_or(false)
    }

    /// Get network namespace info
    pub fn info(&self) -> NamespaceInfo {
        NamespaceInfo {
            name: self.name.clone(),
            host_interface: self.veth_host.clone(),
            agent_interface: self.veth_agent.clone(),
            host_ip: self.host_ip.clone(),
            agent_ip: self.agent_ip.clone(),
            active: self.active && self.exists(),
        }
    }
}

impl Drop for NetworkNamespace {
    fn drop(&mut self) {
        if self.active {
            if let Err(e) = self.cleanup() {
                warn!("Failed to cleanup namespace {}: {}", self.name, e);
            }
        }
    }
}

/// Information about a network namespace
#[derive(Debug, Clone, serde::Serialize)]
pub struct NamespaceInfo {
    pub name: String,
    pub host_interface: String,
    pub agent_interface: String,
    pub host_ip: String,
    pub agent_ip: String,
    pub active: bool,
}

/// Run a command and check for success
fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    debug!("Running: {} {:?}", program, args);
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Failed to execute {}", program))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{} failed: {}",
            program,
            stderr.trim()
        ));
    }

    Ok(())
}

/// Run a command inside a network namespace
fn run_cmd_in_netns(netns: &str, program: &str, args: &[&str]) -> Result<()> {
    debug!("Running in netns {}: {} {:?}", netns, program, args);

    let mut full_args = vec!["netns", "exec", netns, program];
    full_args.extend(args);

    let output = Command::new("ip")
        .args(&full_args)
        .output()
        .with_context(|| format!("Failed to execute {} in namespace {}", program, netns))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{} in namespace {} failed: {}",
            program,
            netns,
            stderr.trim()
        ));
    }

    Ok(())
}

/// Run a command inside a network namespace and return output
fn run_cmd_in_netns_output(netns: &str, program: &str, args: &[&str]) -> Result<std::process::Output> {
    debug!("Running in netns {}: {} {:?}", netns, program, args);

    let mut full_args = vec!["netns", "exec", netns, program];
    full_args.extend(args);

    Command::new("ip")
        .args(&full_args)
        .output()
        .with_context(|| format!("Failed to execute {} in namespace {}", program, netns))
}

/// List all aegis network namespaces
pub fn list_namespaces() -> Result<Vec<String>> {
    let output = Command::new("ip")
        .args(["netns", "list"])
        .output()
        .context("Failed to list network namespaces")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let namespaces: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.starts_with("aegis-") {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect();

    Ok(namespaces)
}

/// Clean up all aegis network namespaces (for recovery/cleanup)
pub fn cleanup_all() -> Result<usize> {
    let namespaces = list_namespaces()?;
    let count = namespaces.len();

    for ns_name in namespaces {
        info!("Cleaning up stale namespace: {}", ns_name);

        // Extract the PID/ID from the namespace name for veth cleanup
        if let Some(id) = ns_name.strip_prefix("aegis-") {
            let veth_host = format!("veth-aegis-{}", id);
            let _ = Command::new("ip")
                .args(["link", "delete", &veth_host])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        // Delete the namespace
        let _ = Command::new("ip")
            .args(["netns", "delete", &ns_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_namespaces() {
        // This test just verifies the function doesn't panic
        // It may return an empty list if not running as root
        let result = list_namespaces();
        // Don't assert success - may fail if ip command isn't available
        if let Ok(ns) = result {
            // All returned namespaces should start with "aegis-"
            for name in ns {
                assert!(name.starts_with("aegis-"));
            }
        }
    }
}
