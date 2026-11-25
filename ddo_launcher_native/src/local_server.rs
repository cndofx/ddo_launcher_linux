// Local Play: runs the Arrowgene.DragonsDogmaOnline server natively next to the game.
//
// The steps follow the Local Play mode of DDON-Launcher-WPF (github.com/D00MK1D/DDON-Launcher-WPF, 97fcd38,
// MainWindow.xaml.cs and LocalServerReleaseInstaller.cs): the LocalServer/Server folder next to ddo.exe, the
// server-release.json manifest, the files kept across server updates, the db.sqlite backup before "dbmigration",
// the readiness checks and "e" on stdin to stop the server. Both launchers can share one db.sqlite.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, Command};

use crate::server::Server;

const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/sebastian-heinz/Arrowgene.DragonsDogmaOnline/releases/latest";
const LINUX_ASSET_PATTERN: &str = "linux-x64";
const MANIFEST_FILE_NAME: &str = "server-release.json";
const SERVER_LOG_FILE_NAME: &str = "server.log";
const SERVER_DLL: &str = "Arrowgene.Ddon.Cli.dll";
const MIGRATION_FAILED_TEXT: &str = "Failed to migrate the database.";
const MIGRATION_TIMEOUT: Duration = Duration::from_secs(90);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DATABASE_PATH: &str = "Files/Database/db.sqlite";
const BACKUPS_PATH: &str = "Files/Database/Backups";
// Paths inside the server folder that survive a server update.
const PRESERVED_PATHS: [&str; 2] = [DATABASE_PATH, BACKUPS_PATH];
// The server writes its own folder into the config as absolute paths on first start (DatabaseSetting.cs,
// WebServerSetting.cs), so a config from another platform's server, such as win-x64 under Proton, points to
// Z:\ paths. Such a config is kept under OTHER_PLATFORM_CONFIG_PATH and the server writes a new one.
const CONFIG_PATH: &str = "Files/Arrowgene.Ddon.config.json";
const OTHER_PLATFORM_CONFIG_PATH: &str = "Files/Arrowgene.Ddon.config.other-platform.json";

pub const ACCOUNT_NAME: &str = "LocalPlayer";
pub const ACCOUNT_PASSWORD: &str = "LocalPlayer";
pub const ACCOUNT_EMAIL: &str = "localplayer@localhost.local";

pub fn server() -> Server {
    Server {
        name: "Local Play".into(),
        lobby_ip: "127.0.0.1".into(),
        lobby_port: 52100,
        dl_ip: "127.0.0.1".into(),
        dl_port: 52099,
    }
}

#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(serde::Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}

// Same file and field names as the WPF launcher's LocalServerInstallManifest.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstallManifest {
    release_tag: String,
    release_url: String,
    asset_name: String,
    asset_digest: Option<String>,
    download_url: String,
    asset_size: u64,
}

/// A server process started by this launcher.
pub struct LocalServer {
    process: Child,
    stdin: ChildStdin,
    log_path: PathBuf,
}

pub fn root_dir(exe_path: &Path) -> anyhow::Result<PathBuf> {
    let Some(game_dir) = exe_path.parent() else {
        anyhow::bail!("DDO.exe Path has no parent folder: {}", exe_path.display());
    };
    Ok(game_dir.join("LocalServer"))
}

/// True when a server, started by this or another launcher, already listens on one of the local ports.
pub async fn is_any_port_alive() -> bool {
    let server = server();
    is_dl_server_alive(&server).await
        || is_tcp_port_alive(&server.lobby_ip, server.lobby_port).await
}

/// Installs the latest linux-x64 server release, or updates an installed one, and returns the server folder.
pub async fn ensure_installed(root: &Path) -> anyhow::Result<PathBuf> {
    let server_dir = root.join("Server");
    let installed = read_manifest(root)?.filter(|manifest| {
        server_dir.join(SERVER_DLL).is_file() && manifest.asset_name.contains(LINUX_ASSET_PATTERN)
    });
    let keep_config = installed.is_some();

    match (installed, fetch_latest_release().await) {
        (Some(manifest), Ok((release, asset))) if is_same_release(&manifest, &release, &asset) => {}
        (Some(_), Err(e)) => {
            println!("could not check the latest server release, using the installed one: {e:#}")
        }
        (_, Ok((release, asset))) => install_release(root, &release, &asset, keep_config).await?,
        (None, Err(e)) => anyhow::bail!(
            "The local server is not installed, and the latest release could not be read from GitHub: {e:#}"
        ),
    }

    Ok(server_dir)
}

/// Backs up db.sqlite, then runs the server's database migration.
pub async fn migrate_database(server_dir: &Path) -> anyhow::Result<()> {
    backup_database(server_dir)?;

    let migration = server_command(server_dir, &["dbmigration"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(MIGRATION_TIMEOUT, migration).await {
        Ok(output) => output
            .map_err(|e| anyhow::anyhow!("Could not run dotnet for the database update: {e}"))?,
        Err(_) => anyhow::bail!("The local database update did not finish in time."),
    };

    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || log.contains(MIGRATION_FAILED_TEXT) {
        anyhow::bail!("The local database update failed.\n{}", last_lines(&log));
    }
    Ok(())
}

impl LocalServer {
    pub fn start(root: &Path, server_dir: &Path) -> anyhow::Result<Self> {
        let log_path = root.join(SERVER_LOG_FILE_NAME);
        let log = std::fs::File::create(&log_path)?;

        let mut process = server_command(server_dir, &["server", "start"])
            .stdin(Stdio::piped())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Could not run dotnet for the local server: {e}"))?;
        let stdin = process.stdin.take().expect("stdin is piped");

        Ok(Self {
            process,
            stdin,
            log_path,
        })
    }

    /// Waits for the download server, which also serves the account API used to log in.
    pub async fn wait_for_dl_server(&mut self) -> anyhow::Result<()> {
        self.wait_until(Readiness::DlServer).await
    }

    /// Waits for the lobby port the game connects to.
    pub async fn wait_for_lobby(&mut self) -> anyhow::Result<()> {
        self.wait_until(Readiness::Lobby).await
    }

    async fn wait_until(&mut self, readiness: Readiness) -> anyhow::Result<()> {
        let what = readiness.description();
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.process.try_wait()? {
                anyhow::bail!(
                    "The local server exited ({status}) before the {what} finished starting.\n{}",
                    self.log_tail()
                );
            }
            if readiness.is_ready().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        anyhow::bail!(
            "The {what} did not finish starting within {} seconds.\n{}",
            STARTUP_TIMEOUT.as_secs(),
            self.log_tail()
        )
    }

    /// Asks the server to exit with "e", and kills it when it does not exit in time.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        if self.process.try_wait()?.is_some() {
            return Ok(());
        }
        if let Err(e) = self.stdin.write_all(b"e\n").await {
            println!("could not send the exit command to the local server: {e}");
        }
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.process.wait())
            .await
            .is_err()
        {
            println!("local server did not exit in time, killing it");
            self.process.kill().await?;
        }
        Ok(())
    }

    fn log_tail(&self) -> String {
        match std::fs::read_to_string(&self.log_path) {
            Ok(log) => format!(
                "{}\nFull log: {}",
                last_lines(&log),
                self.log_path.display()
            ),
            Err(e) => format!(
                "Server log {} could not be read: {e}",
                self.log_path.display()
            ),
        }
    }
}

enum Readiness {
    DlServer,
    Lobby,
}

impl Readiness {
    fn description(&self) -> &'static str {
        match self {
            Readiness::DlServer => "download server",
            Readiness::Lobby => "game server",
        }
    }

    async fn is_ready(&self) -> bool {
        let server = server();
        match self {
            Readiness::DlServer => is_dl_server_alive(&server).await,
            Readiness::Lobby => is_tcp_port_alive(&server.lobby_ip, server.lobby_port).await,
        }
    }
}

// The linux-x64 release is framework-dependent, and its apphost does not start (Arrowgene.Ddon.Cli.csproj
// references Microsoft.NETCore.App 2.2.8), so the server runs through the system dotnet.
fn server_command(server_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("dotnet");
    command
        .current_dir(server_dir)
        .arg(server_dir.join(SERVER_DLL))
        .args(args);
    command
}

async fn is_dl_server_alive(server: &Server) -> bool {
    let url = format!("http://{}:{}", server.dl_ip, server.dl_port);
    let client = reqwest::Client::new();
    client.get(url).timeout(PROBE_TIMEOUT).send().await.is_ok()
}

async fn is_tcp_port_alive(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

fn read_manifest(root: &Path) -> anyhow::Result<Option<InstallManifest>> {
    let path = root.join(MANIFEST_FILE_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let manifest = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} could not be read: {e}", path.display()))?;
    Ok(Some(manifest))
}

async fn fetch_latest_release() -> anyhow::Result<(GitHubRelease, GitHubAsset)> {
    let mut release: GitHubRelease = github_client()?
        .get(LATEST_RELEASE_API_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let Some(index) = release.assets.iter().position(|asset| {
        asset.name.contains(LINUX_ASSET_PATTERN) && asset.name.ends_with(".tar.gz")
    }) else {
        anyhow::bail!(
            "Release {} has no {LINUX_ASSET_PATTERN} server archive.",
            release.tag_name
        );
    };
    let asset = release.assets.swap_remove(index);
    Ok((release, asset))
}

fn is_same_release(
    manifest: &InstallManifest,
    release: &GitHubRelease,
    asset: &GitHubAsset,
) -> bool {
    manifest.release_tag == release.tag_name
        && manifest.asset_name == asset.name
        && (asset.digest.is_none() || manifest.asset_digest == asset.digest)
}

fn github_client() -> reqwest::Result<reqwest::Client> {
    // GitHub's API refuses requests without a User-Agent.
    reqwest::Client::builder()
        .user_agent("ddo_launcher_native")
        .build()
}

async fn install_release(
    root: &Path,
    release: &GitHubRelease,
    asset: &GitHubAsset,
    keep_config: bool,
) -> anyhow::Result<()> {
    println!(
        "installing local server {} ({})",
        release.tag_name, asset.name
    );
    let downloads_dir = root.join("Downloads");
    std::fs::create_dir_all(&downloads_dir)?;
    let archive_path = downloads_dir.join(&asset.name);

    let archive = github_client()?
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    std::fs::write(&archive_path, &archive)?;
    if let Some(digest) = &asset.digest {
        verify_sha256(&archive_path, digest).await?;
    }

    let extract_dir = root.join(format!(".install-{}", unix_time()));
    std::fs::create_dir_all(&extract_dir)?;
    // GNU tar detects the compression. Release ".tar.gz" assets can be bzip2 or plain tar.
    run_tool(
        Command::new("tar")
            .arg("-xf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&extract_dir),
    )
    .await?;
    let Some(new_server_dir) = find_server_dir(&extract_dir)? else {
        anyhow::bail!("{} has no folder with {SERVER_DLL} and Files.", asset.name);
    };

    replace_server_dir(root, &new_server_dir, keep_config)?;
    std::fs::remove_dir_all(&extract_dir)?;
    std::fs::remove_file(&archive_path)?;

    let manifest = InstallManifest {
        release_tag: release.tag_name.clone(),
        release_url: release.html_url.clone(),
        asset_name: asset.name.clone(),
        asset_digest: asset.digest.clone(),
        download_url: asset.browser_download_url.clone(),
        asset_size: asset.size,
    };
    std::fs::write(
        root.join(MANIFEST_FILE_NAME),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(())
}

async fn verify_sha256(path: &Path, digest: &str) -> anyhow::Result<()> {
    let Some(expected) = digest.strip_prefix("sha256:") else {
        anyhow::bail!("Unsupported release digest: {digest}");
    };
    let output = run_tool(Command::new("sha256sum").arg(path)).await?;
    let actual = output.split_whitespace().next().unwrap_or_default();
    if !actual.eq_ignore_ascii_case(expected) {
        anyhow::bail!("{} does not match GitHub's SHA-256 digest.", path.display());
    }
    Ok(())
}

async fn run_tool(command: &mut Command) -> anyhow::Result<String> {
    let output = command.output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "{:?} failed: {}",
            command.as_std(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn find_server_dir(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    if dir.join(SERVER_DLL).is_file() && dir.join("Files").is_dir() {
        return Ok(Some(dir.to_path_buf()));
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if let Some(found) = find_server_dir(&path)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// Moves the new server into LocalServer/Server and carries the database, backups and config over.
/// keep_config is false when the previous server came from another platform.
fn replace_server_dir(root: &Path, new_server_dir: &Path, keep_config: bool) -> anyhow::Result<()> {
    let server_dir = root.join("Server");
    let previous_dir = root.join(format!("Server.previous-{}", unix_time()));
    let has_previous = server_dir.exists();

    if has_previous {
        std::fs::rename(&server_dir, &previous_dir)?;
    }
    std::fs::rename(new_server_dir, &server_dir)?;

    if has_previous {
        for relative in PRESERVED_PATHS {
            copy_path(&previous_dir.join(relative), &server_dir.join(relative))?;
        }
        let config_destination = if keep_config {
            CONFIG_PATH
        } else {
            OTHER_PLATFORM_CONFIG_PATH
        };
        copy_path(
            &previous_dir.join(CONFIG_PATH),
            &server_dir.join(config_destination),
        )?;
        std::fs::remove_dir_all(&previous_dir)?;
    }
    Ok(())
}

fn copy_path(source: &Path, destination: &Path) -> anyhow::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if source.is_file() {
        std::fs::create_dir_all(
            destination
                .parent()
                .expect("destination is inside the server folder"),
        )?;
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

fn backup_database(server_dir: &Path) -> anyhow::Result<()> {
    let database = server_dir.join(DATABASE_PATH);
    if !database.is_file() {
        return Ok(());
    }
    let backups_dir = server_dir.join(BACKUPS_PATH);
    std::fs::create_dir_all(&backups_dir)?;
    std::fs::copy(
        &database,
        backups_dir.join(format!("db-{}.sqlite", unix_time())),
    )?;
    Ok(())
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after 1970")
        .as_secs()
}

fn last_lines(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|line| !line.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}
