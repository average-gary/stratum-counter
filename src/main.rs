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
use std::process;
use serde::{Serialize, Deserialize};

const VERSION: &str = "0.1.0";

fn print_usage() {
    println!("stratum-counter - Monitor TCP connections in Docker containers");
    println!();
    println!("Usage: stratum-counter [OPTIONS] [PORT]");
    println!();
    println!("Options:");
    println!("  -h, --help     Show this help message");
    println!("  -v, --version  Show version information");
    println!("  -j, --json     Output in JSON format");
    println!();
    println!("PORT:");
    println!("  The port number to monitor (default: 3333)");
    println!();
    println!("Examples:");
    println!("  stratum-counter              # Monitor port 3333");
    println!("  stratum-counter 34333         # Monitor port 34333");
    println!("  stratum-counter --json 3333  # Output in JSON format");
}

fn print_version() {
    println!("stratum-counter v{}", VERSION);
}

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

#[derive(Debug, Serialize, Deserialize, Clone)]
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

#[derive(Debug, Serialize, Deserialize)]
struct ContainerInfo {
    name: String,
    id: String,
    connections: Vec<TcpConnection>,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mut port = 3333;
    let mut json_output = false;

    // Parse command line arguments
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                process::exit(0);
            }
            "-v" | "--version" => {
                print_version();
                process::exit(0);
            }
            "-j" | "--json" => {
                json_output = true;
            }
            _ => {
                if let Ok(p) = args[i].parse::<u16>() {
                    port = p;
                } else {
                    eprintln!("Error: Invalid port number '{}'", args[i]);
                    process::exit(1);
                }
            }
        }
        i += 1;
    }

    // Connect to Docker daemon
    let docker = match Docker::connect_with_local_defaults() {
        Ok(docker) => docker,
        Err(e) => {
            eprintln!("Error: Failed to connect to Docker daemon: {}", e);
            process::exit(1);
        }
    };

    // Get list of containers
    let containers = match docker.list_containers(Some(ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    })).await {
        Ok(containers) => containers,
        Err(e) => {
            eprintln!("Error: Failed to list Docker containers: {}", e);
            process::exit(1);
        }
    };

    let mut container_infos = Vec::new();
    let mut total_connections = 0;

    // Process each container
    for container in containers {
        if let Some(container_id) = container.id {
            match get_container_tcp_connections(&docker, &container_id).await {
                Ok(connections) => {
                    let container_connections: Vec<_> = connections
                        .into_iter()
                        .filter(|conn| conn.state == 1 && conn.local_port == port)
                        .collect();

                    if !container_connections.is_empty() {
                        let container_name = container.names.as_ref()
                            .and_then(|n| n.first())
                            .map(|n| n.trim_start_matches('/').to_string())
                            .unwrap_or_else(|| container_id.clone());

                        container_infos.push(ContainerInfo {
                            name: container_name,
                            id: container_id,
                            connections: container_connections.clone(),
                        });

                        total_connections += container_connections.len();
                    }
                }
                Err(e) => {
                    eprintln!("Warning: Failed to get TCP connections for container {}: {}", container_id, e);
                }
            }
        }
    }

    // Output results
    if json_output {
        println!("{}", serde_json::to_string_pretty(&container_infos).unwrap_or_else(|_| "[]".to_string()));
    } else {
        println!("Established TCP Connections from Port {} in Docker Containers:", port);
        println!("{:-<80}", "");

        for info in container_infos {
            println!("Container: {} (ID: {})", info.name, info.id);
            println!("Number of connections: {}", info.connections.len());
            println!("{:-<80}", "");

            for conn in info.connections {
                println!("Local Address:   {}:{}", conn.local_addr, conn.local_port);
                println!("Remote Address:  {}:{}", conn.remote_addr, conn.remote_port);
                println!("State:          ESTABLISHED");
                println!("{:-<80}", "");
            }
        }

        if total_connections == 0 {
            println!("No established TCP connections found from port {} in Docker containers.", port);
        } else {
            println!("Total connections found: {}", total_connections);
        }
    }

    process::exit(0);
}
