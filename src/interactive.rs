use std::io::{self, Write};
use std::path::Path;

use crate::AppResult;
use crate::config::{AppConfig, Protocol, TcpMode, TunnelConfig};

pub fn manage_config(path: &Path) -> AppResult<()> {
    let mut config = AppConfig::load_or_default(path)?;
    let mut dirty = false;

    loop {
        render_dashboard(path, &config, dirty);

        match prompt("Action [a/e/t/d/s/q]")?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "a" | "add" => dirty |= add_tunnel(&mut config)?,
            "e" | "edit" => dirty |= edit_tunnel(&mut config)?,
            "t" | "toggle" => dirty |= toggle_tunnel(&mut config)?,
            "d" | "delete" => dirty |= delete_tunnel(&mut config)?,
            "s" | "save" => {
                config.save(path)?;
                println!("Saved configuration to {}.", path.display());
                return Ok(());
            }
            "q" | "quit" => {
                if dirty && !prompt_bool("Discard unsaved changes? [y/N]", false)? {
                    println!("Nothing was discarded.");
                    continue;
                }
                println!("Exited configuration editor.");
                return Ok(());
            }
            "" => {}
            _ => println!("Unknown action. Use a, e, t, d, s, or q."),
        }
    }
}

fn render_dashboard(path: &Path, config: &AppConfig, dirty: bool) {
    println!();
    println!("Config editor");
    println!("File: {}", path.display());
    println!(
        "Tunnels: {} total, {} enabled, {} disabled{}",
        config.tunnels.len(),
        config
            .tunnels
            .iter()
            .filter(|tunnel| tunnel.enabled)
            .count(),
        config
            .tunnels
            .iter()
            .filter(|tunnel| !tunnel.enabled)
            .count(),
        if dirty { " | unsaved changes" } else { "" }
    );
    println!();
    println!(
        "{:<4} {:<18} {:<8} {:<12} {:<10} {:<24} Local listen",
        "No.", "Name", "Proto", "TCP mode", "State", "Remote target",
    );
    println!("{}", "-".repeat(101));

    if config.tunnels.is_empty() {
        println!("(no tunnels configured)");
    } else {
        for (index, tunnel) in config.tunnels.iter().enumerate() {
            println!(
                "{:<4} {:<18} {:<8} {:<12} {:<10} {:<24} {}",
                index + 1,
                truncate(&tunnel.name, 18),
                tunnel.protocol.label(),
                truncate(tcp_mode_display(tunnel), 12),
                if tunnel.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                truncate(&tunnel.target, 24),
                tunnel.listen
            );
        }
    }

    println!();
    println!("Actions:");
    println!("  a = add tunnel");
    println!("  e = edit tunnel");
    println!("  t = toggle enabled/disabled");
    println!("  d = delete tunnel");
    println!("  s = save and exit");
    println!("  q = quit");
    println!();
}

fn add_tunnel(config: &mut AppConfig) -> AppResult<bool> {
    println!("Add tunnel");
    println!("Press Ctrl+C to abort at any time.");

    let name = prompt_new_name(config, None)?;
    let protocol = prompt_protocol(None)?;
    let tcp_mode = prompt_tcp_mode(protocol, None)?;
    let target = prompt_socket_addr("Remote target address (host:port)", None)?;
    let listen = prompt_socket_addr("Local listen address (host:port)", None)?;
    let enabled = prompt_bool("Enable this tunnel now? [Y/n]", true)?;

    let tunnel = TunnelConfig {
        name,
        protocol,
        tcp_mode,
        target,
        listen,
        enabled,
    };
    tunnel.validate()?;

    println!();
    println!("New tunnel summary:");
    print_tunnel_details(&tunnel);
    if !prompt_bool("Create this tunnel? [Y/n]", true)? {
        println!("Creation cancelled.");
        return Ok(false);
    }

    config.tunnels.push(tunnel);
    println!("Tunnel added.");
    Ok(true)
}

fn edit_tunnel(config: &mut AppConfig) -> AppResult<bool> {
    let Some(index) = select_tunnel(config, "edit")? else {
        return Ok(false);
    };

    let mut tunnel = config.tunnels[index].clone();
    println!();
    println!("Editing '{}':", tunnel.name);
    print_tunnel_details(&tunnel);

    tunnel.name = prompt_new_name(config, Some(index))?;
    tunnel.protocol = prompt_protocol(Some(tunnel.protocol))?;
    tunnel.tcp_mode = prompt_tcp_mode(tunnel.protocol, Some(tunnel.tcp_mode))?;
    tunnel.target = prompt_socket_addr("Remote target address", Some(tunnel.target.as_str()))?;
    tunnel.listen = prompt_socket_addr("Local listen address", Some(tunnel.listen.as_str()))?;
    tunnel.enabled = prompt_bool(
        if tunnel.enabled {
            "Enable this tunnel? [Y/n]"
        } else {
            "Enable this tunnel? [y/N]"
        },
        tunnel.enabled,
    )?;
    tunnel.validate()?;

    println!();
    println!("Updated tunnel summary:");
    print_tunnel_details(&tunnel);
    if !prompt_bool("Apply these changes? [Y/n]", true)? {
        println!("Edit cancelled.");
        return Ok(false);
    }

    config.tunnels[index] = tunnel;
    println!("Tunnel updated.");
    Ok(true)
}

fn toggle_tunnel(config: &mut AppConfig) -> AppResult<bool> {
    let Some(index) = select_tunnel(config, "toggle")? else {
        return Ok(false);
    };

    let tunnel = &mut config.tunnels[index];
    tunnel.enabled = !tunnel.enabled;
    println!(
        "Tunnel '{}' is now {}.",
        tunnel.name,
        if tunnel.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    Ok(true)
}

fn delete_tunnel(config: &mut AppConfig) -> AppResult<bool> {
    let Some(index) = select_tunnel(config, "delete")? else {
        return Ok(false);
    };

    let tunnel = &config.tunnels[index];
    println!();
    println!("Delete tunnel:");
    print_tunnel_details(tunnel);

    if !prompt_bool(&format!("Delete '{}' ? [y/N]", tunnel.name), false)? {
        println!("Delete cancelled.");
        return Ok(false);
    }

    config.tunnels.remove(index);
    println!("Tunnel deleted.");
    Ok(true)
}

fn select_tunnel(config: &AppConfig, action: &str) -> AppResult<Option<usize>> {
    if config.tunnels.is_empty() {
        println!("There are no tunnels to {}.", action);
        return Ok(None);
    }

    loop {
        let input = prompt("Tunnel number or name (blank to cancel)")?;
        let value = input.trim();
        if value.is_empty() {
            println!("Selection cancelled.");
            return Ok(None);
        }

        if let Ok(number) = value.parse::<usize>()
            && (1..=config.tunnels.len()).contains(&number)
        {
            return Ok(Some(number - 1));
        }

        if let Some((index, _)) = config
            .tunnels
            .iter()
            .enumerate()
            .find(|(_, tunnel)| tunnel.name.eq_ignore_ascii_case(value))
        {
            return Ok(Some(index));
        }

        println!("No tunnel matched '{}'.", value);
    }
}

fn prompt_new_name(config: &AppConfig, current_index: Option<usize>) -> AppResult<String> {
    let current_name = current_index.map(|index| config.tunnels[index].name.as_str());

    loop {
        let input = prompt_optional("Name", current_name)?;
        let value = input.trim();

        let name = if value.is_empty() {
            if let Some(current) = current_name {
                current.to_string()
            } else {
                println!("Name cannot be empty.");
                continue;
            }
        } else {
            value.to_string()
        };

        let duplicate = config.tunnels.iter().enumerate().any(|(index, tunnel)| {
            Some(index) != current_index && tunnel.name.eq_ignore_ascii_case(&name)
        });
        if duplicate {
            println!("That name is already in use.");
            continue;
        }

        return Ok(name);
    }
}

fn prompt_protocol(current: Option<Protocol>) -> AppResult<Protocol> {
    loop {
        let default_label = current.map_or("none", Protocol::label);
        println!("Protocol options: tcp, udp, both");
        let input = prompt(&format!("Protocol [default: {}]", default_label))?;
        let value = input.trim().to_ascii_lowercase();

        let selected = if value.is_empty() {
            current
        } else {
            match value.as_str() {
                "1" | "tcp" => Some(Protocol::Tcp),
                "2" | "udp" => Some(Protocol::Udp),
                "3" | "both" => Some(Protocol::Both),
                _ => {
                    println!("Invalid protocol. Use tcp, udp, or both.");
                    continue;
                }
            }
        };

        if let Some(protocol) = selected {
            return Ok(protocol);
        }
    }
}

fn prompt_tcp_mode(protocol: Protocol, current: Option<TcpMode>) -> AppResult<TcpMode> {
    if !protocol.supports_tcp() {
        println!("TCP forwarding mode is not used for UDP-only tunnels.");
        return Ok(TcpMode::Auto);
    }

    loop {
        let default_label = current.unwrap_or(TcpMode::Auto).label();
        println!("TCP mode options:");
        println!("  auto        = current safe default, optimized for throughput");
        println!("  throughput  = highest bulk transfer throughput");
        println!("  latency     = better fit for many small packets and interactive traffic");
        let input = prompt(&format!("TCP forwarding mode [default: {}]", default_label))?;
        let value = input.trim().to_ascii_lowercase();

        let selected = if value.is_empty() {
            current.or(Some(TcpMode::Auto))
        } else {
            match value.as_str() {
                "1" | "auto" => Some(TcpMode::Auto),
                "2" | "throughput" | "bulk" => Some(TcpMode::Throughput),
                "3" | "latency" | "small" => Some(TcpMode::Latency),
                _ => {
                    println!("Invalid TCP mode. Use auto, throughput, or latency.");
                    continue;
                }
            }
        };

        if let Some(tcp_mode) = selected {
            return Ok(tcp_mode);
        }
    }
}

fn prompt_socket_addr(label: &str, current: Option<&str>) -> AppResult<String> {
    loop {
        let input = prompt_optional(label, current)?;
        let value = input.trim();
        if value.is_empty() {
            if let Some(existing) = current {
                return Ok(existing.to_string());
            }
            println!("Address cannot be empty.");
            continue;
        }

        match value.parse::<std::net::SocketAddr>() {
            Ok(_) => return Ok(value.to_string()),
            Err(error) => println!("Invalid address format: {}", error),
        }
    }
}

fn prompt_bool(label: &str, default: bool) -> AppResult<bool> {
    loop {
        let input = prompt(label)?;
        let value = input.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Ok(default);
        }

        match value.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please enter y or n."),
        }
    }
}

fn print_tunnel_details(tunnel: &TunnelConfig) {
    println!("  Name         : {}", tunnel.name);
    println!("  Protocol     : {}", tunnel.protocol.label());
    println!("  TCP mode     : {}", tcp_mode_description(tunnel));
    println!("  Remote target: {}", tunnel.target);
    println!("  Local listen : {}", tunnel.listen);
    println!(
        "  State        : {}",
        if tunnel.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
}

fn truncate(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let shortened = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 3 {
        format!(
            "{}...",
            shortened.chars().take(width - 3).collect::<String>()
        )
    } else {
        shortened
    }
}

fn prompt_optional(label: &str, current: Option<&str>) -> AppResult<String> {
    let suffix = current
        .filter(|value| !value.is_empty())
        .map(|value| format!(" [default: {}]", value))
        .unwrap_or_default();
    prompt(&format!("{}{}", label, suffix))
}

fn tcp_mode_display(tunnel: &TunnelConfig) -> &'static str {
    if tunnel.protocol.supports_tcp() {
        tunnel.tcp_mode.label()
    } else {
        "-"
    }
}

fn tcp_mode_description(tunnel: &TunnelConfig) -> String {
    if !tunnel.protocol.supports_tcp() {
        return "not used for UDP-only tunnels".to_string();
    }

    match tunnel.tcp_mode {
        TcpMode::Auto => "auto (currently uses throughput mode)".to_string(),
        explicit => explicit.label().to_string(),
    }
}

fn prompt(label: &str) -> AppResult<String> {
    println!("{}:", label);
    print!("└─ ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim_end().to_string())
}
