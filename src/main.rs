#![allow(unused_imports)]
#![allow(non_camel_case_types)]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::process;
use std::net::SocketAddr;
use std::fs;
use std::path::Path;
use std::io::{BufReader, Write, BufRead};

use bytes::{Bytes, BytesMut};
use log::{debug, error, info, LevelFilter};
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time;
use env_logger::Builder;
use socket2;
use serde_json;
use toml;

// Command line arguments
#[derive(Debug, Serialize, Deserialize)]
enum Command {
    SET { key: String, value: Vec<u8> },
    GET { key: String },
    DEL { keys: Vec<String> },
    EXISTS { key: String },
    CLUSTER_JOIN { address: String },
    CLUSTER_SLOTS,
}

// Define response types for our protocol
#[derive(Debug, Serialize, Deserialize)]
enum Response {
    Success,
    Error(String),
    Data(Vec<u8>),
    Exists(bool),
    Slots(String),
}

// Custom error type
#[derive(Error, Debug)]
enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    
    #[error("Compression error: {0}")]
    Compression(String),
    
    #[error("Key not found: {0}")]
    KeyNotFound(String),
}

// Cache entry structure
struct CacheEntry {
    compressed_data: Bytes,
}

// Cluster state and slot management
const TOTAL_SLOTS: usize = 16384;
const SLOTS_FILE: &str = "slots.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeSlots {
    address: String,
    slot_range: (usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterState {
    nodes: Vec<String>, // list of node addresses
    slot_map: Vec<NodeSlots>, // slot assignments
}

impl ClusterState {
    fn new(self_addr: String) -> Self {
        let mut state = ClusterState {
            nodes: vec![self_addr],
            slot_map: vec![],
        };
        state.rebalance_slots();
        state
    }

    fn rebalance_slots(&mut self) {
        self.slot_map.clear();
        let n = self.nodes.len();
        if n == 0 { return; }
        let base = TOTAL_SLOTS / n;
        let extra = TOTAL_SLOTS % n;
        let mut slots = 0;
        // The first 'extra' nodes get (base+1) slots, the rest get 'base' slots
        for (i, addr) in self.nodes.iter().enumerate() {
            let count = if i < extra { base + 1 } else { base };
            let start = slots;
            let end = slots + count - 1;
            self.slot_map.push(NodeSlots {
                address: addr.clone(),
                slot_range: (start, end),
            });
            slots += count;
        }
        // Ensure all slots are covered
        assert_eq!(slots, TOTAL_SLOTS);
    }

    fn write_slots_file(&self) {
        let _ = fs::write(SLOTS_FILE, serde_json::to_string_pretty(&self.slot_map).unwrap());
    }

    fn add_node(&mut self, addr: String) {
        if !self.nodes.contains(&addr) {
            self.nodes.push(addr);
            self.nodes.sort(); // keep order stable for slot assignment
            self.rebalance_slots();
            self.write_slots_file();
        }
    }

    fn get_slots_json(&self) -> String {
        serde_json::to_string_pretty(&self.slot_map).unwrap_or_else(|_| "[]".to_string())
    }
}

// Server state
struct ServerState {
    cache: HashMap<String, CacheEntry>,
    cluster: ClusterState,
}

impl ServerState {
    fn new(self_addr: String) -> Self {
        ServerState {
            cache: HashMap::new(),
            cluster: ClusterState::new(self_addr),
        }
    }

    // Compress data using zstd
    fn compress_data(&self, data: &[u8]) -> Result<Bytes, ServerError> {
        let compressed = zstd::encode_all(data, 3)
            .map_err(|e| ServerError::Compression(e.to_string()))?;
        Ok(Bytes::from(compressed))
    }

    // Decompress data using zstd
    fn decompress_data(&self, data: &[u8]) -> Result<Vec<u8>, ServerError> {
        let decompressed = zstd::decode_all(data)
            .map_err(|e| ServerError::Compression(e.to_string()))?;
        Ok(decompressed)
    }
}

// Process client commands
async fn process_command(
    cmd: Command, 
    state: &Arc<RwLock<ServerState>>
) -> Result<Response, ServerError> {
    match cmd {
        Command::SET { key, value } => {
            let mut state = state.write().unwrap();
            let compressed_data = state.compress_data(&value)?;
            let entry = CacheEntry {
                compressed_data,
            };
            state.cache.insert(key, entry);
            Ok(Response::Success)
        },
        Command::GET { key } => {
            let state = state.read().unwrap();
            if let Some(entry) = state.cache.get(&key) {
                let data = state.decompress_data(&entry.compressed_data)?;
                Ok(Response::Data(data))
            } else {
                Err(ServerError::KeyNotFound(key))
            }
        },
        Command::DEL { keys } => {
            let mut state = state.write().unwrap();
            let mut found = false;
            for key in keys {
                if state.cache.remove(&key).is_some() {
                    found = true;
                }
            }
            if found {
                Ok(Response::Success)
            } else {
                Err(ServerError::KeyNotFound("None of the keys found".to_string()))
            }
        },
        Command::EXISTS { key } => {
            let state = state.read().unwrap();
            Ok(Response::Exists(state.cache.contains_key(&key)))
        },
        Command::CLUSTER_JOIN { address } => {
            let mut state = state.write().unwrap();
            state.cluster.add_node(address);
            Ok(Response::Success)
        },
        Command::CLUSTER_SLOTS => {
            let state = state.read().unwrap();
            Ok(Response::Slots(state.cluster.get_slots_json()))
        },
    }
}

// Handle a client connection
async fn handle_client(
    mut socket: TcpStream, 
    state: Arc<RwLock<ServerState>>,
) {
    let (mut reader, mut writer) = socket.split();
    let mut buf = BytesMut::with_capacity(1024 * 1024); // 1MB initial capacity
    loop {
        match reader.read_buf(&mut buf).await {
                    Ok(0) => {
                        // Connection was closed
                        debug!("Client disconnected");
                        break;
                    }
                    Ok(n) => {
                        debug!("Read {n} bytes from client");
                        // Parse the command
                        match serde_json::from_slice::<Command>(&buf[..n]) {
                            Ok(cmd) => {
                                debug!("Received command: {:?}", cmd);
                                // Process the command
                                let response = match process_command(cmd, &state).await {
                                    Ok(resp) => resp,
                                    Err(e) => Response::Error(e.to_string()),
                                };
                                // Serialize and send the response
                                match serde_json::to_vec(&response) {
                                    Ok(data) => {
                                        if let Err(e) = writer.write_all(&data).await {
                                            error!("Failed to write response: {}", e);
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to serialize response: {}", e);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to parse command: {}", e);
                                // Send error response
                                let response = Response::Error(format!("Invalid command: {}", e));
                                match serde_json::to_vec(&response) {
                                    Ok(data) => {
                                        if let Err(e) = writer.write_all(&data).await {
                                            error!("Failed to write error response: {}", e);
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to serialize error response: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        // Clear the buffer for the next command
                        buf.clear();
                    }
                    Err(e) => {
                        error!("Failed to read from socket: {}", e);
                        break;
                    }
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct FluxConfig {
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default = "default_port")]
    port: u16,
}

fn default_bind() -> String { "127.0.0.1".to_string() }
fn default_port() -> u16 { 6214 }

fn read_flux_toml() -> FluxConfig {
    let conf_path = "flux.toml";
    if !Path::new(conf_path).exists() {
        // Create default config if missing
        let default = FluxConfig {
            bind: default_bind(),
            port: default_port(),
        };
        // Write TOML with bind as a quoted string (valid for IP addresses)
        let toml_str = format!("bind = \"127.0.0.1\"\nport = 6214\n");
        let _ = fs::write(conf_path, toml_str);
        return default;
    }
    let content = fs::read_to_string(conf_path).unwrap_or_default();
    match toml::from_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[WARN] Could not parse flux.toml: {e}. Using defaults.");
            FluxConfig::default()
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Read bind IP and port from flux.toml (create if missing)
    let conf = read_flux_toml();
    let node_addr = format!("{}:{}", conf.bind, conf.port);
    
    // Create server state with public address
    let state = Arc::new(RwLock::new(ServerState::new(node_addr.clone())));
    
    // Parse bind address
    let bind_addr = match node_addr.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("Invalid address format - {}: {}", node_addr, e);
            return Ok(());
        }
    };
    
    // Configure TCP socket for large buffers
    let socket_config = socket2::Socket::new(
        match bind_addr {
            SocketAddr::V4(_) => socket2::Domain::IPV4,
            SocketAddr::V6(_) => socket2::Domain::IPV6,
        },
        socket2::Type::STREAM,
        None,
    )?;
    
    // Set socket options for large buffers
    socket_config.set_recv_buffer_size(16 * 1024 * 1024)?; // 16MB buffer
    socket_config.set_send_buffer_size(16 * 1024 * 1024)?;
    
    // Allow address reuse to avoid "address already in use" errors
    socket_config.set_reuse_address(true)?;
    
    // Bind and convert to tokio listener
    socket_config.bind(&bind_addr.into())?;
    socket_config.listen(1024)?; // Allow up to 1024 connections in the queue
    
    let listener = TcpListener::from_std(socket_config.into())?;
    
    // Print startup message
    println!("Flux is running on {}", node_addr);
    
    // Count of active connections
    let mut active_connections = 0;
    
    // Accept connections
    loop {
        match tokio::time::timeout(Duration::from_secs(5), listener.accept()).await {
                    Ok(Ok((socket, addr))) => {
                        active_connections += 1;
                debug!("Accepted connection from: {} (active: {})", addr, active_connections);
                        // Set socket buffer sizes
                        if let Ok(stream) = socket.into_std() {
                            match socket2::Socket::try_from(stream) {
                                Ok(sock) => {
                                    // Set large buffer sizes for this connection
                                    let _ = sock.set_recv_buffer_size(16 * 1024 * 1024);
                                    let _ = sock.set_send_buffer_size(16 * 1024 * 1024);
                                    // Convert back to tokio socket
                                    if let Ok(socket) = TcpStream::from_std(sock.into()) {
                                // Clone state for the new task
                                        let state = state.clone();
                                        // Spawn a new task to handle the connection
                                        tokio::spawn(async move {
                                    handle_client(socket, state).await;
                                            debug!("Client handler task completed for {}", addr);
                                        });
                                    } else {
                                        error!("Failed to convert socket back to TcpStream");
                                    }
                                },
                                Err(e) => {
                                    error!("Failed to convert to socket2::Socket: {}", e);
                                }
                            }
                        } else {
                            error!("Failed to get standard socket from TcpStream");
                        }
                    }
                    Ok(Err(e)) => {
                        error!("Failed to accept connection: {}", e);
                    }
                    Err(_) => {
                        // Timeout occurred, just continue
                        debug!("Accept timed out, checking system state");
                    }
                }
            }
}
