pub mod queue;
pub mod runtime;

#[tokio::test]
async fn main() {
    use runtime::{HttpRequest, SvelteServerRuntime};
    use std::{path::Path, time::Duration};

    let server_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("build");
    let handle = SvelteServerRuntime::create(server_path).unwrap();

    loop {
        let response = handle
            .request(HttpRequest {
                method: "GET".to_string(),
                headers: Vec::new(),
                url: "http://localhost:5173/sverdle".to_string(),
                body: None,
            })
            .await
            .unwrap();

        println!("response: {response:?}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
