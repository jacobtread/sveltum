use std::{
    collections::HashSet, future::poll_fn, path::PathBuf, pin::Pin, rc::Rc, sync::Arc, task::Poll,
};

use anyhow::Context;
use deno_resolver::npm::{DenoInNpmPackageChecker, NpmResolver};
use deno_runtime::{
    deno_core::{
        FastString, FsModuleLoader, JsBuffer, JsRuntime, ModuleSpecifier, PollEventLoopOptions,
        ToJsBuffer,
        serde_v8::{from_v8, to_v8},
    },
    deno_fs::RealFs,
    deno_napi::v8::{self, Global},
    deno_permissions::PermissionsContainer,
    permissions::RuntimePermissionDescriptorParser,
    worker::{MainWorker, WorkerOptions, WorkerServiceOptions},
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, oneshot},
    task::{LocalSet, spawn_local},
};

use crate::queue::WakerQueue;

pub struct InitializedResponse {
    pub prerendered: HashSet<String>,
    pub app_path: String,
}

pub enum SvelteServerMessage {
    HttpRequest {
        // Request itself
        request: HttpRequest,
        // Channel for the response
        tx: oneshot::Sender<HttpResponse>,
    },
}

#[derive(Debug, Serialize)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Box<[u8]>>,
}

#[derive(Debug, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub struct SvelteServerHandle {
    tx: mpsc::Sender<SvelteServerMessage>,
}

impl SvelteServerHandle {
    pub async fn request(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(SvelteServerMessage::HttpRequest { request, tx })
            .await?;
        let result = rx.await?;
        Ok(result)
    }
}

pub fn create_js_worker() -> JsRuntime {
    let permissions = PermissionsContainer::allow_all(Arc::new(
        RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys),
    ));

    let worker = MainWorker::bootstrap_from_options(
        // We do not have a "real" main module so this is just a placeholder (We don't use it)
        &ModuleSpecifier::parse("file://dev/null").expect("failed to create file path url"),
        // Configuration
        WorkerServiceOptions::<
            DenoInNpmPackageChecker,
            NpmResolver<sys_traits::impls::RealSys>,
            sys_traits::impls::RealSys,
        > {
            blob_store: Default::default(),
            broadcast_channel: Default::default(),
            deno_rt_native_addon_loader: None,
            feature_checker: Default::default(),
            fs: Arc::new(RealFs),
            module_loader: Rc::new(FsModuleLoader),
            node_services: Default::default(),
            npm_process_state_provider: Default::default(),
            permissions,
            root_cert_store_provider: Default::default(),
            fetch_dns_resolver: Default::default(),
            shared_array_buffer_store: Default::default(),
            compiled_wasm_module_store: Default::default(),
            v8_code_cache: Default::default(),
        },
        WorkerOptions {
            extensions: vec![],
            ..Default::default()
        },
    );

    // We only need the js runtime from the worker
    worker.js_runtime
}

impl SvelteServerRuntime {
    pub async fn create(
        server_path: PathBuf,
    ) -> anyhow::Result<(InitializedResponse, SvelteServerHandle)> {
        let (handle_tx, handle_rx) = mpsc::channel(10);
        let (init_tx, init_rx) = oneshot::channel();

        std::thread::spawn(move || {
            // Create a new tokio runtime in the dedicated thread
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create script async runtime");

            let local_set = LocalSet::new();

            let mut worker = create_js_worker();

            let bootstrap_fn = create_bootstrap(&mut worker).unwrap();
            let server_object_promise =
                create_server_object(&mut worker, bootstrap_fn, server_path.clone()).unwrap();

            let server_object = runtime
                .block_on(resolve_promise(&mut worker, server_object_promise))
                .unwrap();

            let server_object = parse_server_object(&mut worker, server_object).unwrap();

            println!("created server object");

            let server_runtime = Self {
                worker,
                rx: handle_rx,
                response_queue: Default::default(),
                handler: server_object.handler,
            };

            if let Err(cause) = init_tx.send(InitializedResponse {
                prerendered: server_object.prerendered,
                app_path: server_object.manifest.app_path,
            }) {
                // Creator of the handle dropped the future already, don't keep going
                return;
            }

            local_set.spawn_local(SvelteServerRuntimeFuture {
                runtime: server_runtime,
            });

            runtime.block_on(local_set)
        });

        let response = init_rx.await?;
        let handle = SvelteServerHandle { tx: handle_tx };

        Ok((response, handle))
    }
}

struct ResponseEntry {
    // Response value from JS
    value: v8::Global<v8::Value>,
    // Sender for the parsed response
    tx: oneshot::Sender<HttpResponse>,
}

/// Wrapper around a deno runtime worker that has loaded a svelte
/// server and can perform svelte requests
pub struct SvelteServerRuntime {
    /// Underlying deno runtime worker
    worker: JsRuntime,

    /// Receiver for handle messages
    rx: mpsc::Receiver<SvelteServerMessage>,

    /// Queue for responses
    response_queue: WakerQueue<ResponseEntry>,

    /// Server handling object
    handler: v8::Global<v8::Value>,
}

struct SvelteServerRuntimeFuture {
    /// Runtime itself
    runtime: SvelteServerRuntime,
}

#[derive(Debug, Deserialize)]
struct JsHttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: JsBuffer,
}

#[derive(Debug, Serialize)]
struct JsHttpRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<ToJsBuffer>,
}

#[derive(Debug, Deserialize)]
struct JsManifest {
    app_path: String,
}

impl Future for SvelteServerRuntimeFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let runtime = &mut this.runtime;

        // Set the current waker
        runtime.response_queue.set_waker(cx.waker());

        // Handle waiting messages
        while let Some(response) = runtime.response_queue.next() {
            // Get the handle scope
            let scope = &mut runtime.worker.handle_scope();

            // Convert JS value into Rust value
            let local_value: v8::Local<'_, v8::Value> = v8::Local::new(scope, &response.value);
            let value: JsHttpResponse = from_v8(scope, local_value).unwrap();

            // Send back the response
            _ = response.tx.send(HttpResponse {
                status: value.status,
                headers: value.headers,
                body: Vec::from(value.body.as_ref()),
            });
        }

        // Poll the event loop
        let _ = runtime
            .worker
            .poll_event_loop(cx, PollEventLoopOptions::default());

        // Poll incoming script execute messages
        while let Poll::Ready(msg) = runtime.rx.poll_recv(cx) {
            let msg = match msg {
                Some(msg) => msg,
                None => return Poll::Ready(()),
            };

            match msg {
                SvelteServerMessage::HttpRequest { request, tx } => {
                    let global_promise =
                        invoke_handle_request(&mut runtime.worker, &runtime.handler, request)
                            .unwrap();
                    let resolve = runtime.worker.resolve(global_promise);
                    let res_queue = runtime.response_queue.clone();
                    spawn_local(async move {
                        let value = resolve.await.unwrap();
                        res_queue.push(ResponseEntry { value, tx });
                    });
                }
            }

            // Poll the event loop
            let _ = runtime
                .worker
                .poll_event_loop(cx, PollEventLoopOptions::default());
        }

        Poll::Pending
    }
}

fn create_bootstrap(runtime: &mut JsRuntime) -> anyhow::Result<v8::Global<v8::Value>> {
    let bootstrap = include_str!("../bootstrap.js");
    let output = runtime.execute_script("bootstrap.js", FastString::from_static(bootstrap))?;
    Ok(output)
}

async fn poll_with_event_loop<F>(runtime: &mut JsRuntime, mut future: F) -> F::Output
where
    F: Future + Unpin,
{
    let mut future = Pin::new(&mut future);

    poll_fn(move |cx| {
        if let Poll::Ready(result) = future.as_mut().poll(cx) {
            return Poll::Ready(result);
        }

        // Poll the event loop
        let _ = runtime.poll_event_loop(cx, PollEventLoopOptions::default());
        Poll::Pending
    })
    .await
}

/// Resolve a `promise` on the js runtime while polling the runtime itself
async fn resolve_promise(
    runtime: &mut JsRuntime,
    promise: v8::Global<v8::Value>,
) -> anyhow::Result<v8::Global<v8::Value>> {
    let resolve_future = runtime.resolve(promise);
    poll_with_event_loop(runtime, resolve_future)
        .await
        .map_err(anyhow::Error::new)
}

struct ServerObject {
    handler: v8::Global<v8::Value>,
    prerendered: HashSet<String>,
    manifest: JsManifest,
}

fn parse_server_object(
    runtime: &mut JsRuntime,
    server_object: v8::Global<v8::Value>,
) -> anyhow::Result<ServerObject> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    let server_object = v8::Local::new(scope, server_object);
    let server_object = server_object.try_cast::<v8::Object>()?;

    let handler_key = v8::String::new(scope, "handler")
        .context("failed to make handler key")?
        .try_cast()?;
    let handler = server_object
        .get(scope, handler_key)
        .context("failed to get handler")?;

    let prerendered_key = v8::String::new(scope, "prerendered")
        .context("failed to make prerendered key")?
        .try_cast()?;
    let prerendered = server_object
        .get(scope, prerendered_key)
        .context("failed to get prerendered")?;

    let manifest_key = v8::String::new(scope, "manifest")
        .context("failed to make manifest key")?
        .try_cast()?;
    let manifest = server_object
        .get(scope, manifest_key)
        .context("failed to get manifest")?;

    let prerendered: Vec<String> = from_v8(scope, prerendered)?;
    let manifest: JsManifest = from_v8(scope, manifest)?;

    Ok(ServerObject {
        handler: Global::new(scope, handler),
        prerendered: prerendered.into_iter().collect(),
        manifest,
    })
}

/// Creates and initializes new svelte server object
fn create_server_object(
    runtime: &mut JsRuntime,
    bootstrap: v8::Global<v8::Value>,
    server_path: PathBuf,
) -> anyhow::Result<v8::Global<v8::Value>> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    // Get the global object
    let global = scope.get_current_context().global(scope);

    // Create a callable local function for the createServer function
    let create_server_fn: v8::Local<'_, v8::Function> =
        v8::Local::new(scope, &bootstrap).try_cast()?;

    // Create a value for the global
    let global_value = global.try_cast()?;

    // Turn the server path into a js value
    let path_value = to_v8(scope, server_path)?;

    let output = create_server_fn
        .call(scope, global_value, &[path_value])
        .context("function provided no return value")?;

    Ok(Global::new(scope, output))
}

/// Creates and initializes new svelte server object
fn invoke_handle_request(
    runtime: &mut JsRuntime,
    handle_fn: &v8::Global<v8::Value>,
    request: HttpRequest,
) -> anyhow::Result<v8::Global<v8::Value>> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    // Get the global object
    let global = scope.get_current_context().global(scope);

    // Create a callable local function for the createServer function
    let handle_fn: v8::Local<'_, v8::Function> = v8::Local::new(scope, handle_fn).try_cast()?;

    // Create a value for the global
    let global_value = global.try_cast()?;

    // Turn the server path into a js value
    let request_value = to_v8(
        scope,
        JsHttpRequest {
            url: request.url,
            method: request.method,
            headers: request.headers,
            body: request.body.map(ToJsBuffer::from),
        },
    )?;

    let result = handle_fn
        .call(scope, global_value, &[request_value])
        .context("function provided no return value")?;

    Ok(Global::new(scope, result))
}
