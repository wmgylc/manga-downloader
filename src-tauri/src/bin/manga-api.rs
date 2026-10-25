use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, get_service},
    Json, Router,
};
use manga_downloader_lib::{
    cli::{is_wnacg_collection_target, is_wnacg_index_target},
    utils::temp_download_dir,
    ComicCollection, ComicEpisode,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, watch, OwnedSemaphorePermit, RwLock, Semaphore},
};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    cli_path: String,
    task_manager: Arc<TaskManager>,
    provider_limits: Arc<ProviderLimits>,
    download_queue: mpsc::UnboundedSender<QueuedDownload>,
}

struct QueuedDownload {
    task_id: String,
    query: DownloadQuery,
    cancel_receiver: watch::Receiver<bool>,
}

const TASK_COOLDOWN: Duration = Duration::from_secs(60);

struct TaskManager {
    tasks: RwLock<HashMap<String, DownloadTask>>,
    cancel_signals: RwLock<HashMap<String, watch::Sender<bool>>>,
    task_db_path: PathBuf,
}

impl TaskManager {
    fn new(task_db_path: PathBuf, tasks: HashMap<String, DownloadTask>) -> Self {
        Self {
            tasks: RwLock::new(tasks),
            cancel_signals: RwLock::new(HashMap::new()),
            task_db_path,
        }
    }

    async fn insert(&self, task: DownloadTask) -> watch::Receiver<bool> {
        let (cancel_signal, receiver) = watch::channel(false);
        self.cancel_signals
            .write()
            .await
            .insert(task.id.clone(), cancel_signal);
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.id.clone(), task.clone());
        persist_task(&self.task_db_path, &task).await;
        receiver
    }

    async fn get(&self, id: &str) -> Option<DownloadTask> {
        self.tasks.read().await.get(id).cloned()
    }

    async fn list(&self) -> Vec<DownloadTask> {
        self.tasks.read().await.values().cloned().collect()
    }

    async fn update<F>(&self, id: &str, mut updater: F) -> Option<DownloadTask>
    where
        F: FnMut(&mut DownloadTask),
    {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id)?;
        updater(task);
        let task = task.clone();
        persist_task(&self.task_db_path, &task).await;
        Some(task)
    }

    async fn cancel(&self, id: &str) -> Option<DownloadTask> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id)?;
        if !task.status.is_terminal() {
            if task.status == TaskStatus::Queued {
                task.status = TaskStatus::Cancelled;
                task.finished_at = Some(now_string());
            } else {
                task.cancellation_requested = true;
            }
            task.updated_at = now_string();
        }
        let task = task.clone();

        if !task.status.is_terminal() || task.status == TaskStatus::Cancelled {
            if let Some(sender) = self.cancel_signals.read().await.get(id) {
                sender.send_replace(true);
            }
        }
        persist_task(&self.task_db_path, &task).await;
        Some(task)
    }

    async fn remove_cancel_signal(&self, id: &str) {
        self.cancel_signals.write().await.remove(id);
    }
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
    #[serde(alias = "url", alias = "id")]
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
        let mut args = vec![
            "comic".to_string(),
            "--json".to_string(),
            self.target.clone(),
        ];
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    Queued,
    Downloading,
    #[serde(alias = "success")]
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadTask {
    id: String,
    target: String,
    #[serde(default)]
    provider: Option<String>,
    status: TaskStatus,
    title: Option<String>,
    cover: Option<String>,
    total_pages: Option<i64>,
    completed_pages: usize,
    error: Option<String>,
    zip_path: Option<String>,
    #[serde(default)]
    cancellation_requested: bool,
    #[serde(default, skip_serializing)]
    cleanup_dir: Option<PathBuf>,
    stdout: Vec<String>,
    stderr: Vec<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadTaskResponse {
    #[serde(flatten)]
    task: DownloadTask,
    #[serde(rename = "task_id")]
    task_id: String,
    progress: TaskProgress,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionDownloadResponse {
    ok: bool,
    collection_id: i64,
    collection_title: String,
    status: TaskStatus,
    total_episodes: usize,
    #[serde(rename = "task_ids")]
    task_ids: Vec<String>,
    tasks: Vec<DownloadTaskResponse>,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskProgress {
    completed: usize,
    total: Option<usize>,
    percent: u8,
}

impl DownloadTaskResponse {
    fn new(task: DownloadTask) -> Self {
        let total = task
            .total_pages
            .and_then(|pages| usize::try_from(pages).ok());
        let percent = if task.status == TaskStatus::Completed {
            100
        } else {
            total
                .filter(|total| *total > 0)
                .map(|total| ((task.completed_pages.saturating_mul(100) / total).min(100)) as u8)
                .unwrap_or(0)
        };
        let task_id = task.id.clone();
        let message = task_message(&task);
        let progress = TaskProgress {
            completed: task.completed_pages,
            total,
            percent,
        };
        Self {
            task,
            task_id,
            progress,
            message,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComicInfo {
    id: i64,
    title: String,
    cover: String,
    image_count: i64,
}

enum ComicOrCollection {
    Comic(ComicInfo),
    Collection(ComicCollection),
}

impl DownloadTask {
    fn new(
        task_id: String,
        target: String,
        comic_info: &ComicInfo,
        cleanup_dir: PathBuf,
        now: String,
    ) -> Self {
        Self::new_queued(
            task_id,
            target,
            Some(comic_info.title.clone()),
            Some(comic_info.cover.clone()),
            Some(comic_info.image_count),
            cleanup_dir,
            now,
        )
    }

    fn new_for_collection_episode(
        task_id: String,
        target: String,
        episode: &ComicEpisode,
        cleanup_dir: PathBuf,
        now: String,
    ) -> Self {
        Self::new_queued(
            task_id,
            target,
            Some(episode.title.clone()),
            None,
            None,
            cleanup_dir,
            now,
        )
    }

    fn new_queued(
        task_id: String,
        target: String,
        title: Option<String>,
        cover: Option<String>,
        total_pages: Option<i64>,
        cleanup_dir: PathBuf,
        now: String,
    ) -> Self {
        let provider = Some(infer_provider(&target).to_string());
        Self {
            id: task_id,
            target,
            provider,
            status: TaskStatus::Queued,
            title,
            cover,
            total_pages,
            completed_pages: 0,
            error: None,
            zip_path: None,
            cancellation_requested: false,
            cleanup_dir: Some(cleanup_dir),
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
    let tasks = load_tasks(&task_db_path).await?;
    let provider_img_concurrency = std::env::var("MANGA_PROVIDER_IMG_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5);
    let (download_queue, queue_receiver) = mpsc::unbounded_channel();

    let api_router = Router::new()
        .route("/health", get(health))
        .route("/search/keyword", get(search_keyword))
        .route("/search/tag", get(search_tag))
        .route("/comic", get(get_comic))
        .route("/download", get(start_download).post(start_download_post))
        .route(
            "/download/start",
            get(start_download).post(start_download_post),
        )
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/task/{id}", get(get_task))
        .route("/task/{id}/cancel", axum::routing::post(cancel_task));
    let static_index = web_dist_path.join("index.html");
    let static_service =
        get_service(ServeDir::new(&web_dist_path).not_found_service(ServeFile::new(static_index)));

    let app_state = AppState {
        cli_path,
        task_manager: Arc::new(TaskManager::new(task_db_path, tasks)),
        provider_limits: Arc::new(ProviderLimits::new(provider_img_concurrency)),
        download_queue,
    };
    let queue_state = app_state.clone();
    tokio::spawn(async move {
        run_download_queue(queue_state, queue_receiver).await;
    });
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
            "POST /download {\"target\":\"<id-or-url>\"}",
            "POST /download/start {\"target\":\"<id-or-url>\"}",
            "GET /task/<id>",
            "POST /task/<id>/cancel",
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

async fn get_comic(
    State(state): State<AppState>,
    Query(query): Query<DownloadQuery>,
) -> impl IntoResponse {
    run_cli_json(&state.cli_path, query.comic_args()).await
}

async fn start_download(
    State(state): State<AppState>,
    Query(query): Query<DownloadQuery>,
) -> impl IntoResponse {
    create_download_task(state, query).await
}

async fn start_download_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    match parse_download_post_body(&headers, &body) {
        Ok(query) => create_download_task(state, query).await,
        Err(err) => error_message_response(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn create_download_task(state: AppState, query: DownloadQuery) -> axum::response::Response {
    if is_wnacg_collection_target(&query.target) {
        return create_collection_download_tasks(state, query).await;
    }

    if is_wnacg_index_target(&query.target) {
        return match fetch_comic_or_collection(&state.cli_path, &query).await {
            Ok(ComicOrCollection::Collection(collection)) => {
                create_collection_download_tasks_from_info(state, query, collection).await
            }
            Ok(ComicOrCollection::Comic(comic_info)) => {
                create_single_download_task(state, query, comic_info).await
            }
            Err(err) => error_message_response(
                StatusCode::BAD_GATEWAY,
                format!("解析漫画信息失败：{err:#}"),
            ),
        };
    }

    let comic_info = match fetch_comic_info(&state.cli_path, &query).await {
        Ok(comic) => comic,
        Err(err) => {
            return error_message_response(
                StatusCode::BAD_GATEWAY,
                format!("解析漫画信息失败：{err:#}"),
            );
        }
    };
    create_single_download_task(state, query, comic_info).await
}

async fn create_single_download_task(
    state: AppState,
    query: DownloadQuery,
    comic_info: ComicInfo,
) -> axum::response::Response {
    let provider = infer_provider(&query.target);
    let download_root = match resolve_download_root(&query) {
        Ok(path) => path,
        Err(err) => {
            return error_message_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取下载目录配置失败：{err:#}"),
            );
        }
    };
    let cleanup_dir = temp_download_dir(&download_root, provider, comic_info.id, &comic_info.title);
    let task = DownloadTask::new(
        Uuid::new_v4().to_string(),
        query.target.clone(),
        &comic_info,
        cleanup_dir,
        now_string(),
    );
    let response = enqueue_download_task(&state, task, query).await;

    (StatusCode::ACCEPTED, Json(response)).into_response()
}

async fn create_collection_download_tasks(
    state: AppState,
    query: DownloadQuery,
) -> axum::response::Response {
    let collection = match fetch_collection_info(&state.cli_path, &query).await {
        Ok(collection) => collection,
        Err(err) => {
            return error_message_response(
                StatusCode::BAD_GATEWAY,
                format!("解析合集分集失败：{err:#}"),
            );
        }
    };
    create_collection_download_tasks_from_info(state, query, collection).await
}

async fn create_collection_download_tasks_from_info(
    state: AppState,
    query: DownloadQuery,
    collection: ComicCollection,
) -> axum::response::Response {
    if collection.episodes.is_empty() {
        return error_message_response(StatusCode::BAD_GATEWAY, "合集没有可下载的分集".to_string());
    }

    let download_root = match resolve_download_root(&query) {
        Ok(path) => path,
        Err(err) => {
            return error_message_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取下载目录配置失败：{err:#}"),
            );
        }
    };
    let collection_title = collection.title.clone();
    let total_episodes = collection.episodes.len();
    let mut tasks = Vec::with_capacity(total_episodes);
    for episode in &collection.episodes {
        let target = episode.url.clone();
        let cleanup_dir = temp_download_dir(&download_root, "wnacg", episode.id, &episode.title);
        let task = DownloadTask::new_for_collection_episode(
            Uuid::new_v4().to_string(),
            target.clone(),
            episode,
            cleanup_dir,
            now_string(),
        );
        let mut episode_query = query.clone();
        episode_query.target = target;
        tasks.push(enqueue_download_task(&state, task, episode_query).await);
    }

    let task_ids = tasks.iter().map(|task| task.task_id.clone()).collect();
    (
        StatusCode::ACCEPTED,
        Json(CollectionDownloadResponse {
            ok: true,
            collection_id: collection.id,
            collection_title: collection_title.clone(),
            status: TaskStatus::Queued,
            total_episodes,
            task_ids,
            tasks,
            message: collection_title,
        }),
    )
        .into_response()
}

async fn enqueue_download_task(
    state: &AppState,
    task: DownloadTask,
    query: DownloadQuery,
) -> DownloadTaskResponse {
    let task_id = task.id.clone();
    let cancel_receiver = state.task_manager.insert(task.clone()).await;
    if state
        .download_queue
        .send(QueuedDownload {
            task_id: task_id.clone(),
            query,
            cancel_receiver,
        })
        .is_err()
    {
        update_task(state, &task_id, |task| {
            task.status = TaskStatus::Failed;
            task.error = Some("下载队列不可用".to_string());
            task.finished_at = Some(now_string());
            task.updated_at = now_string();
        })
        .await;
        state.task_manager.remove_cancel_signal(&task_id).await;
    }

    state
        .task_manager
        .get(&task_id)
        .await
        .map(DownloadTaskResponse::new)
        .unwrap_or_else(|| DownloadTaskResponse::new(task))
}

async fn run_download_queue(
    state: AppState,
    mut queue_receiver: mpsc::UnboundedReceiver<QueuedDownload>,
) {
    while let Some(queued) = queue_receiver.recv().await {
        let finished_download = run_download_task(
            state.clone(),
            queued.task_id,
            queued.query,
            queued.cancel_receiver,
        )
        .await;
        if finished_download {
            tokio::time::sleep(TASK_COOLDOWN).await;
        }
    }
}

fn task_title_for_message(task: &DownloadTask) -> &str {
    task.title.as_deref().unwrap_or(task.target.as_str())
}

fn task_message(task: &DownloadTask) -> String {
    if task.status == TaskStatus::Failed {
        task.error.clone().unwrap_or_else(|| "未知错误".to_string())
    } else {
        task_title_for_message(task).to_string()
    }
}

#[derive(Deserialize, Default)]
struct DownloadDirectoryConfig {
    default_download_dir: Option<PathBuf>,
}

fn resolve_download_root(query: &DownloadQuery) -> anyhow::Result<PathBuf> {
    if let Some(download_dir) = &query.download_dir {
        return Ok(PathBuf::from(download_dir));
    }

    let config_path = query
        .config
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("MANGA_CLI_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/config/manga-cli.json"));
    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)?;
        let config: DownloadDirectoryConfig = serde_json::from_str(&contents)?;
        if let Some(download_dir) = config.default_download_dir {
            return Ok(download_dir);
        }
    }

    Ok(std::env::current_dir()?.join("downloads"))
}

fn error_message_response(status: StatusCode, error: String) -> axum::response::Response {
    (
        status,
        Json(json!({
            "ok": false,
            "error": error.clone(),
            "message": error
        })),
    )
        .into_response()
}

fn parse_download_post_body(headers: &HeaderMap, body: &[u8]) -> anyhow::Result<DownloadQuery> {
    if body.is_empty() {
        anyhow::bail!("POST body cannot be empty");
    }

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    match content_type.as_str() {
        "application/x-www-form-urlencoded" => serde_urlencoded::from_bytes(body).map_err(Into::into),
        "application/json" => serde_json::from_slice(body).map_err(Into::into),
        "" => serde_json::from_slice(body)
            .map_err(anyhow::Error::from)
            .or_else(|_| serde_urlencoded::from_bytes(body).map_err(anyhow::Error::from)),
        _ => Err(anyhow::anyhow!(
            "unsupported content type `{}`; use application/json or application/x-www-form-urlencoded",
            content_type
        )),
    }
}

async fn list_tasks(State(state): State<AppState>) -> impl IntoResponse {
    let mut tasks = state.task_manager.list().await;
    tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let tasks = tasks
        .into_iter()
        .map(DownloadTaskResponse::new)
        .collect::<Vec<_>>();
    Json(json!({ "tasks": tasks }))
}

async fn get_task(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.task_manager.get(&id).await {
        Some(task) => (StatusCode::OK, Json(DownloadTaskResponse::new(task))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": format!("task `{id}` not found"),
                "message": format!("task `{id}` not found")
            })),
        )
            .into_response(),
    }
}

async fn cancel_task(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.task_manager.cancel(&id).await {
        Some(task) => (StatusCode::OK, Json(DownloadTaskResponse::new(task))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": format!("task `{id}` not found"),
                "message": format!("task `{id}` not found")
            })),
        )
            .into_response(),
    }
}

async fn run_download_task(
    state: AppState,
    task_id: String,
    mut query: DownloadQuery,
    mut cancel_receiver: watch::Receiver<bool>,
) -> bool {
    let provider = infer_provider(&query.target);
    let mut is_queued = false;
    update_task(&state, &task_id, |task| {
        if task.status == TaskStatus::Queued {
            task.stdout
                .push("waiting in the serial download queue".to_string());
            task.updated_at = now_string();
            is_queued = true;
        }
    })
    .await;
    if !is_queued {
        state.task_manager.remove_cancel_signal(&task_id).await;
        return false;
    }

    let acquisition = tokio::select! {
        biased;
        changed = cancel_receiver.changed() => {
            let _ = changed;
            None
        }
        result = state.provider_limits.acquire(provider) => Some(result),
    };
    let _provider_permit = match acquisition {
        None => {
            finish_cancelled(&state, &task_id).await;
            state.task_manager.remove_cancel_signal(&task_id).await;
            return false;
        }
        Some(Ok(permit)) => permit,
        Some(Err(err)) => {
            update_task(&state, &task_id, |task| {
                task.status = TaskStatus::Failed;
                task.error = Some(err.to_string());
                task.finished_at = Some(now_string());
                task.updated_at = now_string();
            })
            .await;
            state.task_manager.remove_cancel_signal(&task_id).await;
            return false;
        }
    };

    let mut started = false;
    state.provider_limits.cap_query(&mut query);
    let capped_img_concurrency = query
        .img_concurrency
        .unwrap_or(state.provider_limits.img_concurrency);
    update_task(&state, &task_id, |task| {
        if task.status == TaskStatus::Queued && !task.cancellation_requested {
            task.status = TaskStatus::Downloading;
            task.stdout.push(format!(
                "started {provider} download with shared image concurrency {}",
                capped_img_concurrency
            ));
            task.updated_at = now_string();
            started = true;
        }
    })
    .await;
    if !started {
        state.task_manager.remove_cancel_signal(&task_id).await;
        return false;
    }

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
                task.status = TaskStatus::Failed;
                task.error = Some(format!("启动下载任务失败: {err}"));
                task.finished_at = Some(now_string());
                task.updated_at = now_string();
            })
            .await;
            state.task_manager.remove_cancel_signal(&task_id).await;
            return true;
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

    enum DownloadOutcome {
        Cancelled,
        Exited(std::io::Result<ExitStatus>),
    }
    let outcome = tokio::select! {
        biased;
        changed = cancel_receiver.changed() => {
            if changed.is_ok() && *cancel_receiver.borrow() {
                match child.try_wait() {
                    Ok(Some(status)) => DownloadOutcome::Exited(Ok(status)),
                    _ => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        DownloadOutcome::Cancelled
                    }
                }
            } else {
                DownloadOutcome::Exited(child.wait().await)
            }
        }
        status = child.wait() => DownloadOutcome::Exited(status),
    };
    let _ = stdout_handle.await;
    let _ = stderr_handle.await;

    match outcome {
        DownloadOutcome::Cancelled => {
            cleanup_cancelled_task(&state, &task_id).await;
            finish_cancelled(&state, &task_id).await;
        }
        DownloadOutcome::Exited(status) => {
            update_task(&state, &task_id, |task| {
                task.updated_at = now_string();
                task.finished_at = Some(now_string());

                if status.as_ref().is_ok_and(ExitStatus::success) {
                    task.status = TaskStatus::Completed;
                    if let Some(total_pages) = task.total_pages {
                        task.completed_pages = total_pages.max(0) as usize;
                    }
                    if task.title.is_none() {
                        task.title = Some(task.target.clone());
                    }
                } else {
                    task.status = TaskStatus::Failed;
                    if task.error.is_none() {
                        let last_error = status
                            .as_ref()
                            .err()
                            .map(ToString::to_string)
                            .or_else(|| task.stderr.last().cloned())
                            .or_else(|| task.stdout.last().cloned())
                            .unwrap_or_else(|| "下载失败".to_string());
                        task.error = Some(last_error);
                    }
                }
                task.cancellation_requested = false;
                task.updated_at = now_string();
            })
            .await;
        }
    }
    state.task_manager.remove_cancel_signal(&task_id).await;
    true
}

async fn finish_cancelled(state: &AppState, task_id: &str) {
    update_task(state, task_id, |task| {
        if !task.status.is_terminal() {
            task.status = TaskStatus::Cancelled;
            task.cancellation_requested = false;
            task.finished_at = Some(now_string());
            task.updated_at = now_string();
        }
    })
    .await;
}

async fn cleanup_cancelled_task(state: &AppState, task_id: &str) {
    let Some(path) = state
        .task_manager
        .get(task_id)
        .await
        .and_then(|task| task.cleanup_dir)
    else {
        return;
    };
    let is_task_temp_dir = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".下载中-"));
    if !is_task_temp_dir {
        return;
    }
    if let Err(err) = tokio::fs::remove_dir_all(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            update_task(state, task_id, |task| {
                task.error = Some(format!("{}: {err}", path.display()));
                task.updated_at = now_string();
            })
            .await;
        }
    }
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

async fn update_task<F>(state: &AppState, task_id: &str, updater: F)
where
    F: FnMut(&mut DownloadTask),
{
    state.task_manager.update(task_id, updater).await;
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
    let mut interrupted = Vec::new();
    for task in tasks.values_mut() {
        if matches!(task.status, TaskStatus::Queued | TaskStatus::Downloading) {
            task.status = TaskStatus::Failed;
            task.error = Some("服务重启时任务中断".to_string());
            task.finished_at = Some(now_string());
            task.updated_at = now_string();
            interrupted.push(task.clone());
        }
    }
    if !interrupted.is_empty() {
        persist_task_batch(&conn, interrupted.iter())?;
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

async fn fetch_comic_or_collection(
    cli_path: &str,
    query: &DownloadQuery,
) -> anyhow::Result<ComicOrCollection> {
    let response = run_cli(cli_path, query.comic_args()).await?;
    if !response.ok {
        anyhow::bail!(response.stderr);
    }

    let value: Value = serde_json::from_str(&response.stdout)?;
    if value.get("episodes").is_some() {
        Ok(ComicOrCollection::Collection(serde_json::from_value(value)?))
    } else {
        Ok(ComicOrCollection::Comic(serde_json::from_value(value)?))
    }
}

async fn fetch_collection_info(
    cli_path: &str,
    query: &DownloadQuery,
) -> anyhow::Result<ComicCollection> {
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
