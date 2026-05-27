# Manga Downloader

Manga Downloader 是一个面向 Docker 部署的漫画下载服务，提供网页任务面板、HTTP API 和后台下载队列。当前支持 WNACG 与 JMComic/18comic 输入，下载完成后会自动打包为 `.zip`，并保留任务历史。

## 功能特性

- 网页端提交下载任务，查看下载中、下载完成、下载失败任务。
- HTTP API 支持搜索、解析漫画信息、创建下载任务、查询任务状态。
- 支持 WNACG 漫画 ID、详情页、分页页、任意阅读页。
- 支持 JMComic/18comic 的 album、photo、chapter、CDN 图片链接与 `jm:<id>` 输入。
- 下载任务异步执行，前端自动刷新任务进度。
- 任务历史使用 SQLite 持久化，容器重建后继续保留。
- 部署脚本会在重建前自动备份任务数据库。
- 支持 Bark/Webhook 下载完成通知。

## 快速开始

默认 Docker Compose 会启动一个服务：

- 容器名：`manga-downloader`
- 镜像名：`manga-downloader:latest`
- Web/API 端口：`3001`
- 容器内服务端口：`3000`

启动或更新：

```bash
docker compose -f docker-compose.cli.yml up -d --build
```

访问网页：

```text
http://127.0.0.1:3001/
```

健康检查：

```bash
curl http://127.0.0.1:3001/api/health
```

## Docker 配置

`docker-compose.cli.yml` 默认配置：

```yaml
services:
  manga-downloader:
    image: manga-downloader:latest
    build:
      context: .
      dockerfile: Dockerfile.cli
    container_name: manga-downloader
    restart: always
    ports:
      - "3001:3000"
    volumes:
      - /vol4/1000/media/porn/manga:/data
      - ./docker-config:/config
      - ./data/tasks:/var/lib/manga-downloader
```

常用挂载：

| 宿主机路径 | 容器路径 | 用途 |
| --- | --- | --- |
| `/vol4/1000/media/porn/manga` | `/data` | 下载结果目录 |
| `./docker-config` | `/config` | 服务配置 |
| `./data/tasks` | `/var/lib/manga-downloader` | 任务历史数据库 |

任务数据库位置：

```text
./data/tasks/manga-tasks.sqlite
```

每次运行 `deploy.sh` 前会自动备份到：

```text
./data/backups/manga-tasks.<YYYYMMDD-HHMMSS>.sqlite
```

`data/` 已加入 `.gitignore` 和 `.dockerignore`，不要把它作为源码同步或镜像构建内容。

## 配置文件

默认配置文件：

```text
docker-config/manga-cli.json
```

示例：

```json
{
  "webhook_url": "",
  "bark_url": "",
  "default_download_dir": "/data",
  "default_img_concurrency": 5,
  "default_img_interval_sec": 1,
  "default_img_retry_count": 2,
  "default_task_retry_count": 1
}
```

## 支持的输入

WNACG：

- 纯数字漫画 ID，例如 `328415`
- 漫画详情页，例如 `https://www.wnacg.com/photos-index-aid-328415.html`
- 分页详情页，例如 `https://www.wnacg.com/photos-index-page-1-aid-328415.html`
- 任意阅读页，例如 `https://www.wnacg.com/photos-view-id-27566986.html`

JMComic / 18comic：

- `jm:1435054`
- `https://18comic.vip/album/1435054`
- `https://18comic.vip/photo/1435054`
- `https://18comic.vip/chapter/<chapter_id>`
- `https://cdn-msp2.jmapiproxy2.cc/media/photos/<chapter_id>/00001.webp`

其中 `/photo/<id>` 会按 album ID 处理，兼容 18comic 页面常见链接形态。

## API 概览

所有 API 同时支持 `/api/...` 前缀和根路径别名，推荐使用 `/api/...`。

| 接口 | 说明 |
| --- | --- |
| `GET /api/health` | 健康检查 |
| `GET /api/search/keyword?q=<关键词>&page=1` | WNACG 关键词搜索 |
| `GET /api/search/tag?tag=<标签>&page=1` | WNACG 标签搜索 |
| `GET /api/comic?target=<URL或ID>` | 解析漫画标题、封面、页数 |
| `GET /api/download/start?target=<URL或ID>` | 创建下载任务 |
| `GET /api/tasks` | 查询任务列表 |
| `GET /api/tasks/<id>` | 查询单个任务 |

创建下载任务：

```bash
curl "http://127.0.0.1:3001/api/download/start?target=https://18comic.vip/photo/1435054"
```

查询任务：

```bash
curl "http://127.0.0.1:3001/api/tasks"
```

更完整的接口说明见 [public/api-doc.md](public/api-doc.md)。

## 目录结构

```text
.
├── Dockerfile.cli
├── docker-compose.cli.yml
├── deploy.sh
├── docker-config/
│   └── manga-cli.json
├── public/
│   ├── api-doc.html
│   └── api-doc.md
├── src/
│   └── WebDownloadDashboard.tsx
└── src-tauri/
    ├── Cargo.toml
    └── src/
        ├── bin/manga-api.rs
        ├── bin/manga-cli.rs
        ├── cli.rs
        ├── jmcomic.rs
        └── types/
```

## 本地开发

安装前端依赖：

```bash
corepack enable
pnpm install
```

启动前端开发服务：

```bash
pnpm dev
```

构建前端：

```bash
pnpm build
```

构建 Docker 镜像：

```bash
DOCKER_BUILDKIT=1 docker compose -f docker-compose.cli.yml build
```

Rust 后端二进制：

- `manga-api`：Web/API 服务入口
- `manga-cli`：搜索、解析、下载命令行入口

## 部署注意事项

- 不要删除或覆盖 `data/tasks/manga-tasks.sqlite`，它保存下载记录。
- 使用 `deploy.sh` 部署时会自动备份任务库。
- 如果手动同步代码到服务器，请排除 `data/`、`node_modules/`、`dist/`、`src-tauri/target/`。
- 容器重建不会删除下载结果和任务历史，前提是 `/data` 与 `/var/lib/manga-downloader` 已正确挂载。

## 第三方说明

JMComic API 与图片恢复相关逻辑参考了第三方项目，详见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
