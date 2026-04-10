use std::io::{self, Write};
use std::path::Path;

use crate::AppResult;
use crate::config::{AppConfig, Protocol, TunnelConfig};

pub fn manage_config(path: &Path) -> AppResult<()> {
    let mut config = AppConfig::load_or_default(path)?;

    loop {
        render_tunnels(&config);
        println!();
        println!("Select an action:");
        println!("1. Add tunnel");
        println!("2. Edit tunnel");
        println!("3. Delete tunnel");
        println!("4. Save and exit");
        println!("5. Exit without saving");

        match prompt("Choice")?.trim() {
            "1" => add_tunnel(&mut config)?,
            "2" => edit_tunnel(&mut config)?,
            "3" => delete_tunnel(&mut config)?,
            "4" => {
                config.save(path)?;
                println!("Saved configuration to {}.", path.display());
                return Ok(());
            }
            "5" => {
                println!("Exited without saving.");
                return Ok(());
            }
            _ => println!("Invalid choice. Please try again."),
        }
    }
}

fn render_tunnels(config: &AppConfig) {
    println!("Configured tunnels:");
    if config.tunnels.is_empty() {
        println!("  (none)");
        return;
    }

    for (index, tunnel) in config.tunnels.iter().enumerate() {
        println!(
            "  {}. {} | {} | {} -> {} | {}",
            index + 1,
            tunnel.name,
            tunnel.protocol.label(),
            tunnel.listen,
            tunnel.target,
            if tunnel.enabled { "enabled" } else { "disabled" }
        );
    }
}

fn add_tunnel(config: &mut AppConfig) -> AppResult<()> {
    println!("Add a new tunnel:");
    let name = loop {
        let input = prompt("Name")?;
        let name = input.trim();
        if name.is_empty() {
            println!("Name cannot be empty.");
            continue;
        }
        if config.tunnels.iter().any(|tunnel| tunnel.name == name) {
            println!("That name already exists. Please choose another.");
            continue;
        }
        break name.to_string();
    };

    let protocol = prompt_protocol(None)?;
    let listen = prompt_socket_addr("Listen address (for example 0.0.0.0:8080)", None)?;
    let target = prompt_socket_addr("Target address (for example 127.0.0.1:80)", None)?;
    let enabled = prompt_bool("Enable this tunnel? [Y/n]", true)?;

    let tunnel = TunnelConfig {
        name,
        listen,
        target,
        protocol,
        enabled,
    };
    tunnel.validate()?;
    config.tunnels.push(tunnel);
    Ok(())
}

fn edit_tunnel(config: &mut AppConfig) -> AppResult<()> {
    if config.tunnels.is_empty() {
        println!("There are no tunnels to edit.");
        return Ok(());
    }

    let index = prompt_index("Tunnel number to edit", config.tunnels.len())?;
    let mut tunnel = config.tunnels[index].clone();

    let new_name = prompt_optional("Name", Some(tunnel.name.as_str()))?;
    let new_name = new_name.trim().to_string();
    if new_name != tunnel.name {
        if new_name.is_empty() {
            println!("Name left empty. Keeping the current value.");
        } else if config
            .tunnels
            .iter()
            .enumerate()
            .any(|(idx, item)| idx != index && item.name == new_name)
        {
            println!("That name is already in use. Keeping the current value.");
        } else {
            tunnel.name = new_name;
        }
    }

    tunnel.protocol = prompt_protocol(Some(tunnel.protocol))?;
    tunnel.listen = prompt_socket_addr("Listen address", Some(tunnel.listen.as_str()))?;
    tunnel.target = prompt_socket_addr("Target address", Some(tunnel.target.as_str()))?;
    tunnel.enabled = prompt_bool(
        if tunnel.enabled {
            "Enable this tunnel? [Y/n]"
        } else {
            "Enable this tunnel? [y/N]"
        },
        tunnel.enabled,
    )?;

    tunnel.validate()?;
    config.tunnels[index] = tunnel;
    Ok(())
}

fn delete_tunnel(config: &mut AppConfig) -> AppResult<()> {
    if config.tunnels.is_empty() {
        println!("There are no tunnels to delete.");
        return Ok(());
    }

    let index = prompt_index("Tunnel number to delete", config.tunnels.len())?;
    let tunnel = &config.tunnels[index];
    if prompt_bool(&format!("Delete tunnel '{}' ? [y/N]", tunnel.name), false)? {
        config.tunnels.remove(index);
        println!("Tunnel deleted.");
    } else {
        println!("Delete cancelled.");
    }

    Ok(())
}

fn prompt_index(label: &str, max: usize) -> AppResult<usize> {
    loop {
        let input = prompt(label)?;
        match input.trim().parse::<usize>() {
            Ok(value) if (1..=max).contains(&value) => return Ok(value - 1),
            _ => println!("Please enter a number between 1 and {}.", max),
        }
    }
}

fn prompt_protocol(current: Option<Protocol>) -> AppResult<Protocol> {
    loop {
        println!("Select protocol: 1. TCP  2. UDP  3. Both");
        let default = current.map(|protocol| match protocol {
            Protocol::Tcp => "1",
            Protocol::Udp => "2",
            Protocol::Both => "3",
        });
        let input = prompt_optional("Protocol", default)?;
        let value = input.trim();

        let selected = if value.is_empty() {
            current
        } else {
            match value {
                "1" => Some(Protocol::Tcp),
                "2" => Some(Protocol::Udp),
                "3" => Some(Protocol::Both),
                _ => {
                    println!("Invalid choice. Please try again.");
                    continue;
                }
            }
        };

        if let Some(protocol) = selected {
            return Ok(protocol);
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

fn prompt_optional(label: &str, current: Option<&str>) -> AppResult<String> {
    let suffix = current
        .filter(|value| !value.is_empty())
        .map(|value| format!(" [default: {}]", value))
        .unwrap_or_default();
    prompt(&format!("{}{}", label, suffix))
}

fn prompt(label: &str) -> AppResult<String> {
    print!("{}: ", label);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim_end().to_string())
}
