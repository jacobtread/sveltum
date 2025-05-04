use deno_runtime::{
    deno_core::{JsRuntime, PollEventLoopOptions},
    deno_napi::v8::{Global, Value},
};
use std::{cell::RefCell, collections::HashSet, path::PathBuf, pin::Pin, rc::Rc, task::Poll};
use tokio::{
    sync::{mpsc, oneshot},
    task::{LocalSet, spawn_local},
};

use crate::{
    core::{HttpRequest, HttpResponse},
    js::{convert_worker_response, create_js_worker, init_server, invoke_handle_request},
};

pub struct InitializedResponse {
    pub prerendered: HashSet<String>,
    pub app_path: String,
}

struct SvelteServerMessage {
    // Request itself
    request: HttpRequest,
    // Channel for the response
    tx: oneshot::Sender<anyhow::Result<HttpResponse>>,
}

#[derive(Clone)]
pub struct SvelteServerHandle {
    tx: mpsc::Sender<SvelteServerMessage>,
}

impl SvelteServerHandle {
    pub async fn request(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(SvelteServerMessage { request, tx }).await?;
        let result = rx.await??;
        Ok(result)
    }
}

impl SvelteServerRuntime {
    pub async fn create(
        server_path: PathBuf,
    ) -> anyhow::Result<(InitializedResponse, SvelteServerHandle)> {
        let (handle_tx, handle_rx) = mpsc::channel(10);
        let (init_tx, init_rx) = oneshot::channel();

        std::thread::spawn(move || {
            // Create a new tokio runtime in the dedicated thread
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(value) => value,
                Err(err) => {
                    _ = init_tx.send(Err(anyhow::Error::new(err)));
                    return;
                }
            };

            // Create the JS worker
            let mut worker = create_js_worker();

            // Initialize the svelte server
            let server_object =
                match runtime.block_on(init_server(&mut worker, server_path.clone())) {
                    Ok(value) => value,
                    Err(err) => {
                        _ = init_tx.send(Err(err));
                        return;
                    }
                };

            let server_runtime = Self {
                worker: Rc::new(RefCell::new(worker)),
                rx: handle_rx,
                handler: server_object.handler,
            };

            // Notify that creation is complete
            if init_tx
                .send(Ok(InitializedResponse {
                    prerendered: server_object.prerendered,
                    app_path: server_object.app_path,
                }))
                .is_err()
            {
                // Creator of the handle dropped the future already, don't keep going
                return;
            }

            // Setup task set and spawn the runtime task
            let local_set = LocalSet::new();
            local_set.spawn_local(server_runtime);
            runtime.block_on(local_set)
        });

        let response = init_rx.await??;
        let handle = SvelteServerHandle { tx: handle_tx };

        Ok((response, handle))
    }
}

/// Wrapper around a deno runtime worker that has loaded a svelte
/// server and can perform svelte requests
pub struct SvelteServerRuntime {
    /// Access to the runtime
    worker: Rc<RefCell<JsRuntime>>,

    /// Receiver for handle messages
    rx: mpsc::Receiver<SvelteServerMessage>,

    /// Server handling object
    handler: Global<Value>,
}

impl Future for SvelteServerRuntime {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Poll incoming requests
        while let Poll::Ready(msg) = this.rx.poll_recv(cx) {
            let SvelteServerMessage { request, tx } = match msg {
                Some(msg) => msg,

                // All handles are closed
                None => break,
            };

            let handler = this.handler.clone();
            let worker = this.worker.clone();

            spawn_local(async move {
                let response = handle_worker_request(worker, request, handler).await;
                _ = tx.send(response);
            });
        }

        // Poll the event loop
        if this
            .worker
            .borrow_mut()
            .poll_event_loop(cx, PollEventLoopOptions::default())
            .is_ready()
        {
            // Event loop is complete and the channel is closed (We are finished)
            if this.rx.is_closed() {
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
}

/// Processes a request using the provided worker
async fn handle_worker_request(
    worker: Rc<RefCell<JsRuntime>>,
    request: HttpRequest,
    handle_fn: Global<Value>,
) -> anyhow::Result<HttpResponse> {
    // Invoke the JS handler to get a promise for the response
    let response_promise = {
        let worker = &mut *worker.borrow_mut();
        let global_promise = invoke_handle_request(worker, &handle_fn, request)?;
        worker.resolve(global_promise)
    };

    // Wait for the response promise
    let response = response_promise.await?;

    // Translate the response body
    let mut worker = worker.borrow_mut();
    convert_worker_response(&mut worker, response)
}
