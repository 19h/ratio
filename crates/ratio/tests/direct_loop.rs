use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::Duration,
};

use serde_json::{Value, json};

fn mock(
    mut reply: impl FnMut(usize, Value) -> (u16, Value) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    mock_deferred(move |index, request| {
        let (status, response) = reply(index, request);
        (status, response, false)
    })
}

fn mock_deferred(
    mut reply: impl FnMut(usize, Value) -> (u16, Value, bool) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    mock_transport(move |index, request, _| reply(index, request))
}

fn mock_transport(
    mut reply: impl FnMut(usize, Value, BTreeMap<String, String>) -> (u16, Value, bool) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let thread = thread::spawn(move || {
        let mut deferred = Vec::new();
        for index in 0..100 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "no request {index}");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            let (request, headers) = read_mock_request(&mut socket);
            let (status, response, defer) = reply(index, request, headers);
            let done = status == 503 || response["test_terminal"] == true;
            deferred.push((socket, status, response));
            if !defer {
                for (mut socket, status, response) in deferred.drain(..) {
                    if let Some(sse) = response["test_sse"].as_str() {
                        write!(socket, "HTTP/1.1 {status} Response\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n").unwrap();
                        for part in sse.as_bytes().chunks(23) {
                            socket.write_all(part).unwrap();
                        }
                    } else {
                        let body = serde_json::to_vec(&response).unwrap();
                        write!(socket, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                        socket.write_all(&body).unwrap();
                    }
                }
            }
            if done {
                return;
            }
        }
    });
    (endpoint, thread)
}

fn read_mock_request(socket: &mut TcpStream) -> (Value, BTreeMap<String, String>) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let responses = line == "POST /v1/responses HTTP/1.1\r\n";
    assert!(responses || line == "POST /v1/chat/completions HTTP/1.1\r\n");
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .expect("request ended inside the header");
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    assert_eq!(headers["authorization"], "Bearer test-key");
    let mut body = vec![0; headers["content-length"].parse::<usize>().unwrap()];
    std::io::Read::read_exact(&mut reader, &mut body).unwrap();
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["model"], "test-model");
    assert!(request["stream"].is_boolean());
    if responses {
        assert!(request["max_output_tokens"].is_number());
        assert!(request["input"].is_array());
        assert_eq!(request["store"], false);
        assert!(request.get("stream_options").is_none());
    } else {
        assert!(request["max_completion_tokens"].is_number());
        assert_history(&request);
    }
    (request, headers)
}

fn stream_event(delta: Value, finish: Option<&str>) -> Value {
    json!({"choices": [{"index":0, "delta":delta, "finish_reason":finish}]})
}

fn stream_response(events: &[Value], finish: bool, terminal: bool) -> (u16, Value) {
    let mut raw = String::from(": provider heartbeat\r\n\r\n");
    for event in events {
        raw.push_str(&format!("data: {event}\r\n\r\n"));
    }
    if finish {
        raw.push_str("data:[DONE]\r\n\r\n");
    }
    (200, json!({"test_sse":raw,"test_terminal":terminal}))
}

fn stream_response_at_eof(events: &[Value], terminal: bool) -> (u16, Value) {
    let (status, mut response) = stream_response(events, false, terminal);
    response["test_sse"] = json!(
        response["test_sse"]
            .as_str()
            .unwrap()
            .trim_end_matches("\r\n")
    );
    (status, response)
}

fn assert_history(request: &Value) {
    let mut calls = std::collections::VecDeque::new();
    for message in request["messages"].as_array().unwrap() {
        if message["role"] == "tool" {
            assert_eq!(Some(&message["tool_call_id"]), calls.pop_front());
        } else {
            assert!(calls.is_empty(), "unanswered calls: {calls:?}");
            if let Some(tool_calls) = message["tool_calls"].as_array() {
                calls.extend(tool_calls.iter().map(|call| &call["id"]));
            }
        }
    }
    assert!(calls.is_empty());
}

fn cli(cwd: &Path, endpoint: &str, args: &[&str]) -> Output {
    cli_window(cwd, endpoint, "2048", args)
}

fn cli_window(cwd: &Path, endpoint: &str, window: &str, args: &[&str]) -> Output {
    cli_command(cwd, endpoint, window, args).output().unwrap()
}

fn ratio_binary() -> PathBuf {
    let built = PathBuf::from(env!("CARGO_BIN_EXE_ra"));
    if built.is_file() {
        return built;
    }
    // Cross-built integration fixtures can run alongside the target binary.
    std::env::current_exe()
        .unwrap()
        .with_file_name(format!("ra{}", std::env::consts::EXE_SUFFIX))
}

fn cli_command(cwd: &Path, endpoint: &str, window: &str, args: &[&str]) -> Command {
    let mut command = Command::new(ratio_binary());
    command
        .args([
            "-C",
            cwd.to_str().unwrap(),
            "--api-key",
            "test-key",
            "--model",
            "test-model",
            "--context-window",
            window,
            "--output-tokens",
            "256",
            "--endpoint",
            endpoint,
            "--headless",
            "--fixed-team",
            "--max-cycles",
            "2",
        ])
        .args(args);
    command
}

fn decision(verdict: &str, assessment: &str) -> (u16, Value) {
    let mut args = json!({"verdict":verdict,"assessment":assessment});
    if verdict == "needs_revision" {
        args["revision_instructions"] = json!(assessment);
    }
    let (status, mut response) = calls(&[tool("decision", "submit_verdict", args)]);
    if verdict != "needs_revision" {
        response["test_terminal"] = json!(true);
    }
    (status, response)
}

#[test]
fn streamed_completions_preserve_tool_order_reasoning_and_usage() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        r#"
[provider]
temperature = 0.8
top_p = 0.9
seed = 42
parallel_tool_calls = false
[provider.headers]
x-api-key = "provider-test"
"#,
    )
    .unwrap();
    let (endpoint, server) = mock_transport(|index, request, headers| {
        assert_eq!(request["stream"], true);
        assert_eq!(request["stream_options"]["include_usage"], true);
        assert_eq!(request["temperature"], 0.2);
        assert_eq!(request["top_p"], 0.9);
        assert_eq!(request["seed"], -7);
        assert_eq!(request["parallel_tool_calls"], false);
        assert_eq!(headers["x-api-key"], "provider-test");
        let (status, response) = match index {
            0 => stream_response(
                &[
                    stream_event(json!({"role":"assistant","content":"Create"}), None),
                    stream_event(json!({"content":" the fixture."}), Some("stop")),
                    json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
                ],
                true,
                false,
            ),
            1 => stream_response(
                &[
                    stream_event(
                        json!({"reasoning_content":"Check the order.", "tool_calls":[
                            {"index":1,"id":"read","type":"function","function":{"name":"read_", "arguments":"{\"path\":\""}},
                            {"index":0,"id":"create","type":"function","function":{"name":"create_file", "arguments":"{\"path\":\"fixture.txt\",\"content\":\""}}
                        ]}),
                        None,
                    ),
                    stream_event(
                        json!({"tool_calls":[
                            {"index":0,"function":{"arguments":"βeta\",\"explanation\":\"fixture\"}"}},
                            {"index":1,"function":{"name":"file","arguments":"fixture.txt\"}"}}
                        ]}),
                        Some("tool_calls"),
                    ),
                    json!({"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":20}}),
                ],
                true,
                false,
            ),
            2 => {
                let messages = request["messages"].as_array().unwrap();
                let assistant = &messages[messages.len() - 3];
                assert_eq!(assistant["reasoning_content"], "Check the order.");
                assert_eq!(assistant["tool_calls"][0]["id"], "create");
                assert_eq!(messages.last().unwrap()["tool_call_id"], "read");
                assert!(
                    messages.last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .contains("βeta")
                );
                stream_response_at_eof(
                    &[
                        stream_event(json!({"content":"Fixture created."}), Some("stop")),
                        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2}}),
                    ],
                    false,
                )
            }
            3 => {
                let mut packet = stream_event(
                    json!({"tool_calls":[{"index":0,"id":"verdict","type":"function","function":{"name":"submit_verdict","arguments":"{\"verdict\":\"approved\",\"assessment\":\"Verified streamed fixture.\"}"}}]}),
                    Some("tool_calls"),
                );
                packet["usage"] = json!({"prompt_tokens":10,"completion_tokens":5});
                stream_response_at_eof(&[packet], true)
            }
            _ => panic!("unexpected request {index}"),
        };
        (status, response, false)
    });
    let run = cli_window(
        temp.path(),
        &endpoint,
        "4096",
        &[
            "--goal",
            "Create a fixture",
            "--config",
            config.to_str().unwrap(),
            "--temperature",
            "0.2",
            "--seed",
            "-7",
        ],
    );
    succeeded(&run);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("fixture.txt")).unwrap(),
        "βeta"
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("Verified streamed fixture."));
    let ledger = std::fs::read_dir(temp.path().join(".ratio"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("budget-"))
        .unwrap();
    let ledger: Value = serde_json::from_slice(&std::fs::read(ledger.path()).unwrap()).unwrap();
    assert_eq!(ledger["requests"], 4);
    assert_eq!(ledger["tokens"], 95);
    server.join().unwrap();
}

#[test]
fn incomplete_stream_never_dispatches_a_tool() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, _| match index {
        0 => text("Create a fixture."),
        1 => stream_response_at_eof(
            &[stream_event(
                json!({"tool_calls":[
                    {"index":0,"id":"unsafe","type":"function","function":{
                        "name":"create_file","arguments":"{\"path\":\"fixture.txt\",\"content\":\"partial\"}"}}
                ]}),
                None,
            )],
            true,
        ),
        _ => panic!("unexpected request"),
    });
    let run = cli(
        temp.path(),
        &endpoint,
        &["--goal", "Test incomplete stream"],
    );
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("stream ended before completing"));
    assert!(!temp.path().join("fixture.txt").exists());
    server.join().unwrap();
}

#[test]
fn legacy_transport_omits_stream_options() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, request| {
        assert_eq!(request["stream"], false);
        assert!(request.get("stream_options").is_none());
        match index {
            0 => text("Inspect fixture."),
            1 => text("Inspection complete."),
            2 => decision("approved", "Verified with JSON transport."),
            _ => panic!("unexpected request"),
        }
    });
    succeeded(&cli(
        temp.path(),
        &endpoint,
        &["--goal", "Legacy transport", "--stream", "false"],
    ));
    server.join().unwrap();
}

#[test]
fn provider_diagnostics_preserve_short_metadata_and_redact_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        "[provider.headers]\nx-tenant = '1'\nx-api-key = 'keyZ'\n",
    )
    .unwrap();
    let (endpoint, server) = mock_transport(|index, _, headers| {
        assert_eq!(index, 0);
        assert_eq!(headers["x-tenant"], "1");
        assert_eq!(headers["x-api-key"], "keyZ");
        (
            400,
            json!({"error":{"message":"tenant 1, credential keyZ"},"test_terminal":true}),
            false,
        )
    });
    let run = cli(
        temp.path(),
        &endpoint,
        &[
            "--goal",
            "Inspect workspace",
            "--config",
            config.to_str().unwrap(),
        ],
    );
    assert!(!run.status.success());
    let diagnostics = String::from_utf8_lossy(&run.stderr);
    assert!(
        diagnostics.contains("tenant 1, credential [redacted]"),
        "{diagnostics}"
    );
    assert!(!diagnostics.contains("keyZ"));
    server.join().unwrap();
}

#[test]
fn active_time_budget_still_interrupts_a_stalled_request() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (finished, stopped) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        read_mock_request(&mut socket);
        stopped.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let run = cli(
        temp.path(),
        &endpoint,
        &["--goal", "Inspect workspace", "--max-run-secs", "2"],
    );
    finished.send(()).unwrap();
    assert_eq!(run.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&run.stdout).contains("Run budget exhausted: active time"));
    server.join().unwrap();
}

#[test]
fn headless_reports_progress_before_the_first_response() {
    use std::{process::Stdio, sync::mpsc};

    let temp = tempfile::tempdir().unwrap();
    let (release, first_response) = mpsc::channel();
    let (endpoint, server) = mock(move |index, _| match index {
        0 => {
            first_response.recv_timeout(Duration::from_secs(8)).unwrap();
            text("Inspect the fixture.")
        }
        1 => text("Fixture inspected."),
        2 => decision("approved", "Verified the fixture."),
        _ => panic!("unexpected request {index}"),
    });
    let mut child = cli_command(temp.path(), &endpoint, "8192", &["--goal", "Inspect"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (progress, live) = mpsc::channel();
    let read = thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if progress.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut saw_phase = false;
    loop {
        let line = live.recv_timeout(Duration::from_secs(5)).unwrap();
        saw_phase |= line.starts_with("Cycle 1 /");
        if line.contains("Requesting model") {
            break;
        }
    }
    assert!(saw_phase);
    assert!(child.try_wait().unwrap().is_none());
    release.send(()).unwrap();
    let output = child.wait_with_output().unwrap();
    succeeded(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("VERDICT: APPROVED"));
    assert!(!stdout.contains("Requesting model"));
    server.join().unwrap();
    read.join().unwrap();
}

#[test]
fn provider_profile_and_piped_request_produce_a_typed_team_preview() {
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, request| {
        assert_eq!(index, 0);
        assert!(
            request["messages"]
                .to_string()
                .contains("Check flux conservation")
        );
        assert!(request["tools"].to_string().contains("submit_team"));
        let (status, mut response) = calls(&[tool(
            "team",
            "submit_team",
            json!({
                "goal":"Check flux conservation", "message":"One implementation worker",
                "reviewers":[], "stakeholders":[],
                "workers":[{"name":"Numerics","persona":"Verify the flux calculation"}],
                "acceptance":["Conservation test passes"]
            }),
        )]);
        response["test_terminal"] = json!(true);
        (status, response)
    });
    let profile = temp.path().join("profile");
    std::fs::create_dir_all(profile.join("ratio")).unwrap();
    std::fs::write(
        profile.join("ratio/config.toml"),
        format!("endpoint = {endpoint:?}\nmodel = 'test-model'\ncontext_window = 8192\n"),
    )
    .unwrap();
    let mut process = Command::new(ratio_binary())
        .env("XDG_CONFIG_HOME", &profile)
        .env("APPDATA", &profile)
        .env_remove("RATIO_ENDPOINT")
        .env_remove("RATIO_MODEL")
        .env_remove("RATIO_CONTEXT_WINDOW")
        .args([
            "--api-key",
            "test-key",
            "--goal-stdin",
            "--plan-only",
            "-C",
            temp.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(b"Check flux conservation\n")
        .unwrap();
    let output = process.wait_with_output().unwrap();
    succeeded(&output);
    let proposal: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proposal["workers"][0]["name"], "Numerics");
    assert!(!temp.path().join(".ratio").exists());
    server.join().unwrap();
}

#[test]
fn positional_request_runs_with_the_generated_roster_and_persists_the_original() {
    use ra::session::Store;
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, request| {
        let tools = request["tools"].to_string();
        let messages = request["messages"].to_string();
        match index {
            0 => {
                assert!(messages.contains("Check flux"));
                calls(&[tool(
                    "team",
                    "submit_team",
                    json!({
                        "goal":"Verify conservation across cell interfaces","message":"Plan ready",
                        "reviewers":[],"stakeholders":[],
                        "workers":[{"name":"Numerics","persona":"Inspect the flux calculation"}],
                        "acceptance":["Check the conservation identity"]
                    }),
                )])
            }
            1 => {
                assert!(messages.contains("Check the conservation identity"));
                text("Check the conservation identity.")
            }
            2 => {
                assert!(messages.contains("Phase: coordinate"));
                assert!(!tools.contains("create_file"));
                text("The numerics worker owns this inspection.")
            }
            3 => {
                assert!(tools.contains("submit_plan"));
                assert!(tools.contains("depends_on"));
                calls(&[tool(
                    "plan",
                    "submit_plan",
                    json!({
                        "plan":"Inspect the flux calculation", "ownership":{"worker_1":[]},
                        "tasks":[{"id":"flux","worker":"worker_1","description":"Check flux","depends_on":[],"acceptance":["Conservation identity checked"]}]
                    }),
                )])
            }
            4 => {
                assert!(!tools.contains("create_file"));
                let schema = request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|tool| tool["function"]["name"] == "ack_plan")
                    .unwrap();
                let id = &schema["function"]["parameters"]["properties"]["plan_id"]["enum"][0];
                calls(&[tool("ack", "ack_plan", json!({"plan_id":id}))])
            }
            5 => {
                assert!(messages.contains("Phase: execute"));
                text("Conservation identity checked.")
            }
            6 => {
                assert!(messages.contains("Conservation identity checked"));
                decision("approved", "Conservation verified.")
            }
            _ => panic!("unexpected request {index}"),
        }
    });
    let output = Command::new(ratio_binary())
        .env("XDG_CONFIG_HOME", temp.path())
        .env("APPDATA", temp.path())
        .args([
            "-C",
            temp.path().to_str().unwrap(),
            "--headless",
            "--endpoint",
            &endpoint,
            "--api-key",
            "test-key",
            "--model",
            "test-model",
            "--context-window",
            "8192",
            "Check flux",
        ])
        .output()
        .unwrap();
    succeeded(&output);
    let run = Store::open(temp.path()).unwrap().load().unwrap();
    assert_eq!(run.request.as_deref(), Some("Check flux"));
    assert_eq!(run.goal, "Verify conservation across cell interfaces");
    assert_eq!(run.workers.as_ref().unwrap()[0].name, "Numerics");
    assert!(run.worker_plan.unwrap().graph.is_some());
    server.join().unwrap();
}

#[test]
fn live_worker_joins_only_after_both_mentors_and_amended_acknowledgements() {
    exercise_live_worker_addition(false);
}

#[test]
fn worker_objection_preserves_the_incumbent_plan_and_report() {
    exercise_live_worker_addition(true);
}

fn exercise_live_worker_addition(object: bool) {
    use ra::{
        config::Worker,
        session::{Phase, Session, Store, WorkerPlan, WritePath},
    };
    use std::{process::Stdio, sync::mpsc};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let original = Worker {
        name: "Original implementer".into(),
        persona: "Preserve existing work.".into(),
        model: None,
    };
    let original_plan_id = {
        let store = Store::open(root).unwrap();
        let mut session = Session::new("Extend the simulator".into());
        session.phase = Phase::Work;
        session.workers = Some(vec![original.clone()]);
        let mut plan = WorkerPlan::freeze(
            "worker_1 owns original.txt".into(),
            1,
            1,
            &[original],
            vec![vec![WritePath {
                path: "original.txt".into(),
                directory: false,
            }]],
        );
        plan.acknowledgements = vec![0];
        let id = plan.id.clone();
        session.worker_plan = Some(plan);
        store.save(&session).unwrap();
        id
    };
    let (arrived, incoming) = mpsc::channel();
    let (release, unblock) = mpsc::channel();
    let mut acks = std::collections::BTreeSet::new();
    let (endpoint, server) = mock(move |index, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        let tools = &request["tools"];
        let history = request["messages"].to_string();
        if index == 0 {
            assert!(system.contains("Original implementer"));
            arrived.send(()).unwrap();
            unblock.recv_timeout(Duration::from_secs(8)).unwrap();
            return text("Incumbent report preserved.");
        }
        if tools.to_string().contains("submit_worker_extension") {
            return calls(&[tool(
                "extension",
                "submit_worker_extension",
                json!({
                    "name":"Diagnostics","persona":"Write diagnostic evidence","mentor":"worker_1","stakeholder":null,
                    "paths":[{"path":"diagnostics.txt","directory":false}],
                    "task":{"id":"diagnostics","worker":"worker_2","description":"Write diagnostic evidence","depends_on":[],"acceptance":["diagnostics.txt exists"]}
                }),
            )]);
        }
        if system.contains("Read-only onboarding") {
            assert!(!tools.to_string().contains("create_file"));
            if history.contains("Queued") {
                return text("Onboarding briefing delivered.");
            }
            let message = |id, to| {
                tool(
                    id,
                    "send_message",
                    json!({"to":to,"message":"contract: diagnostics required"}),
                )
            };
            return if system.contains("incoming worker") {
                calls(&[
                    message("peer", "worker_1"),
                    message("requirement", "stakeholder_1"),
                ])
            } else {
                calls(&[message("brief", "worker_2")])
            };
        }
        if system.contains("Review this amended plan") {
            assert!(!tools.to_string().contains("create_file"));
            let newcomer = system.contains("Diagnostics");
            acks.insert(newcomer);
            let ack = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|schema| schema["function"]["name"] == "ack_plan")
                .unwrap();
            let id = &ack["function"]["parameters"]["properties"]["plan_id"]["enum"][0];
            return if object && !newcomer {
                calls(&[tool(
                    "objection",
                    "object_to_plan",
                    json!({"plan_id":id,"reason":"Integration contract is incomplete"}),
                )])
            } else {
                calls(&[tool("ack", "ack_plan", json!({"plan_id":id}))])
            };
        }
        if system.contains("You are parallel worker worker_2") {
            assert!(!object, "declined worker received execution tools");
            assert_eq!(acks.len(), 2);
            assert!(history.contains("Onboarding handoff"));
            return if history.contains("Created diagnostics.txt") {
                text("Diagnostics written.")
            } else {
                calls(&[tool(
                    "create",
                    "create_file",
                    json!({"path":"diagnostics.txt","content":"verified"}),
                )])
            };
        }
        assert!(history.contains("Incumbent report preserved"));
        decision("approved", "Both reports verified.")
    });
    let child = cli_command(root, &endpoint, "8192", &["--resume"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    incoming.recv_timeout(Duration::from_secs(8)).unwrap();
    let request = Command::new(ratio_binary())
        .args([
            "-C",
            root.to_str().unwrap(),
            "--control",
            "add-worker",
            "--worker-request",
            "Add diagnostics",
        ])
        .output()
        .unwrap();
    succeeded(&request);
    assert!(String::from_utf8_lossy(&request.stdout).contains("queued"));
    release.send(()).unwrap();
    let output = child.wait_with_output().unwrap();
    succeeded(&output);
    let session = Store::open(root).unwrap().load().unwrap();
    let reader = Store::reader(root).unwrap();
    let admissions = reader.worker_admissions(session.run_id()).unwrap();
    assert_eq!(admissions.len(), 1);
    assert_eq!(
        admissions[0]["state"],
        if object { "declined" } else { "admitted" }
    );
    let routes = reader
        .ipc_routes(
            session.run_id(),
            None,
            Some("OnboardWorker"),
            admissions[0]["id"].as_i64(),
        )
        .unwrap();
    assert!(routes.iter().any(|route| route["from"] == "worker_2"
        && route["to"] == "stakeholder_1"
        && route["state"] == "delivered"));
    let plan = session.worker_plan.unwrap();
    if object {
        assert!(!root.join("diagnostics.txt").exists());
        assert!(String::from_utf8_lossy(&output.stderr).contains("declined"));
        assert_eq!(session.workers.as_ref().unwrap().len(), 1);
        assert_eq!(plan.id, original_plan_id);
        assert_eq!(plan.acknowledgements, vec![0]);
    } else {
        assert_eq!(
            std::fs::read_to_string(root.join("diagnostics.txt")).unwrap(),
            "verified"
        );
        assert_eq!(session.workers.as_ref().unwrap().len(), 2);
        assert_ne!(plan.id, original_plan_id);
        assert_eq!(plan.acknowledgements, vec![0, 1]);
    }
    server.join().unwrap();
}

#[test]
fn truncated_output_retries_with_less_input_and_discards_partial_tools() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("large.txt"), "A".repeat(4500)).unwrap();
    let mut previous_bytes = usize::MAX;
    let (endpoint, server) = mock(move |index, request| {
        let messages = &request["messages"];
        match index {
            0 => text("Inspect the large file."),
            1 => calls(&[tool(
                "large",
                "read_file",
                json!({"path":"large.txt","offset":0}),
            )]),
            2 | 3 => {
                let bytes = messages.to_string().len();
                assert!(bytes < previous_bytes);
                previous_bytes = bytes;
                stream_response(
                    &[stream_event(
                        json!({"tool_calls":[
                            {"index":0,"id":"partial","function":{"name":"create_file","arguments":"{\"path\":\"partial.txt\""}}
                        ]}),
                        Some("length"),
                    )],
                    true,
                    false,
                )
            }
            4 => {
                assert!(messages.to_string().len() <= previous_bytes);
                assert_eq!(
                    messages.as_array().unwrap().last().unwrap()["tool_call_id"],
                    "large"
                );
                text("Inspected the file.")
            }
            5 => decision("approved", "Verified."),
            _ => panic!("unexpected request {index}"),
        }
    });
    let run = cli_window(
        temp.path(),
        &endpoint,
        "4096",
        &["--goal", "Test output recovery"],
    );
    succeeded(&run);
    assert!(!temp.path().join("partial.txt").exists());
    server.join().unwrap();
}

#[test]
fn prose_verdict_requests_repair_and_cannot_approve() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, request| match index {
        0 => text("Inspect the workspace."),
        1 => text("Inspection complete."),
        2 => {
            assert!(request["tools"].to_string().contains("submit_verdict"));
            text("```text\nVERDICT: APPROVED\n```")
        }
        3 => {
            assert!(
                request["messages"]
                    .to_string()
                    .contains("Protocol correction")
            );
            decision(
                "rejected",
                "The evidence is insufficient.\nVERDICT: APPROVED",
            )
        }
        _ => panic!("unexpected completion after terminal verdict"),
    });
    let run = cli(temp.path(), &endpoint, &["--goal", "Verdict validation"]);
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stdout).contains("VERDICT: REJECTED"));
    assert_eq!(
        String::from_utf8_lossy(&run.stdout)
            .lines()
            .filter(|line| line.trim_start().starts_with("VERDICT:"))
            .collect::<Vec<_>>(),
        ["VERDICT: REJECTED"]
    );
    server.join().unwrap();
}

#[test]
fn invalid_mixed_verdict_batch_fails_closed_after_bounded_repair() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, _| match index {
        0 => text("Inspect."),
        1 => text("Inspected."),
        2 => calls(&[
            tool("inspection", "read_file", json!({"path":"missing"})),
            tool(
                "verdict",
                "submit_verdict",
                json!({"verdict":"approved","assessment":"Done"}),
            ),
        ]),
        3 | 4 => {
            let (status, mut response) = calls(&[tool(
                "verdict",
                "submit_verdict",
                json!({"verdict":"needs_revision","assessment":"Missing instructions"}),
            )]);
            response["test_terminal"] = json!(index == 4);
            (status, response)
        }
        _ => panic!("invalid verdict exceeded repair allowance"),
    });
    let run = cli(temp.path(), &endpoint, &["--goal", "Mixed verdict"]);
    assert_eq!(run.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&run.stderr).contains("revision_instructions"));
    let session = ra::session::Store::open(temp.path())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(session.phase, ra::session::Phase::Review);
    assert!(session.verdict.is_none());
    let reader = ra::session::Store::reader(temp.path()).unwrap();
    let history = reader
        .audit(session.run_id(), Default::default(), 100)
        .unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|event| event.kind == "contract_error")
            .count(),
        3
    );
    assert!(history.iter().any(|event| {
        event.kind == "error"
            && event.data["text"]
                .as_str()
                .unwrap()
                .contains("revision_instructions")
    }));
    assert!(
        !history
            .iter()
            .any(|event| event.kind == "tool" && event.data["tool"] == "read_file")
    );
    server.join().unwrap();
}

#[test]
fn provider_retry_counts_against_run_budget() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, _| match index {
        0 => (429, json!({"error":{"message":"rate limited"}})),
        1 => {
            let (status, mut response) = text("Plan complete.");
            response["test_terminal"] = json!(true);
            response["usage"] = json!({"prompt_tokens":17,"completion_tokens":5});
            (status, response)
        }
        _ => panic!("provider request exceeded run budget"),
    });
    let run = cli(
        temp.path(),
        &endpoint,
        &["--goal", "Budget validation", "--max-requests", "2"],
    );
    assert_eq!(run.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&run.stdout).contains("provider attempts"));
    let ledger = std::fs::read_dir(temp.path().join(".ratio"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("budget-"))
        .unwrap();
    let ledger: Value = serde_json::from_slice(&std::fs::read(ledger.path()).unwrap()).unwrap();
    assert_eq!(ledger["requests"], 2);
    assert_eq!(ledger["tokens"], 22);
    server.join().unwrap();
}

#[test]
fn total_token_budget_is_opt_in_and_zero_overrides_a_configured_cap() {
    for (cap, override_cap, should_finish) in [
        ("", false, true),
        ("max_total_tokens = 8000000", false, false),
        ("max_total_tokens = 8000000", true, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("ratio.toml");
        std::fs::write(&profile, format!("[orchestration]\n{cap}\n")).unwrap();
        let (endpoint, server) = mock(move |index, _| match index {
            0 => {
                let (status, mut response) = text("Plan complete.");
                response["usage"] = json!({"total_tokens":8_000_000});
                response["test_terminal"] = json!(!should_finish);
                (status, response)
            }
            1 if should_finish => text("Work complete."),
            2 if should_finish => decision("approved", "Verified."),
            _ => panic!("unexpected request {index}"),
        });
        let mut args = vec!["--config", profile.to_str().unwrap(), "--goal", "Verify"];
        if override_cap {
            args.extend(["--max-total-tokens", "0"]);
        }
        let run = cli(temp.path(), &endpoint, &args);
        if should_finish {
            succeeded(&run);
        } else {
            assert_eq!(run.status.code(), Some(3));
            assert!(String::from_utf8_lossy(&run.stdout).contains("metered tokens"));
        }
        server.join().unwrap();
        if should_finish {
            continue;
        }

        let reader = ra::session::Store::reader(temp.path()).unwrap();
        let records = reader.runs().unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        let (_, phase) = reader
            .budget_resume_checkpoint(&record.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(phase, "Work");
        let (endpoint, server) = mock(|index, request| match index {
            0 => {
                assert!(request["messages"].to_string().contains("Plan complete."));
                text("Work complete.")
            }
            1 => decision("approved", "Verified after resuming."),
            _ => panic!("unexpected resumed request {index}"),
        });
        let resumed = cli(
            temp.path(),
            &endpoint,
            &[
                "--config",
                profile.to_str().unwrap(),
                "--resume",
                "--run-id",
                &record.run_id,
                "--max-total-tokens",
                "0",
            ],
        );
        succeeded(&resumed);
        server.join().unwrap();
        let records = reader.runs().unwrap();
        assert_eq!(records.len(), 1);
        let finished = &records[0];
        assert_eq!(finished.run_id, record.run_id);
        assert_eq!(finished.phase, "Approved");
        let metrics = reader.metrics(&record.run_id).unwrap();
        assert_eq!(metrics.provider_attempts, 3);
        assert!(metrics.metered_tokens > 8_000_000);
    }
}

#[test]
#[cfg(unix)]
fn parallel_result_survives_interrupt_while_another_tool_is_in_flight() {
    use ra::{
        config::Worker,
        session::{Phase, Session, Store, WorkerPlan, WritePath},
    };
    use std::{process::Stdio, time::Instant};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    {
        let store = Store::open(root).unwrap();
        let workers = ["Fast", "Slow"].map(|name| Worker {
            name: name.into(),
            persona: "Execute your assignment".into(),
            model: None,
        });
        let mut session = Session::new("Exercise parallel checkpoint durability".into());
        let mut plan = WorkerPlan::freeze(
            "Run both assignments".into(),
            1,
            1,
            &workers,
            vec![
                vec![WritePath {
                    path: "finished.txt".into(),
                    directory: false,
                }],
                vec![],
            ],
        );
        plan.acknowledgements = vec![0, 1];
        session.workers = Some(workers.to_vec());
        session.worker_plan = Some(plan);
        session.phase = Phase::Work;
        store.save(&session).unwrap();
    }
    let (endpoint, server) = mock(|index, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        let call = if system.starts_with("You are parallel worker worker_1,") {
            tool(
                "fast",
                "create_file",
                json!({"path":"finished.txt","content":"completed"}),
            )
        } else {
            assert!(system.starts_with("You are parallel worker worker_2,"));
            tool(
                "slow",
                "shell",
                json!({"command":"sleep 20","workdir":null}),
            )
        };
        let (status, mut response) = calls(&[call]);
        response["test_terminal"] = json!(index == 1);
        (status, response)
    });
    let mut child = cli_command(root, &endpoint, "8192", &["--resume"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let session = std::fs::read(root.join(".ratio/session.json")).unwrap();
        let data: Value = serde_json::from_slice(&session).unwrap();
        if data["worker_progress"][0]["dialogue"]["groups"]
            .to_string()
            .contains("Created finished.txt")
            && data["worker_progress"][1]["dialogue"]["in_flight"] == true
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "did not checkpoint fast worker: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(130));
    server.join().unwrap();
    let (endpoint, server) = mock(|_, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        let history = request["messages"].to_string();
        if system.starts_with("You are parallel worker worker_1,") {
            assert!(history.contains("Created finished.txt"));
            text("Fast work completed.")
        } else if system.starts_with("You are parallel worker worker_2,") {
            assert!(history.contains("interrupted"));
            text("The interrupted command was not retried.")
        } else {
            decision("approved", "Verified checkpoint.")
        }
    });
    succeeded(&cli_window(root, &endpoint, "8192", &["--resume"]));
    server.join().unwrap();
}

#[test]
fn detached_run_accepts_controls_and_can_resume_after_cancellation() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let _tui = TUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (connected, connection) = std::sync::mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept detached request: {error}"),
            }
        };
        read_mock_request(&mut socket);
        let delta = stream_event(json!({"content":"DETACHED-LIVE-MARKER"}), None);
        let chunk = format!("data: {delta}\n\n");
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{chunk}\r\n",
            chunk.len()
        )
        .unwrap();
        connected.send(()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut byte = [0];
        let result = std::io::Read::read(&mut socket, &mut byte);
        assert!(matches!(result, Ok(0)) || result.is_err());
    });
    let launch = cli(
        workspace,
        &endpoint,
        &["--detached", "--goal", "Managed task"],
    );
    succeeded(&launch);
    let launch: Value = serde_json::from_slice(&launch.stdout).unwrap();
    let run_id = launch["run"]["run_id"].as_str().unwrap();
    connection.recv_timeout(Duration::from_secs(8)).unwrap();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let (mut terminal, attached) = launch_tui(workspace, &endpoint, &["--attach", run_id]);
        let mut display = Vec::new();
        wait_for_screen(&mut terminal, &mut display, "DETACHED-LIVE-MARKER");
        terminal.write_all(b"\x1b").unwrap();
        wait_for_screen(&mut terminal, &mut display, "\x1b[?1049l");
        succeeded(&attached.finish(terminal));
    }
    let control = |action: &str, extra: &[&str]| {
        let output = Command::new(ratio_binary())
            .arg("-C")
            .arg(workspace)
            .args(["--control", action])
            .args(extra)
            .output()
            .unwrap();
        succeeded(&output);
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(control("status", &[])["run_id"], run_id);
    assert_eq!(control("pause", &[])["paused"], true);
    assert_eq!(
        control("guide", &["--guidance", "User note"])["accepted"],
        true
    );
    assert_eq!(control("cancel", &[])["accepted"], true);
    server.join().unwrap();
    let log = PathBuf::from(launch["log"].as_str().unwrap());
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("Requesting model")
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&log)
            .unwrap()
            .contains("ra: interrupted.")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached process did not exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let (endpoint, server) = mock(|index, _| match index {
        0 => text("Plan."),
        1 => text("Work."),
        2 => decision("approved", "Resume verified."),
        _ => panic!("unexpected request"),
    });
    let resume = || cli(workspace, &endpoint, &["--resume", "--run-id", run_id]);
    let output = loop {
        let output = resume();
        if !String::from_utf8_lossy(&output.stderr).contains("lock Ratio session") {
            break output;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached workspace lock did not release"
        );
        thread::sleep(Duration::from_millis(10));
    };
    succeeded(&output);
    server.join().unwrap();
}

#[test]
fn responses_transport_streams_tool_calls_and_round_trips_reasoning_items() {
    let root = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, request| {
        use serde_json::Value;
        let message = |text: &str| {
            json!({
                "type":"message","role":"assistant","status":"completed","id":"msg_mock",
                "content":[{"type":"output_text","text":text}]
            })
        };
        let call = |id: &str, name: &str, arguments: Value| {
            json!({
                "type":"function_call","status":"completed","id":format!("fc_{id}"),
                "call_id":id,"name":name,"arguments":arguments.to_string()
            })
        };
        let reasoning =
            json!({"type":"reasoning","id":"rs_mock","summary":[],"encrypted_content":"opaque"});
        let items = match index {
            0 => vec![message("Create the fixture.")],
            1 => {
                assert!(
                    request["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|tool| tool["name"] == "create_file")
                );
                vec![
                    reasoning.clone(),
                    call(
                        "call_write",
                        "create_file",
                        json!({
                            "path":"responses.txt","content":"tool result"
                        }),
                    ),
                ]
            }
            2 => {
                let input = request["input"].as_array().unwrap();
                assert_eq!(input[input.len() - 3], reasoning);
                assert_eq!(input[input.len() - 2]["call_id"], "call_write");
                assert_eq!(input[input.len() - 1]["type"], "function_call_output");
                assert_eq!(input[input.len() - 1]["call_id"], "call_write");
                assert!(
                    input.last().unwrap()["output"]
                        .as_str()
                        .unwrap()
                        .contains("Created")
                );
                vec![message("Fixture created.")]
            }
            3 => vec![call(
                "call_verdict",
                "submit_verdict",
                json!({
                    "verdict":"approved","assessment":"Responses path passed."
                }),
            )],
            _ => panic!("unexpected request {index}"),
        };
        let event = json!({"type":"response.completed","response":{
            "status":"completed","output":items,"usage":{"input_tokens":15,"output_tokens":7}
        }});
        stream_response_at_eof(&[event], index == 3)
    });
    succeeded(&cli(
        root.path(),
        &endpoint,
        &["--goal", "Responses fixture", "--provider-api", "responses"],
    ));
    server.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("responses.txt")).unwrap(),
        "tool result"
    );
}

#[test]
fn staged_run_publishes_only_the_verified_candidate_and_preserves_the_index() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(workspace)
            .args(args)
            .output()
            .unwrap();
        succeeded(&output);
        output.stdout
    };
    git(&["init", "--quiet"]);
    std::fs::write(workspace.join("tracked.txt"), "index version\n").unwrap();
    git(&["add", "tracked.txt"]);
    let index = std::fs::read(workspace.join(".git/index")).unwrap();
    std::fs::write(workspace.join("tracked.txt"), "dirty baseline\n").unwrap();
    let config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        config.path(),
        "[orchestration]\nstage_workspace = true\nverification_commands = [\"git diff --check\"]\n",
    )
    .unwrap();
    let live = workspace.to_owned();
    let (endpoint, server) = mock(move |index, request| {
        assert_eq!(
            std::fs::read_to_string(live.join("tracked.txt")).unwrap(),
            "dirty baseline\n"
        );
        match index {
            0 => text("Implement the change against the current contents."),
            1 => calls(&[tool(
                "change",
                "edit_file",
                json!({
                    "path":"tracked.txt","old_text":"dirty baseline","new_text":"approved contents"
                }),
            )]),
            2 => {
                assert!(
                    request["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .contains("Edited")
                );
                text("Edited tracked.txt")
            }
            3 => {
                assert!(
                    request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|message| message["content"]
                            .as_str()
                            .is_some_and(|body| body.contains("Verification passed: true")))
                );
                calls(&[tool("inspect", "read_file", json!({"path":"tracked.txt"}))])
            }
            4 => {
                assert!(
                    request["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .contains("approved contents")
                );
                decision("approved", "Verified candidate.")
            }
            _ => panic!("unexpected request {index}"),
        }
    });
    succeeded(&cli(
        workspace,
        &endpoint,
        &[
            "--goal",
            "Edit tracked file",
            "--config",
            config.path().to_str().unwrap(),
        ],
    ));
    server.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "approved contents\n"
    );
    assert_eq!(std::fs::read(workspace.join(".git/index")).unwrap(), index);
    let reader = ra::session::Store::reader(workspace).unwrap();
    let runs = reader.runs().unwrap();
    let events = reader.events(&runs[0].run_id, 0, 1000).unwrap();
    assert!(events.iter().any(|event| event.phase == "Publish"));
    assert_eq!(events.last().unwrap().phase, "Approved");
    let metrics = reader.metrics(&runs[0].run_id).unwrap();
    assert_eq!(metrics.provider_attempts, 5);
    let trace = reader.otlp_trace(&runs[0].run_id).unwrap();
    assert_eq!(
        trace["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"],
        "ratio.run"
    );
    let evaluation = reader
        .evaluate(
            &runs[0].run_id,
            &ratio_protocol::Evaluation {
                require_verified_candidate: true,
                max_provider_attempts: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(evaluation["passed"], true);
    let edits = events
        .iter()
        .flat_map(|event| &event.effects)
        .filter(|effect| effect.tool == "edit_file")
        .collect::<Vec<_>>();
    assert_eq!(edits.len(), 2);
    assert_ne!(edits[0].state, edits[1].state);
}

#[test]
fn resume_uses_refined_checkpoint_goal_and_archives_old_formats() {
    use ra::session::{Phase, Session, Store};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = root.join("ratio.toml");
    std::fs::write(&config, "goal = 'Original seed'\n").unwrap();
    let store = Store::open(root).unwrap();
    let mut session = Session::new("Refined goal".into());
    session.phase = Phase::Approved;
    session.assessment = "Refined result".into();
    std::fs::write(
        root.join(".ratio/session.json"),
        serde_json::to_vec(&session).unwrap(),
    )
    .unwrap();
    drop(store);
    let args = ["--config", config.to_str().unwrap(), "--resume"];
    succeeded(&cli(root, "http://127.0.0.1:1/v1", &args));
    assert_eq!(
        Store::reader(root).unwrap().runs().unwrap()[0].run_id,
        session.run_id()
    );
    let checkpoint = root.join(".ratio/session.json");
    std::fs::write(&checkpoint, r#"{"version":1,"goal":"old"}"#).unwrap();
    succeeded(&cli(root, "http://127.0.0.1:1/v1", &args));
    let legacy = tempfile::tempdir().unwrap();
    let root = legacy.path();
    std::fs::create_dir(root.join(".ratio")).unwrap();
    let checkpoint = root.join(".ratio/session.json");
    std::fs::write(&checkpoint, r#"{"version":1,"goal":"old"}"#).unwrap();
    let resume = cli(root, "http://127.0.0.1:1/v1", &args);
    assert!(
        String::from_utf8_lossy(&resume.stderr).contains("unsupported Ratio checkpoint version")
    );
    assert!(
        std::fs::read_to_string(&checkpoint)
            .unwrap()
            .contains("\"version\":1")
    );
    let (endpoint, server) = mock(|index, _| {
        if index < 2 {
            text("Done.")
        } else {
            decision("approved", "Verified.")
        }
    });
    succeeded(&cli(
        root,
        &endpoint,
        &["--config", config.to_str().unwrap()],
    ));
    server.join().unwrap();
    let old = std::fs::read_dir(root.join(".ratio"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("session-old-")
        })
        .unwrap();
    assert!(
        std::fs::read_to_string(old.path())
            .unwrap()
            .contains("\"version\":1")
    );
}

fn text(content: &str) -> (u16, Value) {
    (
        200,
        json!({"choices": [{"finish_reason":"stop", "message":{"role":"assistant","content":content}}]}),
    )
}

fn tool(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}

fn calls(calls: &[Value]) -> (u16, Value) {
    (
        200,
        json!({"choices": [{"finish_reason":"tool_calls", "message":{"role":"assistant","content":null,"tool_calls":calls}}]}),
    )
}

fn succeeded(run: &Output) {
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

fn diagnostics_script() -> &'static str {
    if cfg!(windows) {
        "for /L %i in (1,1,100) do @echo abcdefghijklmnopqrstuvwxyz"
    } else {
        "yes abcdefghijklmnopqrstuvwxyz | head -n 100"
    }
}

#[test]
fn inspection_shell_opt_in_keeps_native_edits_gated() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(temp.path().join("fixture.txt"), "original").unwrap();
    std::fs::write(
        &config,
        r#"
[orchestration]
allow_inspection_shell = true
[[workers]]
name = "Inspector"
persona = "Check fixture evidence."
"#,
    )
    .unwrap();
    let (endpoint, server) = mock(|index, request| {
        assert_history(&request);
        let tools = request["tools"].as_array().unwrap();
        let find = |name: &str| tools.iter().find(|tool| tool["function"]["name"] == name);
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        let working = system.contains("Phase: execute");
        let shell = &find("shell").unwrap()["function"]["parameters"];
        assert_eq!(shell["properties"]["command"]["type"], "array");
        assert_eq!(
            shell["properties"]["workdir"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(shell["required"], json!(["command", "workdir"]));
        assert_eq!(find("create_file").is_some(), working);
        assert_eq!(find("edit_file").is_some(), working);
        assert_eq!(system.contains("Use shell to inspect or verify"), !working);
        match index {
            0 => text("Check the fixture."),
            1 => {
                assert!(system.contains("Phase: coordinate"));
                let fail = if cfg!(windows) {
                    "cmd /D /C exit /B 7"
                } else {
                    "(exit 7)"
                };
                calls(&[
                    tool(
                        "shell",
                        "shell",
                        json!({"command":["echo inspection-probe", fail, "echo inspection-done"],"workdir":null}),
                    ),
                    tool(
                        "create",
                        "create_file",
                        json!({"path":"forbidden.txt","content":"bad"}),
                    ),
                    tool(
                        "edit",
                        "edit_file",
                        json!({"path":"fixture.txt","old_text":"original","new_text":"bad"}),
                    ),
                ])
            }
            2 => {
                let output = messages
                    .iter()
                    .find(|message| message["tool_call_id"] == "shell")
                    .unwrap()["content"]
                    .as_str()
                    .unwrap();
                let lines = output.lines().collect::<Vec<_>>();
                assert!(
                    lines
                        .windows(3)
                        .any(|lines| lines == ["stdout:", "inspection-probe", "inspection-done"]),
                    "{output}"
                );
                let history = request["messages"].to_string();
                for tool in ["create_file", "edit_file"] {
                    assert!(history.contains(&format!("Tool error: tool {tool} is unavailable")));
                }
                text("The fixture was inspected.")
            }
            3 => calls(&[tool(
                "plan",
                "submit_plan",
                json!({
                    "plan":"Check the fixture.","ownership":{"worker_1":[]}
                }),
            )]),
            4 => {
                let ack = find("ack_plan").unwrap();
                let id = &ack["function"]["parameters"]["properties"]["plan_id"]["enum"][0];
                calls(&[tool("ack", "ack_plan", json!({"plan_id":id}))])
            }
            5 => text("The fixture is intact."),
            6 => decision("approved", "Verified the fixture."),
            _ => panic!("unexpected request {index}"),
        }
    });
    let run = cli_window(
        temp.path(),
        &endpoint,
        "8192",
        &[
            "--config",
            config.to_str().unwrap(),
            "--goal",
            "Verify fixture",
        ],
    );
    succeeded(&run);
    server.join().unwrap();
    assert!(!temp.path().join("forbidden.txt").exists());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("fixture.txt")).unwrap(),
        "original"
    );
}

#[test]
fn binary_line_read_delivers_hex_to_the_model_on_the_first_call() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("image.bin"),
        [0x7f, b'E', b'L', b'F', 0, 0xff],
    )
    .unwrap();
    let (endpoint, server) = mock(|index, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("Binary reads: read_file with hex:true, offset, length."));
        match index {
            0 => calls(&[tool(
                "binary",
                "read_file",
                json!({"path":"image.bin","start_line":1}),
            )]),
            1 => {
                assert_history(&request);
                let result = request["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(result["tool_call_id"], "binary");
                let content = result["content"].as_str().unwrap();
                assert!(content.contains("Hex (auto) 0x0..0x6 / 0x6"), "{content}");
                assert!(content.contains("7f 45 4c 46 00 ff"), "{content}");
                text("The fixture starts with the expected byte sequence.")
            }
            2 => text("Inspection complete."),
            3 => decision("approved", "Verified the binary preview."),
            _ => panic!("unexpected request {index}"),
        }
    });
    succeeded(&cli(
        temp.path(),
        &endpoint,
        &["--goal", "Inspect the binary fixture"],
    ));
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn mcp_inspection_is_available_to_the_team_before_acknowledgement() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        r#"
[[workers]]
name = "Builder"
persona = "Implement the assigned work."
[[reviewers]]
name = "Verifier"
persona = "Inspect evidence."
phases = ["planning"]
[[stakeholders]]
name = "Adviser"
persona = "Check requirements."
phases = ["planning"]
[mcp_servers.fixture]
command = "/bin/sh"
args = ["-c", '''
    IFS= read -r frame
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}'
    IFS= read -r frame
    id=1
    while IFS= read -r frame; do
        id=$((id + 1))
        case "$frame" in
            *tools/list*) result='{"tools":[{"name":"inspect","inputSchema":{}},{"name":"change","inputSchema":{}}]}' ;;
            *'"name":"inspect"'*) result='{"fixture_observed":true}' ;;
            *'"name":"change"'*) result='{"fixture_changed":true}' ;;
            *) exit 1 ;;
        esac
        printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
    done
''']
allow_all = true
inspection_tools = ["inspect"]
lifetime = "worker"
instructions = "Use inspect to verify the fixture."
[mcp_servers.work_only]
command = "/bin/sh"
allow_all = true
instructions = "Use the work-only server during execution."
"#,
    )
    .unwrap();
    let mut observed = std::collections::BTreeSet::new();
    let (endpoint, server) = mock(move |_, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        let task = messages[1]["content"].as_str().unwrap();
        assert!(system.contains("MCP fixture: Use inspect to verify the fixture."));
        let schemas = request["tools"].as_array().unwrap();
        let schema = |name: &str| {
            schemas
                .iter()
                .find(|schema| schema["function"]["name"] == name)
        };
        assert!(schema("mcp_list").is_some());
        assert!(schema("tool_search").is_some());
        assert!(schema("mcp_call").is_some());
        assert!(schema("inspect").is_none());
        assert!(schema("change").is_none());
        let mcp = |name| tool(name, "mcp_call", json!({"server":"fixture","name":name}));
        let history = request["messages"].to_string();
        let working = system.contains("Phase: execute");
        assert_eq!(schema("edit_file").is_some(), working);
        assert_eq!(system.contains("MCP work_only:"), working);
        if working {
            assert!(observed.contains("acknowledgement"));
            if history.contains("fixture_changed") {
                return text("Completed the work.");
            }
            return calls(&[mcp("change")]);
        }
        let role = if schema("ack_plan").is_some() {
            "acknowledgement"
        } else if system.contains("Phase: coordinate") {
            "coordination"
        } else if system.contains("You are stakeholder Adviser") {
            "stakeholder"
        } else if system.contains("You are reviewer Verifier") {
            "panel reviewer"
        } else if schema("submit_plan").is_some() {
            "freeze"
        } else if schema("submit_verdict").is_some() {
            "review"
        } else if task.starts_with("Finalize the work instructions") {
            "synthesis"
        } else {
            "planning"
        };
        if !history.contains("fixture_observed") {
            return if role == "planning" {
                calls(&[mcp("change"), mcp("inspect")])
            } else {
                calls(&[mcp("inspect")])
            };
        }
        observed.insert(role);
        match role {
            "planning" => {
                assert!(history.contains("REFUSED: MCP tool"));
                assert!(!history.contains("fixture_changed"));
                text("Use the fixture to inspect evidence.")
            }
            "freeze" => calls(&[tool(
                "plan",
                "submit_plan",
                json!({"plan":"Check evidence.","ownership":{"worker_1":[]}}),
            )]),
            "acknowledgement" => {
                let id = &schema("ack_plan").unwrap()["function"]["parameters"]["properties"]["plan_id"]
                    ["enum"][0];
                calls(&[tool("ack", "ack_plan", json!({"plan_id":id}))])
            }
            "review" => {
                for role in [
                    "planning",
                    "panel reviewer",
                    "stakeholder",
                    "coordination",
                    "synthesis",
                ] {
                    assert!(observed.contains(role), "missing inspection by {role}");
                }
                decision("approved", "Evidence verified.")
            }
            _ => text("Fixture inspected."),
        }
    });
    let run = cli_window(
        temp.path(),
        &endpoint,
        "8192",
        &[
            "--config",
            config.to_str().unwrap(),
            "--goal",
            "Use the fixture MCP.",
        ],
    );
    succeeded(&run);
    server.join().unwrap();

    let reader = ra::session::Store::reader(temp.path()).unwrap();
    let run = reader.runs().unwrap().remove(0);
    let events = reader.audit(&run.run_id, Default::default(), 500).unwrap();
    for actor in ["lead_reviewer", "reviewer_1", "stakeholder_1", "worker_1"] {
        assert!(
            events.iter().any(|event| event.actor == actor
                && event.kind == "tool_result"
                && event.data["tool"] == "mcp_call"
                && event.data["state"] == "completed"),
            "missing MCP audit for {actor}"
        );
    }
}

#[test]
fn direct_tool_loop_compacts_and_reviews() {
    let temp = tempfile::tempdir().unwrap();
    let mut work_calls = 0;
    let mut summaries = 0;
    let (endpoint, server) = mock(move |index, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        if system.starts_with("Compress the supplied") {
            summaries += 1;
            assert!(request.get("tools").is_none());
            return text(
                "Earlier shell commands printed verification output and created result.txt.",
            );
        }
        assert!(system.contains("test goal"));
        if index == 0 {
            assert!(!request["tools"].to_string().contains("\"shell\""));
            return text("Create result.txt and print its contents.");
        }
        if system.contains("You are the worker") {
            let messages = request["messages"].as_array().unwrap();
            if work_calls == 0 {
                work_calls += 1;
                return calls(&[
                    tool(
                        "create",
                        "shell",
                        json!({"command": if cfg!(windows) {
                            format!("echo done>result.txt & {}", diagnostics_script())
                        } else {
                            format!("printf 'done\\n' > result.txt; {}", diagnostics_script())
                        },"workdir":null}),
                    ),
                    tool("read", "read_file", json!({"path":"result.txt"})),
                ]);
            }
            if work_calls < 6 {
                if work_calls == 1 {
                    assert!(
                        messages.last().unwrap()["content"]
                            .as_str()
                            .unwrap()
                            .contains("done")
                    );
                }
                work_calls += 1;
                return calls(&[tool(
                    &format!("work-{work_calls}"),
                    "shell",
                    json!({"command":diagnostics_script(),"workdir":null}),
                )]);
            }
            assert!(summaries > 0);
            assert!(messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Earlier work for the current task"))
            }));
            return text("Created result.txt and verified its contents.");
        }
        if request["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
            assert!(
                request["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("done")
            );
            return decision("approved", "Verified.");
        }
        calls(&[tool("verify", "read_file", json!({"path":"result.txt"}))])
    });
    let run = cli(temp.path(), &endpoint, &["--goal", "test goal"]);
    succeeded(&run);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("result.txt"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["done"]
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("VERDICT: APPROVED"));
    server.join().unwrap();
}

#[test]
fn resume_advances_a_committed_verdict_without_another_model_request() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path();
    let (endpoint, server) = mock(|index, _| match index {
        0 => text("Plan."),
        1 => text("Work."),
        2 => decision("approved", "Verdict checkpoint."),
        _ => panic!("unexpected request {index}"),
    });
    succeeded(&cli(
        workspace,
        &endpoint,
        &["--goal", "Test verdict resume"],
    ));
    server.join().unwrap();

    let reader = ra::session::Store::reader(workspace).unwrap();
    let run = reader.runs().unwrap().pop().unwrap();
    let checkpoint = reader
        .events(&run.run_id, 0, 100)
        .unwrap()
        .into_iter()
        .filter(|event| event.phase == "Review")
        .filter_map(|event| reader.snapshot(&run.run_id, Some(event.sequence)).unwrap())
        .map(|snapshot| serde_json::from_slice::<Value>(&snapshot).unwrap())
        .find(|snapshot| snapshot["phase_result"] == "Verdict checkpoint.")
        .expect("completed verdict should be checkpointed before the phase advances");
    ra::session::Store::open(workspace)
        .unwrap()
        .save(&serde_json::from_value(checkpoint).unwrap())
        .unwrap();

    let result = cli(
        workspace,
        "http://127.0.0.1:1/v1",
        &["--resume", "--run-id", &run.run_id],
    );
    succeeded(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains("Verdict checkpoint."));
    assert_eq!(reader.runs().unwrap()[0].phase, "Approved");
}

#[test]
fn resume_does_not_replay_an_interrupted_tool() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, _| match index {
        0 => text("Append a line to result.txt once."),
        1 => calls(&[tool(
            "write",
            "shell",
            json!({"command":if cfg!(windows) {"echo once>>result.txt"} else {"printf 'once\\n' >> result.txt"},"workdir":null}),
        )]),
        _ => (
            503,
            json!({"error":{"message":"mock provider unavailable"}}),
        ),
    });
    let failed = cli(temp.path(), &endpoint, &["--goal", "resume test"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("503"));
    server.join().unwrap();

    // Model a crash after the command ran but before the result was committed.
    let path = temp.path().join(".ratio/session.json");
    let mut data: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    data["dialogue"]["in_flight"] = json!(true);
    data["dialogue"]["groups"][0]
        .as_array_mut()
        .unwrap()
        .pop()
        .unwrap();
    ra::session::Store::open(temp.path())
        .unwrap()
        .save(&serde_json::from_value(data).unwrap())
        .unwrap();

    let (endpoint, server) = mock(|index, request| {
        if index == 0 {
            let last = request["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["tool_call_id"], "write");
            assert!(last["content"].as_str().unwrap().contains("interrupted"));
            text("Done.")
        } else {
            decision("approved", "Verified.")
        }
    });
    let run = cli(temp.path(), &endpoint, &["--resume"]);
    succeeded(&run);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("result.txt"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["once"]
    );
    server.join().unwrap();
}

#[test]
fn reported_usage_adjusts_compaction_without_blocking_the_next_request() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(move |_, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        if system.contains("You are the worker") {
            if let Some(last) = messages.last().filter(|last| last["role"] == "tool") {
                assert_eq!(last["tool_call_id"], "inspect");
                return text("Inspected the workspace.");
            }
            let (status, mut response) = calls(&[tool("inspect", "git_status", json!({}))]);
            // The provider accepts the request and uses more tokens than Ratio projected.
            response["usage"] = json!({"prompt_tokens": 2800, "completion_tokens": 5});
            return (status, response);
        }
        if messages[1]["content"]
            .as_str()
            .unwrap()
            .starts_with("Inspect the resulting")
        {
            decision("approved", "Verified.")
        } else {
            text("Inspect the workspace status.")
        }
    });
    let run = cli(
        temp.path(),
        &endpoint,
        &["--goal", "test usage calibration"],
    );
    succeeded(&run);
    server.join().unwrap();
}

#[test]
fn provider_context_limit_retries_with_compacted_history() {
    exercise_provider_context_limit(false);
}

#[test]
fn streamed_context_rejection_retries_with_compacted_history() {
    exercise_provider_context_limit(true);
}

fn exercise_provider_context_limit(streamed: bool) {
    let temp = tempfile::tempdir().unwrap();
    let mut worker_requests = 0;
    let (endpoint, server) = mock(move |_, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        if system.starts_with("Compress the supplied") {
            assert!(messages[1]["content"].as_str().unwrap().contains("old"));
            return text("The old command emitted workspace diagnostics.");
        }
        if system.contains("You are the worker") {
            worker_requests += 1;
            let command = |id| {
                tool(
                    id,
                    "shell",
                    json!({"command":diagnostics_script(),"workdir":null}),
                )
            };
            return match worker_requests {
                1 => calls(&[command("old")]),
                2 => calls(&[command("recent")]),
                3 if streamed => stream_response(
                    &[
                        json!({"error":{"code":"context_length_exceeded","message":"Context window exceeded"}}),
                    ],
                    false,
                    false,
                ),
                3 => (
                    400,
                    json!({"error":{"code":"context_length_exceeded","message":"Context window exceeded"}}),
                ),
                4 => {
                    assert!(
                        messages
                            .iter()
                            .any(|message| message["tool_call_id"] == "recent")
                    );
                    assert!(
                        !messages
                            .iter()
                            .any(|message| message["tool_call_id"] == "old")
                    );
                    assert!(messages.iter().any(|message| {
                        message["content"].as_str().is_some_and(|content| {
                            content.contains("The old command emitted workspace diagnostics.")
                        })
                    }));
                    text("Captured and reviewed diagnostics.")
                }
                _ => panic!("unexpected worker request"),
            };
        }
        if messages[1]["content"]
            .as_str()
            .unwrap()
            .starts_with("Inspect the resulting")
        {
            decision("approved", "Verified.")
        } else {
            text("Capture and review workspace diagnostics.")
        }
    });
    let run = cli_window(
        temp.path(),
        &endpoint,
        "4096",
        &["--goal", "test provider rejection"],
    );
    succeeded(&run);
    server.join().unwrap();
}

#[test]
fn stakeholder_input_reaches_synthesis_and_revision() {
    let temp = tempfile::Builder::new()
        .prefix(&"long-workspace-".repeat(7))
        .tempdir()
        .unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        r#"
[[stakeholders]]
name = "Build reviewer"
persona = "Inspect build requirements."
phases = ["planning", "review"]
"#,
    )
    .unwrap();
    let mut cycles = 0;
    let (endpoint, server) = mock(move |_, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        let task = messages[1]["content"].as_str().unwrap();
        if system.contains("You are stakeholder") {
            assert!(task.starts_with("Evaluate this draft"));
            return text("Check build reproducibility.");
        }
        if task.starts_with("Finalize the work instructions") {
            assert!(task.contains("Check build reproducibility."));
            return text("Implement the plan and run the build.");
        }
        if system.contains("You are the worker") {
            if cycles == 1 {
                assert!(task.contains("Include the build log."));
            }
            return text("Implemented the plan.");
        }
        if task.starts_with("Produce the final review") {
            assert!(task.contains("Check build reproducibility."));
            cycles += 1;
            return if cycles == 1 {
                decision("needs_revision", "Include the build log.")
            } else {
                decision("approved", "Verified.")
            };
        }
        if task.starts_with("Inspect the resulting workspace") {
            return text("Draft assessment.");
        }
        text("Draft implementation plan.")
    });
    let run = cli(
        temp.path(),
        &endpoint,
        &[
            "--config",
            config.to_str().unwrap(),
            "--goal",
            "stakeholder test",
        ],
    );
    succeeded(&run);
    server.join().unwrap();
}

#[test]
fn parallel_panel_delivers_ipc_and_reactivates_finished_agents() {
    let temp = tempfile::Builder::new()
        .prefix(&"long-workspace-".repeat(7))
        .tempdir()
        .unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        r#"
[[reviewers]]
name = "Build reviewer"
persona = "Verify the diagnostic."
phases = ["review"]

[[stakeholders]]
name = "Release stakeholder"
persona = "Assess readiness."
phases = ["review"]
"#,
    )
    .unwrap();
    let mut reviewer_started = false;
    let mut stakeholder_started = false;
    let mut got_reply = false;
    let (endpoint, server) = mock_deferred(move |_, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        let task = messages.last().unwrap()["content"]
            .as_str()
            .unwrap_or_default();
        let content = request["messages"].to_string();
        let (response, defer) = if system.contains("Your ID: reviewer_1") {
            assert!(request["tools"].to_string().contains("\"send_message\""));
            assert!(system.contains("stakeholder_1: Release stakeholder"));
            if !reviewer_started {
                reviewer_started = true;
                (
                    calls(&[
                        tool(
                            "invalid",
                            "send_message",
                            json!({"to":"missing","message":"probe: invalid recipient"}),
                        ),
                        tool(
                            "request",
                            "send_message",
                            json!({"to":"stakeholder_1","message":"Verify the release diagnostic (release build)."}),
                        ),
                    ]),
                    !stakeholder_started,
                )
            } else if content.contains("From stakeholder_1: result: verified") {
                got_reply = true;
                (text("Reviewer corroborated peer evidence."), false)
            } else {
                assert!(content.contains("REFUSED: unknown recipient ID"));
                assert!(content.contains("Queued for delivery"));
                (text("Initial reviewer findings."), false)
            }
        } else if system.contains("Your ID: stakeholder_1") {
            if !stakeholder_started {
                stakeholder_started = true;
                (text("Initial stakeholder findings."), !reviewer_started)
            } else if !content.contains("reply") {
                assert!(
                    content.contains(
                        "From reviewer_1: Verify the release diagnostic (release build)."
                    )
                );
                assert!(content.contains("Previous assessment before new peer evidence"));
                (
                    calls(&[tool(
                        "reply",
                        "send_message",
                        json!({"to":"reviewer_1","message":"result: verified"}),
                    )]),
                    false,
                )
            } else {
                (text("Stakeholder verified the release diagnostic."), false)
            }
        } else if task.starts_with("Produce the final review") {
            assert!(got_reply);
            assert!(task.contains("Reviewer corroborated peer evidence."));
            assert!(task.contains("Stakeholder verified the release diagnostic."));
            (decision("approved", "Verified."), false)
        } else {
            (text("Inspect the release diagnostic."), false)
        };
        (response.0, response.1, defer)
    });
    let output = cli(
        temp.path(),
        &endpoint,
        &[
            "--config",
            config.to_str().unwrap(),
            "--goal",
            "review test",
        ],
    );
    succeeded(&output);
    server.join().unwrap();
}

#[test]
fn three_workers_start_only_after_a_persisted_unanimous_ack() {
    exercise_worker_gate(true, false, false, false);
}

#[test]
fn wrong_plan_ack_and_plain_text_approval_keep_every_worker_read_only() {
    exercise_worker_gate(false, false, false, false);
}

#[test]
fn plan_objection_recoordinates_and_issues_a_fresh_digest() {
    exercise_worker_gate(true, true, false, false);
}

#[test]
fn graph_runs_independent_workers_before_their_dependent() {
    exercise_worker_gate(true, false, true, false);
}

#[test]
fn staged_graph_recovers_truncated_and_incomplete_plans_before_execution() {
    exercise_worker_gate(true, false, true, true);
}

#[test]
fn stakeholder_answers_reopen_all_workers_and_shared_memories_survive_resume() {
    use ra::{
        config::Worker,
        session::{Phase, Session, Store, WorkerPlan},
    };

    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        r#"
[orchestration]
max_parallel_agents = 1
[[stakeholders]]
name = "Hydraulics adviser"
persona = "Specify the vertical datum."
phases = ["planning"]
"#,
    )
    .unwrap();
    {
        let store = Store::open(temp.path()).unwrap();
        let mut session = Session::new("Implement the agreed simulation.".into());
        session.phase = Phase::Work;
        let workers = (1..=3)
            .map(|id| Worker {
                name: format!("Engineer {id}"),
                persona: format!("Own module {id}."),
                model: None,
            })
            .collect::<Vec<_>>();
        let mut plan = WorkerPlan::new("Use the agreed vertical datum.".into(), 1, &workers);
        plan.acknowledgements = vec![0, 1, 2];
        session.worker_plan = Some(plan);
        session.workers = Some(workers);
        store.save(&session).unwrap();
    }

    let body = "Use the local tide gauge (the DEM fixture shares this datum).";
    let reply = "From stakeholder_1 (reply to worker_1): Use the local tide gauge (the DEM fixture shares this datum).";
    let mut early_reports = 0;
    let (endpoint, server) = mock(move |_, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        let history = request["messages"].to_string();
        if system.starts_with("You are parallel worker worker_1,") {
            assert!(system.contains("stakeholder_1: Hydraulics adviser"));
            if history.contains(reply) {
                assert_eq!(early_reports, 2);
                (
                    503,
                    json!({"error":{"message":"interrupt after broadcast"}}),
                )
            } else if history.contains("Queued for delivery") {
                text("Initial worker report.")
            } else {
                calls(&[
                    tool(
                        "note",
                        "remember",
                        json!({"topic":"Vertical datum","note":"Gauge metadata needs review.","source":"field notes"}),
                    ),
                    tool(
                        "ask",
                        "send_message",
                        json!({"to":"stakeholder_1","message":"datum: which vertical reference ? local gauge | mean sea level"}),
                    ),
                ])
            }
        } else if system.starts_with("You are parallel worker") {
            assert!(!history.contains(reply));
            early_reports += 1;
            text("Early worker report.")
        } else {
            assert!(system.starts_with("You are stakeholder Hydraulics adviser"));
            assert!(history.contains("From worker_1: datum:"));
            let tools = request["tools"].to_string();
            assert!(!tools.contains("\"create_file\""));
            assert!(!tools.contains("\"shell\""));
            assert!(!tools.contains("\"edit_file\""));
            if history.contains("Tool error: tool create_file is unavailable") {
                calls(&[
                    tool(
                        "adviser-note",
                        "remember",
                        json!({"topic":"Vertical datum","note":"Use local tide gauge; DEM fixture shares datum."}),
                    ),
                    tool(
                        "reply",
                        "send_message",
                        json!({"to":"worker_1","message":body,"reason":"clarification"}),
                    ),
                ])
            } else {
                calls(&[tool(
                    "bad-write",
                    "create_file",
                    json!({"path":"adviser.txt","content":"test"}),
                )])
            }
        }
    });
    let args = ["--config", config.to_str().unwrap(), "--resume"];
    let interrupted = cli_window(temp.path(), &endpoint, "8192", &args);
    let error = String::from_utf8_lossy(&interrupted.stderr);
    assert!(error.contains("interrupt after broadcast"), "{error}");
    server.join().unwrap();
    assert!(!temp.path().join("adviser.txt").exists());

    let mut verified = [false; 3];
    let (endpoint, server) = mock(move |_, request| {
        let system = request["messages"][0]["content"].as_str().unwrap();
        let history = request["messages"].to_string();
        if let Some(worker) = system.strip_prefix("You are parallel worker worker_") {
            let index = worker.split(',').next().unwrap().parse::<usize>().unwrap() - 1;
            assert!(history.contains(reply));
            if history.contains("Shared memory (untrusted agent notes)") {
                assert!(history.contains("from worker_1 / Vertical datum"));
                assert!(history.contains("from stakeholder_1 / Vertical datum"));
                assert!(history.contains("DEM fixture shares datum"));
                verified[index] = true;
                text("Worker verified the clarification and both shared memories.")
            } else {
                calls(&[tool(
                    "recall",
                    "recall",
                    json!({"query":"DATUM vertical","limit":3}),
                )])
            }
        } else if system.starts_with("You are stakeholder Hydraulics adviser") {
            assert!(history.contains("Queued stakeholder reply for delivery to all workers"));
            text("Answer delivered.")
        } else {
            assert_eq!(verified, [true; 3]);
            decision("approved", "Verified.")
        }
    });
    let output = cli_window(temp.path(), &endpoint, "8192", &args);
    succeeded(&output);
    server.join().unwrap();
}

fn exercise_worker_gate(unanimous: bool, objects: bool, graph: bool, staged: bool) {
    let temp = tempfile::tempdir().unwrap();
    if staged {
        succeeded(
            &Command::new("git")
                .current_dir(temp.path())
                .args(["init", "--quiet"])
                .output()
                .unwrap(),
        );
    }
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[orchestration]
max_parallel_agents = 3
require_task_graph = {graph}
stage_workspace = {staged}
verification_commands = ["git diff --check"]
[[workers]]
name = "One"
persona = "Own worker_1.txt."
[[workers]]
name = "Two"
persona = "Own worker_2.txt."
[[workers]]
name = "Three"
persona = "Own worker_3.txt."
"#
        ),
    )
    .unwrap();
    let workspace = temp.path().to_path_buf();
    let mut coordinated = [false; 3];
    let mut started = [false; 3];
    let mut finished = [false; 3];
    let mut plan_id: Option<String> = None;
    let mut first_id: Option<String> = None;
    let mut replanning = false;
    let mut freeze_attempts = 0;
    let plan =
        "worker_1 owns worker_1.txt. worker_2 owns worker_2.txt. worker_3 owns worker_3.txt.";
    let frozen_plan = if staged {
        concat!(
            "worker_1: Build artifact 1\nAcceptance: file exists\n\n",
            "worker_2: Build artifact 2\nAcceptance: file exists\n\n",
            "worker_3: Build artifact 3\nAcceptance: file exists"
        )
    } else {
        plan
    };
    let mut recovered = 0;
    let (endpoint, server) = mock_deferred(move |_, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        let last = messages.last().unwrap();
        let task = last["content"].as_str().unwrap_or_default();
        let all_messages = request["messages"].to_string();
        let tools = request["tools"].to_string();
        let (response, defer) = if let Some(worker) =
            system.strip_prefix("You are parallel worker worker_")
        {
            let worker = worker.split(',').next().unwrap().parse::<usize>().unwrap();
            let own_file = format!("worker_{worker}.txt");
            let mutators = ["\"shell\"", "\"create_file\"", "\"edit_file\""];
            if system.contains("Phase: coordinate") {
                if objects && first_id.is_some() && !replanning {
                    replanning = true;
                    coordinated = [false; 3];
                    plan_id = None;
                    assert!(all_messages.contains("Resolve interface conflict"));
                }
                assert!(mutators.iter().all(|tool| !tools.contains(tool)));
                assert!(!tools.contains("\"ack_plan\""));
                if !coordinated[worker - 1] {
                    coordinated[worker - 1] = true;
                    let next = worker % 3 + 1;
                    let response = calls(&[tool(
                        "coordinate",
                        "send_message",
                        json!({
                            "to":format!("worker_{next}"),
                            "message":format!("owner: worker_{worker}; path: {own_file}")
                        }),
                    )]);
                    (response, !coordinated.iter().all(|&received| received))
                } else {
                    assert!(all_messages.contains("Untrusted peer IPC context"));
                    (text(&format!("Assignment agreed for {own_file}.")), false)
                }
            } else if system.contains("Phase: acknowledge") {
                assert!(mutators.iter().all(|tool| !tools.contains(tool)));
                assert!(tools.contains("\"ack_plan\""));
                assert!(!workspace.join("premature.txt").exists());
                let user_task = messages[1]["content"].as_str().unwrap();
                let id = user_task
                    .lines()
                    .next()
                    .unwrap()
                    .strip_prefix("Frozen plan ID: ")
                    .unwrap();
                assert_eq!(id.len(), 64);
                assert!(user_task.ends_with(frozen_plan));
                assert_eq!(plan_id.get_or_insert_with(|| id.to_owned()), id);
                if objects && worker == 1 && !replanning {
                    first_id = Some(id.to_owned());
                    let response = calls(&[tool(
                        "object",
                        "object_to_plan",
                        json!({"plan_id":id,"reason":"Resolve interface conflict"}),
                    )]);
                    return (response.0, response.1, false);
                }
                if replanning {
                    assert_ne!(first_id.as_deref(), Some(id));
                }
                match last["tool_call_id"].as_str() {
                    Some("early-edit") => {
                        assert!(task.contains("unavailable"));
                        assert!(all_messages.contains("Tool error: tool shell is unavailable"));
                        assert!(
                            all_messages.contains("Tool error: tool create_file is unavailable")
                        );
                        (
                            calls(&[tool("ack", "ack_plan", json!({"plan_id":id}))]),
                            false,
                        )
                    }
                    Some("wrong") => {
                        assert!(task.contains("REFUSED: ack_plan requires the exact"));
                        let (status, mut response) = text("I acknowledge the plan in prose.");
                        response["test_terminal"] = json!(true);
                        ((status, response), false)
                    }
                    _ if unanimous && worker == 1 && !objects => (
                        calls(&[
                            tool(
                                "early-shell",
                                "shell",
                                json!({"command":"touch premature.txt","workdir":null}),
                            ),
                            tool(
                                "early-create",
                                "create_file",
                                json!({"path":"premature.txt","content":"unexpected"}),
                            ),
                            tool(
                                "early-edit",
                                "edit_file",
                                json!({"path":"ratio.toml","old_text":"One","new_text":"changed"}),
                            ),
                        ]),
                        false,
                    ),
                    _ if !unanimous && worker == 3 => (
                        calls(&[tool("wrong", "ack_plan", json!({"plan_id":"stale"}))]),
                        false,
                    ),
                    _ => (
                        calls(&[tool(
                            "ack",
                            "ack_plan",
                            json!({"plan_id":id,"comment":"reviewed"}),
                        )]),
                        false,
                    ),
                }
            } else {
                assert!(unanimous);
                assert!(system.contains("Phase: execute"));
                assert!(mutators.iter().all(|tool| tools.contains(tool)));
                assert!(
                    messages[1]["content"]
                        .as_str()
                        .unwrap()
                        .ends_with(frozen_plan)
                );
                let checkpoint: Value = serde_json::from_slice(
                    &std::fs::read(workspace.join(".ratio/session.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    checkpoint["worker_plan"]["acknowledgements"],
                    json!([0, 1, 2])
                );
                assert_eq!(checkpoint["worker_plan"]["id"], plan_id.as_deref().unwrap());
                if !started[worker - 1] {
                    if graph && worker == 3 {
                        assert!(finished[0] && finished[1]);
                        let worker_workspace = if staged {
                            assert!(!workspace.join("worker_1.txt").exists());
                            assert!(!workspace.join("worker_2.txt").exists());
                            PathBuf::from(
                                system
                                    .lines()
                                    .find_map(|line| line.strip_prefix("Working directory: "))
                                    .unwrap(),
                            )
                        } else {
                            workspace.clone()
                        };
                        assert!(worker_workspace.join("worker_1.txt").exists());
                        assert!(worker_workspace.join("worker_2.txt").exists());
                    }
                    started[worker - 1] = true;
                    let mut work = Vec::new();
                    if objects && worker == 1 {
                        work.push(tool(
                            "out-of-scope",
                            "create_file",
                            json!({"path":"unowned.txt","content":"unexpected"}),
                        ));
                    }
                    work.push(tool(
                        "write",
                        "create_file",
                        json!({"path":own_file,"content":worker.to_string()}),
                    ));
                    let defer = if graph {
                        !started[0] || !started[1]
                    } else {
                        !started.iter().all(|&active| active)
                    };
                    (calls(&work), defer)
                } else {
                    finished[worker - 1] = true;
                    assert!(task.contains("Created"));
                    if objects && worker == 1 {
                        assert!(all_messages.contains("outside this worker's frozen ownership"));
                    }
                    (
                        text(&format!("Created my file for worker_{worker}.")),
                        false,
                    )
                }
            }
        } else if task.starts_with("Write the final execution plan") {
            freeze_attempts += 1;
            assert!(coordinated.iter().all(|&active| active));
            assert!(task.contains("Assignment agreed for worker_3.txt."));
            if staged && graph && freeze_attempts == 1 {
                let (status, stream) = stream_response(
                    &[stream_event(json!({"content":"partial"}), Some("length"))],
                    true,
                    false,
                );
                return (status, stream, false);
            }
            let mut submission = json!({"plan":plan,"ownership":{
                "worker_1":[{"path":"./worker_1.txt","directory":false}],
                "worker_2":[{"path":"worker_2.txt","directory":false}],
                "worker_3":[{"path":"worker_3.txt","directory":false}]
            }});
            if graph {
                assert!(tools.contains("depends_on"));
                submission["tasks"] = json!((1..=3).map(|worker| json!({
                    "id":format!("task_{worker}"), "worker":format!("worker_{worker}"),
                    "description":format!("Build artifact {worker}"),
                    "depends_on": if worker == 3 { vec!["task_1","task_2"] } else { Vec::new() },
                    "acceptance":["file exists"]
                })).collect::<Vec<_>>());
                if staged {
                    submission["tasks"].as_array_mut().unwrap().pop();
                }
                if staged && freeze_attempts == 3 {
                    assert!(all_messages.contains("worker_3"));
                    assert!(all_messages.contains("missing"));
                }
            }
            (calls(&[tool("plan", "submit_plan", submission)]), false)
        } else if system.starts_with("Recover a multi-worker plan") {
            assert!(staged);
            assert_eq!(freeze_attempts, 4);
            recovered += 1;
            let id = format!("worker_{recovered}");
            assert!(system.contains(&format!("Submit only {id}")));
            let assignment = json!({
                "task": {
                    "id":id, "worker":id, "description":format!("Build artifact {recovered}"),
                    "depends_on": if recovered == 3 { vec!["worker_1","worker_2"] } else { Vec::new() },
                    "acceptance":["file exists"]
                },
                "paths":[{"path":format!("{id}.txt"),"directory":false}]
            });
            (
                calls(&[tool("recovered", "submit_worker_assignment", assignment)]),
                false,
            )
        } else if task.starts_with("Inspect the resulting workspace") {
            assert!(unanimous);
            assert!(started.iter().all(|&active| active));
            if staged {
                assert_eq!(recovered, 3);
            }
            (decision("approved", "Verified."), false)
        } else {
            (text("Coordinate assignments for all three workers."), false)
        };
        (response.0, response.1, defer)
    });
    let output = cli_window(
        temp.path(),
        &endpoint,
        "8192",
        &[
            "--config",
            config.to_str().unwrap(),
            "--goal",
            "worker gate smoke test",
        ],
    );
    assert!(!temp.path().join("premature.txt").exists());
    assert!(!temp.path().join("unowned.txt").exists());
    if unanimous {
        succeeded(&output);
        for worker in 1..=3 {
            assert_eq!(
                std::fs::read_to_string(temp.path().join(format!("worker_{worker}.txt"))).unwrap(),
                worker.to_string()
            );
        }
    } else {
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("missing ACK from worker_3"));
        for worker in 1..=3 {
            assert!(!temp.path().join(format!("worker_{worker}.txt")).exists());
        }
    }
    server.join().unwrap();
    if unanimous && !objects && !graph {
        resume_interrupted_parallel_worker(temp.path(), &config);
    }
}

fn resume_interrupted_parallel_worker(workspace: &Path, config: &Path) {
    let path = workspace.join(".ratio/session.json");
    let mut checkpoint: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    checkpoint["phase"] = json!("Work");
    let progress = &mut checkpoint["worker_progress"][0];
    progress["assessment"] = Value::Null;
    progress["dialogue"]["in_flight"] = json!(true);
    let group = progress["dialogue"]["groups"][0].as_array_mut().unwrap();
    group.pop().unwrap();
    group[0]["tool_calls"][0] = tool(
        "write",
        "shell",
        json!({"command":"printf replay >> worker_1.txt","workdir":null}),
    );
    ra::session::Store::open(workspace)
        .unwrap()
        .save(&serde_json::from_value(checkpoint).unwrap())
        .unwrap();

    let (endpoint, server) = mock(|index, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        if index == 0 {
            assert!(system.starts_with("You are parallel worker worker_1"));
            assert!(system.contains("Phase: execute"));
            assert!(
                messages.last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("Tool was interrupted")
            );
            text("Recovered and inspected my file.")
        } else {
            assert!(system.starts_with("You are the reviewer"));
            decision("approved", "Verified.")
        }
    });
    let output = cli_window(
        workspace,
        &endpoint,
        "8192",
        &["--config", config.to_str().unwrap(), "--resume"],
    );
    succeeded(&output);
    assert_eq!(
        std::fs::read_to_string(workspace.join("worker_1.txt")).unwrap(),
        "1"
    );
    server.join().unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
static TUI_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn tui_previews_pauses_and_restarts_a_generation() {
    use std::io::Read;
    use std::sync::mpsc;

    let _tui = TUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let (cancelled, cancellation) = mpsc::channel();
    let server = thread::spawn(move || {
        for index in 0..5 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "no request {index}");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            let (request, _) = read_mock_request(&mut socket);
            assert_eq!(request["stream"], true);
            if index == 1 {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
                let delta = stream_event(json!({"content":"LIVE-FIRST-PACKET"}), None);
                let first_chunk = format!("data: {delta}\n\n");
                write!(socket, "{:X}\r\n{first_chunk}\r\n", first_chunk.len()).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(8);
                loop {
                    use std::io::ErrorKind::{ConnectionReset, TimedOut, WouldBlock};
                    match socket.read(&mut [0; 1]) {
                        Ok(0) => break,
                        Err(error) if error.kind() == ConnectionReset => break,
                        Err(error) if matches!(error.kind(), TimedOut | WouldBlock) => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "client did not close the cancelled stream"
                            );
                            // Keep the connection readable so the HTTP client can
                            // observe that the body consumer has disappeared.
                            if socket.write_all(b"4\r\n:x\n\n\r\n").is_err() {
                                break;
                            }
                        }
                        unexpected => panic!("unexpected socket result: {unexpected:?}"),
                    }
                }
                cancelled.send(()).unwrap();
                continue;
            }
            assert!(
                !request["messages"]
                    .to_string()
                    .contains("LIVE-FIRST-PACKET")
            );
            let (_, response) = match index {
                0 => text(
                    r#"{"goal":"Inspect the fixture","stakeholders":[],"message":"plan-ready-marker"}"#,
                ),
                2 => {
                    assert!(request["messages"].to_string().contains("use-a-new-check"));
                    text("Inspect the workspace.")
                }
                3 => text("Workspace inspected."),
                4 => decision("approved", "Verified after cancellation."),
                _ => unreachable!(),
            };
            let body = serde_json::to_vec(&response).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(&body).unwrap();
        }
    });
    let (mut terminal, process) = launch_tui(temp.path(), &endpoint, &["--max-run-secs", "3"]);
    let mut display = Vec::new();
    wait_for_screen(&mut terminal, &mut display, "Stakeholders");
    terminal.write_all(b"Inspect fixture\r").unwrap();
    wait_for_screen(&mut terminal, &mut display, "plan-ready-marker");
    terminal.write_all(b"\x12").unwrap(); // Ctrl+R
    wait_for_screen(&mut terminal, &mut display, "LIVE-FIRST-PACKET");
    terminal.write_all(b"\x03").unwrap();
    wait_for_screen(&mut terminal, &mut display, "paused.");
    cancellation
        .recv_timeout(Duration::from_secs(9))
        .unwrap_or_else(|error| {
            panic!(
                "generation cancellation failed: {error}\n{}",
                String::from_utf8_lossy(&display[display.len().saturating_sub(2500)..])
            )
        });
    thread::sleep(Duration::from_millis(3250));
    terminal.write_all(b"use-a-new-check\r").unwrap();
    terminal.write_all(b"\x10").unwrap(); // Ctrl+P
    wait_for_screen(&mut terminal, &mut display, "Approved");
    terminal.write_all(b"q").unwrap();
    wait_for_screen(&mut terminal, &mut display, "\x1b[?1049l");
    let run = process.finish(terminal);
    succeeded(&run);
    assert!(String::from_utf8_lossy(&run.stdout).contains("Verified after cancellation."));
    server.join().unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn tui_refines_and_approves_a_goal_then_displays_file_tools() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let _tui = TUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let approved = Arc::new(AtomicBool::new(false));
    let allowed = approved.clone();
    let mut stakeholder_stages = 0;
    let (endpoint, server) = mock(move |index, request| {
        let messages = request["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        let task = messages[1]["content"].as_str().unwrap();
        if system.starts_with("Help the user plan") {
            assert!(request.get("tools").is_none());
            if index == 0 {
                assert!(task.contains("Write a status file"));
                return text(&json!({
                    "goal":"Create a status file.",
                    "stakeholders":[{"name":"General reviewer","persona":"Check the file.","phases":["review"]}],
                    "message":"First proposal is ready."
                }).to_string());
            }
            assert_eq!(index, 1);
            assert!(task.contains("General reviewer"));
            assert!(task.contains("Focus the reviewer on the exact file contents"));
            return text(&json!({
                "goal":"Create status.txt with the exact text ready.",
                "stakeholders":[{"name":"File reviewer","description":"Verify the exact UTF-8 file contents.","phases":["planning", "review"]}],
                "message":"Refined proposal is ready."
            }).to_string());
        }
        assert!(allowed.load(Ordering::SeqCst));
        assert!(system.contains("Create status.txt with the exact text ready."));
        if system.contains("You are stakeholder File reviewer") {
            assert!(system.contains("Verify the exact UTF-8 file contents."));
            if system.contains("Answer worker questions") {
                let history = request["messages"].to_string();
                assert!(history.contains("From worker_1: status: trailing newline required"));
                if history.contains("Queued stakeholder reply") {
                    return text("Format clarified.");
                }
                return calls(&[tool(
                    "reply",
                    "send_message",
                    json!({"to":"worker_1","message":"status: exact ready; trailing newline !required"}),
                )]);
            }
            stakeholder_stages += 1;
            return text("Inspect status.txt before the final verdict.");
        }
        if system.contains("You are the worker") {
            assert!(request["tools"].to_string().contains("\"edit_file\""));
            let last = messages.last().unwrap();
            if last["tool_call_id"] == "edit" {
                assert!(last["content"].as_str().unwrap().contains("ready"));
                return text("Created, edited, and verified the status file.");
            }
            if matches!(
                last["tool_call_id"].as_str(),
                Some("read" | "read_after_stale")
            ) {
                let line = last["content"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .find(|line| line.starts_with("1 "))
                    .unwrap();
                let tag = line.split(' ').nth(1).unwrap().split('|').next().unwrap();
                assert_eq!(tag.len(), 16);
                assert!(line.ends_with("|pending"));
                if last["tool_call_id"] == "read" {
                    return calls(&[
                        tool(
                            "stale",
                            "edit_file",
                            json!({"path":"status.txt","edits":{(tag):"wrong","0000000000000000":"stale"}}),
                        ),
                        tool(
                            "read_after_stale",
                            "read_file",
                            json!({"path":"status.txt"}),
                        ),
                    ]);
                }
                assert!(messages.iter().any(|message| {
                    message["tool_call_id"] == "stale"
                        && message["content"]
                            .as_str()
                            .unwrap()
                            .contains("stale or unknown line tag")
                }));
                return calls(&[tool(
                    "edit",
                    "edit_file",
                    json!({"path":"status.txt","edits":{(tag):"ready"}}),
                )]);
            }
            return calls(&[
                tool(
                    "ask-format",
                    "send_message",
                    json!({"to":"stakeholder_1","message":"status: trailing newline required ? yes | no"}),
                ),
                tool(
                    "create",
                    "create_file",
                    json!({"path":"status.txt","content":"pending"}),
                ),
                tool(
                    "duplicate",
                    "create_file",
                    json!({"path":"status.txt","content":"wrong"}),
                ),
                tool("read", "read_file", json!({"path":"status.txt"})),
            ]);
        }
        if task.starts_with("Produce the final review") {
            assert_eq!(stakeholder_stages, 2);
            return decision("approved", "File contents verified.");
        }
        text("Inspect and verify the status file.")
    });
    let (mut terminal, process) = launch_tui(temp.path(), &endpoint, &[]);
    let mut display = Vec::new();
    wait_for_screen(&mut terminal, &mut display, "Stakeholders");
    terminal.write_all(b"Write a status file\r").unwrap();
    wait_for_screen(&mut terminal, &mut display, "First");
    terminal
        .write_all(b"Focus the reviewer on the exact file contents\r")
        .unwrap();
    wait_for_screen(&mut terminal, &mut display, "Refined");
    assert!(!temp.path().join(".ratio/session.json").exists());
    approved.store(true, Ordering::SeqCst);
    terminal.write_all(b"\x1b[<0;110;1M\x1b[<0;110;1m").unwrap();
    wait_for_screen(&mut terminal, &mut display, "Approved");
    terminal.write_all(b"\t").unwrap();
    wait_for_screen(&mut terminal, &mut display, "Communication / Sequence");
    wait_for_screen(&mut terminal, &mut display, "Final verdict");
    terminal.write_all(b"\x06").unwrap(); // Ctrl+F
    wait_for_screen(&mut terminal, &mut display, "Stakeholder broadcast");
    wait_for_screen(&mut terminal, &mut display, "!required");
    terminal.write_all(b"\x14").unwrap(); // Ctrl+T
    wait_for_screen(&mut terminal, &mut display, "Outbound routes");
    terminal.write_all(b"\x1b[<0;3;2M\x1b[<0;3;2m").unwrap();
    terminal.write_all(b"\x1b[5~").unwrap(); // Page Up
    wait_for_screen(&mut terminal, &mut display, "tagged");
    terminal.write_all(b"q").unwrap();
    wait_for_screen(&mut terminal, &mut display, "\x1b[?1049l");
    let run = process.finish(terminal);
    succeeded(&run);
    assert!(String::from_utf8_lossy(&run.stdout).contains("VERDICT: APPROVED"));
    let display = String::from_utf8_lossy(&display);
    for text in ["create_file", "edit_file", "read_file", "tagged", "error:"] {
        assert!(display.contains(text), "missing terminal output: {text}");
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("status.txt")).unwrap(),
        "ready"
    );
    let checkpoint: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join(".ratio/session.json")).unwrap())
            .unwrap();
    assert_eq!(checkpoint["stakeholders"][0]["name"], "File reviewer");
    server.join().unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn launch_tui(cwd: &Path, endpoint: &str, args: &[&str]) -> (std::fs::File, TerminalChild) {
    use std::{
        os::{fd::FromRawFd, unix::process::CommandExt},
        process::Stdio,
    };
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 45,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (master, slave) = unsafe {
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size
            ),
            0
        );
        assert_eq!(libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC), 0);
        assert_eq!(libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC), 0);
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    };
    let mut command = Command::new(ratio_binary());
    command
        .args([
            "-C",
            cwd.to_str().unwrap(),
            "--api-key",
            "test-key",
            "--model",
            "test-model",
            "--context-window",
            "8192",
            "--output-tokens",
            "512",
            "--fixed-team",
            "--endpoint",
            endpoint,
        ])
        .args(args)
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stderr(slave)
        .stdout(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (master, TerminalChild(Some(command.spawn().unwrap())))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct TerminalChild(Option<std::process::Child>);

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl TerminalChild {
    fn finish(mut self, terminal: std::fs::File) -> Output {
        // macOS can block in child reaping while the PTY master stays open.
        // The child ignores the test's hangup so stdout can finish separately.
        drop(terminal);
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for TerminalChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            // During unwinding the PTY master may still be open. Reap the
            // child after the test stack has closed it.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_screen(terminal: &mut std::fs::File, display: &mut Vec<u8>, wanted: &str) {
    use std::{io::Read, os::fd::AsRawFd};
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !String::from_utf8_lossy(display).contains(wanted) {
        assert!(
            std::time::Instant::now() < deadline,
            "missing screen text {wanted:?}: {}",
            String::from_utf8_lossy(&display[display.len().saturating_sub(2500)..])
        );
        let mut poll = libc::pollfd {
            fd: terminal.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 100) } > 0 {
            let mut buffer = [0; 8192];
            let count = terminal.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "terminal closed before {wanted:?}");
            display.extend_from_slice(&buffer[..count]);
        }
    }
}
