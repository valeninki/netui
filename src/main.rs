mod app;
mod backend;

use std::{error::Error, io, net::IpAddr, time::Duration};

use app::{ActivePane, App, ConfigField, InputMode, SelectedNetworkAction};
use backend::{
    DnsOverTlsMode, OperationalState, WifiSecurity, detect_wifi_backend,
    disconnect_current_network as backend_disconnect_current_network, forget_network,
    initiate_connection,
    networkd::NetworkdDbus,
    resolved::{DnsPolicyUpdate, active_wifi_interface, set_link_dns_over_tls},
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use tokio::time::MissedTickBehavior;
use unicode_width::UnicodeWidthChar;

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
        } else if app.is_editing_config() {
            handle_config_edit_event(app, key.code);
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.quit(),
                KeyCode::Tab => app.toggle_active_pane(),
                KeyCode::Down | KeyCode::Char('j') => app.next_selection(),
                KeyCode::Up | KeyCode::Char('k') => app.previous_selection(),
                KeyCode::Enter if app.is_busy() => app.show_wait_message(),
                KeyCode::Enter => toggle_selected_item(app),
                KeyCode::Char('e' | 'E') if app.is_busy() => app.show_wait_message(),
                KeyCode::Char('e' | 'E') => begin_config_edit(app),
                KeyCode::Char('d' | 'D') if app.is_busy() => app.show_wait_message(),
                KeyCode::Char('d' | 'D') => forget_selected_network(app),
                KeyCode::Char('6') => app.toggle_show_ipv6(),
                _ => {}
            }
        }
    }
}

fn handle_config_edit_event(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => app.cancel_config_edit(),
        KeyCode::Tab | KeyCode::Down => app.next_config_field(),
        KeyCode::BackTab | KeyCode::Up => app.previous_config_field(),
        KeyCode::Left => match app.edit_field() {
            Some(ConfigField::Method) => app.toggle_edit_method(),
            Some(ConfigField::DnsOverTls) => app.cycle_edit_dot_mode(true),
            _ => {}
        },
        KeyCode::Right | KeyCode::Char(' ') => match app.edit_field() {
            Some(ConfigField::Method) => app.toggle_edit_method(),
            Some(ConfigField::DnsOverTls) => app.cycle_edit_dot_mode(false),
            _ => {}
        },
        KeyCode::Enter if app.is_busy() => app.show_wait_message(),
        KeyCode::Enter => {
            app.submit_config();
        }
        KeyCode::Backspace => app.delete_input_character(),
        KeyCode::Char(character) => app.push_input_character(character),
        _ => {}
    }
}

fn begin_config_edit(app: &mut App) {
    app.begin_config_edit();
}

fn toggle_selected_item(app: &mut App) {
    match app.active_pane() {
        ActivePane::Wifi => toggle_selected_network(app),
        ActivePane::Interfaces => app.show_wired_link_control_unavailable(),
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
    let config = app.wifi_config(&ssid);

    tokio::spawn(async move {
        let event = match initiate_connection(ssid.clone(), password).await {
            Ok(()) => {
                tokio::spawn(apply_dns_policy_for_connection(
                    sender.clone(),
                    ssid.clone(),
                    config,
                ));
                app::BackendEvent::ActionCompleted(Ok(format!("Connected to {ssid}")))
            }
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
        let restore_dns = ssid.eq_ignore_ascii_case("eduroam");
        let (disconnect_result, interface_result) =
            tokio::join!(backend_disconnect_current_network(), async {
                if restore_dns {
                    Some(active_wifi_interface().await)
                } else {
                    None
                }
            });
        let event = match disconnect_result {
            Ok(()) => {
                if let Some(interface_result) = interface_result {
                    match interface_result {
                        Ok(interface) => {
                            tokio::spawn(restore_dns_policy(sender.clone(), interface));
                        }
                        Err(error) => {
                            let _ = sender
                                .send(app::BackendEvent::Log(dns_error_message(error)))
                                .await;
                        }
                    }
                }
                app::BackendEvent::ActionCompleted(Ok(format!("Disconnected from {ssid}")))
            }
            Err(error) => app::BackendEvent::ActionCompleted(Err(error.to_string())),
        };
        let _ = sender.send(event).await;
    });
}

async fn apply_dns_policy_for_connection(
    sender: tokio::sync::mpsc::Sender<app::BackendEvent>,
    ssid: String,
    config: backend::WifiConfig,
) {
    let policy = config.dns_over_tls;
    let result = async {
        let interface = active_wifi_interface().await?;
        let update = set_link_dns_over_tls(&interface, policy).await?;
        Ok::<_, backend::BackendError>((interface, update))
    }
    .await;

    let message = match result {
        Ok((_, DnsPolicyUpdate::SkippedUnavailable)) => {
            "[DNS] systemd-resolved unavailable; skipped DoT policy".into()
        }
        Ok((_, DnsPolicyUpdate::Applied))
            if policy == DnsOverTlsMode::Off && ssid.eq_ignore_ascii_case("eduroam") =>
        {
            "[DNS] Disabled DoT for eduroam".into()
        }
        Ok((interface, DnsPolicyUpdate::Applied)) => format!(
            "[DNS] Applied {} DoT policy for {interface}",
            dot_mode_label(policy)
        ),
        Err(error) => dns_error_message(error),
    };
    let _ = sender.send(app::BackendEvent::Log(message)).await;
}

async fn restore_dns_policy(
    sender: tokio::sync::mpsc::Sender<app::BackendEvent>,
    interface: String,
) {
    let message = match set_link_dns_over_tls(&interface, DnsOverTlsMode::Default).await {
        Ok(DnsPolicyUpdate::Applied) => format!("[DNS] Restored DoT for {interface}"),
        Ok(DnsPolicyUpdate::SkippedUnavailable) => {
            "[DNS] systemd-resolved unavailable; skipped DoT restore".into()
        }
        Err(error) => dns_error_message(error),
    };
    let _ = sender.send(app::BackendEvent::Log(message)).await;
}

fn dns_error_message(error: backend::BackendError) -> String {
    if error.is_service_unavailable() {
        "[DNS] Service unavailable, skipped DoT toggle".into()
    } else {
        let _ = error;
        "[DNS] DoT policy could not be applied".into()
    }
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
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(frame.area());
    let [wifi_area, interface_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(content_area);

    render_wifi_networks(frame, app, wifi_area);
    render_interfaces(frame, app, interface_area);

    render_footer(frame, app, footer_area);

    match app.input_mode() {
        InputMode::Input { .. } => render_password_input(frame, app, content_area),
        InputMode::Edit { .. } => render_config_input(frame, app, content_area),
        InputMode::Normal => {}
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let footer = Block::default()
        .title(Span::styled(" Status ", Style::default().fg(Color::Cyan)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let footer_area = footer.inner(area);
    frame.render_widget(footer, area);

    let status_area = footer_area;
    let bindings = if matches!(app.input_mode(), InputMode::Input { .. }) {
        [("Enter", "connect"), ("Esc", "cancel")].as_slice()
    } else if app.is_editing_config() {
        [
            ("Up/Down/Tab", "field"),
            ("Left/Right/Space", "toggle"),
            ("Enter", "save"),
            ("Esc", "cancel"),
        ]
        .as_slice()
    } else {
        [
            ("q/Esc/Ctrl+C", "quit"),
            ("Tab", "pane"),
            ("Up/Down or j/k", "select"),
            ("Enter", "action"),
            ("e", "edit profile"),
            ("d", "forget"),
            ("6", "ipv6"),
        ]
        .as_slice()
    };
    let (key_hints, key_hint_width) = key_hint_line(bindings, status_area.width.saturating_sub(18));
    let [message_area, key_hints_area] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(key_hint_width.min(status_area.width.saturating_sub(1))),
    ])
    .areas(status_area);

    let status_color = if app.wifi_scanning() {
        Color::Cyan
    } else if app.status_is_error() {
        Color::Red
    } else {
        Color::Green
    };
    let status_message = if app.wifi_scanning() {
        "Scanning..."
    } else {
        app.status_message()
    };
    frame.render_widget(
        Paragraph::new(Line::from(truncate_line(
            status_message,
            message_area.width,
        )))
        .style(Style::default().fg(status_color)),
        message_area,
    );
    frame.render_widget(
        Paragraph::new(key_hints).alignment(Alignment::Right),
        key_hints_area,
    );
}

fn truncate_line(value: &str, width: u16) -> String {
    let mut output = String::new();
    let mut used_width: usize = 0;

    for character in sanitized_first_line(value).chars() {
        let character_width = character.width().unwrap_or(0);
        if used_width.saturating_add(character_width) > usize::from(width) {
            break;
        }
        output.push(character);
        used_width += character_width;
    }

    output
}

fn sanitized_first_line(value: &str) -> String {
    let mut output = String::new();
    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        if matches!(character, '\n' | '\r') {
            break;
        }
        if character == '\u{1b}' {
            match characters.peek() {
                Some('[') => {
                    characters.next();
                    while let Some(sequence_character) = characters.next() {
                        if ('@'..='~').contains(&sequence_character) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    characters.next();
                    while let Some(sequence_character) = characters.next() {
                        if sequence_character == '\u{7}'
                            || (sequence_character == '\u{1b}' && characters.next() == Some('\\'))
                        {
                            break;
                        }
                    }
                }
                _ => {
                    characters.next();
                }
            }
            continue;
        }
        if !character.is_control() {
            output.push(character);
        }
    }

    output
}

fn key_hint_line(bindings: &[(&str, &str)], max_width: u16) -> (Line<'static>, u16) {
    let mut spans = Vec::new();
    let mut used_width = 0;

    for (key, action) in bindings {
        let binding_width = key.len() + 1 + action.len();
        let separator_width = usize::from(!spans.is_empty()) * 3;
        if used_width + separator_width + binding_width > usize::from(max_width) {
            break;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            used_width += 3;
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {action}"),
            Style::default().fg(Color::DarkGray),
        ));
        used_width += binding_width;
    }

    (Line::from(spans), used_width as u16)
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

fn render_config_input(frame: &mut Frame, app: &App, content_area: Rect) {
    let Some(target) = app.edit_target() else {
        return;
    };
    let Some(active_field) = app.edit_field() else {
        return;
    };

    let modal_area = config_modal_area(content_area);
    let config = app.edit_config();
    let fields = [
        (ConfigField::Method, ipv4_method_label(config.ipv4_method)),
        (
            ConfigField::IpAddress,
            if config.ipv4_method == backend::Ipv4Method::Static {
                config.ip_address.as_str()
            } else {
                "DHCP (disabled)"
            },
        ),
        (
            ConfigField::Gateway,
            if config.ipv4_method == backend::Ipv4Method::Static {
                config.gateway.as_str()
            } else {
                "DHCP (disabled)"
            },
        ),
        (ConfigField::Dns, config.dns_servers.as_str()),
        (ConfigField::DnsOverTls, dot_mode_label(config.dns_over_tls)),
    ];
    let lines = fields.into_iter().map(|(field, value)| {
        let marker = if field == active_field { "> " } else { "  " };
        let disabled = config.ipv4_method == backend::Ipv4Method::Dhcp
            && matches!(field, ConfigField::IpAddress | ConfigField::Gateway);
        Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if field == active_field {
                    Color::Cyan
                } else if disabled {
                    Color::DarkGray
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(
                format!("{:<16}: ", field.label()),
                Style::default().fg(if disabled {
                    Color::DarkGray
                } else if field == active_field {
                    Color::Cyan
                } else {
                    Color::Reset
                }),
            ),
            Span::styled(
                value.to_owned(),
                Style::default().fg(if disabled {
                    Color::DarkGray
                } else {
                    Color::Reset
                }),
            ),
        ])
    });
    let modal = Paragraph::new(
        lines
            .chain(std::iter::once(Line::styled(
                "Tab/Up/Down navigate, Left/Right/Space toggle, Enter saves, Esc cancels",
                Style::default().fg(Color::DarkGray),
            )))
            .collect::<Vec<_>>(),
    )
    .block(
        Block::default()
            .title(Span::styled(
                format!(" Connection Profile: {} ", target.display_name()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(Clear, modal_area);
    frame.render_widget(modal, modal_area);

    let (row, value) = match active_field {
        ConfigField::Method => (0, ipv4_method_label(config.ipv4_method)),
        ConfigField::IpAddress => (1, config.ip_address.as_str()),
        ConfigField::Gateway => (2, config.gateway.as_str()),
        ConfigField::Dns => (3, config.dns_servers.as_str()),
        ConfigField::DnsOverTls => (4, dot_mode_label(config.dns_over_tls)),
    };
    let field_prefix_width = 2 + 16 + 2;
    let cursor_x = modal_area
        .x
        .saturating_add(1)
        .saturating_add(field_prefix_width)
        .saturating_add(value.chars().count() as u16)
        .min(
            modal_area
                .x
                .saturating_add(modal_area.width.saturating_sub(2)),
        );
    frame.set_cursor_position(Position::new(
        cursor_x,
        modal_area.y.saturating_add(1).saturating_add(row),
    ));
}

fn config_modal_area(area: Rect) -> Rect {
    let width = area.width.min(76);
    let height = 9.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height).saturating_sub(2));

    Rect::new(x, y, width, height)
}

fn ipv4_method_label(method: backend::Ipv4Method) -> &'static str {
    match method {
        backend::Ipv4Method::Dhcp => "DHCP",
        backend::Ipv4Method::Static => "Static",
    }
}

fn dot_mode_label(mode: DnsOverTlsMode) -> &'static str {
    match mode {
        DnsOverTlsMode::Default => "Default",
        DnsOverTlsMode::Off => "Off",
        DnsOverTlsMode::Strict => "Strict",
    }
}

fn render_wifi_networks(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let is_active = app.is_active_pane(ActivePane::Wifi);
    let mut items = app
        .interfaces()
        .iter()
        .filter(|interface| interface.is_wired)
        .map(|interface| {
            let carrier = if interface.carrier {
                ("Carrier: On", Color::Green)
            } else {
                ("Carrier: Off", Color::Red)
            };
            let mut spans = vec![
                Span::styled(format!("{:<8}", "[ETH]"), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{:<20}", truncate_column(&interface.name, 20)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<14}", carrier.0), Style::default().fg(carrier.1)),
            ];
            spans.extend(interface_address_spans(
                &interface.ip_addresses,
                app.show_ipv6(),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    items.extend(app.wifi_networks().iter().map(|network| {
        let signal_color = match network.signal_strength {
            70..=100 => Color::Green,
            40..=69 => Color::Yellow,
            _ => Color::Red,
        };
        let connected = network
            .connected
            .then(|| Span::styled(" connected", Style::default().fg(Color::Green)));
        let mut spans = vec![
            Span::styled(
                format!("{:<8}", "[Wi-Fi]"),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{:<20}", truncate_column(&network.ssid, 20)),
                Style::default().fg(if network.connected {
                    Color::Green
                } else {
                    Color::Reset
                }),
            ),
            Span::styled(
                format!("{:>4}%", network.signal_strength),
                Style::default().fg(signal_color),
            ),
            Span::styled(
                format!(" {:<9}", wifi_security_label(network.security)),
                Style::default().fg(Color::Cyan),
            ),
        ];
        if let Some(connected) = connected {
            spans.push(connected);
        }
        ListItem::new(Line::from(spans))
    }));
    if items.is_empty() {
        items.push(
            ListItem::new("No Ethernet or Wi-Fi connections found")
                .style(Style::default().fg(Color::DarkGray)),
        );
    }
    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Network Connections ",
                    Style::default().fg(Color::Cyan),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if is_active {
                    Color::White
                } else {
                    Color::DarkGray
                })),
        )
        .highlight_symbol(if is_active { "> " } else { "" })
        .highlight_style(if is_active {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        });

    frame.render_stateful_widget(list, area, app.wifi_list_state());
}

fn render_interfaces(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let is_active = app.is_active_pane(ActivePane::Interfaces);
    let items = if app.interfaces().is_empty() {
        vec![
            ListItem::new("No interfaces reported by systemd-networkd")
                .style(Style::default().fg(Color::DarkGray)),
        ]
    } else {
        app.interfaces()
            .iter()
            .map(|interface| {
                let loopback = interface.is_loopback || interface.name == "lo";
                let state_label = if loopback
                    && matches!(interface.operational_state, OperationalState::Unknown)
                {
                    "loopback"
                } else {
                    operational_state_label(interface.operational_state)
                };
                let state_color = if loopback && state_label == "loopback" {
                    Color::Cyan
                } else {
                    match interface.operational_state {
                        OperationalState::Routable => Color::Green,
                        OperationalState::Degraded
                        | OperationalState::Off
                        | OperationalState::NoCarrier => Color::Red,
                        OperationalState::Dormant => Color::Yellow,
                        OperationalState::Unknown => Color::Cyan,
                    }
                };
                let carrier_on = interface.carrier || loopback;
                let is_active =
                    carrier_on || matches!(interface.operational_state, OperationalState::Routable);
                let interface_style = if is_active {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let interface_kind = if loopback {
                    ("[LOOP]", Color::Cyan)
                } else if interface.is_wired {
                    ("[ETH]", Color::Cyan)
                } else if interface.is_wireless {
                    ("[Wi-Fi]", Color::Yellow)
                } else if is_vpn_interface(&interface.name) {
                    ("[VPN]", Color::Magenta)
                } else {
                    ("[Other]", Color::DarkGray)
                };
                let carrier = if carrier_on {
                    ("Carrier: On", Color::Green)
                } else {
                    ("Carrier: Off", Color::Red)
                };
                let mut spans = vec![
                    Span::styled(
                        format!("{:<9}", interface_kind.0),
                        Style::default().fg(interface_kind.1),
                    ),
                    Span::styled(
                        format!("{:<12}", truncate_column(&interface.name, 12)),
                        interface_style,
                    ),
                    Span::styled(
                        format!("{:<12}", truncate_column(state_label, 12)),
                        interface_style.fg(state_color),
                    ),
                    Span::styled(format!("{:<14}", carrier.0), interface_style.fg(carrier.1)),
                ];
                spans.extend(interface_address_spans(
                    &interface.ip_addresses,
                    app.show_ipv6(),
                ));
                ListItem::new(Line::from(spans))
            })
            .collect()
    };
    let interfaces = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Network Interfaces ",
                    Style::default().fg(Color::Cyan),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if is_active {
                    Color::White
                } else {
                    Color::DarkGray
                })),
        )
        .highlight_symbol(if is_active { "> " } else { "" })
        .highlight_style(if is_active {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        });
    frame.render_stateful_widget(interfaces, area, app.interface_list_state());
}

fn interface_address_spans(addresses: &[IpAddr], show_ipv6: bool) -> Vec<Span<'static>> {
    let visible_addresses = addresses
        .iter()
        .filter(|address| should_render_address(address, show_ipv6))
        .collect::<Vec<_>>();

    if visible_addresses.is_empty() {
        return vec![Span::styled(
            " no address",
            Style::default().fg(Color::DarkGray),
        )];
    }

    let mut spans = vec![Span::raw(" ")];
    for (index, address) in visible_addresses.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(", "));
        }
        let style = if address.is_ipv6() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Reset)
        };
        spans.push(Span::styled(address.to_string(), style));
    }

    spans
}

fn should_render_address(address: &IpAddr, show_ipv6: bool) -> bool {
    match address {
        IpAddr::V4(_) => true,
        IpAddr::V6(address) => show_ipv6 && !address.is_unicast_link_local(),
    }
}

fn is_vpn_interface(interface_name: &str) -> bool {
    let interface_name = interface_name.to_lowercase();
    ["awg", "wg", "openvpn", "ovpn", "tailscale", "vpn"]
        .iter()
        .any(|keyword| interface_name.contains(keyword))
}

fn truncate_column(value: &str, width: usize) -> String {
    let mut characters = value.chars();
    let truncated: String = characters.by_ref().take(width).collect();
    if characters.next().is_none() || width < 3 {
        return truncated;
    }

    format!(
        "{}...",
        truncated.chars().take(width - 3).collect::<String>()
    )
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

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{dns_error_message, should_render_address, truncate_line};
    use crate::backend::BackendError;

    #[test]
    fn truncates_and_sanitizes_footer_lines() {
        assert_eq!(truncate_line("first\nsecond", 20), "first");
        assert_eq!(truncate_line("first\rsecond", 20), "first");
        assert_eq!(truncate_line("a long status", 8), "a long s");
        assert_eq!(truncate_line("\x1b[31mred\x1b[0m", 20), "red");
        assert_eq!(truncate_line("wide 界界", 7), "wide 界");
        assert_eq!(truncate_line("status", 0), "");
    }

    #[test]
    fn unavailable_resolver_is_a_concise_warning() {
        assert_eq!(
            dns_error_message(BackendError::ServiceUnavailable {
                service: "org.freedesktop.resolve1".into()
            }),
            "[DNS] Service unavailable, skipped DoT toggle"
        );
    }

    #[test]
    fn hides_link_local_ipv6_and_respects_ipv6_visibility() {
        assert!(should_render_address(
            &IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            false
        ));
        assert!(!should_render_address(
            &"fe80::1".parse::<Ipv6Addr>().unwrap().into(),
            true
        ));
        assert!(!should_render_address(
            &"2001:db8::1".parse::<Ipv6Addr>().unwrap().into(),
            false
        ));
        assert!(should_render_address(
            &"2001:db8::1".parse::<Ipv6Addr>().unwrap().into(),
            true
        ));
    }
}
