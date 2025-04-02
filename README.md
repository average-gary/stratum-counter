# Stratum Counter

A Unix-style tool for monitoring TCP connections in Docker containers, with a focus on Stratum protocol ports.

## Features

- Monitor TCP connections in Docker containers
- Filter by specific port numbers
- Human-readable and JSON output formats
- Follows Unix tool conventions
- Lightweight and fast

## Installation

### From Source

```bash
git clone https://github.com/average-gary/stratum-counter.git
cd stratum-counter
cargo build --release
```

The binary will be available at `target/release/stratum-counter`.

### From Cargo

```bash
cargo install stratum-counter
```

## Usage

```bash
# Basic usage (monitors port 3333 by default)
stratum-counter

# Monitor a specific port
stratum-counter 34333

# Output in JSON format
stratum-counter --json 

# Show help
stratum-counter --help

# Show version
stratum-counter --version
```

## Examples

```bash
# Monitor default port (3333)
$ stratum-counter
Established TCP Connections from Port 3333 in Docker Containers:
--------------------------------------------------------------------------------
Container: stratum-server (ID: abc123...)
Number of connections: 42
--------------------------------------------------------------------------------
Local Address:   192.168.1.1:3333
Remote Address:  10.0.0.1:54321
State:          ESTABLISHED
--------------------------------------------------------------------------------

# JSON output
$ stratum-counter --json 
[
  {
    "name": "stratum-server",
    "id": "abc123...",
    "connections": [
      {
        "local_addr": "192.168.1.1",
        "local_port": 3333,
        "remote_addr": "10.0.0.1",
        "remote_port": 54321,
        "state": 1
      }
    ]
  }
]
```

## Requirements

- Rust 1.70 or later
- Docker daemon running
- Unix-like operating system (Linux, macOS)

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## Author

Gary Krause - [GitHub](https://github.com/average-gary)
