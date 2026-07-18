mod app;
mod backend;

use std::{error::Error, io, time::Duration};

use app::App;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    widgets::{Block, Borders, Paragraph},
};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = init_terminal()?;
    let result = run_app(&mut terminal, App::new()).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn run_app(terminal: &mut Tui, mut app: App) -> Result<(), Box<dyn Error>> {
    while !app.should_quit() {
        terminal.draw(|frame| {
            let area = frame.area();
            let title = " netui ";
            let content = format!(
                "Wi-Fi networks: {}\nNetwork interfaces: {}\n\nPress q to quit.",
                app.wifi_networks().len(),
                app.interfaces().len()
            );
            frame.render_widget(
                Paragraph::new(content).block(Block::default().title(title).borders(Borders::ALL)),
                area,
            );
        })?;

        tokio::select! {
            Some(event) = app.recv_backend_event() => app.apply_backend_event(event),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if event::poll(Duration::ZERO)? {
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
        && key.code == KeyCode::Char('q')
    {
        app.quit();
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
