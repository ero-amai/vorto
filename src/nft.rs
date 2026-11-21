use crate::{AppResult, config::TunnelConfig};

#[cfg(target_os = "linux")]
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    fs, io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use crate::{config::Protocol, runtime::ActiveTunnel};
#[cfg(target_os = "linux")]
use tokio::io::AsyncWriteExt;

pub struct NftManager {
    #[cfg(target_os = "linux")]
    names: NftNames,
    #[cfg(target_os = "linux")]
    active: Option<AppliedRuleset>,
    #[cfg(target_os = "linux")]
    proxies: HashMap<ProxyKey, LoopbackProxy>,
}

#[cfg(target_os = "linux")]
struct AppliedRuleset {
    specs: Vec<TunnelConfig>,
    ruleset: NftRuleset,
    snapshot: Option<String>,
}

#[cfg(target_os = "linux")]
type ProxyKey = (SocketAddrV4, Protocol);

#[cfg(target_os = "linux")]
struct LoopbackProxy {
    tunnel: ActiveTunnel,
    port: u16,
}

impl NftManager {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            names: NftNames::new(),
            #[cfg(target_os = "linux")]
            active: None,
            #[cfg(target_os = "linux")]
            proxies: HashMap::new(),
        }
    }

    pub async fn reconcile(&mut self, desired: &[TunnelConfig]) -> AppResult<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = desired;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "nft mode is only supported on Linux.",
            )
            .into())
        }

        #[cfg(target_os = "linux")]
        {
            if desired.is_empty() {
                return self.stop_all().await;
            }

            let mut ruleset = match &self.active {
                Some(active) if active.specs == desired => active.ruleset.clone(),
                _ => NftRuleset::from_tunnels(desired)?,
            };
            ruleset.update_local_addresses()?;

            if let Some(active) = &self.active
                && ruleset == active.ruleset
                && self
                    .proxies
                    .values()
                    .all(|proxy| !proxy.tunnel.has_finished())
                && active.snapshot.is_some()
                && list_table(&self.names.table)? == active.snapshot
            {
                return Ok(());
            }

            // Bind replacements before publishing their ports. On a failed batch the
            // old rules and their listeners remain usable.
            let mut replacements = HashMap::new();
            let mut ports = HashMap::new();
            for key in ruleset.rules.iter().filter_map(NftRule::proxy_key) {
                if ports.contains_key(&key) {
                    continue;
                }
                let proxy = match self.proxies.get(&key) {
                    Some(proxy) if !proxy.tunnel.has_finished() => proxy,
                    _ => {
                        let (target, protocol) = key;
                        let (tunnel, port) = ActiveTunnel::spawn_bound(TunnelConfig {
                            name: format!("nft loopback to {target}"),
                            listen: "127.0.0.1:0".to_owned(),
                            target: target.to_string(),
                            protocol,
                            enabled: true,
                        })
                        .await?;
                        replacements
                            .entry(key)
                            .or_insert(LoopbackProxy { tunnel, port })
                    }
                };
                ports.insert(key, proxy.port);
            }

            let script = ruleset.render_script(&self.names, &ports);
            let expected_rules = script
                .lines()
                .filter(|line| line.starts_with("add rule "))
                .count();
            run_nft_script(&script).await?;
            self.proxies.retain(|key, _| ports.contains_key(key));
            self.proxies.extend(replacements);
            let active = self.active.insert(AppliedRuleset {
                specs: desired.to_vec(),
                ruleset,
                snapshot: None,
            });
            let snapshot = list_table(&self.names.table)?.ok_or_else(|| {
                io::Error::other(format!(
                    "nft table '{}' disappeared after apply",
                    self.names.table
                ))
            })?;
            // In a table listing, only rules have handles at the second indentation level.
            let installed_rules = snapshot
                .lines()
                .filter(|line| line.starts_with("\t\t") && line.contains(" # handle "))
                .count();
            if installed_rules != expected_rules {
                return Err(io::Error::other(format!(
                    "nft table '{}' has {installed_rules} of {expected_rules} expected rules after apply",
                    self.names.table
                ))
                .into());
            }
            active.snapshot = Some(snapshot);
            Ok(())
        }
    }

    pub async fn recover_active(&mut self) -> AppResult<()> {
        #[cfg(target_os = "linux")]
        if let Some(active) = &self.active {
            let specs = active.specs.clone();
            return self.reconcile(&specs).await;
        }

        Ok(())
    }

    pub async fn stop_all(&mut self) -> AppResult<()> {
        #[cfg(target_os = "linux")]
        if self.active.is_some() {
            delete_table(&self.names.table)?;
            self.active = None;
            for (_, proxy) in self.proxies.drain() {
                proxy.tunnel.stop().await;
            }
        }

        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for NftManager {
    fn drop(&mut self) {
        if self.active.is_some()
            && let Err(error) = delete_table(&self.names.table)
        {
            eprintln!("Failed to delete nft table '{}': {error}", self.names.table);
        }
    }
}

#[cfg(target_os = "linux")]
struct NftNames {
    table: String,
    mark: u32,
}

#[cfg(target_os = "linux")]
impl NftNames {
    fn new() -> Self {
        let pid = std::process::id();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            table: format!("vorto_nat_{pid}_{unique}"),
            mark: 0x7600_0000 | (((pid as u128) ^ unique) as u32 & 0x00ff_ffff),
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, PartialEq, Eq)]
struct NftRuleset {
    rules: Vec<NftRule>,
    local_addresses: Option<BTreeSet<Ipv4Addr>>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
struct NftRule {
    listen: SocketAddrV4,
    target: SocketAddrV4,
    protocol: Protocol,
}

#[cfg(target_os = "linux")]
impl NftRule {
    fn proxy_key(&self) -> Option<ProxyKey> {
        (!self.target.ip().is_loopback()).then_some((self.target, self.protocol))
    }

    fn listen_match(&self) -> String {
        let protocol = self.protocol.label();
        let port = self.listen.port();
        if self.listen.ip().is_unspecified() {
            format!("{protocol} dport {port}")
        } else {
            format!("ip daddr {} {protocol} dport {port}", self.listen.ip())
        }
    }
}

#[cfg(target_os = "linux")]
impl NftRuleset {
    fn from_tunnels(tunnels: &[TunnelConfig]) -> AppResult<Self> {
        let mut rules = Vec::new();
        for tunnel in tunnels {
            let listen = tunnel.listen.parse::<SocketAddrV4>()?;
            let target = resolve_ipv4_target(&tunnel.target)?;
            if target.ip().is_loopback() && !listen.ip().is_loopback() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "nft mode can only target loopback addresses from explicit loopback listen addresses: {listen} -> {target}"
                    ),
                )
                .into());
            }
            for protocol in [Protocol::Tcp, Protocol::Udp] {
                if tunnel.protocol == protocol || tunnel.protocol == Protocol::Both {
                    rules.push(NftRule {
                        listen,
                        target,
                        protocol,
                    });
                }
            }
        }

        Ok(Self {
            rules,
            local_addresses: None,
        })
    }

    fn update_local_addresses(&mut self) -> AppResult<()> {
        let addresses = interface_ipv4_addresses()?;
        let needs_forwarding = self
            .rules
            .iter()
            .find(|rule| !rule.listen.ip().is_loopback() && !addresses.contains(rule.target.ip()));
        if let Some(rule) = needs_forwarding
            && !ipv4_forwarding_enabled()?
        {
            return Err(io::Error::other(format!(
                "nft forwarding {} -> {} requires net.ipv4.ip_forward=1 or IPv4 forwarding on the ingress interface",
                rule.listen, rule.target
            ))
            .into());
        }
        self.local_addresses = self
            .rules
            .iter()
            .any(|rule| rule.listen.ip().is_unspecified())
            .then(|| {
                addresses
                    .into_iter()
                    .filter(|address| !address.is_loopback())
                    .collect()
            });
        Ok(())
    }

    fn render_script(&self, names: &NftNames, proxy_ports: &HashMap<ProxyKey, u16>) -> String {
        let table = &names.table;
        let mark = names.mark;
        // The first add makes the batch work even if the table was externally removed.
        let mut script =
            format!("add table ip {table}\ndelete table ip {table}\nadd table ip {table}\n");
        for (chain, priority) in [("prerouting", -100), ("output", -100), ("postrouting", 100)] {
            writeln!(
                script,
                "add chain ip {table} {chain} {{ type nat hook {chain} priority {priority}; }}"
            )
            .unwrap();
        }

        if let Some(addresses) = &self.local_addresses {
            writeln!(
                script,
                "add set ip {table} local_ipv4 {{ type ipv4_addr; flags interval; }}"
            )
            .unwrap();
            writeln!(
                script,
                "add element ip {table} local_ipv4 {{ 127.0.0.0/8 }}"
            )
            .unwrap();
            for address in addresses {
                writeln!(script, "add element ip {table} local_ipv4 {{ {address} }}").unwrap();
            }
            for (hook, chain, guard) in [
                (
                    "prerouting",
                    "wildcard_prerouting",
                    "ip daddr != 127.0.0.0/8 ",
                ),
                ("output", "wildcard_output", ""),
            ] {
                writeln!(script, "add chain ip {table} {chain}").unwrap();
                writeln!(
                    script,
                    "add rule ip {table} {hook} {guard}ip daddr @local_ipv4 jump {chain}"
                )
                .unwrap();
                writeln!(
                    script,
                    "add rule ip {table} {hook} {guard}fib daddr type local jump {chain}"
                )
                .unwrap();
            }
        }

        let targets = self
            .rules
            .iter()
            .filter_map(|rule| (!rule.target.ip().is_loopback()).then_some(*rule.target.ip()))
            .collect::<BTreeSet<_>>();
        if !targets.is_empty() {
            writeln!(script, "add set ip {table} targets {{ type ipv4_addr; }}").unwrap();
            for target in targets {
                writeln!(script, "add element ip {table} targets {{ {target} }}").unwrap();
            }
            writeln!(
                script,
                "add rule ip {table} postrouting meta mark 0x{mark:x} ip daddr @targets masquerade"
            )
            .unwrap();
        }

        for rule in &self.rules {
            let matches = rule.listen_match();
            let target = rule.target;
            let (prerouting, output) = if rule.listen.ip().is_unspecified() {
                ("wildcard_prerouting", "wildcard_output")
            } else {
                ("prerouting", "output")
            };
            if !rule.listen.ip().is_loopback() {
                writeln!(script, "add rule ip {table} {prerouting} {matches} meta mark set 0x{mark:x} dnat to {target}").unwrap();
            }
            if let Some(key) = rule.proxy_key() {
                let port = proxy_ports[&key];
                // DNAT alone cannot route a locally sourced 127/8 packet off host.
                writeln!(script, "add rule ip {table} {output} ip saddr 127.0.0.0/8 {matches} redirect to :{port}").unwrap();
            }
            writeln!(
                script,
                "add rule ip {table} {output} {matches} meta mark set 0x{mark:x} dnat to {target}"
            )
            .unwrap();
        }

        script
    }
}

#[cfg(target_os = "linux")]
fn interface_ipv4_addresses() -> io::Result<BTreeSet<Ipv4Addr>> {
    let mut interfaces = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut addresses = BTreeSet::new();
    // getifaddrs owns the list and its sockaddr entries until freeifaddrs.
    unsafe {
        let mut current = interfaces;
        while let Some(interface) = current.as_ref() {
            if !interface.ifa_addr.is_null()
                && (*interface.ifa_addr).sa_family as libc::c_int == libc::AF_INET
            {
                let addr = &*interface.ifa_addr.cast::<libc::sockaddr_in>();
                addresses.insert(Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()));
            }
            current = interface.ifa_next;
        }
        libc::freeifaddrs(interfaces);
    }
    Ok(addresses)
}

#[cfg(target_os = "linux")]
fn ipv4_forwarding_enabled() -> io::Result<bool> {
    let sysctl = Path::new("/proc/sys/net/ipv4");
    if fs::read_to_string(sysctl.join("ip_forward"))?.trim() == "1" {
        return Ok(true);
    }

    for entry in fs::read_dir(sysctl.join("conf"))? {
        let entry = entry?;
        if ["all", "default", "lo"]
            .iter()
            .any(|name| entry.file_name() == *name)
        {
            continue;
        }
        match fs::read_to_string(entry.path().join("forwarding")) {
            Ok(value) if value.trim() == "1" => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn resolve_ipv4_target(value: &str) -> AppResult<SocketAddrV4> {
    if let Ok(SocketAddr::V4(addr)) = value.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if value.parse::<SocketAddr>().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("nft mode only supports IPv4 target addresses: {value}"),
        )
        .into());
    }

    value
        .to_socket_addrs()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Failed to resolve nft target address {value}: {error}"),
            )
        })?
        .find_map(|addr| match addr {
            SocketAddr::V4(addr) => Some(addr),
            SocketAddr::V6(_) => None,
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("nft mode target must resolve to an IPv4 address: {value}"),
            )
            .into()
        })
}

#[cfg(target_os = "linux")]
async fn run_nft_script(script: &str) -> AppResult<()> {
    let mut child = tokio::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("Could not run nft: {error}")))?;
    let mut stdin = child.stdin.take().expect("nft stdin is piped");
    let (write_result, output) = tokio::join!(
        async move { stdin.write_all(script.as_bytes()).await },
        child.wait_with_output()
    );
    let output = output?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "nft apply failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    write_result?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn list_table(table: &str) -> AppResult<Option<String>> {
    table_command("list", table)
}

#[cfg(target_os = "linux")]
fn delete_table(table: &str) -> AppResult<()> {
    table_command("delete", table)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn table_command(action: &str, table: &str) -> AppResult<Option<String>> {
    let output = Command::new("nft")
        .env("LC_ALL", "C")
        .args(["-a", "-s", "-n", action, "table", "ip", table])
        .output()
        .map_err(|error| io::Error::new(error.kind(), format!("Could not run nft: {error}")))?;

    if output.status.success() {
        return Ok(Some(String::from_utf8(output.stdout)?));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such file or directory") {
        return Ok(None);
    }

    Err(io::Error::other(format!(
        "nft {action} table {table} failed: {}",
        stderr.trim()
    ))
    .into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn wildcard_rules_match_interface_addresses_and_limit_dnat_to_local_destinations() {
        let names = NftNames::new();
        let target: SocketAddrV4 = "192.0.2.50:9000".parse().unwrap();
        let mut ruleset = NftRuleset::from_tunnels(&[TunnelConfig {
            name: "wildcard".to_owned(),
            listen: "0.0.0.0:8000".to_owned(),
            target: target.to_string(),
            protocol: Protocol::Both,
            enabled: true,
        }])
        .unwrap();
        ruleset.local_addresses = Some([Ipv4Addr::new(10, 0, 0, 2)].into());
        let ports = HashMap::from([
            ((target, Protocol::Tcp), 20000),
            ((target, Protocol::Udp), 20001),
        ]);
        let script = ruleset.render_script(&names, &ports);
        let table = &names.table;

        assert!(script.starts_with(&format!(
            "add table ip {table}\ndelete table ip {table}\nadd table ip {table}\n"
        )));
        assert!(script.contains("local_ipv4 { 10.0.0.2 }"));
        assert!(script.contains(
            "prerouting ip daddr != 127.0.0.0/8 ip daddr @local_ipv4 jump wildcard_prerouting"
        ));
        assert!(script.contains("fib daddr type local jump wildcard_prerouting"));
        let redirect = script
            .find("wildcard_output ip saddr 127.0.0.0/8 tcp dport 8000 redirect to :20000")
            .unwrap();
        let dnat = script
            .find("wildcard_output tcp dport 8000 meta mark")
            .unwrap();
        assert!(redirect < dnat);
        assert!(
            script
                .contains("wildcard_output ip saddr 127.0.0.0/8 udp dport 8000 redirect to :20001")
        );
        assert!(script.contains(&format!(
            "postrouting meta mark 0x{:x} ip daddr @targets masquerade",
            names.mark
        )));
    }

    #[test]
    fn loopback_to_loopback_keeps_kernel_dnat() {
        let names = NftNames::new();
        let ruleset = NftRuleset::from_tunnels(&[TunnelConfig {
            name: "local".to_owned(),
            listen: "127.0.0.1:8000".to_owned(),
            target: "127.0.0.1:9000".to_owned(),
            protocol: Protocol::Tcp,
            enabled: true,
        }])
        .unwrap();

        let script = ruleset.render_script(&names, &HashMap::new());
        assert!(script.contains("output ip daddr 127.0.0.1 tcp dport 8000"));
        assert!(script.contains("dnat to 127.0.0.1:9000"));
        assert!(!script.contains("redirect to"));
        assert!(!script.contains("prerouting ip daddr 127.0.0.1"));
    }
}
