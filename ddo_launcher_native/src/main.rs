use std::path::Path;
use std::time::Duration;

use directories::ProjectDirs;
use eframe::egui;
use tokio::{
    process::Command,
    sync::{mpsc, oneshot},
};

use crate::local_server::LocalServer;
use crate::server::Server;

mod local_server;
mod server;

const CONFIG_FILE_NAME: &str = "config.json";

enum TokioRequest {
    BeginRequestRepaintLoop(egui::Context),
    Login(LoginRequest),
    Register(RegisterRequest),
    LocalPlay(LocalPlayRequest),
    Shutdown,
}

enum TokioResponse {
    Status(String),
    ChildProcessActive(bool),
}

struct LoginRequest {
    account: Account,
    exe_path: String,
    prefix_path: String,
}

struct RegisterRequest {
    account: Account,
}

struct LocalPlayRequest {
    exe_path: String,
    prefix_path: String,
}

struct Account {
    name: String,
    password: String,
    email: String,
    server: Server,
}

#[derive(Clone, Copy)]
enum DLServerAction {
    Login,
    Register,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct DLServerRequestData<'a> {
    action: &'a str,
    account: &'a str,
    password: &'a str,
    email: &'a str,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DLServerResponse {
    error: Option<String>,
    message: Option<String>,
    token: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let Some(dirs) = ProjectDirs::from("", "cndofx", "ddo_launcher_native") else {
        eprintln!("Error: Unable to determine project storage directories");
        return Ok(());
    };

    let config_path = {
        let mut config = dirs.config_dir().to_path_buf();
        config.push(CONFIG_FILE_NAME);
        config
    };
    dbg!(config_path.display());

    let config = match config_path.try_exists() {
        Err(e) => {
            eprintln!("Error: Unable to check if config file exists: {e}");
            return Ok(());
        }
        Ok(true) if !config_path.is_file() => {
            eprintln!(
                "Error: Config file path exists but it is not a file ({})",
                config_path.display()
            );
            return Ok(());
        }
        Ok(true) => {
            let config_str = std::fs::read_to_string(&config_path).unwrap();
            let config: AppConfig = serde_json::from_str(&config_str).unwrap();
            println!("loaded config file");
            config
        }
        Ok(false) => {
            println!("creating new default config");
            AppConfig::default()
        }
    };

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 {
        eprintln!("usage: ddo_launcher_native [server name | --local-play]");
        return Ok(());
    } else if args.len() == 2 && args[1] == "--local-play" {
        run_local_play(config)?;
    } else if args.len() == 2 {
        run_simple(&args[1], config)?;
    } else {
        run_gui(config, dirs)?;
    }

    Ok(())
}

fn run_simple(server_name: &str, config: AppConfig) -> anyhow::Result<()> {
    let Some(server) = config.servers.iter().find(|s| &s.name == server_name) else {
        anyhow::bail!(
            "error: No server named \"{}\" found in config file",
            server_name
        )
    };

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on((async || -> anyhow::Result<()> {
            let account = Account {
                name: config.account.clone(),
                password: config.password.clone(),
                email: String::new(),
                server: server.clone(),
            };

            let response = dl_server_action(DLServerAction::Login, &account).await?;

            if let Some(error) = &response.error {
                anyhow::bail!("DL Server error: {error}");
            }

            if let Some(token) = &response.token {
                spawn_umu(
                    LoginRequest {
                        account: account,
                        exe_path: config.exe_path.clone(),
                        prefix_path: config.prefix_path.clone(),
                    },
                    token.clone(),
                )
                .await;
            } else {
                anyhow::bail!(
                    "DL Server response did not include a token, message: \"{}\"",
                    response.message.unwrap_or_default(),
                );
            }

            Ok(())
        })())
}

fn run_local_play(config: AppConfig) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(local_play(
            LocalPlayRequest {
                exe_path: config.exe_path,
                prefix_path: config.prefix_path,
            },
            None,
        ))
}

fn run_gui(config: AppConfig, dirs: ProjectDirs) -> anyhow::Result<()> {
    let (main_tx, tokio_rx) = mpsc::channel::<TokioRequest>(8);
    let (tokio_tx, main_rx) = mpsc::channel::<TokioResponse>(8);

    let tokio_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(tokio_message_loop(tokio_tx, tokio_rx));
        println!("tokio thread exited");
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([320.0, 300.0]),
        ..Default::default()
    };
    let eframe_result = eframe::run_native(
        "DDO Launcher",
        options,
        Box::new(|_cc| Ok(Box::new(App::new(config, main_tx.clone(), main_rx, dirs)))),
    );

    main_tx.blocking_send(TokioRequest::Shutdown).unwrap();
    tokio_thread.join().unwrap();

    match eframe_result {
        Ok(_) => {}
        Err(e) => anyhow::bail!("{e}"),
    }

    Ok(())
}

async fn tokio_message_loop(tx: mpsc::Sender<TokioResponse>, mut rx: mpsc::Receiver<TokioRequest>) {
    let mut cancel_repaint_tx: Option<oneshot::Sender<()>> = None;
    let mut repaint_task_handle: Option<tokio::task::JoinHandle<()>> = None;

    while let Some(request) = rx.recv().await {
        println!("tokio thread got request");
        match request {
            TokioRequest::BeginRequestRepaintLoop(ctx) => {
                if let Some(old_tx) = cancel_repaint_tx.take() {
                    old_tx.send(()).unwrap();
                }
                let (new_cancel_repaint_tx, mut cancel_repaint_rx) = oneshot::channel::<()>();
                cancel_repaint_tx = Some(new_cancel_repaint_tx);

                let handle = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                                ctx.request_repaint();
                            }
                            _ = &mut cancel_repaint_rx => {
                                println!("breaking request repaint loop");
                                break;
                            }
                        }
                    }
                    println!("repaint loop exited");
                });
                repaint_task_handle = Some(handle);
            }
            TokioRequest::Login(login) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    println!(
                        "tokio thread got login request (account {}, password {})",
                        login.account.name, login.account.password
                    );

                    let response = dl_server_action(DLServerAction::Login, &login.account)
                        .await
                        .unwrap();

                    if let Some(error) = &response.error {
                        tx.send(TokioResponse::Status(error.clone())).await.unwrap();
                    } else {
                        let message = response.message.as_ref().unwrap();
                        tx.send(TokioResponse::Status(message.clone()))
                            .await
                            .unwrap();
                        if message == "Login Success" {
                            tx.send(TokioResponse::ChildProcessActive(true))
                                .await
                                .unwrap();
                            spawn_umu(login, response.token.unwrap()).await;
                            tx.send(TokioResponse::ChildProcessActive(false))
                                .await
                                .unwrap();
                        }
                    }
                });
            }
            TokioRequest::Register(register) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    println!(
                        "tokio thread got register request (account {}, password {})",
                        register.account.name, register.account.password
                    );

                    let response = dl_server_action(DLServerAction::Register, &register.account)
                        .await
                        .unwrap();

                    if let Some(error) = &response.error {
                        tx.send(TokioResponse::Status(error.clone())).await.unwrap();
                    } else {
                        tx.send(TokioResponse::Status(
                            response.message.as_ref().unwrap().clone(),
                        ))
                        .await
                        .unwrap();
                    }
                });
            }
            TokioRequest::LocalPlay(request) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    tx.send(TokioResponse::ChildProcessActive(true))
                        .await
                        .unwrap();
                    if let Err(e) = local_play(request, Some(&tx)).await {
                        eprintln!("local play failed: {e:#}");
                        tx.send(TokioResponse::Status(format!("{e:#}")))
                            .await
                            .unwrap();
                    }
                    tx.send(TokioResponse::ChildProcessActive(false))
                        .await
                        .unwrap();
                });
            }
            TokioRequest::Shutdown => {
                println!("tokio thread received shutdown request");
                break;
            }
        }
    }
    println!("tokio thread exited message loop");
    if let Some(cancel_repaint_tx) = cancel_repaint_tx {
        println!("cancelling request repaint loop");
        cancel_repaint_tx.send(()).unwrap();
    }
    if let Some(repaint_task_handle) = repaint_task_handle {
        repaint_task_handle.await.unwrap();
    }
}

async fn dl_server_action(
    action: DLServerAction,
    account: &Account,
) -> anyhow::Result<DLServerResponse> {
    let url = format!(
        "http://{}:{}/api/account",
        account.server.dl_ip, account.server.dl_port
    );
    dbg!(&url);

    let request_data = DLServerRequestData {
        action: match action {
            DLServerAction::Login => "login",
            DLServerAction::Register => "create",
        },
        account: &account.name,
        password: &account.password,
        email: &account.email,
    };
    dbg!(&request_data);

    let client = reqwest::Client::new();
    let response = client.post(&url).json(&request_data).send().await?;
    let response: DLServerResponse = response.json().await?;
    dbg!(&response);

    Ok(response)
}

async fn report_status(tx: Option<&mpsc::Sender<TokioResponse>>, text: &str) {
    println!("{text}");
    if let Some(tx) = tx {
        tx.send(TokioResponse::Status(text.into())).await.unwrap();
    }
}

/// Starts the local server unless one is already running, logs in with the local account and runs the game.
/// A server started here is stopped when the game exits or a step fails.
async fn local_play(
    request: LocalPlayRequest,
    tx: Option<&mpsc::Sender<TokioResponse>>,
) -> anyhow::Result<()> {
    let root = local_server::root_dir(Path::new(&request.exe_path))?;

    let mut owned_server = None;
    if local_server::is_any_port_alive().await {
        report_status(tx, "Using the local server that is already running").await;
    } else {
        report_status(tx, "Checking local server files...").await;
        let server_dir = local_server::ensure_installed(&root).await?;
        report_status(tx, "Applying local server database updates...").await;
        local_server::migrate_database(&server_dir).await?;
        report_status(tx, "Starting the local server...").await;
        owned_server = Some(LocalServer::start(&root, &server_dir)?);
    }

    let result = play_on_local_server(request, owned_server.as_mut(), tx).await;
    if let Some(server) = owned_server {
        server.stop().await?;
    }
    result
}

async fn play_on_local_server(
    request: LocalPlayRequest,
    mut owned_server: Option<&mut LocalServer>,
    tx: Option<&mpsc::Sender<TokioResponse>>,
) -> anyhow::Result<()> {
    if let Some(server) = owned_server.as_deref_mut() {
        report_status(tx, "Waiting for the local server to finish starting...").await;
        server.wait_for_dl_server().await?;
    }

    report_status(tx, "Signing in to the local server...").await;
    let account = Account {
        name: local_server::ACCOUNT_NAME.into(),
        password: local_server::ACCOUNT_PASSWORD.into(),
        email: local_server::ACCOUNT_EMAIL.into(),
        server: local_server::server(),
    };
    let mut response = dl_server_action(DLServerAction::Login, &account).await?;
    if response.token.is_none() {
        dl_server_action(DLServerAction::Register, &account).await?;
        response = dl_server_action(DLServerAction::Login, &account).await?;
    }
    let Some(token) = response.token else {
        anyhow::bail!(
            "Local login failed: {}",
            response.error.or(response.message).unwrap_or_default()
        );
    };

    if let Some(server) = owned_server {
        report_status(tx, "Waiting for the game server to finish starting...").await;
        server.wait_for_lobby().await?;
    }

    report_status(tx, "Starting Dragon's Dogma Online...").await;
    spawn_umu(
        LoginRequest {
            account,
            exe_path: request.exe_path,
            prefix_path: request.prefix_path,
        },
        token,
    )
    .await;
    report_status(tx, "Game exited").await;
    Ok(())
}

async fn spawn_umu(login: LoginRequest, token: String) {
    // DDO.exe opens nativePC and its other data relative to the working directory.
    let game_dir = Path::new(&login.exe_path)
        .parent()
        .unwrap_or(Path::new("."));
    let mut child = Command::new("umu-run")
        .current_dir(game_dir)
        .env("WINEPREFIX", &login.prefix_path)
        .arg(&login.exe_path)
        .arg(format!("addr={}", login.account.server.lobby_ip))
        .arg(format!("port={}", login.account.server.lobby_port))
        .arg(format!("token={}", token))
        .arg(format!(
            "DL=http://{}:{}/win/",
            login.account.server.dl_ip, login.account.server.dl_port
        ))
        .arg("LVer=03.04.003.20181115.0")
        .arg("RVer=3040008")
        .spawn()
        .unwrap();

    child.wait().await.unwrap();
}

struct App {
    config: AppConfig,
    status_text: String,
    child_process_active: bool,
    repaint_loop_started: bool,
    tx: mpsc::Sender<TokioRequest>,
    rx: mpsc::Receiver<TokioResponse>,
    dirs: ProjectDirs,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AppConfig {
    account: String,
    password: String,
    remember_me: bool,
    servers: Vec<Server>,
    selected_server: usize,

    exe_path: String,
    prefix_path: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            account: String::new(),
            password: String::new(),
            remember_me: false,
            servers: vec![Server {
                name: "DDOn".into(),
                lobby_ip: "ddon.org".into(),
                lobby_port: 52100,
                dl_ip: "ddon.org".into(),
                dl_port: 52099,
            }],
            selected_server: 0,
            exe_path: String::new(),
            prefix_path: String::new(),
        }
    }
}

impl App {
    fn new(
        config: AppConfig,
        tx: mpsc::Sender<TokioRequest>,
        rx: mpsc::Receiver<TokioResponse>,
        dirs: ProjectDirs,
    ) -> Self {
        Self {
            config: config,
            status_text: String::new(),
            child_process_active: false,
            repaint_loop_started: false,
            tx: tx,
            rx: rx,
            dirs: dirs,
        }
    }

    fn login(&mut self) {
        if !self.verify_account_details() {
            return;
        }

        self.tx
            .blocking_send(TokioRequest::Login(LoginRequest {
                account: Account {
                    name: self.config.account.clone(),
                    password: self.config.password.clone(),
                    email: String::new(),
                    server: self.config.servers[self.config.selected_server].clone(),
                },
                exe_path: self.config.exe_path.clone(),
                prefix_path: self.config.prefix_path.clone(),
            }))
            .unwrap();
    }

    fn register(&mut self) {
        if !self.verify_account_details() {
            return;
        }

        self.tx
            .blocking_send(TokioRequest::Register(RegisterRequest {
                account: Account {
                    name: self.config.account.clone(),
                    password: self.config.password.clone(),
                    email: String::new(),
                    server: self.config.servers[self.config.selected_server].clone(),
                },
            }))
            .unwrap();
    }

    fn local_play(&mut self) {
        if self.config.exe_path.is_empty() {
            self.status_text = "Error: \"DDO.exe Path\" must not be empty".into();
            return;
        }

        self.tx
            .blocking_send(TokioRequest::LocalPlay(LocalPlayRequest {
                exe_path: self.config.exe_path.clone(),
                prefix_path: self.config.prefix_path.clone(),
            }))
            .unwrap();
    }

    fn verify_account_details(&mut self) -> bool {
        if self.config.account.is_empty() {
            self.status_text = "Error: \"Account\" must not be empty".into();
            return false;
        }

        if self.config.password.is_empty() {
            self.status_text = "Error: \"Password\" must not be empty".into();
            return false;
        }

        if self.config.selected_server >= self.config.servers.len() {
            self.status_text = "Error: No server selected".into();
            return false;
        }

        true
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.repaint_loop_started {
            self.tx
                .blocking_send(TokioRequest::BeginRequestRepaintLoop(ctx.clone()))
                .unwrap();
            self.repaint_loop_started = true;
        }

        while !self.rx.is_empty() {
            println!("main thread got response");
            match self.rx.blocking_recv().unwrap() {
                TokioResponse::Status(s) => self.status_text = s,
                TokioResponse::ChildProcessActive(v) => self.child_process_active = v,
            }
        }

        egui::TopBottomPanel::bottom("bottom_status_panel").show(ctx, |ui| {
            ui.label(&self.status_text);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(!self.child_process_active, |ui| {
                ui.label("Account");
                ui.text_edit_singleline(&mut self.config.account);
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.config.password).password(true));
                ui.horizontal(|ui| {
                    if ui.button("Log In").clicked() {
                        println!("log in!");
                        self.login();
                    }
                    if ui.button("Register").clicked() {
                        println!("register!");
                        self.register();
                    }
                    ui.checkbox(&mut self.config.remember_me, "Remember Me");
                });
                if ui.button("Local Play").clicked() {
                    println!("local play!");
                    self.local_play();
                }

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    ui.label("Server");
                    egui::ComboBox::from_id_salt("select_server_combobox")
                        .selected_text(
                            self.config
                                .servers
                                .get(self.config.selected_server)
                                .map_or("<none>", |s| &s.name),
                        )
                        .show_ui(ui, |ui| {
                            for (i, server) in self.config.servers.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.config.selected_server,
                                    i,
                                    &server.name,
                                );
                            }
                        });
                    if ui.button("Edit").clicked() {
                        println!("edit servers!");
                    }
                });

                ui.add_space(20.0);

                ui.label("DDO.exe Path");
                ui.text_edit_singleline(&mut self.config.exe_path);
                ui.label("Wine Prefix Path");
                ui.text_edit_singleline(&mut self.config.prefix_path);
            });
        });
    }

    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        let mut config = self.config.clone();
        if !config.remember_me {
            config.account = String::new();
            config.password = String::new();
        }

        let config_path = {
            let mut path = self.dirs.config_dir().to_path_buf();
            path.push(CONFIG_FILE_NAME);
            path
        };

        let config_str = serde_json::to_string_pretty(&config).unwrap();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, config_str).unwrap();
    }
}
