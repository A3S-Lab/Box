//! Network types for container-to-container communication.
//!
//! Provides network configuration, endpoint tracking, and IP address
//! management (IPAM) for connecting boxes via passt-based virtio-net.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;

/// Network mode for a box.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// TSI mode (default) — no network interfaces, socket syscalls proxied via vsock.
    #[default]
    Tsi,

    /// Bridge mode — real eth0 via passt, container-to-container communication.
    Bridge {
        /// Network name to join.
        network: String,
    },

    /// No networking at all.
    None,
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkMode::Tsi => write!(f, "tsi"),
            NetworkMode::Bridge { network } => write!(f, "bridge:{}", network),
            NetworkMode::None => write!(f, "none"),
        }
    }
}

/// Configuration for a user-defined network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Network name (unique identifier).
    pub name: String,

    /// Subnet in CIDR notation (e.g., "10.88.0.0/24").
    pub subnet: String,

    /// Gateway IP address (e.g., "10.88.0.1").
    pub gateway: Ipv4Addr,

    /// Network driver (currently only "bridge" is supported).
    #[serde(default = "default_driver")]
    pub driver: String,

    /// User-defined labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Connected endpoints (box_id → endpoint).
    #[serde(default)]
    pub endpoints: HashMap<String, NetworkEndpoint>,

    /// Creation timestamp (RFC 3339).
    pub created_at: String,
}

fn default_driver() -> String {
    "bridge".to_string()
}

/// A box's connection to a network.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkEndpoint {
    /// Box ID.
    pub box_id: String,

    /// Box name (for DNS resolution).
    pub box_name: String,

    /// Assigned IPv4 address.
    pub ip_address: Ipv4Addr,

    /// Assigned MAC address (hex string, e.g., "02:42:0a:58:00:02").
    pub mac_address: String,
}

/// Simple sequential IPAM (IP Address Management) for a subnet.
#[derive(Debug)]
pub struct Ipam {
    /// Network address (e.g., 10.88.0.0).
    network: Ipv4Addr,
    /// Prefix length (e.g., 24).
    prefix_len: u8,
    /// Gateway (first usable, e.g., 10.88.0.1).
    gateway: Ipv4Addr,
}

impl Ipam {
    /// Create a new IPAM from a CIDR string (e.g., "10.88.0.0/24").
    pub fn new(cidr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() != 2 {
            return Err(format!("invalid CIDR notation: {}", cidr));
        }

        let network: Ipv4Addr = parts[0]
            .parse()
            .map_err(|e| format!("invalid network address '{}': {}", parts[0], e))?;
        let prefix_len: u8 = parts[1]
            .parse()
            .map_err(|e| format!("invalid prefix length '{}': {}", parts[1], e))?;

        if prefix_len > 30 {
            return Err(format!(
                "prefix length {} too large (max 30 for usable hosts)",
                prefix_len
            ));
        }

        // Gateway is network + 1
        let net_u32 = u32::from(network);
        let gateway = Ipv4Addr::from(net_u32 + 1);

        Ok(Self {
            network,
            prefix_len,
            gateway,
        })
    }

    /// Get the gateway address.
    pub fn gateway(&self) -> Ipv4Addr {
        self.gateway
    }

    /// Get the subnet CIDR string.
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.network, self.prefix_len)
    }

    /// Calculate the broadcast address.
    pub fn broadcast(&self) -> Ipv4Addr {
        let net_u32 = u32::from(self.network);
        let host_bits = 32 - self.prefix_len as u32;
        let broadcast = net_u32 | ((1u32 << host_bits) - 1);
        Ipv4Addr::from(broadcast)
    }

    /// Total number of usable host addresses (excluding network, gateway, broadcast).
    pub fn capacity(&self) -> u32 {
        let host_bits = 32 - self.prefix_len as u32;
        let total = (1u32 << host_bits) - 1; // exclude network address
        total.saturating_sub(2) // exclude gateway and broadcast
    }

    /// Allocate the next available IP, given a set of already-used IPs.
    pub fn allocate(&self, used: &[Ipv4Addr]) -> Result<Ipv4Addr, String> {
        let net_u32 = u32::from(self.network);
        let broadcast_u32 = u32::from(self.broadcast());
        let gateway_u32 = u32::from(self.gateway);

        // Start from network + 2 (skip network and gateway)
        let mut candidate = net_u32 + 2;
        while candidate < broadcast_u32 {
            if candidate != gateway_u32 {
                let ip = Ipv4Addr::from(candidate);
                if !used.contains(&ip) {
                    return Ok(ip);
                }
            }
            candidate += 1;
        }

        Err("no available IP addresses in subnet".to_string())
    }

    /// Generate a deterministic MAC address from an IPv4 address.
    /// Uses the locally-administered prefix 02:42 (same as Docker).
    pub fn mac_from_ip(ip: &Ipv4Addr) -> String {
        let octets = ip.octets();
        format!(
            "02:42:{:02x}:{:02x}:{:02x}:{:02x}",
            octets[0], octets[1], octets[2], octets[3]
        )
    }
}

impl NetworkConfig {
    /// Create a new network with the given name and subnet.
    pub fn new(name: &str, subnet: &str) -> Result<Self, String> {
        let ipam = Ipam::new(subnet)?;

        Ok(Self {
            name: name.to_string(),
            subnet: ipam.cidr(),
            gateway: ipam.gateway(),
            driver: "bridge".to_string(),
            labels: HashMap::new(),
            endpoints: HashMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Allocate an IP and register a new endpoint for a box.
    pub fn connect(&mut self, box_id: &str, box_name: &str) -> Result<NetworkEndpoint, String> {
        if self.endpoints.contains_key(box_id) {
            return Err(format!("box '{}' is already connected to network '{}'", box_id, self.name));
        }

        let ipam = Ipam::new(&self.subnet)?;
        let used: Vec<Ipv4Addr> = self.endpoints.values().map(|e| e.ip_address).collect();
        let ip = ipam.allocate(&used)?;
        let mac = Ipam::mac_from_ip(&ip);

        let endpoint = NetworkEndpoint {
            box_id: box_id.to_string(),
            box_name: box_name.to_string(),
            ip_address: ip,
            mac_address: mac,
        };

        self.endpoints.insert(box_id.to_string(), endpoint.clone());
        Ok(endpoint)
    }

    /// Remove a box from this network.
    pub fn disconnect(&mut self, box_id: &str) -> Result<NetworkEndpoint, String> {
        self.endpoints
            .remove(box_id)
            .ok_or_else(|| format!("box '{}' is not connected to network '{}'", box_id, self.name))
    }

    /// Get all connected endpoints.
    pub fn connected_boxes(&self) -> Vec<&NetworkEndpoint> {
        self.endpoints.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- NetworkMode tests ---

    #[test]
    fn test_network_mode_default_is_tsi() {
        let mode = NetworkMode::default();
        assert_eq!(mode, NetworkMode::Tsi);
    }

    #[test]
    fn test_network_mode_display() {
        assert_eq!(NetworkMode::Tsi.to_string(), "tsi");
        assert_eq!(NetworkMode::None.to_string(), "none");
        assert_eq!(
            NetworkMode::Bridge {
                network: "mynet".to_string()
            }
            .to_string(),
            "bridge:mynet"
        );
    }

    #[test]
    fn test_network_mode_serialization() {
        let mode = NetworkMode::Bridge {
            network: "test-net".to_string(),
        };
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: NetworkMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mode);
    }

    #[test]
    fn test_network_mode_tsi_serialization() {
        let mode = NetworkMode::Tsi;
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: NetworkMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, NetworkMode::Tsi);
    }

    // --- IPAM tests ---

    #[test]
    fn test_ipam_new_valid() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        assert_eq!(ipam.gateway(), Ipv4Addr::new(10, 88, 0, 1));
        assert_eq!(ipam.cidr(), "10.88.0.0/24");
    }

    #[test]
    fn test_ipam_new_slash16() {
        let ipam = Ipam::new("172.20.0.0/16").unwrap();
        assert_eq!(ipam.gateway(), Ipv4Addr::new(172, 20, 0, 1));
    }

    #[test]
    fn test_ipam_invalid_cidr() {
        assert!(Ipam::new("10.88.0.0").is_err());
        assert!(Ipam::new("not-an-ip/24").is_err());
        assert!(Ipam::new("10.88.0.0/33").is_err());
        assert!(Ipam::new("10.88.0.0/31").is_err());
    }

    #[test]
    fn test_ipam_broadcast() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        assert_eq!(ipam.broadcast(), Ipv4Addr::new(10, 88, 0, 255));

        let ipam16 = Ipam::new("172.20.0.0/16").unwrap();
        assert_eq!(ipam16.broadcast(), Ipv4Addr::new(172, 20, 255, 255));
    }

    #[test]
    fn test_ipam_capacity() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        // /24 = 256 total, minus network(1) minus gateway(1) minus broadcast(1) = 253
        assert_eq!(ipam.capacity(), 253);

        let ipam28 = Ipam::new("10.88.0.0/28").unwrap();
        // /28 = 16 total, minus network(1) = 15, minus gateway(1) minus broadcast(1) = 13
        assert_eq!(ipam28.capacity(), 13);
    }

    #[test]
    fn test_ipam_allocate_first() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        let ip = ipam.allocate(&[]).unwrap();
        // First allocation: network+2 (skip network and gateway)
        assert_eq!(ip, Ipv4Addr::new(10, 88, 0, 2));
    }

    #[test]
    fn test_ipam_allocate_sequential() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        let ip1 = ipam.allocate(&[]).unwrap();
        let ip2 = ipam.allocate(&[ip1]).unwrap();
        let ip3 = ipam.allocate(&[ip1, ip2]).unwrap();

        assert_eq!(ip1, Ipv4Addr::new(10, 88, 0, 2));
        assert_eq!(ip2, Ipv4Addr::new(10, 88, 0, 3));
        assert_eq!(ip3, Ipv4Addr::new(10, 88, 0, 4));
    }

    #[test]
    fn test_ipam_allocate_skips_gateway() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        // Gateway is 10.88.0.1, first alloc should be .2
        let ip = ipam.allocate(&[]).unwrap();
        assert_ne!(ip, ipam.gateway());
    }

    #[test]
    fn test_ipam_allocate_exhausted() {
        let ipam = Ipam::new("10.88.0.0/30").unwrap();
        // /30 = 4 total: .0 (network), .1 (gateway), .2 (host), .3 (broadcast)
        // Only 1 usable host
        let ip1 = ipam.allocate(&[]).unwrap();
        assert_eq!(ip1, Ipv4Addr::new(10, 88, 0, 2));

        let result = ipam.allocate(&[ip1]);
        assert!(result.is_err());
    }

    #[test]
    fn test_ipam_mac_from_ip() {
        let ip = Ipv4Addr::new(10, 88, 0, 2);
        assert_eq!(Ipam::mac_from_ip(&ip), "02:42:0a:58:00:02");

        let ip2 = Ipv4Addr::new(192, 168, 1, 100);
        assert_eq!(Ipam::mac_from_ip(&ip2), "02:42:c0:a8:01:64");
    }

    // --- NetworkConfig tests ---

    #[test]
    fn test_network_config_new() {
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        assert_eq!(net.name, "mynet");
        assert_eq!(net.subnet, "10.88.0.0/24");
        assert_eq!(net.gateway, Ipv4Addr::new(10, 88, 0, 1));
        assert_eq!(net.driver, "bridge");
        assert!(net.endpoints.is_empty());
    }

    #[test]
    fn test_network_config_invalid_subnet() {
        assert!(NetworkConfig::new("bad", "invalid").is_err());
    }

    #[test]
    fn test_network_config_connect() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep = net.connect("box-1", "web").unwrap();

        assert_eq!(ep.box_id, "box-1");
        assert_eq!(ep.box_name, "web");
        assert_eq!(ep.ip_address, Ipv4Addr::new(10, 88, 0, 2));
        assert_eq!(ep.mac_address, "02:42:0a:58:00:02");
        assert_eq!(net.endpoints.len(), 1);
    }

    #[test]
    fn test_network_config_connect_multiple() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep1 = net.connect("box-1", "web").unwrap();
        let ep2 = net.connect("box-2", "api").unwrap();

        assert_eq!(ep1.ip_address, Ipv4Addr::new(10, 88, 0, 2));
        assert_eq!(ep2.ip_address, Ipv4Addr::new(10, 88, 0, 3));
        assert_eq!(net.endpoints.len(), 2);
    }

    #[test]
    fn test_network_config_connect_duplicate() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        let result = net.connect("box-1", "web");
        assert!(result.is_err());
    }

    #[test]
    fn test_network_config_disconnect() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();

        let ep = net.disconnect("box-1").unwrap();
        assert_eq!(ep.box_id, "box-1");
        assert!(net.endpoints.is_empty());
    }

    #[test]
    fn test_network_config_disconnect_not_connected() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let result = net.disconnect("box-1");
        assert!(result.is_err());
    }

    #[test]
    fn test_network_config_connected_boxes() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "api").unwrap();

        let boxes = net.connected_boxes();
        assert_eq!(boxes.len(), 2);
    }

    #[test]
    fn test_network_config_ip_reuse_after_disconnect() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep1 = net.connect("box-1", "web").unwrap();
        assert_eq!(ep1.ip_address, Ipv4Addr::new(10, 88, 0, 2));

        net.disconnect("box-1").unwrap();

        // After disconnect, the IP should be reusable
        let ep2 = net.connect("box-2", "api").unwrap();
        assert_eq!(ep2.ip_address, Ipv4Addr::new(10, 88, 0, 2));
    }

    #[test]
    fn test_network_config_serialization() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();

        let json = serde_json::to_string(&net).unwrap();
        let parsed: NetworkConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, "mynet");
        assert_eq!(parsed.subnet, "10.88.0.0/24");
        assert_eq!(parsed.endpoints.len(), 1);
        assert!(parsed.endpoints.contains_key("box-1"));
    }

    // --- NetworkEndpoint tests ---

    #[test]
    fn test_network_endpoint_serialization() {
        let ep = NetworkEndpoint {
            box_id: "abc123".to_string(),
            box_name: "web".to_string(),
            ip_address: Ipv4Addr::new(10, 88, 0, 2),
            mac_address: "02:42:0a:58:00:02".to_string(),
        };

        let json = serde_json::to_string(&ep).unwrap();
        let parsed: NetworkEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ep);
    }
}
