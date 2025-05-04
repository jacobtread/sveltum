use anyhow::Context;
use bytes::Bytes;
use deno_resolver::npm::{DenoInNpmPackageChecker, NpmResolver};
use deno_runtime::{
    deno_core::{
        FsModuleLoader, JsBuffer, JsRuntime, ModuleSpecifier, PollEventLoopOptions, ToJsBuffer,
        serde_v8::{self, from_v8, to_v8},
    },
    deno_fs::RealFs,
    deno_napi::v8::{Function, Global, Local, Object, Value},
    deno_permissions::PermissionsContainer,
    permissions::RuntimePermissionDescriptorParser,
    worker::{MainWorker, WorkerOptions, WorkerServiceOptions},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf, rc::Rc, sync::Arc};

use crate::core::{HttpRequest, HttpResponse};

pub struct ServerObject {
    /// Request handler function object from JS
    pub handler: Global<Value>,
    /// Hash set of pre-rendered routes for the svelte app
    pub prerendered: HashSet<String>,
    /// SvelteKit app path from the manifest
    pub app_path: String,
}

/// Wrapper for deserializing a created server object from a JS value
#[derive(Deserialize)]
struct JsServerObject<'a> {
    handler: serde_v8::Value<'a>,
    prerendered: Vec<String>,
    manifest: JsManifest,
}

/// Wrapper for deserializing a response from a JS value
#[derive(Deserialize)]
struct JsHttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: JsBuffer,
}

/// Wrapper for serializing a HTTP request to a JS value
#[derive(Serialize)]
struct JsHttpRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<ToJsBuffer>,
}

/// Wrapper for deserializing a svelte manifest from a JS value
#[derive(Debug, Deserialize)]
struct JsManifest {
    app_path: String,
}

pub fn create_js_worker(main_module: &ModuleSpecifier) -> JsRuntime {
    let permissions = PermissionsContainer::allow_all(Arc::new(
        RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys),
    ));

    let worker = MainWorker::bootstrap_from_options(
        // We do not have a "real" main module so this is just a placeholder (We don't use it)
        main_module,
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

pub async fn init_server(
    runtime: &mut JsRuntime,
    main_module: &ModuleSpecifier,
) -> anyhow::Result<ServerObject> {
    // Load the main module
    let module_id = runtime.load_main_es_module(main_module).await?;

    // Evaluate the main module
    let eval_future = runtime.mod_evaluate(module_id);
    runtime
        .with_event_loop_future(eval_future, PollEventLoopOptions::default())
        .await?;

    let namespace = runtime.get_module_namespace(module_id)?;

    let server_object = parse_server_object(runtime, namespace)?;

    Ok(server_object)
}

fn parse_server_object(
    runtime: &mut JsRuntime,
    server_object: Global<Object>,
) -> anyhow::Result<ServerObject> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    let server_object = Local::new(scope, server_object).cast();
    let server_object: JsServerObject = from_v8(scope, server_object)?;

    Ok(ServerObject {
        handler: Global::new(scope, server_object.handler.v8_value),
        prerendered: server_object.prerendered.into_iter().collect(),
        app_path: server_object.manifest.app_path,
    })
}

/// Creates and initializes new svelte server object
fn create_server_object(
    runtime: &mut JsRuntime,
    bootstrap: Global<Value>,
    server_path: PathBuf,
) -> anyhow::Result<Global<Value>> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    // Get the global object
    let global = scope.get_current_context().global(scope).cast();

    // Create a callable local function for the createServer function
    let create_server_fn = Local::new(scope, &bootstrap).cast::<Function>();

    // Turn the server path into a js value
    let path_value = to_v8(scope, server_path)?;

    let output = create_server_fn
        .call(scope, global, &[path_value])
        .context("failed to create server object")?;

    Ok(Global::new(scope, output))
}

pub fn convert_worker_response(
    runtime: &mut JsRuntime,
    response: Global<Value>,
) -> anyhow::Result<HttpResponse> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    // Convert JS value into Rust value
    let local_value: Local<'_, Value> = Local::new(scope, response);
    let response: JsHttpResponse = from_v8(scope, local_value)?;

    let bytes = response.body.as_ref();
    let body = Bytes::copy_from_slice(bytes);

    Ok(HttpResponse {
        status: response.status,
        headers: response.headers,
        body,
    })
}

/// Creates and initializes new svelte server object
pub fn invoke_handle_request(
    runtime: &mut JsRuntime,
    handle_fn: &Global<Value>,
    request: HttpRequest,
) -> anyhow::Result<Global<Value>> {
    // Get the handle scope
    let scope = &mut runtime.handle_scope();

    // Get the global object
    let global = scope.get_current_context().global(scope).cast();

    // Create a callable local function for the createServer function
    let handle_fn = Local::new(scope, handle_fn).cast::<Function>();

    let body = request.body.map(|body| ToJsBuffer::from(body.to_vec()));

    // Turn the server path into a js value
    let request_value = to_v8(
        scope,
        JsHttpRequest {
            url: request.url,
            method: request.method,
            headers: request.headers,
            body,
        },
    )?;

    let result = handle_fn
        .call(scope, global, &[request_value])
        .context("failed to call request handler")?;

    Ok(Global::new(scope, result))
}
