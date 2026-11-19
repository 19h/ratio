#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener},
    path::Path,
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
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "POST /v1/chat/completions HTTP/1.1\r\n");
            let mut length = None;
            let mut authenticated = false;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty(), "request ended inside the header");
                let (name, value) = line.split_once(':').unwrap();
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = Some(value.trim().parse::<usize>().unwrap()),
                    "authorization" => authenticated = value.trim() == "Bearer test-key",
                    _ => {}
                }
            }
            assert!(authenticated);
            let mut body = vec![0; length.unwrap()];
            std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["model"], "test-model");
            assert_eq!(request["stream"], false);
            assert!(request["max_completion_tokens"].is_number());
            assert_history(&request);
            let (status, response, defer) = reply(index, request);
            let done = status == 503 || response["test_terminal"] == true;
            deferred.push((socket, status, serde_json::to_vec(&response).unwrap()));
            if !defer {
                for (mut socket, status, body) in deferred.drain(..) {
                    write!(socket, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    socket.write_all(&body).unwrap();
                }
            }
            if done {
                return;
            }
        }
    });
    (endpoint, thread)
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

fn cli_command(cwd: &Path, endpoint: &str, window: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ra"));
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
fn invalid_mixed_verdict_batch_fails_closed_after_one_repair() {
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
        3 => {
            let (status, mut response) = calls(&[tool(
                "verdict",
                "submit_verdict",
                json!({"verdict":"needs_revision","assessment":"Missing instructions"}),
            )]);
            response["test_terminal"] = json!(true);
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
            tool("slow", "shell", json!({"command":"sleep 20"}))
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
    store.save(&session).unwrap();
    drop(store);
    let args = ["--config", config.to_str().unwrap(), "--resume"];
    succeeded(&cli(root, "http://127.0.0.1:1/v1", &args));
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
                        json!({"command":"printf 'done\\n' > result.txt; yes abcdefghijklmnopqrstuvwxyz | head -n 100"}),
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
                    json!({"command":"yes abcdefghijklmnopqrstuvwxyz | head -n 100"}),
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
        std::fs::read_to_string(temp.path().join("result.txt")).unwrap(),
        "done\n"
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("VERDICT: APPROVED"));
    server.join().unwrap();
}

#[test]
fn resume_does_not_replay_an_interrupted_tool() {
    let temp = tempfile::tempdir().unwrap();
    let (endpoint, server) = mock(|index, _| match index {
        0 => text("Append a line to result.txt once."),
        1 => calls(&[tool(
            "write",
            "shell",
            json!({"command":"printf 'once\\n' >> result.txt"}),
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
    std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();

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
        std::fs::read_to_string(temp.path().join("result.txt")).unwrap(),
        "once\n"
    );
    server.join().unwrap();
}

#[test]
fn provider_context_limit_retries_with_compacted_history() {
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
                    json!({"command":"yes abcdefghijklmnopqrstuvwxyz | head -n 100"}),
                )
            };
            return match worker_requests {
                1 => calls(&[command("old")]),
                2 => calls(&[command("recent")]),
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
fn parallel_panel_delivers_bwp_and_reactivates_finished_agents() {
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
                            json!({"to":"stakeholder_1","message":"probe:no spacing"}),
                        ),
                        tool(
                            "request",
                            "send_message",
                            json!({"to":"stakeholder_1","message":"probe: verify release diagnostic; result => report"}),
                        ),
                    ]),
                    !stakeholder_started,
                )
            } else if content.contains("From stakeholder_1: result: verified") {
                got_reply = true;
                (text("Reviewer corroborated peer evidence."), false)
            } else {
                assert!(content.contains("REFUSED"));
                assert!(content.contains("Queued for delivery"));
                (text("Initial reviewer findings."), false)
            }
        } else if system.contains("Your ID: stakeholder_1") {
            if !stakeholder_started {
                stakeholder_started = true;
                (text("Initial stakeholder findings."), !reviewer_started)
            } else if !content.contains("reply") {
                assert!(content.contains("From reviewer_1: probe: verify release diagnostic"));
                assert!(content.contains("Previous assessment before new peer evidence"));
                assert!(!content.contains("probe:no spacing"));
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
    exercise_worker_gate(true, false);
}

#[test]
fn wrong_plan_ack_and_plain_text_approval_keep_every_worker_read_only() {
    exercise_worker_gate(false, false);
}

#[test]
fn plan_objection_recoordinates_and_issues_a_fresh_digest() {
    exercise_worker_gate(true, true);
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

    let reply = "From stakeholder_1 (reply to worker_1): datum: local tide gauge";
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
                        json!({"topic":"Vertical datum","note":"Gauge metadata needs review."}),
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
            if history.contains("REFUSED: BWP") {
                assert!(history.contains("Tool error: tool create_file is unavailable"));
                calls(&[
                    tool(
                        "adviser-note",
                        "remember",
                        json!({"topic":"Vertical datum","note":"Use local tide gauge; DEM fixture shares datum."}),
                    ),
                    tool(
                        "reply",
                        "send_message",
                        json!({"to":"worker_1","message":"datum: local tide gauge"}),
                    ),
                ])
            } else {
                calls(&[
                    tool(
                        "bad-write",
                        "create_file",
                        json!({"path":"adviser.txt","content":"test"}),
                    ),
                    tool(
                        "bad-bwp",
                        "send_message",
                        json!({"to":"worker_1","message":"datum:no spacing"}),
                    ),
                ])
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
            assert!(!history.contains("datum:no spacing"));
            if history.contains("Shared memory (untrusted agent notes)") {
                assert!(history.contains("from worker_1 / Vertical datum"));
                assert!(history.contains("from stakeholder_1 / Vertical datum"));
                assert!(history.contains("DEM fixture shares datum"));
                verified[index] = true;
                text("Worker verified the clarification and both shared memories.")
            } else {
                calls(&[tool("recall", "recall", json!({"query":"DATUM vertical"}))])
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

fn exercise_worker_gate(unanimous: bool, objects: bool) {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ratio.toml");
    std::fs::write(
        &config,
        r#"
[orchestration]
max_parallel_agents = 3
[[workers]]
name = "One"
persona = "Own worker_1.txt."
[[workers]]
name = "Two"
persona = "Own worker_2.txt."
[[workers]]
name = "Three"
persona = "Own worker_3.txt."
"#,
    )
    .unwrap();
    let workspace = temp.path().to_path_buf();
    let mut coordinated = [false; 3];
    let mut started = [false; 3];
    let mut plan_id: Option<String> = None;
    let mut first_id: Option<String> = None;
    let mut replanning = false;
    let plan =
        "worker_1 owns worker_1.txt. worker_2 owns worker_2.txt. worker_3 owns worker_3.txt.";
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
                assert!(user_task.ends_with(plan));
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
                                json!({"command":"touch premature.txt"}),
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
                        calls(&[tool("ack", "ack_plan", json!({"plan_id":id}))]),
                        false,
                    ),
                }
            } else {
                assert!(unanimous);
                assert!(system.contains("Phase: execute"));
                assert!(mutators.iter().all(|tool| tools.contains(tool)));
                assert!(messages[1]["content"].as_str().unwrap().ends_with(plan));
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
                    (calls(&work), !started.iter().all(|&active| active))
                } else {
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
            assert!(coordinated.iter().all(|&active| active));
            assert!(task.contains("Assignment agreed for worker_3.txt."));
            (
                calls(&[tool(
                    "plan",
                    "submit_plan",
                    json!({"plan":plan,"ownership":{
                        "worker_1":[{"path":"./worker_1.txt","directory":false}],
                        "worker_2":[{"path":"worker_2.txt","directory":false}],
                        "worker_3":[{"path":"worker_3.txt","directory":false}]
                    }}),
                )]),
                false,
            )
        } else if task.starts_with("Inspect the resulting workspace") {
            assert!(unanimous);
            assert!(started.iter().all(|&active| active));
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
    if unanimous && !objects {
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
        json!({"command":"printf replay >> worker_1.txt"}),
    );
    std::fs::write(path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();

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
#[test]
fn tui_refines_and_approves_a_goal_then_displays_file_tools() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

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
    let (mut terminal, process) = launch_tui(temp.path(), &endpoint);
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
    terminal.write_all(b"\x1b[15~").unwrap();
    wait_for_screen(&mut terminal, &mut display, "Approved");
    terminal.write_all(b"\x1bOR").unwrap();
    wait_for_screen(&mut terminal, &mut display, "Communication / Sequence");
    wait_for_screen(&mut terminal, &mut display, "Final verdict");
    terminal.write_all(b"\x1bOS").unwrap();
    wait_for_screen(&mut terminal, &mut display, "Stakeholder broadcast");
    wait_for_screen(&mut terminal, &mut display, "trailing newline !required");
    terminal.write_all(b"\t").unwrap();
    wait_for_screen(&mut terminal, &mut display, "Outbound routes");
    terminal.write_all(b"\x1bOQ").unwrap();
    terminal.write_all(b"q").unwrap();
    wait_for_screen(&mut terminal, &mut display, "\x1b[?1049l");
    let run = process.finish();
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
fn launch_tui(cwd: &Path, endpoint: &str) -> (std::fs::File, TerminalChild) {
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
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_ra"));
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
            "--endpoint",
            endpoint,
        ])
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stderr(slave)
        .stdout(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
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
    fn finish(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for TerminalChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
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
