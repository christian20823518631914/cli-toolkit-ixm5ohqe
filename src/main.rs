use anyhow::{anyhow, Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::Builder;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::sleep;

const MAX_CODE_BYTES: usize = 64 * 1024;
const MAX_STDIN_BYTES: usize = 64 * 1024;
const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
const DEFAULT_COMPILE_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_RUN_TIMEOUT_MS: u64 = 1_000;
const DEFAULT_COMPILE_MEMORY_MB: u64 = 384;
const GO_COMPILE_TIMEOUT_MS: u64 = 30_000;
const SAFE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Aegis Core: low-latency multi-language execution core"
)]
struct Cli {
    #[arg(long, global = true)]
    runtime_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    List,
    Run {
        #[arg(short, long)]
        language: String,
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long, default_value = "")]
        stdin: String,
        #[arg(long)]
        json: bool,
    },
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: SocketAddr,
        #[arg(long)]
        demo_mode: bool,
    },
    Demo {
        #[arg(long, default_value = "demo/index.html")]
        out: PathBuf,
    },
}

#[derive(Clone)]
struct Engine {
    runtimes: Arc<Vec<Runtime>>,
    aliases: Arc<HashMap<String, usize>>,
    compiler_slots: Arc<Semaphore>,
    runner_slots: Arc<Semaphore>,
    demo_mode: bool,
    demo_hashes: Arc<HashMap<String, String>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct RuntimeInfo {
    id: String,
    display_name: String,
    aliases: Vec<String>,
    kind: RuntimeKind,
    source_file: String,
    toolchain: Vec<String>,
    memory_mb: u64,
    timeout_ms: u64,
}

#[derive(Clone)]
struct Runtime {
    info: RuntimeInfo,
    compile: Vec<Step>,
    run: Step,
    sample: String,
    transform: Transform,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeKind {
    Compiled,
    Interpreted,
    Transpiled,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Transform {
    #[default]
    None,
    TypeScriptLite,
}

#[derive(Clone, Deserialize)]
struct Step {
    program: String,
    args: Vec<String>,
    timeout_ms: u64,
    memory_mb: u64,
}

#[derive(Deserialize)]
struct RuntimeManifest {
    #[serde(flatten)]
    info: RuntimeInfo,
    #[serde(default)]
    compile: Vec<Step>,
    run: Step,
    sample: String,
    #[serde(default)]
    transform: Transform,
}

#[derive(Deserialize)]
struct RunRequest {
    language: String,
    code: String,
    stdin: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct RunResponse {
    status: RunStatus,
    language: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    signal: Option<i32>,
    compile_ms: u128,
    run_ms: u128,
    total_ms: u128,
    cached: bool,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
    security_mode: &'static str,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RunStatus {
    Success,
    CompileError,
    RuntimeError,
    Timeout,
    Rejected,
    ToolUnavailable,
    InternalError,
}

struct ProcessResult {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed_ms: u128,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

struct LimitedRead {
    data: Vec<u8>,
    truncated: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Cli {
        runtime_dir,
        command,
    } = Cli::parse();
    match command {
        Commands::List => {
            let engine = Engine::new(false, runtime_dir.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&engine.languages())?);
        }
        Commands::Run {
            language,
            file,
            stdin,
            json,
        } => {
            let engine = Engine::new(false, runtime_dir.as_deref())?;
            let code = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let response = engine
                .execute(RunRequest {
                    language,
                    code,
                    stdin: Some(stdin),
                    timeout_ms: None,
                })
                .await;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print!("{}", response.stdout);
                eprint!("{}", response.stderr);
                if !matches!(response.status, RunStatus::Success) {
                    return Err(anyhow!(
                        "execution failed: {}",
                        response
                            .error
                            .unwrap_or_else(|| "unknown error".to_string())
                    ));
                }
            }
        }
        Commands::Serve { addr, demo_mode } => {
            let engine = Arc::new(Engine::new(demo_mode, runtime_dir.as_deref())?);
            let app = Router::new()
                .route("/", get(index))
                .route("/healthz", get(healthz))
                .route("/v1/languages", get(languages))
                .route("/v1/run", post(run))
                .with_state(engine);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }
        Commands::Demo { out } => {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out, demo_html())?;
            println!("{}", out.display());
        }
    }
    Ok(())
}

async fn index() -> Html<String> {
    Html(demo_html())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn languages(State(engine): State<Arc<Engine>>) -> Json<Vec<RuntimeInfo>> {
    Json(engine.languages())
}

async fn run(
    State(engine): State<Arc<Engine>>,
    Json(request): Json<RunRequest>,
) -> impl IntoResponse {
    let status = match engine.validate_request(&request) {
        Ok(()) => StatusCode::OK,
        Err(response) => {
            let response = *response;
            let code = match response.status {
                RunStatus::Rejected => StatusCode::BAD_REQUEST,
                RunStatus::ToolUnavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return (code, Json(response)).into_response();
        }
    };
    (status, Json(engine.execute(request).await)).into_response()
}

impl Engine {
    fn new(demo_mode: bool, runtime_dir: Option<&Path>) -> Result<Self> {
        let mut runtimes = default_runtimes();
        if let Some(runtime_dir) = runtime_dir {
            merge_runtime_manifests(&mut runtimes, runtime_dir)?;
        }
        let aliases = runtimes
            .iter()
            .enumerate()
            .flat_map(|(idx, runtime)| {
                let mut keys = vec![runtime.info.id.to_string()];
                keys.extend(runtime.info.aliases.iter().cloned());
                keys.into_iter().map(move |key| (key.to_lowercase(), idx))
            })
            .collect();
        let demo_hashes = runtimes
            .iter()
            .map(|runtime| {
                (
                    runtime.info.id.clone(),
                    blake3::hash(runtime.sample.as_bytes()).to_hex().to_string(),
                )
            })
            .collect();
        Ok(Self {
            runtimes: Arc::new(runtimes),
            aliases: Arc::new(aliases),
            compiler_slots: Arc::new(Semaphore::new(1)),
            runner_slots: Arc::new(Semaphore::new(2)),
            demo_mode,
            demo_hashes: Arc::new(demo_hashes),
        })
    }

    fn languages(&self) -> Vec<RuntimeInfo> {
        self.runtimes
            .iter()
            .map(|runtime| runtime.info.clone())
            .collect()
    }

    fn runtime_for(&self, language: &str) -> Option<&Runtime> {
        self.aliases
            .get(&language.to_lowercase())
            .and_then(|idx| self.runtimes.get(*idx))
    }

    fn validate_request(&self, request: &RunRequest) -> std::result::Result<(), Box<RunResponse>> {
        let Some(runtime) = self.runtime_for(&request.language) else {
            return Err(Box::new(failure(
                RunStatus::Rejected,
                &request.language,
                "unsupported language",
            )));
        };
        if request.code.len() > MAX_CODE_BYTES {
            return Err(Box::new(failure(
                RunStatus::Rejected,
                &runtime.info.id,
                "source exceeds 64 KiB",
            )));
        }
        if request
            .stdin
            .as_ref()
            .map(|stdin| stdin.len() > MAX_STDIN_BYTES)
            .unwrap_or(false)
        {
            return Err(Box::new(failure(
                RunStatus::Rejected,
                &runtime.info.id,
                "stdin exceeds 64 KiB",
            )));
        }
        if self.demo_mode {
            let allowed = self
                .demo_hashes
                .get(&runtime.info.id)
                .map(|hash| hash == &blake3::hash(request.code.as_bytes()).to_hex().to_string())
                .unwrap_or(false);
            if !allowed {
                return Err(Box::new(failure(
                    RunStatus::Rejected,
                    &runtime.info.id,
                    "demo mode only runs bundled samples",
                )));
            }
        }
        Ok(())
    }

    async fn execute(&self, request: RunRequest) -> RunResponse {
        let total_start = Instant::now();
        if let Err(response) = self.validate_request(&request) {
            return *response;
        }
        let runtime = self
            .runtime_for(&request.language)
            .expect("validated runtime")
            .clone();

        let workspace = match Builder::new().prefix("aegis-job-").tempdir() {
            Ok(workspace) => workspace,
            Err(error) => {
                return failure(
                    RunStatus::InternalError,
                    &runtime.info.id,
                    &format!("workspace creation failed: {error}"),
                )
            }
        };

        let source_path = workspace.path().join(&runtime.info.source_file);
        let mut code = request.code;
        if let Transform::TypeScriptLite = runtime.transform {
            match transpile_typescript_lite(&code) {
                Ok(js) => {
                    code = js;
                }
                Err(error) => {
                    return response_with_times(
                        RunStatus::CompileError,
                        &runtime.info.id,
                        "",
                        &error.to_string(),
                        None,
                        None,
                        0,
                        0,
                        total_start.elapsed().as_millis(),
                        false,
                        false,
                        false,
                        false,
                        None,
                    );
                }
            }
        }
        if let Err(error) = fs::write(&source_path, code.as_bytes()) {
            return failure(
                RunStatus::InternalError,
                &runtime.info.id,
                &format!("source write failed: {error}"),
            );
        }

        let mut compile_ms = 0;
        let mut compile_stderr = String::new();
        let mut cached = false;
        let cache_path = compiled_cache_path(&runtime, &code);
        if !runtime.compile.is_empty() && cache_path.exists() {
            if let Err(error) = fs::copy(&cache_path, workspace.path().join("program")) {
                return failure(
                    RunStatus::InternalError,
                    &runtime.info.id,
                    &format!("cache restore failed: {error}"),
                );
            }
            cached = true;
        } else {
            for step in &runtime.compile {
                let _permit = match self.compiler_slots.acquire().await {
                    Ok(permit) => permit,
                    Err(error) => {
                        return failure(
                            RunStatus::InternalError,
                            &runtime.info.id,
                            &format!("compiler semaphore closed: {error}"),
                        )
                    }
                };
                let result = match execute_step(
                    step,
                    workspace.path(),
                    "",
                    DEFAULT_OUTPUT_LIMIT,
                    request.timeout_ms,
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        return failure(
                            RunStatus::ToolUnavailable,
                            &runtime.info.id,
                            &error.to_string(),
                        )
                    }
                };
                compile_ms += result.elapsed_ms;
                compile_stderr.push_str(&String::from_utf8_lossy(&result.stderr));
                if result.timed_out {
                    return response_with_times(
                        RunStatus::Timeout,
                        &runtime.info.id,
                        &String::from_utf8_lossy(&result.stdout),
                        &compile_stderr,
                        status_code(result.status),
                        signal(result.status),
                        compile_ms,
                        0,
                        total_start.elapsed().as_millis(),
                        false,
                        true,
                        result.stdout_truncated,
                        result.stderr_truncated,
                        Some("compile timeout"),
                    );
                }
                if !result.status.success() {
                    return response_with_times(
                        RunStatus::CompileError,
                        &runtime.info.id,
                        &String::from_utf8_lossy(&result.stdout),
                        &compile_stderr,
                        status_code(result.status),
                        signal(result.status),
                        compile_ms,
                        0,
                        total_start.elapsed().as_millis(),
                        false,
                        false,
                        result.stdout_truncated,
                        result.stderr_truncated,
                        Some("compile failed"),
                    );
                }
            }
            if !runtime.compile.is_empty() {
                if let Some(parent) = cache_path.parent() {
                    if let Err(error) = fs::create_dir_all(parent) {
                        return failure(
                            RunStatus::InternalError,
                            &runtime.info.id,
                            &format!("cache directory creation failed: {error}"),
                        );
                    }
                }
                if let Err(error) = fs::copy(workspace.path().join("program"), &cache_path) {
                    return failure(
                        RunStatus::InternalError,
                        &runtime.info.id,
                        &format!("cache write failed: {error}"),
                    );
                }
            }
        }

        let _permit = match self.runner_slots.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                return failure(
                    RunStatus::InternalError,
                    &runtime.info.id,
                    &format!("runner semaphore closed: {error}"),
                )
            }
        };
        let stdin = request.stdin.unwrap_or_default();
        let result = match execute_step(
            &runtime.run,
            workspace.path(),
            &stdin,
            DEFAULT_OUTPUT_LIMIT,
            request.timeout_ms,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                return failure(
                    RunStatus::ToolUnavailable,
                    &runtime.info.id,
                    &error.to_string(),
                )
            }
        };

        let run_status = if result.timed_out {
            RunStatus::Timeout
        } else if result.status.success() {
            RunStatus::Success
        } else {
            RunStatus::RuntimeError
        };
        let mut stderr = compile_stderr;
        stderr.push_str(&String::from_utf8_lossy(&result.stderr));
        response_with_times(
            run_status,
            &runtime.info.id,
            &String::from_utf8_lossy(&result.stdout),
            &stderr,
            status_code(result.status),
            signal(result.status),
            compile_ms,
            result.elapsed_ms,
            total_start.elapsed().as_millis(),
            cached,
            result.timed_out,
            result.stdout_truncated,
            result.stderr_truncated,
            None,
        )
    }
}

async fn execute_step(
    step: &Step,
    workspace: &Path,
    stdin: &str,
    output_limit: usize,
    timeout_override_ms: Option<u64>,
) -> Result<ProcessResult> {
    let expanded_program = expand_arg(&step.program, workspace);
    let program = resolve_program(&expanded_program)
        .ok_or_else(|| anyhow!("tool unavailable: {}", expanded_program))?;
    let mut command = Command::new(program);
    command
        .args(step.args.iter().map(|arg| expand_arg(arg, workspace)))
        .current_dir(workspace)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .env("HOME", workspace)
        .env("TMPDIR", workspace)
        .env("GOMAXPROCS", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let timeout_ms = timeout_override_ms.unwrap_or(step.timeout_ms);
    let memory_mb = step.memory_mb;
    unsafe {
        command.pre_exec(move || {
            set_process_limits(timeout_ms, memory_mb)?;
            Ok(())
        });
    }

    let start = Instant::now();
    let mut child = command.spawn()?;
    if let Some(mut child_stdin) = child.stdin.take() {
        let input = stdin.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = child_stdin.write_all(&input).await;
        });
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("stderr pipe unavailable"))?;
    let stdout_task = tokio::spawn(read_limited(stdout, output_limit));
    let stderr_task = tokio::spawn(read_limited(stderr, output_limit));
    let pid = child.id().unwrap_or_default();
    let timeout = sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = &mut timeout => {
            timed_out = true;
            kill_process_group(pid);
            child.wait().await?
        }
    };

    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    Ok(ProcessResult {
        status,
        stdout: stdout.data,
        stderr: stderr.data,
        elapsed_ms: start.elapsed().as_millis(),
        timed_out,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

async fn read_limited<R>(mut reader: R, limit: usize) -> Result<LimitedRead>
where
    R: AsyncRead + Unpin,
{
    let mut data = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(data.len());
        if remaining > 0 {
            let keep = remaining.min(read);
            data.extend_from_slice(&buffer[..keep]);
            truncated |= keep < read;
        } else {
            truncated = true;
        }
    }
    Ok(LimitedRead { data, truncated })
}

fn set_process_limits(timeout_ms: u64, memory_mb: u64) -> std::io::Result<()> {
    let cpu_seconds = (timeout_ms / 1_000).saturating_add(2).max(1);
    set_rlimit(libc::RLIMIT_CPU, cpu_seconds, cpu_seconds)?;
    if memory_mb > 0 {
        let memory_bytes = memory_mb.saturating_mul(1024 * 1024);
        set_rlimit(libc::RLIMIT_AS, memory_bytes, memory_bytes)?;
    }
    set_rlimit(libc::RLIMIT_FSIZE, 2 * 1024 * 1024, 2 * 1024 * 1024)?;
    set_rlimit(libc::RLIMIT_CORE, 0, 0)?;
    let rc = unsafe { libc::setpgid(0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_rlimit(resource: libc::__rlimit_resource_t, soft: u64, hard: u64) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    let rc = unsafe { libc::setrlimit(resource, &limit) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn kill_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn expand_arg(arg: &str, workspace: &Path) -> String {
    arg.replace("{workspace}", &workspace.display().to_string())
        .replace("{source}", &workspace.join("main").display().to_string())
        .replace(
            "{c_source}",
            &workspace.join("main.c").display().to_string(),
        )
        .replace(
            "{cpp_source}",
            &workspace.join("main.cpp").display().to_string(),
        )
        .replace(
            "{py_source}",
            &workspace.join("main.py").display().to_string(),
        )
        .replace(
            "{js_source}",
            &workspace.join("main.js").display().to_string(),
        )
        .replace(
            "{go_source}",
            &workspace.join("main.go").display().to_string(),
        )
        .replace("{binary}", &workspace.join("program").display().to_string())
}

fn resolve_program(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.exists().then(|| path.to_path_buf());
    }
    env::split_paths(OsStr::new(SAFE_PATH))
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.exists())
}

fn compiled_cache_path(runtime: &Runtime, code: &str) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    hasher.update(runtime.info.id.as_bytes());
    hasher.update(runtime.info.display_name.as_bytes());
    hasher.update(code.as_bytes());
    for step in &runtime.compile {
        hasher.update(step.program.as_bytes());
        for arg in &step.args {
            hasher.update(arg.as_bytes());
        }
    }
    let digest = hasher.finalize().to_hex().to_string();
    env::var_os("AEGIS_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".aegis-cache"))
        .join(&runtime.info.id)
        .join(digest)
}

fn status_code(status: ExitStatus) -> Option<i32> {
    status.code()
}

fn signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

fn failure(status: RunStatus, language: &str, error: &str) -> RunResponse {
    response_with_times(
        status,
        language,
        "",
        "",
        None,
        None,
        0,
        0,
        0,
        false,
        false,
        false,
        false,
        Some(error),
    )
}

#[allow(clippy::too_many_arguments)]
fn response_with_times(
    status: RunStatus,
    language: &str,
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    compile_ms: u128,
    run_ms: u128,
    total_ms: u128,
    cached: bool,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
    error: Option<&str>,
) -> RunResponse {
    RunResponse {
        status,
        language: language.to_string(),
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
        exit_code,
        signal,
        compile_ms,
        run_ms,
        total_ms,
        cached,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        security_mode: "process_rlimit_pgrp_v1; not a hostile multi-tenant boundary",
        error: error.map(ToString::to_string),
    }
}

fn transpile_typescript_lite(source: &str) -> Result<String> {
    let type_alias = Regex::new(r"(?m)^\s*type\s+[A-Za-z_$][A-Za-z0-9_$]*\s*=.*?;\s*$")?;
    let interface = Regex::new(r"(?ms)^\s*interface\s+[A-Za-z_$][A-Za-z0-9_$]*\s*\{.*?^\s*\}\s*$")?;
    let annotation = Regex::new(r":\s*[A-Za-z_$][A-Za-z0-9_$<>,\[\]|&.? ]*?([,)=;{])")?;
    let as_cast = Regex::new(r"\s+as\s+[A-Za-z_$][A-Za-z0-9_$<>,\[\]\s|&.?]*")?;
    let export = Regex::new(r"(?m)^\s*export\s+")?;
    let transformed = type_alias.replace_all(source, "");
    let transformed = interface.replace_all(&transformed, "");
    let transformed = annotation.replace_all(&transformed, "$1");
    let transformed = as_cast.replace_all(&transformed, "");
    let transformed = export.replace_all(&transformed, "");
    Ok(transformed.to_string())
}

fn merge_runtime_manifests(runtimes: &mut Vec<Runtime>, runtime_dir: &Path) -> Result<()> {
    let entries = fs::read_dir(runtime_dir)
        .with_context(|| format!("failed to read runtime directory {}", runtime_dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let manifest: RuntimeManifest = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
        )
        .with_context(|| format!("invalid runtime manifest {}", path.display()))?;
        validate_manifest(&manifest, &path)?;
        let runtime = Runtime {
            info: manifest.info,
            compile: manifest.compile,
            run: manifest.run,
            sample: manifest.sample,
            transform: manifest.transform,
        };
        if let Some(existing) = runtimes
            .iter_mut()
            .find(|existing| existing.info.id == runtime.info.id)
        {
            *existing = runtime;
        } else {
            runtimes.push(runtime);
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &RuntimeManifest, path: &Path) -> Result<()> {
    if manifest.info.id.is_empty()
        || !manifest
            .info
            .id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(anyhow!("{} has an invalid runtime id", path.display()));
    }
    let source_file = Path::new(&manifest.info.source_file);
    if source_file.file_name() != Some(source_file.as_os_str()) {
        return Err(anyhow!("{} source_file must be a basename", path.display()));
    }
    if manifest.run.program.is_empty() {
        return Err(anyhow!("{} run.program is empty", path.display()));
    }
    if manifest.compile.iter().any(|step| step.program.is_empty()) {
        return Err(anyhow!(
            "{} contains an empty compile program",
            path.display()
        ));
    }
    Ok(())
}

fn step(program: &str, args: &[&str], timeout_ms: u64, memory_mb: u64) -> Step {
    Step {
        program: program.to_string(),
        args: args.iter().map(ToString::to_string).collect(),
        timeout_ms,
        memory_mb,
    }
}

fn default_runtimes() -> Vec<Runtime> {
    vec![
        Runtime {
            info: RuntimeInfo {
                id: "c".to_string(),
                display_name: "C17 / GCC".to_string(),
                aliases: vec!["c17".to_string(), "gcc".to_string()],
                kind: RuntimeKind::Compiled,
                source_file: "main.c".to_string(),
                toolchain: vec!["cc".to_string()],
                memory_mb: 64,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            },
            compile: vec![step(
                "cc",
                &[
                    "-std=c17",
                    "-O0",
                    "-pipe",
                    "-Wall",
                    "-Wextra",
                    "{c_source}",
                    "-o",
                    "{binary}",
                ],
                DEFAULT_COMPILE_TIMEOUT_MS,
                DEFAULT_COMPILE_MEMORY_MB,
            )],
            run: step("{binary}", &[], DEFAULT_RUN_TIMEOUT_MS, 64),
            sample: C_SAMPLE.to_string(),
            transform: Transform::None,
        },
        Runtime {
            info: RuntimeInfo {
                id: "cpp".to_string(),
                display_name: "C++20 / G++".to_string(),
                aliases: vec!["c++".to_string(), "cpp20".to_string(), "g++".to_string()],
                kind: RuntimeKind::Compiled,
                source_file: "main.cpp".to_string(),
                toolchain: vec!["c++".to_string()],
                memory_mb: 64,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            },
            compile: vec![step(
                "c++",
                &[
                    "-std=c++20",
                    "-O0",
                    "-pipe",
                    "-Wall",
                    "-Wextra",
                    "{cpp_source}",
                    "-o",
                    "{binary}",
                ],
                DEFAULT_COMPILE_TIMEOUT_MS,
                DEFAULT_COMPILE_MEMORY_MB,
            )],
            run: step("{binary}", &[], DEFAULT_RUN_TIMEOUT_MS, 64),
            sample: CPP_SAMPLE.to_string(),
            transform: Transform::None,
        },
        Runtime {
            info: RuntimeInfo {
                id: "python".to_string(),
                display_name: "Python 3".to_string(),
                aliases: vec!["py".to_string(), "python3".to_string()],
                kind: RuntimeKind::Interpreted,
                source_file: "main.py".to_string(),
                toolchain: vec!["python3".to_string()],
                memory_mb: 128,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            },
            compile: vec![],
            run: step("python3", &["{py_source}"], DEFAULT_RUN_TIMEOUT_MS, 128),
            sample: PYTHON_SAMPLE.to_string(),
            transform: Transform::None,
        },
        Runtime {
            info: RuntimeInfo {
                id: "javascript".to_string(),
                display_name: "JavaScript / Node.js".to_string(),
                aliases: vec!["js".to_string(), "node".to_string()],
                kind: RuntimeKind::Interpreted,
                source_file: "main.js".to_string(),
                toolchain: vec!["node".to_string()],
                memory_mb: 256,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            },
            compile: vec![],
            run: step(
                "node",
                &["--max-old-space-size=128", "{js_source}"],
                DEFAULT_RUN_TIMEOUT_MS,
                0,
            ),
            sample: JS_SAMPLE.to_string(),
            transform: Transform::None,
        },
        Runtime {
            info: RuntimeInfo {
                id: "typescript".to_string(),
                display_name: "TypeScript / Aegis TS Lite".to_string(),
                aliases: vec!["ts".to_string()],
                kind: RuntimeKind::Transpiled,
                source_file: "main.js".to_string(),
                toolchain: vec!["aegis-ts-lite".to_string(), "node".to_string()],
                memory_mb: 256,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            },
            compile: vec![],
            run: step(
                "node",
                &["--max-old-space-size=128", "{js_source}"],
                DEFAULT_RUN_TIMEOUT_MS,
                0,
            ),
            sample: TS_SAMPLE.to_string(),
            transform: Transform::TypeScriptLite,
        },
        Runtime {
            info: RuntimeInfo {
                id: "go".to_string(),
                display_name: "Go".to_string(),
                aliases: vec!["golang".to_string()],
                kind: RuntimeKind::Compiled,
                source_file: "main.go".to_string(),
                toolchain: vec!["go".to_string()],
                memory_mb: 128,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            },
            compile: vec![step(
                "go",
                &["build", "-o", "{binary}", "{go_source}"],
                GO_COMPILE_TIMEOUT_MS,
                0,
            )],
            run: step("{binary}", &[], DEFAULT_RUN_TIMEOUT_MS, 0),
            sample: GO_SAMPLE.to_string(),
            transform: Transform::None,
        },
    ]
}

const C_SAMPLE: &str = r#"#include <stdio.h>

int main(void) {
    puts("aegis/c: hello in a constrained process");
    return 0;
}
"#;

const CPP_SAMPLE: &str = r#"#include <iostream>
#include <vector>

int main() {
    std::vector<int> xs{1, 2, 3, 5, 8};
    int sum = 0;
    for (int x : xs) sum += x;
    std::cout << "aegis/cpp: " << sum << "\n";
}
"#;

const PYTHON_SAMPLE: &str = r#"print("aegis/python:", sum([1, 2, 3, 5, 8]))
"#;

const JS_SAMPLE: &str = r#"const xs = [1, 2, 3, 5, 8];
console.log("aegis/javascript:", xs.reduce((a, b) => a + b, 0));
"#;

const TS_SAMPLE: &str = r#"type Label = string;

function score(xs: number[]): number {
  return xs.reduce((a: number, b: number) => a + b, 0);
}

const label: Label = "aegis/typescript:";
console.log(label, score([1, 2, 3, 5, 8]));
"#;

const GO_SAMPLE: &str = r#"package main

import "fmt"

func main() {
    xs := []int{1, 2, 3, 5, 8}
    sum := 0
    for _, x := range xs {
        sum += x
    }
    fmt.Println("aegis/go:", sum)
}
"#;

fn demo_html() -> String {
    let samples = serde_json::json!({
        "c": C_SAMPLE,
        "cpp": CPP_SAMPLE,
        "python": PYTHON_SAMPLE,
        "javascript": JS_SAMPLE,
        "typescript": TS_SAMPLE,
        "go": GO_SAMPLE,
    });
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Aegis Core Demo</title>
  <style>
    :root {{
      color-scheme: light;
      --bg: #fcfcfc;
      --panel: #f3f3f3;
      --line: #14141413;
      --text: #141414eb;
      --muted: #14141499;
      --accent: #3c7cab;
      --ok: #1f8a65;
      --err: #cf2d56;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    body {{ margin: 0; background: var(--bg); color: var(--text); }}
    main {{ max-width: 1180px; margin: 32px auto; padding: 0 20px; }}
    .shell {{ border: 1px solid var(--line); border-radius: 18px; overflow: hidden; background: white; box-shadow: 0 22px 80px #00000012; }}
    header {{ display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 14px 16px; background: var(--panel); border-bottom: 1px solid var(--line); }}
    h1 {{ font-size: 15px; margin: 0; letter-spacing: -0.02em; }}
    .badge {{ color: var(--muted); font-size: 12px; }}
    .toolbar {{ display: flex; gap: 10px; align-items: center; }}
    select, button {{ border: 1px solid var(--line); border-radius: 12px; background: white; color: var(--text); padding: 9px 12px; font: inherit; }}
    button {{ background: var(--text); color: white; cursor: pointer; font-weight: 650; }}
    .grid {{ display: grid; grid-template-columns: minmax(0, 1.2fr) minmax(320px, .8fr); min-height: 620px; }}
    textarea {{ width: 100%; height: 100%; box-sizing: border-box; border: 0; resize: none; padding: 22px; font: 14px/1.65 "JetBrains Mono", "SFMono-Regular", Consolas, monospace; outline: none; color: var(--text); background: #fff; }}
    aside {{ border-left: 1px solid var(--line); background: #fbfbfb; display: flex; flex-direction: column; }}
    .metrics {{ display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; padding: 16px; border-bottom: 1px solid var(--line); }}
    .metric {{ background: white; border: 1px solid var(--line); border-radius: 14px; padding: 10px; }}
    .metric b {{ display: block; font-size: 18px; letter-spacing: -0.03em; }}
    .metric span {{ color: var(--muted); font-size: 12px; }}
    pre {{ margin: 0; padding: 18px; overflow: auto; white-space: pre-wrap; font: 13px/1.55 "JetBrains Mono", Consolas, monospace; }}
    .stdout {{ flex: 1; color: var(--ok); }}
    .stderr {{ min-height: 120px; border-top: 1px solid var(--line); color: var(--err); background: #fff; }}
    @media (max-width: 860px) {{ .grid {{ grid-template-columns: 1fr; }} aside {{ border-left: 0; border-top: 1px solid var(--line); }} }}
  </style>
</head>
<body>
  <main>
    <div class="shell">
      <header>
        <div>
          <h1>Aegis Core</h1>
          <div class="badge">Rust execution core · first languages · constrained demo mode</div>
        </div>
        <div class="toolbar">
          <select id="language"></select>
          <button id="run">Run</button>
        </div>
      </header>
      <div class="grid">
        <textarea id="code" spellcheck="false"></textarea>
        <aside>
          <div class="metrics">
            <div class="metric"><b id="status">idle</b><span>status</span></div>
            <div class="metric"><b id="total">–</b><span>total</span></div>
            <div class="metric"><b id="compile">–</b><span>compile</span></div>
          </div>
          <pre id="stdout" class="stdout">Select a language and run the bundled sample.</pre>
          <pre id="stderr" class="stderr"></pre>
        </aside>
      </div>
    </div>
  </main>
  <script>
    const samples = {samples};
    const names = {{
      c: "C", cpp: "C++", python: "Python", javascript: "JavaScript", typescript: "TypeScript", go: "Go"
    }};
    const language = document.getElementById("language");
    const code = document.getElementById("code");
    const stdout = document.getElementById("stdout");
    const stderr = document.getElementById("stderr");
    const status = document.getElementById("status");
    const total = document.getElementById("total");
    const compile = document.getElementById("compile");
    for (const id of Object.keys(samples)) {{
      const option = document.createElement("option");
      option.value = id;
      option.textContent = names[id];
      language.append(option);
    }}
    function loadSample() {{
      code.value = samples[language.value];
      stdout.textContent = "";
      stderr.textContent = "";
      status.textContent = "idle";
      total.textContent = "–";
      compile.textContent = "–";
    }}
    language.addEventListener("change", loadSample);
    document.getElementById("run").addEventListener("click", async () => {{
      status.textContent = "running";
      stdout.textContent = "Compiling / executing...";
      stderr.textContent = "";
      const response = await fetch("/v1/run", {{
        method: "POST",
        headers: {{ "content-type": "application/json" }},
        body: JSON.stringify({{ language: language.value, code: code.value }})
      }});
      const result = await response.json();
      status.textContent = result.status;
      total.textContent = `${{result.total_ms}}ms`;
      compile.textContent = `${{result.compile_ms}}ms`;
      stdout.textContent = result.stdout || "";
      stderr.textContent = result.stderr || result.error || "";
    }});
    language.value = "c";
    loadSample();
  </script>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_aliases_resolve() {
        let engine = Engine::new(false, None).unwrap();
        assert_eq!(engine.runtime_for("c++").unwrap().info.id, "cpp");
        assert_eq!(engine.runtime_for("TS").unwrap().info.id, "typescript");
        assert_eq!(engine.runtime_for("golang").unwrap().info.id, "go");
        assert_eq!(
            engine.runtime_for("go").unwrap().compile[0].timeout_ms,
            GO_COMPILE_TIMEOUT_MS
        );
    }

    #[test]
    fn typescript_lite_removes_common_type_syntax() {
        let output = transpile_typescript_lite(TS_SAMPLE).unwrap();
        assert!(!output.contains("type Label"));
        assert!(!output.contains(": number"));
        assert!(!output.contains(": Label"));
        assert!(output.contains("function score(xs)"));
    }

    #[test]
    fn demo_mode_rejects_modified_source() {
        let engine = Engine::new(true, None).unwrap();
        let result = engine.validate_request(&RunRequest {
            language: "c".to_string(),
            code: "int main(void) { return 0; }".to_string(),
            stdin: None,
            timeout_ms: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn generated_demo_contains_every_first_launch_language() {
        let html = demo_html();
        for language in ["c", "cpp", "python", "javascript", "typescript", "go"] {
            assert!(html.contains(&format!("\"{language}\"")));
        }
    }

    #[test]
    fn runtime_manifest_adds_language_without_core_changes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("lua.json"),
            r#"{
              "id": "lua",
              "display_name": "Lua",
              "aliases": ["lua54"],
              "kind": "interpreted",
              "source_file": "main.lua",
              "toolchain": ["lua"],
              "memory_mb": 64,
              "timeout_ms": 1000,
              "compile": [],
              "run": {
                "program": "lua",
                "args": ["{workspace}/main.lua"],
                "timeout_ms": 1000,
                "memory_mb": 64
              },
              "sample": "print('hello')\n",
              "transform": "none"
            }"#,
        )
        .unwrap();
        let engine = Engine::new(false, Some(directory.path())).unwrap();
        assert_eq!(engine.runtime_for("lua54").unwrap().info.id, "lua");
    }
}
