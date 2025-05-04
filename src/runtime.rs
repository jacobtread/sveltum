use deno_runtime::{
    deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, futures::FutureExt},
    deno_napi::v8::{Global, Value},
};
use std::{
    cell::RefCell, collections::HashSet, future::poll_fn, path::PathBuf, pin::Pin, rc::Rc,
    task::Poll,
};
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

            let main_module_path = server_path.join("index.js");
            let main_module = match ModuleSpecifier::from_file_path(main_module_path) {
                Ok(value) => value,
                Err(err) => {
                    _ = init_tx.send(Err(anyhow::Error::msg("invalid main module path")));
                    return;
                }
            };

            // Create the JS worker
            let mut worker = create_js_worker(&main_module);

            // Initialize the svelte server
            let server_object = match runtime.block_on(init_server(&mut worker, &main_module)) {
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

            let worker = server_runtime.worker.clone();

            // Setup task set and spawn the runtime task
            let mut local_set = LocalSet::new();
            local_set.spawn_local(server_runtime);

            runtime.block_on(poll_fn(move |cx| {
                // Poll the current future once
                if local_set.poll_unpin(cx).is_ready() {
                    return Poll::Ready(());
                }

                if let Poll::Ready(t) = {
                    // Poll the runtime
                    worker
                        .borrow_mut()
                        .poll_event_loop(cx, PollEventLoopOptions::default())
                } {
                    // Runtime returned error
                    if t.is_err() {
                        return Poll::Ready(());
                    }

                    // Try polling future set again for finished promises
                    if local_set.poll_unpin(cx).is_ready() {
                        return Poll::Ready(());
                    }
                }

                Poll::Pending
            }));
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

            let worker = this.worker.clone();

            // Invoke the request handler
            let promise_future = {
                let worker: &mut JsRuntime = &mut worker.borrow_mut();
                let global_promise = match invoke_handle_request(worker, &this.handler, request) {
                    Ok(value) => value,

                    // Handle invoke error
                    Err(err) => {
                        _ = tx.send(Err(err));
                        continue;
                    }
                };
                worker.resolve(global_promise)
            };

            spawn_local(async move {
                let response = promise_future
                    .await
                    .map_err(anyhow::Error::new)
                    // Translate the response body
                    .and_then(|response| {
                        let worker: &mut JsRuntime = &mut worker.borrow_mut();
                        convert_worker_response(worker, response)
                    });

                _ = tx.send(response);
            });
        }

        Poll::Pending
    }
}
