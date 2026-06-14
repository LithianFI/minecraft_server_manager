use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{self, Stream, StreamExt};
use serde_json::json;

use crate::state::{AppState, InstanceInfo};

pub async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe to the broadcast BEFORE reading state to avoid missing events
    // that fire between the snapshot and the live stream starting.
    let rx = state.log_tx.subscribe();

    let mut init: Vec<Result<Event, Infallible>> = Vec::new();
    {
        let instances = state.instances.read().await;
        let infos: Vec<InstanceInfo> = instances.values().map(|i| i.into()).collect();

        if let Ok(json) = serde_json::to_string(&json!({"type": "init", "instances": infos})) {
            init.push(Ok(Event::default().data(json)));
        }

        // Send buffered logs for any instance that has them
        for inst in instances.values() {
            if inst.log_buffer.is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&json!({
                "type": "log_history",
                "instance_id": inst.id,
                "lines": inst.log_buffer,
            })) {
                init.push(Ok(Event::default().data(json)));
            }
        }
    }

    let live = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&event) {
                        return Some((Ok(Event::default().data(json)), rx));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });

    Sse::new(stream::iter(init).chain(live))
        .keep_alive(KeepAlive::default())
}
