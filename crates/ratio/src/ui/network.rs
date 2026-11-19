use std::collections::{HashMap, VecDeque};

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

use crate::events::{AgentNode, Delivery, NetworkEvent, Traffic, TrafficKind, TrafficScope, clean};

const HISTORY: usize = 512;
const GUTTER: usize = 10;
const LANE: usize = 6;

#[derive(Default)]
pub(super) struct Network {
    agents: Vec<AgentNode>,
    history: VecDeque<Exchange>,
    serial: usize,
    selection: Option<usize>,
    lane: usize,
    source: usize,
    detail_scroll: u16,
    matrix: bool,
    filter: Filter,
}

#[derive(Default, Clone, Copy)]
enum Filter {
    #[default]
    All,
    Ipc,
    Handoffs,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All traffic",
            Self::Ipc => "BWP IPC",
            Self::Handoffs => "Handoffs",
        }
    }

    fn includes(self, exchange: &Exchange) -> bool {
        match self {
            Self::All => true,
            Self::Ipc => exchange.sequence.is_some(),
            Self::Handoffs => exchange.sequence.is_none(),
        }
    }
}

struct Exchange {
    id: usize,
    scope: TrafficScope,
    from: usize,
    to: Vec<(usize, Delivery)>,
    reply_to: Option<usize>,
    sequence: Option<usize>,
    label: &'static str,
    body: String,
}

#[derive(Default)]
struct RouteCounts {
    delivered: usize,
    queued: usize,
    dropped: usize,
    handoffs: usize,
}

impl RouteCounts {
    fn add(&mut self, exchange: &Exchange, delivery: Delivery) {
        if exchange.sequence.is_none() {
            self.handoffs += 1;
            return;
        }
        match delivery {
            Delivery::Queued => self.queued += 1,
            Delivery::Delivered => self.delivered += 1,
            Delivery::Dropped => self.dropped += 1,
        }
    }
}

impl Exchange {
    fn fanout(&self) -> bool {
        self.reply_to.is_some() || self.to.len() > 1
    }
}

impl Network {
    pub(super) fn ingest(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::Roster(agents) => {
                for AgentNode { id, name } in agents {
                    let index = self.agent(&id);
                    self.agents[index].name = clean(name).replace('\n', " ");
                }
            }
            NetworkEvent::Traffic(message) => self.ingest_traffic(message),
        }
    }

    fn agent(&mut self, id: &str) -> usize {
        if let Some(index) = self.agents.iter().position(|agent| agent.id == id) {
            return index;
        }
        let index = self.agents.len();
        self.agents.push(AgentNode {
            id: id.into(),
            name: id.into(),
        });
        index
    }

    fn ingest_traffic(&mut self, message: Traffic) {
        let from = self.agent(&message.from);
        let (sequence, reply_to, delivery, label) = match message.kind {
            TrafficKind::Ipc {
                sequence,
                reply_to,
                delivery,
            } => {
                let label = if reply_to.is_some() {
                    "Stakeholder broadcast"
                } else {
                    "Direct BWP IPC"
                };
                (
                    Some(sequence),
                    reply_to.map(|id| self.agent(&id)),
                    delivery,
                    label,
                )
            }
            TrafficKind::Handoff { label } => (None, None, Delivery::Delivered, label),
        };
        let to = message
            .to
            .iter()
            .map(|id| (self.agent(id), delivery))
            .collect::<Vec<_>>();
        let existing = sequence.and_then(|sequence| {
            self.history.iter_mut().rev().find(|exchange| {
                exchange.scope == message.scope
                    && exchange.from == from
                    && exchange.sequence == Some(sequence)
            })
        });
        if let Some(exchange) = existing {
            for (index, delivery) in to {
                match exchange
                    .to
                    .iter_mut()
                    .find(|(recipient, _)| *recipient == index)
                {
                    Some((_, status)) if delivery != Delivery::Queued => *status = delivery,
                    Some(_) => {}
                    None => exchange.to.push((index, delivery)),
                }
            }
            return;
        }
        self.serial += 1;
        self.history.push_back(Exchange {
            id: self.serial,
            scope: message.scope,
            from,
            to,
            reply_to,
            sequence,
            label,
            body: message.body,
        });
        if self.selection.is_none() && !self.matrix {
            self.detail_scroll = 0;
        }
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    pub(super) fn key(&mut self, key: KeyCode, modified: bool) -> bool {
        let amount = if matches!(key, KeyCode::PageUp | KeyCode::PageDown) {
            8
        } else {
            1
        };
        match key {
            KeyCode::Tab => {
                self.matrix = !self.matrix;
                self.detail_scroll = 0;
            }
            KeyCode::F(4) => {
                self.filter = match self.filter {
                    Filter::All => Filter::Ipc,
                    Filter::Ipc => Filter::Handoffs,
                    Filter::Handoffs => Filter::All,
                };
                self.selection = None;
                self.detail_scroll = 0;
            }
            KeyCode::Left => self.lane = self.lane.saturating_sub(1),
            KeyCode::Right => self.lane = (self.lane + 1).min(self.agents.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::PageUp if modified => {
                self.detail_scroll = self.detail_scroll.saturating_sub(amount as u16)
            }
            KeyCode::Down | KeyCode::PageDown if modified => {
                self.detail_scroll = self.detail_scroll.saturating_add(amount as u16)
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => {
                let backward = matches!(key, KeyCode::Up | KeyCode::PageUp);
                let (current, count) = if self.matrix {
                    (self.source, self.agents.len())
                } else {
                    let items = self.visible();
                    (self.selected(&items), items.len())
                };
                let last = count.saturating_sub(1);
                let target = match key {
                    KeyCode::Home => 0,
                    KeyCode::End => last,
                    _ if backward => current.saturating_sub(amount),
                    _ => (current + amount).min(last),
                };
                if self.matrix {
                    self.source = target;
                } else {
                    self.selection = (target < last).then(|| self.visible()[target].id);
                }
                self.detail_scroll = 0;
            }
            _ => return false,
        }
        true
    }

    fn visible(&self) -> Vec<&Exchange> {
        self.history
            .iter()
            .filter(|exchange| self.filter.includes(exchange))
            .collect()
    }

    fn selected(&self, items: &[&Exchange]) -> usize {
        self.selection.map_or_else(
            || items.len().saturating_sub(1),
            |id| {
                items
                    .partition_point(|exchange| exchange.id < id)
                    .min(items.len().saturating_sub(1))
            },
        )
    }

    pub(super) fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let [summary, content] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
        let items = self.visible();
        let mut counts = RouteCounts::default();
        for item in &items {
            for &(_, status) in &item.to {
                counts.add(item, status);
            }
        }
        let overview = Line::from(vec![
            Span::styled(
                format!(" {} ", self.filter.label()),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {} participants  |  {} messages",
                self.agents.len(),
                items.len()
            )),
        ]);
        let status = Line::from(vec![
            Span::styled(
                format!("  * {} delivered", counts.delivered),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("  ? {} queued", counts.queued),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("  x {} dropped", counts.dropped),
                Style::default().fg(Color::LightRed),
            ),
            Span::styled(
                format!("  + {} handoffs", counts.handoffs),
                Style::default().fg(Color::Cyan),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![overview, status]), summary);
        let [map, detail] = if content.width >= 108 {
            Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)])
                .areas(content)
        } else {
            Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)])
                .areas(content)
        };
        let mut detail_lines = if self.matrix {
            self.render_matrix(frame, map, &items)
        } else {
            self.render_timeline(frame, map, &items)
        };
        let block = Block::bordered().title(if self.matrix {
            " Outbound routes "
        } else {
            " Message inspector "
        });
        let inner = block.inner(detail);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        if detail_lines.is_empty() {
            detail_lines.push(Line::raw("Waiting for agent traffic."));
        }
        let paragraph = Paragraph::new(detail_lines).wrap(Wrap { trim: false });
        let max = paragraph
            .line_count(inner.width)
            .saturating_sub(usize::from(inner.height));
        self.detail_scroll = self
            .detail_scroll
            .min(u16::try_from(max).unwrap_or(u16::MAX));
        frame.render_widget(
            paragraph.scroll((self.detail_scroll, 0)).block(block),
            detail,
        );
    }

    fn render_timeline(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        items: &[&Exchange],
    ) -> Vec<Line<'static>> {
        let block = Block::bordered()
            .title(" Communication / Sequence ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        let lanes = self.lanes(inner.width);
        let selected = self.selected(items);
        let height = usize::from(inner.height).saturating_sub(2);
        let start = selected
            .saturating_sub(height.saturating_sub(1))
            .min(items.len().saturating_sub(height));
        let mut lines = vec![self.header(" MESSAGE", lanes.clone())];
        for (index, exchange) in items.iter().enumerate().skip(start).take(height) {
            let hint = if exchange.sequence.is_none() {
                "CTX"
            } else if exchange.fanout() {
                "FAN"
            } else {
                "IPC"
            };
            let number = format!(
                "{} {:>4} {:3}",
                if index == selected { '>' } else { ' ' },
                exchange.id % 10000,
                hint
            );
            let from = self.node(exchange.from);
            let diagram = diagram(exchange, lanes.clone());
            let mut style = Style::default().fg(role_color(&from.id));
            if index == selected {
                style = style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            }
            lines.push(Line::styled(format!("{number}{diagram}"), style));
        }
        lines.push(Line::styled(
            " o sender  * delivered  ? queued  x dropped  + handoff  = fan-out",
            Style::default().fg(Color::Gray),
        ));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        items
            .get(selected)
            .map_or_else(Vec::new, |item| self.inspect(item))
    }

    fn render_matrix(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        items: &[&Exchange],
    ) -> Vec<Line<'static>> {
        let block = Block::bordered()
            .title(" Communication / Routes ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        let lanes = self.lanes(inner.width);
        let visible = usize::from(inner.height).saturating_sub(2);
        let start = self
            .source
            .saturating_sub(visible.saturating_sub(1))
            .min(self.agents.len().saturating_sub(visible));
        let mut totals: HashMap<(usize, usize), RouteCounts> = HashMap::new();
        for exchange in items {
            for &(to, status) in &exchange.to {
                totals
                    .entry((exchange.from, to))
                    .or_default()
                    .add(exchange, status);
            }
        }
        let mut lines = vec![self.header(" FROM\\TO", lanes.clone())];
        for row in (start..self.agents.len()).take(visible) {
            let agent = self.node(row);
            let label = format!(
                "{} {:<8}",
                if row == self.source { '>' } else { ' ' },
                alias(&agent.id)
            );
            let mut spans = vec![Span::styled(
                label,
                Style::default().fg(role_color(&agent.id)),
            )];
            for column in lanes.clone() {
                let (complete, queued, dropped) =
                    totals.get(&(row, column)).map_or((0, 0, 0), |counts| {
                        (
                            counts.delivered + counts.handoffs,
                            counts.queued,
                            counts.dropped,
                        )
                    });
                let (value, color) = match (complete, queued, dropped) {
                    (0, 0, 0) => (".".into(), Color::DarkGray),
                    (0, 0, _) => ("x".into(), Color::LightRed),
                    (0, _, _) => ("?".into(), Color::Yellow),
                    (_, 0, 0) => (complete.to_string(), Color::LightGreen),
                    _ => (format!("{complete}+"), Color::LightYellow),
                };
                spans.push(Span::styled(
                    format!("{value:^LANE$}"),
                    Style::default().fg(color),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled(
            " Counts: delivered IPC + handoffs. Suffix + indicates pending or dropped.",
            Style::default().fg(Color::Gray),
        ));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        let Some(source) = self.agents.get(self.source) else {
            return Vec::new();
        };
        let mut detail = vec![agent_line(source, "")];
        for (target, agent) in self.agents.iter().enumerate() {
            let Some(counts) = totals.get(&(self.source, target)) else {
                continue;
            };
            detail.push(agent_line(agent, " -> "));
            detail.push(Line::raw(format!(
                "    {} delivered / {} queued / {} dropped / {} handoffs",
                counts.delivered, counts.queued, counts.dropped, counts.handoffs
            )));
        }
        if detail.len() == 1 {
            detail.push(Line::raw(" No outbound traffic in this filter."));
        }
        detail
    }

    fn lanes(&self, width: u16) -> std::ops::Range<usize> {
        let count = (usize::from(width).saturating_sub(GUTTER) / LANE)
            .max(1)
            .min(self.agents.len());
        let start = self.lane.min(self.agents.len().saturating_sub(count));
        start..start + count
    }

    fn header(&self, label: &str, lanes: std::ops::Range<usize>) -> Line<'static> {
        let mut row = vec![Span::styled(
            format!("{label:<GUTTER$}"),
            Style::default().fg(Color::Gray),
        )];
        for index in lanes {
            let agent = self.node(index);
            row.push(Span::styled(
                format!("{:^LANE$}", alias(&agent.id)),
                Style::default()
                    .fg(role_color(&agent.id))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(row)
    }

    fn node(&self, index: usize) -> &AgentNode {
        &self.agents[index]
    }

    fn inspect(&self, item: &Exchange) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
                format!(" #{}  {}", item.id, item.label),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!(
                    " Cycle {} / attempt {} / {}",
                    item.scope.cycle, item.scope.attempt, item.scope.phase
                ),
                Style::default().fg(Color::Gray),
            ),
            agent_line(self.node(item.from), " From "),
        ];
        if let Some(requester) = item.reply_to {
            lines.push(agent_line(self.node(requester), " Reply to "));
        }
        if item.sequence.is_some() {
            let mut counts = RouteCounts::default();
            for &(_, status) in &item.to {
                counts.add(item, status);
            }
            lines.push(Line::raw(format!(
                " {} delivered / {} queued / {} dropped",
                counts.delivered, counts.queued, counts.dropped
            )));
        }
        lines.push(Line::default());
        lines.extend(item.body.lines().map(|text| Line::raw(text.to_owned())));
        lines.push(Line::default());
        lines.push(Line::styled(
            format!(" Recipients ({})", item.to.len()),
            Style::default().fg(Color::Gray),
        ));
        for &(to, status) in &item.to {
            let (prefix, color) = match status {
                Delivery::Delivered if item.sequence.is_none() => ("  + ", Color::Cyan),
                Delivery::Delivered => ("  * ", Color::LightGreen),
                Delivery::Queued => ("  ? ", Color::Yellow),
                Delivery::Dropped => ("  x ", Color::LightRed),
            };
            let mut line = agent_line(self.node(to), "");
            line.spans
                .insert(0, Span::styled(prefix, Style::default().fg(color)));
            lines.push(line);
        }
        lines
    }
}

fn alias(id: &str) -> String {
    for (prefix, label) in [("worker_", "W"), ("reviewer_", "R"), ("stakeholder_", "S")] {
        if let Some(number) = id.strip_prefix(prefix) {
            return format!("{label}{number}");
        }
    }
    match id {
        "lead_reviewer" => "Lead",
        "user" => "You",
        _ => "Agent",
    }
    .into()
}

fn role_color(id: &str) -> Color {
    match id.split('_').next().unwrap_or_default() {
        "worker" => Color::LightBlue,
        "reviewer" | "lead" => Color::LightMagenta,
        "stakeholder" => Color::LightGreen,
        _ => Color::Cyan,
    }
}

fn agent_line(agent: &AgentNode, prefix: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(prefix.to_owned()),
        Span::styled(
            alias(&agent.id),
            Style::default()
                .fg(role_color(&agent.id))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {}", agent.name)),
    ])
}

fn diagram(exchange: &Exchange, lanes: std::ops::Range<usize>) -> String {
    let mut row = vec![b' '; lanes.len() * LANE];
    if row.is_empty() {
        return String::new();
    }
    for lane in 0..lanes.len() {
        row[lane * LANE + LANE / 2] = b'|';
    }
    let position =
        |agent| (agent as isize - lanes.start as isize) * LANE as isize + (LANE / 2) as isize;
    let from = position(exchange.from);
    let (mut left, mut right) = (from, from);
    for &(to, _) in &exchange.to {
        let end = position(to);
        left = left.min(end);
        right = right.max(end);
    }
    let width = row.len() as isize;
    if left < width && right >= 0 {
        let start = left.max(0) as usize;
        let end = right.min(width - 1) as usize;
        row[start..=end].fill(if exchange.fanout() { b'=' } else { b'-' });
        if left < 0 {
            row[0] = b'<';
        }
        if right >= width {
            row[(width - 1) as usize] = b'>';
        }
    }
    for &(to, status) in &exchange.to {
        let end = position(to);
        if (0..width).contains(&end) {
            row[end as usize] = match status {
                Delivery::Queued => b'?',
                Delivery::Delivered if exchange.sequence.is_none() => b'+',
                Delivery::Delivered => b'*',
                Delivery::Dropped => b'x',
            };
            let arrow = if end > from { end - 1 } else { end + 1 };
            row[arrow as usize] = if end > from { b'>' } else { b'<' };
        }
    }
    if (0..width).contains(&from) {
        row[from as usize] = b'o';
    }
    String::from_utf8(row).expect("ASCII diagram")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_fanout_as_one_exchange_with_independent_recipient_states() {
        let mut network = Network::default();
        network.ingest(NetworkEvent::Roster(
            ["worker_1", "worker_2", "worker_3", "stakeholder_1"]
                .into_iter()
                .map(|id| AgentNode {
                    id: id.into(),
                    name: id.into(),
                })
                .collect(),
        ));
        let event = |delivery, to: &[&str]| {
            NetworkEvent::Traffic(Traffic {
                scope: TrafficScope {
                    cycle: 1,
                    attempt: 1,
                    phase: "Worker",
                },
                from: "stakeholder_1".into(),
                to: to.iter().map(|id| (*id).into()).collect(),
                body: "datum: tide gauge".into(),
                kind: TrafficKind::Ipc {
                    sequence: 0,
                    reply_to: Some("worker_1".into()),
                    delivery,
                },
            })
        };
        network.ingest(event(
            Delivery::Queued,
            &["worker_1", "worker_2", "worker_3"],
        ));
        network.ingest(event(Delivery::Delivered, &["worker_1"]));
        network.ingest(event(Delivery::Dropped, &["worker_2"]));
        assert_eq!(network.history.len(), 1);
        let exchange = &network.history[0];
        let visible = diagram(exchange, 0..4);
        for mark in ["*<", "x<", "?<", "o"] {
            assert!(visible.contains(mark), "{visible}");
        }
        assert!(diagram(exchange, 0..2).ends_with('>'));

        let mut screen =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(76, 26)).unwrap();
        for matrix in [false, true] {
            network.matrix = matrix;
            network.source = 3;
            screen
                .draw(|frame| network.render(frame, frame.area()))
                .unwrap();
            let buffer = screen.backend().buffer();
            let row = |y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            let screen = (0..buffer.area.height)
                .map(row)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains(if matrix {
                "Communication / Routes"
            } else {
                "Communication / Sequence"
            }));
            assert!(screen.contains(if matrix {
                "1 delivered / 0 queued / 0 dropped / 0 handoffs"
            } else {
                "datum: tide gauge"
            }));
        }
    }
}
