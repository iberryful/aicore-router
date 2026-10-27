//! Spawn-once shared `acr` process for the e2e suite.
//!
//! The binary is built externally by `run_e2e_tests.sh` (avoids cargo lock
//! contention from inside a running `cargo test`). We just locate the
//! resulting binary, spawn it with our synthesized config, and probe
//! `GET /health` until ready.

#![cfg(feature = "e2e")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;
use tokio::time::sleep;

use super::config_synth::SynthesizedConfig;

/// Shared, lazily-initialized acr process. Tests retrieve it via
/// [`shared`]; the first call boots the binary, all subsequent calls hand
/// back a borrow.
static ACR: OnceCell<Acr> = OnceCell::const_new();

pub struct Acr {
    pub config: SynthesizedConfig,
    /// Child handle held in a Mutex so `Drop` can take it; `&'static`
    /// shared borrow forbids mutating the field directly.
    child: Mutex<Option<Child>>,
}

impl Acr {
    pub fn base_url(&self) -> String {
        self.config.base_url()
    }
}

impl Drop for Acr {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Get (and on first call, boot) the shared acr process.
pub async fn shared() -> &'static Acr {
    ACR.get_or_init(start).await
}

async fn start() -> Acr {
    let port = pick_free_port();
    let config = SynthesizedConfig::build(port);
    let base_url = config.base_url();

    let bin = locate_binary();
    let child = Command::new(&bin)
        .args(["--config", config.config_path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn `{}` — did `run_e2e_tests.sh` build it? ({e})",
                bin.display()
            );
        });

    // Build Acr first so its Drop kills the child even if readiness panics
    // (std::process::Child does NOT kill on drop).
    let mut acr = Acr {
        config,
        child: Mutex::new(Some(child)),
    };

    wait_for_ready(&acr).await;

    // Replace the static model list with what actually resolved against the
    // live backend. Family scenarios pick from this, so a model no enabled
    // provider serves is absent and its scenario skips rather than failing.
    if let Some(resolved) = fetch_resolved_models(&base_url).await {
        acr.config.model_names = resolved;
    }

    acr
}

/// Fetch the models acr actually resolved (`GET /v1/models`), returning their
/// client-facing ids. `None` if the request or parse fails.
async fn fetch_resolved_models(base_url: &str) -> Option<Vec<String>> {
    let resp = reqwest::Client::new()
        .get(format!("{base_url}/v1/models"))
        .header(
            "Authorization",
            format!("Bearer {}", super::config_synth::KEY_DEFAULT),
        )
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let ids: Vec<String> = json
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
        .collect();
    Some(ids)
}

/// Pre-pick a free TCP port by binding to `:0` and dropping the listener.
/// Tiny TOCTOU window between bind/drop and acr-bind, but acceptable for a
/// loopback-only test harness running serially.
fn pick_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port for e2e")
        .local_addr()
        .expect("local_addr from ephemeral listener")
        .port()
}

fn locate_binary() -> PathBuf {
    // CARGO_MANIFEST_DIR is set when cargo runs the integration tests.
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    manifest.join("target").join("debug").join("acr")
}

async fn wait_for_ready(acr: &Acr) {
    let url = format!("{}/health", acr.base_url());
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() > deadline {
            panic!(
                "acr did not become ready within 60s — url: {url}, config: {}",
                acr.config.config_path.display()
            );
        }
        // Has the child died on us?
        if let Ok(mut guard) = acr.child.lock()
            && let Some(child) = guard.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            panic!("acr exited early with status {status}");
        }
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            return;
        }
        sleep(Duration::from_millis(150)).await;
    }
}
