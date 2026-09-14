//! A mock of the upstream pricing document: one URL, one body, and a
//! count of how many times it was asked for.
//!
//! The count is the point. "A redelivered refresh does not fetch again"
//! is a claim about the world outside the process, and the only honest
//! way to assert it is to ask the thing that would have been fetched.
//! The body is served verbatim, so a test writes a LiteLLM-shaped
//! document and the real parse, the real acceptance and the real cache
//! write all run over it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

struct Inner {
    /// The document served next. Behind a lock so a test can change what
    /// upstream says between two refreshes.
    body: Mutex<String>,
    fetches: AtomicUsize,
}

/// In-process HTTP server serving one pricing document.
pub struct MockLitellm {
    inner: Arc<Inner>,
    url: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl MockLitellm {
    /// Bind an ephemeral port and serve `body` at
    /// [`url`](Self::url).
    pub async fn start(body: impl Into<String>) -> Self {
        let inner = Arc::new(Inner {
            body: Mutex::new(body.into()),
            fetches: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/model_prices.json", get(document))
            .with_state(inner.clone());
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind ephemeral port");
        let url = format!(
            "http://{}/model_prices.json",
            listener.local_addr().expect("local_addr")
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join_handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("mock pricing server fell over");
        });
        Self {
            inner,
            url,
            shutdown_tx: Some(shutdown_tx),
            join_handle: Some(join_handle),
        }
    }

    /// Where the document lives — pass to `PricingRefresh::with_upstream`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// How many times the document has been fetched.
    pub fn fetches(&self) -> usize {
        self.inner.fetches.load(Ordering::SeqCst)
    }

    /// Change what upstream says from the next fetch on.
    pub async fn serve(&self, body: impl Into<String>) {
        *self.inner.body.lock().await = body.into();
    }

    /// Stop the server and wait for its task.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for MockLitellm {
    fn drop(&mut self) {
        if let Some(handle) = self.join_handle.take() {
            handle.abort();
        }
    }
}

async fn document(State(inner): State<Arc<Inner>>) -> String {
    inner.fetches.fetch_add(1, Ordering::SeqCst);
    inner.body.lock().await.clone()
}
