use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppResult;
use crate::config::TunnelConfig;

pub struct NftManager {
    names: NftNames,
    active_specs: Vec<TunnelConfig>,
    table_active: bool,
}

impl NftManager {
    pub fn new() -> Self {
        Self {
            names: NftNames::new(),
            active_specs: Vec::new(),
            table_active: false,
        }
    }

    pub async fn reconcile(&mut self, desired_tunnels: Vec<TunnelConfig>) -> bool {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = desired_tunnels;
            eprintln!("nft mode is only supported on Linux.");
            false
        }

        #[cfg(target_os = "linux")]
        {
            if self.table_active && self.active_specs == desired_tunnels {
                return true;
            }

            match NftRuleset::from_tunnels(&desired_tunnels, &self.names)
                .and_then(|ruleset| self.apply_ruleset(&ruleset))
            {
                Ok(()) => {
                    self.active_specs = desired_tunnels;
                    self.table_active = !self.active_specs.is_empty();
                    true
                }
                Err(error) => {
                    eprintln!("Failed to apply nft rules: {}", error);
                    false
                }
            }
        }
    }

    pub async fn stop_all(&mut self) {
        #[cfg(target_os = "linux")]
        if self.table_active
            && let Err(error) = delete_table(&self.names.table)
        {
            eprintln!(
                "Failed to delete nft table '{}': {}",
                self.names.table, error
            );
        }

        self.active_specs.clear();
        self.table_active = false;
    }

    #[cfg(target_os = "linux")]
    fn apply_ruleset(&self, ruleset: &NftRuleset) -> AppResult<()> {
        if ruleset.is_empty() {
            delete_table(&self.names.table)?;
            return Ok(());
        }

        let _ = delete_table(&self.names.table);
        run_nft_script(&ruleset.render_script())
    }
}

impl Drop for NftManager {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if self.table_active
            && let Err(error) = delete_table(&self.names.table)
        {
            eprintln!(
                "Failed to delete nft table '{}' during drop: {}",
                self.names.table, error
            );
        }
    }
}

#[derive(Clone)]
struct NftNames {
    table: String,
    prerouting_chain: String,
    output_chain: String,
    postrouting_chain: String,
    target_set: String,
}

impl NftNames {
    fn new() -> Self {
        let pid = std::process::id();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let suffix = format!("{pid}_{unique}");

        Self {
            table: format!("vorto_nat_{suffix}"),
            prerouting_chain: format!("vorto_pre_{suffix}"),
            output_chain: format!("vorto_out_{suffix}"),
            postrouting_chain: format!("vorto_post_{suffix}"),
            target_set: format!("vorto_targets_{suffix}"),
        }
    }
}

#[cfg(target_os = "linux")]
struct NftRuleset {
    names: NftNames,
    rules: Vec<NftRule>,
    masquerade_targets: Vec<Ipv4Addr>,
}

#[cfg(target_os = "linux")]
impl NftRuleset {
    fn from_tunnels(tunnels: &[TunnelConfig], names: &NftNames) -> AppResult<Self> {
        let mut rules = Vec::new();
        let mut targets = HashSet::new();

        for tunnel in tunnels {
            let listen = tunnel.listen.parse::<SocketAddr>()?;
            let target = tunnel.target.parse::<SocketAddr>()?;

            let SocketAddr::V4(listen_v4) = listen else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "nft mode only supports IPv4 listen addresses: {}",
                        tunnel.listen
                    ),
                )
                .into());
            };
            let SocketAddr::V4(target_v4) = target else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "nft mode only supports IPv4 target addresses: {}",
                        tunnel.target
                    ),
                )
                .into());
            };

            if listen_v4.ip().is_unspecified() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "nft mode requires a specific IPv4 listen address, not {}",
                        tunnel.listen
                    ),
                )
                .into());
            }

            if tunnel.protocol.supports_tcp() {
                rules.push(NftRule::Tcp {
                    listen_ip: *listen_v4.ip(),
                    listen_port: listen_v4.port(),
                    target_ip: *target_v4.ip(),
                    target_port: target_v4.port(),
                });
            }

            if tunnel.protocol.supports_udp() {
                rules.push(NftRule::Udp {
                    listen_ip: *listen_v4.ip(),
                    listen_port: listen_v4.port(),
                    target_ip: *target_v4.ip(),
                    target_port: target_v4.port(),
                });
            }

            if !target_v4.ip().is_loopback() {
                targets.insert(*target_v4.ip());
            }
        }

        let mut masquerade_targets = targets.into_iter().collect::<Vec<_>>();
        masquerade_targets.sort_unstable();

        Ok(Self {
            names: names.clone(),
            rules,
            masquerade_targets,
        })
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn render_script(&self) -> String {
        let mut script = String::new();
        writeln!(&mut script, "add table ip {}", self.names.table).unwrap();
        writeln!(
            &mut script,
            "add chain ip {} {} {{ type nat hook prerouting priority -100; }}",
            self.names.table, self.names.prerouting_chain
        )
        .unwrap();
        writeln!(
            &mut script,
            "add chain ip {} {} {{ type nat hook output priority -100; }}",
            self.names.table, self.names.output_chain
        )
        .unwrap();
        writeln!(
            &mut script,
            "add chain ip {} {} {{ type nat hook postrouting priority 100; }}",
            self.names.table, self.names.postrouting_chain
        )
        .unwrap();

        if !self.masquerade_targets.is_empty() {
            writeln!(
                &mut script,
                "add set ip {} {} {{ type ipv4_addr; flags interval; }}",
                self.names.table, self.names.target_set
            )
            .unwrap();

            for target in &self.masquerade_targets {
                writeln!(
                    &mut script,
                    "add element ip {} {} {{ {} }}",
                    self.names.table, self.names.target_set, target
                )
                .unwrap();
            }

            writeln!(
                &mut script,
                "add rule ip {} {} ip daddr @{} masquerade",
                self.names.table, self.names.postrouting_chain, self.names.target_set
            )
            .unwrap();
        }

        for rule in &self.rules {
            for rendered in rule.render(&self.names.table, &self.names.prerouting_chain) {
                writeln!(&mut script, "{rendered}").unwrap();
            }
            for rendered in rule.render(&self.names.table, &self.names.output_chain) {
                writeln!(&mut script, "{rendered}").unwrap();
            }
        }

        script
    }
}

#[cfg(target_os = "linux")]
enum NftRule {
    Tcp {
        listen_ip: Ipv4Addr,
        listen_port: u16,
        target_ip: Ipv4Addr,
        target_port: u16,
    },
    Udp {
        listen_ip: Ipv4Addr,
        listen_port: u16,
        target_ip: Ipv4Addr,
        target_port: u16,
    },
}

#[cfg(target_os = "linux")]
impl NftRule {
    fn render(&self, table: &str, chain: &str) -> Vec<String> {
        match self {
            Self::Tcp {
                listen_ip,
                listen_port,
                target_ip,
                target_port,
            } => vec![format!(
                "add rule ip {table} {chain} ip daddr {listen_ip} tcp dport {listen_port} dnat to {target_ip}:{target_port}"
            )],
            Self::Udp {
                listen_ip,
                listen_port,
                target_ip,
                target_port,
            } => vec![format!(
                "add rule ip {table} {chain} ip daddr {listen_ip} udp dport {listen_port} dnat to {target_ip}:{target_port}"
            )],
        }
    }
}

#[cfg(target_os = "linux")]
fn run_nft_script(script: &str) -> AppResult<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "nft apply failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into())
    }
}

#[cfg(target_os = "linux")]
fn delete_table(table: &str) -> AppResult<()> {
    let output = Command::new("nft")
        .args(["delete", "table", "ip", table])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such file or directory") {
        return Ok(());
    }

    Err(io::Error::other(format!("nft delete table failed: {}", stderr.trim())).into())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn nft_names_are_unique() {
        let first = NftNames::new();
        let second = NftNames::new();
        assert_ne!(first.table, second.table);
        assert_ne!(first.prerouting_chain, second.prerouting_chain);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn render_ruleset_contains_unique_table_and_rules() {
        let names = NftNames::new();
        let ruleset = NftRuleset {
            names: names.clone(),
            rules: vec![
                NftRule::Tcp {
                    listen_ip: Ipv4Addr::new(127, 0, 0, 1),
                    listen_port: 8080,
                    target_ip: Ipv4Addr::new(10, 0, 0, 2),
                    target_port: 80,
                },
                NftRule::Udp {
                    listen_ip: Ipv4Addr::new(127, 0, 0, 1),
                    listen_port: 5353,
                    target_ip: Ipv4Addr::new(1, 1, 1, 1),
                    target_port: 53,
                },
            ],
            masquerade_targets: vec![Ipv4Addr::new(10, 0, 0, 2)],
        };

        let script = ruleset.render_script();
        assert!(script.contains(&format!("add table ip {}", names.table)));
        assert!(script.contains("tcp dport 8080 dnat to 10.0.0.2:80"));
        assert!(script.contains("udp dport 5353 dnat to 1.1.1.1:53"));
        assert!(script.contains(&names.target_set));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_skips_delete_when_table_is_not_active() {
        let manager = NftManager::new();
        drop(manager);
    }
}
