use runtime::{HttpRequest, SvelteServerMessage, SvelteServerRuntime};
use std::{path::Path, time::Duration};

mod runtime;

#[tokio::main]
async fn main() {
    let server_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("build");
    let handle = SvelteServerRuntime::create(server_path).unwrap();

    loop {
        handle
            .tx
            .send(SvelteServerMessage::HttpRequest {
                request: HttpRequest {
                    method: "GET".to_string(),
                    url: "http://localhost:5173/sverdle".to_string(),
                },
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
