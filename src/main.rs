use bollard::{
    container::ListContainersOptions, container::LogOutput, exec::CreateExecOptions,
    exec::StartExecResults, Docker,
};
use futures::StreamExt;
use opentelemetry::{
    global,
    trace::{Span, Tracer, TracerProvider},
    KeyValue,
    metrics::{Counter, Meter},
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{self, RandomIdGenerator, Sampler, SdkTracer},
    Resource,
};
use serde::{Deserialize, Serialize};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::net::Ipv4Addr;
use std::process;
use std::str::FromStr;
use std::time::Duration;

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
                                Ok(conn) => connections.push(conn),
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

enum Env {
    Dev,
    Prod,
    Staging,
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Env::Dev => write!(f, "dev"),
            Env::Prod => write!(f, "prod"),
            Env::Staging => write!(f, "staging"),
        }
    }
}

impl Into<String> for Env {
    fn into(self) -> String {
        self.to_string()
    }
}

fn get_env() -> Env {
    match env::var("ENV").unwrap_or_else(|_| "dev".to_string()).as_str() {
        "prod" => Env::Prod,
        "staging" => Env::Staging,
        _ => Env::Dev,
    }
}

struct Components {
    tracer: opentelemetry_sdk::trace::Tracer,
    meter: Meter,
    provider: opentelemetry_sdk::trace::SdkTracerProvider,
}

fn init_tracer() -> Components {
    // Create a new OTLP exporter
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint("http://localhost:4318")
        .build()
        .expect("Failed to create OTLP exporter");

    // Create a tracer provider with the exporter
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::AlwaysOn)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", "stratum-counter"),
                KeyValue::new("service.version", VERSION),
            ])
            .build())
        .build();

    // Set the global tracer provider
    global::set_tracer_provider(tracer_provider.clone());

    // Get a tracer
    let tracer = tracer_provider.tracer("stratum-counter");

    // Get a meter from the global provider
    let meter = global::meter("stratum-counter");

    Components {
        tracer,
        meter,
        provider: tracer_provider,
    }
}

#[tokio::main]
async fn main() {
    let components = init_tracer();
    let mut span = components.tracer.start("stratum_counter_run");
    span.set_attribute(KeyValue::new("version", VERSION));

    // Create metrics
    let tcp_connections = components.meter
        .u64_counter("tcp.connections")
        .with_description("Number of TCP connections")
        .build();

    let tcp_connections_by_state = components.meter
        .u64_counter("tcp.connections.by_state")
        .with_description("Number of TCP connections by state")
        .build();

    println!("stratum-counter v{} (src: {}, built: {})", VERSION, SRC_HASH, BUILD_DATE);
    println!("{:-<80}", "");

    let args: Vec<String> = env::args().collect();
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
                eprintln!("Error: Unknown argument '{}'", args[i]);
                process::exit(1);
            }
        }
        i += 1;
    }

    span.set_attribute(KeyValue::new("json_output", json_output));

    // Connect to Docker daemon
    let docker = match Docker::connect_with_local_defaults() {
        Ok(docker) => docker,
        Err(e) => {
            span.record_error(&e as &dyn StdError);
            eprintln!("Error: Failed to connect to Docker daemon: {}", e);
            process::exit(1);
        }
    };

    // Get list of containers
    let containers = match docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        }))
        .await
    {
        Ok(containers) => containers,
        Err(e) => {
            span.record_error(&e as &dyn StdError);
            eprintln!("Error: Failed to list Docker containers: {}", e);
            process::exit(1);
        }
    };

    let mut container_infos = Vec::new();
    let mut total_connections = 0;

    // Process each container
    for container in containers {
        if let Some(container_id) = container.id {
            let mut container_span = components.tracer.start("process_container");
            container_span.set_attribute(KeyValue::new("container.id", container_id.clone()));

            match get_container_tcp_connections(&docker, &container_id).await {
                Ok(connections) => {
                    if !connections.is_empty() {
                        let container_name = container
                            .names
                            .as_ref()
                            .and_then(|n| n.first())
                            .map(|n| n.trim_start_matches('/').to_string())
                            .unwrap_or_else(|| container_id.clone());

                        container_span
                            .set_attribute(KeyValue::new("container.name", container_name.clone()));
                        container_span.set_attribute(KeyValue::new(
                            "connection.count",
                            connections.len() as i64,
                        ));

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

                        container_infos.push(ContainerInfo {
                            name: container_name,
                            id: container_id,
                            connections: connections.clone(),
                        });

                        total_connections += connections.len();
                    }
                    container_span.end();
                }
                Err(e) => {
                    let error_msg = e.clone();
                    let error = StringError(e);
                    container_span.record_error(&error as &dyn StdError);
                    eprintln!(
                        "Warning: Failed to get TCP connections for container {}: {}",
                        container_id, error_msg
                    );
                    container_span.end();
                }
            }
        }
    }

    span.set_attribute(KeyValue::new("total_connections", total_connections as i64));

    // Output results
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&container_infos).unwrap_or_else(|_| "[]".to_string())
        );
    } else {
        println!("TCP Connections in Docker Containers:");
        println!("{:-<80}", "");

        for info in container_infos {
            println!("Container: {} (ID: {})", info.name, info.id);
            println!("Number of connections: {}", info.connections.len());
            println!("{:-<80}", "");

            for conn in info.connections {
                println!("Local Address:   {}:{}", conn.local_addr, conn.local_port);
                println!("Remote Address:  {}:{}", conn.remote_addr, conn.remote_port);
                println!("State:          {}", conn.get_state_name());
                println!("{:-<80}", "");
            }
        }

        println!("Total connections found: {}", total_connections);
    }

    // Add a small delay to ensure metrics are exported
    tokio::time::sleep(Duration::from_secs(1)).await;

    span.end();

    // Shutdown the provider
    let _ = components.provider.shutdown();
    println!("Shutdown OpenTelemetry provider");
    process::exit(0);
}
