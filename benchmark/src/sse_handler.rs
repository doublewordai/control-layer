use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::sse::Event,
    response::Sse,
};
use std::{convert::Infallible, sync::Arc};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::AppState;

pub async fn benchmark_events_handler(
    Path(run_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let rx = {
        let channels = state.progress_channels.read().await;
        channels.get(&run_id).map(|tx| tx.subscribe())
    };

    let Some(rx) = rx else {
        return Err(StatusCode::NOT_FOUND);
    };

    let stream = BroadcastStream::new(rx).map(|result| {
        match result {
            Ok(update) => {
                let json = serde_json::to_string(&update).unwrap_or_default();
                Ok(Event::default().data(json))
            }
            Err(_) => Ok(Event::default().data(""))
        }
    });

    Ok(Sse::new(stream))
}