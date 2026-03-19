#!/usr/bin/env bash
# setup_master.sh — Install and run mtpdb_master as a systemd service.
#
# Usage (public repo — no token needed):
#   curl -fsSL https://raw.githubusercontent.com/manaknight/mtpdb_master/main/setup_master.sh | sudo bash
#   OR clone the repo and run:  sudo bash setup_master.sh
#
# Usage (private repo — pass token at runtime, never hardcode):
#   sudo GH_TOKEN=ghp_xxx bash setup_master.sh
#
# Options (environment variables):
#   MASTER_PORT           Listen port              (default: 7000)
#   MASTER_ADVERTISE_URL  URL managers call back to (default: auto-detect)
#   INSTALL_DIR           Where to put the binary  (default: /usr/local/bin)
#   SERVICE_USER          systemd service user      (default: mtpdb)
#   SKIP_BUILD            Set to 1 to skip Rust build (binary must already exist)
#   REPO_DIR              Where to clone/build      (default: /opt/mtpdb_master)
#   GH_TOKEN              GitHub PAT for private repos (optional)

set -euo pipefail

# ── config ────────────────────────────────────────────────────────────────────

MASTER_PORT="${MASTER_PORT:-7000}"
MASTER_ADVERTISE_URL="${MASTER_ADVERTISE_URL:-}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
SERVICE_USER="${SERVICE_USER:-mtpdb}"
SKIP_BUILD="${SKIP_BUILD:-0}"
REPO_DIR="${REPO_DIR:-/opt/mtpdb_master}"
SERVICE_NAME="mtpdb_master"
BINARY_NAME="mtpdb_master"

# Build the clone URL: inject token only when GH_TOKEN is provided (private repos).
# Public repos work fine with the plain URL and no credentials.
_REPO_BASE="github.com/manaknight/mtpdb_master.git"
if [[ -n "${GH_TOKEN:-}" ]]; then
  REPO_URL="https://${GH_TOKEN}@${_REPO_BASE}"
else
  REPO_URL="https://${_REPO_BASE}"
fi

# ── colours ───────────────────────────────────────────────────────────────────

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }

# ── root check ────────────────────────────────────────────────────────────────

if [[ "$EUID" -ne 0 ]]; then
  error "Please run as root: sudo bash $0"
fi

# ── detect OS ─────────────────────────────────────────────────────────────────

if ! command -v apt-get &>/dev/null; then
  error "This script requires a Debian/Ubuntu system with apt-get."
fi

info "Updating package lists…"
apt-get update -qq

# ── system dependencies ───────────────────────────────────────────────────────
# ssh2 crate needs libssh2 and OpenSSL dev headers; reqwest needs OpenSSL too.

info "Installing system dependencies…"
apt-get install -y -qq \
  build-essential \
  curl \
  git \
  pkg-config \
  libssl-dev \
  libssh2-1-dev \
  ca-certificates

# ── Rust ──────────────────────────────────────────────────────────────────────

if ! command -v cargo &>/dev/null; then
  info "Installing Rust toolchain via rustup…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path
  export PATH="$HOME/.cargo/bin:$PATH"
  source "$HOME/.cargo/env" 2>/dev/null || true
else
  info "Rust already installed: $(rustc --version)"
fi

# Ensure cargo is on PATH for the rest of this script.
export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"

if ! command -v cargo &>/dev/null; then
  error "cargo not found after install — check Rust setup."
fi

# ── clone / update repo ───────────────────────────────────────────────────────

if [[ "$SKIP_BUILD" != "1" ]]; then
  if [[ -d "$REPO_DIR/.git" ]]; then
    info "Updating existing clone at $REPO_DIR…"
    git -C "$REPO_DIR" pull --ff-only
  else
    info "Cloning $REPO_URL → $REPO_DIR…"
    git clone "$REPO_URL" "$REPO_DIR"
  fi

  # ── build ─────────────────────────────────────────────────────────────────

  info "Building $BINARY_NAME in release mode (this may take a few minutes)…"
  cargo build --manifest-path "$REPO_DIR/Cargo.toml" --release

  BUILT_BINARY="$REPO_DIR/target/release/$BINARY_NAME"
  if [[ ! -f "$BUILT_BINARY" ]]; then
    error "Build finished but binary not found at $BUILT_BINARY"
  fi

  info "Installing binary to $INSTALL_DIR/$BINARY_NAME…"
  install -m 755 "$BUILT_BINARY" "$INSTALL_DIR/$BINARY_NAME"
else
  if [[ ! -f "$INSTALL_DIR/$BINARY_NAME" ]]; then
    error "SKIP_BUILD=1 but $INSTALL_DIR/$BINARY_NAME does not exist."
  fi
  info "Skipping build; using existing binary at $INSTALL_DIR/$BINARY_NAME"
fi

# ── service user ──────────────────────────────────────────────────────────────

if ! id "$SERVICE_USER" &>/dev/null; then
  info "Creating system user '$SERVICE_USER'…"
  useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
else
  info "User '$SERVICE_USER' already exists."
fi

# ── firewall ──────────────────────────────────────────────────────────────────

if command -v ufw &>/dev/null; then
  info "Opening port $MASTER_PORT in UFW…"
  ufw allow "$MASTER_PORT/tcp" || warn "UFW rule may already exist — skipping."
fi

# ── resolve advertise URL ─────────────────────────────────────────────────────

if [[ -z "$MASTER_ADVERTISE_URL" ]]; then
  # Try to auto-detect outbound IP.
  DETECTED_IP=$(ip route get 8.8.8.8 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1); exit}' || true)
  if [[ -z "$DETECTED_IP" ]]; then
    DETECTED_IP=$(hostname -I | awk '{print $1}')
  fi
  MASTER_ADVERTISE_URL="http://${DETECTED_IP}:${MASTER_PORT}"
  warn "MASTER_ADVERTISE_URL not set — using detected: $MASTER_ADVERTISE_URL"
  warn "Override with: export MASTER_ADVERTISE_URL=http://<your-public-ip>:$MASTER_PORT"
fi

# ── systemd service ───────────────────────────────────────────────────────────

SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"

info "Writing systemd unit to $SERVICE_FILE…"
cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=MTPDB Master Orchestrator
Documentation=https://github.com/manaknight/mtpdb_master
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
ExecStart=${INSTALL_DIR}/${BINARY_NAME}
Restart=on-failure
RestartSec=5s

# Configuration — edit and run: systemctl daemon-reload && systemctl restart ${SERVICE_NAME}
Environment=MASTER_PORT=${MASTER_PORT}
Environment=MASTER_ADVERTISE_URL=${MASTER_ADVERTISE_URL}

# Logging — journalctl -u ${SERVICE_NAME} -f
StandardOutput=journal
StandardError=journal
SyslogIdentifier=${SERVICE_NAME}

# Hardening
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/tmp

[Install]
WantedBy=multi-user.target
EOF

# ── enable & start ────────────────────────────────────────────────────────────

info "Enabling and (re)starting ${SERVICE_NAME}…"
systemctl daemon-reload
systemctl enable --quiet "${SERVICE_NAME}"
systemctl restart "${SERVICE_NAME}"

# Wait a moment for the service to come up.
sleep 2

if systemctl is-active --quiet "${SERVICE_NAME}"; then
  echo ""
  echo -e "${GREEN}╔══════════════════════════════════════════════════╗${NC}"
  echo -e "${GREEN}║      mtpdb_master is running!                    ║${NC}"
  echo -e "${GREEN}╚══════════════════════════════════════════════════╝${NC}"
  echo ""
  info "Service: ${SERVICE_NAME}"
  info "Binary:  ${INSTALL_DIR}/${BINARY_NAME}"
  info "Port:    ${MASTER_PORT}"
  info "Advertise URL: ${MASTER_ADVERTISE_URL}"
  echo ""
  info "Health check:"
  echo "  curl http://localhost:${MASTER_PORT}/health"
  echo ""
  info "Logs:"
  echo "  journalctl -u ${SERVICE_NAME} -f"
  echo ""
  info "Manage:"
  echo "  systemctl status  ${SERVICE_NAME}"
  echo "  systemctl restart ${SERVICE_NAME}"
  echo "  systemctl stop    ${SERVICE_NAME}"
  echo ""
  info "To change config, edit ${SERVICE_FILE} then:"
  echo "  systemctl daemon-reload && systemctl restart ${SERVICE_NAME}"
else
  error "${SERVICE_NAME} failed to start. Check logs: journalctl -u ${SERVICE_NAME} --no-pager -n 50"
fi
