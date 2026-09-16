use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use navette_protocol::{App, Session, SessionStatus};
use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use thiserror::Error;

use crate::registry::{Registry, RegistryError, default_session_name, validate_session_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

pub trait ProcessRunner: Send + Sync + 'static {
    fn spawn(&self, spec: ProcessSpec) -> io::Result<u32>;
    fn is_alive(&self, pid: u32) -> bool;
    fn terminate(&self, pid: u32, force: bool) -> io::Result<()>;
}

#[derive(Debug, Default)]
pub struct RealProcessRunner;

impl ProcessRunner for RealProcessRunner {
    fn spawn(&self, spec: ProcessSpec) -> io::Result<u32> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args).envs(&spec.env).process_group(0);
        let mut child = command.spawn()?;
        let pid = child.id();
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(pid)
    }

    fn is_alive(&self, pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        match signal::kill(Pid::from_raw(pid), None) {
            Ok(()) | Err(Errno::EPERM) => true,
            Err(Errno::ESRCH) => false,
            Err(_) => false,
        }
    }

    fn terminate(&self, pid: u32, force: bool) -> io::Result<()> {
        let pid = i32::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds i32"))?;
        let signal = if force {
            Signal::SIGKILL
        } else {
            Signal::SIGTERM
        };
        match signal::kill(Pid::from_raw(-pid), signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("application has no executable: {0}")]
    InvalidApp(String),
    #[error("failed to create runtime directory {path}: {source}")]
    RuntimeDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("wprsd did not create session sockets before timeout")]
    ReadinessTimeout,
    #[error("failed to terminate PID {pid}: {source}")]
    Terminate {
        pid: u32,
        #[source]
        source: io::Error,
    },
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("session registry lock is poisoned")]
    RegistryLock,
}

#[derive(Debug)]
pub struct Supervisor<R: ProcessRunner> {
    runner: Arc<R>,
    registry: Arc<Mutex<Registry>>,
    xdg_runtime_dir: PathBuf,
    runtime_root: PathBuf,
    wprsd_program: String,
    readiness_timeout: Duration,
    shutdown_timeout: Duration,
    poll_interval: Duration,
}

impl<R: ProcessRunner> Supervisor<R> {
    pub fn new(
        runner: Arc<R>,
        registry: Arc<Mutex<Registry>>,
        xdg_runtime_dir: PathBuf,
        wprsd_program: impl Into<String>,
    ) -> Self {
        let runtime_root = xdg_runtime_dir.join("navette");
        Self {
            runner,
            registry,
            xdg_runtime_dir,
            runtime_root,
            wprsd_program: wprsd_program.into(),
            readiness_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(25),
        }
    }

    /// Runtime-only root for session-scoped bulk clipboard blobs.
    pub fn blob_root(&self) -> PathBuf {
        self.xdg_runtime_dir.join("navette-blobs")
    }

    pub fn with_timeouts(
        mut self,
        readiness_timeout: Duration,
        shutdown_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        self.readiness_timeout = readiness_timeout;
        self.shutdown_timeout = shutdown_timeout;
        self.poll_interval = poll_interval;
        self
    }

    pub fn registry(&self) -> &Arc<Mutex<Registry>> {
        &self.registry
    }

    pub async fn start(&self, app: &App, name: Option<&str>) -> Result<Session, SupervisorError> {
        let name = name
            .map(str::to_string)
            .unwrap_or_else(|| default_session_name(&app.id));
        validate_session_name(&name)?;
        if app.exec.is_empty() {
            return Err(SupervisorError::InvalidApp(app.id.clone()));
        }
        {
            let registry = self
                .registry
                .lock()
                .map_err(|_| SupervisorError::RegistryLock)?;
            if registry.get(&name).is_some() {
                return Err(RegistryError::AlreadyExists(name).into());
            }
        }

        let resources = SessionResources::new(&self.xdg_runtime_dir, &self.runtime_root, &name);
        create_private_directory(&resources.runtime_dir)?;

        // Both children must resolve the Wayland socket under the same
        // directory the supervisor polls, not whatever this process inherited.
        let runtime_env = (
            "XDG_RUNTIME_DIR".to_string(),
            self.xdg_runtime_dir.to_string_lossy().into_owned(),
        );
        let daemon_spec = ProcessSpec {
            program: self.wprsd_program.clone(),
            args: vec![
                format!("--wayland-display={}", resources.wayland_display),
                format!("--socket={}", resources.socket_path.display()),
            ],
            env: BTreeMap::from([runtime_env.clone()]),
        };
        let daemon_pid = self.spawn(daemon_spec)?;

        if !self.wait_for_ready(&resources, daemon_pid).await {
            let _ = self.runner.terminate(daemon_pid, true);
            return Err(SupervisorError::ReadinessTimeout);
        }

        let app_spec = ProcessSpec {
            program: app.exec[0].clone(),
            args: app.exec[1..].to_vec(),
            env: BTreeMap::from([
                runtime_env,
                (
                    "WAYLAND_DISPLAY".to_string(),
                    resources.wayland_display.clone(),
                ),
            ]),
        };
        let app_pid = match self.spawn(app_spec) {
            Ok(pid) => pid,
            Err(error) => {
                let _ = self.runner.terminate(daemon_pid, true);
                return Err(error);
            }
        };

        let session = Session {
            name,
            app_id: app.id.clone(),
            app_pid,
            daemon_pid,
            wayland_display: resources.wayland_display,
            socket_path: resources.socket_path.to_string_lossy().into_owned(),
            created_at_ms: now_ms()?,
            last_attached_at_ms: None,
            client_count: 0,
            status: SessionStatus::Running,
        };
        let insert_result = self
            .registry
            .lock()
            .map_err(|_| SupervisorError::RegistryLock)?
            .insert(session.clone());
        if let Err(error) = insert_result {
            let _ = self.runner.terminate(app_pid, true);
            let _ = self.runner.terminate(daemon_pid, true);
            return Err(error.into());
        }
        Ok(session)
    }

    pub async fn kill(&self, name: &str) -> Result<Session, SupervisorError> {
        let session = self
            .registry
            .lock()
            .map_err(|_| SupervisorError::RegistryLock)?
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;

        self.stop_process(session.app_pid).await?;
        self.stop_process(session.daemon_pid).await?;
        let removed = self
            .registry
            .lock()
            .map_err(|_| SupervisorError::RegistryLock)?
            .remove(name)?;
        let _ = fs::remove_dir_all(self.runtime_root.join(name));
        let _ = fs::remove_dir_all(self.blob_root().join(name));
        let _ = fs::remove_file(self.xdg_runtime_dir.join(&session.wayland_display));
        Ok(removed)
    }

    pub fn reconcile(&self) -> Result<bool, SupervisorError> {
        let runner = &self.runner;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| SupervisorError::RegistryLock)?;
        let running_before: Vec<String> = registry
            .list()
            .into_iter()
            .filter(|session| session.status == SessionStatus::Running)
            .map(|session| session.name)
            .collect();
        let changed = registry.reconcile(|pid| runner.is_alive(pid))?;
        let stopped: Vec<String> = running_before
            .into_iter()
            .filter(|name| {
                registry
                    .get(name)
                    .is_some_and(|session| session.status == SessionStatus::Stopped)
            })
            .collect();
        drop(registry);
        for name in stopped {
            let _ = fs::remove_dir_all(self.blob_root().join(name));
        }
        Ok(changed)
    }

    fn spawn(&self, spec: ProcessSpec) -> Result<u32, SupervisorError> {
        let program = spec.program.clone();
        self.runner
            .spawn(spec)
            .map_err(|source| SupervisorError::Spawn { program, source })
    }

    async fn wait_for_ready(&self, resources: &SessionResources, daemon_pid: u32) -> bool {
        let deadline = tokio::time::Instant::now() + self.readiness_timeout;
        loop {
            if resources.socket_path.exists() && resources.wayland_socket.exists() {
                return true;
            }
            if !self.runner.is_alive(daemon_pid) || tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn stop_process(&self, pid: u32) -> Result<(), SupervisorError> {
        if !self.runner.is_alive(pid) {
            return Ok(());
        }
        self.runner
            .terminate(pid, false)
            .map_err(|source| SupervisorError::Terminate { pid, source })?;
        let deadline = tokio::time::Instant::now() + self.shutdown_timeout;
        while self.runner.is_alive(pid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(self.poll_interval).await;
        }
        if self.runner.is_alive(pid) {
            self.runner
                .terminate(pid, true)
                .map_err(|source| SupervisorError::Terminate { pid, source })?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct SessionResources {
    runtime_dir: PathBuf,
    wayland_display: String,
    wayland_socket: PathBuf,
    socket_path: PathBuf,
}

impl SessionResources {
    fn new(xdg_runtime_dir: &Path, runtime_root: &Path, name: &str) -> Self {
        let runtime_dir = runtime_root.join(name);
        let wayland_display = format!("navette-{name}");
        Self {
            runtime_dir: runtime_dir.clone(),
            wayland_socket: xdg_runtime_dir.join(&wayland_display),
            wayland_display,
            socket_path: runtime_dir.join("wprs.sock"),
        }
    }
}

fn create_private_directory(path: &Path) -> Result<(), SupervisorError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|source| SupervisorError::RuntimeDirectory {
            path: path.to_path_buf(),
            source,
        })
}

fn now_ms() -> Result<u64, SupervisorError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SupervisorError::Clock)?
        .as_millis();
    u64::try_from(millis).map_err(|_| SupervisorError::Clock)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct FakeState {
        next_pid: u32,
        spawned: Vec<ProcessSpec>,
        alive: BTreeSet<u32>,
        terminated: Vec<(u32, bool)>,
        fail_program: Option<String>,
    }

    #[derive(Debug)]
    struct FakeRunner {
        state: Mutex<FakeState>,
        runtime_dir: PathBuf,
        create_sockets: bool,
    }

    impl FakeRunner {
        fn new(runtime_dir: PathBuf) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    next_pid: 100,
                    spawned: Vec::new(),
                    alive: BTreeSet::new(),
                    terminated: Vec::new(),
                    fail_program: None,
                }),
                runtime_dir,
                create_sockets: true,
            }
        }

        fn without_sockets(runtime_dir: PathBuf) -> Self {
            Self {
                create_sockets: false,
                ..Self::new(runtime_dir)
            }
        }
    }

    impl ProcessRunner for FakeRunner {
        fn spawn(&self, spec: ProcessSpec) -> io::Result<u32> {
            let mut state = self.state.lock().unwrap();
            if state.fail_program.as_deref() == Some(&spec.program) {
                return Err(io::Error::other("injected spawn failure"));
            }
            let pid = state.next_pid;
            state.next_pid += 1;
            state.alive.insert(pid);
            state.spawned.push(spec.clone());
            drop(state);

            if self.create_sockets && spec.program == "wprsd-test" {
                for arg in &spec.args {
                    if let Some(display) = arg.strip_prefix("--wayland-display=") {
                        fs::write(self.runtime_dir.join(display), b"").unwrap();
                    }
                    if let Some(socket) = arg.strip_prefix("--socket=") {
                        fs::write(socket, b"").unwrap();
                    }
                }
            }
            Ok(pid)
        }

        fn is_alive(&self, pid: u32) -> bool {
            self.state.lock().unwrap().alive.contains(&pid)
        }

        fn terminate(&self, pid: u32, force: bool) -> io::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.terminated.push((pid, force));
            state.alive.remove(&pid);
            Ok(())
        }
    }

    fn app() -> App {
        App {
            id: "firefox".into(),
            name: "Firefox".into(),
            icon: None,
            categories: Vec::new(),
            exec: vec!["firefox".into(), "--new-instance".into()],
            terminal: false,
        }
    }

    fn harness(runner: Arc<FakeRunner>, temp: &TempDir) -> Supervisor<FakeRunner> {
        let registry = Registry::open(temp.path().join("state/registry.json")).unwrap();
        Supervisor::new(
            runner,
            Arc::new(Mutex::new(registry)),
            temp.path().join("runtime"),
            "wprsd-test",
        )
        .with_timeouts(
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
    }

    #[tokio::test]
    async fn starts_daemon_then_app_with_isolated_resources() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runtime_env = runtime.to_string_lossy().into_owned();
        let runner = Arc::new(FakeRunner::new(runtime));
        let supervisor = harness(runner.clone(), &temp);

        let session = supervisor.start(&app(), Some("work")).await.unwrap();
        assert_eq!(session.daemon_pid, 100);
        assert_eq!(session.app_pid, 101);
        assert_eq!(session.wayland_display, "navette-work");
        assert!(session.socket_path.ends_with("navette/work/wprs.sock"));

        let state = runner.state.lock().unwrap();
        assert_eq!(state.spawned[0].program, "wprsd-test");
        assert_eq!(state.spawned[1].program, "firefox");
        assert_eq!(
            state.spawned[1].env,
            BTreeMap::from([
                ("WAYLAND_DISPLAY".into(), "navette-work".into()),
                ("XDG_RUNTIME_DIR".into(), runtime_env.clone()),
            ])
        );
    }

    /// `--runtime-dir` moves where the supervisor waits for the Wayland socket;
    /// both children must be told the same directory or wprsd creates the
    /// socket under the inherited `XDG_RUNTIME_DIR` and readiness times out.
    #[tokio::test]
    async fn spawned_processes_receive_the_supervisor_runtime_dir() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runner = Arc::new(FakeRunner::new(runtime.clone()));
        let supervisor = harness(runner.clone(), &temp);

        supervisor.start(&app(), Some("work")).await.unwrap();

        let expected = runtime.to_string_lossy().into_owned();
        let state = runner.state.lock().unwrap();
        for spec in &state.spawned {
            assert_eq!(
                spec.env.get("XDG_RUNTIME_DIR"),
                Some(&expected),
                "{} was spawned without the supervisor's runtime dir",
                spec.program
            );
        }
    }

    #[tokio::test]
    async fn rejects_duplicate_session_before_spawning() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runner = Arc::new(FakeRunner::new(runtime));
        let supervisor = harness(runner.clone(), &temp);
        supervisor.start(&app(), Some("work")).await.unwrap();

        let error = supervisor.start(&app(), Some("work")).await.unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::Registry(RegistryError::AlreadyExists(_))
        ));
        assert_eq!(runner.state.lock().unwrap().spawned.len(), 2);
    }

    #[tokio::test]
    async fn rolls_back_daemon_when_app_spawn_fails() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runner = Arc::new(FakeRunner::new(runtime));
        runner.state.lock().unwrap().fail_program = Some("firefox".into());
        let supervisor = harness(runner.clone(), &temp);

        assert!(matches!(
            supervisor.start(&app(), Some("work")).await,
            Err(SupervisorError::Spawn { .. })
        ));
        let state = runner.state.lock().unwrap();
        assert_eq!(state.terminated, [(100, true)]);
        assert!(supervisor.registry.lock().unwrap().get("work").is_none());
    }

    #[tokio::test]
    async fn times_out_and_kills_unready_daemon() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runner = Arc::new(FakeRunner::without_sockets(runtime));
        let supervisor = harness(runner.clone(), &temp);

        assert!(matches!(
            supervisor.start(&app(), Some("work")).await,
            Err(SupervisorError::ReadinessTimeout)
        ));
        assert_eq!(runner.state.lock().unwrap().terminated, [(100, true)]);
    }

    #[tokio::test]
    async fn kill_stops_both_processes_and_removes_session() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runner = Arc::new(FakeRunner::new(runtime));
        let supervisor = harness(runner.clone(), &temp);
        supervisor.start(&app(), Some("work")).await.unwrap();
        let blob_dir = supervisor.blob_root().join("work");
        fs::create_dir_all(&blob_dir).unwrap();
        fs::write(blob_dir.join("stale-blob"), b"old").unwrap();

        let removed = supervisor.kill("work").await.unwrap();
        assert_eq!(removed.name, "work");
        assert!(supervisor.registry.lock().unwrap().get("work").is_none());
        assert!(
            !blob_dir.exists(),
            "killing a session must discard its blob namespace before the name can be reused"
        );
        supervisor.start(&app(), Some("work")).await.unwrap();
        assert!(
            !blob_dir.exists(),
            "recreating the same session name must not resurrect stale blob data"
        );
        assert_eq!(
            runner.state.lock().unwrap().terminated,
            [(101, false), (100, false)]
        );
    }

    #[test]
    fn reconcile_uses_runner_liveness() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let runner = Arc::new(FakeRunner::new(runtime));
        let supervisor = harness(runner.clone(), &temp);
        supervisor
            .registry
            .lock()
            .unwrap()
            .insert(Session {
                name: "work".into(),
                app_id: "firefox".into(),
                app_pid: 100,
                daemon_pid: 101,
                wayland_display: "navette-work".into(),
                socket_path: "/run/work.sock".into(),
                created_at_ms: 0,
                last_attached_at_ms: None,
                client_count: 1,
                status: SessionStatus::Running,
            })
            .unwrap();
        let blob_dir = supervisor.blob_root().join("work");
        fs::create_dir_all(&blob_dir).unwrap();
        fs::write(blob_dir.join("stale-blob"), b"old").unwrap();

        assert!(supervisor.reconcile().unwrap());
        let registry = supervisor.registry.lock().unwrap();
        assert_eq!(registry.get("work").unwrap().status, SessionStatus::Stopped);
        assert_eq!(registry.get("work").unwrap().client_count, 0);
        assert!(
            !blob_dir.exists(),
            "crash reconciliation must remove stale blob namespaces"
        );
    }
}
