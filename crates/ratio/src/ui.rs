use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::{self, Stderr},
    sync::mpsc,
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as Input, EventStream, KeyCode,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use ratio_engine::RunReader;
use ratio_protocol::{
    ControlAction, ControlReply, ControlRequest, RunEvent, TaskState, TaskView, Workflow,
};
use tokio::sync::{
    mpsc::{UnboundedReceiver, unbounded_channel},
    watch,
};

use crate::{
    api::Api,
    budget::Budget,
    config::{Config, ConsultationPhase},
    context::clip,
    events::{
        AgentNode, Event, Generation, GenerationControl, Kind, NetworkEvent, Reporter, Traffic,
        TrafficKind, TrafficScope, clean,
    },
    orchestrator::Orchestrator,
    session::{Phase, Session, Store},
    setup::{self, Draft},
};

mod network;

type Screen = Terminal<CrosstermBackend<Stderr>>;

struct TerminalMode;

impl TerminalMode {
    fn enter() -> Result<(Self, Screen)> {
        enable_raw_mode()?;
        let mode = Self;
        execute!(io::stderr(), EnterAlternateScreen, EnableBracketedPaste)?;
        let screen = Terminal::new(CrosstermBackend::new(io::stderr()))?;
        Ok((mode, screen))
    }
}

impl Drop for TerminalMode {
    fn drop(&mut self) {
        let _ = execute!(
            io::stderr(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
    }
}

pub enum Outcome {
    Finished(Box<Session>),
    Interrupted,
    BudgetExhausted(&'static str),
}

pub async fn run(
    config: &Config,
    store: &Store,
    resumed: Option<Session>,
    budget: Budget,
) -> Result<Outcome> {
    let (_mode, screen) = TerminalMode::enter()?;
    let (sender, receiver) = unbounded_channel();
    let (control, control_rx) = watch::channel(GenerationControl::default());
    let reporter = Reporter::Tui(sender, control_rx);
    let mut ui = Ui {
        screen,
        inputs: EventStream::new(),
        events: receiver,
        control,
        view: View::default(),
    };
    let session = match resumed {
        Some(session) => session,
        None => {
            let api = Api::new(config, reporter.clone(), budget.clone())?;
            let draft = match ui.setup(config, &api, &budget).await {
                Ok(Some(draft)) => draft,
                Ok(None) => return Ok(Outcome::Interrupted),
                Err(error) => {
                    if let Some(limit) = budget.exhaustion() {
                        return Ok(Outcome::BudgetExhausted(limit));
                    }
                    return Err(error);
                }
            };
            draft.into_session()
        }
    };
    let orchestrator = Orchestrator::new(config, store, reporter, budget)?;
    ui.execute(orchestrator, session, Store::reader(&config.cwd)?)
        .await
}

pub async fn attach<F, R>(reader: RunReader, run_id: &str, control: F) -> Result<u8>
where
    F: Fn(ControlRequest) -> R,
    R: Future<Output = Result<ControlReply>>,
{
    anyhow::ensure!(reader.run(run_id)?.is_some(), "unknown Ratio run {run_id}");
    let (_mode, mut screen) = TerminalMode::enter()?;
    let mut inputs = EventStream::new();
    let mut view = View {
        executing: true,
        status: format!("Run {run_id}"),
        ..View::default()
    };
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut after = 0;
    let mut done = false;
    if let Ok(reply) = control(ControlRequest {
        action: ControlAction::Status,
        guidance: None,
        run_id: Some(run_id.into()),
    })
    .await
    {
        view.paused = reply.accepted && reply.paused;
    }
    loop {
        screen
            .draw(|frame| view.draw(frame, None, if done { Mode::Done } else { Mode::Attached }))?;
        tokio::select! {
            _ = tick.tick() => {
                let events = reader.events(run_id, after, 256)?;
                for event in events {
                    after = event.sequence;
                    let phase = event_phase(&event);
                    done = phase.is_some_and(Phase::done) || event.workflow.stopped.is_some();
                    view.ingest_checkpoint(event, phase);
                }
            }
            event = inputs.next() => {
                let action = match view.input(terminal_input(event)?) {
                    Action::Escape | Action::Interrupt => return Ok(0),
                    Action::Submit if done => return Ok(0),
                    Action::ToggleGeneration if !done => {
                        Some(if view.paused { ControlAction::Resume } else { ControlAction::Pause })
                    }
                    Action::Submit if !done => Some(ControlAction::Guide),
                    _ if done && view.editor.text == "q" => return Ok(0),
                    _ => None,
                };
                if let Some(action) = action {
                    let guidance = (action == ControlAction::Guide).then(|| view.take_input()).flatten();
                    if action != ControlAction::Guide || guidance.is_some() {
                        match control(ControlRequest { action, guidance, run_id: Some(run_id.into()) }).await {
                            Ok(reply) if reply.run_id == run_id => {
                                view.paused = reply.paused;
                                view.push(Kind::Info, "Control", &reply.detail);
                            }
                            Ok(_) => view.push(Kind::Info, "Control", "This run is no longer active."),
                            Err(error) => view.push(Kind::Info, "Control", &format!("{error:#}")),
                        }
                    }
                }
            }
        }
    }
}

fn event_phase(event: &RunEvent) -> Option<Phase> {
    serde_json::from_value(serde_json::Value::String(event.phase.clone())).ok()
}

struct Ui {
    screen: Screen,
    inputs: EventStream,
    events: UnboundedReceiver<Event>,
    control: watch::Sender<GenerationControl>,
    view: View,
}

impl Ui {
    async fn setup(
        &mut self,
        config: &Config,
        api: &Api,
        budget: &Budget,
    ) -> Result<Option<Draft>> {
        let mut draft = Draft::seed(config);
        let mut ready = false;
        let mut pending = if draft.goal.trim().is_empty() {
            None
        } else {
            Some("Refine the supplied goal and suggest reviewers and stakeholders where appropriate.".to_owned())
        };
        self.view.status = "Setup · State your goal".into();
        self.view.follow();
        loop {
            if let Some(note) = pending.take() {
                self.view.status = "Setup · Drafting proposal…".into();
                let result = {
                    let request = setup::refine(config, api, &draft, &note);
                    tokio::pin!(request);
                    loop {
                        self.draw(Some(&draft), Mode::Drafting)?;
                        tokio::select! {
                            input = self.inputs.next() => match self.view.input(terminal_input(input)?) {
                                Action::Interrupt => return Ok(None),
                                Action::Escape => break None,
                                _ => {}
                            },
                            result = &mut request => break Some(result),
                            Some(event) = self.events.recv() => self.view.push_event(event),
                        }
                    }
                };
                while let Ok(event) = self.events.try_recv() {
                    self.view.push_event(event);
                }
                match result {
                    None => self.view.push(Kind::Info, "Setup", "Drafting cancelled."),
                    Some(Ok(proposal)) => {
                        self.view
                            .push(Kind::Agent, "Setup agent", &proposal.message);
                        draft = proposal;
                        self.view.proposal_scroll = 0;
                        ready = true;
                    }
                    Some(Err(error)) if budget.exhaustion().is_some() => return Err(error),
                    Some(Err(error)) => {
                        self.view
                            .push(Kind::Info, "Setup error", &format!("{error:#}"))
                    }
                }
            }
            self.view.status = if ready {
                "Setup · Review the proposal"
            } else {
                "Setup · State your goal"
            }
            .into();
            self.draw(Some(&draft), if ready { Mode::Ready } else { Mode::Setup })?;
            match self.view.input(terminal_input(self.inputs.next().await)?) {
                Action::Interrupt | Action::Escape => return Ok(None),
                Action::Approve if ready && self.view.editor.text.trim().is_empty() => {
                    return Ok(Some(draft));
                }
                Action::Approve if ready => self.view.push(
                    Kind::Info,
                    "Setup",
                    "Send or clear your unsent feedback before approving.",
                ),
                Action::Submit => {
                    if let Some(note) = self.view.take_input() {
                        self.view.push(Kind::Agent, "You", &note);
                        pending = Some(note);
                    }
                }
                _ => {}
            }
        }
    }

    async fn execute(
        &mut self,
        orchestrator: Orchestrator<'_>,
        session: Session,
        reader: RunReader,
    ) -> Result<Outcome> {
        let (send, receive) = mpsc::sync_channel(16);
        self.view.executing = true;
        self.view.status = session.phase.label().into();
        self.view.push(Kind::Phase, "Goal", &session.goal);
        let run_id = session.run_id().to_owned();
        let run = orchestrator.run(session, Some(receive));
        tokio::pin!(run);
        let mut workflow_tick = tokio::time::interval(Duration::from_millis(250));
        let result = loop {
            self.draw(None, Mode::Running)?;
            tokio::select! {
                input = self.inputs.next() => match self.view.input(terminal_input(input)?) {
                    Action::Interrupt if !self.view.paused && !self.view.generations.is_empty() => self.toggle_generation(),
                    Action::Interrupt => {
                        orchestrator.interrupt(&run_id)?;
                        return Ok(Outcome::Interrupted);
                    }
                    Action::ToggleGeneration => self.toggle_generation(),
                    Action::Submit => {
                        if let Some(note) = self.view.take_input() {
                            match send.try_send(note) {
                                Ok(()) => self.view.push(Kind::Info, "Guidance", "Queued for the next model request."),
                                Err(mpsc::TrySendError::Full(note) | mpsc::TrySendError::Disconnected(note)) => {
                                    self.view.editor = Editor::with_text(note);
                                    self.view.push(Kind::Info, "Guidance", "Could not queue guidance. Your text remains in the editor.");
                                }
                            }
                        }
                    }
                    _ => {}
                },
                Some(event) = self.events.recv() => {
                    self.view.push_event(event);
                    for _ in 0..127 {
                        let Ok(event) = self.events.try_recv() else { break };
                        self.view.push_event(event);
                    }
                },
                _ = workflow_tick.tick() => {
                    if let Some(event) = reader.latest_event(&run_id)? {
                        self.view.workflow = event.workflow;
                    }
                }
                result = &mut run => break result,
            }
        };
        while let Ok(event) = self.events.try_recv() {
            self.view.push_event(event);
        }
        if let Some(event) = reader.latest_event(&run_id)? {
            self.view.workflow = event.workflow;
        }
        match &result {
            Ok(session) => {
                self.view.status = session.phase.label().into();
                self.view
                    .push(Kind::Phase, session.phase.label(), &session.assessment);
            }
            Err(error) => {
                self.view.status = "Run failed".into();
                self.view.push(Kind::Info, "Error", &format!("{error:#}"));
            }
        }
        self.view.follow();
        self.view.editor = Editor::default();
        loop {
            self.draw(None, Mode::Done)?;
            match self.view.input(terminal_input(self.inputs.next().await)?) {
                Action::Interrupt | Action::Escape | Action::Submit => break,
                _ if self.view.editor.text == "q" => break,
                _ => {}
            }
        }
        Ok(Outcome::Finished(Box::new(result?)))
    }

    fn draw(&mut self, draft: Option<&Draft>, mode: Mode) -> Result<()> {
        self.screen
            .draw(|frame| self.view.draw(frame, draft, mode))?;
        Ok(())
    }

    fn toggle_generation(&mut self) {
        self.view.paused = !self.view.paused;
        self.control.send_modify(|state| {
            state.paused = self.view.paused;
            if state.paused {
                state.epoch = state.epoch.wrapping_add(1);
            }
        });
        let message = if self.view.paused {
            "Model requests paused. Press F6 to resume."
        } else {
            "Model requests resumed."
        };
        self.view.push(Kind::Info, "Generation", message);
    }
}

fn terminal_input(event: Option<io::Result<Input>>) -> Result<Input> {
    event
        .context("terminal input closed")?
        .context("read terminal event")
}

#[derive(Clone, Copy)]
enum Mode {
    Setup,
    Drafting,
    Ready,
    Running,
    Attached,
    Done,
}

enum Action {
    None,
    Submit,
    Approve,
    Escape,
    Interrupt,
    ToggleGeneration,
}

#[derive(Default)]
struct View {
    status: String,
    transcript: VecDeque<Line<'static>>,
    transcript_bytes: usize,
    editor: Editor,
    scroll_back: usize,
    proposal_scroll: u16,
    proposal_focused: bool,
    executing: bool,
    show_network: bool,
    show_workflow: bool,
    workflow: Workflow,
    workflow_scroll: u16,
    network: network::Network,
    generations: BTreeMap<usize, (String, String)>,
    selected_generation: usize,
    paused: bool,
}

impl View {
    fn ingest_checkpoint(&mut self, event: RunEvent, phase: Option<Phase>) {
        let label = phase.map_or("Recorded phase", Phase::label);
        let status = match event.workflow.stopped {
            Some(stop) => format!("Cycle {} / {label} / {stop:?}", event.cycle),
            None => format!("Cycle {} / {label}", event.cycle),
        };
        if self.status != status {
            self.status.clone_from(&status);
            self.push(Kind::Phase, &status, "");
        }
        if self.workflow.agents != event.workflow.agents {
            self.network.ingest(NetworkEvent::Roster(
                event
                    .workflow
                    .agents
                    .iter()
                    .map(|agent| AgentNode {
                        id: agent.id.clone(),
                        name: agent.name.clone(),
                    })
                    .collect(),
            ));
        }
        self.workflow = event.workflow;
        for effect in event.effects {
            self.push(
                Kind::Tool,
                &format!("{} / {}", effect.actor, effect.tool),
                &format!("{:?}", effect.state),
            );
        }
        for message in event.ipc {
            self.network.ingest(NetworkEvent::Traffic(Traffic {
                scope: TrafficScope {
                    cycle: event.cycle,
                    attempt: self.workflow.plan_attempt,
                    phase: label,
                },
                from: message.from,
                to: message.to,
                body: message.body,
                kind: TrafficKind::Ipc {
                    sequence: message.sequence,
                    reply_to: message.reply_to,
                    delivery: message.delivery,
                },
            }));
        }
    }

    fn push_event(&mut self, event: Event) {
        if let Some(generation) = event.generation {
            match generation {
                Generation::Preview { id, label, text } => {
                    self.generations.insert(id, (label, text));
                }
                Generation::End(id) => {
                    self.generations.remove(&id);
                    self.selected_generation = self
                        .selected_generation
                        .min(self.generations.len().saturating_sub(1));
                }
            }
            return;
        }
        if let Some(network) = event.network {
            self.network.ingest(network);
            return;
        }
        if matches!(event.kind, Kind::Phase) {
            self.status.clone_from(&event.title);
        }
        self.push(event.kind, &event.title, &event.body);
    }

    fn push(&mut self, kind: Kind, title: &str, body: &str) {
        let color = match kind {
            Kind::Phase => Color::Cyan,
            Kind::Agent => Color::LightMagenta,
            Kind::Tool => Color::Yellow,
            Kind::Output => Color::Green,
            Kind::Ipc => Color::LightCyan,
            Kind::Info => Color::LightBlue,
        };
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        let title = clean(clip(title, 512));
        let body = clean(clip(body, 24 * 1024));
        self.transcript_bytes += title.len();
        self.transcript.push_back(Line::styled(title, style));
        for line in body.lines() {
            self.transcript_bytes += line.len();
            self.transcript.push_back(Line::raw(line.to_owned()));
        }
        self.transcript.push_back(Line::default());
        while self.transcript.len() > 3000 || self.transcript_bytes > 512 * 1024 {
            if let Some(line) = self.transcript.pop_front() {
                self.transcript_bytes -= line
                    .spans
                    .iter()
                    .map(|span| span.content.len())
                    .sum::<usize>();
            }
        }
    }

    fn follow(&mut self) {
        self.scroll_back = 0;
    }

    fn input(&mut self, event: Input) -> Action {
        let key = match event {
            Input::Paste(text) => {
                self.editor.insert(&clean(text.replace("\r\n", "\n")));
                return Action::None;
            }
            Input::Key(key) if key.kind != KeyEventKind::Release => key,
            _ => return Action::None,
        };
        let modified = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if self.executing {
            match key.code {
                KeyCode::F(6) => return Action::ToggleGeneration,
                KeyCode::F(7) if !self.generations.is_empty() => {
                    self.selected_generation =
                        (self.selected_generation + 1) % self.generations.len();
                    return Action::None;
                }
                KeyCode::F(2) | KeyCode::F(3) | KeyCode::F(8) => {
                    self.show_network = key.code == KeyCode::F(3);
                    self.show_workflow = key.code == KeyCode::F(8);
                    return Action::None;
                }
                KeyCode::Up | KeyCode::PageUp if self.show_workflow => {
                    self.workflow_scroll = self
                        .workflow_scroll
                        .saturating_sub(if key.code == KeyCode::Up { 1 } else { 12 });
                    return Action::None;
                }
                KeyCode::Down | KeyCode::PageDown if self.show_workflow => {
                    self.workflow_scroll = self
                        .workflow_scroll
                        .saturating_add(if key.code == KeyCode::Down { 1 } else { 12 });
                    return Action::None;
                }
                _ if self.show_network && self.network.key(key.code, modified) => {
                    return Action::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Interrupt;
            }
            KeyCode::Esc => return Action::Escape,
            KeyCode::F(5) => return Action::Approve,
            KeyCode::Enter if modified || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.editor.insert("\n")
            }
            KeyCode::Enter => return Action::Submit,
            KeyCode::Tab => self.proposal_focused = !self.proposal_focused,
            KeyCode::PageUp if self.proposal_focused => {
                self.proposal_scroll = self.proposal_scroll.saturating_sub(12)
            }
            KeyCode::PageDown if self.proposal_focused => {
                self.proposal_scroll = self.proposal_scroll.saturating_add(12)
            }
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(12),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(12),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.cursor = 0,
            KeyCode::End => self.editor.cursor = self.editor.text.len(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Char(c) if !modified && !c.is_control() => {
                self.editor.insert(c.encode_utf8(&mut [0; 4]))
            }
            _ => {}
        }
        Action::None
    }

    fn take_input(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.editor).text;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>, draft: Option<&Draft>, mode: Mode) {
        let [header, content, composer, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(if matches!(mode, Mode::Done) { 0 } else { 5 }),
            Constraint::Length(2),
        ])
        .areas(frame.area());
        let title = Line::from(vec![
            Span::styled(
                " RATIO ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {}{}",
                self.status,
                if self.paused {
                    " · Model requests paused"
                } else {
                    ""
                }
            )),
        ]);
        let mut heading = vec![title];
        if self.executing {
            let tab = |text, active| {
                if active {
                    Span::styled(
                        text,
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(text, Style::default().fg(Color::Gray))
                }
            };
            heading.push(Line::from(vec![
                tab(" F2 Activity ", !self.show_network && !self.show_workflow),
                Span::raw("  "),
                tab(" F3 Communication ", self.show_network),
                Span::raw("  "),
                tab(" F8 Workflow ", self.show_workflow),
                Span::raw(format!("  {} generating", self.generations.len())),
            ]));
        }
        frame.render_widget(Paragraph::new(heading), header);
        if let Some(draft) = draft {
            let [discussion, proposal] = if content.width >= 92 {
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(content)
            } else {
                Layout::vertical([Constraint::Percentage(52), Constraint::Percentage(48)])
                    .areas(content)
            };
            self.render_activity(frame, discussion);
            self.render_proposal(frame, proposal, draft);
        } else if self.show_network {
            self.network.render(frame, content);
        } else if self.show_workflow {
            self.render_workflow(frame, content);
        } else {
            self.proposal_focused = false;
            self.render_activity(frame, content);
        }
        if !matches!(mode, Mode::Done) {
            let prompt = if matches!(mode, Mode::Running | Mode::Attached) {
                " Guidance "
            } else {
                " Message "
            };
            let block = Block::bordered().title(prompt);
            let inner = block.inner(composer);
            let mut text = self.editor.text.clone();
            text.insert(self.editor.cursor, '▏');
            let cursor_row = Paragraph::new(&text[..self.editor.cursor + '▏'.len_utf8()])
                .wrap(Wrap { trim: false })
                .line_count(inner.width);
            let input = Paragraph::new(text).wrap(Wrap { trim: false });
            let first = cursor_row.saturating_sub(usize::from(inner.height));
            frame.render_widget(input.scroll((line_offset(first), 0)).block(block), composer);
        }
        let hints = match mode {
            Mode::Running if self.show_network => {
                " Tab Sequence/Routes   F4 Filter   Arrows Navigate   Ctrl+Up/Down Inspector\n Enter Queue guidance   F2 Activity   F6 Pause/resume   Ctrl+C Interrupt"
            }
            Mode::Done if self.show_network => {
                " Tab Sequence/Routes   F4 Filter   Arrows Navigate   Ctrl+Up/Down Inspector\n F2 Activity   Enter / q Exit"
            }
            Mode::Setup => " Enter  Send    Tab  Choose pane    PgUp/PgDn  Scroll    Esc  Exit",
            Mode::Drafting => " Drafting…    Esc  Cancel request    Ctrl+C  Exit",
            Mode::Ready => {
                " Enter  Refine    F5  Approve and run    Tab  Choose pane    PgUp/PgDn  Scroll"
            }
            Mode::Running => {
                " Enter Queue guidance   PgUp/PgDn Scroll   F6 Pause/resume\n F7 Next live agent   Ctrl+C Cancel generation / Exit"
            }
            Mode::Attached => {
                " F2 Activity  F3 Communication  F8 Workflow  F6 Pause/resume\n Enter Queue guidance   Esc Detach"
            }
            Mode::Done => " PgUp/PgDn  Scroll    Enter / q  Exit",
        };
        frame.render_widget(
            Paragraph::new(hints)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            footer,
        );
    }

    fn render_workflow(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        if let Some(plan_id) = &self.workflow.plan_id {
            lines.push(Line::raw(format!("Plan {plan_id}")));
            lines.push(Line::raw(format!(
                "Acknowledged by {}",
                if self.workflow.acknowledgements.is_empty() {
                    "no workers".into()
                } else {
                    self.workflow.acknowledgements.join(", ")
                }
            )));
        }
        for TaskView {
            task,
            state,
            artifact_sha256,
        } in &self.workflow.tasks
        {
            let color = match state {
                TaskState::Complete => Color::Green,
                TaskState::Working => Color::Cyan,
                TaskState::Blocked | TaskState::AwaitingAck => Color::Yellow,
                TaskState::Ready => Color::White,
            };
            lines.push(Line::default());
            lines.push(Line::styled(
                format!("{} / {}  [{state:?}]", task.id, task.worker),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            if !task.depends_on.is_empty() {
                lines.push(Line::raw(format!("  <- {}", task.depends_on.join(", "))));
            }
            if let Some(digest) = artifact_sha256 {
                lines.push(Line::raw(format!("  Artifact {digest}")));
            }
            lines.push(Line::raw(clean(task.description.clone())));
            lines.extend(
                task.acceptance
                    .iter()
                    .map(|criterion| Line::raw(format!("  - {}", clean(criterion.clone())))),
            );
        }
        if let Some(receipt) = &self.workflow.verification {
            lines.push(Line::default());
            lines.push(Line::raw(format!("Candidate {}", receipt.candidate)));
            lines.push(Line::raw(if receipt.passed {
                "Verification passed"
            } else {
                "Verification failed"
            }));
        }
        if let Some(verdict) = &self.workflow.verdict {
            lines.push(Line::raw(format!("Reviewer verdict {verdict}")));
        }
        for receipt in &self.workflow.context {
            lines.push(Line::raw(format!(
                "{} folded {} exchanges, estimated tokens {} -> {}",
                receipt.actor, receipt.folded_groups, receipt.before_tokens, receipt.after_tokens
            )));
        }
        if lines.is_empty() {
            lines.push(Line::raw("Waiting for a plan or verification receipt."));
        }
        let block = Block::bordered().title(" Workflow ");
        let inner = block.inner(area);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max_scroll = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.workflow_scroll = self.workflow_scroll.min(line_offset(max_scroll));
        frame.render_widget(
            paragraph.scroll((self.workflow_scroll, 0)).block(block),
            area,
        );
    }

    fn render_activity(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let area =
            if let Some((label, text)) = self.generations.values().nth(self.selected_generation) {
                let [transcript, preview] = Layout::vertical([
                    Constraint::Min(2),
                    Constraint::Length((area.height / 3).clamp(3, 9).min(area.height)),
                ])
                .areas(area);
                let block = Block::bordered().title(format!(
                    " Live {}/{} · {label} ",
                    self.selected_generation + 1,
                    self.generations.len(),
                ));
                let inner = block.inner(preview);
                let paragraph = Paragraph::new(text.as_str()).wrap(Wrap { trim: false });
                let bottom = paragraph
                    .line_count(inner.width)
                    .saturating_sub(usize::from(inner.height));
                frame.render_widget(
                    paragraph.scroll((line_offset(bottom), 0)).block(block),
                    preview,
                );
                transcript
            } else {
                area
            };
        let block = Block::bordered().title(" Activity ");
        let block = if self.proposal_focused {
            block
        } else {
            block.border_style(Style::default().fg(Color::Cyan))
        };
        let inner = block.inner(area);
        let text = self.transcript.iter().cloned().collect::<Vec<_>>();
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let bottom = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.scroll_back = self.scroll_back.min(bottom);
        frame.render_widget(
            paragraph
                .scroll((line_offset(bottom - self.scroll_back), 0))
                .block(block),
            area,
        );
    }

    fn render_proposal(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, draft: &Draft) {
        let label = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::styled("Goal", label)];
        lines.extend(
            draft
                .goal
                .lines()
                .map(|line| Line::raw(clean(line.to_owned()))),
        );
        for (title, participants) in [
            ("Reviewers", &draft.reviewers),
            ("Stakeholders", &draft.stakeholders),
        ] {
            lines.push(Line::default());
            lines.push(Line::styled(title, label));
            if participants.is_empty() {
                lines.push(Line::raw("None"));
            }
            for stakeholder in participants {
                let phases = stakeholder
                    .phases
                    .iter()
                    .map(|phase| match phase {
                        ConsultationPhase::Planning => "planning",
                        ConsultationPhase::Review => "review",
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(Line::styled(
                    clean(format!("{} [{}]", stakeholder.name, phases)),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                lines.extend(
                    stakeholder
                        .persona
                        .lines()
                        .map(|line| Line::raw(clean(line.to_owned()))),
                );
            }
        }
        let block = Block::bordered().title(" Proposal ");
        let block = if self.proposal_focused {
            block.border_style(Style::default().fg(Color::Cyan))
        } else {
            block
        };
        let inner = block.inner(area);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let bottom = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.proposal_scroll = self.proposal_scroll.min(line_offset(bottom));
        frame.render_widget(
            paragraph.scroll((self.proposal_scroll, 0)).block(block),
            area,
        );
    }
}

fn line_offset(offset: usize) -> u16 {
    u16::try_from(offset).unwrap_or(u16::MAX)
}

#[derive(Default)]
struct Editor {
    text: String,
    cursor: usize,
}

impl Editor {
    fn with_text(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }

    fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    fn insert(&mut self, text: &str) {
        let mut length = text.len().min(8192usize.saturating_sub(self.text.len()));
        while !text.is_char_boundary(length) {
            length -= 1;
        }
        self.text.insert_str(self.cursor, &text[..length]);
        self.cursor += length;
    }
}
