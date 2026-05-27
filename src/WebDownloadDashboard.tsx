import { computed, defineComponent, onBeforeUnmount, onMounted, ref } from 'vue'
import {
  NButton,
  NCard,
  NEmpty,
  NImage,
  NInput,
  NProgress,
  NSpace,
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
        task.provider ??
        (lower.startsWith('jm:') ||
        lower.includes('jmcomic') ||
        lower.includes('jm-comic') ||
        lower.includes('18comic') ||
        lower.includes('jmapiproxy') ||
        lower.includes('cdnzack') ||
        lower.includes('cdnhth') ||
        lower.includes('cdnbea')
          ? 'jmcomic'
          : 'wnacg')
      return provider === 'jmcomic'
        ? { label: 'JMComic', type: 'info' as const }
        : { label: 'WNACG', type: 'default' as const }
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
      <div class="min-h-screen bg-[radial-gradient(circle_at_top,#fff7ed_0%,#fff 38%,#f8fafc_100%)] text-slate-900">
        <div class="mx-auto max-w-6xl px-4 py-8 md:px-8">
          <div class="mb-8 overflow-hidden rounded-6 border border-solid border-orange-200 bg-white/88 p-6 shadow-[0_24px_80px_rgba(15,23,42,0.08)] backdrop-blur">
            <div class="mb-6 flex flex-col gap-3 md:flex-row">
              <NInput
                value={url.value}
                onUpdate:value={(value) => (url.value = value)}
                placeholder="输入任意页或漫画详情页 URL"
                size="large"
                onKeydown={(event: KeyboardEvent) => {
                  if (event.key === 'Enter') {
                    void startDownload()
                  }
                }}
              />
              <NButton type="primary" size="large" loading={submitting.value} onClick={() => void startDownload()}>
                开始下载
              </NButton>
            </div>
          </div>

          <div class="mb-4 flex items-center justify-between">
            <div class="text-xl font-700">任务列表</div>
          </div>

          {sortedTasks.value.length === 0 ? (
            <NCard bordered={false} class="rounded-6 shadow-[0_10px_40px_rgba(15,23,42,0.06)]">
              <NEmpty description="还没有下载任务" />
            </NCard>
          ) : (
            <div class="space-y-8">
              {TASK_GROUPS.map((group) =>
                groupedTasks.value[group.key].length > 0 ? (
                  <section key={group.key}>
                    <div class="mb-3 flex items-center gap-3">
                      <div class="text-lg font-700">{group.label}</div>
                      <NTag bordered={false} type={group.tagType}>
                        {groupedTasks.value[group.key].length}
                      </NTag>
                    </div>
                    <div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
                      {groupedTasks.value[group.key].map((task) => (
                        <NCard
                          key={task.id}
                          bordered={false}
                          class="overflow-hidden rounded-6 border border-solid border-slate-200 shadow-[0_18px_50px_rgba(15,23,42,0.08)]">
                          <div class="mb-4 flex gap-4">
                            <div class="h-36 w-25 shrink-0 overflow-hidden rounded-4 bg-slate-100">
                              {task.cover ? (
                                <NImage
                                  src={task.cover}
                                  alt={task.title || task.target}
                                  preview-disabled
                                  class="h-full w-full object-cover"
                                />
                              ) : (
                                <div class="flex h-full items-center justify-center text-xs text-slate-400">无封面</div>
                              )}
                            </div>
                            <div class="min-w-0 flex-1">
                              <NSpace align="center">
                                <NTag type={statusMeta(task.status).tagType} bordered={false}>
                                  {statusMeta(task.status).label}
                                </NTag>
                                <NTag type={providerMeta(task).type} bordered={false}>
                                  {providerMeta(task).label}
                                </NTag>
                              </NSpace>
                              <div class="mt-3 line-clamp-3 text-base font-700 leading-snug">
                                {task.title || task.target}
                              </div>
                              <div class="mt-2 text-sm text-slate-500">
                                已下载 {task.completedPages}
                                {task.totalPages ? ` / ${task.totalPages}` : ''} 页
                              </div>
                            </div>
                          </div>

                          <NProgress
                            percentage={progressPercentage(task)}
                            processing={task.status === 'downloading'}
                            status={
                              task.status === 'failed' ? 'error' : task.status === 'success' ? 'success' : 'info'
                            }
                            show-indicator={false}
                          />

                          {task.error && (
                            <div class="mt-4 rounded-4 bg-rose-50 px-3 py-2 text-sm text-rose-700">{task.error}</div>
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
