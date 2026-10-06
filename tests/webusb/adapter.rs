// Test-only visibility adapter, appended to a scratch copy of usb_wasm.rs.
// The production worker, parser, transport and backend are not replaced.
thread_local! {
    static TEST_COMMANDS: std::cell::RefCell<Option<futures::channel::mpsc::UnboundedSender<BackendCommand>>> = const { std::cell::RefCell::new(None) };
    static TEST_EVENTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub fn boundary_start() {
    use futures::StreamExt as _;
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let (events, mut receive) = futures::channel::mpsc::unbounded();
    TEST_COMMANDS.with(|slot| *slot.borrow_mut() = Some(tx));
    wasm_bindgen_futures::spawn_local(worker::local_backend_task(
        rx,
        BackendEventSender::new(events, || {}),
    ));
    wasm_bindgen_futures::spawn_local(async move {
        while let Some(event) = receive.next().await {
            let text = match event {
                BackendEvent::Update(state) => serde_json::json!({"update": state.update}),
                BackendEvent::Snapshot(snapshot) => serde_json::json!({"snapshot": snapshot}),
                BackendEvent::Sample(sample) => serde_json::json!({"sample": sample}),
                BackendEvent::CycleSample(sample) => serde_json::json!({"cycle_sample": sample}),
                other => serde_json::json!({"event": format!("{other:?}")}),
            }
            .to_string();
            TEST_EVENTS.with(|events| events.borrow_mut().push(text));
        }
    });
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub fn boundary_command(kind: &str, body: &str) {
    use crate::core::ApiCommand;
    let command = match kind {
        "connect" => BackendCommand::Connect(0),
        "disconnect" => BackendCommand::Disconnect,
        "start" => BackendCommand::StartTest(serde_json::from_str(body).unwrap()),
        "cycle" => BackendCommand::StartCycle(serde_json::from_str(body).unwrap()),
        "resume" => BackendCommand::Resume(serde_json::from_str(body).unwrap()),
        "api" => BackendCommand::Api(serde_json::from_str(body).unwrap()),
        "stop" => BackendCommand::Api(ApiCommand::Stop),
        "shutdown" => BackendCommand::Shutdown,
        _ => panic!("invalid test command"),
    };
    TEST_COMMANDS.with(|slot| {
        slot.borrow()
            .as_ref()
            .unwrap()
            .unbounded_send(command)
            .unwrap()
    });
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub fn boundary_events() -> String {
    TEST_EVENTS.with(|events| {
        format!(
            "[{}]",
            events.borrow_mut().drain(..).collect::<Vec<_>>().join(",")
        )
    })
}
