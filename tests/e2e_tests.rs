#[cfg(feature = "e2e")]
mod e2e_tests {
    use aicore_router::config::Config;
    use futures::StreamExt;
    use reqwest::Client;
    use serde_json::json;
    use std::net::TcpStream;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use tokio::time::{sleep, timeout};

    struct AcrProcess {
        child: Child,
        port: u16,
    }

    impl AcrProcess {
        async fn start() -> Result<Self, Box<dyn std::error::Error>> {
            let port = find_available_port()?;

            let build_output = Command::new("cargo")
                .args(["build", "--bin", "acr"])
                .output()
                .expect("Failed to build acr binary");

            if !build_output.status.success() {
                panic!(
                    "Failed to build acr binary: {}",
                    String::from_utf8_lossy(&build_output.stderr)
                );
            }

            let mut child = Command::new("cargo")
                .args(["run", "--bin", "acr", "--", "--port", &port.to_string()])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to start acr process");

            let start_time = Instant::now();
            let timeout_duration = Duration::from_secs(30);

            loop {
                if start_time.elapsed() > timeout_duration {
                    let _ = child.kill();
                    panic!("acr server failed to start within 30 seconds");
                }

                if is_port_listening(port) {
                    break;
                }

                match child.try_wait() {
                    Ok(Some(status)) => {
                        panic!("acr process exited early with status: {status}");
                    }
                    Ok(None) => {
                        sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        panic!("Error checking process status: {e}");
                    }
                }
            }

            println!("acr server started on port {port}");
            Ok(AcrProcess { child, port })
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for AcrProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn find_available_port() -> Result<u16, Box<dyn std::error::Error>> {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        Ok(port)
    }

    fn is_port_listening(port: u16) -> bool {
        TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
    }

    async fn get_api_key_from_config() -> String {
        let config = Config::load(None).expect("Failed to load config.yaml for API key");
        config.api_key
    }

    // Test helper functions
    async fn make_request(
        client: &Client,
        url: &str,
        body: serde_json::Value,
        api_key: &str,
    ) -> reqwest::Response {
        timeout(
            Duration::from_secs(30),
            client
                .post(url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&body)
                .send(),
        )
        .await
        .expect("Request timed out")
        .expect("Request failed")
    }

    async fn assert_successful_response(response: reqwest::Response) {
        assert_eq!(response.status(), 200, "Expected successful response");
    }

    async fn assert_stream_response(response: reqwest::Response) {
        assert_eq!(
            response.status(),
            200,
            "Expected successful stream response"
        );

        let mut stream = response.bytes_stream();
        let mut chunk_count = 0;
        let mut total_bytes = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.expect("Failed to read stream chunk");
            chunk_count += 1;
            total_bytes += chunk.len();

            if chunk_count >= 5 || total_bytes > 1000 {
                break;
            }
        }

        assert!(
            chunk_count > 0,
            "Expected to receive at least one chunk from stream"
        );
        assert!(total_bytes > 0, "Expected to receive some data from stream");
    }

    // Request builders
    fn claude_request(stream: bool) -> serde_json::Value {
        json!({
            "model": "claude-sonnet-4",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello in one word"
                }
            ],
            "max_tokens": 10,
            "stream": stream
        })
    }

    fn openai_request(stream: bool) -> serde_json::Value {
        json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello in one word"
                }
            ],
            "max_tokens": 10,
            "stream": stream
        })
    }

    fn gemini_request() -> serde_json::Value {
        json!({
            "contents": [{
                "role": "user",
                "parts": [{
                    "text": "Say hello in one word"
                }]
            }]
        })
    }

    fn invalid_model_request() -> serde_json::Value {
        json!({
            "model": "invalid-model-name-123",
            "messages": [
                {
                    "role": "user",
                    "content": "Hello"
                }
            ],
            "max_tokens": 10
        })
    }

    // Test cases
    #[tokio::test]
    async fn test_claude_non_stream() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!("{}/v1/messages", acr.base_url()),
            claude_request(false),
            &api_key,
        )
        .await;

        assert_successful_response(response).await;
    }

    #[tokio::test]
    async fn test_claude_stream() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!("{}/v1/messages", acr.base_url()),
            claude_request(true),
            &api_key,
        )
        .await;

        assert_stream_response(response).await;
    }

    #[tokio::test]
    async fn test_openai_non_stream() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!("{}/v1/chat/completions", acr.base_url()),
            openai_request(false),
            &api_key,
        )
        .await;

        assert_successful_response(response).await;
    }

    #[tokio::test]
    async fn test_openai_stream() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!("{}/v1/chat/completions", acr.base_url()),
            openai_request(true),
            &api_key,
        )
        .await;

        assert_stream_response(response).await;
    }

    #[tokio::test]
    async fn test_gemini_non_stream() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!(
                "{}/gemini/models/gemini-2.5-flash:generateContent",
                acr.base_url()
            ),
            gemini_request(),
            &api_key,
        )
        .await;

        assert_successful_response(response).await;
    }

    #[tokio::test]
    async fn test_gemini_stream() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!(
                "{}/gemini/models/gemini-2.5-flash:streamGenerateContent",
                acr.base_url()
            ),
            gemini_request(),
            &api_key,
        )
        .await;

        assert_stream_response(response).await;
    }

    #[tokio::test]
    async fn test_invalid_model_name() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();
        let api_key = get_api_key_from_config().await;

        let response = make_request(
            &client,
            &format!("{}/v1/chat/completions", acr.base_url()),
            invalid_model_request(),
            &api_key,
        )
        .await;

        assert_eq!(
            response.status(),
            400,
            "Expected 400 for invalid model name"
        );
    }

    #[tokio::test]
    async fn test_invalid_api_key() {
        let acr = AcrProcess::start().await.expect("Failed to start acr");
        let client = Client::new();

        let response = make_request(
            &client,
            &format!("{}/v1/chat/completions", acr.base_url()),
            openai_request(false),
            "invalid-api-key-123",
        )
        .await;

        assert_eq!(response.status(), 401, "Expected 401 for invalid API key");
    }
}
