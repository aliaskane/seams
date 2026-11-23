//! LAN device discovery over mDNS. Each device advertises `_seams._udp.local.` with
//! its identity in the TXT record; browsing yields named peer announcements. The
//! Android shell must hold a `WifiManager.MulticastLock` while this runs.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use seams_identity::DeviceId;
use seams_proto::PROTOCOL_VERSION;
use thiserror::Error;

pub const SERVICE_TYPE: &str = "_seams._udp.local.";

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("mdns daemon failure: {0}")]
    Mdns(String),
    #[error("announcement is missing or malformed: {0}")]
    Invalid(String),
}

/// A device seen on the LAN. Shown in the UI only while the announcement is fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAnnouncement {
    pub device_id: DeviceId,
    pub name: String,
    pub protocol_version: u32,
    pub addr: IpAddr,
    pub port: u16,
}

/// TXT record contents advertised alongside the service.
pub fn txt_records(device_id: &DeviceId, name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("v".to_string(), "1".to_string()),
        ("id".to_string(), device_id.to_string()),
        ("name".to_string(), name.to_string()),
        ("pv".to_string(), PROTOCOL_VERSION.to_string()),
    ])
}

/// Parses the TXT fields of an announcement; keeps parsing logic testable without a
/// real network.
pub fn parse_txt(
    properties: &BTreeMap<String, String>,
) -> Result<(DeviceId, String, u32), DiscoveryError> {
    let get = |key: &str| -> Result<String, DiscoveryError> {
        properties
            .get(key)
            .cloned()
            .ok_or_else(|| DiscoveryError::Invalid(format!("missing TXT key `{key}`")))
    };

    let id = DeviceId::from_base32(&get("id")?)
        .ok_or_else(|| DiscoveryError::Invalid("device id is not valid base32".into()))?;
    let name = get("name")?;
    let protocol_version = get("pv")?
        .parse()
        .map_err(|_| DiscoveryError::Invalid("protocol version is not a number".into()))?;

    Ok((id, name, protocol_version))
}

/// Advertises this device on the LAN and receives announcements of other devices.
pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
    instance_name: String,
}

impl MdnsDiscovery {
    /// Starts advertising on `port` and browsing for peers.
    pub fn start(device_id: &DeviceId, name: &str, port: u16) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Mdns(e.to_string()))?;

        let instance_name = format!("{device_id}.{SERVICE_TYPE}");
        let host_name = format!("seams-{}.local.", device_id.short());
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_name,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port,
            Some(txt_records(device_id, name).into_iter().collect()),
        )
        .map_err(|e| DiscoveryError::Mdns(e.to_string()))?;

        daemon
            .register(service)
            .map_err(|e| DiscoveryError::Mdns(e.to_string()))?;

        let receiver = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| DiscoveryError::Mdns(e.to_string()))?;

        Ok(Self {
            daemon,
            receiver,
            instance_name,
        })
    }

    /// Blocks up to `timeout` for the next resolved announcement of a peer device.
    pub fn next_peer(&self, timeout: Duration) -> Option<PeerAnnouncement> {
        loop {
            match self.receiver.recv_timeout(timeout) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    return peer_from_resolved(&info).ok();
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Stops advertising and browsing. Best effort.
    pub fn stop(self) {
        let _ = self.daemon.unregister(&self.instance_name);
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

fn peer_from_resolved(info: &ServiceInfo) -> Result<PeerAnnouncement, DiscoveryError> {
    let properties = info.get_properties();
    let mut map = BTreeMap::new();
    for key in ["v", "id", "name", "pv"] {
        if let Some(value) = properties.get_property_val_str(key) {
            map.insert(key.to_string(), value.to_string());
        }
    }
    let (device_id, name, protocol_version) = parse_txt(&map)?;

    let addr = info
        .get_addresses()
        .iter()
        .find(|ip| !ip.is_loopback())
        .copied()
        .or_else(|| info.get_addresses().iter().copied().next())
        .ok_or_else(|| DiscoveryError::Invalid("announcement carries no address".into()))?;

    Ok(PeerAnnouncement {
        device_id,
        name,
        protocol_version,
        addr,
        port: info.get_port(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use seams_identity::DeviceIdentity;

    #[test]
    fn txt_roundtrip() {
        let device = DeviceIdentity::from_seed([41u8; 32]);
        let id = device.device_id();

        let records = txt_records(&id, "Kitchen laptop");
        let (parsed_id, parsed_name, pv) = parse_txt(&records).unwrap();
        assert_eq!(parsed_id, id);
        assert_eq!(parsed_name, "Kitchen laptop");
        assert_eq!(pv, PROTOCOL_VERSION);
    }

    #[test]
    fn parse_rejects_broken_records() {
        let device = DeviceIdentity::from_seed([42u8; 32]);
        let mut records = txt_records(&device.device_id(), "x");
        records.remove("id");
        assert!(matches!(parse_txt(&records), Err(DiscoveryError::Invalid(_))));

        let mut records = txt_records(&device.device_id(), "x");
        records.insert("pv".to_string(), "banana".to_string());
        assert!(matches!(parse_txt(&records), Err(DiscoveryError::Invalid(_))));
    }

    /// Requires a working multicast loopback; run explicitly with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn two_devices_see_each_other_on_loopback() {
        let a = DeviceIdentity::from_seed([43u8; 32]);
        let b = DeviceIdentity::from_seed([44u8; 32]);

        let discovery_a = MdnsDiscovery::start(&a.device_id(), "A", 51_001).unwrap();
        let discovery_b = MdnsDiscovery::start(&b.device_id(), "B", 51_002).unwrap();

        let seen = discovery_a
            .next_peer(Duration::from_secs(5))
            .expect("device A should discover device B");
        assert_eq!(seen.device_id, b.device_id());
        assert_eq!(seen.name, "B");

        discovery_a.stop();
        discovery_b.stop();
    }
}
