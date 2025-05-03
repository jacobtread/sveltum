use std::{
    future::poll_fn,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::Poll,
};

use anyhow::Context;
use deno_resolver::npm::{DenoInNpmPackageChecker, NpmResolver};
use deno_runtime::{
    deno_core::{
        FastString, FsModuleLoader, JsBuffer, JsRuntime, ModuleSpecifier, PollEventLoopOptions,
        futures::FutureExt,
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
    sync::{Mutex, mpsc},
    task::LocalSet,
};

pub enum SvelteServerMessage {
    HttpRequest { request: HttpRequest },
}

#[derive(Serialize)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
}

#[derive(Debug, Deserialize)]
pub struct HttpResponse {
    pub status: i32,
    pub headers: Vec<(String, String)>,
    pub body: JsBuffer,
}

#[derive(Clone)]
pub struct SvelteServerHandle {
    pub tx: mpsc::Sender<SvelteServerMessage>,
}

impl SvelteServerHandle {}

/// Wrapper around a deno runtime worker that has loaded a svelte
/// server and can perform svelte requests
pub struct SvelteServerRuntime {
    /// Underlying deno runtime worker
    worker: MainWorker,

    /// Path to the underlying server
    server_path: PathBuf,

    /// Receiver for handle messages
    rx: mpsc::Receiver<SvelteServerMessage>,

    /// Local set for spawned promise tasks
    local_set: LocalSet,
}

impl SvelteServerRuntime {
    pub fn create(server_path: PathBuf) -> anyhow::Result<SvelteServerHandle> {
        let (tx, rx) = mpsc::channel(10);

        std::thread::spawn(move || {
            // Create a new tokio runtime in the dedicated thread
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create script async runtime");

            // Setup the main module file
            let js_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap.js");
            let main_module = ModuleSpecifier::from_file_path(js_path).unwrap();
            let permissions = PermissionsContainer::allow_all(Arc::new(
                RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys),
            ));

            let worker = MainWorker::bootstrap_from_options(
                &main_module,
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

            let server_runtime = Self {
                worker,
                rx,
                server_path,
                local_set: LocalSet::new(),
            };

            runtime.block_on(server_runtime.run());
        });

        Ok(SvelteServerHandle { tx })
    }

    pub async fn run(mut self) {
        let bootstrap_fn = create_bootstrap(&mut self.worker.js_runtime).unwrap();
        println!("initialized bootstrap");
        let server_object_promise = create_server_object(
            &mut self.worker.js_runtime,
            bootstrap_fn,
            self.server_path.clone(),
        )
        .unwrap();
        println!("created server object promise");

        let handle_fn = resolve_promise(&mut self.worker.js_runtime, server_object_promise)
            .await
            .unwrap();

        println!("created server object");

        let responses = Rc::new(Mutex::new(Vec::new()));

        poll_fn(move |cx| {
            let this = &mut self;

            {
                if let Ok(mut lock) = responses.try_lock() {
                    if let Some(response) = lock.pop() {
                        // Get the handle scope
                        let scope = &mut this.worker.js_runtime.handle_scope();
                        let local_value: v8::Local<'_, v8::Value> =
                            v8::Local::new(scope, &response);
                        let value: HttpResponse = from_v8(scope, local_value).unwrap();
                        println!("Got response {value:?}");
                    }
                }
            }

            // Initial pass when not messages are available
            {
                // Poll the promises local set
                _ = Pin::new(&mut this.local_set).poll(cx);

                // Poll event loop for any promises
                let _ = this
                    .worker
                    .js_runtime
                    .poll_event_loop(cx, PollEventLoopOptions::default());
            }

            // Poll incoming script execute messages
            while let Poll::Ready(msg) = this.rx.poll_recv(cx) {
                let msg = match msg {
                    Some(msg) => msg,
                    None => return Poll::Ready(()),
                };

                match msg {
                    SvelteServerMessage::HttpRequest { request } => {
                        let global_promise =
                            invoke_handle_request(&mut this.worker.js_runtime, &handle_fn, request)
                                .unwrap();
                        let resolve = this.worker.js_runtime.resolve(global_promise);
                        let res = responses.clone();
                        this.local_set.spawn_local(async move {
                            let result = resolve.await.unwrap();
                            res.lock().await.push(result);
                        });
                        println!("spawned a promise task");
                    }
                }

                // Poll the promises local set
                _ = Pin::new(&mut this.local_set).poll(cx);

                // Poll the event loop
                let _ = this
                    .worker
                    .js_runtime
                    .poll_event_loop(cx, PollEventLoopOptions::default());
            }

            Poll::Pending
        })
        .await;
    }
}

fn create_bootstrap(runtime: &mut JsRuntime) -> anyhow::Result<v8::Global<v8::Value>> {
    let bootstrap = include_str!("../bootstrap.js");
    let output = runtime.execute_script("bootstrap.js", FastString::from_static(bootstrap))?;
    Ok(output)
}

/// Resolve a `promise` on the js runtime while polling the runtime itself
async fn resolve_promise(
    runtime: &mut JsRuntime,
    promise: v8::Global<v8::Value>,
) -> anyhow::Result<v8::Global<v8::Value>> {
    let mut resolve_future = runtime.resolve(promise);

    poll_fn(move |cx| {
        if let Poll::Ready(result) = resolve_future.poll_unpin(cx) {
            return Poll::Ready(result.map_err(anyhow::Error::new));
        }

        // Poll the event loop
        let _ = runtime.poll_event_loop(cx, PollEventLoopOptions::default());
        Poll::Pending
    })
    .await
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

    let result = create_server_fn
        .call(scope, global_value, &[path_value])
        .context("function provided no return value")?;

    Ok(Global::new(scope, result))
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
    let request_value = to_v8(scope, request)?;

    let result = handle_fn
        .call(scope, global_value, &[request_value])
        .context("function provided no return value")?;

    Ok(Global::new(scope, result))
}
