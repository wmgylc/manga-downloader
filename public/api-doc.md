# Manga Downloader API 文档

这个服务同时提供网页界面和 HTTP API。

基础地址：

```text
http://10.10.10.206:3001
```

API 前缀：

```text
/api
```

## 输入规则

以下几种输入都支持，服务会自动归一到对应漫画：

- 漫画详情页，例如 `https://www.wnacg.com/photos-index-aid-328415.html`
- 分页详情页，例如 `https://www.wnacg.com/photos-index-page-1-aid-328415.html`
- 漫画任意一页，例如 `https://www.wnacg.com/photos-view-id-27566986.html`
- 纯数字漫画 ID，例如 `328415`
- JMComic / 18comic 漫画或章节 URL，例如 `https://18comic.vip/album/123456`

## 接口总览

| 接口 | 说明 |
| --- | --- |
| `GET /api/health` | 健康检查 |
| `GET /api/search/keyword?q=关键词&page=1` | 按关键词搜索 |
| `GET /api/search/tag?tag=标签&page=1` | 按标签搜索 |
| `GET /api/comic?target=URL或ID` | 解析漫画信息 |
| `POST /api/download` | 解析漫画信息并创建异步下载任务 |
| `GET /api/download?target=URL或ID` | 兼容旧调用，创建下载任务 |
| `GET /api/download/start?target=URL或ID` | 兼容旧调用，创建下载任务 |
| `GET /api/task/{task_id}` | 获取单个任务状态、进度和 message |
| `POST /api/task/{task_id}/cancel` | 取消排队中或正在下载的任务 |
| `GET /api/tasks` | 获取任务列表 |
| `GET /api/tasks/{taskId}` | 兼容旧路径，获取单个任务详情 |

## 1. 健康检查

```text
GET /api/health
```

示例：

```bash
curl "http://10.10.10.206:3001/api/health"
```

## 2. 关键词搜索

```text
GET /api/search/keyword?q=<关键词>&page=<页码>
```

示例：

```bash
curl "http://10.10.10.206:3001/api/search/keyword?q=fate&page=1"
```

## 3. 标签搜索

```text
GET /api/search/tag?tag=<标签>&page=<页码>
```

示例：

```bash
curl "http://10.10.10.206:3001/api/search/tag?tag=%E4%BA%BA%E5%A6%BB&page=1"
```

## 4. 解析漫画信息

```text
GET /api/comic?target=<URL或ID>
```

适用于先确认标题、封面、页数，再决定是否下载。

示例：

```bash
curl "http://10.10.10.206:3001/api/comic?target=https://www.wnacg.com/photos-view-id-27566986.html"
```

## 5. 创建下载任务

推荐使用 POST。接口会先解析漫画信息，再创建排队任务并立即返回；不会等待图片下载完成。漫画页数通过 `totalPages` 和 `progress` 返回。

```text
POST /api/download
Content-Type: application/json
```

请求可以使用 `url`、`target` 或 `id` 字段：

```bash
curl -X POST "http://10.10.10.206:3001/api/download" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.wnacg.com/photos-index-aid-349565.html"}'
```

成功入队时返回 HTTP `202`，示例：

```json
{
  "task_id": "7e2fef4c-7aaf-4bdb-8777-4a7194461a0e",
  "id": "7e2fef4c-7aaf-4bdb-8777-4a7194461a0e",
  "status": "queued",
  "title": "漫画标题",
  "totalPages": 66,
  "completedPages": 0,
  "progress": { "completed": 0, "total": 66, "percent": 0 },
  "message": "漫画标题"
}
```

漫画信息解析失败时不会创建任务，响应中包含 `error` 和 `message`。

如果 `url`/`target` 是 WNACG 合集页（可带或不带分页后缀），例如：

```bash
curl -X POST "http://10.10.10.206:3001/api/download" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.wnacg.com/photos-index-aid-397051-page-1.html"}'
```

也支持无分页后缀的合集入口：`https://www.wnacg.com/photos-index-aid-397051.html`。

接口会自动遍历合集分页并把各分集分别加入任务队列。合集响应包含 `collectionId`、`collectionTitle`、`totalEpisodes`、`task_ids` 和每个分集对应的 `tasks`；每个任务仍可单独通过 `GET /api/task/{task_id}` 查询或取消。

```json
{
  "ok": true,
  "collectionId": 397051,
  "collectionTitle": "合集标题",
  "status": "queued",
  "totalEpisodes": 34,
  "task_ids": ["task-id-1", "task-id-2"],
  "tasks": [
    { "task_id": "task-id-1", "status": "queued", "title": "分集一", "message": "分集一" },
    { "task_id": "task-id-2", "status": "queued", "title": "分集二", "message": "分集二" }
  ],
  "message": "合集标题"
}
```

合集的 `message` 只返回合集标题；单个分集任务的 `message` 只返回分集标题。所有下载任务按全局 FIFO 队列串行执行；每个实际下载结束后，至少等待 60 秒再启动下一项。单项下载中的图片并发上限保持不变。

```text
GET /api/download?target=<URL或ID>
```

或者：

```text
GET /api/download/start?target=<URL或ID>
```

说明：

- 旧 GET 路径保留兼容，新接入请使用 POST
- 创建后会进入统一任务中心
- 下载完成后会自动打包为 zip
- 原始下载目录会删除，只保留 zip

## 6. 查询任务列表

```text
GET /api/tasks
```

示例：

```bash
curl "http://10.10.10.206:3001/api/tasks"
```

任务状态：

- `queued`
- `downloading`
- `completed`
- `failed`
- `cancelled`

常见字段：

- `id`
- `target`
- `title`
- `cover`
- `completedPages`
- `totalPages`
- `error`
- `zipPath`
- `createdAt`
- `updatedAt`
- `finishedAt`
- `progress`
- `message`

## 7. 查询单个任务

```text
GET /api/task/{task_id}
```

示例：

```bash
curl "http://10.10.10.206:3001/api/task/7e2fef4c-7aaf-4bdb-8777-4a7194461a0e"
```

`message` 对于非失败任务只返回漫画标题；任务状态和页数进度分别通过 `status`、`progress` 返回。任务失败时，`message` 返回具体错误信息。

## 8. 取消任务

```text
POST /api/task/{task_id}/cancel
```

```bash
curl -X POST "http://10.10.10.206:3001/api/task/7e2fef4c-7aaf-4bdb-8777-4a7194461a0e/cancel"
```

- 排队中的任务会直接变成 `cancelled`
- 正在下载的任务会终止下载进程，并清理该任务的临时目录
- 已完成、失败或取消的任务保持原状态；重复请求不会重复执行取消

## 可选参数

以下参数可用于部分接口，尤其是下载接口：

- `download_dir`
- `img_concurrency`
- `img_interval_sec`
- `proxy`
- `api_domain`
- `config`
- `format`
- `use_original_filename=true`

示例：

```bash
curl "http://10.10.10.206:3001/api/download?target=328415&img_concurrency=5&img_interval_sec=1"
```

## 下载结果

当前服务器部署时，下载文件会落到宿主机：

```text
/vol2/1000/download
```

下载完成后只保留 `.zip` 文件，不保留原始图片目录。

## 通知

下载完成/失败不再发送 Bark；调用方通过 `POST /api/download` 和 `GET /api/task/{task_id}` 的 `message` 获取漫画标题，失败时获取具体错误信息。配置的 Webhook 仍按原设置工作。

## 网页端说明

网页首页主要做三件事：

- 提交下载任务
- 查看排队中、下载中、已完成、失败或取消的任务状态
- 取消尚未结束的任务

如果你只是想接服务端接口，使用 POST 创建任务，再通过 GET 查询进度。
