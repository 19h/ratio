use std::collections::BTreeMap;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use ratio_protocol::{EffectState, LiveGeneration, TaskState, Workflow};

use crate::{
    context::clip,
    events::{Generation, Kind, clean},
};

#[derive(Default)]
pub(super) struct Agents {
    roster: BTreeMap<String, Agent>,
    requests: BTreeMap<usize, LiveGeneration>,
    pub(super) scroll: usize,
}

#[derive(Default)]
struct Agent {
    name: String,
    activity: Option<Activity>,
    task: Option<(String, TaskState)>,
}

struct Activity {
    state: &'static str,
    detail: String,
    color: Color,
    running: bool,
}

impl Agents {
    pub(super) fn register(&mut self, id: &str, name: &str) {
        if id != "user" {
            self.roster.entry(id.into()).or_default().name = clean(clip(name, 180));
        }
    }

    pub(super) fn workflow(&mut self, workflow: &Workflow) {
        for agent in &workflow.agents {
            self.register(&agent.id, &agent.name);
        }
        for agent in self.roster.values_mut() {
            agent.task = None;
        }
        for task in &workflow.tasks {
            self.roster
                .entry(task.task.worker.clone())
                .or_default()
                .task = Some((clean(clip(&task.task.id, 180)), task.state));
        }
    }

    pub(super) fn generation(&mut self, event: Generation) {
        match event {
            Generation::Preview(preview) => {
                self.requests.insert(preview.id, preview);
            }
            Generation::End(id) => {
                self.requests.remove(&id);
            }
        }
    }

    pub(super) fn sync_generations(&mut self, previews: Vec<LiveGeneration>) {
        self.requests = previews
            .into_iter()
            .map(|preview| (preview.id, preview))
            .collect();
    }

    pub(super) fn generating(&self) -> usize {
        self.requests.len()
    }

    pub(super) fn finish(&mut self) {
        self.requests.clear();
        for agent in self.roster.values_mut() {
            if let Some(activity) = agent.activity.as_mut().filter(|activity| activity.running) {
                activity.running = false;
                activity.state = "Last started";
                activity.color = Color::Yellow;
            }
        }
    }

    pub(super) fn observe(&mut self, actor: &str, kind: Kind, title: &str, body: &str) {
        let (state, color, running) = match kind {
            Kind::Tool if body.is_empty() => ("Tool running", Color::Yellow, true),
            Kind::Tool => ("Decision recorded", Color::Green, false),
            Kind::Output if body.starts_with("REFUSED") || body.starts_with("Tool error") => {
                ("Tool refused", Color::LightRed, false)
            }
            Kind::Output => ("Tool result", Color::Green, false),
            Kind::Agent => ("Latest report", Color::LightMagenta, false),
            Kind::Info => ("Notice", Color::LightBlue, false),
            _ => return,
        };
        self.record(actor, state, title, body, color, running);
    }

    pub(super) fn effect(&mut self, actor: &str, tool: &str, effect: EffectState) {
        let (state, color, running) = match effect {
            EffectState::Intended => ("Tool pending", Color::Yellow, true),
            EffectState::Recorded => ("Tool recorded", Color::Green, false),
            EffectState::Unknown => ("Outcome unknown", Color::LightRed, false),
        };
        self.record(actor, state, tool, "", color, running);
    }

    fn record(
        &mut self,
        actor: &str,
        state: &'static str,
        title: &str,
        body: &str,
        color: Color,
        running: bool,
    ) {
        let detail = if body.trim().is_empty() {
            clip(title, 240)
        } else if matches!(state, "Tool result" | "Tool refused") {
            format!("{}\n{}", clip(title, 240), clip(body, 768))
        } else {
            clip(body, 768)
        };
        let agent = self.roster.entry(actor.into()).or_default();
        agent.activity = Some(Activity {
            state,
            detail: clean(detail),
            color,
            running,
        });
    }

    pub(super) fn render(&mut self, frame: &mut Frame<'_>, area: Rect) -> Rect {
        let mut actors: BTreeMap<&str, (&Agent, Option<&LiveGeneration>)> = self
            .roster
            .iter()
            .map(|(id, agent)| (id.as_str(), (agent, None)))
            .collect();
        let placeholder = Agent::default();
        for request in self.requests.values() {
            actors
                .entry(&request.actor)
                .or_insert((&placeholder, None))
                .1 = Some(request);
        }
        if actors.is_empty() || area.height < 7 || area.width < 22 {
            return area;
        }
        let mut actors: Vec<_> = actors.into_iter().collect();
        actors.sort_by_key(|(_, (agent, request))| {
            if request.is_some()
                || agent
                    .activity
                    .as_ref()
                    .is_some_and(|activity| activity.running)
            {
                0
            } else if agent
                .task
                .as_ref()
                .is_some_and(|(_, state)| *state == TaskState::Working)
            {
                1
            } else if agent.activity.is_some() || agent.task.is_some() {
                2
            } else {
                3
            }
        });
        let columns = match area.width {
            0..=65 => 1,
            66..=107 => 2,
            108..=155 => 3,
            _ => 4,
        };
        let needed_rows = actors.len().div_ceil(columns);
        let available = usize::from(area.height.saturating_sub(4).min(21));
        let rows = needed_rows.min((available.saturating_sub(2) / 2).max(1));
        let height = ((available.saturating_sub(2) / rows).clamp(2, 5) * rows + 2)
            .min(usize::from(area.height) - 2) as u16;
        let [board, transcript] =
            Layout::vertical([Constraint::Length(height), Constraint::Min(2)]).areas(area);
        let slots = rows * columns;
        self.scroll = self.scroll.min(actors.len().saturating_sub(slots));
        let end = (self.scroll + slots).min(actors.len());
        let block = Block::bordered().title(format!(
            " Agents {}-{}/{}  |  {} generating ",
            self.scroll + 1,
            end,
            actors.len(),
            self.requests.len()
        ));
        let inner = block.inner(board);
        frame.render_widget(block, board);
        let row_areas = Layout::vertical(vec![Constraint::Fill(1); rows]).split(inner);
        for ((id, (agent, request)), cell) in
            actors[self.scroll..end]
                .iter()
                .zip(row_areas.iter().flat_map(|row| {
                    Layout::horizontal(vec![Constraint::Fill(1); columns])
                        .split(*row)
                        .to_vec()
                }))
        {
            render_card(frame, cell, id, agent, *request);
        }
        transcript
    }
}

fn render_card(
    frame: &mut Frame<'_>,
    area: Rect,
    id: &str,
    agent: &Agent,
    request: Option<&LiveGeneration>,
) {
    let name = if !agent.name.is_empty() {
        agent.name.as_str()
    } else if let Some(request) = request {
        request.label.as_str()
    } else {
        id
    };
    let (state, color) = if request.is_some() {
        ("Generating", Color::Cyan)
    } else if let Some(activity) = &agent.activity {
        (activity.state, activity.color)
    } else {
        ("", Color::DarkGray)
    };
    let task = agent
        .task
        .as_ref()
        .map(|(id, state)| format!("{id} / {state:?}"));
    let heading = Line::from(vec![
        Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            if state.is_empty() {
                String::new()
            } else {
                format!(" / {state}")
            },
            Style::default().fg(color),
        ),
        Span::raw(if name == id {
            String::new()
        } else {
            format!(" / {name}")
        }),
    ]);
    let text = if let Some(request) = request {
        request.text.as_str()
    } else if let Some(activity) = &agent.activity {
        activity.detail.as_str()
    } else {
        task.as_deref().unwrap_or("No activity yet")
    };
    let [header, detail] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let header = Rect {
        x: header.x + 1,
        width: header.width.saturating_sub(2),
        ..header
    };
    let detail = Rect {
        x: detail.x + 1,
        width: detail.width.saturating_sub(2),
        ..detail
    };
    frame.render_widget(Paragraph::new(heading), header);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let offset = if request.is_some() {
        paragraph
            .line_count(detail.width)
            .saturating_sub(usize::from(detail.height))
    } else {
        0
    };
    frame.render_widget(
        paragraph.scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
        detail,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn three_worker_previews_and_a_tool_are_visible_in_one_frame() {
        let mut agents = Agents::default();
        agents.sync_generations(
            (1..=3)
                .map(|id| LiveGeneration {
                    id,
                    actor: format!("worker_{id}"),
                    label: format!("Worker {id}"),
                    text: format!("parallel-preview-{id}"),
                })
                .collect(),
        );
        agents.observe("stakeholder_1", Kind::Tool, "Inspecting datum", "");
        let mut terminal = Terminal::new(TestBackend::new(80, 17)).unwrap();
        terminal
            .draw(|frame| {
                let transcript = agents.render(frame, frame.area());
                assert!(transcript.height >= 4);
            })
            .unwrap();
        let displayed = terminal.backend().to_string();
        for preview in [
            "parallel-preview-1",
            "parallel-preview-2",
            "parallel-preview-3",
            "Inspecting datum",
        ] {
            assert!(
                displayed.contains(preview),
                "{preview} missing from {displayed}"
            );
        }
    }
}
