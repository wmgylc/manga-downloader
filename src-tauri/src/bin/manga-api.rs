use std::{collections::HashMap, net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, get_service},
    Json, Router,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    cli_path: String,
    tasks: Arc<RwLock<HashMap<String, DownloadTask>>>,
    task_db_path: Arc<PathBuf>,
    provider_limits: Arc<ProviderLimits>,
}

struct ProviderLimits {
    wnacg: Arc<Semaphore>,
    jmcomic: Arc<Semaphore>,
    img_concurrency: usize,
}

impl ProviderLimits {
    fn new(img_concurrency: usize) -> Self {
        Self {
            wnacg: Arc::new(Semaphore::new(1)),
            jmcomic: Arc::new(Semaphore::new(1)),
            img_concurrency: img_concurrency.max(1),
        }
    }

    async fn acquire(&self, provider: &str) -> anyhow::Result<OwnedSemaphorePermit> {
        let semaphore = match provider {
            "jmcomic" => self.jmcomic.clone(),
            _ => self.wnacg.clone(),
        };
        semaphore
            .acquire_owned()
            .await
            .map_err(|err| anyhow::anyhow!("获取 `{provider}` 下载槽位失败: {err}"))
    }

    fn cap_query(&self, query: &mut DownloadQuery) {
        query.img_concurrency = Some(
            query
                .img_concurrency
                .unwrap_or(self.img_concurrency)
                .clamp(1, self.img_concurrency),
        );
    }
}

#[derive(Debug, Deserialize, Clone)]
struct CommonQuery {
    api_domain: Option<String>,
    proxy: Option<String>,
    download_dir: Option<String>,
    config: Option<String>,
}

impl CommonQuery {
    fn append_cli_args(self, args: &mut Vec<String>) {
        if let Some(api_domain) = self.api_domain {
            args.push("--api-domain".to_string());
            args.push(api_domain);
        }
        if let Some(proxy) = self.proxy {
            args.push("--proxy".to_string());
            args.push(proxy);
        }
        if let Some(download_dir) = self.download_dir {
            args.push("--download-dir".to_string());
            args.push(download_dir);
        }
        if let Some(config) = self.config {
            args.push("--config".to_string());
            args.push(config);
        }
    }
}

#[derive(Debug, Deserialize)]
struct KeywordSearchQuery {
    q: String,
    page: Option<i64>,
    api_domain: Option<String>,
    proxy: Option<String>,
    download_dir: Option<String>,
    config: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TagSearchQuery {
    tag: String,
    page: Option<i64>,
    api_domain: Option<String>,
    proxy: Option<String>,
    download_dir: Option<String>,
    config: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct DownloadQuery {
    target: String,
    format: Option<String>,
    img_concurrency: Option<usize>,
    img_interval_sec: Option<u64>,
    use_original_filename: Option<bool>,
    api_domain: Option<String>,
    proxy: Option<String>,
    download_dir: Option<String>,
    config: Option<String>,
}

impl DownloadQuery {
    fn common_query(&self) -> CommonQuery {
        CommonQuery {
            api_domain: self.api_domain.clone(),
            proxy: self.proxy.clone(),
            download_dir: self.download_dir.clone(),
            config: self.config.clone(),
        }
    }

    fn comic_args(&self) -> Vec<String> {
        let mut args = vec!["comic".to_string(), "--json".to_string(), self.target.clone()];
        self.common_query().append_cli_args(&mut args);
        args
    }

    fn download_args(&self) -> Vec<String> {
        let mut args = vec!["download".to_string(), self.target.clone()];
        if let Some(format) = &self.format {
            args.push("--format".to_string());
            args.push(format.clone());
        }
        if let Some(img_concurrency) = self.img_concurrency {
            args.push("--img-concurrency".to_string());
            args.push(img_concurrency.to_string());
        }
        if let Some(img_interval_sec) = self.img_interval_sec {
            args.push("--img-interval-sec".to_string());
            args.push(img_interval_sec.to_string());
        }
        if self.use_original_filename.unwrap_or(false) {
            args.push("--use-original-filename".to_string());
        }
        self.common_query().append_cli_args(&mut args);
        args
    }
}

#[derive(Debug, Serialize)]
struct CommandResponse {
    ok: bool,
    exit_code: i32,
    command: Vec<String>,
    stdout: String,
    stderr: String,
    data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadTask {
    id: String,
    target: String,
    #[serde(default)]
    provider: Option<String>,
    status: String,
    title: Option<String>,
    cover: Option<String>,
    total_pages: Option<i64>,
    completed_pages: usize,
    error: Option<String>,
    zip_path: Option<String>,
    stdout: Vec<String>,
    stderr: Vec<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComicInfo {
    id: i64,
    title: String,
    cover: String,
    image_count: i64,
}

impl DownloadTask {
    fn new(task_id: String, target: String, comic_info: Option<&ComicInfo>, now: String) -> Self {
        let provider = Some(infer_provider(&target).to_string());
        Self {
            id: task_id,
            target,
            provider,
            status: "downloading".to_string(),
            title: comic_info.map(|comic| comic.title.clone()),
            cover: comic_info.map(|comic| comic.cover.clone()),
            total_pages: comic_info.map(|comic| comic.image_count),
            completed_pages: 0,
            error: None,
            zip_path: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            finished_at: None,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let host = std::env::var("MANGA_API_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = std::env::var("MANGA_API_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);
    let cli_path = std::env::var("MANGA_CLI_PATH").unwrap_or_else(|_| "manga-cli".to_string());
    let web_dist_path = std::env::var("MANGA_WEB_DIST_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/local/share/manga-web"));
    let task_db_path = std::env::var("MANGA_TASK_STORE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/manga-tasks.sqlite"));
    let tasks = Arc::new(RwLock::new(load_tasks(&task_db_path).await?));
    let provider_img_concurrency = std::env::var("MANGA_PROVIDER_IMG_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5);

    let api_router = Router::new()
        .route("/health", get(health))
        .route("/search/keyword", get(search_keyword))
        .route("/search/tag", get(search_tag))
        .route("/comic", get(get_comic))
        .route("/download", get(start_download))
        .route("/download/start", get(start_download))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task));
    let static_index = web_dist_path.join("index.html");
    let static_service = get_service(
        ServeDir::new(&web_dist_path).not_found_service(ServeFile::new(static_index)),
    );

    let app_state = AppState {
        cli_path,
        tasks,
        task_db_path: Arc::new(task_db_path),
        provider_limits: Arc::new(ProviderLimits::new(provider_img_concurrency)),
    };
    let app = Router::new()
        .nest("/api", api_router.clone())
        .merge(api_router)
        .fallback_service(static_service)
        .layer(CorsLayer::permissive())
        .with_state(app_state);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    println!("manga-api listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "commands": [
            "GET /search/keyword?q=<keyword>&page=1",
            "GET /search/tag?tag=<tag>&page=1",
            "GET /comic?target=<id-or-url>",
            "GET /download?target=<id-or-url>",
            "GET /download/start?target=<id-or-url>",
            "GET /tasks",
            "GET /tasks/<id>"
        ]
    }))
}

async fn search_keyword(
    State(state): State<AppState>,
    Query(query): Query<KeywordSearchQuery>,
) -> impl IntoResponse {
    let mut args = vec![
        "search".to_string(),
        "--json".to_string(),
        "--page".to_string(),
        query.page.unwrap_or(1).to_string(),
        "keyword".to_string(),
        query.q,
    ];
    CommonQuery {
        api_domain: query.api_domain,
        proxy: query.proxy,
        download_dir: query.download_dir,
        config: query.config,
    }
    .append_cli_args(&mut args);
    run_cli_json(&state.cli_path, args).await
}

async fn search_tag(
    State(state): State<AppState>,
    Query(query): Query<TagSearchQuery>,
) -> impl IntoResponse {
    let mut args = vec![
        "search".to_string(),
        "--json".to_string(),
        "--page".to_string(),
        query.page.unwrap_or(1).to_string(),
        "tag".to_string(),
        query.tag,
    ];
    CommonQuery {
        api_domain: query.api_domain,
        proxy: query.proxy,
        download_dir: query.download_dir,
        config: query.config,
    }
    .append_cli_args(&mut args);
    run_cli_json(&state.cli_path, args).await
}

async fn get_comic(State(state): State<AppState>, Query(query): Query<DownloadQuery>) -> impl IntoResponse {
    run_cli_json(&state.cli_path, query.comic_args()).await
}

async fn start_download(
    State(state): State<AppState>,
    Query(query): Query<DownloadQuery>,
) -> impl IntoResponse {
    let task_id = Uuid::new_v4().to_string();
    let now = now_string();

    let comic_info = fetch_comic_info(&state.cli_path, &query).await.ok();
    let task = DownloadTask::new(task_id.clone(), query.target.clone(), comic_info.as_ref(), now);

    state
        .tasks
        .write()
        .await
        .insert(task_id.clone(), task.clone());
    persist_task(state.task_db_path.as_ref(), &task).await;

    let state_for_task = state.clone();
    tokio::spawn(async move {
        run_download_task(state_for_task, task_id, query).await;
    });

    (StatusCode::OK, Json(task)).into_response()
}

async fn list_tasks(State(state): State<AppState>) -> impl IntoResponse {
    let mut tasks = state.tasks.read().await.values().cloned().collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(json!({ "tasks": tasks }))
}

async fn get_task(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.tasks.read().await.get(&id).cloned() {
        Some(task) => (StatusCode::OK, Json(json!(task))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("task `{id}` not found") })),
        )
            .into_response(),
    }
}

async fn run_download_task(state: AppState, task_id: String, mut query: DownloadQuery) {
    let provider = infer_provider(&query.target);
    update_task(&state, &task_id, |task| {
        task.stdout.push(format!("waiting for {provider} download slot"));
        task.updated_at = now_string();
    })
    .await;

    let _provider_permit = match state.provider_limits.acquire(provider).await {
        Ok(permit) => permit,
        Err(err) => {
            update_task(&state, &task_id, |task| {
                task.status = "failed".to_string();
                task.error = Some(err.to_string());
                task.finished_at = Some(now_string());
                task.updated_at = now_string();
            })
            .await;
            return;
        }
    };
    state.provider_limits.cap_query(&mut query);
    let capped_img_concurrency = query
        .img_concurrency
        .unwrap_or(state.provider_limits.img_concurrency);
    update_task(&state, &task_id, |task| {
        task.stdout.push(format!(
            "started {provider} download with shared image concurrency {}",
            capped_img_concurrency
        ));
        task.updated_at = now_string();
    })
    .await;

    let args = query.download_args();
    let mut command = Command::new(&state.cli_path);
    command
        .args(&args)
        .env("MANGA_CLI_DISABLE_TASK_PROXY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            update_task(&state, &task_id, |task| {
                task.status = "failed".to_string();
                task.error = Some(format!("启动下载任务失败: {err}"));
                task.finished_at = Some(now_string());
                task.updated_at = now_string();
            })
            .await;
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let state_stdout = state.clone();
    let task_stdout = task_id.clone();
    let stdout_handle = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                handle_stdout_line(&state_stdout, &task_stdout, &line).await;
            }
        }
    });

    let state_stderr = state.clone();
    let task_stderr = task_id.clone();
    let stderr_handle = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                update_task(&state_stderr, &task_stderr, |task| {
                    task.stderr.push(line.clone());
                    task.updated_at = now_string();
                })
                .await;
            }
        }
    });

    let status = child.wait().await.ok();
    let _ = stdout_handle.await;
    let _ = stderr_handle.await;

    update_task(&state, &task_id, |task| {
        task.updated_at = now_string();
        task.finished_at = Some(now_string());

        if status.is_some_and(|status| status.success()) {
            task.status = "success".to_string();
            if let Some(total_pages) = task.total_pages {
                task.completed_pages = total_pages as usize;
            }
            if task.title.is_none() {
                task.title = Some(task.target.clone());
            }
        } else {
            task.status = "failed".to_string();
            if task.error.is_none() {
                let last_error = task
                    .stderr
                    .last()
                    .cloned()
                    .or_else(|| task.stdout.last().cloned())
                    .unwrap_or_else(|| "下载失败".to_string());
                task.error = Some(last_error);
            }
        }
    })
    .await;
}

async fn handle_stdout_line(state: &AppState, task_id: &str, line: &str) {
    update_task(state, task_id, |task| {
        task.stdout.push(line.to_string());
        if let Some((completed, total)) = parse_progress(line) {
            task.completed_pages = completed;
            task.total_pages = Some(total as i64);
        }
        if let Some(zip_path) = line.strip_prefix("zipped to ") {
            task.zip_path = Some(zip_path.trim().to_string());
        }
        if let Some(downloaded_to) = line.strip_prefix("downloaded to ") {
            task.zip_path = Some(downloaded_to.trim().to_string());
        }
        task.updated_at = now_string();
    })
    .await;
}

async fn update_task<F>(state: &AppState, task_id: &str, mut updater: F)
where
    F: FnMut(&mut DownloadTask),
{
    let mut changed_task = None;
    {
        let mut tasks = state.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            updater(task);
            changed_task = Some(task.clone());
        }
    }
    if let Some(task) = changed_task {
        persist_task(state.task_db_path.as_ref(), &task).await;
    }
}

async fn persist_task(path: &PathBuf, task: &DownloadTask) {
    let path = path.clone();
    let task = task.clone();
    let task_id = task.id.clone();
    match tokio::task::spawn_blocking(move || persist_task_blocking(&path, &task)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("failed to persist task `{}`: {err:#}", task_id),
        Err(err) => eprintln!("failed to join sqlite task writer: {err}"),
    }
}

async fn load_tasks(path: &PathBuf) -> anyhow::Result<HashMap<String, DownloadTask>> {
    let path = path.clone();
    tokio::task::spawn_blocking(move || load_tasks_blocking(&path))
        .await
        .map_err(|err| anyhow::anyhow!("join sqlite loader failed: {err}"))?
}

fn persist_task_blocking(path: &PathBuf, task: &DownloadTask) -> anyhow::Result<()> {
    let conn = open_task_db(path)?;
    let payload = serde_json::to_string(task)?;
    conn.execute(
        "INSERT INTO tasks (id, updated_at, payload)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET
           updated_at = excluded.updated_at,
           payload = excluded.payload",
        params![task.id, task.updated_at, payload],
    )?;
    Ok(())
}

fn load_tasks_blocking(path: &PathBuf) -> anyhow::Result<HashMap<String, DownloadTask>> {
    let conn = open_task_db(path)?;
    let mut tasks = load_tasks_from_db(&conn)?;
    if tasks.is_empty() {
        if let Some(legacy_json_path) = first_existing_legacy_json_path(path) {
            let imported = load_legacy_json_tasks(&legacy_json_path)?;
            if !imported.is_empty() {
                persist_task_batch(&conn, imported.values())?;
                tasks = imported;
            }
        }
    }
    Ok(tasks)
}

fn open_task_db(path: &PathBuf) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            updated_at TEXT NOT NULL,
            payload TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_updated_at ON tasks(updated_at DESC);
        ",
    )?;
    Ok(conn)
}

fn load_tasks_from_db(conn: &Connection) -> anyhow::Result<HashMap<String, DownloadTask>> {
    let mut stmt = conn.prepare("SELECT payload FROM tasks ORDER BY updated_at DESC")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut tasks = HashMap::new();
    for row in rows {
        let payload = match row {
            Ok(payload) => payload,
            Err(err) => {
                eprintln!("failed to read task row from sqlite: {err:#}");
                continue;
            }
        };
        let task: DownloadTask = match serde_json::from_str(&payload) {
            Ok(task) => task,
            Err(err) => {
                eprintln!("failed to parse task payload from sqlite: {err:#}");
                continue;
            }
        };
        tasks.insert(task.id.clone(), task);
    }
    Ok(tasks)
}

fn first_existing_legacy_json_path(path: &PathBuf) -> Option<PathBuf> {
    let candidates = [
        path.with_extension("json"),
        PathBuf::from("/var/lib/manga-downloader/manga-tasks.json"),
        PathBuf::from("/data/manga-tasks.json"),
        PathBuf::from("/data/wnacg-tasks.json"),
    ];
    candidates.into_iter().find(|candidate| candidate.exists())
}

fn persist_task_batch<'a, I>(conn: &Connection, tasks: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = &'a DownloadTask>,
{
    let mut stmt = conn.prepare(
        "INSERT INTO tasks (id, updated_at, payload)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET
           updated_at = excluded.updated_at,
           payload = excluded.payload",
    )?;
    for task in tasks {
        let payload = serde_json::to_string(task)?;
        stmt.execute(params![task.id, task.updated_at, payload])?;
    }
    Ok(())
}

fn load_legacy_json_tasks(path: &PathBuf) -> anyhow::Result<HashMap<String, DownloadTask>> {
    let content = std::fs::read(path)?;
    Ok(serde_json::from_slice(&content)?)
}

fn parse_progress(line: &str) -> Option<(usize, usize)> {
    let line = line.strip_prefix('[')?;
    let (progress, _) = line.split_once(']')?;
    let (completed, total) = progress.split_once('/')?;
    Some((completed.parse().ok()?, total.parse().ok()?))
}

async fn fetch_comic_info(cli_path: &str, query: &DownloadQuery) -> anyhow::Result<ComicInfo> {
    let response = run_cli(cli_path, query.comic_args()).await?;
    if !response.ok {
        anyhow::bail!(response.stderr);
    }
    serde_json::from_str(&response.stdout).map_err(Into::into)
}

async fn run_cli_json(cli_path: &str, args: Vec<String>) -> axum::response::Response {
    match run_cli(cli_path, args).await {
        Ok(mut payload) => {
            payload.data = serde_json::from_str::<Value>(&payload.stdout).ok();
            let status = if payload.ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, Json(payload)).into_response()
        }
        Err(err) => command_error_response(err),
    }
}

fn command_error_response(err: anyhow::Error) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(CommandResponse {
            ok: false,
            exit_code: -1,
            command: vec![],
            stdout: String::new(),
            stderr: err.to_string(),
            data: None,
        }),
    )
        .into_response()
}


async fn run_cli(cli_path: &str, args: Vec<String>) -> anyhow::Result<CommandResponse> {
    let output = Command::new(cli_path)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    Ok(CommandResponse {
        ok: output.status.success(),
        exit_code: output.status.code().unwrap_or(-1),
        command: std::iter::once(cli_path.to_string())
            .chain(args.iter().cloned())
            .collect(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        data: None,
    })
}

fn now_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.to_string()
}

fn infer_provider(target: &str) -> &'static str {
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("jm:")
        || lower.contains("jmcomic")
        || lower.contains("jm-comic")
        || lower.contains("18comic")
        || lower.contains("jmapiproxy")
        || lower.contains("cdnzack")
        || lower.contains("cdnhth")
        || lower.contains("cdnbea")
    {
        "jmcomic"
    } else {
        "wnacg"
    }
}
