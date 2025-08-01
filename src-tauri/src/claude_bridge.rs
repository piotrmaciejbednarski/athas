use anyhow::{Context, Result, bail, ensure};
use interceptor::{InterceptorMessage, start_proxy_server_with_ws};
use log::info;
use serde::Serialize;
use std::process::Stdio;
use tauri::{AppHandle, Emitter};
use tokio::{
   process::{Child, Command},
   sync::mpsc,
};

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeStatus {
   pub running: bool,
   pub connected: bool,
   pub interceptor_running: bool,
}

pub struct ClaudeCodeBridge {
   claude_process: Option<Child>,
   pub claude_stdin: Option<tokio::process::ChildStdin>,
   interceptor_handle: Option<tokio::task::JoinHandle<()>>,
   server_handle: Option<tokio::task::JoinHandle<()>>,
   ws_connected: bool,
   app_handle: AppHandle,
}

impl ClaudeCodeBridge {
   pub fn new(app_handle: AppHandle) -> Self {
      Self {
         claude_process: None,
         claude_stdin: None,
         interceptor_handle: None,
         server_handle: None,
         ws_connected: false,
         app_handle,
      }
   }

   pub async fn start_interceptor(&mut self) -> Result<()> {
      if self.interceptor_handle.is_some() {
         bail!("Interceptor is already running");
      }

      log::info!("Starting interceptor as embedded service...");

      // todo: we shouldn't hardcode this
      let proxy_port = 3456;

      // Start the interceptor proxy server
      let (rx, ws_state, server_handle) = start_proxy_server_with_ws(proxy_port).await?;

      // Create channels for message distribution
      let (broadcast_tx, mut broadcast_rx) = mpsc::unbounded_channel::<InterceptorMessage>();
      let app_handle = self.app_handle.clone();

      // Spawn WebSocket broadcaster
      tokio::spawn(async move {
         while let Some(message) = broadcast_rx.recv().await {
            ws_state.broadcast(message);
         }
      });

      // Spawn message handler that forwards to frontend
      let message_handler = tokio::spawn(async move {
         let mut rx = rx;
         while let Some(message) = rx.recv().await {
            // Forward to WebSocket clients
            let _ = broadcast_tx.send(message.clone());

            // Emit to frontend
            let _ = app_handle.emit("claude-message", message);
         }
      });

      self.interceptor_handle = Some(message_handler);
      self.server_handle = Some(server_handle);
      self.ws_connected = true;

      log::info!("Interceptor started successfully on port {}", proxy_port);
      Ok(())
   }

   pub async fn start_claude_code(&mut self, workspace_path: Option<String>) -> Result<()> {
      ensure!(
         self.claude_process.is_none(),
         "Claude Code is already running"
      );

      let mut cmd = Command::new("claude");
      cmd.args([
         "--dangerously-skip-permissions",
         "--print",
         "--verbose",
         "--output-format",
         "stream-json",
         "--input-format",
         "stream-json",
      ])
      .env("ANTHROPIC_BASE_URL", "http://localhost:3456")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());

      if let Some(path) = &workspace_path {
         cmd.current_dir(path);
         log::info!("Starting Claude Code in workspace: {}", path);
      }

      let mut child = cmd
         .spawn()
         .context("Failed to spawn Claude process. Make sure 'claude' is in your PATH")?;

      self.claude_stdin = Some(child.stdin.take().context("Failed to get stdin")?);

      // Consume stdout and stderr to prevent broken pipe errors
      use tokio::io::{AsyncBufReadExt, BufReader};
      macro_rules! consume_stream {
         ($stream:expr) => {
            if let Some(stream) = $stream {
               tokio::spawn(async move {
                  let mut lines = BufReader::new(stream).lines();
                  while let Ok(Some(_)) = lines.next_line().await {}
               });
            }
         };
      }

      consume_stream!(child.stdout.take());
      consume_stream!(child.stderr.take());

      self.claude_process = Some(child);

      info!("Started `claude` successfully on {:?}", workspace_path);

      Ok(())
   }

   pub async fn stop_claude_process_only(&mut self) -> Result<()> {
      // Stop Claude Code process only, keep interceptor running
      if let Some(mut child) = self.claude_process.take() {
         let _ = child.kill().await;
         info!("Killed current `claude` process instance");
      }

      self.claude_stdin = None;

      Ok(())
   }

   pub fn get_status(&self) -> ClaudeStatus {
      ClaudeStatus {
         running: self.claude_process.is_some(),
         connected: self.ws_connected,
         interceptor_running: self.interceptor_handle.is_some(),
      }
   }
}
