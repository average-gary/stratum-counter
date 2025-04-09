use bollard::{
    container::ListContainersOptions, container::LogOutput, exec::CreateExecOptions,
    exec::StartExecResults, Docker,
};
use futures::StreamExt;
use opentelemetry::{
    global,
    metrics::{Counter, Meter, MeterProvider},
    trace::FutureExt,
    KeyValue,
};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    metrics::{PeriodicReader, SdkMeterProvider},
    runtime::Tokio,
};
use serde::{Deserialize, Serialize};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::net::Ipv4Addr;
use std::process;
use std::str::FromStr;
use std::time::Duration;
use tokio::signal;
use tokio::time::sleep;

#[derive(Debug)]
struct StringError(String);

impl fmt::Display for StringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl StdError for StringError {}

const VERSION: &str = "0.1.0";

// Add build information constants
const SRC_HASH: &str = env!("SRC_HASH", "unknown");
const BUILD_DATE: &str = env!("BUILD_DATE", "unknown");

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
    // The IP address is stored in network byte order (big-endian)
    // Each byte is represented by 2 hex characters
    if hex.len() != 8 {
        return hex.to_string();
    }

    let mut bytes = Vec::new();
    for i in 0..4 {
        if let Ok(byte) = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
            bytes.push(byte);
        }
    }

    if bytes.len() == 4 {
        // Convert from network byte order to host byte order
        Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()
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

impl TcpConnection {
    fn get_state_name(&self) -> &'static str {
        match self.state {
            1 => "ESTABLISHED",
            2 => "SYN_SENT",
            3 => "SYN_RECV",
            4 => "FIN_WAIT1",
            5 => "FIN_WAIT2",
            6 => "TIME_WAIT",
            7 => "CLOSE",
            8 => "CLOSE_WAIT",
            9 => "LAST_ACK",
            10 => "LISTEN",
            11 => "CLOSING",
            _ => "UNKNOWN",
        }
    }
}

impl FromStr for TcpConnection {
    type Err = String;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        // Skip empty lines and header line
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("sl") {
            return Err("SKIP".to_string());  // Special value to indicate intentional skip
        }

        // Split the line into parts and ensure we have enough fields
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 4 {
            return Err(format!("Invalid line format: expected at least 4 fields, got {}", parts.len()));
        }

        // Extract the relevant fields (skip the first field which is the line number)
        let local = parts[1];  // local_address:port in hex
        let remote = parts[2]; // remote_address:port in hex
        let state = parts[3];  // connection state in hex

        // Parse local address and port (format: 00000000:0000)
        let local_parts: Vec<&str> = local.split(':').collect();
        if local_parts.len() != 2 {
            return Err(format!("Invalid local address format: {}", local));
        }
        let local_addr = hex_to_ip(local_parts[0]);
        let local_port = u16::from_str_radix(local_parts[1], 16)
            .map_err(|e| format!("Failed to parse local port: {}", e))?;

        // Parse remote address and port (format: 00000000:0000)
        let remote_parts: Vec<&str> = remote.split(':').collect();
        if remote_parts.len() != 2 {
            return Err(format!("Invalid remote address format: {}", remote));
        }
        let remote_addr = hex_to_ip(remote_parts[0]);
        let remote_port = u16::from_str_radix(remote_parts[1], 16)
            .map_err(|e| format!("Failed to parse remote port: {}", e))?;

        // Parse state (format: 0A)
        let state = u8::from_str_radix(state, 16)
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

async fn get_container_tcp_connections(
    docker: &Docker,
    container_id: &str,
) -> Result<Vec<TcpConnection>, String> {
    // Create exec command to read /proc/net/tcp
    let exec_options = CreateExecOptions {
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        cmd: Some(vec!["cat", "/proc/net/tcp"]),
        ..Default::default()
    };

    let exec = docker
        .create_exec(container_id, exec_options)
        .await
        .map_err(|e| format!("Failed to create exec: {}", e))?;

    let exec_id = exec.id;
    let start_exec = docker
        .start_exec(&exec_id, None)
        .await
        .map_err(|e| format!("Failed to start exec: {}", e))?;

    let mut connections = Vec::new();
    match start_exec {
        StartExecResults::Attached { mut output, .. } => {
            while let Some(result) = output.next().await {
                match result {
                    Ok(LogOutput::StdOut { message }) | Ok(LogOutput::StdErr { message }) => {
                        let content = String::from_utf8_lossy(&message);
                        for line in content.lines() {
                            match TcpConnection::from_str(line) {
                                Ok(conn) => {
                                    connections.push(conn.clone());
                                    println!("Connection: {:?}", conn);
                                },
                                Err(e) if e == "SKIP" => continue,
                                Err(e) => eprintln!("Error parsing TCP connection: {}", e),
                            }
                        }
                    }
                    Ok(_) => continue,
                    Err(e) => eprintln!("Error reading from container: {}", e),
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

async fn collect_metrics(
    docker: &Docker,
    tcp_connections: &Counter<u64>,
    tcp_connections_by_state: &Counter<u64>,
) -> Result<(), Box<dyn StdError + Send + Sync>> {
    // Get list of containers
    let containers = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        }))
        .await?;

    for container in containers {
        if let Some(container_id) = container.id {
            match get_container_tcp_connections(&docker, &container_id).await {
                Ok(connections) => {
                    if !connections.is_empty() {
                        let container_name = container
                            .names
                            .as_ref()
                            .and_then(|n| n.first())
                            .map(|n| n.trim_start_matches('/').to_string())
                            .unwrap_or_else(|| container_id.clone());

                        // Record metrics for total connections
                        tcp_connections.add(
                            connections.len() as u64,
                            &[
                                KeyValue::new("container.name", container_name.clone()),
                                KeyValue::new("container.id", container_id.clone()),
                            ],
                        );

                        // Record metrics for connections by state
                        for conn in &connections {
                            tcp_connections_by_state.add(
                                1,
                                &[
                                    KeyValue::new("container.name", container_name.clone()),
                                    KeyValue::new("container.id", container_id.clone()),
                                    KeyValue::new("state", conn.get_state_name().to_string()),
                                    KeyValue::new("local_port", conn.local_port.to_string()),
                                ],
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to get TCP connections for container {}: {}",
                        container_id, e
                    );
                }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError + Send + Sync>> {
    
    // Initialize metrics
    let meter_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint("http://localhost:4317")
        .build()?;
    
    let reader = PeriodicReader::builder(meter_exporter)
        .with_interval(Duration::from_secs(2)) // Export every minute
        .build();
    
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .build();
    
    global::set_meter_provider(provider);

    // Create metrics
    let meter = global::meter("stratum-counter");
    let tcp_connections = meter
        .u64_counter("tcp.connections")
        .with_description("Number of TCP connections")
        .build();

    let tcp_connections_by_state = meter
        .u64_counter("tcp.connections.by_state")
        .with_description("Number of TCP connections by state")
        .build();

    // Connect to Docker daemon
    let docker = Docker::connect_with_local_defaults()?;

    println!("stratum-counter v{} (src: {}, built: {})", VERSION, SRC_HASH, BUILD_DATE);
    println!("Starting daemon mode - checking containers every 5 minutes");
    println!("Press Ctrl+C to exit");

    // Main loop
    loop {
        // Collect metrics
        if let Err(e) = collect_metrics(&docker, &tcp_connections, &tcp_connections_by_state).await {
            eprintln!("Error collecting metrics: {}", e);
        }
        println!("Metrics collected");

        // Wait for next iteration or shutdown signal
        tokio::select! {
            _ = sleep(Duration::from_secs(300)) => {
                // 5 minutes have passed, continue to next iteration
            }
            _ = signal::ctrl_c() => {
                println!("\nShutting down...");
                break;
            }
        }
    }

    println!("Shutting down...");



    sleep(Duration::from_secs(1)).await; // Give time for final export

    Ok(())
}
