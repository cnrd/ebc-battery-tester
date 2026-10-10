//! Best-effort LAN advertisement; never owns device or HTTP lifecycle.

use std::net::SocketAddr;
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, ServiceDaemon, ServiceInfo};

use crate::core::MachineApiInfo;
use crate::mdns::{MDNS_SERVICE_TYPE, MDNS_TXT_API, MDNS_TXT_ID};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn parse_enabled(value: Option<&str>) -> Result<bool, String> {
    value
        .unwrap_or("true")
        .parse()
        .map_err(|error| format!("invalid EBC_MDNS boolean: {error}"))
}

fn wildcard_interface(addr: SocketAddr) -> Option<IfKind> {
    addr.ip().is_unspecified().then_some(if addr.is_ipv4() {
        IfKind::IPv4
    } else {
        IfKind::IPv6
    })
}

fn publication_interface(addr: SocketAddr) -> IfKind {
    if let Some(family) = wildcard_interface(addr) {
        return family;
    }
    if let SocketAddr::V6(v6) = addr
        && v6.scope_id() != 0
    {
        return IfKind::IndexV6(v6.scope_id());
    }
    IfKind::Addr(addr.ip())
}

fn service_info(addr: SocketAddr, machine: &MachineApiInfo) -> Result<ServiceInfo, String> {
    let id = machine
        .instance_id
        .as_deref()
        .ok_or("missing server instance identity")?;
    let name = format!(
        "EBC Battery Tester {}",
        id.get(..8).ok_or("invalid server instance identity")?
    );
    let api = machine.api_version.to_string();
    let addresses = if addr.ip().is_unspecified() {
        String::new()
    } else {
        addr.ip().to_string()
    };
    let mut info = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &name,
        &format!("ebc-{id}.local."),
        addresses.as_str(),
        addr.port(),
        &[(MDNS_TXT_ID, id), (MDNS_TXT_API, api.as_str())][..],
    )
    .map_err(|error| error.to_string())?;
    if let Some(interface) = wildcard_interface(addr) {
        info = info.enable_addr_auto();
        info.set_interfaces(vec![interface]);
    }
    // Conflict resolution changes DNS names, never the identity in TXT.
    info.set_requires_probe(true);
    Ok(info)
}

fn optional_start<T>(
    enabled: bool,
    addr: SocketAddr,
    start: impl FnOnce() -> Result<T, String>,
) -> Option<T> {
    if !enabled {
        return None;
    }
    if addr.ip().is_loopback() {
        log::info!(
            "mDNS advertisement skipped: loopback-only HTTP bind {addr} is not reachable from the LAN"
        );
        return None;
    }
    if let SocketAddr::V6(v6) = addr
        && v6.ip().to_ipv4_mapped().is_some()
    {
        log::info!(
            "mDNS advertisement skipped: IPv4-mapped IPv6 bind {addr} is not a supported LAN advertisement address"
        );
        return None;
    }
    match start() {
        Ok(advertisement) => Some(advertisement),
        Err(error) => {
            log::warn!("mDNS advertisement unavailable; HTTP remains available: {error}");
            None
        }
    }
}

pub(super) struct Advertisement {
    daemon: Option<ServiceDaemon>,
    fullname: String,
    monitor: Option<tokio::task::JoinHandle<()>>,
}

impl Advertisement {
    pub(super) fn start(enabled: bool, addr: SocketAddr, machine: &MachineApiInfo) -> Option<Self> {
        optional_start(enabled, addr, || {
            let info = service_info(addr, machine)?;
            let mut advertisement = Self {
                daemon: Some(ServiceDaemon::new().map_err(|error| error.to_string())?),
                fullname: info.get_fullname().to_owned(),
                monitor: None,
            };
            let daemon = advertisement.daemon.as_ref().ok_or("missing mDNS daemon")?;
            // Service-level filters govern auto-address insertion, not all publication
            // interfaces. Restrict the daemon too, preserving concrete IPv6 scope.
            daemon
                .disable_interface(IfKind::All)
                .map_err(|error| error.to_string())?;
            daemon
                .enable_interface(publication_interface(addr))
                .map_err(|error| error.to_string())?;
            // Loopback is enabled by default in mdns-sd 0.21. Exclude it from
            // both initial auto-selection and subsequent interface updates.
            daemon
                .disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])
                .map_err(|error| error.to_string())?;
            let events = daemon.monitor().map_err(|error| error.to_string())?;
            advertisement.monitor = Some(tokio::spawn(async move {
                while let Ok(event) = events.recv_async().await {
                    match event {
                        DaemonEvent::Error(error) => log::warn!("mDNS daemon: {error}"),
                        DaemonEvent::NameChange(change) => log::warn!(
                            "mDNS name conflict: {} -> {} on {} (installation identity unchanged)",
                            change.original,
                            change.new_name,
                            change.intf_name
                        ),
                        other => log::debug!("mDNS daemon: {other:?}"),
                    }
                }
            }));
            daemon.register(info).map_err(|error| error.to_string())?;
            Ok(advertisement)
        })
    }

    pub(super) async fn shutdown(&mut self) {
        let Some(daemon) = self.daemon.as_ref() else {
            return;
        };
        match daemon.unregister(&self.fullname) {
            Ok(status) => match tokio::time::timeout(SHUTDOWN_TIMEOUT, status.recv_async()).await {
                Ok(Ok(_status)) => {}
                Ok(Err(error)) => log::warn!("mDNS unregister response failed: {error}"),
                Err(_elapsed) => log::warn!("mDNS unregister timed out"),
            },
            Err(error) => log::warn!("mDNS unregister failed: {error}"),
        }
        match daemon.shutdown() {
            Ok(status) => match tokio::time::timeout(SHUTDOWN_TIMEOUT, status.recv_async()).await {
                Ok(Ok(_status)) => {}
                Ok(Err(error)) => log::warn!("mDNS shutdown response failed: {error}"),
                Err(_elapsed) => log::warn!("mDNS shutdown timed out"),
            },
            Err(error) => log::warn!("mDNS shutdown failed: {error}"),
        }
        self.daemon = None;
        if let Some(monitor) = self.monitor.take() {
            monitor.abort();
            let _joined = monitor.await;
        }
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Also cover partial startup failures and cancellation of run().
        if let Some(daemon) = self.daemon.take() {
            let _unregistered = daemon.unregister(&self.fullname);
            let _shutdown = daemon.shutdown();
        }
        if let Some(monitor) = self.monitor.take() {
            monitor.abort();
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test setup and assertions should fail fast"
)]
mod tests {
    use super::*;

    fn machine() -> MachineApiInfo {
        MachineApiInfo::for_instance("7f7fb259-89ef-49c2-a545-40ecf8d63e22".to_owned())
    }

    #[test]
    fn boolean_config_defaults_enabled_and_rejects_invalid_values() {
        assert_eq!(parse_enabled(None), Ok(true));
        assert_eq!(parse_enabled(Some("true")), Ok(true));
        assert_eq!(parse_enabled(Some("false")), Ok(false));
        assert!(parse_enabled(Some("yes")).unwrap_err().contains("EBC_MDNS"));
    }

    #[test]
    fn advertisement_contract_and_addresses() {
        for (address, auto, family) in [
            ("0.0.0.0:1234", true, 4),
            ("[::]:5678", true, 6),
            ("192.0.2.5:9012", false, 4),
            ("[2001:db8::5]:9012", false, 6),
        ] {
            let addr: SocketAddr = address.parse().unwrap();
            let info = service_info(addr, &machine()).unwrap();
            assert_eq!(info.get_type(), MDNS_SERVICE_TYPE);
            assert_eq!(
                info.get_fullname(),
                format!("EBC Battery Tester 7f7fb259.{MDNS_SERVICE_TYPE}")
            );
            assert_eq!(
                info.get_hostname(),
                "ebc-7f7fb259-89ef-49c2-a545-40ecf8d63e22.local."
            );
            assert_eq!(info.get_port(), addr.port());
            assert_eq!(
                info.get_property_val_str(MDNS_TXT_ID),
                machine().instance_id.as_deref()
            );
            assert_eq!(info.get_property_val_str(MDNS_TXT_API), Some("1"));
            assert_eq!(info.get_properties().len(), 2);
            assert!(info.requires_probe());
            assert_eq!(info.is_addr_auto(), auto);
            if auto {
                assert!(info.get_addresses().is_empty());
                assert!(matches!(
                    (family, wildcard_interface(addr)),
                    (4, Some(IfKind::IPv4)) | (6, Some(IfKind::IPv6))
                ));
            } else {
                assert_eq!(info.get_addresses().len(), 1);
                assert!(info.get_addresses().contains(&addr.ip()));
                assert!(wildcard_interface(addr).is_none());
            }
            assert_eq!(if addr.is_ipv4() { 4 } else { 6 }, family);
        }
    }

    #[test]
    fn disabled_and_loopback_do_not_start_daemon_and_errors_are_nonfatal() {
        for (enabled, address) in [
            (false, "0.0.0.0:8080"),
            (true, "127.0.0.2:8080"),
            (true, "[::1]:8080"),
            (true, "[::ffff:127.0.0.1]:8080"),
            (true, "[::ffff:192.168.1.2]:8080"),
        ] {
            assert!(
                optional_start::<()>(enabled, address.parse().unwrap(), || panic!(
                    "must not start"
                ))
                .is_none()
            );
        }
        assert!(
            optional_start::<()>(true, "0.0.0.0:8080".parse().unwrap(), || Err(
                "injected failure".to_owned()
            ))
            .is_none()
        );
    }

    #[test]
    fn publication_is_family_or_concrete_interface_scoped() {
        assert!(matches!(
            publication_interface("0.0.0.0:8080".parse().unwrap()),
            IfKind::IPv4
        ));
        assert!(matches!(
            publication_interface("[::]:8080".parse().unwrap()),
            IfKind::IPv6
        ));
        let concrete: SocketAddr = "192.0.2.5:8080".parse().unwrap();
        assert!(matches!(publication_interface(concrete), IfKind::Addr(ip) if ip == concrete.ip()));
        let scoped = SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1234".parse().unwrap(),
            8080,
            0,
            7,
        ));
        assert!(matches!(publication_interface(scoped), IfKind::IndexV6(7)));
    }

    #[tokio::test]
    async fn advertisement_uses_dynamic_bound_port() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        assert_ne!(bound.port(), 0);
        assert_eq!(
            service_info(bound, &machine()).unwrap().get_port(),
            bound.port()
        );
    }
}
