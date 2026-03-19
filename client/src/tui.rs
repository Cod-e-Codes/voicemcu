use std::collections::VecDeque;
use std::io::{self, Stdout};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use voicemcu_common::protocol::{ClientId, ClientInfo, SignalMessage};

pub const DEFAULT_MAX_EVENTS: usize = 1000;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PeerEntry {
    pub client_id: ClientId,
    pub display_name: String,
    pub self_muted: bool,
    pub server_muted: bool,
    pub is_host: bool,
}

pub enum TuiEvent {
    Signal(SignalMessage),
    Disconnected,
}

pub struct AppState {
    pub client_id: ClientId,
    pub room_code: String,
    pub display_name: String,
    pub is_host: bool,
    pub self_muted: bool,
    pub server_muted: bool,
    pub peers: Vec<PeerEntry>,
    pub events: VecDeque<(String, Style)>,
    pub input: String,
    pub should_quit: bool,
    max_events: usize,
}

impl AppState {
    pub fn new(
        client_id: ClientId,
        room_code: String,
        display_name: String,
        max_events: usize,
    ) -> Self {
        Self {
            client_id,
            room_code,
            display_name,
            is_host: false,
            self_muted: false,
            server_muted: false,
            peers: Vec::new(),
            events: VecDeque::new(),
            input: String::new(),
            should_quit: false,
            max_events,
        }
    }

    pub fn add_event(&mut self, text: impl Into<String>, style: Style) {
        self.events.push_back((text.into(), style));
        if self.events.len() > self.max_events {
            self.events.pop_front();
        }
    }

    pub fn set_peers_from_info(&mut self, clients: &[ClientInfo]) {
        self.peers = clients
            .iter()
            .map(|c| PeerEntry {
                client_id: c.client_id,
                display_name: c.display_name.clone(),
                self_muted: c.muted,
                server_muted: c.server_muted,
                is_host: c.is_host,
            })
            .collect();
        if let Some(me) = clients.iter().find(|c| c.client_id == self.client_id) {
            self.is_host = me.is_host;
            self.self_muted = me.muted;
            self.server_muted = me.server_muted;
        }
    }

    pub fn has_duplicate_name(&self, name: &str) -> bool {
        self.peers
            .iter()
            .filter(|p| p.display_name.eq_ignore_ascii_case(name))
            .count()
            > 1
    }

    pub fn find_peer(&self, name_or_id: &str) -> Result<ClientId, &'static str> {
        if let Ok(id) = name_or_id.parse::<ClientId>()
            && self.peers.iter().any(|p| p.client_id == id)
        {
            return Ok(id);
        }
        let matches: Vec<_> = self
            .peers
            .iter()
            .filter(|p| p.display_name.eq_ignore_ascii_case(name_or_id))
            .collect();
        match matches.len() {
            1 => Ok(matches[0].client_id),
            0 => Err("unknown peer"),
            _ => Err("ambiguous name -- use the numeric client ID instead"),
        }
    }

    pub fn handle_signal(&mut self, msg: SignalMessage) {
        match msg {
            SignalMessage::ClientJoined {
                client_id,
                display_name,
            } => {
                self.peers.push(PeerEntry {
                    client_id,
                    display_name: display_name.clone(),
                    self_muted: false,
                    server_muted: false,
                    is_host: false,
                });
                self.add_event(
                    format!("{display_name} joined"),
                    Style::new().fg(Color::Green),
                );
            }
            SignalMessage::ClientLeft { client_id } => {
                let name = self
                    .peers
                    .iter()
                    .find(|p| p.client_id == client_id)
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| format!("#{client_id}"));
                self.peers.retain(|p| p.client_id != client_id);
                self.add_event(format!("{name} left"), Style::new().fg(Color::Red));
            }
            SignalMessage::RoomInfo { clients } => {
                self.set_peers_from_info(&clients);
                self.add_event("roster updated", Style::new().fg(Color::DarkGray));
            }
            SignalMessage::YouAreHost => {
                self.is_host = true;
                if let Some(me) = self
                    .peers
                    .iter_mut()
                    .find(|p| p.client_id == self.client_id)
                {
                    me.is_host = true;
                }
                self.add_event("you are now the host", Style::new().fg(Color::Cyan).bold());
            }
            SignalMessage::Kicked { reason } => {
                self.add_event(
                    format!("kicked: {reason}"),
                    Style::new().fg(Color::Red).bold(),
                );
                self.should_quit = true;
            }
            SignalMessage::PeerMuted {
                client_id,
                muted,
                by_server,
            } => {
                let name = self
                    .peers
                    .iter()
                    .find(|p| p.client_id == client_id)
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| format!("#{client_id}"));

                if let Some(p) = self.peers.iter_mut().find(|p| p.client_id == client_id) {
                    if by_server {
                        p.server_muted = muted;
                    } else {
                        p.self_muted = muted;
                    }
                }
                if client_id == self.client_id {
                    if by_server {
                        self.server_muted = muted;
                    } else {
                        self.self_muted = muted;
                    }
                }

                let action = match (by_server, muted) {
                    (false, true) => "muted",
                    (false, false) => "unmuted",
                    (true, true) => "silenced by host",
                    (true, false) => "unsilenced by host",
                };
                self.add_event(format!("{name} {action}"), Style::new().fg(Color::Yellow));
            }
            SignalMessage::Error { message } => {
                self.add_event(format!("error: {message}"), Style::new().fg(Color::Red));
            }
            _ => {}
        }
    }

    /// Parse input, clear it, and return a signal to send (if any).
    pub fn process_command(&mut self) -> Option<SignalMessage> {
        let input = self.input.trim().to_string();
        self.input.clear();

        if input.is_empty() {
            return None;
        }

        if !input.starts_with('/') {
            self.add_event(
                format!("unknown input (commands start with /): {input}"),
                Style::new().fg(Color::DarkGray),
            );
            return None;
        }

        let mut parts = input[1..].splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();

        match cmd {
            "mute" => {
                self.self_muted = !self.self_muted;
                let verb = if self.self_muted { "muted" } else { "unmuted" };
                self.add_event(
                    format!("you {verb} yourself"),
                    Style::new().fg(Color::Yellow),
                );
                if !self.self_muted && self.server_muted {
                    self.add_event(
                        "note: still silenced by host",
                        Style::new().fg(Color::DarkGray),
                    );
                }
                Some(SignalMessage::Mute {
                    muted: self.self_muted,
                })
            }
            "kick" => {
                if arg.is_empty() {
                    self.add_event("usage: /kick <peer>", Style::new().fg(Color::DarkGray));
                    return None;
                }
                if !self.is_host {
                    self.add_event("only the host can kick", Style::new().fg(Color::Red));
                    return None;
                }
                match self.find_peer(arg) {
                    Ok(target) => Some(SignalMessage::Kick { target }),
                    Err(reason) => {
                        self.add_event(format!("{reason}: {arg}"), Style::new().fg(Color::Red));
                        None
                    }
                }
            }
            "forcemute" => {
                if arg.is_empty() {
                    self.add_event("usage: /forcemute <peer>", Style::new().fg(Color::DarkGray));
                    return None;
                }
                if !self.is_host {
                    self.add_event("only the host can force-mute", Style::new().fg(Color::Red));
                    return None;
                }
                match self.find_peer(arg) {
                    Ok(target) => Some(SignalMessage::ServerMute {
                        target,
                        muted: true,
                    }),
                    Err(reason) => {
                        self.add_event(format!("{reason}: {arg}"), Style::new().fg(Color::Red));
                        None
                    }
                }
            }
            "forceunmute" => {
                if arg.is_empty() {
                    self.add_event(
                        "usage: /forceunmute <peer>",
                        Style::new().fg(Color::DarkGray),
                    );
                    return None;
                }
                if !self.is_host {
                    self.add_event(
                        "only the host can force-unmute",
                        Style::new().fg(Color::Red),
                    );
                    return None;
                }
                match self.find_peer(arg) {
                    Ok(target) => Some(SignalMessage::ServerMute {
                        target,
                        muted: false,
                    }),
                    Err(reason) => {
                        self.add_event(format!("{reason}: {arg}"), Style::new().fg(Color::Red));
                        None
                    }
                }
            }
            "block" => {
                if arg.is_empty() {
                    self.add_event("usage: /block <peer>", Style::new().fg(Color::DarkGray));
                    return None;
                }
                match self.find_peer(arg) {
                    Ok(target) => {
                        let name = self
                            .peers
                            .iter()
                            .find(|p| p.client_id == target)
                            .map(|p| p.display_name.as_str())
                            .unwrap_or("?");
                        self.add_event(format!("blocked {name}"), Style::new().fg(Color::Yellow));
                        Some(SignalMessage::BlockPeer { target })
                    }
                    Err(reason) => {
                        self.add_event(format!("{reason}: {arg}"), Style::new().fg(Color::Red));
                        None
                    }
                }
            }
            "unblock" => {
                if arg.is_empty() {
                    self.add_event("usage: /unblock <peer>", Style::new().fg(Color::DarkGray));
                    return None;
                }
                match self.find_peer(arg) {
                    Ok(target) => {
                        let name = self
                            .peers
                            .iter()
                            .find(|p| p.client_id == target)
                            .map(|p| p.display_name.as_str())
                            .unwrap_or("?");
                        self.add_event(format!("unblocked {name}"), Style::new().fg(Color::Yellow));
                        Some(SignalMessage::UnblockPeer { target })
                    }
                    Err(reason) => {
                        self.add_event(format!("{reason}: {arg}"), Style::new().fg(Color::Red));
                        None
                    }
                }
            }
            "leave" | "quit" | "q" => {
                self.should_quit = true;
                Some(SignalMessage::Leave)
            }
            "help" | "?" => {
                let help = [
                    "/mute              toggle self-mute",
                    "/kick <peer>       remove peer from room (host)",
                    "/forcemute <peer>  server-side mute (host)",
                    "/forceunmute <peer> undo server-mute (host)",
                    "/block <peer>      block peer from your mix",
                    "/unblock <peer>    unblock peer",
                    "/leave             disconnect and exit",
                    "/help              show this message",
                    "",
                    "<peer> is a display name or client ID number",
                ];
                for line in help {
                    self.add_event(line, Style::new().fg(Color::White));
                }
                None
            }
            other => {
                self.add_event(
                    format!("unknown command: /{other} -- type /help"),
                    Style::new().fg(Color::Red),
                );
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

pub type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

pub fn restore_terminal(mut terminal: Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_raw_mode().ok();
        execute!(io::stdout(), LeaveAlternateScreen).ok();
        original(info);
    }));
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn draw(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(frame.area());

    let cols = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(chunks[0]);

    draw_peers(frame, state, cols[0]);
    draw_events(frame, state, cols[1]);
    draw_input(frame, state, chunks[1]);
}

fn draw_peers(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {
    let peer_count = state.peers.len();
    let title = format!(" Peers ({peer_count}) ");
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::DarkGray));

    let lines: Vec<Line> = state
        .peers
        .iter()
        .map(|p| {
            let marker = if p.client_id == state.client_id {
                ">"
            } else {
                " "
            };
            let id_str = format!("{marker}#{:<4}", p.client_id);
            let id_span = Span::styled(id_str, Style::new().fg(Color::DarkGray));

            let is_dup = state.has_duplicate_name(&p.display_name);
            let name_style = if p.client_id == state.client_id {
                Style::new().fg(Color::White).bold()
            } else if is_dup {
                Style::new().fg(Color::LightYellow)
            } else {
                Style::new().fg(Color::White)
            };
            let name_span = Span::styled(format!("{} ", p.display_name), name_style);

            let mut spans = vec![id_span, name_span];

            if is_dup {
                spans.push(Span::styled("(!) ", Style::new().fg(Color::LightYellow)));
            }

            if p.is_host {
                spans.push(Span::styled("HOST ", Style::new().fg(Color::Cyan)));
            }
            if p.server_muted {
                spans.push(Span::styled("SILENCED ", Style::new().fg(Color::Red)));
            } else if p.self_muted {
                spans.push(Span::styled("MUTE ", Style::new().fg(Color::Yellow)));
            }

            Line::from(spans)
        })
        .collect();

    let widget = Paragraph::new(lines).block(block);
    frame.render_widget(widget, area);
}

fn draw_events(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {
    let block = Block::bordered()
        .title(" Events ")
        .border_style(Style::new().fg(Color::DarkGray));

    let inner_height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = state
        .events
        .iter()
        .map(|(text, style)| Line::from(Span::styled(text.as_str(), *style)))
        .collect();

    let scroll = if lines.len() > inner_height {
        (lines.len() - inner_height) as u16
    } else {
        0
    };

    let widget = Paragraph::new(lines).block(block).scroll((scroll, 0));
    frame.render_widget(widget, area);
}

fn draw_input(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {
    let mut title_spans = vec![
        Span::styled(" ", Style::new()),
        Span::styled(&state.display_name, Style::new().fg(Color::White).bold()),
        Span::styled(
            format!(" | room: {} ", state.room_code),
            Style::new().fg(Color::DarkGray),
        ),
    ];
    if state.is_host {
        title_spans.push(Span::styled("[HOST] ", Style::new().fg(Color::Cyan)));
    }
    if state.server_muted {
        title_spans.push(Span::styled("[SILENCED] ", Style::new().fg(Color::Red)));
    } else if state.self_muted {
        title_spans.push(Span::styled("[MUTED] ", Style::new().fg(Color::Yellow)));
    }

    let block = Block::bordered()
        .title(Line::from(title_spans))
        .border_style(Style::new().fg(Color::DarkGray));

    let input_text = Line::from(vec![
        Span::styled("> ", Style::new().fg(Color::DarkGray)),
        Span::raw(&state.input),
    ]);
    let widget = Paragraph::new(input_text).block(block);
    frame.render_widget(widget, area);

    let cursor_x = area.x + 3 + state.input.len() as u16;
    let cursor_y = area.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

pub async fn run(
    terminal: &mut Term,
    state: &mut AppState,
    event_rx: &mut mpsc::Receiver<TuiEvent>,
    cmd_tx: &mpsc::Sender<SignalMessage>,
) {
    let mut es = EventStream::new();

    loop {
        terminal.draw(|f| draw(f, state)).ok();

        if state.should_quit {
            break;
        }

        tokio::select! {
            ct_event = poll_event_stream(&mut es) => {
                if let Some(Ok(Event::Key(key))) = ct_event
                    && key.kind == KeyEventKind::Press {
                        handle_key(state, key, cmd_tx).await;
                    }
            }
            tui_event = event_rx.recv() => {
                match tui_event {
                    Some(TuiEvent::Signal(msg)) => state.handle_signal(msg),
                    Some(TuiEvent::Disconnected) | None => {
                        state.add_event(
                            "disconnected from server",
                            Style::new().fg(Color::Red).bold(),
                        );
                        state.should_quit = true;
                    }
                }
            }
        }
    }

    terminal.draw(|f| draw(f, state)).ok();
}

/// Minimal poll helper: reads one crossterm event without pulling in `futures::StreamExt`.
async fn poll_event_stream(es: &mut EventStream) -> Option<Result<Event, std::io::Error>> {
    use futures_core::Stream;
    use std::pin::Pin;

    std::future::poll_fn(|cx| Pin::new(&mut *es).poll_next(cx)).await
}

async fn handle_key(state: &mut AppState, key: KeyEvent, cmd_tx: &mpsc::Sender<SignalMessage>) {
    match key.code {
        KeyCode::Enter => {
            if let Some(msg) = state.process_command() {
                cmd_tx.send(msg).await.ok();
            }
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                state.should_quit = true;
                cmd_tx.send(SignalMessage::Leave).await.ok();
            } else {
                state.input.push(c);
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Esc => {
            state.should_quit = true;
            cmd_tx.send(SignalMessage::Leave).await.ok();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AppState {
        let mut s = AppState::new(1, "room-1".into(), "alice".into(), DEFAULT_MAX_EVENTS);
        s.peers.push(PeerEntry {
            client_id: 1,
            display_name: "alice".into(),
            self_muted: false,
            server_muted: false,
            is_host: true,
        });
        s.peers.push(PeerEntry {
            client_id: 2,
            display_name: "bob".into(),
            self_muted: false,
            server_muted: false,
            is_host: false,
        });
        s.is_host = true;
        s
    }

    #[test]
    fn find_peer_by_id() {
        let state = make_state();
        assert_eq!(state.find_peer("2"), Ok(2));
    }

    #[test]
    fn find_peer_by_name_case_insensitive() {
        let state = make_state();
        assert_eq!(state.find_peer("BOB"), Ok(2));
        assert_eq!(state.find_peer("bob"), Ok(2));
    }

    #[test]
    fn find_peer_unknown() {
        let state = make_state();
        assert!(state.find_peer("nobody").is_err());
        assert!(state.find_peer("999").is_err());
    }

    #[test]
    fn command_mute_toggles() {
        let mut state = make_state();
        assert!(!state.self_muted);

        state.input = "/mute".into();
        let msg = state.process_command();
        assert!(state.self_muted);
        assert_eq!(msg, Some(SignalMessage::Mute { muted: true }));

        state.input = "/mute".into();
        let msg = state.process_command();
        assert!(!state.self_muted);
        assert_eq!(msg, Some(SignalMessage::Mute { muted: false }));
    }

    #[test]
    fn command_mute_warns_when_server_muted() {
        let mut state = make_state();
        state.server_muted = true;
        state.self_muted = true;

        state.input = "/mute".into();
        let msg = state.process_command();
        assert!(!state.self_muted);
        assert_eq!(msg, Some(SignalMessage::Mute { muted: false }));
        assert!(
            state
                .events
                .back()
                .unwrap()
                .0
                .contains("still silenced by host")
        );
    }

    #[test]
    fn command_kick_requires_host() {
        let mut state = make_state();
        state.is_host = false;
        state.input = "/kick bob".into();
        let msg = state.process_command();
        assert!(msg.is_none());
        assert!(state.events.back().unwrap().0.contains("host"));
    }

    #[test]
    fn command_kick_as_host() {
        let mut state = make_state();
        state.input = "/kick bob".into();
        let msg = state.process_command();
        assert_eq!(msg, Some(SignalMessage::Kick { target: 2 }));
    }

    #[test]
    fn command_block_returns_message() {
        let mut state = make_state();
        state.input = "/block bob".into();
        let msg = state.process_command();
        assert_eq!(msg, Some(SignalMessage::BlockPeer { target: 2 }));
    }

    #[test]
    fn command_unblock_returns_message() {
        let mut state = make_state();
        state.input = "/unblock bob".into();
        let msg = state.process_command();
        assert_eq!(msg, Some(SignalMessage::UnblockPeer { target: 2 }));
    }

    #[test]
    fn command_leave_sets_quit() {
        let mut state = make_state();
        state.input = "/leave".into();
        let msg = state.process_command();
        assert!(state.should_quit);
        assert_eq!(msg, Some(SignalMessage::Leave));
    }

    #[test]
    fn command_help_adds_events() {
        let mut state = make_state();
        state.input = "/help".into();
        let msg = state.process_command();
        assert!(msg.is_none());
        assert!(state.events.len() >= 5);
    }

    #[test]
    fn command_unknown_reports_error() {
        let mut state = make_state();
        state.input = "/bogus".into();
        let msg = state.process_command();
        assert!(msg.is_none());
        assert!(state.events.back().unwrap().0.contains("unknown command"));
    }

    #[test]
    fn command_forcemute_as_host() {
        let mut state = make_state();
        state.input = "/forcemute bob".into();
        let msg = state.process_command();
        assert_eq!(
            msg,
            Some(SignalMessage::ServerMute {
                target: 2,
                muted: true
            })
        );
    }

    #[test]
    fn command_forceunmute_as_host() {
        let mut state = make_state();
        state.input = "/forceunmute bob".into();
        let msg = state.process_command();
        assert_eq!(
            msg,
            Some(SignalMessage::ServerMute {
                target: 2,
                muted: false
            })
        );
    }

    #[test]
    fn command_empty_arg_shows_usage() {
        let mut state = make_state();
        state.input = "/kick".into();
        let msg = state.process_command();
        assert!(msg.is_none());
        assert!(state.events.back().unwrap().0.contains("usage"));
    }

    #[test]
    fn handle_signal_client_joined() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::ClientJoined {
            client_id: 3,
            display_name: "charlie".into(),
        });
        assert_eq!(state.peers.len(), 3);
        assert_eq!(state.peers[2].display_name, "charlie");
        assert!(state.events.back().unwrap().0.contains("charlie joined"));
    }

    #[test]
    fn handle_signal_client_left() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::ClientLeft { client_id: 2 });
        assert_eq!(state.peers.len(), 1);
        assert!(state.events.back().unwrap().0.contains("bob left"));
    }

    #[test]
    fn handle_signal_you_are_host() {
        let mut state = make_state();
        state.is_host = false;
        state.handle_signal(SignalMessage::YouAreHost);
        assert!(state.is_host);
        assert!(state.events.back().unwrap().0.contains("host"));
    }

    #[test]
    fn handle_signal_kicked_sets_quit() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::Kicked {
            reason: "testing".into(),
        });
        assert!(state.should_quit);
        assert!(state.events.back().unwrap().0.contains("kicked"));
    }

    #[test]
    fn handle_signal_self_mute() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::PeerMuted {
            client_id: 2,
            muted: true,
            by_server: false,
        });
        assert!(state.peers[1].self_muted);
        assert!(!state.peers[1].server_muted);
        assert!(state.events.back().unwrap().0.contains("bob muted"));
    }

    #[test]
    fn handle_signal_server_mute() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::PeerMuted {
            client_id: 2,
            muted: true,
            by_server: true,
        });
        assert!(!state.peers[1].self_muted);
        assert!(state.peers[1].server_muted);
        assert!(
            state
                .events
                .back()
                .unwrap()
                .0
                .contains("bob silenced by host")
        );
    }

    #[test]
    fn handle_signal_server_mute_updates_self() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::PeerMuted {
            client_id: 1,
            muted: true,
            by_server: true,
        });
        assert!(state.server_muted);
        assert!(!state.self_muted);
    }

    #[test]
    fn handle_signal_room_info_updates_peers() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::RoomInfo {
            clients: vec![
                ClientInfo {
                    client_id: 1,
                    display_name: "alice".into(),
                    muted: false,
                    server_muted: false,
                    is_host: false,
                },
                ClientInfo {
                    client_id: 5,
                    display_name: "eve".into(),
                    muted: true,
                    server_muted: true,
                    is_host: true,
                },
            ],
        });
        assert_eq!(state.peers.len(), 2);
        assert_eq!(state.peers[1].display_name, "eve");
        assert!(state.peers[1].is_host);
        assert!(state.peers[1].self_muted);
        assert!(state.peers[1].server_muted);
    }

    #[test]
    fn set_peers_from_info_updates_host_status() {
        let mut state = make_state();
        state.is_host = false;
        state.set_peers_from_info(&[ClientInfo {
            client_id: 1,
            display_name: "alice".into(),
            muted: false,
            server_muted: false,
            is_host: true,
        }]);
        assert!(state.is_host);
    }

    #[test]
    fn set_peers_from_info_updates_server_muted() {
        let mut state = make_state();
        state.set_peers_from_info(&[ClientInfo {
            client_id: 1,
            display_name: "alice".into(),
            muted: false,
            server_muted: true,
            is_host: false,
        }]);
        assert!(state.server_muted);
        assert!(!state.self_muted);
    }

    #[test]
    fn non_command_input_rejected() {
        let mut state = make_state();
        state.input = "hello world".into();
        let msg = state.process_command();
        assert!(msg.is_none());
        assert!(
            state
                .events
                .back()
                .unwrap()
                .0
                .contains("commands start with /")
        );
    }

    #[test]
    fn empty_input_returns_none() {
        let mut state = make_state();
        state.input = "".into();
        assert!(state.process_command().is_none());
        state.input = "   ".into();
        assert!(state.process_command().is_none());
    }

    #[test]
    fn events_capped_at_max() {
        let mut state = AppState::new(1, "room".into(), "a".into(), 50);
        for i in 0..150 {
            state.add_event(format!("event {i}"), Style::new());
        }
        assert_eq!(state.events.len(), 50);
        assert!(state.events.front().unwrap().0.contains("event 100"));
    }

    #[test]
    fn has_duplicate_name_detects_collisions() {
        let state = make_state();
        assert!(!state.has_duplicate_name("alice"));
        assert!(!state.has_duplicate_name("bob"));
    }

    #[test]
    fn has_duplicate_name_true_when_duplicated() {
        let mut state = make_state();
        state.peers.push(PeerEntry {
            client_id: 3,
            display_name: "bob".into(),
            self_muted: false,
            server_muted: false,
            is_host: false,
        });
        assert!(state.has_duplicate_name("bob"));
        assert!(!state.has_duplicate_name("alice"));
    }

    #[test]
    fn find_peer_ambiguous_name_returns_error() {
        let mut state = make_state();
        state.peers.push(PeerEntry {
            client_id: 3,
            display_name: "bob".into(),
            self_muted: false,
            server_muted: false,
            is_host: false,
        });
        let result = state.find_peer("bob");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn find_peer_by_id_still_works_with_duplicates() {
        let mut state = make_state();
        state.peers.push(PeerEntry {
            client_id: 3,
            display_name: "bob".into(),
            self_muted: false,
            server_muted: false,
            is_host: false,
        });
        assert_eq!(state.find_peer("2"), Ok(2));
        assert_eq!(state.find_peer("3"), Ok(3));
    }

    #[test]
    fn command_kick_ambiguous_name_shows_error() {
        let mut state = make_state();
        state.peers.push(PeerEntry {
            client_id: 3,
            display_name: "bob".into(),
            self_muted: false,
            server_muted: false,
            is_host: false,
        });
        state.input = "/kick bob".into();
        let msg = state.process_command();
        assert!(msg.is_none());
        assert!(state.events.back().unwrap().0.contains("ambiguous"));
    }

    #[test]
    fn handle_signal_error_shows_in_events() {
        let mut state = make_state();
        state.handle_signal(SignalMessage::Error {
            message: "only the host can kick".into(),
        });
        let last = &state.events.back().unwrap().0;
        assert!(last.contains("only the host can kick"));
    }
}
