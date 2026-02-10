use axactor::{rt::System, Context, SpawnConfig};
use axum::{
    extract::{State, Json},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::time::Duration;
use tower_http::trace::TraceLayer;

pub struct Counter {
    n: i64,
}

#[axactor::actor]
impl Counter {
    #[msg]
    async fn get(&mut self) -> i64 {
        self.n
    }

    #[msg]
    async fn add(&mut self, delta: i64) -> i64 {
        self.n += delta;
        self.n
    }

    #[msg]
    async fn reset(&mut self, _ctx: &mut Context) {
        self.n = 0;
        tracing::info!("Counter reset!");
    }
}

#[derive(Clone)]
struct AppState {
    counter: CounterRef,
}

#[derive(Deserialize)]
struct AddRequest {
    delta: i64,
}

async fn get_counter(State(st): State<AppState>) -> Result<Json<i64>, axactor::AskError> {

    let val = st.counter.get_timeout(Duration::from_millis(300)).await?;
    Ok(Json(val))
}

async fn add_counter(
    State(st): State<AppState>,
    Json(req): Json<AddRequest>,
) -> Result<Json<i64>, axactor::AskError> {
    let val = st.counter.add_timeout(req.delta, Duration::from_secs(1)).await?;
    Ok(Json(val))
}

async fn reset_counter(State(st): State<AppState>) -> Result<Json<&'static str>, axactor::TellError> {

    st.counter.reset()?;
    Ok(Json("ok"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let system = System::new();

    let cfg = SpawnConfig::new("counter", 1024)
        .shutdown_graceful(Duration::from_secs(2))
        .restart_on_panic(5, Duration::from_millis(50), Duration::from_secs(2));

    let spawned = system.spawn(Counter { n: 0 }, cfg).unwrap();
    let counter_ref = spawned.actor();

    let state = AppState { counter: counter_ref };

    let app = Router::new()
        .route("/get", get(get_counter))
        .route("/add", post(add_counter))
        .route("/reset", post(reset_counter))
        .with_state(state)
        .layer(
            tower::ServiceBuilder::new()
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
