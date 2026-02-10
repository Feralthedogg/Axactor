use axactor::{rt::System, SpawnConfig, registry::Registry};
use axum::{
    extract::{State, Json, Path},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::time::Duration;
use std::sync::Arc;
use std::collections::VecDeque;
use tower_http::trace::TraceLayer;
use tower::ServiceBuilder;

pub struct RoomActor {
    _room_id: String,
    messages: VecDeque<String>,
    cap: usize,
}

#[axactor::actor]
impl RoomActor {
    #[msg]
    async fn get_messages(&mut self, limit: usize) -> Vec<String> {
        self.messages.iter().rev().take(limit).cloned().collect()
    }

    #[msg]
    async fn post_message(&mut self, msg: String) {
        if self.messages.len() >= self.cap {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }
}

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry<String, RoomActorRef>>,
}

#[derive(Deserialize)]
struct PostBody {
    msg: String,
}

async fn get_room_messages(
    State(st): State<AppState>,
    Path(room_id): Path<String>,
) -> Result<Json<Vec<String>>, axactor::AskError> {
    let room = st.registry.get_or_spawn(
        room_id.clone(),
        SpawnConfig::new(format!("room:{}", room_id), 1024),
        {
            let room_id = room_id.clone();
            move || RoomActor {
                _room_id: room_id.clone(),
                messages: VecDeque::with_capacity(100),
                cap: 100
            }
        },
    ).await;

    let msgs = room.get_messages_timeout(20, Duration::from_millis(500)).await?;
    Ok(Json(msgs))
}

async fn post_room_message(
    State(st): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<PostBody>,
) -> Result<Json<&'static str>, axactor::TellError> {
    let room = st.registry.get_or_spawn(
        room_id.clone(),
        SpawnConfig::new(format!("room:{}", room_id), 1024),
        {
            let room_id = room_id.clone();
            move || RoomActor {
                _room_id: room_id.clone(),
                messages: VecDeque::with_capacity(100),
                cap: 100
            }
        },
    ).await;

    room.post_message(body.msg)?;
    Ok(Json("ok"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let system = Arc::new(System::new());
    let registry = Arc::new(Registry::new(system.clone()));

    registry.start_reaper(Duration::from_secs(60), Duration::from_secs(10));

    let state = AppState { registry };

    let app = Router::new()
        .route("/rooms/:room_id", get(get_room_messages))
        .route("/rooms/:room_id", post(post_room_message))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(tower_http::timeout::TimeoutLayer::new(Duration::from_secs(2)))
                .layer(tower::limit::ConcurrencyLimitLayer::new(1024))
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());

    let server = axum::serve(listener, app);

    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
        system.shutdown_graceful(Duration::from_secs(2)).await;
    };

    server.with_graceful_shutdown(shutdown).await.unwrap();
}