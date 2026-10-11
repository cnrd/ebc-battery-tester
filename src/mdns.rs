//! Native DNS-SD constants and application-lifetime server discovery.

pub const MDNS_SERVICE_TYPE: &str = "_ebc-battery._tcp.local.";
pub const MDNS_TXT_ID: &str = "id";
pub const MDNS_TXT_API: &str = "api";

#[cfg(feature = "gui")]
pub use discovery::{DiscoveredServer, DiscoverySnapshot, LinkLocalIpv6, NativeDiscovery};

#[cfg(feature = "gui")]
mod discovery {
    use super::{MDNS_SERVICE_TYPE, MDNS_TXT_API, MDNS_TXT_ID};
    use mdns_sd::{DaemonEvent, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
    use std::collections::{BTreeMap, BTreeSet};
    use std::net::{IpAddr, Ipv6Addr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    /// Retained for display only: the HTTP client cannot reliably use scoped URLs.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct LinkLocalIpv6 {
        pub address: Ipv6Addr,
        pub interface_name: String,
        pub interface_index: u32,
    }

    /// Untrusted discovery hints, not proof of identity or API compatibility.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct DiscoveredServer {
        /// Parsed UUID normalized to lowercase hyphenated text.
        pub instance_id: String,
        pub instance_name: String,
        pub hostname: String,
        pub port: u16,
        /// An unsigned advertised version; `/api/info` remains authoritative.
        pub api_version_hint: u32,
        /// Numeric HTTP base URLs, IPv4 first, then global/ULA IPv6.
        pub endpoints: Vec<String>,
        pub link_local_ipv6: Vec<LinkLocalIpv6>,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct DiscoverySnapshot {
        pub servers: Vec<DiscoveredServer>,
        pub warning: Option<String>,
    }

    /// Own this in `MainApp`, not in a connection/backend, to browse continuously.
    pub struct NativeDiscovery {
        state: Arc<Mutex<DiscoverySnapshot>>,
        stop: Arc<AtomicBool>,
        daemon: Option<ServiceDaemon>,
        worker: Option<JoinHandle<()>>,
    }

    impl NativeDiscovery {
        /// Discovery failure is nonfatal and is exposed through the snapshot warning.
        #[expect(
            clippy::needless_pass_by_value,
            reason = "Application-lifetime ownership API"
        )]
        pub fn start(context: egui::Context) -> Self {
            let mut discovery = Self {
                state: Arc::new(Mutex::new(DiscoverySnapshot::default())),
                stop: Arc::new(AtomicBool::new(false)),
                daemon: None,
                worker: None,
            };
            let result = (|| {
                let daemon = ServiceDaemon::new().map_err(|error| error.to_string())?;
                discovery.daemon = Some(daemon);
                let daemon = discovery
                    .daemon
                    .as_ref()
                    .ok_or("Missing discovery daemon")?;
                let monitor = daemon.monitor().map_err(|error| error.to_string())?;
                let events = daemon
                    .browse(MDNS_SERVICE_TYPE)
                    .map_err(|error| error.to_string())?;
                let state = Arc::clone(&discovery.state);
                let stop = Arc::clone(&discovery.stop);
                let repaint = context.clone();
                let worker = thread::Builder::new()
                    .name("ebc-mdns-discovery".into())
                    .spawn(move || {
                        let mut sources = Sources::default();
                        while !stop.load(Ordering::Relaxed) {
                            let mut changed = false;
                            for event in monitor.try_iter() {
                                if let DaemonEvent::Error(error) = event {
                                    set_warning(
                                        &state,
                                        &repaint,
                                        format!("LAN discovery: {error}"),
                                    );
                                }
                            }
                            if monitor.is_disconnected() || events.is_disconnected() {
                                set_warning(
                                    &state,
                                    &repaint,
                                    "LAN discovery stopped unexpectedly".into(),
                                );
                                break;
                            }
                            if let Ok(event) = events.recv_timeout(Duration::from_millis(100)) {
                                match event {
                                    ServiceEvent::ServiceResolved(service) => {
                                        changed = sources.resolve(&service);
                                    }
                                    ServiceEvent::ServiceRemoved(service_type, fullname)
                                        if service_type.eq_ignore_ascii_case(MDNS_SERVICE_TYPE) =>
                                    {
                                        changed = sources.remove(&fullname);
                                    }
                                    ServiceEvent::SearchStopped(_) => {
                                        set_warning(
                                            &state,
                                            &repaint,
                                            "LAN discovery browse stopped".into(),
                                        );
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            if changed {
                                let servers = sources.servers();
                                state
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .servers = servers;
                                repaint.request_repaint();
                            }
                        }
                    })
                    .map_err(|error| error.to_string())?;
                discovery.worker = Some(worker);
                Ok::<(), String>(())
            })();
            if let Err(error) = result {
                set_warning(
                    &discovery.state,
                    &context,
                    format!("LAN discovery unavailable: {error}"),
                );
                discovery.shutdown();
            }
            discovery
        }

        pub fn snapshot(&self) -> DiscoverySnapshot {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn shutdown(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(daemon) = self.daemon.take() {
                drop(daemon.stop_browse(MDNS_SERVICE_TYPE));
                if let Ok(status) = daemon.shutdown() {
                    drop(status.recv_timeout(Duration::from_secs(1)));
                }
            }
            if let Some(worker) = self.worker.take() {
                drop(worker.join());
            }
        }
    }

    impl Drop for NativeDiscovery {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    fn set_warning(state: &Mutex<DiscoverySnapshot>, context: &egui::Context, warning: String) {
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .warning = Some(warning);
        context.request_repaint();
    }

    #[derive(Default)]
    struct Sources {
        // Fullnames identify DNS records; UUIDs identify the logical installation.
        records: BTreeMap<String, DiscoveredServer>,
    }

    impl Sources {
        fn resolve(&mut self, service: &ResolvedService) -> bool {
            let fullname = service.fullname.to_ascii_lowercase();
            if let Some(server) = parse_service(service) {
                if self.records.get(&fullname) == Some(&server) {
                    false
                } else {
                    self.records.insert(fullname, server);
                    true
                }
            } else {
                log::debug!("Ignoring unusable EBC mDNS service {}", service.fullname);
                // A malformed update must not leave an earlier valid record selectable.
                self.remove(&fullname)
            }
        }

        fn remove(&mut self, fullname: &str) -> bool {
            self.records
                .remove(&fullname.to_ascii_lowercase())
                .is_some()
        }

        fn servers(&self) -> Vec<DiscoveredServer> {
            let mut servers: BTreeMap<String, DiscoveredServer> = BTreeMap::new();
            for source in self.records.values() {
                let server = servers
                    .entry(source.instance_id.clone())
                    .or_insert_with(|| source.clone());
                server.endpoints.extend(source.endpoints.iter().cloned());
                server
                    .link_local_ipv6
                    .extend(source.link_local_ipv6.iter().cloned());
            }
            for server in servers.values_mut() {
                // Sort numerically, not lexically (e.g. .2 before .10).
                server
                    .endpoints
                    .sort_by_key(|endpoint| endpoint_key(endpoint));
                server.endpoints.dedup();
                server.link_local_ipv6.sort();
                server.link_local_ipv6.dedup();
            }
            servers.into_values().collect()
        }
    }

    fn endpoint_key(endpoint: &str) -> Option<(IpAddr, u16)> {
        endpoint
            .strip_prefix("http://")?
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|address| (address.ip(), address.port()))
    }

    fn parse_service(service: &ResolvedService) -> Option<DiscoveredServer> {
        if !service.ty_domain.eq_ignore_ascii_case(MDNS_SERVICE_TYPE) || service.port == 0 {
            return None;
        }
        let suffix = format!(".{MDNS_SERVICE_TYPE}");
        if !service.fullname.to_ascii_lowercase().ends_with(&suffix) {
            return None;
        }
        let escaped_name = service
            .fullname
            .get(..service.fullname.len().checked_sub(suffix.len())?)?;
        if escaped_name.is_empty() || service.host.is_empty() {
            return None;
        }
        let id = service.get_property_val_str(MDNS_TXT_ID)?;
        let instance_id = uuid::Uuid::parse_str(id).ok()?.hyphenated().to_string();
        let api = service.get_property_val_str(MDNS_TXT_API)?;
        if api.is_empty() || !api.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let api_version = api.parse::<u32>().ok()?;
        let mut endpoints = BTreeSet::new();
        let mut link_local_ipv6 = BTreeSet::new();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "Results are normalized through ordered sets"
        )]
        for address in &service.addresses {
            let ip = address.to_ip_addr();
            if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
                continue;
            }
            match ip {
                IpAddr::V4(ip) if !ip.is_broadcast() => {
                    endpoints.insert((IpAddr::V4(ip), service.port));
                }
                IpAddr::V6(ip) if ip.is_unicast_link_local() => {
                    if let ScopedIp::V6(scoped) = address {
                        link_local_ipv6.insert(LinkLocalIpv6 {
                            address: ip,
                            interface_name: scoped.scope_id().name.clone(),
                            interface_index: scoped.scope_id().index,
                        });
                    }
                }
                // Global unicast 2000::/3 or unique-local fc00::/7 only.
                IpAddr::V6(ip) if ip.segments()[0] & 0xe000 == 0x2000 || ip.is_unique_local() => {
                    endpoints.insert((IpAddr::V6(ip), service.port));
                }
                _ => {}
            }
        }
        if endpoints.is_empty() && link_local_ipv6.is_empty() {
            return None;
        }
        Some(DiscoveredServer {
            instance_id,
            instance_name: unescape_name(escaped_name),
            hostname: service.host.clone(),
            port: service.port,
            api_version_hint: api_version,
            endpoints: endpoints
                .into_iter()
                .map(|(ip, port)| match ip {
                    IpAddr::V4(ip) => format!("http://{ip}:{port}"),
                    IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
                })
                .collect(),
            link_local_ipv6: link_local_ipv6.into_iter().collect(),
        })
    }

    fn unescape_name(name: &str) -> String {
        let mut chars = name.chars();
        let mut result = String::new();
        while let Some(character) = chars.next() {
            if character == '\\' {
                result.push(chars.next().unwrap_or('\\'));
            } else {
                result.push(character);
            }
        }
        result
    }

    #[cfg(test)]
    #[expect(
        clippy::unwrap_used,
        reason = "Test fixture construction and asserted parsing success"
    )]
    mod tests {
        use super::*;

        const ID: &str = "12345678-1234-4234-8234-123456789abc";
        const OTHER_ID: &str = "87654321-4321-4321-8321-cba987654321";

        fn service(name: &str, id: &str, api: &str, addresses: &str) -> ResolvedService {
            mdns_sd::ServiceInfo::new(
                MDNS_SERVICE_TYPE,
                name,
                "tester.local.",
                addresses,
                8080,
                &[(MDNS_TXT_ID, id), (MDNS_TXT_API, api)][..],
            )
            .unwrap()
            .as_resolved_service()
        }

        #[test]
        fn parses_identity_name_and_future_api_hint() {
            let record = service("Bench. \\ tester", &ID.to_uppercase(), "99", "192.168.1.2");
            let parsed = parse_service(&record).unwrap();
            assert_eq!(parsed.instance_id, ID);
            assert_eq!(parsed.instance_name, "Bench. \\ tester");
            assert_eq!(parsed.hostname, "tester.local.");
            assert_eq!(parsed.port, 8080);
            assert_eq!(parsed.api_version_hint, 99);
            assert_eq!(
                parse_service(&service("Bench", ID, "0", "192.168.1.2"))
                    .unwrap()
                    .api_version_hint,
                0
            );
        }

        #[test]
        fn rejects_invalid_and_missing_metadata() {
            for id in ["", "not-a-uuid", " uuid "] {
                assert!(parse_service(&service("Bench", id, "1", "192.168.1.2")).is_none());
            }
            for api in ["", "-1", "+1", " 1", "1 ", "v1", "4294967296"] {
                assert!(parse_service(&service("Bench", ID, api, "192.168.1.2")).is_none());
            }
            let original = service("Bench", ID, "1", "192.168.1.2");
            let mut record = original.clone();
            record.ty_domain = "_other._tcp.local.".into();
            assert!(parse_service(&record).is_none());
            record = original.clone();
            record.port = 0;
            assert!(parse_service(&record).is_none());
            record = original.clone();
            record.txt_properties = mdns_sd::TxtProperties::new();
            assert!(parse_service(&record).is_none());
            record = original.clone();
            record.fullname = "wrong._other._tcp.local.".into();
            assert!(parse_service(&record).is_none());
            record = original;
            record.ty_domain = MDNS_SERVICE_TYPE.to_uppercase();
            record.fullname = record.fullname.to_uppercase();
            assert!(parse_service(&record).is_some());
        }

        #[test]
        fn formats_numeric_sorted_urls_and_filters_unusable_addresses() {
            let record = service(
                "Bench",
                ID,
                "1",
                "192.168.1.10,192.168.1.2,2001:db8::2,fd00::1,fe80::1,::,::1,ff02::1,127.0.0.1,0.0.0.0,224.0.0.1,255.255.255.255,::ffff:127.0.0.1,fec0::1",
            );
            let parsed = parse_service(&record).unwrap();
            assert_eq!(
                parsed.endpoints,
                [
                    "http://192.168.1.2:8080",
                    "http://192.168.1.10:8080",
                    "http://[2001:db8::2]:8080",
                    "http://[fd00::1]:8080",
                ]
            );
            assert_eq!(parsed.link_local_ipv6.len(), 1);
            assert_eq!(
                parsed.link_local_ipv6[0].address,
                "fe80::1".parse::<Ipv6Addr>().unwrap()
            );
            for endpoint in &parsed.endpoints {
                assert!(url::Url::parse(endpoint).is_ok());
            }
        }

        #[test]
        fn link_local_only_remains_visible_without_a_connect_url() {
            let parsed = parse_service(&service("Bench", ID, "1", "fe80::abcd")).unwrap();
            assert!(parsed.endpoints.is_empty());
            assert_eq!(parsed.link_local_ipv6.len(), 1);
        }

        #[test]
        fn loopback_and_mapped_only_are_not_lan_candidates() {
            for addresses in [
                "127.0.0.1,::1",
                "::ffff:127.0.0.1",
                "::ffff:192.168.1.2",
                "0.0.0.0,::",
            ] {
                assert!(parse_service(&service("Bench", ID, "1", addresses)).is_none());
            }
        }

        #[test]
        fn rejects_missing_individual_and_binary_txt_properties() {
            let mut record = service("Bench", ID, "1", "192.168.1.2");
            for properties in [
                vec![mdns_sd::TxtProperty::from((MDNS_TXT_ID, ID))],
                vec![mdns_sd::TxtProperty::from((MDNS_TXT_API, "1"))],
                vec![
                    mdns_sd::TxtProperty::from((MDNS_TXT_ID, ID)),
                    mdns_sd::TxtProperty::from((MDNS_TXT_API, &[0xff][..])),
                ],
            ] {
                record.txt_properties = mdns_sd::IntoTxtProperties::into_txt_properties(properties);
                assert!(parse_service(&record).is_none());
            }
        }

        #[test]
        fn shared_endpoint_is_deduplicated_and_survives_one_source_removal() {
            let first = service("A", ID, "1", "192.168.1.2,fe80::1");
            let second = service("B", ID, "1", "192.168.1.2,fe80::1");
            let mut forward = Sources::default();
            forward.resolve(&first);
            forward.resolve(&second);
            let mut reverse = Sources::default();
            reverse.resolve(&second);
            reverse.resolve(&first);
            assert_eq!(forward.servers(), reverse.servers());
            assert_eq!(forward.servers()[0].endpoints.len(), 1);
            assert_eq!(forward.servers()[0].link_local_ipv6.len(), 1);
            forward.remove(&first.fullname);
            assert_eq!(forward.servers()[0].endpoints, ["http://192.168.1.2:8080"]);
            forward.resolve(&service("C", OTHER_ID, "1", "192.168.1.2"));
            assert_eq!(forward.servers().len(), 2);
        }

        #[test]
        fn deduplicates_sources_by_uuid_and_removes_only_departed_source() {
            let first = service("A", ID, "1", "192.168.1.10,192.168.1.2");
            let mut second = service("B", &ID.to_uppercase(), "1", "192.168.1.2,fd00::1");
            second.port = 9000;
            let mut sources = Sources::default();
            assert!(sources.resolve(&second));
            assert!(sources.resolve(&first));
            assert!(!sources.resolve(&first));
            let servers = sources.servers();
            assert_eq!(servers.len(), 1);
            assert_eq!(servers[0].instance_name, "A");
            assert_eq!(
                servers[0].endpoints,
                [
                    "http://192.168.1.2:8080",
                    "http://192.168.1.2:9000",
                    "http://192.168.1.10:8080",
                    "http://[fd00::1]:9000"
                ]
            );
            assert!(sources.remove(&first.fullname.to_uppercase()));
            assert_eq!(
                sources.servers()[0].endpoints,
                ["http://192.168.1.2:9000", "http://[fd00::1]:9000"]
            );
            assert!(!sources.remove(&first.fullname));
            assert!(sources.remove(&second.fullname));
            assert!(sources.servers().is_empty());
        }

        #[test]
        fn source_update_replaces_old_identity_and_addresses() {
            let mut sources = Sources::default();
            let first = service("Bench", ID, "1", "192.168.1.2");
            sources.resolve(&first);
            sources.resolve(&service("Bench", ID, "1", "192.168.1.5"));
            assert_eq!(sources.servers()[0].endpoints, ["http://192.168.1.5:8080"]);
            let replacement = service("Bench", OTHER_ID, "99", "192.168.1.10");
            sources.resolve(&replacement);
            let servers = sources.servers();
            assert_eq!(servers.len(), 1);
            assert_eq!(servers[0].instance_id, OTHER_ID);
            assert_eq!(servers[0].endpoints, ["http://192.168.1.10:8080"]);
            let invalid = service("Bench", "bad", "1", "192.168.1.10");
            assert!(sources.resolve(&invalid));
            assert!(sources.servers().is_empty());
        }
    }
}
