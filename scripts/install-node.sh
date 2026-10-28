#!/usr/bin/env bash
set -euo pipefail
SERVER="${JOE_RUST_SERVER:-}"
ENROLL=""
NAME="$(hostname 2>/dev/null || echo managed-node)"
CONFIG="/var/lib/joe-mcp-rust-agent/config.json"
LEGACY_CONFIG="/var/lib/joe-mcp-agent/config.json"
MIGRATED_LEGACY=0
rollback_legacy_migration() {
  local rc="${1:-$?}"
  if [[ "$MIGRATED_LEGACY" == "1" ]]; then
    systemctl disable --now joe-mcp-rust-agent.service >/dev/null 2>&1 || true
    rm -f "$CONFIG" || true
    systemctl daemon-reload >/dev/null 2>&1 || true
    echo "Rust migration failed before handoff; legacy Python Joe remains the active rollback path." >&2
  fi
  exit "$rc"
}
trap 'rollback_legacy_migration $?' ERR
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server) SERVER="${2:-}"; shift 2;;
    --enroll|--pairing-code) ENROLL="${2:-}"; shift 2;;
    --name) NAME="${2:-}"; shift 2;;
    --install-dir) shift 2;; # accepted only for legacy managed-updater compatibility
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
if [[ ! -s "$CONFIG" && -z "$ENROLL" && -s "$LEGACY_CONFIG" ]]; then
  command -v python3 >/dev/null || { echo "python3 is required to migrate an existing Python-generation Joe node." >&2; exit 1; }
  python3 - "$LEGACY_CONFIG" "$CONFIG" "$SERVER" "$NAME" <<'PYMIGRATE'
import json, os, pathlib, re, sys
legacy_path, config_path, expected_server, fallback_name = sys.argv[1:]
with open(legacy_path, "r", encoding="utf-8") as handle:
    legacy = json.load(handle)
server = str(legacy.get("server") or "").rstrip("/")
if server != expected_server.rstrip("/"):
    raise SystemExit("Legacy Joe node belongs to a different controller; refusing identity migration.")
device_id = str(legacy.get("device_id") or "")
token = str(legacy.get("device_token") or legacy.get("token") or "")
name = str(legacy.get("name") or fallback_name)
if not device_id.startswith("dev_") or len(token) < 32 or not re.fullmatch(r"[A-Za-z0-9._-]{1,64}", name):
    raise SystemExit("Legacy Joe node identity is incomplete or invalid; refusing migration.")
payload = {"server": expected_server.rstrip("/"), "device_id": device_id, "token": token, "name": name, "legacy_migration_pending": True}
target = pathlib.Path(config_path)
target.parent.mkdir(parents=True, exist_ok=True)
tmp = target.with_name("." + target.name + ".migrate")
fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2, sort_keys=True)
    handle.write("\n")
os.replace(tmp, target)
os.chmod(target, 0o600)
PYMIGRATE
  MIGRATED_LEGACY=1
  echo "Imported the existing Joe device identity for an in-place Rust migration."
fi
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
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
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
if ! systemctl is-active --quiet joe-mcp-rust-agent.service; then
  echo "Rust Joe agent failed to start." >&2
  rollback_legacy_migration 1
fi
trap - ERR
echo "Joe MCP Rust Agent is ready: $NAME"
echo "Controller: $SERVER"
