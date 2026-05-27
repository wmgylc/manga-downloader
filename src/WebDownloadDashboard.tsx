import { computed, defineComponent, onBeforeUnmount, onMounted, ref } from 'vue'
import {
  NButton,
  NCard,
  NEmpty,
  NImage,
  NInput,
  NProgress,
  NTag,
  useMessage,
} from 'naive-ui'

type DownloadTask = {
  id: string
  target: string
  provider?: 'wnacg' | 'jmcomic'
  status: 'downloading' | 'success' | 'failed'
  title?: string
  cover?: string
  totalPages?: number
  completedPages: number
  error?: string
  zipPath?: string
  createdAt: string
  updatedAt: string
  finishedAt?: string
}

type TaskGroupKey = 'downloading' | 'success' | 'failed'

const API_BASE =
  typeof window !== 'undefined' ? `${window.location.origin}/api` : 'http://10.10.10.206:3000'

const TASK_GROUPS: Array<{
  key: TaskGroupKey
  label: string
  tagType: 'warning' | 'success' | 'error'
}> = [
  { key: 'downloading', label: '下载中', tagType: 'warning' },
  { key: 'success', label: '下载完成', tagType: 'success' },
  { key: 'failed', label: '下载失败', tagType: 'error' },
]

const PROVIDER_MARKERS = [
  'jm:',
  'jmcomic',
  'jm-comic',
  '18comic',
  'jmapiproxy',
  'cdnzack',
  'cdnhth',
  'cdnbea',
]

export default defineComponent({
  name: 'WebDownloadDashboard',
  setup() {
    const message = useMessage()
    const url = ref('')
    const tasks = ref<DownloadTask[]>([])
    const submitting = ref(false)
    let timer: number | undefined

    async function requestJson(path: string, params?: Record<string, string>) {
      const requestUrl = new URL(path.replace(/^\/+/, ''), `${API_BASE.replace(/\/+$/, '')}/`)
      Object.entries(params ?? {}).forEach(([key, value]) => {
        requestUrl.searchParams.set(key, value)
      })

      const response = await fetch(requestUrl)
      const payload = await response.json()
      if (!response.ok) {
        throw new Error(payload.error || payload.stderr || '请求失败')
      }
      return payload
    }

    const sortedTasks = computed(() =>
      [...tasks.value].sort((a, b) => Number(b.updatedAt) - Number(a.updatedAt)),
    )
    const groupedTasks = computed<Record<TaskGroupKey, DownloadTask[]>>(() => ({
      downloading: sortedTasks.value.filter((task) => task.status === 'downloading'),
      success: sortedTasks.value.filter((task) => task.status === 'success'),
      failed: sortedTasks.value.filter((task) => task.status === 'failed'),
    }))
    const totalPages = computed(() =>
      sortedTasks.value.reduce((total, task) => total + (task.totalPages ?? 0), 0),
    )
    const completedPages = computed(() =>
      sortedTasks.value.reduce((total, task) => total + task.completedPages, 0),
    )

    async function loadTasks() {
      try {
        const payload = await requestJson('tasks')
        tasks.value = payload.tasks ?? []
      } catch (error) {
        console.error(error)
      }
    }

    async function startDownload() {
      const target = url.value.trim()
      if (!target) {
        message.warning('请输入任意页或漫画详情页 URL')
        return
      }

      submitting.value = true
      try {
        await requestJson('download/start', { target })
        url.value = ''
        message.success('下载任务已创建')
        await loadTasks()
      } catch (error) {
        message.error(error instanceof Error ? error.message : '创建下载任务失败')
      } finally {
        submitting.value = false
      }
    }

    function statusMeta(status: DownloadTask['status']) {
      return TASK_GROUPS.find((group) => group.key === status) ?? TASK_GROUPS[0]
    }

    function progressPercentage(task: DownloadTask) {
      if (!task.totalPages || task.totalPages <= 0) {
        return task.status === 'success' ? 100 : 0
      }
      return Math.min(100, Math.round((task.completedPages / task.totalPages) * 100))
    }

    function providerMeta(task: DownloadTask) {
      const lower = task.target.toLowerCase()
      const provider =
        task.provider ?? (PROVIDER_MARKERS.some((marker) => lower.includes(marker)) ? 'jmcomic' : 'wnacg')
      return provider === 'jmcomic'
        ? { label: 'JMComic', type: 'info' as const }
        : { label: 'WNACG', type: 'default' as const }
    }

    function formatTime(value?: string) {
      if (!value) {
        return '-'
      }
      const timestamp = Number(value)
      if (!Number.isFinite(timestamp)) {
        return '-'
      }
      return new Intl.DateTimeFormat('zh-CN', {
        month: '2-digit',
        day: '2-digit',
        hour: '2-digit',
        minute: '2-digit',
      }).format(new Date(timestamp * 1000))
    }

    function taskTitle(task: DownloadTask) {
      return task.title || task.target
    }

    onMounted(async () => {
      await loadTasks()
      timer = window.setInterval(loadTasks, 2000)
    })

    onBeforeUnmount(() => {
      if (timer) {
        window.clearInterval(timer)
      }
    })

    return () => (
      <div class="min-h-screen bg-[#f7f5ef] bg-[linear-gradient(rgba(35,48,44,0.045)_1px,transparent_1px),linear-gradient(90deg,rgba(35,48,44,0.045)_1px,transparent_1px)] bg-[size:28px_28px] px-4 py-5 text-[#14201c] md:px-8 md:py-8">
        <div class="mx-auto max-w-7xl">
          <section class="mb-5 grid overflow-hidden border border-solid border-[#d8ddd4] bg-white/92 md:grid-cols-[1.35fr_0.65fr]">
            <div class="border-0 border-b border-solid border-[#d8ddd4] p-5 md:border-b-0 md:border-r">
              <div class="mb-3 text-xs font-700 uppercase tracking-[0.14em] text-[#69776f]">下载目标</div>
              <div class="flex flex-col gap-3 md:flex-row">
                <NInput
                  value={url.value}
                  onUpdate:value={(value) => (url.value = value)}
                  placeholder="输入漫画 ID 或 URL"
                  size="large"
                  onKeydown={(event: KeyboardEvent) => {
                    if (event.key === 'Enter') {
                      void startDownload()
                    }
                  }}
                  class="min-w-0 flex-1"
                />
                <NButton
                  type="primary"
                  size="large"
                  loading={submitting.value}
                  class="min-w-32"
                  onClick={() => void startDownload()}>
                  开始下载
                </NButton>
              </div>
            </div>
            <div class="grid grid-cols-3 divide-x divide-solid divide-[#d8ddd4] md:grid-cols-1 md:divide-x-0 md:divide-y">
              {TASK_GROUPS.map((group) => (
                <div key={group.key} class="px-4 py-3">
                  <div class="text-xs font-700 uppercase tracking-[0.12em] text-[#69776f]">{group.label}</div>
                  <div class="mt-1 text-2xl font-800 tabular-nums text-[#14201c]">
                    {groupedTasks.value[group.key].length}
                  </div>
                </div>
              ))}
            </div>
          </section>

          <div class="mb-3 flex flex-col gap-2 border-0 border-b border-solid border-[#ccd4ce] pb-3 md:flex-row md:items-end md:justify-between">
            <div>
              <div class="text-xl font-800 tracking-tight">任务列表</div>
              <div class="mt-1 text-sm text-[#69776f]">
                {sortedTasks.value.length} 个任务 · {completedPages.value}
                {totalPages.value ? ` / ${totalPages.value}` : ''} 页
              </div>
            </div>
            <NButton size="small" quaternary class="self-start md:self-auto" onClick={() => void loadTasks()}>
              刷新
            </NButton>
          </div>

          {sortedTasks.value.length === 0 ? (
            <NCard bordered={false} class="border border-solid border-[#d8ddd4] bg-white/92">
              <NEmpty description="还没有下载任务" />
            </NCard>
          ) : (
            <div class="space-y-6">
              {TASK_GROUPS.map((group) =>
                groupedTasks.value[group.key].length > 0 ? (
                  <section key={group.key}>
                    <div class="mb-2 flex items-center gap-2">
                      <div class="text-sm font-800 uppercase tracking-[0.12em] text-[#14201c]">{group.label}</div>
                      <NTag bordered={false} size="small" type={group.tagType}>
                        {groupedTasks.value[group.key].length}
                      </NTag>
                    </div>
                    <div class="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
                      {groupedTasks.value[group.key].map((task) => (
                        <NCard
                          key={task.id}
                          bordered={false}
                          class="overflow-hidden border border-solid border-[#d8ddd4] bg-white/94 transition-colors hover:border-[#95aaa0]">
                          <div class="flex gap-4">
                            <div class="h-36 w-25 shrink-0 overflow-hidden border border-solid border-[#e3e6df] bg-[#eef1ec]">
                              {task.cover ? (
                                <NImage
                                  src={task.cover}
                                  alt={taskTitle(task)}
                                  preview-disabled
                                  class="h-full w-full object-cover"
                                />
                              ) : (
                                <div class="flex h-full items-center justify-center text-xs text-[#7b8981]">无封面</div>
                              )}
                            </div>
                            <div class="min-w-0 flex-1">
                              <div class="mb-2 flex flex-wrap items-center gap-2">
                                <NTag type={statusMeta(task.status).tagType} bordered={false} size="small">
                                  {statusMeta(task.status).label}
                                </NTag>
                                <NTag type={providerMeta(task).type} bordered={false} size="small">
                                  {providerMeta(task).label}
                                </NTag>
                              </div>
                              <div class="line-clamp-2 text-base font-800 leading-snug text-[#14201c]">
                                {taskTitle(task)}
                              </div>
                              <div class="mt-2 truncate font-mono text-xs text-[#69776f]">{task.target}</div>
                              <div class="mt-3">
                                <NProgress
                                  percentage={progressPercentage(task)}
                                  processing={task.status === 'downloading'}
                                  status={
                                    task.status === 'failed' ? 'error' : task.status === 'success' ? 'success' : 'info'
                                  }
                                  show-indicator={false}
                                  height={6}
                                />
                              </div>
                              <div class="mt-2 flex flex-wrap items-center justify-between gap-2 text-sm text-[#52615a]">
                                <span>
                                  {task.completedPages}
                                  {task.totalPages ? ` / ${task.totalPages}` : ''} 页
                                </span>
                                <span class="text-xs text-[#7b8981]">更新 {formatTime(task.updatedAt)}</span>
                              </div>
                            </div>
                          </div>

                          {task.error && (
                            <div class="mt-3 border border-solid border-[#f1c7c7] bg-[#fff4f2] px-3 py-2 text-sm text-[#9a2e2e]">
                              {task.error}
                            </div>
                          )}
                        </NCard>
                      ))}
                    </div>
                  </section>
                ) : null,
              )}
            </div>
          )}
        </div>
      </div>
    )
  },
})
