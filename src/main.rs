/// mtpdb_master — Central orchestrator for MTPDB deployments.
///
/// Workflow:
///   1. Run `mtpdb_master` on any server (or same box as an MTPDB instance).
///   2. POST /servers   → provide a remote server's IP + SSH credentials.
///                        Master SSHes in, starts mtpdb_manager, which calls
///                        back to POST /internal/register once online.
///   3. GET  /servers   → see all registered managers and their regions.
///   4. POST /databases → pick a server_id, master forwards to that manager,
///                        manager spins up a new MTPDB process and returns
///                        { host, pg_port, mysql_port, api_port, password }.
///
/// Environment variables:
///   MASTER_PORT            — HTTP listen port (default 7000)
///   MASTER_ADVERTISE_URL   — URL managers use to reach this master
///                            (default auto-detected via UDP trick)

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::TcpStream, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use uuid::Uuid;

// ── shared types ──────────────────────────────────────────────────────────────

/// A registered manager (one per remote server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerEntry {
    pub server_id: String,
    pub ip: String,
    pub region: String,
    pub manager_port: u16,
    pub registered_at: String,
    /// "provisioning" | "online" | "unreachable"
    pub status: String,
}

// ── application state ─────────────────────────────────────────────────────────

#[derive(Default)]
struct MasterState {
    /// server_id → manager metadata
    managers: HashMap<String, ManagerEntry>,
}

type SharedState = Arc<RwLock<MasterState>>;

// ── request bodies ────────────────────────────────────────────────────────────

/// POST /servers — provision a new manager on a remote box via SSH.
#[derive(Deserialize)]
struct ProvisionServerRequest {
    /// Remote server IP (SSH target)
    ip: String,
    /// SSH username
    username: String,
    /// SSH password
    password: String,
    /// Logical region label, e.g. "us-east-1" (default: "default")
    region: Option<String>,
    /// Port the manager process should listen on (default: 7001)
    manager_port: Option<u16>,
    /// Absolute path to the mtpdb_manager binary on the remote server
    /// (default: /usr/local/bin/mtpdb_manager)
    manager_binary_path: Option<String>,
}

/// POST /databases — provision a new MTPDB instance.
#[derive(Deserialize)]
struct ProvisionDbRequest {
    /// Which managed server to provision on
    server_id: String,
    /// Optional DB name (auto-generated if omitted)
    db_name: Option<String>,
}

/// POST /internal/register — called by managers on startup.
#[derive(Deserialize)]
struct ManagerRegisterRequest {
    server_id: String,
    ip: String,
    region: String,
    manager_port: u16,
}

// ── error helper ──────────────────────────────────────────────────────────────

fn api_err(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

fn not_found(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

// ── handlers ──────────────────────────────────────────────────────────────────

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "service": "mtpdb_master" }))
}

/// GET /servers — list all registered managers.
async fn list_servers(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let st = state.read().await;
    let servers: Vec<&ManagerEntry> = st.managers.values().collect();
    Json(serde_json::json!({ "servers": servers, "count": servers.len() }))
}

/// POST /servers — SSH into a remote box and start mtpdb_manager there.
async fn provision_server(
    State(state): State<SharedState>,
    Json(req): Json<ProvisionServerRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let manager_port = req.manager_port.unwrap_or(7001);
    let region = req.region.clone().unwrap_or_else(|| "default".to_string());
    let binary_path = req
        .manager_binary_path
        .clone()
        .unwrap_or_else(|| "/usr/local/bin/mtpdb_manager".to_string());

    // Determine the URL managers will use to reach us back.
    let master_url = std::env::var("MASTER_ADVERTISE_URL")
        .unwrap_or_else(|_| format!("http://{}:7000", local_ip_best_effort()));

    // Allocate a server_id now so master can pre-register it.
    let server_id = Uuid::new_v4().to_string();

    let ip = req.ip.clone();
    let username = req.username.clone();
    let ssh_password = req.password.clone();
    let sid = server_id.clone();
    let mu = master_url.clone();

    // SSH is synchronous — run it in a blocking thread.
    let result = tokio::task::spawn_blocking(move || {
        ssh_start_manager(&ip, &username, &ssh_password, &sid, manager_port, &region, &mu, &binary_path)
    })
    .await
    .map_err(|e| api_err(format!("Spawn error: {}", e)))?;

    match result {
        Ok(()) => {
            // Pre-register with "provisioning" status.
            // Manager will overwrite this via POST /internal/register once it's up.
            let entry = ManagerEntry {
                server_id: server_id.clone(),
                ip: req.ip.clone(),
                region: req.region.unwrap_or_else(|| "default".to_string()),
                manager_port,
                registered_at: Utc::now().to_rfc3339(),
                status: "provisioning".to_string(),
            };
            state.write().await.managers.insert(server_id.clone(), entry);

            Ok(Json(serde_json::json!({
                "server_id": server_id,
                "status": "provisioning",
                "message": "Manager process started via SSH; it will register shortly.",
                "manager_url": format!("http://{}:{}", req.ip, manager_port),
            })))
        }
        Err(e) => Err(api_err(format!("SSH provisioning failed: {}", e))),
    }
}

/// POST /internal/register — managers call this once they are up.
async fn manager_register(
    State(state): State<SharedState>,
    Json(req): Json<ManagerRegisterRequest>,
) -> Json<serde_json::Value> {
    println!(
        "[master] Manager registered: {} @ {}:{} region={}",
        req.server_id, req.ip, req.manager_port, req.region
    );
    let entry = ManagerEntry {
        server_id: req.server_id.clone(),
        ip: req.ip.clone(),
        region: req.region.clone(),
        manager_port: req.manager_port,
        registered_at: Utc::now().to_rfc3339(),
        status: "online".to_string(),
    };
    state.write().await.managers.insert(req.server_id.clone(), entry);
    Json(serde_json::json!({ "status": "registered", "server_id": req.server_id }))
}

/// GET /databases — aggregate instances from every online manager.
async fn list_databases(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let managers: Vec<ManagerEntry> = {
        let st = state.read().await;
        st.managers.values().cloned().collect()
    };

    let client = Client::new();
    let mut all: Vec<serde_json::Value> = Vec::new();

    for mgr in &managers {
        let url = format!("http://{}:{}/instances", mgr.ip, mgr.manager_port);
        match client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(arr) = body.get("instances").and_then(|v| v.as_array()) {
                        for inst in arr {
                            let mut inst = inst.clone();
                            // Annotate with routing metadata so callers know
                            // which server each instance lives on.
                            inst["server_id"] = serde_json::json!(&mgr.server_id);
                            inst["region"] = serde_json::json!(&mgr.region);
                            inst["host"] = serde_json::json!(&mgr.ip);
                            all.push(inst);
                        }
                    }
                }
            }
            // Manager unreachable — skip; don't fail the whole call.
            Err(_) => {}
        }
    }

    Ok(Json(serde_json::json!({ "databases": all, "count": all.len() })))
}

/// POST /databases — ask a specific manager to provision a new MTPDB instance.
async fn provision_database(
    State(state): State<SharedState>,
    Json(req): Json<ProvisionDbRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mgr = {
        let st = state.read().await;
        st.managers.get(&req.server_id).cloned()
    };
    let mgr = mgr.ok_or_else(|| not_found(format!("No manager for server_id '{}'", req.server_id)))?;

    if mgr.status != "online" {
        return Err(api_err(format!(
            "Manager '{}' is not online (status: {})",
            req.server_id, mgr.status
        )));
    }

    let db_name = req
        .db_name
        .unwrap_or_else(|| format!("db_{}", &Uuid::new_v4().to_string()[..8]));

    let client = Client::new();
    let url = format!("http://{}:{}/provision", mgr.ip, mgr.manager_port);

    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "db_name": db_name }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| api_err(format!("Could not reach manager: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err((status, Json(serde_json::json!({ "error": body }))));
    }

    let mut result = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| api_err(format!("Bad response from manager: {}", e)))?;

    // Annotate the response with routing info so the caller has everything
    // needed to connect.
    result["server_id"] = serde_json::json!(&mgr.server_id);
    result["region"] = serde_json::json!(&mgr.region);
    result["host"] = serde_json::json!(&mgr.ip);

    Ok(Json(result))
}

/// GET /databases/:id — find the instance across all managers.
async fn get_database(
    State(state): State<SharedState>,
    Path(db_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let managers: Vec<ManagerEntry> = {
        let st = state.read().await;
        st.managers.values().cloned().collect()
    };

    let client = Client::new();
    for mgr in &managers {
        let url = format!("http://{}:{}/instances/{}", mgr.ip, mgr.manager_port, db_id);
        match client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let mut result = resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| api_err(format!("Bad response: {}", e)))?;
                result["server_id"] = serde_json::json!(&mgr.server_id);
                result["region"] = serde_json::json!(&mgr.region);
                result["host"] = serde_json::json!(&mgr.ip);
                return Ok(Json(result));
            }
            _ => continue,
        }
    }

    Err(not_found(format!("Database '{}' not found", db_id)))
}

/// DELETE /databases/:id — stop and remove an instance.
async fn delete_database(
    State(state): State<SharedState>,
    Path(db_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let managers: Vec<ManagerEntry> = {
        let st = state.read().await;
        st.managers.values().cloned().collect()
    };

    let client = Client::new();
    for mgr in &managers {
        let url = format!("http://{}:{}/instances/{}", mgr.ip, mgr.manager_port, db_id);
        match client
            .delete(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                return Ok(Json(serde_json::json!({ "status": "deleted", "id": db_id })));
            }
            _ => continue,
        }
    }

    Err(not_found(format!("Database '{}' not found", db_id)))
}

// ── SSH provisioning ──────────────────────────────────────────────────────────

/// Connect to a remote server via SSH and start mtpdb_manager.
///
/// The manager is started with `nohup … &` so it outlives the SSH session.
/// It will call POST /internal/register on this master once it's listening.
fn ssh_start_manager(
    ip: &str,
    username: &str,
    password: &str,
    server_id: &str,
    manager_port: u16,
    region: &str,
    master_url: &str,
    binary_path: &str,
) -> Result<(), String> {
    let addr = format!("{}:22", ip);
    let tcp = TcpStream::connect(&addr)
        .map_err(|e| format!("TCP connect to {} failed: {}", addr, e))?;

    let mut sess = ssh2::Session::new()
        .map_err(|e| format!("SSH session error: {}", e))?;
    sess.set_tcp_stream(tcp);
    sess.handshake()
        .map_err(|e| format!("SSH handshake failed: {}", e))?;
    sess.userauth_password(username, password)
        .map_err(|e| format!("SSH auth failed: {}", e))?;

    if !sess.authenticated() {
        return Err("SSH authentication was rejected".to_string());
    }

    // Build the remote launch command.
    // Logs go to /tmp/mtpdb_manager_<server_id>.log for easy debugging.
    let log_path = format!("/tmp/mtpdb_manager_{}.log", server_id);
    let cmd = format!(
        "nohup {binary} \
            --server-id {server_id} \
            --master-url {master_url} \
            --port {manager_port} \
            --region {region} \
            > {log_path} 2>&1 &",
        binary = binary_path,
        server_id = server_id,
        master_url = master_url,
        manager_port = manager_port,
        region = region,
        log_path = log_path,
    );

    let mut channel = sess
        .channel_session()
        .map_err(|e| format!("SSH channel failed: {}", e))?;
    channel
        .exec(&cmd)
        .map_err(|e| format!("SSH exec failed: {}", e))?;
    channel
        .wait_close()
        .map_err(|e| format!("SSH channel close error: {}", e))?;

    // nohup + & means the remote command itself exits 0 immediately.
    let exit = channel
        .exit_status()
        .map_err(|e| format!("Could not read exit status: {}", e))?;
    if exit != 0 {
        return Err(format!("Remote command exited with status {}", exit));
    }

    println!(
        "[master] SSH command sent to {}; manager pid running in background. \
         Logs: {}:{}",
        ip, ip, log_path
    );
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Best-effort detection of our outbound IP address.
/// Used when MASTER_ADVERTISE_URL is not set.
fn local_ip_best_effort() -> String {
    use std::net::UdpSocket;
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("MASTER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7000);

    let state: SharedState = Arc::new(RwLock::new(MasterState::default()));

    let app = Router::new()
        // ── health ────────────────────────────────────────────────────────
        .route("/health", get(health))
        // ── server/manager management ─────────────────────────────────────
        .route("/servers", get(list_servers))
        .route("/servers", post(provision_server))
        // ── database instance management ──────────────────────────────────
        .route("/databases", get(list_databases))
        .route("/databases", post(provision_database))
        .route("/databases/:id", get(get_database))
        .route("/databases/:id", delete(delete_database))
        // ── internal: manager → master ────────────────────────────────────
        .route("/internal/register", post(manager_register))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("╔══════════════════════════════════════════════════╗");
    println!("║           mtpdb_master  v0.1.0                   ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!("Listening on http://{}", addr);
    println!();
    println!("  POST /servers              provision a manager via SSH");
    println!("  GET  /servers              list managed servers");
    println!("  POST /databases            provision a new DB instance");
    println!("  GET  /databases            list all DB instances");
    println!("  GET  /databases/:id        get instance details");
    println!("  DELETE /databases/:id      stop & remove instance");
    println!("  POST /internal/register    [manager use only]");
    println!();
    println!("  Set MASTER_ADVERTISE_URL=http://<your-ip>:7000");
    println!("  Set MASTER_PORT=<port> to change listen port");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
