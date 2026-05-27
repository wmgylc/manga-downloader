use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use bytes::Bytes;
use clap::{Args, Parser, Subcommand, ValueEnum};
use image::ImageFormat;
use reqwest::{Client, Proxy, StatusCode, Url};
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::{policies::ExponentialBackoff, Jitter, RetryTransientMiddleware};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, task::JoinSet, time::sleep};
use zip::write::SimpleFileOptions;

use crate::{
    config::DEFAULT_API_DOMAIN,
    jmcomic,
    types::{Comic, DownloadFormat, ImgList, SearchResult},
    utils::filename_filter,
};

#[derive(Parser, Debug)]
#[command(name = "wnacg-cli", about = "Search and download wnacg comics from the terminal")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Search(SearchArgs),
    Comic(ComicArgs),
    Download(DownloadArgs),
    Tasks(TasksArgs),
}

#[derive(Args, Debug)]
struct CommonArgs {
    #[arg(long, default_value = DEFAULT_API_DOMAIN)]
    api_domain: String,
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long)]
    download_dir: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct SearchArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(subcommand)]
    mode: SearchMode,
    #[arg(long, default_value_t = 1)]
    page: i64,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum SearchMode {
    Keyword { keyword: String },
    Tag { tag: String },
}

#[derive(Args, Debug)]
struct DownloadArgs {
    #[command(flatten)]
    common: CommonArgs,
    target: String,
    #[arg(long, value_enum, default_value_t = CliFormat::Jpeg)]
    format: CliFormat,
    #[arg(long, default_value_t = 10)]
    img_concurrency: usize,
    #[arg(long, default_value_t = 1)]
    img_interval_sec: u64,
    #[arg(long)]
    use_original_filename: bool,
}

#[derive(Args, Debug)]
struct ComicArgs {
    #[command(flatten)]
    common: CommonArgs,
    target: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct TasksArgs {
    id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliFormat {
    Jpeg,
    Png,
    Webp,
    Original,
}

impl From<CliFormat> for DownloadFormat {
    fn from(value: CliFormat) -> Self {
        match value {
            CliFormat::Jpeg => Self::Jpeg,
            CliFormat::Png => Self::Png,
            CliFormat::Webp => Self::Webp,
            CliFormat::Original => Self::Original,
        }
    }
}

#[derive(Clone)]
struct CliClient {
    api_domain: String,
    api_client: ClientWithMiddleware,
    img_client: ClientWithMiddleware,
    cover_client: Client,
}

#[derive(Clone)]
struct DownloadOptions {
    download_dir: PathBuf,
    format: DownloadFormat,
    img_concurrency: usize,
    img_interval_sec: u64,
    img_retry_count: usize,
    task_retry_count: usize,
    use_original_filename: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CliConfig {
    webhook_url: Option<String>,
    bark_url: Option<String>,
    default_download_dir: Option<PathBuf>,
    default_img_concurrency: Option<usize>,
    default_img_interval_sec: Option<u64>,
    default_img_retry_count: Option<usize>,
    default_task_retry_count: Option<usize>,
}

#[derive(Debug, Serialize)]
struct WebhookPayload {
    event: &'static str,
    status: &'static str,
    comic_id: Option<i64>,
    title: String,
    download_dir: Option<String>,
    zip_path: Option<String>,
    image_count: Option<i64>,
    completed_images: usize,
    total_images: usize,
    reason: Option<String>,
}

enum NotificationTarget<'a> {
    Webhook(&'a str),
    Bark(&'a str),
}

struct DownloadSuccess {
    final_dir: PathBuf,
    completed_images: usize,
    total_images: usize,
}

struct DownloadFailure {
    title: String,
    reason: String,
    completed_images: usize,
    total_images: usize,
    temp_dir: Option<PathBuf>,
    final_dir: Option<PathBuf>,
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().context("创建 Tokio 运行时失败")?;
    runtime.block_on(async move {
        match cli.command {
            Command::Search(args) => run_search(args).await,
            Command::Comic(args) => run_comic(args).await,
            Command::Download(args) => run_download(args).await,
            Command::Tasks(args) => run_tasks(args).await,
        }
    })
}

async fn run_search(args: SearchArgs) -> anyhow::Result<()> {
    let cli_config = load_cli_config(args.common.config.as_deref())?;
    let download_dir = resolve_download_dir(args.common.download_dir, &cli_config);
    let client = CliClient::new(&args.common.api_domain, args.common.proxy.as_deref())?;
    let result = match args.mode {
        SearchMode::Keyword { keyword } => {
            client
                .search_by_keyword(&keyword, args.page, Some(download_dir.as_path()))
                .await
                .with_context(|| format!("搜索关键词 `{keyword}` 失败"))?
        }
        SearchMode::Tag { tag } => client
            .search_by_tag(&tag, args.page, Some(download_dir.as_path()))
            .await
            .with_context(|| format!("搜索标签 `{tag}` 失败"))?,
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).context("序列化搜索结果失败")?
        );
        return Ok(());
    }

    println!("page {}/{}", result.current_page, result.total_page);
    for comic in result.comics {
        let downloaded = if comic.is_downloaded { "downloaded" } else { "-" };
        println!(
            "[{}] {} | {} | {}",
            comic.id, downloaded, comic.title, comic.additional_info
        );
    }

    Ok(())
}

async fn run_comic(args: ComicArgs) -> anyhow::Result<()> {
    if jmcomic::looks_like_jm_target(&args.target) {
        return run_jm_comic(args).await;
    }

    let cli_config = load_cli_config(args.common.config.as_deref())?;
    let download_dir = resolve_download_dir(args.common.download_dir, &cli_config);
    let client = CliClient::new(&args.common.api_domain, args.common.proxy.as_deref())?;
    let comic_id = client
        .resolve_comic_id(&args.target)
        .await
        .with_context(|| format!("无法从 `{}` 解析漫画 ID", args.target))?;
    let comic = client
        .get_comic(comic_id, Some(download_dir.as_path()))
        .await
        .with_context(|| format!("获取漫画 `{comic_id}` 详情失败"))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&comic).context("序列化漫画详情失败")?
        );
        return Ok(());
    }

    println!("[{}] {}", comic.id, comic.title);
    println!("cover: {}", comic.cover);
    println!("pages: {}", comic.image_count);
    Ok(())
}

async fn run_jm_comic(args: ComicArgs) -> anyhow::Result<()> {
    let api_domain = jm_api_domain(&args.common.api_domain);
    let client = jmcomic::JmClient::new(&api_domain, args.common.proxy.as_deref())?;
    let comic = client
        .fetch_summary(&args.target)
        .await
        .with_context(|| format!("获取 JMComic `{}` 详情失败", args.target))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&comic).context("序列化 JMComic 详情失败")?
        );
        return Ok(());
    }

    println!("[JM{}] {}", comic.id, comic.title);
    println!("cover: {}", comic.cover);
    println!("pages: {}", comic.image_count);
    Ok(())
}

async fn run_download(args: DownloadArgs) -> anyhow::Result<()> {
    if let Some(response) = maybe_submit_download_task(&args).await? {
        println!("{response}");
        return Ok(());
    }

    if jmcomic::looks_like_jm_target(&args.target) {
        return run_jm_download(args).await;
    }

    let cli_config = load_cli_config(args.common.config.as_deref())?;
    let download_dir = resolve_download_dir(args.common.download_dir, &cli_config);
    std::fs::create_dir_all(&download_dir)
        .with_context(|| format!("创建下载目录 `{}` 失败", download_dir.display()))?;

    let client = CliClient::new(&args.common.api_domain, args.common.proxy.as_deref())?;
    let comic_id = client
        .resolve_comic_id(&args.target)
        .await
        .with_context(|| format!("无法从 `{}` 解析漫画 ID", args.target))?;
    let comic = match client
        .get_comic(comic_id, Some(download_dir.as_path()))
        .await
    {
        Ok(comic) => comic,
        Err(err) => {
            notify_download_result(
                &client,
                &cli_config,
                WebhookPayload {
                    event: "download_finished",
                    status: "failed",
                    comic_id: Some(comic_id),
                    title: args.target.clone(),
                    download_dir: None,
                    zip_path: None,
                    image_count: None,
                    completed_images: 0,
                    total_images: 0,
                    reason: Some(format!("获取漫画详情失败: {err:#}")),
                },
            )
            .await?;
            return Err(err).with_context(|| format!("获取漫画 `{comic_id}` 详情失败"));
        }
    };

    let options = DownloadOptions {
        download_dir,
        format: args.format.into(),
        img_concurrency: cli_config
            .default_img_concurrency
            .unwrap_or(args.img_concurrency)
            .max(1),
        img_interval_sec: cli_config
            .default_img_interval_sec
            .unwrap_or(args.img_interval_sec),
        img_retry_count: cli_config.default_img_retry_count.unwrap_or(2),
        task_retry_count: cli_config.default_task_retry_count.unwrap_or(1),
        use_original_filename: args.use_original_filename,
    };

    let mut retry_index = 0usize;
    let download_success = loop {
        match download_comic(&client, &comic, &options).await {
            Ok(success) => break success,
            Err(failure) => {
                if retry_index < options.task_retry_count {
                    retry_index += 1;
                    println!(
                        "download attempt failed, retrying ({}/{}): {}",
                        retry_index + 1,
                        options.task_retry_count + 1,
                        failure.reason
                    );
                    sleep(Duration::from_secs(2)).await;
                    continue;
                }

                let resume_dir = failure
                    .temp_dir
                    .as_ref()
                    .or(failure.final_dir.as_ref())
                    .map(|path| path.display().to_string());
                let reason = match &resume_dir {
                    Some(path) => format!("{}。临时目录已保留，可续传：{path}", failure.reason),
                    None => failure.reason.clone(),
                };
                notify_download_result(
                    &client,
                    &cli_config,
                    WebhookPayload {
                        event: "download_finished",
                        status: "failed",
                        comic_id: Some(comic.id),
                        title: failure.title.clone(),
                        download_dir: resume_dir,
                        zip_path: None,
                        image_count: Some(comic.image_count),
                        completed_images: failure.completed_images,
                        total_images: failure.total_images,
                        reason: Some(reason.clone()),
                    },
                )
                .await?;
                return Err(anyhow!(reason));
            }
        }
    };

    let final_dir = download_success.final_dir;
    let zip_path = match create_zip_archive(&final_dir) {
        Ok(zip_path) => zip_path,
        Err(err) => {
            notify_download_result(
                &client,
                &cli_config,
                WebhookPayload {
                    event: "download_finished",
                    status: "failed",
                    comic_id: Some(comic.id),
                    title: comic.title.clone(),
                    download_dir: Some(final_dir.display().to_string()),
                    zip_path: None,
                    image_count: Some(comic.image_count),
                    completed_images: download_success.completed_images,
                    total_images: download_success.total_images,
                    reason: Some(format!("打包 zip 失败，目录已保留：{err:#}")),
                },
            )
            .await?;
            return Err(err).with_context(|| format!("将 `{}` 打包为 zip 失败", final_dir.display()));
        }
    };
    if let Err(err) = std::fs::remove_dir_all(&final_dir) {
        notify_download_result(
            &client,
            &cli_config,
            WebhookPayload {
                event: "download_finished",
                status: "failed",
                comic_id: Some(comic.id),
                title: comic.title.clone(),
                download_dir: Some(final_dir.display().to_string()),
                zip_path: Some(zip_path.display().to_string()),
                image_count: Some(comic.image_count),
                completed_images: download_success.completed_images,
                total_images: download_success.total_images,
                reason: Some(format!("删除已打包目录 `{}` 失败，目录已保留: {err}", final_dir.display())),
            },
        )
        .await?;
        return Err(anyhow!("删除已打包目录 `{}` 失败: {err}", final_dir.display()));
    }
    println!("zipped to {}", zip_path.display());

    notify_download_result(
        &client,
        &cli_config,
        WebhookPayload {
            event: "download_finished",
            status: "success",
            comic_id: Some(comic.id),
            title: comic.title.clone(),
            download_dir: None,
            zip_path: Some(zip_path.display().to_string()),
            image_count: Some(comic.image_count),
            completed_images: download_success.completed_images,
            total_images: download_success.total_images,
            reason: None,
        },
    )
    .await?;

    println!("downloaded to {}", zip_path.display());
    Ok(())
}

async fn run_jm_download(args: DownloadArgs) -> anyhow::Result<()> {
    let cli_config = load_cli_config(args.common.config.as_deref())?;
    let download_dir = resolve_download_dir(args.common.download_dir.clone(), &cli_config);
    std::fs::create_dir_all(&download_dir)
        .with_context(|| format!("创建下载目录 `{}` 失败", download_dir.display()))?;

    let notify_client = CliClient::new(&args.common.api_domain, args.common.proxy.as_deref())?;
    let api_domain = jm_api_domain(&args.common.api_domain);
    let jm_client = jmcomic::JmClient::new(&api_domain, args.common.proxy.as_deref())?;
    let options = jmcomic::JmDownloadOptions {
        download_dir,
        format: args.format.into(),
        img_concurrency: cli_config
            .default_img_concurrency
            .unwrap_or(args.img_concurrency)
            .max(1),
        img_interval_sec: cli_config
            .default_img_interval_sec
            .unwrap_or(args.img_interval_sec),
        img_retry_count: cli_config.default_img_retry_count.unwrap_or(2),
    };
    let task_retry_count = cli_config.default_task_retry_count.unwrap_or(1);

    let mut retry_index = 0usize;
    loop {
        match jm_client.download_target(&args.target, &options).await {
            Ok(success) => {
                notify_download_result(
                    &notify_client,
                    &cli_config,
                    WebhookPayload {
                        event: "download_finished",
                        status: "success",
                        comic_id: Some(success.comic_id),
                        title: success.title,
                        download_dir: None,
                        zip_path: Some(success.zip_path.display().to_string()),
                        image_count: Some(success.total_images as i64),
                        completed_images: success.completed_images,
                        total_images: success.total_images,
                        reason: None,
                    },
                )
                .await?;
                return Ok(());
            }
            Err(failure) => {
                if retry_index < task_retry_count {
                    retry_index += 1;
                    println!(
                        "jmcomic download attempt failed, retrying ({}/{}): {}",
                        retry_index + 1,
                        task_retry_count + 1,
                        failure.reason
                    );
                    sleep(Duration::from_secs(2)).await;
                    continue;
                }

                let resume_dir = failure
                    .download_dir
                    .as_ref()
                    .map(|path| path.display().to_string());
                let reason = match &resume_dir {
                    Some(path) => format!("{}。临时目录已保留，可续传：{path}", failure.reason),
                    None => failure.reason.clone(),
                };
                notify_download_result(
                    &notify_client,
                    &cli_config,
                    WebhookPayload {
                        event: "download_finished",
                        status: "failed",
                        comic_id: failure.comic_id,
                        title: failure.title,
                        download_dir: resume_dir,
                        zip_path: failure.zip_path.map(|path| path.display().to_string()),
                        image_count: Some(failure.total_images as i64),
                        completed_images: failure.completed_images,
                        total_images: failure.total_images,
                        reason: Some(reason.clone()),
                    },
                )
                .await?;
                return Err(anyhow!(reason));
            }
        }
    }
}

async fn run_tasks(args: TasksArgs) -> anyhow::Result<()> {
    let body = fetch_task_api_json(args.id.as_deref()).await?;
    if args.json {
        println!("{body}");
        return Ok(());
    }

    let payload: serde_json::Value =
        serde_json::from_str(&body).context("解析任务 API 返回值失败")?;
    if let Some(id) = args.id {
        print_task_summary(payload, Some(id.as_str()))?;
    } else if let Some(tasks) = payload.get("tasks").and_then(|tasks| tasks.as_array()) {
        for task in tasks {
            print_task_summary(task.clone(), None)?;
        }
    } else {
        return Err(anyhow!("任务 API 返回值中缺少 `tasks` 字段"));
    }
    Ok(())
}

async fn maybe_submit_download_task(args: &DownloadArgs) -> anyhow::Result<Option<String>> {
    if std::env::var_os("WNACG_CLI_DISABLE_TASK_PROXY").is_some() {
        return Ok(None);
    }

    let Some(api_base) = task_api_base() else {
        return Ok(None);
    };

    let mut url = task_api_url(&api_base, "download/start")
        .with_context(|| format!("解析任务 API 地址 `{api_base}` 失败"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("target", &args.target);
        let format_name = args
            .format
            .to_possible_value()
            .map(|value| value.get_name().to_string())
            .unwrap_or_else(|| "jpeg".to_string());
        query.append_pair("format", &format_name);
        query.append_pair("img_concurrency", &args.img_concurrency.to_string());
        query.append_pair("img_interval_sec", &args.img_interval_sec.to_string());
        if args.use_original_filename {
            query.append_pair("use_original_filename", "true");
        }
        query.append_pair("api_domain", &args.common.api_domain);
        if let Some(proxy) = &args.common.proxy {
            query.append_pair("proxy", proxy);
        }
        if let Some(download_dir) = &args.common.download_dir {
            query.append_pair("download_dir", &download_dir.display().to_string());
        }
        if let Some(config) = &args.common.config {
            query.append_pair("config", &config.display().to_string());
        }
    }

    let response = task_api_client()?
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("调用任务 API `{url}` 失败"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("任务 API 返回异常状态码({status}): {body}"));
    }
    Ok(Some(body))
}

async fn fetch_task_api_json(task_id: Option<&str>) -> anyhow::Result<String> {
    let Some(api_base) = task_api_base() else {
        return Err(anyhow!(
            "当前环境未配置 `WNACG_TASK_API_BASE`，无法查询统一任务中心"
        ));
    };

    let api_path = match task_id {
        Some(task_id) => format!("tasks/{task_id}"),
        None => "tasks".to_string(),
    };
    let url = task_api_url(&api_base, &api_path)
        .with_context(|| format!("解析任务 API 地址 `{api_base}/{api_path}` 失败"))?;
    let response = task_api_client()?
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("调用任务 API `{url}` 失败"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("任务 API 返回异常状态码({status}): {body}"));
    }
    Ok(body)
}

fn task_api_base() -> Option<String> {
    std::env::var("WNACG_TASK_API_BASE")
        .ok()
        .filter(|value| !value.is_empty())
}

fn task_api_url(api_base: &str, path: &str) -> anyhow::Result<Url> {
    Url::parse(&format!(
        "{}/{}",
        api_base.trim_end_matches('/'),
        path.trim_start_matches('/')
    ))
    .with_context(|| format!("解析任务 API 地址 `{api_base}` 失败"))
}

fn task_api_client() -> anyhow::Result<Client> {
    Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("创建任务 API 客户端失败")
}

fn print_task_summary(task: serde_json::Value, explicit_id: Option<&str>) -> anyhow::Result<()> {
    let id = task
        .get("id")
        .and_then(|value| value.as_str())
        .or(explicit_id)
        .unwrap_or("-");
    let status = task
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("-");
    let title = task
        .get("title")
        .and_then(|value| value.as_str())
        .or_else(|| task.get("target").and_then(|value| value.as_str()))
        .unwrap_or("-");
    let completed = task
        .get("completedPages")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let total = task
        .get("totalPages")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    println!("#{id} [{status}] {title}");
    println!("progress: {completed}/{total}");
    if let Some(error) = task.get("error").and_then(|value| value.as_str()) {
        println!("error: {error}");
    }
    if let Some(zip_path) = task.get("zipPath").and_then(|value| value.as_str()) {
        println!("zip: {zip_path}");
    }
    println!();
    Ok(())
}

impl CliClient {
    fn new(api_domain: &str, proxy: Option<&str>) -> anyhow::Result<Self> {
        let api_client = create_client(proxy, Duration::from_secs(3))?;
        let img_client = create_client(proxy, Duration::from_secs(15))?;
        let cover_client = create_plain_client(proxy, Duration::from_secs(15))?;
        Ok(Self {
            api_domain: api_domain.to_string(),
            api_client,
            img_client,
            cover_client,
        })
    }

    async fn search_by_keyword(
        &self,
        keyword: &str,
        page_num: i64,
        download_dir: Option<&Path>,
    ) -> anyhow::Result<SearchResult> {
        let params = serde_json::json!({
            "q": keyword,
            "syn": "yes",
            "f": "_all",
            "s": "create_time_DESC",
            "p": page_num,
        });
        let http_resp = self
            .api_client
            .get(format!("https://{}/search/index.php", self.api_domain))
            .header("referer", format!("https://{}/", self.api_domain))
            .query(&params)
            .send()
            .await?;
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        SearchResult::from_html_with_download_dir(download_dir, &body, false)
            .context("解析搜索结果失败")
    }

    async fn search_by_tag(
        &self,
        tag_name: &str,
        page_num: i64,
        download_dir: Option<&Path>,
    ) -> anyhow::Result<SearchResult> {
        let url = format!(
            "https://{}/albums-index-page-{page_num}-tag-{tag_name}.html",
            self.api_domain
        );
        let http_resp = self
            .api_client
            .get(url)
            .header("referer", format!("https://{}/", self.api_domain))
            .send()
            .await?;
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        SearchResult::from_html_with_download_dir(download_dir, &body, true)
            .context("解析标签搜索结果失败")
    }

    async fn get_img_list(&self, id: i64) -> anyhow::Result<ImgList> {
        let url = format!("https://{}/photos-gallery-aid-{id}.html", self.api_domain);
        let http_resp = self
            .api_client
            .get(url)
            .header("referer", format!("https://{}/", self.api_domain))
            .send()
            .await?;
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        let img_list_line = body
            .lines()
            .find(|line| line.contains("var imglist = "))
            .context("没有找到包含 `imglist` 的行")?;
        let start = img_list_line.find('[').context("没有找到 `[`")?;
        let end = img_list_line.rfind(']').context("没有找到 `]`")?;
        let json_str = &img_list_line[start..=end]
            .replace("url:", "\"url\":")
            .replace("caption:", "\"caption\":")
            .replace("fast_img_host+", "")
            .replace("\\\"", "\"");
        serde_json::from_str::<ImgList>(json_str).context("解析图片列表失败")
    }

    async fn get_comic(&self, id: i64, download_dir: Option<&Path>) -> anyhow::Result<Comic> {
        let http_resp = self
            .api_client
            .get(format!("https://{}/photos-index-aid-{id}.html", self.api_domain))
            .header("referer", format!("https://{}/", self.api_domain))
            .send()
            .await?;
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        let img_list = self.get_img_list(id).await?;
        Comic::from_html_with_context(download_dir, &self.api_domain, &body, img_list)
            .context("解析漫画详情失败")
    }

    async fn resolve_comic_id(&self, input: &str) -> anyhow::Result<i64> {
        if let Ok(id) = extract_comic_id(input) {
            return Ok(id);
        }

        let parsed_url =
            Url::parse(input).with_context(|| format!("`{input}` 不是支持的漫画 ID 或 URL"))?;
        let http_resp = self
            .api_client
            .get(parsed_url.clone())
            .header("referer", format!("https://{}/", self.api_domain))
            .send()
            .await
            .with_context(|| format!("请求 `{input}` 失败"))?;
        let final_url = http_resp.url().clone();
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        extract_comic_id(final_url.as_str())
            .or_else(|_| extract_comic_id_from_html(&body))
            .with_context(|| format!("无法从页面 `{}` 解析漫画 ID", final_url))
    }

    async fn get_img_data_and_format(&self, url: &str) -> anyhow::Result<(Bytes, ImageFormat)> {
        let http_resp = self
            .img_client
            .get(url)
            .header("referer", format!("https://{}/", self.api_domain))
            .send()
            .await?;
        let status = http_resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow!("IP 被封，请降低并发或增加下载间隔后重试"));
        }
        if status != StatusCode::OK {
            let body = http_resp.text().await?;
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        let image_data = http_resp.bytes().await?;
        let format = image::guess_format(&image_data)
            .context("无法从图片数据中判断格式，可能数据不完整或已损坏")?;
        Ok((image_data, format))
    }

    #[allow(dead_code)]
    async fn get_cover_data(&self, cover_url: &str) -> anyhow::Result<Bytes> {
        let http_resp = self
            .cover_client
            .get(cover_url)
            .header("referer", format!("https://{}/", self.api_domain))
            .send()
            .await?;
        let status = http_resp.status();
        if status != StatusCode::OK {
            let body = http_resp.text().await?;
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        Ok(http_resp.bytes().await?)
    }
}

async fn download_comic(
    client: &CliClient,
    comic: &Comic,
    options: &DownloadOptions,
) -> Result<DownloadSuccess, DownloadFailure> {
    let safe_dir_name = build_download_dir_name(comic.id, &comic.title);
    let temp_download_dir = options
        .download_dir
        .join(format!(".下载中-{safe_dir_name}"));
    let final_download_dir = options.download_dir.join(&safe_dir_name);
    std::fs::create_dir_all(&temp_download_dir).map_err(|err| DownloadFailure {
        title: comic.title.clone(),
        reason: format!("创建临时下载目录 `{}` 失败: {err}", temp_download_dir.display()),
        completed_images: 0,
        total_images: 0,
        temp_dir: Some(temp_download_dir.clone()),
        final_dir: Some(final_download_dir.clone()),
    })?;
    clean_temp_download_dir(&temp_download_dir, options.format).map_err(|err| DownloadFailure {
        title: comic.title.clone(),
        reason: format!("{err:#}"),
        completed_images: 0,
        total_images: 0,
        temp_dir: Some(temp_download_dir.clone()),
        final_dir: Some(final_download_dir.clone()),
    })?;
    save_metadata(comic, &temp_download_dir).map_err(|err| DownloadFailure {
        title: comic.title.clone(),
        reason: format!("{err:#}"),
        completed_images: 0,
        total_images: 0,
        temp_dir: Some(temp_download_dir.clone()),
        final_dir: Some(final_download_dir.clone()),
    })?;

    let urls = comic
        .img_list
        .iter()
        .map(|img| &img.url)
        .filter(|url| !url.ends_with("shoucang.jpg"))
        .map(|url| format!("https:{url}"))
        .collect::<Vec<_>>();

    let total = urls.len();
    let completed = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(options.img_concurrency.max(1)));
    let started_at = Instant::now();
    let mut join_set = JoinSet::new();

    for (index, url) in urls.into_iter().enumerate() {
        let permit_pool = semaphore.clone();
        let client = client.clone();
        let comic_title = comic.title.clone();
        let temp_download_dir = temp_download_dir.clone();
        let options = options.clone();
        let completed = completed.clone();
        let total_bytes = total_bytes.clone();
        join_set.spawn(async move {
            let _permit = permit_pool.acquire_owned().await?;
            let result = download_single_image(
                &client,
                &comic_title,
                &temp_download_dir,
                &options,
                index,
                &url,
            )
            .await;
            if let Ok(bytes) = result.as_ref() {
                total_bytes.fetch_add(*bytes as u64, Ordering::Relaxed);
                let current = completed.fetch_add(1, Ordering::Relaxed) + 1;
                println!("[{current}/{total}] {url}");
            }
            if options.img_interval_sec > 0 {
                sleep(Duration::from_secs(options.img_interval_sec)).await;
            }
            result
        });
    }

    let mut failures = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => failures.push(err),
            Err(err) => failures.push(anyhow!(err)),
        }
    }

    let completed_count = completed.load(Ordering::Relaxed);

    if !failures.is_empty() {
        let sample = failures
            .into_iter()
            .next()
            .map(|err| err.to_string())
            .unwrap_or_else(|| "存在下载失败，但没有具体错误".to_string());
        return Err(DownloadFailure {
            title: comic.title.clone(),
            reason: format!("漫画下载不完整: {sample}"),
            completed_images: completed_count,
            total_images: total,
            temp_dir: Some(temp_download_dir.clone()),
            final_dir: Some(final_download_dir.clone()),
        });
    }

    if final_download_dir.exists() {
        std::fs::remove_dir_all(&final_download_dir).map_err(|err| DownloadFailure {
            title: comic.title.clone(),
            reason: format!("删除目录 `{}` 失败: {err}", final_download_dir.display()),
            completed_images: completed_count,
            total_images: total,
            temp_dir: Some(temp_download_dir.clone()),
            final_dir: Some(final_download_dir.clone()),
        })?;
    }
    std::fs::rename(&temp_download_dir, &final_download_dir).map_err(|err| DownloadFailure {
        title: comic.title.clone(),
        reason: format!(
            "将 `{}` 重命名为 `{}` 失败: {err}",
            temp_download_dir.display(),
            final_download_dir.display()
        ),
        completed_images: completed_count,
        total_images: total,
        temp_dir: Some(temp_download_dir.clone()),
        final_dir: Some(final_download_dir.clone()),
    })?;

    let bytes = total_bytes.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0;
    let elapsed = started_at.elapsed().as_secs_f64().max(0.1);
    println!("saved {:.2} MiB in {:.1}s ({:.2} MiB/s)", bytes, elapsed, bytes / elapsed);

    Ok(DownloadSuccess {
        final_dir: final_download_dir,
        completed_images: completed_count,
        total_images: total,
    })
}

async fn download_single_image(
    client: &CliClient,
    comic_title: &str,
    temp_download_dir: &Path,
    options: &DownloadOptions,
    index: usize,
    url: &str,
) -> anyhow::Result<usize> {
    let index_filename = format!("{:04}", index + 1);
    let original_filename = url
        .rsplit('/')
        .next()
        .and_then(|segment| segment.split('.').next())
        .map(filename_filter)
        .unwrap_or(index_filename.clone());
    let img_filename = if options.use_original_filename {
        original_filename
    } else {
        index_filename
    };

    if let Some(ext) = options.format.extension() {
        let user_format_path = temp_download_dir.join(format!("{img_filename}.{ext}"));
        let gif_path = temp_download_dir.join(format!("{img_filename}.gif"));
        if user_format_path.exists() || gif_path.exists() {
            return Ok(0);
        }
    }

    let mut last_err = None;
    let mut img_data_and_format = None;
    for attempt in 0..=options.img_retry_count {
        match client.get_img_data_and_format(url).await {
            Ok(result) => {
                img_data_and_format = Some(result);
                break;
            }
            Err(err) => {
                last_err = Some(err);
                if attempt < options.img_retry_count {
                    sleep(Duration::from_secs(1 + attempt as u64)).await;
                }
            }
        }
    }

    let (img_data, img_format) = img_data_and_format.ok_or_else(|| {
        let err = last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "未知错误".to_string());
        anyhow!(
            "下载 `{comic_title}` 的图片 `{url}` 失败，已重试 {} 次: {err}",
            options.img_retry_count
        )
    })?;

    let src_img_ext = match img_format {
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Png => "png",
        ImageFormat::WebP => "webp",
        ImageFormat::Gif => "gif",
        _ => return Err(anyhow!("不支持的图片格式: {img_format:?}")),
    };

    let ext = match img_format {
        ImageFormat::Gif => "gif",
        _ => options.format.extension().unwrap_or(src_img_ext),
    };
    let save_path = temp_download_dir.join(format!("{img_filename}.{ext}"));
    let target_format = match img_format {
        ImageFormat::Gif => ImageFormat::Gif,
        _ => options.format.to_image_format().unwrap_or(img_format),
    };

    save_img(&save_path, target_format, img_data.clone(), img_format).await?;
    Ok(img_data.len())
}

fn save_metadata(comic: &Comic, temp_download_dir: &Path) -> anyhow::Result<()> {
    let mut metadata = comic.clone();
    metadata.is_downloaded = None;
    let content =
        serde_json::to_string_pretty(&metadata).context("将漫画元数据序列化为 JSON 失败")?;
    let path = temp_download_dir.join("元数据.json");
    std::fs::write(&path, content)
        .with_context(|| format!("写入元数据文件 `{}` 失败", path.display()))?;
    Ok(())
}

fn clean_temp_download_dir(temp_download_dir: &Path, format: DownloadFormat) -> anyhow::Result<()> {
    let extension = format.extension();
    for entry in std::fs::read_dir(temp_download_dir)
        .with_context(|| format!("读取目录 `{}` 失败", temp_download_dir.display()))?
    {
        let path = entry?.path();
        if path.file_name().and_then(|name| name.to_str()) == Some("元数据.json") {
            continue;
        }
        let should_keep = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "gif" || Some(ext) == extension);
        if should_keep {
            continue;
        }
        if path.is_file() {
            std::fs::remove_file(&path)
                .with_context(|| format!("删除旧文件 `{}` 失败", path.display()))?;
        }
    }
    Ok(())
}

async fn save_img(
    save_path: &Path,
    target_format: ImageFormat,
    src_img_data: Bytes,
    src_format: ImageFormat,
) -> anyhow::Result<()> {
    if target_format == src_format {
        std::fs::write(save_path, &src_img_data)
            .with_context(|| format!("将图片数据写入 `{}` 失败", save_path.display()))?;
        return Ok(());
    }

    let save_path = save_path.to_path_buf();
    let process_img = move || -> anyhow::Result<()> {
        let img = image::load_from_memory(&src_img_data).context("加载图片数据失败")?;
        let mut converted_data = Vec::new();
        match target_format {
            ImageFormat::Jpeg => img
                .to_rgb8()
                .write_to(&mut Cursor::new(&mut converted_data), target_format)
                .context(format!("将 `{src_format:?}` 转换为 `{target_format:?}` 失败"))?,
            ImageFormat::Png | ImageFormat::WebP => img
                .to_rgba8()
                .write_to(&mut Cursor::new(&mut converted_data), target_format)
                .context(format!("将 `{src_format:?}` 转换为 `{target_format:?}` 失败"))?,
            _ => return Err(anyhow!("不支持的图片格式: {target_format:?}")),
        }
        std::fs::write(&save_path, &converted_data)
            .with_context(|| format!("将图片数据写入 `{}` 失败", save_path.display()))?;
        Ok(())
    };

    let (sender, receiver) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    rayon::spawn(move || {
        let _ = sender.send(process_img());
    });
    receiver.await?
}

fn resolve_download_dir(download_dir: Option<PathBuf>, cli_config: &CliConfig) -> PathBuf {
    download_dir
        .or_else(|| cli_config.default_download_dir.clone())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("downloads")
        })
}

fn jm_api_domain(api_domain: &str) -> String {
    if api_domain == DEFAULT_API_DOMAIN || looks_like_wn_api_domain(api_domain) {
        jmcomic::default_api_domain().to_string()
    } else {
        api_domain.to_string()
    }
}

fn looks_like_wn_api_domain(api_domain: &str) -> bool {
    let trimmed = api_domain.trim();
    if trimmed.is_empty() {
        return false;
    }

    let normalized_url = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let Ok(url) = Url::parse(&normalized_url) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    host.contains("wnacg") || host.starts_with("wn") || host.contains(".wn")
}

fn load_cli_config(config_path: Option<&Path>) -> anyhow::Result<CliConfig> {
    let path = config_path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("WNACG_CLI_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/config/wnacg-cli.json"));

    if !path.exists() {
        return Ok(CliConfig::default());
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件 `{}` 失败", path.display()))?;
    serde_json::from_str::<CliConfig>(&content)
        .with_context(|| format!("解析配置文件 `{}` 失败", path.display()))
}

fn extract_comic_id(input: &str) -> anyhow::Result<i64> {
    if let Ok(id) = input.parse::<i64>() {
        return Ok(id);
    }
    let marker = "aid-";
    let start = input
        .find(marker)
        .map(|idx| idx + marker.len())
        .context("未找到 `aid-` 标记")?;
    let digits = input[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return Err(anyhow!("没有解析到数字 ID"));
    }
    digits.parse::<i64>().context("解析漫画 ID 失败")
}

fn extract_comic_id_from_html(body: &str) -> anyhow::Result<i64> {
    for marker in [
        "photos-index-aid-",
        "photos-index-page-1-aid-",
        "photos-gallery-aid-",
    ] {
        if let Some(start) = body.find(marker).map(|idx| idx + marker.len()) {
            let digits = body[start..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if !digits.is_empty() {
                return digits.parse::<i64>().context("解析页面中的漫画 ID 失败");
            }
        }
    }
    Err(anyhow!(
        "页面中未找到 `photos-index-aid-*` / `photos-index-page-1-aid-*` / `photos-gallery-aid-*` 标记"
    ))
}

fn create_client(proxy: Option<&str>, timeout: Duration) -> anyhow::Result<ClientWithMiddleware> {
    let retry_policy = ExponentialBackoff::builder()
        .base(1)
        .jitter(Jitter::Bounded)
        .build_with_total_retry_duration(Duration::from_secs(5));

    let client = create_plain_client(proxy, timeout)?;
    Ok(reqwest_middleware::ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build())
}

fn create_plain_client(proxy: Option<&str>, timeout: Duration) -> anyhow::Result<Client> {
    let mut builder = reqwest::ClientBuilder::new()
        .use_rustls_tls()
        .timeout(timeout);
    if let Some(proxy) = proxy {
        builder = builder.proxy(Proxy::all(proxy).with_context(|| format!("无效代理 `{proxy}`"))?);
    }
    builder.build().context("创建 HTTP 客户端失败")
}

fn create_zip_archive(download_dir: &Path) -> anyhow::Result<PathBuf> {
    let parent = download_dir
        .parent()
        .with_context(|| format!("无法获取 `{}` 的父目录", download_dir.display()))?;
    let name = download_dir
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("无法获取 `{}` 的目录名", download_dir.display()))?;
    let zip_path = parent.join(format!("{name}.zip"));

    if zip_path.exists() {
        std::fs::remove_file(&zip_path)
            .with_context(|| format!("删除旧 zip 文件 `{}` 失败", zip_path.display()))?;
    }

    let zip_file = std::fs::File::create(&zip_path)
        .with_context(|| format!("创建 zip 文件 `{}` 失败", zip_path.display()))?;
    let mut zip_writer = zip::ZipWriter::new(zip_file);
    let mut file_paths = std::fs::read_dir(download_dir)
        .with_context(|| format!("读取目录 `{}` 失败", download_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    file_paths.sort();

    for path in file_paths {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("无法获取文件名 `{}`", path.display()))?;
        zip_writer
            .start_file(
                filename,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .with_context(|| format!("向 `{}` 写入条目 `{filename}` 失败", zip_path.display()))?;
        let data =
            std::fs::read(&path).with_context(|| format!("读取文件 `{}` 失败", path.display()))?;
        use std::io::Write;
        zip_writer
            .write_all(&data)
            .with_context(|| format!("写入 zip 条目 `{filename}` 失败"))?;
    }

    zip_writer
        .finish()
        .with_context(|| format!("关闭 zip 文件 `{}` 失败", zip_path.display()))?;

    Ok(zip_path)
}

fn build_download_dir_name(comic_id: i64, title: &str) -> String {
    const MAX_NAME_BYTES: usize = 180;

    let sanitized = filename_filter(title).trim().to_string();
    if sanitized.is_empty() {
        return comic_id.to_string();
    }
    if sanitized.len() <= MAX_NAME_BYTES {
        return sanitized;
    }

    let suffix = format!(" [{comic_id}]");
    let available = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let truncated = truncate_utf8_by_bytes(&sanitized, available);
    format!("{truncated}{suffix}")
}

fn truncate_utf8_by_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = 0;
    for (idx, ch) in input.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }

    input[..end].trim_end().to_string()
}

async fn notify_download_result(
    client: &CliClient,
    cli_config: &CliConfig,
    payload: WebhookPayload,
) -> anyhow::Result<()> {
    let mut targets = Vec::new();

    if let Some(webhook_url) = cli_config.webhook_url.as_deref().filter(|url| !url.is_empty()) {
        targets.push(NotificationTarget::Webhook(webhook_url));
    }
    if let Some(bark_url) = cli_config.bark_url.as_deref().filter(|url| !url.is_empty()) {
        targets.push(NotificationTarget::Bark(bark_url));
    }

    for target in targets {
        match target {
            NotificationTarget::Webhook(webhook_url) => {
                notify_webhook(client, webhook_url, &payload).await?;
                println!("webhook called {}", webhook_url);
            }
            NotificationTarget::Bark(bark_url) => {
                notify_bark(client, bark_url, &payload).await?;
                println!("bark called {}", bark_url);
            }
        }
    }

    Ok(())
}

async fn notify_webhook(
    client: &CliClient,
    webhook_url: &str,
    payload: &WebhookPayload,
) -> anyhow::Result<()> {
    let response = client
        .cover_client
        .post(webhook_url)
        .json(payload)
        .send()
        .await
        .with_context(|| format!("调用 webhook `{webhook_url}` 失败"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("webhook 返回异常状态码({status}): {body}"));
    }

    Ok(())
}

async fn notify_bark(client: &CliClient, bark_url: &str, payload: &WebhookPayload) -> anyhow::Result<()> {
    let (title, body) = bark_message_from_payload(payload);
    let encoded_title = urlencoding::encode(&title);
    let encoded_body = urlencoding::encode(&body);
    let bark_url = format!(
        "{}/{}/{}",
        bark_url.trim_end_matches('/'),
        encoded_title,
        encoded_body
    );
    let response = client
        .cover_client
        .get(&bark_url)
        .send()
        .await
        .with_context(|| format!("调用 Bark `{}` 失败", bark_url))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Bark 返回异常状态码({status}): {body}"));
    }

    Ok(())
}

fn bark_message_from_payload(payload: &WebhookPayload) -> (String, String) {
    let title = match payload.status {
        "success" => format!(
            "下载完成 #{}",
            payload
                .comic_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string())
        ),
        _ => format!(
            "下载失败 #{}",
            payload
                .comic_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string())
        ),
    };
    let body = match payload.status {
        "success" => payload.title.clone(),
        _ => format!(
            "{} | {} | {}/{}",
            payload.reason.as_deref().unwrap_or("未知错误"),
            payload.title,
            payload.completed_images,
            payload.total_images
        ),
    };
    (title, body)
}
