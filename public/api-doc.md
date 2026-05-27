# Manga Downloader API 文档

这个服务同时提供网页界面和 HTTP GET 接口。

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
| `GET /api/download?target=URL或ID` | 创建下载任务 |
| `GET /api/download/start?target=URL或ID` | 创建下载任务，和上面等价 |
| `GET /api/tasks` | 获取任务列表 |
| `GET /api/tasks/{taskId}` | 获取单个任务详情 |

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

```text
GET /api/download?target=<URL或ID>
```

或者：

```text
GET /api/download/start?target=<URL或ID>
```

说明：

- 创建后会进入统一任务中心
- 下载完成后会自动打包为 zip
- 原始下载目录会删除，只保留 zip
- 成功或失败都会触发已配置的通知

示例：

```bash
curl "http://10.10.10.206:3001/api/download?target=https://www.wnacg.com/photos-index-aid-349565.html"
```

返回通常会包含：

- `id`
- `status`
- `title`
- `cover`
- `totalPages`

## 6. 查询任务列表

```text
GET /api/tasks
```

示例：

```bash
curl "http://10.10.10.206:3001/api/tasks"
```

任务状态：

- `downloading`
- `success`
- `failed`

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

## 7. 查询单个任务

```text
GET /api/tasks/{taskId}
```

示例：

```bash
curl "http://10.10.10.206:3001/api/tasks/7e2fef4c-7aaf-4bdb-8777-4a7194461a0e"
```

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

当前服务支持下载结束通知。

- 成功时展示漫画标题
- 失败时展示失败原因、标题、下载完成度

## 网页端说明

网页首页主要做两件事：

- 提交下载任务
- 查看下载中、已完成、失败的任务状态

如果你只是想接服务端接口，直接使用上面的 GET API 即可。
