pub mod layer;
pub mod queue;
pub mod runtime;

#[cfg(test)]
mod test {

    #[tokio::test]
    async fn main() {
        use crate::runtime::{HttpRequest, SvelteServerRuntime};
        use std::path::Path;

        let server_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("build");
        let handle = SvelteServerRuntime::create(server_path).unwrap();

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
    }
}
