use crate::backend::{
    BackendCommand, BackendConnectionStatus, BackendEvent, BackendEventSender, DiagnosticDirection,
    DiagnosticEvent, remote_api_commands,
};
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, MACHINE_API_INVALID_INFO, MACHINE_API_UNAVAILABLE_INFO,
    MachineApiInfo, RecipeExport, SavedRecipe, WebSocketEvent, validate_machine_api,
};
use crate::remote_backend::{
    COMMAND_HEADER, DiscoveryError, INITIAL_RECONNECT_DELAY_MS, MAX_RECONNECT_DELAY_MS,
    command_endpoint, publish_websocket,
};
use futures::channel::mpsc::UnboundedReceiver;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;

#[expect(clippy::too_many_lines)]
pub(super) async fn remote_task(
    mut command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
) {
    let mut reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
    loop {
        send_connection(
            &event_tx,
            if reconnect_delay_ms == INITIAL_RECONNECT_DELAY_MS {
                BackendConnectionStatus::Connecting
            } else {
                BackendConnectionStatus::Reconnecting
            },
        );
        let url = match websocket_url() {
            Ok(url) => url,
            Err(error) => {
                send_connection(&event_tx, BackendConnectionStatus::Error(error));
                return;
            }
        };
        let Some(discovery) = discover_while_disconnected(&mut command_rx, &event_tx).await else {
            return;
        };
        let connection = match discovery {
            Ok(()) => WebSocket::open(&url).map_err(|error| error.to_string()),
            Err(DiscoveryError::Transient(error)) => Err(error),
            Err(DiscoveryError::Incompatible(error)) => {
                send_connection(&event_tx, BackendConnectionStatus::Error(error));
                return;
            }
        };
        match connection {
            Ok(socket) => {
                let (mut writer, mut reader) = socket.split();
                let mut received_snapshot = false;
                loop {
                    let message = reader.next().fuse();
                    let command = command_rx.next().fuse();
                    futures::pin_mut!(message, command);
                    futures::select! {
                        incoming = message => match incoming {
                            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                                Ok(WebSocketEvent::RecipeLibrary(subscription_library)) => {
                                    let recipes = match fetch_recipes().await {
                                        Ok(recipes) => recipes,
                                        Err(error) => {
                                            event_tx.send(BackendEvent::CommandError(
                                                format!("failed to refresh recipes: {error}"),
                                            ));
                                            subscription_library
                                        }
                                    };
                                    event_tx.send(BackendEvent::RecipeLibrary(recipes));
                                    if !received_snapshot {
                                        received_snapshot = true;
                                        reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
                                        send_connection(&event_tx, BackendConnectionStatus::Connected);
                                    }
                                }
                                Ok(event) => {
                                    publish_websocket(event, &event_tx);
                                }
                                Err(error) => log::error!("invalid server websocket event: {error}"),
                            },
                            Some(Ok(Message::Bytes(_))) => {}
                            Some(Err(error)) => {
                                log::warn!("server websocket failed: {error}");
                                break;
                            }
                            None => break,
                        },
                        command = command => {
                            let Some(command) = command else {
                                let _closed = writer.close().await;
                                return;
                            };
                            if matches!(command, BackendCommand::Shutdown) {
                                let _closed = writer.close().await;
                                return;
                            }
                            if !received_snapshot {
                                let error = "browser is synchronizing the remote recipe library; command was not sent".to_owned();
                                if let BackendCommand::History(request) = &command {
                                    event_tx.send(BackendEvent::HistoryResult { request: request.clone(), result: Err(error) });
                                } else {
                                    event_tx.send(BackendEvent::CommandError(error));
                                }
                                continue;
                            }
                            match &command {
                                BackendCommand::History(request) => {
                                    let result = fetch_history(request).await;
                                    event_tx.send(BackendEvent::HistoryResult { request: request.clone(), result });
                                    continue;
                                }
                                BackendCommand::StartTest(request) => {
                                    publish_cycle_result(
                                        send_json("/api/test/start", request).await,
                                        &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::StartCycle(request) => {
                                    publish_cycle_result(
                                        send_json("/api/cycle/start", request).await,
                                        &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::StartSavedRecipe { recipe_id, request } => {
                                    publish_cycle_result(
                                        send_json(
                                            &format!("/api/recipes/{recipe_id}/start"),
                                            request,
                                        ).await,
                                        &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::RefreshRecipes => {
                                    match fetch_recipes().await {
                                        Ok(recipes) => {
                                            event_tx.send(BackendEvent::RecipeLibrary(recipes));
                                            event_tx.send(BackendEvent::CommandSucceeded);
                                        }
                                        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
                                    }
                                    continue;
                                }
                                BackendCommand::CreateSavedRecipe(request) => {
                                    publish_created_recipe_result(
                                        send_json("/api/recipes", request).await,
                                        &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::UpdateSavedRecipe { recipe_id, request } => {
                                    let result = send_put_json(
                                        &format!("/api/recipes/{recipe_id}"),
                                        request,
                                    ).await;
                                    if result.as_ref().is_err_and(|error| error.contains("HTTP 409"))
                                        && let Ok(recipes) = fetch_recipes().await
                                    {
                                        event_tx.send(BackendEvent::RecipeLibrary(recipes));
                                    }
                                    publish_recipe_result(result, &event_tx);
                                    continue;
                                }
                                BackendCommand::DeleteSavedRecipe { recipe_id, request } => {
                                    let result: Result<SavedRecipe, String> = send_delete_json(
                                        &format!("/api/recipes/{recipe_id}"),
                                        request,
                                    ).await;
                                    match result {
                                        Ok(recipe) => {
                                            event_tx.send(BackendEvent::RecipeDeleted(recipe.id));
                                            event_tx.send(BackendEvent::CommandSucceeded);
                                        }
                                        Err(error) => {
                                            if error.contains("HTTP 409")
                                                && let Ok(recipes) = fetch_recipes().await
                                            {
                                                event_tx.send(BackendEvent::RecipeLibrary(recipes));
                                            }
                                            event_tx.send(BackendEvent::CommandError(error));
                                        }
                                    }
                                    continue;
                                }
                                BackendCommand::ImportRecipe(export) => {
                                    publish_created_recipe_result(
                                        send_json("/api/recipes/import", export).await,
                                        &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::ExportRecipe { recipe_id } => {
                                    match get_json::<RecipeExport>(
                                        &format!("/api/recipes/{recipe_id}/export"),
                                    ).await {
                                        Ok(export) => {
                                            event_tx.send(BackendEvent::RecipeExported(export));
                                            event_tx.send(BackendEvent::CommandSucceeded);
                                        }
                                        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
                                    }
                                    continue;
                                }
                                BackendCommand::StartSavedRecipeSnapshot { .. } => {
                                    event_tx.send(BackendEvent::CommandError(
                                        "local saved recipe snapshot sent to remote backend".to_owned(),
                                    ));
                                    continue;
                                }
                                BackendCommand::RenameRun { run_id, request } => {
                                    publish_rename_result(
                                        send_json(&format!("/api/runs/{run_id}/name"), request).await,
                                        run_id, false, request, &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::RenameCycle { execution_id, request } => {
                                    publish_rename_result(
                                        send_json(&format!("/api/cycles/{execution_id}/name"), request).await,
                                        execution_id, true, request, &event_tx,
                                    );
                                    continue;
                                }
                                BackendCommand::StopCycle => {
                                    publish_cycle_result(send_stop_cycle().await, &event_tx);
                                    continue;
                                }
                                _ => {}
                            }
                            for command in remote_api_commands(command) {
                                event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
                                    direction: DiagnosticDirection::Out,
                                    label: format!("{command:?}"),
                                    raw_bytes: Vec::new(),
                                }));
                                match send_command(command).await {
                                Ok(_snapshot) => {
                                    event_tx.send(BackendEvent::CommandSucceeded);
                                }
                                Err(error) => {
                                    log::error!("remote command failed: {error}");
                                    event_tx.send(BackendEvent::CommandError(error));
                                    break;
                                }
                                }
                            }
                        },
                    }
                }
            }
            Err(error) => {
                log::warn!("failed to connect remote server: {error}");
                event_tx.send(BackendEvent::CommandError(error));
            }
        }
        send_connection(&event_tx, BackendConnectionStatus::Reconnecting);
        let timeout = TimeoutFuture::new(reconnect_delay_ms as u32).fuse();
        futures::pin_mut!(timeout);
        loop {
            let command = command_rx.next().fuse();
            futures::pin_mut!(command);
            futures::select_biased! {
                command = command => match command {
                    Some(BackendCommand::Shutdown) | None => return,
                    Some(_) => event_tx.send(BackendEvent::CommandError(
                        "browser is disconnected; command was not sent".to_owned(),
                    )),
                },
                () = timeout => break,
            }
        }
        while let Ok(command) = command_rx.try_recv() {
            if matches!(command, BackendCommand::Shutdown) {
                return;
            }
            event_tx.send(BackendEvent::CommandError(
                "browser is disconnected; command was not sent".to_owned(),
            ));
        }
        reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
    }
}

async fn discover_while_disconnected(
    command_rx: &mut UnboundedReceiver<BackendCommand>,
    event_tx: &BackendEventSender,
) -> Option<Result<(), DiscoveryError>> {
    let discovery = discover_machine_api().fuse();
    futures::pin_mut!(discovery);
    loop {
        let command = command_rx.next().fuse();
        futures::pin_mut!(command);
        futures::select_biased! {
            command = command => match command {
                Some(BackendCommand::Shutdown) | None => return None,
                Some(command) => {
                    let error = "browser is verifying remote compatibility; command was not sent".to_owned();
                    if let BackendCommand::History(request) = command {
                        event_tx.send(BackendEvent::HistoryResult { request, result: Err(error) });
                    } else {
                        event_tx.send(BackendEvent::CommandError(error));
                    }
                }
            },
            result = discovery => return Some(result),
        }
    }
}

async fn discover_machine_api() -> Result<(), DiscoveryError> {
    let response = Request::get("/api/info").send().await.map_err(|error| {
        DiscoveryError::Transient(format!("{MACHINE_API_UNAVAILABLE_INFO} {error}"))
    })?;
    if !response.ok() {
        return Err(DiscoveryError::Incompatible(
            MACHINE_API_UNAVAILABLE_INFO.to_owned(),
        ));
    }
    let info = response
        .json::<MachineApiInfo>()
        .await
        .map_err(|_error| DiscoveryError::Incompatible(MACHINE_API_INVALID_INFO.to_owned()))?;
    validate_machine_api(&info).map_err(|error| DiscoveryError::Incompatible(error.to_string()))?;
    log::info!(
        "connected to {} {} machine API v{}",
        info.service,
        info.server_version,
        info.api_version
    );
    Ok(())
}

fn publish_cycle_result(
    result: Result<AuthoritativeSnapshot, String>,
    event_tx: &BackendEventSender,
) {
    match result {
        Ok(_snapshot) => event_tx.send(BackendEvent::CommandSucceeded),
        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
    }
}

async fn send_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    endpoint: &str,
    body: &T,
) -> Result<R, String> {
    let response = Request::post(endpoint)
        .header(COMMAND_HEADER, "1")
        .json(body)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode_json_response(response).await
}

async fn send_put_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    endpoint: &str,
    body: &T,
) -> Result<R, String> {
    let response = Request::put(endpoint)
        .header(COMMAND_HEADER, "1")
        .json(body)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode_json_response(response).await
}

async fn send_delete_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    endpoint: &str,
    body: &T,
) -> Result<R, String> {
    let response = Request::delete(endpoint)
        .header(COMMAND_HEADER, "1")
        .json(body)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode_json_response(response).await
}

async fn get_json<T: serde::de::DeserializeOwned>(endpoint: &str) -> Result<T, String> {
    let response = Request::get(endpoint)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode_json_response(response).await
}

async fn fetch_recipes() -> Result<Vec<SavedRecipe>, String> {
    get_json("/api/recipes").await
}

fn publish_recipe_result(result: Result<SavedRecipe, String>, event_tx: &BackendEventSender) {
    match result {
        Ok(recipe) => {
            event_tx.send(BackendEvent::RecipeUpsert(recipe));
            event_tx.send(BackendEvent::CommandSucceeded);
        }
        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
    }
}

fn publish_created_recipe_result(
    result: Result<SavedRecipe, String>,
    event_tx: &BackendEventSender,
) {
    match result {
        Ok(recipe) => {
            event_tx.send(BackendEvent::RecipeCreated(recipe));
            event_tx.send(BackendEvent::CommandSucceeded);
        }
        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
    }
}

async fn decode_json_response<T: serde::de::DeserializeOwned>(
    response: gloo_net::http::Response,
) -> Result<T, String> {
    if !response.ok() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    response.json().await.map_err(|error| error.to_string())
}

async fn send_stop_cycle() -> Result<AuthoritativeSnapshot, String> {
    let response = Request::post("/api/cycle/stop")
        .header(COMMAND_HEADER, "1")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode_response(response).await
}

async fn decode_response(
    response: gloo_net::http::Response,
) -> Result<AuthoritativeSnapshot, String> {
    if !response.ok() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    response.json().await.map_err(|error| error.to_string())
}

fn send_connection(event_tx: &BackendEventSender, status: BackendConnectionStatus) {
    event_tx.send(BackendEvent::BackendConnectionChanged(status));
}

fn websocket_url() -> Result<String, String> {
    let window = web_sys::window().ok_or_else(|| "browser window is unavailable".to_owned())?;
    let location = window.location();
    let protocol = location
        .protocol()
        .map_err(|error| format!("failed to read page protocol: {error:?}"))?;
    let host = location
        .host()
        .map_err(|error| format!("failed to read page host: {error:?}"))?;
    let websocket_protocol = if protocol == "https:" { "wss" } else { "ws" };
    Ok(format!("{websocket_protocol}://{host}/api/ws"))
}

async fn send_command(command: ApiCommand) -> Result<AuthoritativeSnapshot, String> {
    if matches!(command, ApiCommand::Start(_)) {
        return Err(
            "ApiCommand::Start cannot be sent remotely; use BackendCommand::StartTest".to_owned(),
        );
    }
    let response = match command {
        ApiCommand::Connect | ApiCommand::Disconnect | ApiCommand::Stop | ApiCommand::Resume => {
            Request::post(command_endpoint(command)?)
                .header(COMMAND_HEADER, "1")
                .send()
                .await
        }
        ApiCommand::Adjust(config) => {
            Request::post(command_endpoint(command)?)
                .header(COMMAND_HEADER, "1")
                .json(&config)
                .map_err(|error| error.to_string())?
                .send()
                .await
        }
        ApiCommand::Calibration(calibration) => {
            Request::post(command_endpoint(command)?)
                .header(COMMAND_HEADER, "1")
                .json(&calibration)
                .map_err(|error| error.to_string())?
                .send()
                .await
        }
        ApiCommand::Start(_) => unreachable!("start was rejected above"),
    }
    .map_err(|error| error.to_string())?;
    if !response.ok() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    response.json().await.map_err(|error| error.to_string())
}

async fn fetch_history(
    request: &crate::backend::HistoryRequest,
) -> Result<crate::backend::HistoryEvent, String> {
    let response = Request::get(&request.path())
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!(
            "history request failed: HTTP {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ));
    }
    request.decode(response.binary().await.map_err(|error| error.to_string())?)
}

fn publish_rename_result(
    result: Result<AuthoritativeSnapshot, String>,
    id: &str,
    cycle: bool,
    request: &crate::core::RenameRequest,
    event_tx: &BackendEventSender,
) {
    if result.is_ok() {
        event_tx.send(BackendEvent::HistoryRenamed {
            id: id.to_owned(),
            cycle,
            name: crate::core::normalize_optional_name(request.name.as_deref()).unwrap_or_default(),
        });
    }
    publish_cycle_result(result, event_tx);
}
