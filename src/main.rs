mod app;
mod backend;

use std::{error::Error, io, time::Duration};

use app::{App, InputMode, SelectedNetworkAction};
use backend::{
    OperationalState, WifiSecurity, detect_wifi_backend,
    disconnect_current_network as backend_disconnect_current_network, forget_network,
    initiate_connection, networkd::NetworkdDbus,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use tokio::time::MissedTickBehavior;

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = init_terminal()?;
    let app = App::new();
    start_backends(&app).await;
    let result = run_app(&mut terminal, app).await;
    let restore_result = restore_terminal(&mut terminal);

    result?;
    restore_result?;
    Ok(())
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
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    while !app.should_quit() {
        tokio::select! {
            Some(event) = app.recv_backend_event() => app.apply_backend_event(event),
            _ = tick.tick() => {
                while event::poll(Duration::ZERO)? {
                    handle_terminal_event(&mut app, event::read()?);
                }
                terminal.draw(|frame| render(frame, &mut app))?;
            }
        }
    }

    Ok(())
}

fn handle_terminal_event(app: &mut App, event: Event) {
    if let Event::Key(key) = event
        && key.kind == KeyEventKind::Press
    {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            app.quit();
            return;
        }

        if matches!(app.input_mode(), InputMode::Input { .. }) {
            match key.code {
                KeyCode::Esc => app.cancel_password_input(),
                KeyCode::Enter if app.is_busy() => app.show_wait_message(),
                KeyCode::Enter => submit_connection(app),
                KeyCode::Backspace => app.delete_input_character(),
                KeyCode::Char(character) => app.push_input_character(character),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.quit(),
                KeyCode::Down | KeyCode::Char('j') => app.next_wifi_network(),
                KeyCode::Up | KeyCode::Char('k') => app.previous_wifi_network(),
                KeyCode::Enter if app.is_busy() => app.show_wait_message(),
                KeyCode::Enter => toggle_selected_network(app),
                KeyCode::Char('d' | 'D') if app.is_busy() => app.show_wait_message(),
                KeyCode::Char('d' | 'D') => forget_selected_network(app),
                _ => {}
            }
        }
    }
}

fn toggle_selected_network(app: &mut App) {
    match app.toggle_selected_network() {
        Some(SelectedNetworkAction::Connect { ssid, password }) => {
            start_connection(app, ssid, password);
        }
        Some(SelectedNetworkAction::Disconnect { ssid }) => start_disconnection(app, ssid),
        None => {}
    }
}

fn submit_connection(app: &mut App) {
    let Some((ssid, password)) = app.submit_password() else {
        return;
    };

    start_connection(app, ssid, password);
}

fn start_connection(app: &mut App, ssid: String, password: String) {
    if !app.begin_backend_action() {
        return;
    }
    let sender = app.backend_event_sender();

    tokio::spawn(async move {
        let event = match initiate_connection(ssid.clone(), password).await {
            Ok(()) => app::BackendEvent::ActionCompleted(Ok(format!("Connected to {ssid}"))),
            Err(error) => app::BackendEvent::ActionCompleted(Err(error.to_string())),
        };
        let _ = sender.send(event).await;
    });
}

fn start_disconnection(app: &mut App, ssid: String) {
    if !app.begin_backend_action() {
        return;
    }
    let sender = app.backend_event_sender();

    tokio::spawn(async move {
        let event = match backend_disconnect_current_network().await {
            Ok(()) => app::BackendEvent::ActionCompleted(Ok(format!("Disconnected from {ssid}"))),
            Err(error) => app::BackendEvent::ActionCompleted(Err(error.to_string())),
        };
        let _ = sender.send(event).await;
    });
}

fn forget_selected_network(app: &mut App) {
    let Some(ssid) = app.forget_selected_network() else {
        return;
    };
    if !app.begin_backend_action() {
        return;
    }
    let sender = app.backend_event_sender();

    tokio::spawn(async move {
        let event = match forget_network(ssid.clone()).await {
            Ok(()) => app::BackendEvent::ActionCompleted(Ok(format!("Network {ssid} forgotten"))),
            Err(error) => app::BackendEvent::ActionCompleted(Err(error.to_string())),
        };
        let _ = sender.send(event).await;
    });
}

fn render(frame: &mut Frame, app: &mut App) {
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(8)]).areas(frame.area());
    let [wifi_area, interface_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(content_area);

    render_wifi_networks(frame, app, wifi_area);
    render_interfaces(frame, app, interface_area);

    let status_color = if app.wifi_scanning() {
        Color::Cyan
    } else if app.status_is_error() {
        Color::Red
    } else {
        Color::Yellow
    };
    let status_message = if app.wifi_scanning() {
        "Scanning..."
    } else {
        app.status_message()
    };
    let key_hints = if matches!(app.input_mode(), InputMode::Input { .. }) {
        "Enter connect  Esc cancel"
    } else {
        "q/Esc/Ctrl+C quit  Up/Down or j/k select  Enter connect/disconnect  d forget"
    };
    let status = Line::from(vec![
        Span::styled(
            format!("{status_message}  |  "),
            Style::default().fg(status_color),
        ),
        Span::styled(key_hints, Style::default().fg(Color::DarkGray)),
    ]);
    let mut footer_items = vec![ListItem::new(status)];
    footer_items.extend(app.logs().iter().map(|message| {
        let color = if message.starts_with("Error:") {
            Color::Red
        } else {
            Color::DarkGray
        };
        ListItem::new(Line::styled(message.clone(), Style::default().fg(color)))
    }));
    let footer = List::new(footer_items).block(
        Block::default()
            .title(Span::styled(" Footer ", Style::default().fg(Color::Cyan)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(footer, footer_area);

    if matches!(app.input_mode(), InputMode::Input { .. }) {
        render_password_input(frame, app, content_area);
    }
}

fn render_password_input(frame: &mut Frame, app: &App, content_area: Rect) {
    let InputMode::Input { ssid } = app.input_mode() else {
        return;
    };

    let modal_area = password_modal_area(content_area);
    let masked_password = "*".repeat(app.input_buffer().chars().count());
    let input_line = format!("Password: {masked_password}");
    let modal = Paragraph::new(vec![
        Line::from(input_line.clone()),
        Line::styled(
            "Enter to connect, Esc to cancel",
            Style::default().fg(Color::DarkGray),
        ),
    ])
    .block(
        Block::default()
            .title(Span::styled(
                format!(" Password for {ssid} "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(Clear, modal_area);
    frame.render_widget(modal, modal_area);

    let cursor_x = modal_area
        .x
        .saturating_add(1)
        .saturating_add(input_line.len() as u16)
        .min(
            modal_area
                .x
                .saturating_add(modal_area.width.saturating_sub(2)),
        );
    frame.set_cursor_position(Position::new(cursor_x, modal_area.y.saturating_add(1)));
}

fn password_modal_area(area: Rect) -> Rect {
    let width = area.width.min(60);
    let height = 4.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height).saturating_sub(2));

    Rect::new(x, y, width, height)
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
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error);
    }

    Terminal::new(CrosstermBackend::new(stdout)).inspect_err(|_| {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    })
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    let raw_mode_result = disable_raw_mode();
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();

    raw_mode_result?;
    screen_result?;
    cursor_result
}
