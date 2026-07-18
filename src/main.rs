mod app;
mod backend;

use std::{error::Error, io, time::Duration};

use app::App;
use backend::{OperationalState, WifiSecurity, detect_wifi_backend, networkd::NetworkdDbus};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = init_terminal()?;
    let app = App::new();
    start_backends(&app).await;
    let result = run_app(&mut terminal, app).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn start_backends(app: &App) {
    let sender = app.backend_event_sender();

    match detect_wifi_backend().await {
        Ok(Some(backend)) => {
            backend.spawn_listener(sender.clone());
        }
        Ok(None) => {}
        Err(error) => {
            let _ = sender
                .send(app::BackendEvent::Error(error.to_string()))
                .await;
        }
    }

    match NetworkdDbus::connect_system().await {
        Ok(networkd) => {
            networkd.spawn_listener(sender);
        }
        Err(error) => {
            let _ = sender
                .send(app::BackendEvent::Error(error.to_string()))
                .await;
        }
    }
}

async fn run_app(terminal: &mut Tui, mut app: App) -> Result<(), Box<dyn Error>> {
    while !app.should_quit() {
        terminal.draw(|frame| render(frame, &mut app))?;

        tokio::select! {
            Some(event) = app.recv_backend_event() => app.apply_backend_event(event),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    handle_terminal_event(&mut app, event::read()?);
                }
            }
        }
    }

    Ok(())
}

fn handle_terminal_event(app: &mut App, event: Event) {
    if let Event::Key(key) = event
        && key.kind == KeyEventKind::Press
    {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => app.quit(),
            KeyCode::Down | KeyCode::Char('j') => app.next_wifi_network(),
            KeyCode::Up | KeyCode::Char('k') => app.previous_wifi_network(),
            KeyCode::Enter => app.connect_selected_wifi_network(),
            _ => {}
        }
    }
}

fn render(frame: &mut Frame, app: &mut App) {
    let [content_area, status_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let [wifi_area, interface_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(content_area);

    render_wifi_networks(frame, app, wifi_area);
    render_interfaces(frame, app, interface_area);

    let status_color = if app.status_is_error() {
        Color::Red
    } else {
        Color::Yellow
    };
    let status = Line::from(vec![
        Span::styled(
            format!(" {}", app.status_message()),
            Style::default().fg(status_color),
        ),
        Span::styled(
            "  |  q/Esc quit  Up/Down or j/k select  Enter connect",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(status), status_area);
}

fn render_wifi_networks(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let items = if app.wifi_networks().is_empty() {
        vec![ListItem::new("No Wi-Fi networks found").style(Style::default().fg(Color::DarkGray))]
    } else {
        app.wifi_networks()
            .iter()
            .map(|network| {
                let signal_color = match network.signal_strength {
                    70..=100 => Color::Green,
                    40..=69 => Color::Yellow,
                    _ => Color::Red,
                };
                let connected = network
                    .connected
                    .then(|| Span::styled(" connected", Style::default().fg(Color::Green)));
                let mut spans = vec![
                    Span::raw(network.ssid.clone()),
                    Span::styled(
                        format!("  {:>3}%", network.signal_strength),
                        Style::default().fg(signal_color),
                    ),
                    Span::styled(
                        format!("  {}", wifi_security_label(network.security)),
                        Style::default().fg(Color::Cyan),
                    ),
                ];
                if let Some(connected) = connected {
                    spans.push(connected);
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Wi-Fi Networks ",
                    Style::default().fg(Color::Cyan),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, area, app.wifi_list_state());
}

fn render_interfaces(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let lines = if app.interfaces().is_empty() {
        vec![Line::styled(
            "No interfaces reported by systemd-networkd",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        app.interfaces()
            .iter()
            .map(|interface| {
                let state_color = match interface.operational_state {
                    OperationalState::Routable => Color::Green,
                    OperationalState::Degraded => Color::Yellow,
                    OperationalState::Off | OperationalState::NoCarrier => Color::DarkGray,
                    OperationalState::Dormant | OperationalState::Unknown => Color::Cyan,
                };
                let is_active = interface.carrier
                    || matches!(interface.operational_state, OperationalState::Routable);
                let ip_addresses = if interface.ip_addresses.is_empty() {
                    "no address".into()
                } else {
                    interface
                        .ip_addresses
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let interface_style = if is_active {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!("{}  ", interface.name), interface_style),
                    Span::styled(
                        format!(
                            "{:<10}",
                            operational_state_label(interface.operational_state)
                        ),
                        interface_style.fg(state_color),
                    ),
                    Span::styled(
                        format!("  {ip_addresses}"),
                        interface_style.fg(Color::DarkGray),
                    ),
                ])
            })
            .collect()
    };
    let interfaces = Paragraph::new(lines).block(
        Block::default()
            .title(Span::styled(
                " Network Interfaces ",
                Style::default().fg(Color::Cyan),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(interfaces, area);
}

fn wifi_security_label(security: WifiSecurity) -> &'static str {
    match security {
        WifiSecurity::Open => "open",
        WifiSecurity::Wep => "WEP",
        WifiSecurity::Wpa => "WPA",
        WifiSecurity::Rsn => "WPA2/3",
        WifiSecurity::WpaPersonal => "WPA-Personal",
        WifiSecurity::WpaEnterprise => "WPA-Enterprise",
        WifiSecurity::Unknown => "unknown",
    }
}

fn operational_state_label(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Unknown => "unknown",
        OperationalState::Off => "off",
        OperationalState::NoCarrier => "no-carrier",
        OperationalState::Dormant => "dormant",
        OperationalState::Degraded => "degraded",
        OperationalState::Routable => "routable",
    }
}

fn init_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}
