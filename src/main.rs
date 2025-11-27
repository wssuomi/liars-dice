use rand::prelude::*;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::Rect,
    prelude::*,
    style::Style,
    widgets::{Block, Borders, Clear, Paragraph},
};
use russh::{
    Channel, ChannelId, Pty,
    keys::ssh_key::{self, PublicKey, rand_core::OsRng},
    server::*,
};
use std::{collections::HashMap, fmt::Display, path::Path, sync::Arc, usize};
use tokio::sync::{
    Mutex,
    mpsc::{UnboundedSender, unbounded_channel},
};

const TURN_TIMEOUT: isize = 60;

type SshTerminal = Terminal<CrosstermBackend<TerminalHandle>>;

const INSTRUCTIONS: &[&str] = &[
    "- instructions -",
    "increase amount: up, k",
    "increase face: right, l",
    "decrease amount: up, j",
    "decrease face: left, h",
    "place bid: enter",
    "call liar: space",
    "quit: q",
    "",
    "- valid bid -",
    "value is bigger than previously",
    "or",
    "face is bigger than previously",
];

struct App {
    rolls: Option<Vec<usize>>,
    names: Vec<String>,
    amount: usize,
    face: usize,
    liar: bool,
    bid: bool,
    can_play: bool,
}

impl App {
    pub fn new() -> App {
        Self {
            rolls: None,
            names: vec![],
            amount: 1,
            face: 1,
            liar: false,
            bid: false,
            can_play: true,
        }
    }
}

enum State {
    Waiting,
    Roll,
    Turn,
}

struct Bid {
    face: usize,
    amount: usize,
}

impl Display for Bid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} x {}", self.amount, self.face)
    }
}

struct ServerApp {
    timer: isize,
    turn: Option<usize>,
    state: State,
    bid: Bid,
    totals: HashMap<usize, usize>,
    prev_turn_id: usize,
}

impl ServerApp {
    pub fn new() -> ServerApp {
        Self {
            timer: 0,
            turn: None,
            state: State::Waiting,
            bid: Bid { amount: 0, face: 0 },
            totals: HashMap::from([(1, 0), (2, 0), (3, 0), (3, 0), (4, 0), (5, 0), (6, 0)]),
            prev_turn_id: 0,
        }
    }
}

struct TerminalHandle {
    sender: UnboundedSender<Vec<u8>>,
    // The sink collects the data which is finally sent to sender.
    sink: Vec<u8>,
}

impl TerminalHandle {
    async fn start(handle: Handle, channel_id: ChannelId) -> Self {
        let (sender, mut receiver) = unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                let result = handle.data(channel_id, data.into()).await;
                if result.is_err() {
                    eprintln!("Failed to send data: {:?}", result);
                }
            }
        });
        Self {
            sender,
            sink: Vec::new(),
        }
    }
}

// The crossterm backend writes to the terminal handle.
impl std::io::Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let result = self.sender.send(self.sink.clone());
        if result.is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                result.unwrap_err(),
            ));
        }

        self.sink.clear();
        Ok(())
    }
}

fn roll_dice() -> Vec<usize> {
    let mut rng = rand::rng();
    (0..6)
        .into_iter()
        .map(|_| rng.random_range(1..=6))
        .collect()
}

#[derive(Clone)]
struct AppServer {
    clients: Arc<Mutex<HashMap<usize, (SshTerminal, App)>>>,
    id: usize,
    app: Arc<Mutex<ServerApp>>,
}

impl AppServer {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            id: 0,
            app: Arc::new(Mutex::new(ServerApp::new())),
        }
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let clients = self.clients.clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                let mut ids: Vec<usize> = clients
                    .lock()
                    .await
                    .iter()
                    .map(|(e, _)| e.clone())
                    .collect();
                let mut al = app.lock().await;
                let mut cl = clients.lock().await;
                let mut to_remove: Option<usize> = None;
                match al.state {
                    State::Waiting => {
                        al.bid = Bid { face: 0, amount: 0 };
                        al.timer += 1;
                        al.timer %= 3;
                        if ids.len() >= 2 {
                            al.state = State::Roll;
                        }
                        for (id, (_, app)) in cl.iter_mut() {
                            let names: Vec<String> = ids
                                .iter()
                                .enumerate()
                                .map(|(i, e)| {
                                    let mut r = format!("player {}", i + 1);
                                    if id == e {
                                        r = format!("{r} (you)");
                                    }
                                    r
                                })
                                .collect();
                            app.amount = 1;
                            app.face = 1;
                            app.bid = false;
                            app.liar = false;
                            app.names = names;
                        }
                    }
                    State::Roll => {
                        al.timer = TURN_TIMEOUT;
                        if al.turn.is_none() {
                            al.turn = Some(0);
                        }
                        if ids.len() <= 1 {
                            al.state = State::Waiting;
                        }
                        al.totals =
                            HashMap::from([(1, 0), (2, 0), (3, 0), (3, 0), (4, 0), (5, 0), (6, 0)]);
                        for (_, (_, app)) in cl.iter_mut() {
                            app.rolls = Some(roll_dice());
                            for r in app.rolls.clone().unwrap().iter() {
                                let _ = *al.totals.entry(*r).and_modify(|e| *e += 1).or_insert(1);
                            }
                        }
                        al.state = State::Turn;
                    }
                    State::Turn => {
                        if ids.len() <= 1 {
                            al.state = State::Waiting;
                        }
                        al.timer -= 1;
                        if al.timer <= 0 {
                            al.timer = TURN_TIMEOUT;
                            al.prev_turn_id = ids[al.turn.unwrap()];
                            if al.turn.is_some() {
                                al.turn = Some(al.turn.unwrap() + 1);
                                if al.turn.unwrap() >= ids.len() {
                                    al.turn = Some(0);
                                }
                            } else {
                                al.turn = Some(0);
                            }
                            if al.bid.face == 0 || al.bid.amount == 0 {
                                al.bid = Bid { face: 1, amount: 1 }
                            }
                        }
                        let mut is_liar = false;
                        let mut submitted = false;
                        for (id, (_, app)) in cl.iter_mut() {
                            let names: Vec<String> = ids
                                .iter()
                                .enumerate()
                                .map(|(i, e)| {
                                    let mut r = format!("player {}", i + 1);
                                    if id == e {
                                        r = format!("{r} (you)");
                                    }
                                    if i == al.turn.unwrap_or(0) {
                                        r = format!("{r} ({})", al.timer);
                                    }
                                    r
                                })
                                .collect();
                            if ids.get(al.turn.unwrap()).is_some() {
                                if id == &ids[al.turn.unwrap()] {
                                    if app.bid {
                                        // TODO: give feedback if not valid bid
                                        if app.face > al.bid.face || app.amount > al.bid.amount {
                                            al.bid = Bid {
                                                face: app.face,
                                                amount: app.amount,
                                            };
                                            al.timer = 1;
                                            submitted = true;
                                        }
                                    }
                                    if app.liar {
                                        if al.bid.amount > al.totals[&al.bid.face] {
                                            is_liar = true;
                                            to_remove = Some(al.prev_turn_id);
                                        } else {
                                            app.can_play = false;
                                            to_remove = Some(*id);
                                        }
                                        al.state = State::Roll;
                                        al.timer = 1;
                                        submitted = true;
                                    }
                                    if !submitted && al.timer <= 1 {
                                        al.bid.amount += 1;
                                    }
                                }
                            }
                            app.names = names;
                            app.bid = false;
                            app.liar = false;
                        }
                        for (id, (_, app)) in cl.iter_mut() {
                            if id == &al.prev_turn_id {
                                if is_liar {
                                    app.can_play = false;
                                }
                            }
                        }
                        if to_remove.is_some() {
                            // cl.remove(&to_remove.unwrap());
                            if cl.len() < 2 {
                                al.state = State::Waiting;
                            }
                            ids.retain(|value| *value != to_remove.unwrap());
                        }
                    }
                }
                match al.state {
                    State::Waiting => {
                        for (_, (terminal, app)) in cl.iter_mut() {
                            terminal
                                .draw(|f| {
                                    let area = Rect::new(0, 0, f.area().width, 6);
                                    f.render_widget(Clear, area);
                                    let style = Style::default();
                                    let paragraph = Paragraph::new(app.names.join("\n"))
                                        .alignment(ratatui::layout::Alignment::Left)
                                        .style(style);
                                    let block =
                                        Block::default().title("[ Players ]").borders(Borders::ALL);
                                    f.render_widget(paragraph.block(block), area);

                                    let area = Rect::new(0, 6, f.area().width, f.area().height - 6);
                                    f.render_widget(Clear, area);
                                    let paragraph = Paragraph::new("test")
                                        .alignment(ratatui::layout::Alignment::Left)
                                        .style(style);
                                    let block = Block::default()
                                        .title("[ Liar's Dice ]")
                                        .borders(Borders::ALL);
                                    f.render_widget(paragraph.block(block), area);

                                    let l = Layout::default()
                                        .direction(Direction::Horizontal)
                                        .constraints(vec![
                                            Constraint::Percentage(67),
                                            Constraint::Percentage(33),
                                        ])
                                        .split(area.inner(Margin {
                                            horizontal: 1,
                                            vertical: 1,
                                        }));
                                    f.render_widget(
                                        Paragraph::new(format!(
                                            "Waiting for players{}",
                                            match al.timer {
                                                0 => {
                                                    ".  "
                                                }
                                                1 => {
                                                    ".. "
                                                }
                                                2 => {
                                                    "..."
                                                }
                                                _ => {
                                                    "   "
                                                }
                                            }
                                        ))
                                        .alignment(ratatui::layout::Alignment::Center)
                                        .block(
                                            Block::new().borders(Borders::ALL).title("[ Game ]"),
                                        ),
                                        l[0],
                                    );
                                    f.render_widget(
                                        Paragraph::new(format!(
                                            "{}",
                                            (INSTRUCTIONS.iter().map(|e| e.to_string()))
                                                .collect::<Vec<String>>()
                                                .join("\n"),
                                        ))
                                        .alignment(ratatui::layout::Alignment::Center)
                                        .block(
                                            Block::new().borders(Borders::ALL).title("[ Game ]"),
                                        ),
                                        l[1],
                                    );
                                    f.render_widget(
                                        Block::new().borders(Borders::ALL).title("[ Feed ]"),
                                        l[1],
                                    );
                                })
                                .unwrap();
                        }
                    }
                    State::Roll => {}
                    State::Turn => {
                        for (_, (terminal, app)) in cl.iter_mut() {
                            terminal
                                .draw(|f| {
                                    let area = Rect::new(0, 0, f.area().width, 6);
                                    f.render_widget(Clear, area);
                                    let style = Style::default();
                                    let paragraph = Paragraph::new(app.names.join("\n"))
                                        .alignment(ratatui::layout::Alignment::Left)
                                        .style(style);
                                    let block =
                                        Block::default().title("[ Players ]").borders(Borders::ALL);
                                    f.render_widget(paragraph.block(block), area);

                                    let area = Rect::new(0, 6, f.area().width, f.area().height - 6);
                                    f.render_widget(Clear, area);
                                    let paragraph = Paragraph::new("test")
                                        .alignment(ratatui::layout::Alignment::Left)
                                        .style(style);
                                    let block = Block::default()
                                        .title("[ Liar's Dice ]")
                                        .borders(Borders::ALL);
                                    f.render_widget(paragraph.block(block), area);

                                    let l = Layout::default()
                                        .direction(Direction::Horizontal)
                                        .constraints(vec![
                                            Constraint::Percentage(67),
                                            Constraint::Percentage(33),
                                        ])
                                        .split(area.inner(Margin {
                                            horizontal: 1,
                                            vertical: 1,
                                        }));
                                    f.render_widget(
                                        Paragraph::new(format!(
                                            "Your rolls: {}\nPrevious bid {}\n\n\nAmount: {}\n\nFace: {}",
                                            (app.rolls
                                                .clone()
                                                .unwrap_or(vec![])
                                                .iter()
                                                .map(|e| e.to_string()))
                                            .collect::<Vec<String>>()
                                            .join(" "),
                                            al.bid,
                                            app.amount,
                                            app.face,
                                        ))
                                        .alignment(ratatui::layout::Alignment::Center)
                                        .block(
                                            Block::new().borders(Borders::ALL).title("[ Game ]"),
                                        ),
                                        l[0],
                                    );
                                    f.render_widget(
                                        Paragraph::new(format!(
                                            "{}",
                                            (INSTRUCTIONS
                                                .iter()
                                                .map(|e| e.to_string()))
                                            .collect::<Vec<String>>()
                                            .join("\n"),
                                        ))
                                        .alignment(ratatui::layout::Alignment::Center)
                                        .block(
                                            Block::new().borders(Borders::ALL).title("[ Game ]"),
                                        ),
                                        l[1],
                                    );
                                    f.render_widget(
                                        Block::new().borders(Borders::ALL).title("[ Feed ]"),
                                        l[1],
                                    );
                                })
                                .unwrap();
                        }
                    }
                }
            }
        });
        russh::keys::PrivateKey::random(&mut OsRng, ssh_key::Algorithm::Ed25519).unwrap();
        if let Err(_) = russh::keys::PrivateKey::read_openssh_file(Path::new("./key")) {
            russh::keys::PrivateKey::random(&mut OsRng, ssh_key::Algorithm::Ed25519)
                .unwrap()
                .write_openssh_file(Path::new("./key"), ssh_key::LineEnding::LF)?;
        }
        let config = Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            auth_rejection_time: std::time::Duration::from_secs(3),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![russh::keys::PrivateKey::read_openssh_file(Path::new(
                "./key",
            ))?],
            nodelay: true,
            ..Default::default()
        };

        self.run_on_address(Arc::new(config), ("0.0.0.0", 2222))
            .await?;
        Ok(())
    }
}

impl Server for AppServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        let s = self.clone();
        self.id += 1;
        s
    }
}

impl Handler for AppServer {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let terminal_handle = TerminalHandle::start(session.handle(), channel.id()).await;

        let backend = CrosstermBackend::new(terminal_handle);

        // the correct viewport area will be set when the client request a pty
        let options = TerminalOptions {
            viewport: Viewport::Fixed(Rect::default()),
        };

        let terminal = Terminal::with_options(backend, options)?;
        let app = App::new();

        let mut clients = self.clients.lock().await;
        clients.insert(self.id, (terminal, app));

        Ok(true)
    }

    async fn auth_publickey(&mut self, _: &str, _: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let mut clients = self.clients.lock().await;
        match data {
            b"q" => {
                clients.remove(&self.id);
                session.close(channel)?;
            }
            [27, 91, 65] | b"k" => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                app.amount += 1;
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
            [27, 91, 66] | b"j" => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                if app.amount > 1 {
                    app.amount -= 1;
                }
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
            [27, 91, 68] | b"h" => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                if app.face > 1 {
                    app.face -= 1;
                }
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
            [27, 91, 67] | b"l" => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                if app.face < 6 {
                    app.face += 1;
                }
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
            // 32: Space
            [32] => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                app.liar = true;
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
            // 13: Enter
            [13] => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                app.bid = true;
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
            _ => {
                let (_, app) = clients.get_mut(&self.id).unwrap();
                if !app.can_play {
                    clients.remove(&self.id);
                    session.close(channel)?;
                }
            }
        }

        Ok(())
    }

    /// The client's window size has changed.
    async fn window_change_request(
        &mut self,
        _: ChannelId,
        col_width: u32,
        row_height: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        let rect = Rect {
            x: 0,
            y: 0,
            width: col_width as u16,
            height: row_height as u16,
        };

        let mut clients = self.clients.lock().await;
        let (terminal, _) = clients.get_mut(&self.id).unwrap();
        terminal.resize(rect)?;

        Ok(())
    }

    /// The client requests a pseudo-terminal with the given
    /// specifications.
    ///
    /// **Note:** Success or failure should be communicated to the client by calling
    /// `session.channel_success(channel)` or `session.channel_failure(channel)` respectively.
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        col_width: u32,
        row_height: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let rect = Rect {
            x: 0,
            y: 0,
            width: col_width as u16,
            height: row_height as u16,
        };

        let mut clients = self.clients.lock().await;
        let (terminal, _) = clients.get_mut(&self.id).unwrap();
        terminal.resize(rect)?;

        session.channel_success(channel)?;

        Ok(())
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let id = self.id;
        let clients = self.clients.clone();
        tokio::spawn(async move {
            let mut clients = clients.lock().await;
            clients.remove(&id);
        });
    }
}

#[tokio::main]
async fn main() {
    let mut server = AppServer::new();
    server.run().await.expect("Failed running server");
}
