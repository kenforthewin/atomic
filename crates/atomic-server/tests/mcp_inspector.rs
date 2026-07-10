//! Compatibility smoke test for the official MCP Inspector CLI.
//!
//! The test is part of `cargo test`, but runs opportunistically by default:
//! if Inspector is not already available through `npx --no-install`, it skips.
//! Set `ATOMIC_RUN_MCP_INSPECTOR=1` to require the check and allow `npx -y`
//! to install the package if needed.

use actix_web::{web, App, HttpServer};
use atomic_server::{
    export_jobs::ExportJobManager,
    migration_jobs::MigrationJobManager,
    log_buffer::LogBuffer,
    mcp::AtomicMcpTransport,
    mcp_auth::McpAuth,
    state::{AppState, ServerEvent, SetupClaimLimiter},
};
use std::{net::TcpListener, process::Output, sync::Arc, time::Duration};
use tokio::{process::Command, sync::broadcast};

const INSPECTOR_PACKAGE: &str = "@modelcontextprotocol/inspector";

struct TestServer {
    url: String,
    token: String,
    handle: actix_web::dev::ServerHandle,
    _temp: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (_info, token) = manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("mcp-inspector-test")
            .await
            .unwrap();
        let (event_tx, _) = broadcast::channel::<ServerEvent>(16);
        let state = web::Data::new(AppState {
            manager: Arc::clone(&manager),
            event_tx: event_tx.clone(),
            public_url: None,
            log_buffer: LogBuffer::new(16),
            export_jobs: ExportJobManager::for_tests(temp.path().join("exports")),
            migration_jobs: MigrationJobManager::for_tests(temp.path().join("migrations")),
            setup_token: None,
            dangerously_skip_setup_token: false,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: SetupClaimLimiter::new(),
        });
        let transport = AtomicMcpTransport::new(manager, event_tx, Duration::from_secs(30));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = HttpServer::new(move || {
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .service(transport.clone().scope()),
            )
        })
        .listen(listener)
        .unwrap()
        .run();
        let handle = server.handle();
        tokio::spawn(server);

        Self {
            url: format!("http://{addr}/mcp"),
            token,
            handle,
            _temp: temp,
        }
    }

    async fn stop(self) {
        self.handle.stop(true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_inspector_cli_can_list_atomic_tools() {
    let require_inspector = std::env::var_os("ATOMIC_RUN_MCP_INSPECTOR").is_some();
    let npx_args = if require_inspector {
        vec!["-y", INSPECTOR_PACKAGE]
    } else if inspector_available_without_install().await {
        vec!["--no-install", INSPECTOR_PACKAGE]
    } else {
        eprintln!(
            "skipping MCP Inspector smoke test; set ATOMIC_RUN_MCP_INSPECTOR=1 to require it"
        );
        return;
    };

    let server = TestServer::start().await;
    let output = run_inspector(&npx_args, &server.url, &server.token).await;
    server.stop().await;

    let output = output.unwrap_or_else(|error| panic!("failed to run MCP Inspector: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "MCP Inspector failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );

    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("semantic_search"),
        "Inspector tools/list output did not include Atomic tools\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
}

async fn inspector_available_without_install() -> bool {
    let mut command = Command::new("npx");
    command
        .arg("--no-install")
        .arg(INSPECTOR_PACKAGE)
        .arg("--version");

    matches!(
        tokio::time::timeout(Duration::from_secs(3), command.output()).await,
        Ok(Ok(output)) if output.status.success()
    )
}

async fn run_inspector(args: &[&str], url: &str, token: &str) -> std::io::Result<Output> {
    let mut command = Command::new("npx");
    command
        .args(args)
        .arg("--cli")
        .arg(url)
        .arg("--transport")
        .arg("http")
        .arg("--method")
        .arg("tools/list")
        .arg("--header")
        .arg(format!("Authorization: Bearer {token}"))
        .env("MCP_AUTO_OPEN_ENABLED", "false");

    match tokio::time::timeout(Duration::from_secs(60), command.output()).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "MCP Inspector timed out",
        )),
    }
}
