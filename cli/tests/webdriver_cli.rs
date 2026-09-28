#![cfg(unix)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");
const PHONE_A: &str = "00000001-0000000000000001";
const PHONE_B: &str = "00000002-0000000000000002";

#[derive(Default)]
struct DriverState {
    created: Vec<Value>,
    deleted: Vec<String>,
    requests: Vec<(String, String, Value)>,
    sessions: HashMap<String, String>,
    reject_session: bool,
    reject_delete: bool,
    create_delay_ms: u64,
    element_epoch: u64,
    stale_elements: bool,
}

struct Fixture {
    root: tempfile::TempDir,
    port: u16,
    driver: Arc<Mutex<DriverState>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    daemon: Child,
}

impl Fixture {
    fn new() -> Self {
        Self::new_with_daemon_env(&[])
    }

    fn new_with_daemon_env(env: &[(&str, &str)]) -> Self {
        let root = tempfile::Builder::new()
            .prefix("ab-wd-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(root.path().join("config.json"), "{}").unwrap();
        std::fs::create_dir(root.path().join("tools")).unwrap();
        let xcrun = root.path().join("tools/xcrun");
        std::fs::write(&xcrun, format!(r#"#!/bin/sh
case "$*" in
  'simctl list devices --json') printf '%s\n' '{{"devices":{{"com.apple.CoreSimulator.SimRuntime.iOS-26-4":[{{"name":"iPhone Simulator","udid":"SIMULATOR-ONLY","state":"Booted","isAvailable":true}}]}}}}' ;;
  'xctrace list devices') printf '%s\n' '== Devices ==' 'Phone A (26.4) ({PHONE_A})' 'Phone B Max (26.4) (00000003-0000000000000003)' 'Phone B (26.4) ({PHONE_B})' '== Simulators ==' ;;
  *) echo "unexpected device operation: $*" >&2; exit 90 ;;
esac
"#)).unwrap();
        std::fs::set_permissions(&xcrun, std::fs::Permissions::from_mode(0o755)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 4723);
        listener.set_nonblocking(true).unwrap();
        let driver = Arc::new(Mutex::new(DriverState::default()));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_state = driver.clone();
        let server_stop = stopped.clone();
        let server = thread::spawn(move || {
            while !server_stop.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &server_state),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("mock driver: {e}"),
                }
            }
        });
        let mut daemon_cmd = isolated_command(&root, port);
        for (key, value) in env {
            daemon_cmd.env(key, value);
        }
        let daemon = daemon_cmd
            .env("AGENT_BROWSER_DAEMON", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root
            .path()
            .join("sockets/namespaces/wd/run/phone.sock")
            .exists()
        {
            assert!(Instant::now() < deadline, "daemon did not start");
            thread::sleep(Duration::from_millis(10));
        }
        Self {
            root,
            port,
            driver,
            stopped,
            server: Some(server),
            daemon,
        }
    }

    fn call(&self, args: &[&str]) -> Value {
        self.call_with_env(args, &[])
    }

    fn call_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        self.call_with_budget(args, env, Duration::from_secs(15))
    }

    fn call_with_budget(&self, args: &[&str], env: &[(&str, &str)], budget: Duration) -> Value {
        let mut cmd = isolated_command(&self.root, self.port);
        cmd.env_remove("AGENT_BROWSER_EXECUTABLE_PATH");
        cmd.args(["--json"]).args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        // Files avoid waiting for pipe EOF if a restarted daemon inherits a
        // descriptor. The CLI process itself must still finish within 15s.
        let stdout = tempfile::NamedTempFile::new_in(self.root.path()).unwrap();
        let stderr = tempfile::NamedTempFile::new_in(self.root.path()).unwrap();
        cmd.stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap());
        let mut child = cmd.spawn().unwrap();
        let deadline = Instant::now() + budget;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("CLI timed out: {args:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let out = std::fs::read(stdout.path()).unwrap();
        serde_json::from_slice(&out).unwrap_or_else(|e| {
            panic!(
                "{args:?}: {e}; stdout={} stderr={}",
                String::from_utf8_lossy(&out),
                std::fs::read_to_string(stderr.path()).unwrap()
            )
        })
    }

    fn ok(&self, args: &[&str]) -> Value {
        let result = self.call(args);
        assert_eq!(result["success"], true, "{args:?}: {result}");
        result["data"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Close only this fixture's backend; then terminate only our child.
        // Address our socket directly: CLI close can spawn another daemon
        // when the previous fixture process has already exited/restarted.
        if let Ok(mut socket) = std::os::unix::net::UnixStream::connect(
            self.root
                .path()
                .join("sockets/namespaces/wd/run/phone.sock"),
        ) {
            use std::io::{BufRead, BufReader};
            let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
            let _ = writeln!(
                socket,
                "{}",
                json!({"id":"fixture-stop","action":"__agent_browser_internal_shutdown"})
            );
            let _ = BufReader::new(socket).read_line(&mut String::new());
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn isolated_command(root: &tempfile::TempDir, port: u16) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.path().join("tools").display()),
        )
        .env("AGENT_BROWSER_CONFIG", root.path().join("config.json"))
        .env("AGENT_BROWSER_SOCKET_DIR", root.path().join("sockets"))
        .env("AGENT_BROWSER_NAMESPACE", "wd")
        .env("AGENT_BROWSER_SESSION", "phone")
        .env(
            "AGENT_BROWSER_EXECUTABLE_PATH",
            "/nonexistent/webdriver-regression-chrome",
        )
        .env("AGENT_BROWSER_APPIUM_PORT", port.to_string())
        .env("AGENT_BROWSER_ENCRYPTION_KEY", "00".repeat(32))
        .env("NO_COLOR", "1");
    cmd
}

fn serve(mut stream: TcpStream, shared: &Mutex<DriverState>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    let header_end;
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return
            }
            Err(error) => panic!("mock request: {error}"),
        };
        if n == 0 {
            return;
        } // Appium readiness TCP probe.
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(i) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            header_end = i + 4;
            break;
        }
    }
    let header = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let first = header.lines().next().unwrap();
    let parts: Vec<_> = first.split_whitespace().collect();
    let method = parts[0];
    let path = parts[1];
    let length = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let n = stream.read(&mut chunk).unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    let body: Value = if length == 0 {
        json!({})
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
    };
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state
        .requests
        .push((method.into(), path.into(), body.clone()));
    let (status, value) = if method == "POST" && path == "/session" {
        let caps = body["capabilities"]["alwaysMatch"].clone();
        if state.reject_session || !state.sessions.is_empty() {
            (
                500,
                json!({"error": "session not created", "message": "existing session or rejected target"}),
            )
        } else {
            thread::sleep(Duration::from_millis(state.create_delay_ms));
            state.created.push(caps);
            let sid = format!("session-{}", state.created.len());
            state.sessions.insert(sid.clone(), "about:blank".into());
            (
                200,
                json!({"sessionId": sid, "capabilities": body["capabilities"]["alwaysMatch"]}),
            )
        }
    } else {
        let parts: Vec<_> = path.split('/').collect();
        let sid = parts.get(2).copied().unwrap_or("");
        if !state.sessions.contains_key(sid) {
            (
                404,
                json!({"error": "invalid session id", "message": "session no longer exists"}),
            )
        } else if method == "DELETE" && parts.len() == 3 {
            if state.reject_delete {
                (
                    500,
                    json!({"error":"unknown error","message":"delete refused"}),
                )
            } else {
                state.sessions.remove(sid);
                state.deleted.push(sid.into());
                (200, Value::Null)
            }
        } else {
            match (method, parts.get(3).copied().unwrap_or("")) {
                ("POST", "url") => {
                    state
                        .sessions
                        .insert(sid.into(), body["url"].as_str().unwrap().into());
                    (200, Value::Null)
                }
                ("GET", "url") => (200, json!(state.sessions[sid])),
                ("GET", "title") => (200, json!("Phone Page")),
                ("POST", "execute") => {
                    let script = body["script"].as_str().unwrap();
                    // W3C execute/sync is a function body: expressions require return.
                    if state.stale_elements
                        && body["args"][0]["element-6066-11e4-a52e-4f735466cecf"]
                            == "snapshot-element"
                    {
                        (
                            404,
                            json!({"error":"stale element reference","message":"document replaced"}),
                        )
                    } else if script.contains("__agentBrowserSnapshot") {
                        (
                            200,
                            json!([{"role":"button","name":"Continue","selector":"#continue","element":{"element-6066-11e4-a52e-4f735466cecf":"snapshot-element"},"depth":0,"interactive":true}]),
                        )
                    } else if script.contains("user payload") {
                        (
                            200,
                            json!({"error":"user payload","message":"ordinary script result"}),
                        )
                    } else if script.contains("21 * 2") {
                        (
                            200,
                            if script.contains("return") {
                                json!(42)
                            } else {
                                Value::Null
                            },
                        )
                    } else if script.contains("throw") {
                        (
                            500,
                            json!({"error":"javascript error", "message":"fixture exception"}),
                        )
                    } else {
                        (200, json!(true))
                    }
                }
                ("POST", "element") if parts.len() == 4 => {
                    if body["value"] == "#missing" {
                        (404, json!({"error":"no such element","message":"missing"}))
                    } else {
                        (
                            200,
                            json!({"element-6066-11e4-a52e-4f735466cecf":format!("css-element-{}", state.element_epoch)}),
                        )
                    }
                }
                ("POST", "element") if parts.last() == Some(&"click") => {
                    if state.stale_elements && parts.get(4) == Some(&"snapshot-element") {
                        (
                            404,
                            json!({"error":"stale element reference","message":"document replaced"}),
                        )
                    } else {
                        (200, Value::Null)
                    }
                }
                _ => (404, json!({"error":"unknown command", "message":path})),
            }
        }
    };
    drop(state);
    let body = json!({"value": value}).to_string();
    let _ = write!(stream, "HTTP/1.1 {status} mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

#[test]
fn ios_independent_cli_calls_keep_selected_phone_and_session() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let data = f.ok(&["-p", "ios", "--device", PHONE_A, "get", "url"]);
    assert_eq!(data["url"], "https://phone.example/");
    let state = f.driver.lock().unwrap();
    assert_eq!(
        state.created.len(),
        1,
        "repeated CLI launch must reuse session"
    );
    assert_eq!(state.created[0]["appium:udid"], PHONE_A);
    assert!(state.deleted.is_empty());
    assert!(state
        .requests
        .iter()
        .any(|(method, path, _)| method == "GET" && path == "/session/session-1/url"));
}

#[test]
fn changed_ios_device_replaces_session_and_name_alias_reuses_actual_target() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/a",
    ]);
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_B,
        "open",
        "https://phone.example/b",
    ]);
    f.ok(&["-p", "ios", "--device", "Phone B", "get", "url"]);
    {
        let state = f.driver.lock().unwrap();
        assert_eq!(
            state.created.len(),
            2,
            "changed actual phone needs a new session"
        );
        assert_eq!(state.created[0]["appium:udid"], PHONE_A);
        assert_eq!(state.created[1]["appium:udid"], PHONE_B);
        assert_eq!(state.deleted, ["session-1"]);
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(method, _, _)| method == "DELETE")
                .count(),
            1,
            "one owner must delete the shared Appium session"
        );
    }
    let error = f.call(&["-p", "ios", "--device", "unknown-phone", "get", "url"]);
    assert_eq!(error["success"], false);
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("No iOS device matched selector"));
    assert_eq!(
        f.driver.lock().unwrap().created.len(),
        2,
        "unknown selector must not choose a default phone"
    );
}

#[test]
fn ios_eval_returns_expression_result_from_webdriver_session() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let data = f.ok(&["-p", "ios", "--device", PHONE_A, "eval", "21 * 2"]);
    assert_eq!(
        data["result"], 42,
        "WebDriver execute/sync needs a returned completion value"
    );
    assert_eq!(f.driver.lock().unwrap().created.len(), 1);
}

#[test]
fn webdriver_errors_surface_without_backend_or_device_fallback() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let error = f.call(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "eval",
        "throw new Error('fixture exception')",
    ]);
    assert_eq!(
        error["success"], false,
        "WebDriver error payload must not become a successful eval result: {error}"
    );
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("fixture exception"));
    let missing = f.call(&["-p", "ios", "--device", PHONE_A, "click", "#missing"]);
    assert_eq!(missing["success"], false);
    assert!(missing["error"].as_str().unwrap().contains("#missing"));
    assert_eq!(f.driver.lock().unwrap().created.len(), 1);
    assert_eq!(
        f.ok(&["-p", "ios", "--device", PHONE_A, "get", "url"])["url"],
        "https://phone.example/"
    );
}

#[test]
fn ios_snapshot_wait_and_click_use_existing_webdriver_session() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let snapshot = f.ok(&["-p", "ios", "--device", PHONE_A, "snapshot", "-i"]);
    assert!(snapshot["snapshot"].as_str().unwrap().contains("Continue"));
    let refs = snapshot["refs"].as_object().unwrap();
    let reference = format!("@{}", refs.keys().next().unwrap());
    f.ok(&["-p", "ios", "--device", PHONE_A, "wait", &reference]);
    f.ok(&[
        "-p", "ios", "--device", PHONE_A, "wait", "--text", "Continue",
    ]);
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "wait",
        "--url",
        "**/phone.example/**",
    ]);
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "wait",
        "--fn",
        "document.readyState === 'complete'",
    ]);
    f.ok(&["-p", "ios", "--device", PHONE_A, "wait", "--load", "load"]);
    f.ok(&["-p", "ios", "--device", PHONE_A, "wait", "1"]);
    f.ok(&["-p", "ios", "--device", PHONE_A, "click", &reference]);
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 1);
    assert!(state
        .requests
        .iter()
        .any(|(method, path, _)| method == "POST"
            && path == "/session/session-1/element/snapshot-element/click"));
    assert!(state
        .requests
        .iter()
        .filter(|(method, path, _)| method == "POST" && path.ends_with("/execute/sync"))
        .all(|(_, _, body)| body["script"].as_str().unwrap().contains("return")));
}

#[test]
fn changing_client_environment_udid_changes_target() {
    let f = Fixture::new();
    for phone in [PHONE_A, PHONE_B] {
        let result = f.call_with_env(
            &["-p", "ios", "open", "https://phone.example/"],
            &[("AGENT_BROWSER_IOS_UDID", phone)],
        );
        assert_eq!(result["success"], true, "{result}");
    }
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 2);
    assert_eq!(state.created[1]["appium:udid"], PHONE_B);
}

#[test]
fn failed_relaunch_never_falls_back_to_chrome() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    f.driver.lock().unwrap().reject_session = true;
    let failed = f.call(&[
        "-p",
        "ios",
        "--device",
        PHONE_B,
        "open",
        "https://phone.example/",
    ]);
    assert_eq!(failed["success"], false);
    let next = f.call(&["get", "url"]);
    assert_eq!(next["success"], false);
    assert!(
        !next["error"].as_str().unwrap().contains("Chrome"),
        "{next}"
    );
}

#[test]
fn changed_platform_configuration_replaces_existing_session() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let mut socket =
        UnixStream::connect(f.root.path().join("sockets/namespaces/wd/run/phone.sock")).unwrap();
    let command = json!({"id":"config-change","action":"launch","provider":"ios","udid":PHONE_A,"appiumPort":f.port,"platformVersion":"26.5"});
    writeln!(socket, "{command}").unwrap();
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line).unwrap();
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["success"], true, "{result}");
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 2);
    assert_eq!(state.created[1]["appium:platformVersion"], "26.5");
    assert_eq!(state.deleted, ["session-1"]);
}

#[test]
fn dead_session_recovers_exact_target_and_bad_endpoint_does_not_fall_back() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let bad = f.call_with_env(
        &["-p", "ios", "--device", PHONE_A, "get", "url"],
        &[("AGENT_BROWSER_APPIUM_PORT", "invalid")],
    );
    assert_eq!(bad["success"], false);
    assert!(bad["error"]
        .as_str()
        .unwrap()
        .contains("Invalid AGENT_BROWSER_APPIUM_PORT"));
    f.driver.lock().unwrap().sessions.clear();
    let dead = f.call(&["-p", "ios", "--device", PHONE_A, "get", "url"]);
    assert_eq!(
        dead["success"], true,
        "explicit launch must recover a confirmed expired session: {dead}"
    );
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 2);
    assert!(state
        .created
        .iter()
        .all(|caps| caps["appium:udid"] == PHONE_A));
}

#[test]
fn explicit_cli_selector_beats_client_environment_udid() {
    let f = Fixture::new();
    let result = f.call_with_env(
        &[
            "-p",
            "ios",
            "--device",
            PHONE_A,
            "open",
            "https://phone.example/",
        ],
        &[("AGENT_BROWSER_IOS_UDID", PHONE_B)],
    );
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(f.driver.lock().unwrap().created[0]["appium:udid"], PHONE_A);
}

#[test]
fn changed_appium_endpoint_replaces_session() {
    let f = Fixture::new();
    let other = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let port = other.port.to_string();
    let response = f.call_with_env(
        &[
            "-p",
            "ios",
            "--device",
            PHONE_A,
            "open",
            "https://phone.example/new-endpoint",
        ],
        &[("AGENT_BROWSER_APPIUM_PORT", &port)],
    );
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(f.driver.lock().unwrap().deleted, ["session-1"]);
    assert_eq!(other.driver.lock().unwrap().created.len(), 1);
    assert_eq!(
        other.driver.lock().unwrap().created[0]["appium:udid"],
        PHONE_A
    );
}

#[test]
fn direct_daemon_command_autolaunches_configured_ios_provider() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let f = Fixture::new_with_daemon_env(&[
        ("AGENT_BROWSER_PROVIDER", "ios"),
        ("AGENT_BROWSER_IOS_UDID", PHONE_A),
    ]);
    let mut socket =
        UnixStream::connect(f.root.path().join("sockets/namespaces/wd/run/phone.sock")).unwrap();
    writeln!(
        socket,
        "{}",
        json!({"id":"direct","action":"navigate","url":"https://phone.example/direct"})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(f.driver.lock().unwrap().created[0]["appium:udid"], PHONE_A);
}

#[test]
fn failed_session_delete_prevents_relaunch_and_surfaces_error() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    f.driver.lock().unwrap().reject_delete = true;
    let result = f.call(&[
        "-p",
        "ios",
        "--device",
        PHONE_B,
        "open",
        "https://phone.example/",
    ]);
    assert_eq!(result["success"], false);
    assert!(
        result["error"].as_str().unwrap().contains("delete refused"),
        "{result}"
    );
    assert_eq!(
        f.driver
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, path, _)| method == "POST" && path == "/session")
            .count(),
        1
    );
    f.driver.lock().unwrap().reject_delete = false;
}

#[test]
fn unknown_initial_device_does_not_enable_chrome_recovery() {
    let f = Fixture::new();
    let invalid = f.call(&["-p", "ios", "--device", "unknown-phone", "get", "url"]);
    assert_eq!(invalid["success"], false);
    let next = f.call(&["get", "url"]);
    assert_eq!(next["success"], false);
    assert!(
        !next["error"].as_str().unwrap().contains("Chrome"),
        "{next}"
    );
    assert!(
        next["error"]
            .as_str()
            .unwrap()
            .contains("WebDriver session unavailable"),
        "{next}"
    );
    assert!(f.driver.lock().unwrap().created.is_empty());
}

#[test]
fn script_result_with_error_property_is_not_a_protocol_error() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let result = f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "eval",
        "({error:'user payload',message:'ordinary script result'})",
    ]);
    assert_eq!(result["result"]["error"], "user payload");
}

#[test]
fn exact_device_name_beats_earlier_partial_match_and_ambiguous_names_fail() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        "Phone B",
        "open",
        "https://phone.example/",
    ]);
    assert_eq!(f.driver.lock().unwrap().created[0]["appium:udid"], PHONE_B);
    let ambiguous = f.call(&["-p", "ios", "--device", "Phone", "get", "url"]);
    assert_eq!(ambiguous["success"], false);
    assert!(ambiguous["error"]
        .as_str()
        .unwrap()
        .contains("Ambiguous iOS device selector"));
    assert_eq!(f.driver.lock().unwrap().created.len(), 1);
}

#[test]
fn review_omitted_selector_retains_exact_phone_with_simulator_available() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    assert_eq!(
        f.ok(&["-p", "ios", "get", "url"])["url"],
        "https://phone.example/"
    );
    let state = f.driver.lock().unwrap();
    assert_eq!(
        state.created.len(),
        1,
        "omission must retain the existing phone, not default to a simulator"
    );
    assert_eq!(state.created[0]["appium:udid"], PHONE_A);
    assert!(state.deleted.is_empty());
}

#[test]
fn review_cold_session_creation_survives_40_seconds_without_launch_retry() {
    let f = Fixture::new();
    f.driver.lock().unwrap().create_delay_ms = 40_000;
    let result = f.call_with_budget(
        &[
            "-p",
            "ios",
            "--device",
            PHONE_A,
            "open",
            "https://phone.example/",
        ],
        &[],
        Duration::from_secs(75),
    );
    assert_eq!(result["success"], true, "{result}");
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 1);
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, path, _)| method == "POST" && path == "/session")
            .count(),
        1
    );
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, path, _)| method == "GET" && path.ends_with("/url"))
            .count(),
        1,
        "an outer launch retry would issue an extra reuse probe"
    );
}

#[test]
fn review_expired_session_can_close_and_explicitly_recover() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    f.driver.lock().unwrap().sessions.clear();
    f.ok(&["close"]);
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/recovered",
    ]);
    f.driver.lock().unwrap().sessions.clear();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/relaunched",
    ]);
    assert_eq!(f.driver.lock().unwrap().created.len(), 3);
}

#[test]
fn review_refs_keep_element_identity_after_reorder_and_reject_navigation() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    let snapshot = f.ok(&["-p", "ios", "--device", PHONE_A, "snapshot", "-i"]);
    let reference = format!(
        "@{}",
        snapshot["refs"].as_object().unwrap().keys().next().unwrap()
    );
    f.driver.lock().unwrap().element_epoch += 1;
    f.ok(&["-p", "ios", "--device", PHONE_A, "click", &reference]);
    {
        let state = f.driver.lock().unwrap();
        assert!(
            state
                .requests
                .iter()
                .any(|(method, path, _)| method == "POST"
                    && path == "/session/session-1/element/snapshot-element/click"),
            "reordered CSS paths must not resolve a different element"
        );
    }
    f.driver.lock().unwrap().stale_elements = true;
    let stale = f.call(&["-p", "ios", "--device", PHONE_A, "click", &reference]);
    assert_eq!(
        stale["success"], false,
        "stale refs must not resolve an identically named element in another document: {stale}"
    );
    assert!(stale["error"].as_str().unwrap().contains("stale element"));
}

#[test]
fn review_close_waits_for_daemon_shutdown_before_independent_reopen() {
    let f = Fixture::new();
    for _ in 0..4 {
        f.ok(&[
            "-p",
            "ios",
            "--device",
            PHONE_A,
            "open",
            "https://phone.example/",
        ]);
        f.ok(&["close"]);
        assert!(
            !f.root
                .path()
                .join("sockets/namespaces/wd/run/phone.sock")
                .exists(),
            "close must finish retiring the daemon before another CLI can relaunch"
        );
    }
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 4);
    assert_eq!(state.deleted.len(), 4);
}

#[test]
fn review_failed_recreation_retains_phone_intent_for_omitted_retry() {
    let f = Fixture::new();
    f.ok(&[
        "-p",
        "ios",
        "--device",
        PHONE_A,
        "open",
        "https://phone.example/",
    ]);
    {
        let mut state = f.driver.lock().unwrap();
        state.sessions.clear();
        state.reject_session = true;
    }
    let failed = f.call(&["-p", "ios", "get", "url"]);
    assert_eq!(failed["success"], false, "{failed}");
    f.driver.lock().unwrap().reject_session = false;
    f.ok(&["-p", "ios", "get", "url"]);
    let state = f.driver.lock().unwrap();
    assert_eq!(state.created.len(), 2);
    for (method, path, body) in &state.requests {
        if method == "POST" && path == "/session" {
            assert_eq!(
                body["capabilities"]["alwaysMatch"]["appium:udid"], PHONE_A,
                "failed recreation must retain phone intent, never choose the available simulator"
            );
        }
    }
    drop(state);
    f.ok(&["close"]);
    f.ok(&["-p", "ios", "get", "url"]);
    assert_eq!(
        f.driver.lock().unwrap().created.last().unwrap()["appium:udid"],
        "SIMULATOR-ONLY",
        "deliberate close clears retained device intent"
    );
}
