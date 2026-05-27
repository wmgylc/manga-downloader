# Manga Downloader Service

一个用于 WNACG 与 JMComic 下载的网页与 HTTP 服务。

## 当前形态

服务启动后会同时提供：

- 网页界面
- API 接口
- 后台下载任务

默认入口：

```text
http://127.0.0.1:3001
```

API 文档页：

```text
http://127.0.0.1:3001/api-doc.html
```

健康检查：

```text
http://127.0.0.1:3001/api/health
```

## 目录结构

```text
.
├── Dockerfile.cli
├── THIRD_PARTY_NOTICES.md
├── docker-compose.cli.yml
├── docker-config/
├── public/
│   ├── api-doc.html
│   └── api-doc.md
├── src/
│   ├── App.tsx
│   ├── WebDownloadDashboard.tsx
│   ├── global.css
│   └── main.ts
└── src-tauri/
    ├── Cargo.toml
    └── src/
        ├── bin/manga-api.rs
        ├── bin/manga-cli.rs
        ├── cli.rs
        ├── config.rs
        ├── errors.rs
        ├── extensions.rs
        ├── jmcomic.rs
        ├── lib.rs
        ├── types/
        └── utils.rs
```

## Docker 运行

启动：

```bash
docker compose -f docker-compose.cli.yml up -d --build
```

默认配置：

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

默认挂载：

- 宿主机 `./docker-config` -> 容器 `/config`
- 宿主机下载目录 -> 容器 `/data`

任务历史默认持久化到：

```text
/data/manga-tasks.sqlite
```

如果发现旧的：

```text
/data/manga-tasks.json
```

服务会在首次启动时自动导入。

## API 概览

- `GET /api/health`
- `GET /api/search/keyword?q=<关键词>&page=1`
- `GET /api/search/tag?tag=<标签>&page=1`
- `GET /api/comic?target=<漫画ID或URL>`
- `GET /api/download?target=<漫画ID或URL>`
- `GET /api/download/start?target=<漫画ID或URL>`
- `GET /api/tasks`
- `GET /api/tasks/<id>`

支持的 `target` 输入：

- 漫画详情页
- 分页详情页
- 漫画任意一页
- 纯数字漫画 ID
- JMComic / 18comic 漫画或章节 URL

更完整的接口说明见：

[`public/api-doc.md`](public/api-doc.md)

## 下载行为

- 下载完成后自动打包为 `.zip`
- 原始图片目录会删除
- 成功和失败都会触发通知
- 失败时会带失败原因、完成张数和总张数

## 本地开发

前端开发：

```bash
corepack enable
pnpm install
pnpm dev
```

构建前端：

```bash
pnpm build
```

当前仓库默认面向 Docker 部署和网页/API 使用。
