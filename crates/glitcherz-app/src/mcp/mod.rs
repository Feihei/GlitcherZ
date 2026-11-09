//! The MCP server (S5, spec `mcp-server.md`).
//!
//! `glitcherz --mcp` speaks MCP over stdio: one JSON object per line in,
//! one per line out, everything else (logs, diagnostics) on stderr. It owns a
//! [`Session`] — the same transactional document the GUI edits — plus a path
//! sandbox and a plugin policy, and exposes six tools over it.
//!
//! This is a fourth headless mode rather than a new crate (S5-D1): the client
//! spawns a separate process either way, and `glitcherz-ui::headless` is
//! welded to the live app's node installation, so hoisting it out would be a
//! large refactor with exactly one consumer.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use glitcherz_command::Session;
use glitcherz_model::Project;
use serde_json::{Value, json};

mod protocol;
mod sandbox;
mod tools;

pub use sandbox::Sandbox;

/// The server's mutable state: one document, one root, one policy.
pub struct Server {
    pub(super) session: Session,
    pub(super) sandbox: Sandbox,
    pub(super) allow_plugins: bool,
    /// Where the current document came from / where a pathless save goes.
    pub(super) source: Option<PathBuf>,
    /// Set by `shutdown`; the loop exits after answering.
    stop: bool,
}

impl Server {
    pub(super) fn new(sandbox: Sandbox, allow_plugins: bool) -> Self {
        Self {
            session: Session::new(Project::default()),
            sandbox,
            allow_plugins,
            source: None,
            stop: false,
        }
    }

    /// Loads the `--open` document. `Err` carries the operator-facing message.
    fn preload(&mut self, raw: &str) -> Result<(), String> {
        let path = self.sandbox.resolve(raw)?;
        let project = glitcherz_model::file::load_project(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        self.session.replace(project);
        self.session.mark_clean();
        self.source = Some(path);
        Ok(())
    }

    /// Handles one input line. `None` means "no answer" (a notification).
    pub fn handle(&mut self, line: &str) -> Option<String> {
        let request = match protocol::parse(line) {
            Ok(request) => request,
            Err(code) => {
                let message = match code {
                    protocol::PARSE_ERROR => "the line is not JSON",
                    protocol::INVALID_REQUEST => "not a JSON-RPC request object",
                    _ => "the request parameters are not an object",
                };
                return Some(protocol::error(None, code, message));
            }
        };
        // Notifications never get a reply — not even for an unknown method,
        // which is how a client finds out it guessed wrong without a loop.
        let id = request.id.clone()?;
        Some(match request.method.as_str() {
            "initialize" => protocol::result(&id, self.initialize(&request.params)),
            "ping" => protocol::result(&id, json!({})),
            "tools/list" => protocol::result(&id, tools::list()),
            "tools/call" => match self.call_tool(&request.params) {
                Ok(result) => protocol::result(
                    &id,
                    json!({
                        "content": [{"type": "text", "text": result.text}],
                        "isError": result.is_error,
                    }),
                ),
                Err(message) => protocol::error(Some(&id), protocol::INVALID_PARAMS, &message),
            },
            "shutdown" => {
                self.stop = true;
                protocol::result(&id, json!({}))
            }
            other => protocol::error(
                Some(&id),
                protocol::METHOD_NOT_FOUND,
                &format!("unknown method `{other}`"),
            ),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        json!({
            "protocolVersion": protocol::negotiate(requested),
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "glitcherz",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })
    }

    fn call_tool(&mut self, params: &Value) -> Result<tools::ToolResult, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "`name` is required".to_string())?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        tools::call(self, name, &arguments)
    }
}

/// Serves the protocol on stdio until stdin closes. Returns the exit code
/// (spec §3, S5-D18).
pub fn run(root: &std::path::Path, allow_plugins: bool, open: Option<&std::path::Path>) -> u8 {
    let sandbox = match Sandbox::new(root) {
        Ok(sandbox) => sandbox,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    let mut server = Server::new(sandbox, allow_plugins);
    if let Some(path) = open
        && let Err(err) = server.preload(&path.display().to_string())
    {
        eprintln!("{err}");
        return 3;
    }
    // Operator-facing, on stderr: MCP clients only parse stdout.
    eprintln!(
        "glitcherz mcp: root {}, plugins {}, {} commands",
        server.sandbox.root().display(),
        if allow_plugins { "enabled" } else { "disabled" },
        glitcherz_command::command_specs().len(),
    );

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(err) => {
                eprintln!("cannot read stdin: {err}");
                return 1;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle(&line)
            && writeln!(stdout, "{response}")
                .and_then(|()| stdout.flush())
                .is_err()
        {
            return 1;
        }
        if server.stop {
            return 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        let root = std::env::temp_dir().join(format!("glitcherz_s5_mcp_{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create root");
        Server::new(Sandbox::new(&root).expect("sandbox"), false)
    }

    fn json_of(line: &str) -> Value {
        serde_json::from_str(line).expect("response is JSON")
    }

    #[test]
    fn initialize_echoes_the_version_and_declares_tools() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#)
            .expect("response");
        let value = json_of(&line);
        assert_eq!(value["id"], json!(1));
        assert_eq!(value["result"]["protocolVersion"], json!("2025-03-26"));
        assert_eq!(value["result"]["serverInfo"]["name"], json!("glitcherz"));
        assert!(value["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn an_unknown_protocol_version_falls_back() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#)
            .expect("response");
        assert_eq!(
            json_of(&line)["result"]["protocolVersion"],
            json!(protocol::LATEST_VERSION)
        );
    }

    #[test]
    fn notifications_get_no_answer() {
        let mut server = server();
        assert!(
            server
                .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        assert!(
            server
                .handle(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#)
                .is_none()
        );
    }

    #[test]
    fn ping_answers_with_an_empty_object() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#)
            .expect("response");
        let value = json_of(&line);
        assert_eq!(value["id"], json!("p"));
        assert_eq!(value["result"], json!({}));
    }

    #[test]
    fn tools_list_answers_with_the_six_tools() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .expect("response");
        let value = json_of(&line);
        assert_eq!(value["result"]["tools"].as_array().expect("tools").len(), 6);
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#)
            .expect("response");
        assert_eq!(json_of(&line)["error"]["code"], json!(-32601));
    }

    #[test]
    fn an_unknown_tool_is_invalid_params() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#)
            .expect("response");
        assert_eq!(json_of(&line)["error"]["code"], json!(-32602));
    }

    #[test]
    fn broken_json_does_not_kill_the_server() {
        let mut server = server();
        let line = server.handle("{oops").expect("response");
        assert_eq!(json_of(&line)["error"]["code"], json!(-32700));
        // The next frame still works.
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#)
            .expect("response");
        assert_eq!(json_of(&line)["id"], json!(5));
    }

    #[test]
    fn tools_call_wraps_the_report_as_text_content() {
        let mut server = server();
        let line = server
            .handle(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"project_info"}}"#)
            .expect("response");
        let value = json_of(&line);
        assert_eq!(value["result"]["isError"], json!(false));
        let text = value["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        let report: Value = serde_json::from_str(text).expect("report is JSON");
        assert_eq!(report["ok"], json!(true));
    }

    #[test]
    fn shutdown_answers_then_stops() {
        let mut server = server();
        assert!(
            server
                .handle(r#"{"jsonrpc":"2.0","id":7,"method":"shutdown"}"#)
                .is_some()
        );
        assert!(server.stop);
    }
}
