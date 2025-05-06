# Sveltum 

Prototype adapter to connect Sveltekit and Axum allowing a Rust based Axum HTTP server to serve a Sveltekit app (With SSR)

This allows your to add a layer to your Axum HTTP server that can serve a SvelteKit app with SSR, server routes, client rendering and prerendering support.

Using the Rust based Deno runtime and its node compatibility the Rust based Axum layer can invoke the SvelteKit request handling logic for SSR and server routes.

This library allows you to build your main app HTTP server in Rust while serving a svelte app from it yet still retaining the SvelteKit features like SSR and server routes.

> [!CAUTION]
> Due to depending on the `deno_runtime` package you will see a substantially larger build time (Initial, incremental builds will take most of the time away) when including this package. You will also see a substantial increase in your binary size up to around 100Mb optimizing the binary for size you can get it down to around 60Mb

svelte.config.js:

```js
import adapter from 'svelte-adapter-sveltum';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	// Consult https://svelte.dev/docs/kit/integrations
	// for more information about preprocessors
	preprocess: vitePreprocess(),

	kit: {
		// adapter-auto only supports some environments, see https://svelte.dev/docs/kit/adapter-auto for a list.
		// If your environment is not supported, or you settled on a specific environment, switch out the adapter.
		// See https://svelte.dev/docs/kit/adapters for more information about adapters.
		adapter: adapter()
	}
};

export default config;
```

Axum:

```rust
use std::path::Path;

use axum::Router;
use sveltum_axum_layer::{ServeSvelte, ServeSvelteConfig};

#[tokio::main]
async fn main() {
    let server_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-app/build");
    let serve_svelte = ServeSvelte::create(
        server_path,
        ServeSvelteConfig {
            origin: Some("http://localhost:3000".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // build our application with a single route
    let app = Router::new().fallback_service(serve_svelte);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```