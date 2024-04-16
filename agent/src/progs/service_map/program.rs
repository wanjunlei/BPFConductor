use std::cmp::PartialEq;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use async_trait::async_trait;
use aya::maps::{HashMap as AyaHashMap, Map, MapData};
use bpfman_lib::directories::RTDIR_FS_MAPS;
use log::{debug, error, info};
use parking_lot::RwLock;
use prometheus_client::encoding::DescriptorEncoder;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tokio::time;

use agent_api::v1::{BytecodeLocation, ProgramInfo};
use conn_tracer_common::{
    ConnectionKey, ConnectionStats, CONNECTION_ROLE_CLIENT, CONNECTION_ROLE_SERVER,
    CONNECTION_ROLE_UNKNOWN,
};

use crate::common::constants::METRICS_INTERVAL;
use crate::common::types::{ProgramState, ProgramType};
use crate::errors::ParseError;
use crate::managers::cache::{CacheManager, Workload};
use crate::progs::types::{Program, ShutdownSignal};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Connection {
    client: Arc<Workload>,
    server: Arc<Workload>,
    role: u32,
    server_port: u32,
}

#[derive(Debug)]
struct Inner {
    name: String,
    program_type: ProgramType,
    program_state: ProgramState,
    metadata: HashMap<String, String>,
    current_conns_map: Option<AyaHashMap<MapData, ConnectionKey, ConnectionStats>>,
    past_conns_map: HashMap<Connection, u64>,
    cache_mgr: Option<CacheManager>,
}

impl Inner {
    fn new() -> Self {
        Self {
            name: "service_map".to_string(),
            program_type: ProgramType::Builtin,
            program_state: ProgramState::Uninitialized,
            metadata: HashMap::new(),
            current_conns_map: None,
            past_conns_map: HashMap::new(),
            cache_mgr: None,
        }
    }
}

#[derive(Debug)]
pub struct ServiceMap {
    inner: Arc<RwLock<Inner>>,
}

impl ServiceMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::new())),
        }
    }

    async fn reset(&self) -> Result<(), Error> {
        let mut inner = self.inner.write();

        inner.current_conns_map = None;
        inner.past_conns_map.clear();
        inner.metadata.clear();

        inner.program_state = ProgramState::Uninitialized;

        info!("ServiceMap has been cleaned up and reset to uninitialized state.");

        Ok(())
    }

    fn poll(&self) -> Result<HashMap<Connection, u64>, Error> {
        let inner = self.inner.read();
        let mut keys_to_remove = Vec::new();
        let mut current_conns: HashMap<Connection, u64> = HashMap::new();

        let tcp_conns_map = inner
            .current_conns_map
            .as_ref()
            .ok_or(Error::msg("No current connections map"))?;
        for item in tcp_conns_map.iter() {
            let (key, stats) = item?;
            if stats.is_active != 1 {
                keys_to_remove.push(key);
                continue;
            }
            if key.src_addr == key.dest_addr || self.is_loopback_address(key.dest_addr) {
                continue;
            }
            if key.role == CONNECTION_ROLE_UNKNOWN {
                continue;
            }

            if let Ok(connection) = self.build_connection(key) {
                current_conns
                    .entry(connection.clone())
                    .and_modify(|e| *e += stats.bytes_sent)
                    .or_insert(stats.bytes_sent);
            }
        }

        let past_conns_map = inner.past_conns_map.clone();
        for (conn, bytes_sent) in past_conns_map.iter() {
            current_conns
                .entry(conn.clone())
                .and_modify(|e| *e += *bytes_sent)
                .or_insert(*bytes_sent);
        }

        for key in keys_to_remove {
            let _ = self.handle_inactive_connection(key);
        }

        Ok(current_conns)
    }

    fn resolve_ip(&self, ip: u32) -> Option<Arc<Workload>> {
        let inner = self.inner.read();
        let cache_mgr_ref = inner.cache_mgr.as_ref()?;
        let ip_to_workload_lock = cache_mgr_ref.ip_to_workload.clone();
        let ip_to_workload = ip_to_workload_lock.read();
        let ip_addr = Ipv4Addr::from(ip);
        let ip_string = ip_addr.to_string();
        ip_to_workload.get(&ip_string).cloned()
    }

    fn build_connection(&self, key: ConnectionKey) -> Result<Connection, Error> {
        let client_workload = self.resolve_ip(key.src_addr).ok_or(Error::msg(format!(
            "Unknown IP: {}",
            Ipv4Addr::from(key.src_addr)
        )))?;
        let server_workload = self.resolve_ip(key.dest_addr).ok_or(Error::msg(format!(
            "Unknown IP: {}",
            Ipv4Addr::from(key.dest_addr)
        )))?;

        let (client, server, port) = match key.role {
            CONNECTION_ROLE_CLIENT => (client_workload, server_workload, key.dest_port),
            CONNECTION_ROLE_SERVER => (server_workload, client_workload, key.src_port),
            _ => return Err(Error::msg("Unknown connection role")),
        };

        Ok(Connection {
            client,
            server,
            role: key.role,
            server_port: port,
        })
    }

    fn handle_inactive_connection(&self, key: ConnectionKey) -> Result<(), Error> {
        let mut inner = self.inner.write();
        let tcp_conns_map = inner
            .current_conns_map
            .as_mut()
            .ok_or(Error::msg("No current connections map"))?;
        let throughput = match tcp_conns_map.get(&key, 0) {
            Ok(stats) => stats.bytes_sent,
            Err(_) => 0,
        };

        tcp_conns_map.remove(&key)?;

        let mut past_conns_map = inner.past_conns_map.clone();
        let connection = self.build_connection(key)?;
        past_conns_map
            .entry(connection)
            .and_modify(|e| *e += throughput)
            .or_insert(throughput);
        Ok(())
    }

    fn is_loopback_address(&self, addr: u32) -> bool {
        let ip_addr = Ipv4Addr::from(addr);
        ip_addr.is_loopback()
    }
}

#[async_trait]
impl Program for ServiceMap {
    fn init(
        &self,
        metadata: HashMap<String, String>,
        cache_manager: CacheManager,
        maps: HashMap<String, u32>,
    ) -> Result<(), Error> {
        let mut inner = self.inner.write();
        inner.metadata = metadata;
        inner.cache_mgr = Some(cache_manager);

        let map_name = "CONNECTIONS";
        let prog_id = maps.get(map_name).ok_or(anyhow::anyhow!(
            "No map named CONNECTIONS in the provided maps"
        ))?;
        let bpfman_maps = Path::new(RTDIR_FS_MAPS);
        if !bpfman_maps.exists() {
            return Err(anyhow::anyhow!("{} does not exist", RTDIR_FS_MAPS));
        }

        let map_pin_path = bpfman_maps.join(format!("{}/{}", prog_id, map_name));
        let map_data = MapData::from_pin(map_pin_path)
            .map_err(|_| anyhow::anyhow!("No maps named CONNECTIONS"))?;
        let tcp_conns_map: AyaHashMap<MapData, ConnectionKey, ConnectionStats> =
            Map::HashMap(map_data)
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to convert map"))?;
        inner.current_conns_map = Some(tcp_conns_map);
        inner.program_state = ProgramState::Initialized;

        Ok(())
    }
    async fn start(
        &self,
        mut shutdown_rx: broadcast::Receiver<ShutdownSignal>,
    ) -> Result<(), Error> {
        self.set_state(ProgramState::Running);
        let mut interval = time::interval(Duration::from_secs(METRICS_INTERVAL));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.poll() {
                        debug!("Error polling: {:?}", e);
                        self.set_state(ProgramState::Failed);
                        return Err(e.into());
                    }
                }
                Ok(signal) = shutdown_rx.recv() => {
                    match signal {
                        ShutdownSignal::All => {
                            info!("Shutting down all programs");
                            break;
                        },
                        ShutdownSignal::ProgramName(name) if name == self.get_name() => {
                            info!("Stopping program {}", self.get_name());
                            break;
                        },
                        _ => {}
                    }
                },
            }
        }

        self.reset().await?;

        Ok(())
    }

    fn collect(&self, mut encoder: &DescriptorEncoder) -> Result<(), Error> {
        Ok(())
    }

    fn get_name(&self) -> String {
        let inner = self.inner.read();
        inner.name.clone()
    }

    fn get_state(&self) -> ProgramState {
        let inner = self.inner.read();
        inner.program_state.clone()
    }

    fn set_state(&self, state: ProgramState) {
        let mut inner = self.inner.write();
        inner.program_state = state
    }

    fn get_type(&self) -> ProgramType {
        let inner = self.inner.read();
        inner.program_type.clone()
    }

    fn get_metadata(&self) -> HashMap<String, String> {
        let inner = self.inner.read();
        inner.metadata.clone()
    }

    fn set_metadata(&self, metadata: HashMap<String, String>) {
        let mut inner = self.inner.write();
        inner.metadata = metadata;
    }

    fn get_program_info(&self) -> Result<ProgramInfo, ParseError> {
        Ok(ProgramInfo {
            name: self.get_name(),
            program_type: self.get_type().try_into()?,
            state: self.get_state().clone().try_into()?,
            bytecode: None,
            metadata: self.get_metadata(),
        })
    }
}
