use std::{
    collections::{HashSet, VecDeque},
    ffi::OsString,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes256;
use anyhow::{anyhow, Context};
use base64::engine::general_purpose;
use base64::Engine;
use bytes::Bytes;
use image::{ImageFormat, RgbImage};
use reqwest::{Client, Proxy, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::Semaphore, task::JoinSet, time::sleep};
use zip::write::SimpleFileOptions;

use crate::{
    types::DownloadFormat,
    utils::{filename_filter, md5_hex},
};

const DEFAULT_API_DOMAIN: &str = "www.cdnhth.cc";
const IMAGE_DOMAIN: &str = "cdn-msp2.jmapiproxy2.cc";
const COVER_DOMAIN: &str = "cdn-msp3.18comic.vip";
const APP_TOKEN_SECRET: &str = "18comicAPP";
const APP_TOKEN_SECRET_2: &str = "18comicAPPContent";
const APP_DATA_SECRET: &str = "185Hcomic3PAPP7R";
const APP_VERSION: &str = "2.0.13";
const JM_HOST_MARKERS: &[&str] = &[
    "jmcomic",
    "jm-comic",
    "18comic",
    "jmapiproxy",
    "cdnzack",
    "cdnhth",
    "cdnbea",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiPath {
    GetComic,
    GetChapter,
    GetScrambleId,
}

impl ApiPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::GetComic => "/album",
            Self::GetChapter => "/chapter",
            Self::GetScrambleId => "/chapter_view_template",
        }
    }

    fn token_secret(self) -> &'static str {
        match self {
            Self::GetScrambleId => APP_TOKEN_SECRET_2,
            Self::GetComic | Self::GetChapter => APP_TOKEN_SECRET,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JmTarget {
    Album(i64),
    Chapter(i64),
}

#[derive(Clone)]
pub struct JmClient {
    api_domain: String,
    api_client: Client,
    img_client: Client,
}

#[derive(Debug, Clone)]
pub struct JmDownloadOptions {
    pub download_dir: PathBuf,
    pub format: DownloadFormat,
    pub img_concurrency: usize,
    pub img_interval_sec: u64,
    pub img_retry_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JmComicSummary {
    pub id: i64,
    pub title: String,
    pub cover: String,
    pub image_count: i64,
}

#[derive(Debug)]
pub struct JmDownloadSuccess {
    pub comic_id: i64,
    pub title: String,
    pub zip_path: PathBuf,
    pub completed_images: usize,
    pub total_images: usize,
}

#[derive(Debug)]
pub struct JmDownloadFailure {
    pub comic_id: Option<i64>,
    pub title: String,
    pub reason: String,
    pub completed_images: usize,
    pub total_images: usize,
    pub download_dir: Option<PathBuf>,
    pub zip_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct JmDownloadPlan {
    comic_id: i64,
    title: String,
    cover: String,
    temp_dir: PathBuf,
    final_dir: PathBuf,
    chapters: Vec<JmChapterPlan>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JmMetadata {
    provider: &'static str,
    id: i64,
    title: String,
    cover: String,
    chapters: Vec<JmChapterMetadata>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JmChapterMetadata {
    chapter_id: i64,
    chapter_title: String,
    order: i64,
    image_count: usize,
}

#[derive(Debug, Clone)]
struct JmChapterPlan {
    chapter_id: i64,
    chapter_title: String,
    order: i64,
    dir: PathBuf,
    images: Vec<JmImagePlan>,
}

#[derive(Debug, Clone)]
struct JmImagePlan {
    url: String,
    save_path: PathBuf,
    block_num: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmResp {
    code: i64,
    data: Value,
    #[serde(default)]
    error_msg: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmSeries {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmComicResp {
    id: i64,
    name: String,
    #[serde(default)]
    series: Vec<JmSeries>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmChapterResp {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    images: Vec<String>,
    #[serde(rename = "series_id", default)]
    series_id: String,
}

pub fn default_api_domain() -> &'static str {
    DEFAULT_API_DOMAIN
}

pub fn looks_like_jm_domain(input: &str) -> bool {
    let trimmed = input.trim();
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
    JM_HOST_MARKERS
        .iter()
        .any(|marker| host.contains(marker))
}

pub fn looks_like_jm_target(input: &str) -> bool {
    parse_jm_target(input).is_some()
}

impl JmClient {
    pub fn new(api_domain: &str, proxy: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            api_domain: api_domain.to_string(),
            api_client: create_plain_client(proxy, Duration::from_secs(8))?,
            img_client: create_plain_client(proxy, Duration::from_secs(20))?,
        })
    }

    pub async fn fetch_summary(&self, input: &str) -> anyhow::Result<JmComicSummary> {
        let detail = self.fetch_detail(input).await?;
        Ok(detail.summary())
    }

    pub async fn download_target(
        &self,
        input: &str,
        options: &JmDownloadOptions,
    ) -> Result<JmDownloadSuccess, JmDownloadFailure> {
        let plan = self
            .build_download_plan(input, options)
            .await
            .map_err(|err| JmDownloadFailure {
                comic_id: None,
                title: input.to_string(),
                reason: format!("{err:#}"),
                completed_images: 0,
                total_images: 0,
                download_dir: None,
                zip_path: None,
            })?;

        if let Err(err) = prepare_plan_dirs(&plan) {
            return Err(JmDownloadFailure {
                comic_id: Some(plan.comic_id),
                title: plan.title.clone(),
                reason: format!("{err:#}"),
                completed_images: 0,
                total_images: plan.image_count(),
                download_dir: Some(plan.temp_dir.clone()),
                zip_path: None,
            });
        }

        if let Err(err) = save_metadata(&plan) {
            return Err(JmDownloadFailure {
                comic_id: Some(plan.comic_id),
                title: plan.title.clone(),
                reason: format!("{err:#}"),
                completed_images: 0,
                total_images: plan.image_count(),
                download_dir: Some(plan.temp_dir.clone()),
                zip_path: None,
            });
        }

        let total = plan.image_count();
        let completed = Arc::new(AtomicUsize::new(0));
        let total_bytes = Arc::new(AtomicU64::new(0));
        let semaphore = Arc::new(Semaphore::new(options.img_concurrency.max(1)));
        let mut join_set = JoinSet::new();

        for image in plan.images() {
            let client = self.clone();
            let image = image.clone();
            let options = options.clone();
            let completed = completed.clone();
            let total_bytes = total_bytes.clone();
            let semaphore = semaphore.clone();
            join_set.spawn(async move {
                let _permit = semaphore.acquire_owned().await?;
                let result = download_single_image(&client, &image, &options).await;
                if let Ok(bytes) = result.as_ref() {
                    total_bytes.fetch_add(*bytes as u64, Ordering::Relaxed);
                    let current = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    println!("[{current}/{total}] {}", image.url);
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

        let completed_images = completed.load(Ordering::Relaxed);
        if !failures.is_empty() {
            let sample = failures
                .into_iter()
                .next()
                .map(|err| err.to_string())
                .unwrap_or_else(|| "存在下载失败，但没有具体错误".to_string());
            return Err(JmDownloadFailure {
                comic_id: Some(plan.comic_id),
                title: plan.title.clone(),
                reason: format!("JMComic 下载不完整: {sample}"),
                completed_images,
                total_images: total,
                download_dir: Some(plan.temp_dir.clone()),
                zip_path: None,
            });
        }

        if let Err(err) = replace_dir(&plan.temp_dir, &plan.final_dir) {
            return Err(JmDownloadFailure {
                comic_id: Some(plan.comic_id),
                title: plan.title.clone(),
                reason: format!("{err:#}"),
                completed_images,
                total_images: total,
                download_dir: Some(plan.temp_dir.clone()),
                zip_path: None,
            });
        }

        let zip_path = match create_zip_archive_recursive(&plan.final_dir) {
            Ok(zip_path) => zip_path,
            Err(err) => {
                return Err(JmDownloadFailure {
                    comic_id: Some(plan.comic_id),
                    title: plan.title.clone(),
                    reason: format!("打包 zip 失败，目录已保留：{err:#}"),
                    completed_images,
                    total_images: total,
                    download_dir: Some(plan.final_dir.clone()),
                    zip_path: None,
                })
            }
        };

        if let Err(err) = std::fs::remove_dir_all(&plan.final_dir) {
            return Err(JmDownloadFailure {
                comic_id: Some(plan.comic_id),
                title: plan.title.clone(),
                reason: format!(
                    "删除已打包目录 `{}` 失败，目录已保留: {err}",
                    plan.final_dir.display()
                ),
                completed_images,
                total_images: total,
                download_dir: Some(plan.final_dir.clone()),
                zip_path: Some(zip_path),
            });
        }

        println!("zipped to {}", zip_path.display());
        println!("downloaded to {}", zip_path.display());

        Ok(JmDownloadSuccess {
            comic_id: plan.comic_id,
            title: plan.title,
            zip_path,
            completed_images,
            total_images: total,
        })
    }

    async fn build_download_plan(
        &self,
        input: &str,
        options: &JmDownloadOptions,
    ) -> anyhow::Result<JmDownloadPlan> {
        let detail = self.fetch_detail(input).await?;
        let safe_dir_name = build_download_dir_name(detail.comic.id, &detail.title());
        let temp_dir = options.download_dir.join(format!(".下载中-{safe_dir_name}"));
        let final_dir = options.download_dir.join(safe_dir_name);

        let mut chapters = Vec::new();
        for (index, chapter) in detail.chapters.iter().cloned().enumerate() {
            let order = (index + 1) as i64;
            let chapter_title = detail.chapter_title(&chapter, order);
            let chapter_dir_name = filename_filter(&format!("{order:03} {chapter_title}"));
            let chapter_dir = temp_dir.join(chapter_dir_name);
            let scramble_id = self.get_scramble_id(chapter.id).await?;
            let images = chapter
                .images
                .into_iter()
                .enumerate()
                .filter_map(|(image_index, filename)| {
                    build_image_plan(
                        chapter.id,
                        scramble_id,
                        image_index,
                        &filename,
                        &chapter_dir,
                        options.format,
                    )
                })
                .collect::<Vec<_>>();
            chapters.push(JmChapterPlan {
                chapter_id: chapter.id,
                chapter_title,
                order,
                dir: chapter_dir,
                images,
            });
        }

        let image_count = chapters
            .iter()
            .map(|chapter| chapter.images.len())
            .sum::<usize>();
        if image_count == 0 {
            return Err(anyhow!(
                "JMComic `{}` 没有可下载图片，可能章节数据为空或图片格式暂不支持",
                detail.title()
            ));
        }

        Ok(JmDownloadPlan {
            comic_id: detail.comic.id,
            title: detail.title(),
            cover: detail.cover(),
            temp_dir,
            final_dir,
            chapters,
        })
    }

    async fn fetch_detail(&self, input: &str) -> anyhow::Result<JmComicDetail> {
        let target = parse_jm_target(input)
            .with_context(|| format!("`{input}` 不是支持的 JMComic URL"))?;

        match target {
            JmTarget::Album(album_id) => {
                let comic = self.get_comic(album_id).await?;
                let selected = selected_chapter_ids(&comic, None);
                let chapters = self.fetch_chapters(selected).await?;
                Ok(JmComicDetail {
                    comic,
                    selected_chapter_id: None,
                    chapters,
                })
            }
            JmTarget::Chapter(chapter_id) => {
                let chapter = self.get_chapter(chapter_id).await?;
                let album_id = chapter
                    .series_id
                    .parse::<i64>()
                    .unwrap_or(chapter_id);
                let comic = self.get_comic(album_id).await?;
                Ok(JmComicDetail {
                    comic,
                    selected_chapter_id: Some(chapter_id),
                    chapters: vec![chapter],
                })
            }
        }
    }

    async fn fetch_chapters(&self, chapter_ids: Vec<i64>) -> anyhow::Result<Vec<JmChapterResp>> {
        let mut chapters = Vec::new();
        for chapter_id in chapter_ids {
            chapters.push(self.get_chapter(chapter_id).await?);
        }
        Ok(chapters)
    }

    async fn get_comic(&self, album_id: i64) -> anyhow::Result<JmComicResp> {
        self.get_encrypted(
            ApiPath::GetComic,
            &[("id", album_id.to_string())],
            "获取 JMComic 漫画失败",
        )
        .await
    }

    async fn get_chapter(&self, chapter_id: i64) -> anyhow::Result<JmChapterResp> {
        self.get_encrypted(
            ApiPath::GetChapter,
            &[("id", chapter_id.to_string())],
            "获取 JMComic 章节失败",
        )
        .await
    }

    async fn get_scramble_id(&self, chapter_id: i64) -> anyhow::Result<i64> {
        let ts = now_ts()?;
        let query = [
            ("id", chapter_id.to_string()),
            ("v", ts.to_string()),
            ("mode", "vertical".to_string()),
            ("page", "0".to_string()),
            ("app_img_shunt", "1".to_string()),
            ("express", "off".to_string()),
        ];
        let body = self
            .request_text(ApiPath::GetScrambleId, &query, ts)
            .await
            .context("获取 JMComic scramble_id 失败")?;
        Ok(body
            .split("var scramble_id = ")
            .nth(1)
            .and_then(|value| value.split(';').next())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(220_980))
    }

    async fn get_encrypted<T>(
        &self,
        path: ApiPath,
        query: &[(&str, String)],
        context: &str,
    ) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        let ts = now_ts()?;
        let body = self.request_text(path, query, ts).await?;
        let jm_resp =
            serde_json::from_str::<JmResp>(&body).with_context(|| format!("解析响应失败: {body}"))?;
        if jm_resp.code != 200 {
            return Err(anyhow!("{context}: {} ({jm_resp:?})", jm_resp.error_msg));
        }
        let data = jm_resp
            .data
            .as_str()
            .with_context(|| format!("{context}: data 字段不是字符串 ({jm_resp:?})"))?;
        let data = decrypt_data(ts, data)?;
        serde_json::from_str::<T>(&data)
            .with_context(|| format!("{context}: 解析解密数据失败: {data}"))
    }

    async fn request_text(
        &self,
        path: ApiPath,
        query: &[(&str, String)],
        ts: u64,
    ) -> anyhow::Result<String> {
        let token = md5_hex(&format!("{ts}{}", path.token_secret()));
        let tokenparam = format!("{ts},{APP_VERSION}");
        let url = format!("https://{}{}", self.api_domain, path.as_str());
        let response = self
            .api_client
            .get(url)
            .header("token", token)
            .header("tokenparam", tokenparam)
            .header("user-agent", desktop_user_agent())
            .query(query)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }
        Ok(body)
    }

    async fn get_img_data_and_format(&self, url: &str) -> anyhow::Result<(Bytes, ImageFormat)> {
        let response = self
            .img_client
            .get(url)
            .header("user-agent", desktop_user_agent())
            .send()
            .await?;
        let status = response.status();
        if status != StatusCode::OK {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("下载图片 `{url}` 失败，预料之外的状态码({status}): {body}"));
        }

        let mut image_data = response.bytes().await?;
        if image_data.is_empty() {
            let ts = now_ts()?;
            let response = self
                .img_client
                .get(url)
                .query(&[("ts", ts.to_string())])
                .header("user-agent", desktop_user_agent())
                .send()
                .await?;
            let status = response.status();
            if status != StatusCode::OK {
                let body = response.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "下载图片 `{url}` 失败，预料之外的状态码({status}): {body}"
                ));
            }
            image_data = response.bytes().await?;
        }

        let format = image::guess_format(&image_data)
            .context("无法从图片数据中判断格式，可能数据不完整或已损坏")?;
        Ok((image_data, format))
    }
}

#[derive(Debug, Clone)]
struct JmComicDetail {
    comic: JmComicResp,
    selected_chapter_id: Option<i64>,
    chapters: Vec<JmChapterResp>,
}

impl JmComicDetail {
    fn title(&self) -> String {
        if let Some(chapter_id) = self.selected_chapter_id {
            let chapter_title = self
                .chapters
                .iter()
                .find(|chapter| chapter.id == chapter_id)
                .map(|chapter| self.chapter_title(chapter, 1))
                .unwrap_or_else(|| format!("章节 {chapter_id}"));
            return filename_filter(&format!("{} - {chapter_title}", self.comic.name));
        }
        filename_filter(&self.comic.name)
    }

    fn cover(&self) -> String {
        format!("https://{COVER_DOMAIN}/media/albums/{}.jpg", self.comic.id)
    }

    fn image_count(&self) -> i64 {
        self.chapters
            .iter()
            .map(|chapter| chapter.images.len() as i64)
            .sum()
    }

    fn summary(&self) -> JmComicSummary {
        JmComicSummary {
            id: self.comic.id,
            title: self.title(),
            cover: self.cover(),
            image_count: self.image_count(),
        }
    }

    fn chapter_title(&self, chapter: &JmChapterResp, fallback_order: i64) -> String {
        self.comic
            .series
            .iter()
            .position(|series| series.id.parse::<i64>().ok() == Some(chapter.id))
            .map(|index| {
                let order = index as i64 + 1;
                let mut title = format!("第{order}话");
                if !self.comic.series[index].name.is_empty() {
                    title.push(' ');
                    title.push_str(&self.comic.series[index].name);
                }
                title
            })
            .unwrap_or_else(|| {
                if chapter.name.is_empty() {
                    format!("第{fallback_order}话")
                } else {
                    chapter.name.clone()
                }
            })
    }
}

impl JmDownloadPlan {
    fn image_count(&self) -> usize {
        self.chapters
            .iter()
            .map(|chapter| chapter.images.len())
            .sum()
    }

    fn images(&self) -> impl Iterator<Item = &JmImagePlan> {
        self.chapters.iter().flat_map(|chapter| chapter.images.iter())
    }
}

fn selected_chapter_ids(comic: &JmComicResp, selected_chapter_id: Option<i64>) -> Vec<i64> {
    if let Some(chapter_id) = selected_chapter_id {
        return vec![chapter_id];
    }
    let ids = comic
        .series
        .iter()
        .filter_map(|series| series.id.parse::<i64>().ok())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        vec![comic.id]
    } else {
        ids
    }
}

fn build_image_plan(
    chapter_id: i64,
    scramble_id: i64,
    image_index: usize,
    filename: &str,
    chapter_dir: &Path,
    download_format: DownloadFormat,
) -> Option<JmImagePlan> {
    let file_path = Path::new(filename);
    let src_ext = file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "webp".to_string());
    let url = format!("https://{IMAGE_DOMAIN}/media/photos/{chapter_id}/{filename}");
    let filename_without_ext = file_path.file_stem()?.to_str()?;
    let block_num = if src_ext == "gif" {
        0
    } else if src_ext == "webp" {
        calculate_block_num(scramble_id, chapter_id, filename_without_ext)
    } else {
        0
    };
    let save_ext = if src_ext == "gif" {
        "gif".to_string()
    } else {
        download_format
            .extension()
            .map(|ext| ext.to_string())
            .unwrap_or_else(|| src_ext.clone())
    };
    let save_path = chapter_dir.join(format!("{:04}.{save_ext}", image_index + 1));
    Some(JmImagePlan {
        url,
        save_path,
        block_num,
    })
}

fn parse_jm_target(input: &str) -> Option<JmTarget> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(id) = lower
        .strip_prefix("jm:")
        .or_else(|| lower.strip_prefix("jm"))
        .and_then(parse_positive_i64)
    {
        return Some(JmTarget::Album(id));
    }

    let normalized_url = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = Url::parse(&normalized_url).ok()?;
    if !looks_like_jm_domain(url.as_str()) {
        return None;
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();

    for pair in segments.windows(2) {
        let key = pair[0].to_ascii_lowercase();
        if matches!(key.as_str(), "album" | "albums") {
            if let Some(id) = parse_positive_i64(pair[1]) {
                return Some(JmTarget::Album(id));
            }
        }
        if matches!(key.as_str(), "photo" | "chapter" | "photos") {
            if let Some(id) = parse_positive_i64(pair[1]) {
                return Some(JmTarget::Chapter(id));
            }
        }
    }

    for (key, value) in url.query_pairs() {
        let key = key.to_ascii_lowercase();
        if matches!(key.as_str(), "album" | "aid" | "album_id") {
            if let Some(id) = parse_positive_i64(&value) {
                return Some(JmTarget::Album(id));
            }
        }
        if matches!(key.as_str(), "chapter" | "chapter_id" | "photo") {
            if let Some(id) = parse_positive_i64(&value) {
                return Some(JmTarget::Chapter(id));
            }
        }
    }

    segments
        .iter()
        .find_map(|segment| parse_positive_i64(segment))
        .map(JmTarget::Album)
}

fn parse_positive_i64(value: &str) -> Option<i64> {
    if value.is_empty() || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    value.parse::<i64>().ok().filter(|id| *id > 0)
}

fn create_plain_client(proxy: Option<&str>, timeout: Duration) -> anyhow::Result<Client> {
    let mut builder = reqwest::ClientBuilder::new()
        .use_rustls_tls()
        .timeout(timeout);
    if let Some(proxy) = proxy {
        builder = builder.proxy(Proxy::all(proxy).with_context(|| format!("无效代理 `{proxy}`"))?);
    }
    builder.build().context("创建 JMComic HTTP 客户端失败")
}

fn now_ts() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn decrypt_data(ts: u64, data: &str) -> anyhow::Result<String> {
    let encrypted = general_purpose::STANDARD.decode(data)?;
    if encrypted.is_empty() || encrypted.len() % 16 != 0 {
        return Err(anyhow!("JMComic 加密数据长度不是有效的 AES 分块长度"));
    }

    let key = md5_hex(&format!("{ts}{APP_DATA_SECRET}"));
    let cipher = Aes256::new(GenericArray::from_slice(key.as_bytes()));
    let decrypted_with_padding = encrypted
        .chunks(16)
        .map(GenericArray::clone_from_slice)
        .flat_map(|mut block| {
            cipher.decrypt_block(&mut block);
            block.to_vec()
        })
        .collect::<Vec<_>>();
    let padding_length = decrypted_with_padding
        .last()
        .copied()
        .context("JMComic 解密结果为空")? as usize;
    if padding_length == 0
        || padding_length > 16
        || padding_length > decrypted_with_padding.len()
        || !decrypted_with_padding[decrypted_with_padding.len() - padding_length..]
            .iter()
            .all(|byte| *byte as usize == padding_length)
    {
        return Err(anyhow!("JMComic 解密结果的 PKCS#7 padding 无效"));
    }
    let decrypted =
        decrypted_with_padding[..decrypted_with_padding.len() - padding_length].to_vec();
    String::from_utf8(decrypted).context("JMComic 解密结果不是 UTF-8")
}

fn prepare_plan_dirs(plan: &JmDownloadPlan) -> anyhow::Result<()> {
    std::fs::create_dir_all(&plan.temp_dir)
        .with_context(|| format!("创建临时下载目录 `{}` 失败", plan.temp_dir.display()))?;
    clean_temp_root_dir(plan)?;
    for chapter in &plan.chapters {
        std::fs::create_dir_all(&chapter.dir)
            .with_context(|| format!("创建章节目录 `{}` 失败", chapter.dir.display()))?;
        clean_temp_download_dir(&chapter.dir, &chapter.images)?;
    }
    Ok(())
}

fn clean_temp_root_dir(plan: &JmDownloadPlan) -> anyhow::Result<()> {
    let mut keep_names = plan
        .chapters
        .iter()
        .filter_map(|chapter| chapter.dir.file_name().map(OsString::from))
        .collect::<HashSet<_>>();
    keep_names.insert(OsString::from("元数据.json"));

    for entry in std::fs::read_dir(&plan.temp_dir)
        .with_context(|| format!("读取目录 `{}` 失败", plan.temp_dir.display()))?
    {
        let path = entry?.path();
        let should_delete = path
            .file_name()
            .is_some_and(|name| !keep_names.contains(name));
        if should_delete {
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("删除旧目录 `{}` 失败", path.display()))?;
            } else if path.is_file() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("删除旧文件 `{}` 失败", path.display()))?;
            }
        }
    }
    Ok(())
}

fn clean_temp_download_dir(dir: &Path, images: &[JmImagePlan]) -> anyhow::Result<()> {
    let mut keep_names = images
        .iter()
        .filter_map(|image| image.save_path.file_name().map(OsString::from))
        .collect::<HashSet<_>>();
    keep_names.insert(OsString::from("章节元数据.json"));

    for entry in
        std::fs::read_dir(dir).with_context(|| format!("读取目录 `{}` 失败", dir.display()))?
    {
        let path = entry?.path();
        let should_delete = path
            .file_name()
            .is_some_and(|name| !keep_names.contains(name));
        if should_delete {
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("删除旧目录 `{}` 失败", path.display()))?;
            } else if path.is_file() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("删除旧文件 `{}` 失败", path.display()))?;
            }
        }
    }
    Ok(())
}

fn save_metadata(plan: &JmDownloadPlan) -> anyhow::Result<()> {
    let metadata = JmMetadata {
        provider: "jmcomic",
        id: plan.comic_id,
        title: plan.title.clone(),
        cover: plan.cover.clone(),
        chapters: plan
            .chapters
            .iter()
            .map(|chapter| JmChapterMetadata {
                chapter_id: chapter.chapter_id,
                chapter_title: chapter.chapter_title.clone(),
                order: chapter.order,
                image_count: chapter.images.len(),
            })
            .collect(),
    };
    let metadata_path = plan.temp_dir.join("元数据.json");
    std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)
        .with_context(|| format!("写入元数据文件 `{}` 失败", metadata_path.display()))?;

    for chapter in &plan.chapters {
        let chapter_metadata = JmChapterMetadata {
            chapter_id: chapter.chapter_id,
            chapter_title: chapter.chapter_title.clone(),
            order: chapter.order,
            image_count: chapter.images.len(),
        };
        let metadata_path = chapter.dir.join("章节元数据.json");
        std::fs::write(&metadata_path, serde_json::to_string_pretty(&chapter_metadata)?)
            .with_context(|| format!("写入章节元数据文件 `{}` 失败", metadata_path.display()))?;
    }
    Ok(())
}

async fn download_single_image(
    client: &JmClient,
    image: &JmImagePlan,
    options: &JmDownloadOptions,
) -> anyhow::Result<usize> {
    if image.save_path.exists() {
        return Ok(0);
    }

    let mut last_err = None;
    let mut data_and_format = None;
    for attempt in 0..=options.img_retry_count {
        match client.get_img_data_and_format(&image.url).await {
            Ok(result) => {
                data_and_format = Some(result);
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

    let (img_data, src_format) = data_and_format.ok_or_else(|| {
        let err = last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "未知错误".to_string());
        anyhow!(
            "下载 JMComic 图片 `{}` 失败，已重试 {} 次: {err}",
            image.url,
            options.img_retry_count
        )
    })?;

    let bytes = img_data.len();
    save_img(
        &image.save_path,
        options.format,
        image.block_num,
        img_data,
        src_format,
    )
    .await?;
    Ok(bytes)
}

async fn save_img(
    save_path: &Path,
    download_format: DownloadFormat,
    block_num: u32,
    src_img_data: Bytes,
    src_format: ImageFormat,
) -> anyhow::Result<()> {
    if block_num == 0
        && (src_format == ImageFormat::Gif
            || download_format == DownloadFormat::Original
            || download_format.to_image_format() == Some(src_format))
    {
        std::fs::write(save_path, &src_img_data)
            .with_context(|| format!("保存图片 `{}` 失败", save_path.display()))?;
        return Ok(());
    }

    let save_path = save_path.to_path_buf();
    let process_img = move || -> anyhow::Result<()> {
        let mut src_img = image::load_from_memory(&src_img_data)
            .context("解码 JMComic 图片失败")?
            .to_rgb8();
        let dst_img = if block_num == 0 {
            src_img
        } else {
            stitch_img(&mut src_img, block_num)
        };
        let target_format = download_format.to_image_format().unwrap_or(src_format);
        let mut encoded = Vec::new();
        match target_format {
            ImageFormat::Jpeg => dst_img.write_to(&mut Cursor::new(&mut encoded), ImageFormat::Jpeg)?,
            ImageFormat::Png => dst_img.write_to(&mut Cursor::new(&mut encoded), ImageFormat::Png)?,
            ImageFormat::WebP => dst_img.write_to(&mut Cursor::new(&mut encoded), ImageFormat::WebP)?,
            _ => return Err(anyhow!("不支持的图片格式: {target_format:?}")),
        }
        std::fs::write(&save_path, encoded)
            .with_context(|| format!("保存图片 `{}` 失败", save_path.display()))?;
        Ok(())
    };

    let (sender, receiver) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    rayon::spawn(move || {
        let _ = sender.send(process_img());
    });
    receiver.await?
}

fn stitch_img(src_img: &mut RgbImage, block_num: u32) -> RgbImage {
    let (width, height) = src_img.dimensions();
    if block_num <= 1 || block_num > height {
        return src_img.clone();
    }

    let mut stitched_img = image::ImageBuffer::new(width, height);
    let remainder_height = height % block_num;
    for i in 0..block_num {
        let mut block_height = height / block_num;
        let src_img_y_start = height - (block_height * (i + 1)) - remainder_height;
        let mut dst_img_y_start = block_height * i;
        if i == 0 {
            block_height += remainder_height;
        } else {
            dst_img_y_start += remainder_height;
        }
        for y in 0..block_height {
            let src_y = src_img_y_start + y;
            let dst_y = dst_img_y_start + y;
            for x in 0..width {
                stitched_img.put_pixel(x, dst_y, *src_img.get_pixel(x, src_y));
            }
        }
    }
    stitched_img
}

fn calculate_block_num(scramble_id: i64, id: i64, filename: &str) -> u32 {
    if id < scramble_id {
        0
    } else if id < 268_850 {
        10
    } else {
        let x = if id < 421_926 { 10 } else { 8 };
        let hash = md5_hex(&format!("{id}{filename}"));
        let mut block_num = hash
            .chars()
            .last()
            .map(u32::from)
            .unwrap_or_default();
        block_num %= x;
        block_num * 2 + 2
    }
}

fn replace_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    if to.exists() {
        std::fs::remove_dir_all(to)
            .with_context(|| format!("删除旧目录 `{}` 失败", to.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("将 `{}` 重命名为 `{}` 失败", from.display(), to.display()))
}

fn create_zip_archive_recursive(download_dir: &Path) -> anyhow::Result<PathBuf> {
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

    let mut file_paths = Vec::new();
    let mut dirs = VecDeque::from([download_dir.to_path_buf()]);
    while let Some(dir) = dirs.pop_front() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("读取目录 `{}` 失败", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push_back(path);
            } else if path.is_file() {
                file_paths.push(path);
            }
        }
    }
    file_paths.sort();

    let zip_file = std::fs::File::create(&zip_path)
        .with_context(|| format!("创建 zip 文件 `{}` 失败", zip_path.display()))?;
    let mut zip_writer = zip::ZipWriter::new(zip_file);
    for path in file_paths {
        let relative = path
            .strip_prefix(download_dir)
            .with_context(|| format!("计算 `{}` 的相对路径失败", path.display()))?;
        let filename = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        zip_writer
            .start_file(
                &filename,
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
    let sanitized = if sanitized.is_empty() {
        format!("JMComic {comic_id}")
    } else {
        format!("{sanitized} [JM{comic_id}]")
    };
    if sanitized.len() <= MAX_NAME_BYTES {
        return sanitized;
    }

    let suffix = format!(" [JM{comic_id}]");
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

fn desktop_user_agent() -> &'static str {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36"
}

#[cfg(test)]
mod tests {
    use super::{parse_jm_target, JmTarget};

    #[test]
    fn parses_album_urls() {
        assert_eq!(
            parse_jm_target("https://18comic.vip/album/123456/title"),
            Some(JmTarget::Album(123456))
        );
        assert_eq!(
            parse_jm_target("jmcomic1.me/album/654321"),
            Some(JmTarget::Album(654321))
        );
    }

    #[test]
    fn parses_chapter_urls() {
        assert_eq!(
            parse_jm_target("https://18comic.vip/photo/987654"),
            Some(JmTarget::Chapter(987654))
        );
        assert_eq!(
            parse_jm_target("https://cdn-msp2.jmapiproxy2.cc/media/photos/456789/00001.webp"),
            Some(JmTarget::Chapter(456789))
        );
    }

    #[test]
    fn parses_explicit_jm_ids_without_stealing_plain_numbers() {
        assert_eq!(parse_jm_target("jm:112233"), Some(JmTarget::Album(112233)));
        assert_eq!(parse_jm_target("JM112233"), Some(JmTarget::Album(112233)));
        assert_eq!(parse_jm_target("112233"), None);
    }

    #[test]
    fn detects_jm_domains_without_matching_wn_domains() {
        assert!(super::looks_like_jm_domain("www.cdnhth.cc"));
        assert!(super::looks_like_jm_domain("https://18comic.vip/album/1"));
        assert!(!super::looks_like_jm_domain("www.wn07.ru"));
        assert!(!super::looks_like_jm_domain("www.wnacg.com"));
    }
}
