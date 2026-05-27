import { defineComponent } from 'vue'
import WebDownloadDashboard from './WebDownloadDashboard.tsx'
import {
  NConfigProvider,
  NModalProvider,
  NNotificationProvider,
  NMessageProvider,
  GlobalThemeOverrides,
} from 'naive-ui'

const themeOverrides: GlobalThemeOverrides = {
  common: {
    primaryColor: '#13211c',
    primaryColorHover: '#20332d',
    primaryColorPressed: '#0a1410',
    primaryColorSuppl: '#2f7668',
    borderRadius: '4px',
    borderRadiusSmall: '3px',
    heightMedium: '32px',
    fontFamily:
      'Avenir Next, Helvetica Neue, Helvetica, Arial, PingFang SC, Hiragino Sans GB, Microsoft YaHei, sans-serif',
  },
  Button: {
    paddingSmall: '0 8px',
    paddingMedium: '0 12px',
  },
  Radio: {
    buttonColorActive: '#13211c',
    buttonTextColorActive: '#FFF',
  },
  Dropdown: {
    borderRadius: '5px',
    padding: '6px 2px',
    optionColorHover: '#13211c',
    optionTextColorHover: '#FFF',
    optionHeightMedium: '28px',
  },
}

export default defineComponent({
  name: 'App',
  setup() {
    return () => (
      <NConfigProvider theme-overrides={themeOverrides}>
        <NModalProvider>
          <NNotificationProvider placement="bottom-right" max={3}>
            <NMessageProvider>
              <WebDownloadDashboard />
            </NMessageProvider>
          </NNotificationProvider>
        </NModalProvider>
      </NConfigProvider>
    )
  },
})
