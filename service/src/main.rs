use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::Duration,
};

use clap::{Args, Parser, Subcommand};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use snafu::{OptionExt, ResultExt, Snafu, ensure};
use tokio::{
    fs,
    io::{AsyncReadExt, copy_bidirectional},
    net::{TcpStream, UnixListener, UnixStream},
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_RUNTIME_DIR: &str = "/var/run/microvm-builder";
const DAEMON_SOCKET_NAME: &str = "daemon.sock";
const VFKIT_SOCKET_NAME: &str = "vfkit.sock";

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("failed to inspect {name} process"))]
    InspectProcess {
        name: &'static str,
        source: io::Error,
    },

    #[snafu(display(
        "failed to create runtime directory {}",
        path.display()
    ))]
    CreateRuntimeDirectory { path: PathBuf, source: io::Error },

    #[snafu(display(
        "failed to remove {}",
        path.display()
    ))]
    RemoveFile { path: PathBuf, source: io::Error },

    #[snafu(display(
        "failed to bind daemon socket {}",
        path.display()
    ))]
    BindDaemonSocket { path: PathBuf, source: io::Error },

    #[snafu(display("failed to accept daemon connection"))]
    AcceptConnection { source: io::Error },

    #[snafu(display("failed to install {signal} handler"))]
    InstallSignalHandler {
        signal: &'static str,
        source: io::Error,
    },

    #[snafu(display(
        "failed to connect to daemon socket {}",
        path.display()
    ))]
    ConnectDaemon { path: PathBuf, source: io::Error },

    #[snafu(display("failed to connect to builder SSH port {port}"))]
    ConnectBuilder { port: u16, source: io::Error },

    #[snafu(display("failed to proxy SSH transport"))]
    ProxyTransport { source: io::Error },

    #[snafu(display(
        "failed to start {name} at {}",
        path.display()
    ))]
    SpawnProcess {
        name: &'static str,
        path: PathBuf,
        source: io::Error,
    },

    #[snafu(display("{name} has no process ID"))]
    MissingProcessId { name: &'static str },

    #[snafu(display("failed to send {signal:?} to {name} process group"))]
    SignalProcessGroup {
        name: &'static str,
        signal: Signal,
        source: Errno,
    },

    #[snafu(display("failed to reap {name}"))]
    ReapProcess {
        name: &'static str,
        source: io::Error,
    },

    #[snafu(display(
        "{name} exited before creating {}: {status}",
        path.display()
    ))]
    ProcessExitedBeforePath {
        name: &'static str,
        path: PathBuf,
        status: ExitStatus,
    },

    #[snafu(display(
        "timed out waiting for {name} to create {}",
        path.display()
    ))]
    PathTimeout { name: &'static str, path: PathBuf },

    #[snafu(display("{name} exited before builder became ready: {status}"))]
    ProcessExitedBeforeBuilder {
        name: &'static str,
        status: ExitStatus,
    },

    #[snafu(display("timed out waiting for builder SSH port {port}"))]
    BuilderTimeout { port: u16 },

    #[snafu(display(
        "{name} does not exist at {}",
        path.display()
    ))]
    ReadMetadata {
        name: &'static str,
        path: PathBuf,
        source: io::Error,
    },

    #[snafu(display(
        "{name} is not a regular file: {}",
        path.display()
    ))]
    NotAFile { name: &'static str, path: PathBuf },
}

#[derive(Debug, Parser)]
#[command(name = "microvm-builder")]
#[command(about = "On-demand MicroVM Nix builder manager")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the persistent manager daemon.
    Daemon(DaemonArgs),

    /// Proxy stdin/stdout to the builder SSH server through the daemon.
    Connect(ClientArgs),
}

#[derive(Debug, Clone, Args)]
struct DaemonArgs {
    /// Directory containing daemon runtime state.
    #[arg(long, default_value = DEFAULT_RUNTIME_DIR)]
    runtime_dir: PathBuf,

    /// Path to gvproxy.
    #[arg(long)]
    gvproxy: PathBuf,

    /// Path to vfkit.
    ///
    /// The microvm.nix runner currently launches vfkit itself. This is
    /// accepted now so direct vfkit management can be added later without
    /// changing the CLI.
    #[arg(long)]
    vfkit: PathBuf,

    /// Path to the microvm.nix declared runner.
    #[arg(long)]
    runner: PathBuf,

    /// Host TCP port forwarded by gvproxy to guest SSH.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,

    /// Stop the VM this many seconds after the last connection closes.
    #[arg(long, default_value_t = 120)]
    idle_timeout: u64,

    /// Maximum number of seconds allowed for VM startup.
    #[arg(long, default_value_t = 120)]
    start_timeout: u64,

    /// Grace period between SIGTERM and SIGKILL.
    #[arg(long, default_value_t = 10)]
    stop_timeout: u64,
}

#[derive(Debug, Clone, Args)]
struct ClientArgs {
    /// Directory containing daemon runtime state.
    #[arg(long, default_value = DEFAULT_RUNTIME_DIR)]
    runtime_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct Config {
    runtime_dir: PathBuf,
    daemon_socket: PathBuf,
    vfkit_socket: PathBuf,

    gvproxy: PathBuf,
    vfkit: PathBuf,
    runner: PathBuf,

    ssh_port: u16,

    idle_timeout: Duration,
    start_timeout: Duration,
    stop_timeout: Duration,
}

impl TryFrom<DaemonArgs> for Config {
    type Error = Error;

    fn try_from(args: DaemonArgs) -> Result<Self> {
        validate_file(&args.gvproxy, "gvproxy")?;
        validate_file(&args.vfkit, "vfkit")?;
        validate_file(&args.runner, "runner")?;

        Ok(Self {
            daemon_socket: args.runtime_dir.join(DAEMON_SOCKET_NAME),
            vfkit_socket: args.runtime_dir.join(VFKIT_SOCKET_NAME),
            runtime_dir: args.runtime_dir,

            gvproxy: args.gvproxy,
            vfkit: args.vfkit,
            runner: args.runner,

            ssh_port: args.ssh_port,

            idle_timeout: Duration::from_secs(args.idle_timeout),
            start_timeout: Duration::from_secs(args.start_timeout),
            stop_timeout: Duration::from_secs(args.stop_timeout),
        })
    }
}

struct VmProcesses {
    gvproxy: ManagedChild,
    runner: ManagedChild,
}

impl VmProcesses {
    fn is_running(&mut self) -> Result<bool> {
        Ok(self.gvproxy.is_running()? && self.runner.is_running()?)
    }
}

struct ManagedChild {
    name: &'static str,
    child: Child,
    process_group: Pid,
}

impl ManagedChild {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .context(InspectProcessSnafu { name: self.name })
    }

    fn is_running(&mut self) -> Result<bool> {
        Ok(self.try_wait()?.is_none())
    }
}

#[derive(Default)]
struct State {
    vm: Option<VmProcesses>,
    active_connections: usize,

    /// Incrementing this invalidates previously scheduled idle shutdowns.
    idle_generation: u64,
}

struct App {
    config: Config,
    state: Mutex<State>,

    /// Serializes VM startup and shutdown.
    lifecycle: Mutex<()>,
}

impl App {
    fn new(config: Config) -> Self {
        Self {
            config,
            state: Mutex::new(State::default()),
            lifecycle: Mutex::new(()),
        }
    }

    async fn ensure_vm_running(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;

        let stale_vm = {
            let mut state = self.state.lock().await;

            if let Some(vm) = state.vm.as_mut() {
                if vm.is_running()? {
                    return Ok(());
                }

                warn!("builder process exited unexpectedly");
            }

            state.vm.take()
        };

        if let Some(stale_vm) = stale_vm
            && let Err(error) = self.terminate_vm(stale_vm).await
        {
            warn!(%error, "failed to clean up stale VM processes");
        }

        let processes = self.start_vm_processes().await?;

        self.state.lock().await.vm = Some(processes);

        info!("builder VM is ready");

        Ok(())
    }

    async fn start_vm_processes(&self) -> Result<VmProcesses> {
        fs::create_dir_all(&self.config.runtime_dir).await.context(
            CreateRuntimeDirectorySnafu {
                path: self.config.runtime_dir.clone(),
            },
        )?;

        remove_file_if_present(&self.config.vfkit_socket).await?;

        debug!(
            vfkit = %self.config.vfkit.display(),
            "configured vfkit executable"
        );

        info!(
            gvproxy = %self.config.gvproxy.display(),
            ssh_port = self.config.ssh_port,
            "starting gvproxy"
        );

        let gvproxy_args = [
            OsString::from("--ssh-port"),
            OsString::from(self.config.ssh_port.to_string()),
            OsString::from("--listen-vfkit"),
            OsString::from(format!("unixgram://{}", self.config.vfkit_socket.display())),
        ];

        let mut gvproxy = spawn_process_group("gvproxy", &self.config.gvproxy, &gvproxy_args)?;

        if let Err(error) = wait_for_path(
            &self.config.vfkit_socket,
            &mut gvproxy,
            self.config.start_timeout,
        )
        .await
        {
            let _ = terminate_process_group(&mut gvproxy, self.config.stop_timeout).await;

            return Err(error);
        }

        info!(
            runner = %self.config.runner.display(),
            "starting MicroVM runner"
        );

        let mut runner = match spawn_process_group("runner", &self.config.runner, &[]) {
            Ok(runner) => runner,

            Err(error) => {
                let _ = terminate_process_group(&mut gvproxy, self.config.stop_timeout).await;

                return Err(error);
            }
        };

        if let Err(error) = wait_for_builder(
            self.config.ssh_port,
            &mut gvproxy,
            &mut runner,
            self.config.start_timeout,
        )
        .await
        {
            let _ = terminate_process_group(&mut runner, self.config.stop_timeout).await;
            let _ = terminate_process_group(&mut gvproxy, self.config.stop_timeout).await;

            return Err(error);
        }

        Ok(VmProcesses { gvproxy, runner })
    }

    async fn stop_vm(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;

        let vm = {
            let mut state = self.state.lock().await;

            state.idle_generation = state.idle_generation.wrapping_add(1);
            state.vm.take()
        };

        if let Some(vm) = vm {
            info!("stopping builder VM");
            self.terminate_vm(vm).await?;
        }

        Ok(())
    }

    async fn terminate_vm(&self, mut vm: VmProcesses) -> Result<()> {
        let runner_result = terminate_process_group(&mut vm.runner, self.config.stop_timeout).await;

        let gvproxy_result =
            terminate_process_group(&mut vm.gvproxy, self.config.stop_timeout).await;

        let _ = remove_file_if_present(&self.config.vfkit_socket).await;

        runner_result?;
        gvproxy_result?;

        info!("builder VM stopped");

        Ok(())
    }

    async fn connection_opened(&self) {
        let mut state = self.state.lock().await;

        state.active_connections += 1;
        state.idle_generation = state.idle_generation.wrapping_add(1);

        debug!(
            active_connections = state.active_connections,
            "builder connection opened"
        );
    }

    async fn connection_closed(self: &Arc<Self>) {
        let generation = {
            let mut state = self.state.lock().await;

            state.active_connections = state.active_connections.saturating_sub(1);

            debug!(
                active_connections = state.active_connections,
                "builder connection closed"
            );

            if state.active_connections != 0 {
                return;
            }

            state.idle_generation = state.idle_generation.wrapping_add(1);
            state.idle_generation
        };

        let app = Arc::clone(self);

        tokio::spawn(async move {
            sleep(app.config.idle_timeout).await;

            if let Err(error) = app.stop_if_idle(generation).await {
                error!(%error, "idle shutdown failed");
            }
        });
    }

    async fn stop_if_idle(&self, expected_generation: u64) -> Result<()> {
        {
            let state = self.state.lock().await;

            if state.active_connections != 0 || state.idle_generation != expected_generation {
                debug!("idle shutdown cancelled");
                return Ok(());
            }
        }

        let _lifecycle = self.lifecycle.lock().await;

        let vm = {
            let mut state = self.state.lock().await;

            if state.active_connections != 0 || state.idle_generation != expected_generation {
                debug!("idle shutdown cancelled while waiting for lifecycle lock");

                return Ok(());
            }

            state.vm.take()
        };

        if let Some(vm) = vm {
            info!("stopping idle builder VM");
            self.terminate_vm(vm).await?;
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    fn init_logging() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .init();
    }

    match Cli::parse().command {
        CliCommand::Daemon(args) => {
            init_logging();
            run_daemon(Config::try_from(args)?).await
        }

        CliCommand::Connect(args) => run_connect(args.runtime_dir.join(DAEMON_SOCKET_NAME)).await,
    }
}

async fn run_daemon(config: Config) -> Result<()> {
    fs::create_dir_all(&config.runtime_dir)
        .await
        .context(CreateRuntimeDirectorySnafu {
            path: config.runtime_dir.clone(),
        })?;

    remove_file_if_present(&config.daemon_socket).await?;

    let listener = UnixListener::bind(&config.daemon_socket).context(BindDaemonSocketSnafu {
        path: config.daemon_socket.clone(),
    })?;

    info!(
        socket = %config.daemon_socket.display(),
        runtime_dir = %config.runtime_dir.display(),
        ssh_port = config.ssh_port,
        "microvm builder daemon started"
    );

    let app = Arc::new(App::new(config));

    let mut sigterm =
        signal(SignalKind::terminate()).context(InstallSignalHandlerSnafu { signal: "SIGTERM" })?;

    let mut sigint =
        signal(SignalKind::interrupt()).context(InstallSignalHandlerSnafu { signal: "SIGINT" })?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) =
                    accepted.context(AcceptConnectionSnafu)?;

                let app = Arc::clone(&app);

                tokio::spawn(async move {
                    if let Err(error) =
                        handle_proxy_connection(app, stream).await
                    {
                        warn!(%error, "builder connection failed");
                    }
                });
            }

            _ = sigterm.recv() => {
                info!("received SIGTERM");
                break;
            }

            _ = sigint.recv() => {
                info!("received SIGINT");
                break;
            }
        }
    }

    if let Err(error) = app.stop_vm().await {
        error!(%error, "failed to stop VM during daemon shutdown");
    }

    let _ = remove_file_if_present(&app.config.daemon_socket).await;

    Ok(())
}

async fn handle_proxy_connection(app: Arc<App>, mut unix: UnixStream) -> Result<()> {
    app.connection_opened().await;

    let result = async {
        app.ensure_vm_running().await?;

        let mut tcp = TcpStream::connect(("127.0.0.1", app.config.ssh_port))
            .await
            .context(ConnectBuilderSnafu {
                port: app.config.ssh_port,
            })?;

        copy_bidirectional(&mut unix, &mut tcp)
            .await
            .context(ProxyTransportSnafu)?;

        Ok(())
    }
    .await;

    app.connection_closed().await;

    result
}

async fn run_connect(socket: PathBuf) -> Result<()> {
    let mut stream = UnixStream::connect(&socket)
        .await
        .context(ConnectDaemonSnafu { path: socket })?;

    let mut stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());

    copy_bidirectional(&mut stdio, &mut stream)
        .await
        .context(ProxyTransportSnafu)?;

    Ok(())
}

fn spawn_process_group(
    name: &'static str,
    executable: &Path,
    args: &[OsString],
) -> Result<ManagedChild> {
    let mut command = Command::new(executable);

    command.args(args);
    command.kill_on_drop(false);

    unsafe {
        command.pre_exec(|| {
            nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(io::Error::other)
        });
    }

    let child = command.spawn().context(SpawnProcessSnafu {
        name,
        path: executable.to_owned(),
    })?;

    let pid = child.id().context(MissingProcessIdSnafu { name })?;

    Ok(ManagedChild {
        name,
        child,
        process_group: Pid::from_raw(pid as i32),
    })
}

async fn terminate_process_group(child: &mut ManagedChild, grace_period: Duration) -> Result<()> {
    if let Some(status) = child.try_wait()? {
        debug!(
            process = child.name,
            %status,
            "process already exited"
        );

        return Ok(());
    }

    debug!(
        process = child.name,
        pgid = child.process_group.as_raw(),
        "sending SIGTERM"
    );

    send_process_group_signal(child, Signal::SIGTERM)?;

    let deadline = Instant::now() + grace_period;

    loop {
        if let Some(status) = child.try_wait()? {
            debug!(
                process = child.name,
                %status,
                "process exited after SIGTERM"
            );

            return Ok(());
        }

        if Instant::now() >= deadline {
            break;
        }

        sleep(Duration::from_millis(100)).await;
    }

    warn!(
        process = child.name,
        "process ignored SIGTERM; sending SIGKILL"
    );

    send_process_group_signal(child, Signal::SIGKILL)?;

    child
        .child
        .wait()
        .await
        .context(ReapProcessSnafu { name: child.name })?;

    Ok(())
}

fn send_process_group_signal(child: &ManagedChild, signal: Signal) -> Result<()> {
    match killpg(child.process_group, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),

        Err(source) => Err(Error::SignalProcessGroup {
            name: child.name,
            signal,
            source,
        }),
    }
}

async fn wait_for_path(path: &Path, child: &mut ManagedChild, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;

    loop {
        if fs::metadata(path).await.is_ok() {
            return Ok(());
        }

        if let Some(status) = child.try_wait()? {
            return ProcessExitedBeforePathSnafu {
                name: child.name,
                path: path.to_owned(),
                status,
            }
            .fail();
        }

        if Instant::now() >= deadline {
            return PathTimeoutSnafu {
                name: child.name,
                path: path.to_owned(),
            }
            .fail();
        }

        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_builder(
    ssh_port: u16,
    gvproxy: &mut ManagedChild,
    runner: &mut ManagedChild,
    wait_timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + wait_timeout;

    loop {
        match try_read_ssh_banner(ssh_port).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                debug!(%error, "builder SSH is not ready yet");
            }
        }

        if let Some(status) = gvproxy.try_wait()? {
            return ProcessExitedBeforeBuilderSnafu {
                name: gvproxy.name,
                status,
            }
            .fail();
        }

        if let Some(status) = runner.try_wait()? {
            return ProcessExitedBeforeBuilderSnafu {
                name: runner.name,
                status,
            }
            .fail();
        }

        if Instant::now() >= deadline {
            return BuilderTimeoutSnafu { port: ssh_port }.fail();
        }

        sleep(Duration::from_millis(250)).await;
    }
}

async fn try_read_ssh_banner(port: u16) -> io::Result<bool> {
    let mut stream = match timeout(
        Duration::from_secs(1),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Ok(false),
    };

    let mut buffer = [0_u8; 255];

    let length = match timeout(Duration::from_secs(1), stream.read(&mut buffer)).await {
        Ok(result) => result?,
        Err(_) => return Ok(false),
    };

    Ok(buffer[..length].starts_with(b"SSH-"))
}

async fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),

        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),

        Err(source) => Err(Error::RemoveFile {
            path: path.to_owned(),
            source,
        }),
    }
}

fn validate_file(path: &Path, name: &'static str) -> Result<()> {
    let metadata = std::fs::metadata(path).context(ReadMetadataSnafu {
        name,
        path: path.to_owned(),
    })?;

    ensure!(
        metadata.is_file(),
        NotAFileSnafu {
            name,
            path: path.to_owned(),
        }
    );

    Ok(())
}
