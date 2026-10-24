#!/usr/bin/env bash
set -euo pipefail
SERVER="${JOE_RUST_SERVER:-}"
ENROLL=""
NAME="$(hostname 2>/dev/null || echo managed-node)"
CONFIG="/var/lib/joe-mcp-rust-agent/config.json"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server) SERVER="${2:-}"; shift 2;;
    --enroll|--pairing-code) ENROLL="${2:-}"; shift 2;;
    --name) NAME="${2:-}"; shift 2;;
    *) echo "Unknown argument: $1" >&2; exit 2;;
  esac
done
[[ ${EUID:-$(id -u)} -eq 0 ]] || { echo "Run with sudo/root." >&2; exit 1; }
[[ "$SERVER" == https://* && "$SERVER" != *[[:space:]]* ]] || { echo "--server must be HTTPS." >&2; exit 1; }
[[ "$NAME" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || { echo "Invalid device name." >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required." >&2; exit 1; }
command -v sha256sum >/dev/null || { echo "sha256sum is required." >&2; exit 1; }
command -v systemctl >/dev/null || { echo "systemd is required." >&2; exit 1; }
SERVER="${SERVER%/}"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
for item in agent updater; do
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 "$SERVER/rust-$item" -o "$TMP/$item"
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 "$SERVER/rust-$item.sha256" -o "$TMP/$item.sha256"
  (cd "$TMP" && sed "s/joe-mcp-rust-$item/$item/" "$item.sha256" | sha256sum -c - >/dev/null)
done
install -d -m 0755 /usr/local/libexec
install -d -m 0700 /var/lib/joe-mcp-rust-agent
install -m 0755 "$TMP/agent" /usr/local/libexec/joe-mcp-rust-agent
install -m 0755 "$TMP/updater" /usr/local/libexec/joe-mcp-rust-updater
if [[ ! -s "$CONFIG" ]]; then
  [[ -n "$ENROLL" ]] || { echo "A one-time --enroll code is required for first installation." >&2; exit 1; }
  /usr/local/libexec/joe-mcp-rust-agent --server "$SERVER" --enroll "$ENROLL" --name "$NAME" --config "$CONFIG" --enroll-only
fi
cat >/etc/systemd/system/joe-mcp-rust-agent.service <<EOF
[Unit]
Description=Joe MCP Rust Managed Node Agent
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=root
Group=root
UMask=0077
ExecStart=/usr/local/libexec/joe-mcp-rust-agent --server $SERVER --config $CONFIG
Restart=always
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictRealtime=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
[Install]
WantedBy=multi-user.target
EOF
cat >/etc/systemd/system/joe-mcp-rust-update.service <<EOF
[Unit]
Description=Joe MCP Rust Guarded Managed Node Updater
After=network-online.target joe-mcp-rust-agent.service
Wants=network-online.target
[Service]
Type=oneshot
User=root
Group=root
UMask=0077
ExecStartPre=/bin/sleep 2
ExecStart=/usr/local/libexec/joe-mcp-rust-updater --server $SERVER
NoNewPrivileges=true
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictRealtime=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
EOF
chmod 0644 /etc/systemd/system/joe-mcp-rust-agent.service /etc/systemd/system/joe-mcp-rust-update.service
systemctl daemon-reload
systemctl enable joe-mcp-rust-agent.service >/dev/null
systemctl restart joe-mcp-rust-agent.service
for _ in $(seq 1 20); do systemctl is-active --quiet joe-mcp-rust-agent.service && break; sleep 1; done
systemctl is-active --quiet joe-mcp-rust-agent.service || { echo "Rust Joe agent failed to start." >&2; exit 1; }
echo "Joe MCP Rust Agent is ready: $NAME"
echo "Controller: $SERVER"
