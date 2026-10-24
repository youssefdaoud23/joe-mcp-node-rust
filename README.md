# Joe MCP Node - Rust

Rust managed-node runtime for Joe MCP Server.

Version: `0.1.0`  
Source server commit: `83f1f60352f4c311e940d1070f5d21ca5e8e1589`

This public repository contains the customer-facing Rust managed-node installer, service templates, and node source snapshot exported from the private `joe-mcp-server-rust` project. It does not contain the private Rust controller runtime, account state, OAuth state, customer data, or deployment secrets.

## Normal installation

Customers normally do not need to clone this repository. In the Rust Joe MCP controller, open **Devices -> Add Device**, copy the generated one-time installation command, and run it with `sudo` on the Linux machine to connect.

The installer:

- requires Linux with systemd;
- connects only to an HTTPS Joe MCP Rust controller;
- downloads the Rust agent and updater binaries from that controller;
- verifies SHA-256 checksums before installation;
- enrolls using a one-time code;
- starts the outbound managed-node service;
- installs the rollback-aware updater.

## Source layout

- `scripts/install-node.sh` - customer installer
- `deploy/` - systemd service templates
- `src/agent.rs` - Rust node-agent source snapshot
- `src/updater.rs` - Rust updater source snapshot

The source snapshots are built as part of the private Rust server workspace. This public repository is the customer node distribution and is not a standalone copy of the private controller Cargo workspace.

## Security

See `SECURITY.md`. Never commit enrollment codes, device credentials, API keys, controller state, private keys, or runtime configuration containing secrets.
