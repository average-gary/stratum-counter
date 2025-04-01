use sysinfo::System;
use bollard::{
    Docker,
    container::ListContainersOptions,
    exec::CreateExecOptions,
    exec::StartExecResults,
    container::LogOutput
};
use std::collections::HashMap;
use std::str::FromStr;
use futures::StreamExt;
use std::net::Ipv4Addr;
use std::env;

fn hex_to_ip(hex: &str) -> String {
    if hex.len() != 8 {
        return hex.to_string();
    }
    
    let mut bytes = Vec::new();
    for i in 0..4 {
        if let Ok(byte) = u8::from_str_radix(&hex[i*2..i*2+2], 16) {
            bytes.push(byte);
        }
    }
    
    if bytes.len() == 4 {
        Ipv4Addr::new(bytes[3], bytes[2], bytes[1], bytes[0]).to_string()
    } else {
        hex.to_string()
    }
}

#[derive(Debug)]
struct TcpConnection {
    local_addr: String,
    local_port: u16,
    remote_addr: String,
    remote_port: u16,
    state: u8,
}

impl FromStr for TcpConnection {
    type Err = String;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            return Err("Invalid line format".to_string());
        }

        // Parse local address and port
        let local = parts[1];
        let local_parts: Vec<&str> = local.split(':').collect();
        if local_parts.len() != 2 {
            return Err("Invalid local address format".to_string());
        }
        let local_addr = hex_to_ip(local_parts[0]);
        let local_port = u16::from_str_radix(local_parts[1], 16)
            .map_err(|e| format!("Failed to parse local port: {}", e))?;

        // Parse remote address and port
        let remote = parts[2];
        let remote_parts: Vec<&str> = remote.split(':').collect();
        if remote_parts.len() != 2 {
            return Err("Invalid remote address format".to_string());
        }
        let remote_addr = hex_to_ip(remote_parts[0]);
        let remote_port = u16::from_str_radix(remote_parts[1], 16)
            .map_err(|e| format!("Failed to parse remote port: {}", e))?;

        // Parse state
        let state = u8::from_str_radix(parts[3], 16)
            .map_err(|e| format!("Failed to parse state: {}", e))?;

        Ok(TcpConnection {
            local_addr,
            local_port,
            remote_addr,
            remote_port,
            state,
        })
    }
}

async fn get_container_tcp_connections(docker: &Docker, container_id: &str) -> Result<Vec<TcpConnection>, String> {
    // Create exec command to read /proc/net/tcp
    let exec_options = CreateExecOptions {
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        cmd: Some(vec!["cat", "/proc/net/tcp"]),
        ..Default::default()
    };

    let exec = docker.create_exec(container_id, exec_options)
        .await
        .map_err(|e| format!("Failed to create exec: {}", e))?;

    let exec_id = exec.id;
    let start_exec = docker.start_exec(&exec_id, None)
        .await
        .map_err(|e| format!("Failed to start exec: {}", e))?;

    let mut connections = Vec::new();
    match start_exec {
        StartExecResults::Attached { mut output, .. } => {
            while let Some(Ok(output)) = output.next().await {
                match output {
                    LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                        let line = String::from_utf8_lossy(&message);
                        // Skip header line
                        if line.starts_with("sl") {
                            continue;
                        }
                        if let Ok(conn) = TcpConnection::from_str(&line) {
                            connections.push(conn);
                        }
                    }
                    _ => continue,
                }
            }
        }
        _ => return Err("Failed to get exec output".to_string()),
    }

    Ok(connections)
}

#[tokio::main]
async fn main() {
    // Get port from command line arguments
    let port = env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u16>().ok())
        .unwrap_or(3333); // Default to 3333 if no port specified

    // Initialize system information
    let mut sys = System::new_all();
    sys.refresh_all();

    // Connect to Docker daemon
    let docker = match Docker::connect_with_local_defaults() {
        Ok(docker) => docker,
        Err(e) => {
            eprintln!("Failed to connect to Docker daemon: {}", e);
            return;
        }
    };

    // Get list of containers
    let containers = match docker.list_containers(Some(ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    })).await {
        Ok(containers) => containers,
        Err(e) => {
            eprintln!("Failed to list Docker containers: {}", e);
            return;
        }
    };

    // Create a map of container IDs to container names
    let container_map: HashMap<String, String> = containers
        .iter()
        .filter_map(|c| {
            c.id.as_ref().map(|id| {
                let name = c.names.as_ref()
                    .and_then(|n| n.first())
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| id.clone());
                (id.clone(), name)
            })
        })
        .collect();

    println!("\nEstablished TCP Connections from Port {} in Docker Containers:", port);
    println!("{:-<80}", "");

    let mut total_connections = 0;

    // Check each container's TCP connections
    for container in containers {
        if let Some(container_id) = container.id {
            match get_container_tcp_connections(&docker, &container_id).await {
                Ok(connections) => {
                    // Filter for established connections (state 1) from specified port
                    let container_connections: Vec<_> = connections
                        .into_iter()
                        .filter(|conn| {
                            conn.state == 1 && // ESTABLISHED state
                            conn.local_port == port
                        })
                        .collect();

                    if !container_connections.is_empty() {
                        let container_name = container_map.get(&container_id).unwrap_or(&container_id);
                        println!("Container: {} (ID: {})", container_name, container_id);
                        println!("Number of connections: {}", container_connections.len());
                        println!("{:-<80}", "");

                        for conn in container_connections {
                            println!("Local Address:   {}:{}", conn.local_addr, conn.local_port);
                            println!("Remote Address:  {}:{}", conn.remote_addr, conn.remote_port);
                            println!("State:          ESTABLISHED");
                            println!("{:-<80}", "");
                            total_connections += 1;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to get TCP connections for container {}: {}", container_id, e);
                }
            }
        }
    }

    if total_connections == 0 {
        println!("No established TCP connections found from port {} in Docker containers.", port);
    } else {
        println!("Total connections found: {}", total_connections);
    }
}
