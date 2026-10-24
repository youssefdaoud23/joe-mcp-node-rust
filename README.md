# Joe MCP Node - Rust

Version: `0.1.0`  
Source server commit: `9643c4c5d438bee4a95d31e785da7d765c3b67c1`

This repository is the public customer-node distribution for the Rust implementation of Joe MCP Server. Its private controller source is maintained in `joe-mcp-server-rust`; customers do not need access to that private repository.

## Normal customer installation

Customers normally do not clone this repository. In the Rust Joe controller, open **Devices -> Add device** and run the generated one-time command on the Linux machine to enroll.

The installer in `scripts/install-node.sh`:

- requires Linux with systemd, `curl` and `sha256sum`;
- downloads `joe-mcp-rust-agent` and `joe-mcp-rust-updater` from the configured HTTPS controller;
- verifies SHA-256 checksums before installation;
- enrolls with a short-lived one-time code;
- stores the resulting device configuration under `/var/lib/joe-mcp-rust-agent/`;
- installs and enables `joe-mcp-rust-agent.service`;
- installs the guarded `joe-mcp-rust-update.service`;
- opens no inbound management port.

## Runtime components

- `src/agent.rs` - Rust managed-node agent source.
- `src/updater.rs` - rollback-aware guarded updater source.
- `scripts/install-node.sh` - customer installation/enrollment script.
- `deploy/joe-mcp-rust-agent.service.in` - agent service template.
- `deploy/joe-mcp-rust-update.service.in` - updater service template.

The release binaries are built from the private Rust workspace and served checksum-pinned by the controller. This public repository contains the intended customer-facing node source/install assets only, not controller code, account/OAuth state, customer data or controller secrets.

## Troubleshooting

```bash
systemctl status joe-mcp-rust-agent.service
journalctl -u joe-mcp-rust-agent.service -n 100 --no-pager
systemctl status joe-mcp-rust-update.service
```

## Source of truth

Do not independently fork or modify the distributed runtime files and still treat them as an official Joe release. `joe-mcp-server-rust` is the private source of truth; `MANIFEST.sha256` pins the five public node runtime files exported from it.

## Security

See `SECURITY.md`. Never commit or publish enrollment codes, device credentials, controller secrets, customer data or runtime configuration. The managed agent is outbound-only and enforces local policy before host operations are executed.
