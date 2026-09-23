//! Shared semantic mapping for browser and native remote clients.

use crate::backend::{BackendEvent, BackendEventSender, BackendState};
use crate::core::{ApiCommand, SnapshotUpdate, WebSocketEvent};

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
mod tests {
    use futures::channel::mpsc;

    use super::*;
    use crate::core::{CalibrationCommand, CycleRecipe, SavedRecipe, TestConfiguration};

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
