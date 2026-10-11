//! Shared semantic mapping for browser and native remote clients.

use crate::backend::{BackendEvent, BackendEventSender, BackendState};
use crate::core::{ApiCommand, MACHINE_API_UNAVAILABLE_INFO, SnapshotUpdate, WebSocketEvent};

/// Discovery failures distinguish reconnectable network errors from incompatible servers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryError {
    Transient(String),
    Incompatible(String),
}

/// One HTTP discovery policy for native and browser transports.
pub(crate) fn check_discovery_status(status: u16) -> Result<(), DiscoveryError> {
    match status {
        200..=299 => Ok(()),
        408 | 425 | 429 | 500..=599 => Err(DiscoveryError::Transient(format!(
            "Machine API discovery temporarily failed with HTTP {status}."
        ))),
        404 | 405 => Err(DiscoveryError::Incompatible(
            MACHINE_API_UNAVAILABLE_INFO.to_owned(),
        )),
        _ => Err(DiscoveryError::Incompatible(format!(
            "Machine API discovery was rejected with HTTP {status}."
        ))),
    }
}

/// Unknown event tags are compatible extensions; invalid known events remain errors.
pub(crate) fn decode_websocket_event(
    text: &str,
) -> Result<Option<WebSocketEvent>, serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        event: String,
    }
    let envelope: Envelope = serde_json::from_str(text)?;
    match envelope.event.as_str() {
        "snapshot" | "update" | "sample" | "cycle_sample" | "recipe_library" | "recipe_upsert"
        | "recipe_delete" => serde_json::from_str(text).map(Some),
        _ => Ok(None),
    }
}

/// Required initial resources, tracked independently of their arrival order.
#[derive(Default)]
pub(crate) struct RemoteSynchronization {
    received_snapshot: bool,
    received_recipe_library: bool,
}

impl RemoteSynchronization {
    pub(crate) fn observe(&mut self, event: &WebSocketEvent) {
        match event {
            WebSocketEvent::Snapshot(_) => self.received_snapshot = true,
            WebSocketEvent::RecipeLibrary(_) => self.received_recipe_library = true,
            _ => {}
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.received_snapshot && self.received_recipe_library
    }
}

pub(crate) const COMMAND_HEADER: &str = "X-EBC-Command";
pub(crate) const INITIAL_RECONNECT_DELAY_MS: u64 = 1_000;
pub(crate) const MAX_RECONNECT_DELAY_MS: u64 = 15_000;

pub(crate) fn command_endpoint(command: ApiCommand) -> Result<&'static str, String> {
    match command {
        ApiCommand::Connect => Ok("/api/connect"),
        ApiCommand::Disconnect => Ok("/api/disconnect"),
        ApiCommand::Start(_) => Err(
            "ApiCommand::Start cannot be sent remotely; use BackendCommand::StartTest".to_owned(),
        ),
        ApiCommand::Adjust(_) => Ok("/api/test/adjust"),
        ApiCommand::Stop => Ok("/api/test/stop"),
        ApiCommand::Resume => Ok("/api/test/resume"),
        ApiCommand::Calibration(_) => Ok("/api/calibration"),
    }
}

pub(crate) fn publish_websocket(event: WebSocketEvent, event_tx: &BackendEventSender) {
    match event {
        WebSocketEvent::Snapshot(snapshot) => event_tx.send(BackendEvent::Snapshot(snapshot)),
        WebSocketEvent::Update(update) => publish_update(update, event_tx),
        WebSocketEvent::Sample(sample) => event_tx.send(BackendEvent::Sample(sample)),
        WebSocketEvent::CycleSample(sample) => event_tx.send(BackendEvent::CycleSample(sample)),
        WebSocketEvent::RecipeLibrary(recipes) => {
            event_tx.send(BackendEvent::RecipeLibrary(recipes));
        }
        WebSocketEvent::RecipeUpsert(recipe) => {
            event_tx.send(BackendEvent::RecipeUpsert(recipe));
        }
        WebSocketEvent::RecipeDelete(id) => event_tx.send(BackendEvent::RecipeDeleted(id)),
    }
}

pub(crate) fn publish_update(update: SnapshotUpdate, event_tx: &BackendEventSender) {
    event_tx.send(BackendEvent::Update(BackendState { update }));
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteUrls {
    pub base: String,
    pub websocket: String,
    pub origin: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl RemoteUrls {
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err("remote server URL is empty".to_owned());
        }
        let candidate = if trimmed.contains("://") {
            trimmed.to_owned()
        } else {
            format!("http://{trimmed}")
        };
        let mut url = url::Url::parse(&candidate)
            .map_err(|error| format!("invalid remote server URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("remote server URL must use http or https".to_owned());
        }
        if url.host_str().is_none() {
            return Err("remote server URL must include a host".to_owned());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("remote server URL must not include credentials".to_owned());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("remote server URL must not include a query or fragment".to_owned());
        }

        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&path);
        let base = url.as_str().trim_end_matches('/').to_owned();

        let mut origin_url = url.clone();
        origin_url.set_path("");
        let origin = origin_url.as_str().trim_end_matches('/').to_owned();

        let websocket_scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(websocket_scheme)
            .map_err(|()| "failed to derive WebSocket URL".to_owned())?;
        url.set_path(&format!("{path}/api/ws"));

        Ok(Self {
            base,
            websocket: url.to_string(),
            origin,
        })
    }

    pub(crate) fn endpoint(&self, command: ApiCommand) -> Result<String, String> {
        Ok(format!("{}{}", self.base, command_endpoint(command)?))
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "protocol tests should fail fast")]
mod tests {
    use futures::channel::mpsc;

    use super::*;
    use crate::core::{CalibrationCommand, CycleRecipe, SavedRecipe, TestConfiguration};

    #[test]
    fn discovery_status_policy_is_shared_and_precise() {
        for status in [200, 201, 204, 299] {
            assert_eq!(check_discovery_status(status), Ok(()));
        }
        for status in [400, 401, 403, 404, 405, 301] {
            assert!(
                matches!(
                    check_discovery_status(status),
                    Err(DiscoveryError::Incompatible(_))
                ),
                "HTTP {status}"
            );
        }
        for status in [408, 425, 429, 500, 502, 503, 504, 599] {
            assert!(
                matches!(
                    check_discovery_status(status),
                    Err(DiscoveryError::Transient(_))
                ),
                "HTTP {status}"
            );
        }
    }

    fn known_events() -> Vec<WebSocketEvent> {
        use crate::core::{AuthoritativeSnapshot, CycleSample, CycleState, Sample, TestState};
        let snapshot = AuthoritativeSnapshot::default();
        let recipe = SavedRecipe {
            id: "recipe".to_owned(),
            name: "Recipe".to_owned(),
            recipe: CycleRecipe {
                steps: Vec::new(),
                repeat_count: 1,
            },
            revision: 1,
            created_at_utc: String::new(),
            updated_at_utc: String::new(),
        };
        let sample = Sample {
            run_id: "run".to_owned(),
            sequence: 0,
            timestamp_utc: String::new(),
            elapsed_seconds: 0,
            voltage_mv: 4000,
            current_ma: 1000,
            capacity_mah: 0,
            energy_wh: 0.0,
            mode: crate::device::DeviceMode::DischargeConstantCurrent,
        };
        let cycle_sample = CycleSample {
            execution_id: "cycle".to_owned(),
            sequence: 0,
            timestamp_utc: String::new(),
            elapsed_milliseconds: 0,
            repeat_index: 0,
            step_index: 0,
            cycle_state: CycleState::RunningStep,
            test_state: TestState::Running,
            mode: sample.mode,
            activity_known: true,
            active: true,
            voltage_mv: 4000,
            current_ma: 1000,
            device_capacity_mah: 0,
            test_capacity_mah: Some(0),
            test_energy_wh: 0.0,
        };
        vec![
            WebSocketEvent::Update(SnapshotUpdate::from(&snapshot)),
            WebSocketEvent::Snapshot(snapshot),
            WebSocketEvent::Sample(sample),
            WebSocketEvent::CycleSample(cycle_sample),
            WebSocketEvent::RecipeLibrary(vec![recipe.clone()]),
            WebSocketEvent::RecipeUpsert(recipe),
            WebSocketEvent::RecipeDelete("recipe".to_owned()),
        ]
    }

    #[test]
    fn websocket_decoder_preserves_every_known_event() {
        for event in known_events() {
            let text = serde_json::to_string(&event).expect("serialize known event");
            assert_eq!(
                decode_websocket_event(&text).expect("decode event"),
                Some(event)
            );
        }
    }

    #[test]
    fn websocket_decoder_distinguishes_extensions_from_malformed_protocol() {
        for text in [
            r#"{"event":"future_optional_event","payload":{"hello":"world"}}"#,
            r#"{"event":"future_optional_event","payload":[null,42,"arbitrary"]}"#,
            r#"{"event":"future_optional_event"}"#,
        ] {
            assert_eq!(
                decode_websocket_event(text).expect("compatible extension"),
                None
            );
        }
        for text in [
            "not JSON",
            "{}",
            r#"{"event":42}"#,
            r#"{"event":"future","payload":}"#,
        ] {
            assert!(decode_websocket_event(text).is_err(), "{text}");
        }
        for event in known_events() {
            let mut value = serde_json::to_value(event).expect("known event");
            value["payload"] = serde_json::json!({"broken": true});
            assert!(
                decode_websocket_event(&value.to_string()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn synchronization_requires_both_resources_and_ignores_extensions() {
        for reverse in [false, true] {
            let mut sync = RemoteSynchronization::default();
            let mut resources = [
                WebSocketEvent::Snapshot(crate::core::AuthoritativeSnapshot::default()),
                WebSocketEvent::RecipeLibrary(Vec::new()),
            ];
            if reverse {
                resources.reverse();
            }
            assert!(!sync.is_ready());
            for (index, event) in resources.iter().enumerate() {
                let unknown = decode_websocket_event(r#"{"event":"future","payload":null}"#)
                    .expect("extension");
                if let Some(event) = unknown {
                    sync.observe(&event);
                }
                assert!(!sync.is_ready());
                sync.observe(event);
                assert_eq!(sync.is_ready(), index == 1);
            }
            for event in known_events() {
                sync.observe(&event);
            }
            assert!(sync.is_ready());
        }
    }

    fn config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    #[test]
    fn command_endpoints_are_shared() {
        let commands = [
            (ApiCommand::Connect, "/api/connect"),
            (ApiCommand::Disconnect, "/api/disconnect"),
            (ApiCommand::Adjust(config()), "/api/test/adjust"),
            (ApiCommand::Stop, "/api/test/stop"),
            (ApiCommand::Resume, "/api/test/resume"),
            (
                ApiCommand::Calibration(CalibrationCommand::VoltageLow(1000)),
                "/api/calibration",
            ),
        ];
        for (command, endpoint) in commands {
            assert_eq!(command_endpoint(command).as_deref(), Ok(endpoint));
        }
        assert!(command_endpoint(ApiCommand::Start(config())).is_err());
        assert_eq!(COMMAND_HEADER, "X-EBC-Command");
    }

    #[test]
    fn recipe_websocket_events_preserve_semantic_mapping() {
        let (event_tx, mut event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, || {});
        let recipe = SavedRecipe {
            id: "recipe-1".to_owned(),
            name: "Recipe".to_owned(),
            recipe: CycleRecipe {
                steps: Vec::new(),
                repeat_count: 1,
            },
            revision: 3,
            created_at_utc: "created".to_owned(),
            updated_at_utc: "updated".to_owned(),
        };

        publish_websocket(
            WebSocketEvent::RecipeLibrary(vec![recipe.clone()]),
            &event_tx,
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(BackendEvent::RecipeLibrary(recipes)) if recipes == vec![recipe.clone()]
        ));

        publish_websocket(WebSocketEvent::RecipeUpsert(recipe.clone()), &event_tx);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(BackendEvent::RecipeUpsert(actual)) if actual == recipe
        ));

        publish_websocket(
            WebSocketEvent::RecipeDelete("recipe-1".to_owned()),
            &event_tx,
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(BackendEvent::RecipeDeleted(id)) if id == "recipe-1"
        ));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn normalizes_remote_urls_and_maps_websocket_schemes() {
        let Ok(http) = RemoteUrls::parse("  battery.local:8080/ ") else {
            panic!("HTTP URL should parse");
        };
        assert_eq!(http.base, "http://battery.local:8080");
        assert_eq!(http.websocket, "ws://battery.local:8080/api/ws");
        assert_eq!(http.origin, "http://battery.local:8080");

        let Ok(https) = RemoteUrls::parse("https://battery.example/") else {
            panic!("HTTPS URL should parse");
        };
        assert_eq!(https.base, "https://battery.example");
        assert_eq!(https.websocket, "wss://battery.example/api/ws");
        assert_eq!(https.origin, "https://battery.example");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn rejects_malformed_or_unsupported_remote_urls() {
        for invalid in [
            "",
            "http://",
            "ftp://battery.local",
            "http://user@host",
            "http://host?q=1",
        ] {
            assert!(RemoteUrls::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
