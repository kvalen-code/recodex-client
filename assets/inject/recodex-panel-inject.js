// recodex-overlay: ReCodex in-page 面板。注入官方 ChatGPT/Codex 页面,提供一个悬浮
// 「ReCodex」按钮 → 点开面板(账号/额度/网关/登录)。所有数据经 CDP 桥 /recodex/* 取,
// 逻辑在 recodex-integration crate 的 desktop 模块;本文件只画 UI + 调桥。
// 幂等:重注入(SPA 跳转)只装一次。
(() => {
  "use strict";
  if (window.__recodexPanelInstalled === "1") return;
  window.__recodexPanelInstalled = "1";

  // ── 调桥封装 ───────────────────────────────────────────────
  // 桥调用超时。没有它的时候,原生侧一旦不应答(launcher 卡住 / binding 只装了一半),
  // Promise 就永远不落地,页签停在「加载中」——没有报错、没有重试,用户只能重启。
  // 这个症状真的发生过,根因单独修了;但面板自己也该有防线。
  const BRIDGE_TIMEOUT_MS = 20000;

  // 这些是**本来就慢**的命令,不能按 20 秒判死:
  // 自更新要下十几 MB;微信启停内部自带轮询;探测最快网关要逐个测速;
  // 登录轮询本身就是等人扫码;退出/重启/卸载会把进程带走,不会有回包。
  const BRIDGE_NO_TIMEOUT = [
    "/self-update", "/uninstall", "/quit", "/restart-codex",
    "/weixin/start", "/weixin/stop", "/weixin/qr-start", "/weixin/qr-status",
    "/recodex/login/poll", "/recodex/gateway/fastest",
  ];

  function bridge(path, payload) {
    const fn = window.__codexSessionDeleteBridge;
    if (typeof fn !== "function") {
      return Promise.resolve({ status: "error", error: { code: "no_bridge", message: t("ReCodex 桥未就绪") } });
    }
    const call = Promise.resolve(fn(path, payload || {})).catch((e) => ({
      status: "error",
      error: { code: "bridge_error", message: String(e && e.message ? e.message : e) },
    }));
    if (BRIDGE_NO_TIMEOUT.indexOf(path) >= 0) return call;
    let timer = null;
    const guard = new Promise((resolve) => {
      timer = setTimeout(
        () => resolve({ status: "error", error: { code: "bridge_timeout", message: t("ReCodex 桥未响应") } }),
        BRIDGE_TIMEOUT_MS,
      );
    });
    return Promise.race([call, guard]).then((result) => {
      if (timer !== null) clearTimeout(timer);
      return result;
    });
  }

  // ── 多语言 ─────────────────────────────────────────────────
  // 以简体原文作为 key:未翻译的条目自动回落简体,不会漏成空白或 key 名。
  // 覆盖面板全部文案 + 我们注入到官方 UI 上的文字(如「7 天用量」)。
  const I18N = {
    tw: {
      "版本与更新": "版本與更新", "当前版本": "目前版本", "最新版本": "最新版本",
      "已是最新版本": "已是最新版本", "重新检查": "重新檢查", "检查中…": "檢查中…",
      "更新到最新版": "更新到最新版", "立即更新(必需)": "立即更新(必要)",
      "无法检查更新": "無法檢查更新", "正在下载…": "正在下載…", "更新失败": "更新失敗",
      "更新完成,正在重启…": "更新完成,正在重啟…", "返回": "返回", "最低版本": "最低版本",
      "当前版本已停止支持,必须更新后才能继续使用。": "目前版本已停止支援,必須更新後才能繼續使用。",
      "当前版本已停止支持,但暂时没有可用的更新包。": "目前版本已停止支援,但暫時沒有可用的更新包。",
      "请联系管理员,或稍后重新检查。": "請聯絡管理員,或稍後重新檢查。",
      "你不在本次更新的推送名单内。": "你不在本次更新的推送名單內。",
      "服务端尚未配置更新包。": "伺服器尚未設定更新包。",
      "卸载 ReCodex": "解除安裝 ReCodex", "重新启动连接…": "重新啟動連線…", "停止中…": "停止中…", "首次登录需重启生效:请": "首次登入需重新啟動才生效:請", "完全退出 Codex,再双击桌面 ReCodex 重新打开": "完全結束 Codex,再雙擊桌面 ReCodex 重新開啟", ",官方界面才会用上你的账号(无需再登 ChatGPT)。": ",官方介面才會使用你的帳號(不需再登入 ChatGPT)。", "重启失败": "重新啟動失敗", "如 gpt-5.6-sol": "例如 gpt-5.6-sol", "卸载失败,未做任何改动": "解除安裝失敗,未做任何變更", "确定卸载?此操作不可撤销:": "確定解除安裝?此操作無法復原:",
      "还原 Codex 配置并清除登录凭据": "還原 Codex 設定並清除登入憑據",
      "服务端吊销本设备": "伺服器端撤銷本裝置", "删除快捷方式与程序本体": "刪除捷徑與程式本體", "删除设备标识与用户脚本(不可恢复)": "刪除裝置識別與使用者指令碼(無法復原)",
      "确认卸载": "確認解除安裝", "卸载中…": "解除安裝中…", "卸载完成": "解除安裝完成",
      "运行模式": "執行模式", "当前": "目前", "官方 ChatGPT": "官方 ChatGPT",
      "切回 ReCodex 并重启": "切回 ReCodex 並重啟", "切到官方 ChatGPT 并重启": "切到官方 ChatGPT 並重啟",
      "切换中…": "切換中…", "切换失败": "切換失敗", "正在重启 Codex…": "正在重啟 Codex…",
      "切回不需要重新登录(凭据与 Codex 配置无关)。": "切回不需要重新登入(憑據與 Codex 設定無關)。",
      "临时改用官方账号;登录状态会保留,可随时切回。": "暫時改用官方帳號;登入狀態會保留,可隨時切回。",
      "连接中断": "連線中斷", "无法连接": "無法連線", "额度已用尽": "額度已用盡",
      "正常": "正常", "官方模式": "官方模式",
      "居中宽度(px)": "置中寬度(px)", "继承": "繼承", "全局 Standard": "全域 Standard",
      "全局 Fast": "全域 Fast", "自定义": "自訂", "继承 config.toml": "繼承 config.toml",
      "服务模式引擎未就绪": "服務模式引擎未就緒",
      "对话居中宽度": "對話置中寬度", "切换对话保留位置": "切換對話保留位置",
      "强制中文界面": "強制中文介面", "服务模式控件": "服務模式控件",
      "系统集成": "系統整合", "原生菜单栏位置": "原生選單列位置",
      "原生菜单本地化": "原生選單在地化", "Zed Remote 打开": "Zed Remote 開啟",
      "上游 worktree 创建": "上游 worktree 建立",
      "账号": "帳號", "增强": "增強", "微信": "微信", "高级": "進階",
      "界面语言": "介面語言", "UI 助手": "UI 助手",
      "跟随 Codex 语言;英语等未支持语种显示简体中文。": "跟隨 Codex 語言;英語等未支援語種顯示簡體中文。",
      "打开 MotionSites,快速生成前端界面。": "開啟 MotionSites,快速產生前端介面。",
      "增强功能": "增強功能", "微信连接": "微信連接", "加载中…": "載入中…",
      "邮箱": "郵箱", "套餐": "套餐", "网关": "網關", "未选": "未選",
      "用最快网关": "用最快網關", "刷新额度": "重新整理額度", "登出": "登出",
      "5 小时": "5 小時", "7 天": "7 天", "5 小时用量": "5 小時用量", "7 天用量": "7 天用量",
      "未登录 ReCodex。": "未登入 ReCodex。", "登录 ReCodex": "登入 ReCodex",
      "无法读取状态": "無法讀取狀態", "重试": "重試",
      "正在发起登录…": "正在發起登入…", "登录发起失败": "登入發起失敗",
      "在浏览器打开并输入授权码:": "在瀏覽器開啟並輸入授權碼:",
      "授权码": "授權碼", "打开授权页": "開啟授權頁", "等待确认…": "等待確認…",
      "授权超时,请重试": "授權逾時,請重試", "无法自动打开,请手动复制地址": "無法自動開啟,請手動複製網址", "登录失败": "登入失敗",
      "✅ 登录成功": "✅ 登入成功",
      "我已重启,刷新状态": "我已重啟,重新整理狀態",
      "会话删除": "工作階段刪除", "Markdown 导出": "Markdown 匯出", "会话 ID 标识": "工作階段 ID 標識",
      "粘贴修复(需重启)": "貼上修復(需重啟)", "Fast 按钮": "Fast 按鈕",
      "模型白名单解锁": "模型白名單解鎖", "插件市场解锁": "外掛市集解鎖",
      "桌宠跟随真实鼠标": "桌寵跟隨真實滑鼠",
      "状态": "狀態", "账号": "帳號", "已处理": "已處理", "条": "條",
      "运行中": "運行中", "启动中": "啟動中", "重连中": "重連中",
      "停止中": "停止中", "出错": "出錯", "已停止": "已停止", "未启动": "未啟動",
      "启动连接": "啟動連接", "停止连接": "停止連接", "保存配置": "儲存設定",
      "保存中…": "儲存中…", "已保存": "已儲存", "启动中…": "啟動中…", "启动失败": "啟動失敗",
      "停止超时,请稍后手动启动": "停止逾時,請稍後手動啟動",
      "扫码登录微信": "掃碼登入微信", "正在生成二维码…": "正在產生 QR Code…",
      "生成二维码失败": "產生 QR Code 失敗", "用微信扫码并确认授权:": "用微信掃碼並確認授權:",
      "等待扫码…": "等待掃碼…", "已扫码,请在手机上确认…": "已掃碼,請在手機上確認…",
      "二维码已过期,请重试": "QR Code 已過期,請重試", "扫码失败": "掃碼失敗", "取消": "取消",
      "工作目录(Codex 在此目录执行)": "工作目錄(Codex 在此目錄執行)",
      "留空 = 启动器当前目录": "留空 = 啟動器目前目錄",
      "沙箱级别": "沙箱等級", "read-only(只读,推荐)": "read-only(唯讀,推薦)",
      "workspace-write(可改工作目录)": "workspace-write(可改工作目錄)",
      "danger-full-access(完全放开)": "danger-full-access(完全開放)",
      "白名单(微信 user id,逗号分隔)": "白名單(微信 user id,逗號分隔)",
      "留空=不响应任何人": "留空=不回應任何人",
      "模型(留空=Codex 默认)": "模型(留空=Codex 預設)",
      "ReCodex 桥未就绪": "ReCodex 橋未就緒", "设置没有生效,请重试": "設定沒有生效,請重試", "数据可能不是最新的": "資料可能不是最新的", "数据截至": "資料截至", "配置没有保存成功,请重试": "設定沒有儲存成功,請重試", "ReCodex 桥未响应": "ReCodex 橋沒有回應",
      "未绑定微信。扫码后可在微信里直接指挥本机 Codex。": "未綁定微信。掃碼後可在微信裡直接指揮本機 Codex。",
      "⚠ 白名单为空:微信连接不会响应任何人。填入你的微信 ID,或填 * 放开所有人。": "⚠ 白名單為空:微信連線不會回應任何人。填入你的微信 ID,或填 * 放開所有人。",
      "⚠ 白名单为 *:任何人给该微信号发消息都能在本机运行 Codex。": "⚠ 白名單為 *:任何人給該微信號發訊息都能在本機執行 Codex。",
    },
    ru: {
      "版本与更新": "Версия и обновление", "当前版本": "Текущая версия", "最新版本": "Последняя версия",
      "已是最新版本": "Установлена последняя версия", "重新检查": "Проверить снова", "检查中…": "Проверка…",
      "更新到最新版": "Обновить", "立即更新(必需)": "Обновить (обязательно)",
      "无法检查更新": "Не удалось проверить обновления", "正在下载…": "Загрузка…", "更新失败": "Ошибка обновления",
      "更新完成,正在重启…": "Обновление завершено, перезапуск…", "返回": "Назад", "最低版本": "Минимальная версия",
      "当前版本已停止支持,必须更新后才能继续使用。": "Эта версия больше не поддерживается — обновитесь, чтобы продолжить.",
      "当前版本已停止支持,但暂时没有可用的更新包。": "Версия больше не поддерживается, но обновление пока недоступно.",
      "请联系管理员,或稍后重新检查。": "Обратитесь к администратору или проверьте позже.",
      "你不在本次更新的推送名单内。": "Вы не входите в список рассылки этого обновления.",
      "服务端尚未配置更新包。": "Обновление ещё не настроено на сервере.",
      "卸载 ReCodex": "Удалить ReCodex", "重新启动连接…": "Перезапуск подключения…", "停止中…": "Остановка…", "首次登录需重启生效:请": "Первый вход вступит в силу после перезапуска: ", "完全退出 Codex,再双击桌面 ReCodex 重新打开": "полностью закройте Codex и снова запустите ReCodex с рабочего стола", ",官方界面才会用上你的账号(无需再登 ChatGPT)。": " — тогда официальный интерфейс использует ваш аккаунт (входить в ChatGPT не нужно).", "重启失败": "Не удалось перезапустить", "如 gpt-5.6-sol": "например gpt-5.6-sol", "卸载失败,未做任何改动": "Удаление не выполнено, ничего не изменено", "确定卸载?此操作不可撤销:": "Удалить? Действие необратимо:",
      "还原 Codex 配置并清除登录凭据": "Восстановить конфиг Codex и удалить учётные данные",
      "服务端吊销本设备": "Отозвать это устройство на сервере", "删除快捷方式与程序本体": "Удалить ярлыки и сам файл программы", "删除设备标识与用户脚本(不可恢复)": "Удалить идентификатор устройства и пользовательские скрипты (безвозвратно)",
      "确认卸载": "Подтвердить удаление", "卸载中…": "Удаление…", "卸载完成": "Удаление завершено",
      "运行模式": "Режим работы", "当前": "Сейчас", "官方 ChatGPT": "Официальный ChatGPT",
      "切回 ReCodex 并重启": "Вернуться в ReCodex и перезапустить", "切到官方 ChatGPT 并重启": "Перейти на официальный ChatGPT и перезапустить",
      "切换中…": "Переключение…", "切换失败": "Не удалось переключить", "正在重启 Codex…": "Перезапуск Codex…",
      "切回不需要重新登录(凭据与 Codex 配置无关)。": "Возврат не требует повторного входа — учётные данные не связаны с конфигом Codex.",
      "临时改用官方账号;登录状态会保留,可随时切回。": "Временно используйте официальный аккаунт; вход сохранится, вернуться можно в любой момент.",
      "连接中断": "Нет связи", "无法连接": "Не удалось подключиться", "额度已用尽": "Квота исчерпана",
      "正常": "Всё в порядке", "官方模式": "Официальный режим",
      "居中宽度(px)": "Ширина по центру (px)", "继承": "Наследовать", "全局 Standard": "Везде Standard",
      "全局 Fast": "Везде Fast", "自定义": "Свой", "继承 config.toml": "Из config.toml",
      "服务模式引擎未就绪": "Движок режима сервиса не готов",
      "对话居中宽度": "Ширина диалога по центру", "切换对话保留位置": "Сохранять позицию прокрутки",
      "强制中文界面": "Принудительный китайский интерфейс", "服务模式控件": "Переключатель режима сервиса",
      "系统集成": "Интеграция с системой", "原生菜单栏位置": "Положение системного меню",
      "原生菜单本地化": "Локализация системного меню", "Zed Remote 打开": "Открывать в Zed Remote",
      "上游 worktree 创建": "Создание upstream worktree",
      "账号": "Аккаунт", "增强": "Функции", "微信": "WeChat", "高级": "Ещё",
      "界面语言": "Язык интерфейса", "UI 助手": "UI-помощник",
      "跟随 Codex 语言;英语等未支持语种显示简体中文。": "Следует языку Codex; для неподдерживаемых языков — упрощённый китайский.",
      "打开 MotionSites,快速生成前端界面。": "Откройте MotionSites — быстрая генерация интерфейсов.",
      "增强功能": "Улучшения", "微信连接": "WeChat", "加载中…": "Загрузка…",
      "邮箱": "Почта", "套餐": "Тариф", "网关": "Шлюз", "未选": "не выбран",
      "用最快网关": "Самый быстрый шлюз", "刷新额度": "Обновить квоту", "登出": "Выйти",
      "5 小时": "5 часов", "7 天": "7 дней", "5 小时用量": "Расход за 5 ч", "7 天用量": "Расход за 7 дн",
      "未登录 ReCodex。": "Вы не вошли в ReCodex.", "登录 ReCodex": "Войти в ReCodex",
      "无法读取状态": "Не удалось получить статус", "重试": "Повторить",
      "正在发起登录…": "Запуск входа…", "登录发起失败": "Не удалось начать вход",
      "在浏览器打开并输入授权码:": "Откройте в браузере и введите код:",
      "授权码": "Код", "打开授权页": "Открыть страницу входа", "等待确认…": "Ожидание подтверждения…",
      "授权超时,请重试": "Время вышло, повторите", "无法自动打开,请手动复制地址": "Не удалось открыть автоматически, скопируйте ссылку вручную", "登录失败": "Ошибка входа",
      "✅ 登录成功": "✅ Вход выполнен",
      "我已重启,刷新状态": "Я перезапустил — обновить",
      "会话删除": "Удаление диалогов", "Markdown 导出": "Экспорт в Markdown", "会话 ID 标识": "Показывать ID диалога",
      "粘贴修复(需重启)": "Исправление вставки (нужен перезапуск)", "Fast 按钮": "Кнопка Fast",
      "模型白名单解锁": "Разблокировать модели", "插件市场解锁": "Разблокировать плагины",
      "桌宠跟随真实鼠标": "Питомец следит за курсором",
      "状态": "Статус", "账号": "Аккаунт", "已处理": "Обработано", "条": "шт.",
      "运行中": "Работает", "启动中": "Запуск", "重连中": "Переподключение",
      "停止中": "Остановка", "出错": "Ошибка", "已停止": "Остановлено", "未启动": "Не запущено",
      "启动连接": "Запустить", "停止连接": "Остановить", "保存配置": "Сохранить",
      "保存中…": "Сохранение…", "已保存": "Сохранено", "启动中…": "Запуск…", "启动失败": "Ошибка запуска",
      "停止超时,请稍后手动启动": "Остановка затянулась, запустите вручную позже",
      "扫码登录微信": "Войти через QR-код WeChat", "正在生成二维码…": "Генерация QR-кода…",
      "生成二维码失败": "Не удалось создать QR-код", "用微信扫码并确认授权:": "Отсканируйте в WeChat и подтвердите:",
      "等待扫码…": "Ожидание сканирования…", "已扫码,请在手机上确认…": "Отсканировано, подтвердите на телефоне…",
      "二维码已过期,请重试": "QR-код истёк, повторите", "扫码失败": "Ошибка сканирования", "取消": "Отмена",
      "工作目录(Codex 在此目录执行)": "Рабочая папка (здесь выполняется Codex)",
      "留空 = 启动器当前目录": "Пусто = текущая папка лаунчера",
      "沙箱级别": "Уровень песочницы", "read-only(只读,推荐)": "read-only (только чтение, рекомендуется)",
      "workspace-write(可改工作目录)": "workspace-write (запись в рабочую папку)",
      "danger-full-access(完全放开)": "danger-full-access (полный доступ)",
      "白名单(微信 user id,逗号分隔)": "Белый список (user id WeChat, через запятую)",
      "留空=不响应任何人": "Пусто = никто не получит ответа",
      "模型(留空=Codex 默认)": "Модель (пусто = по умолчанию)",
      "ReCodex 桥未就绪": "Мост ReCodex не готов", "设置没有生效,请重试": "Настройка не применилась, попробуйте ещё раз", "数据可能不是最新的": "Данные могут быть устаревшими", "数据截至": "данные на", "配置没有保存成功,请重试": "Настройки не сохранились, попробуйте ещё раз", "ReCodex 桥未响应": "Мост ReCodex не отвечает",
      "未绑定微信。扫码后可在微信里直接指挥本机 Codex。": "WeChat не привязан. После сканирования можно управлять Codex прямо из WeChat.",
      "⚠ 白名单为空:微信连接不会响应任何人。填入你的微信 ID,或填 * 放开所有人。": "⚠ Белый список пуст: бот никому не ответит. Укажите свой WeChat ID или * , чтобы разрешить всем.",
      "⚠ 白名单为 *:任何人给该微信号发消息都能在本机运行 Codex。": "⚠ Белый список = *: любой, кто напишет боту, сможет запускать Codex на этом компьютере.",
    },
  };

  const LANG_KEY = "recodex.lang";
  // 跟随 Codex 语言;英语等未支持语种回落简体(按产品要求,不做英文界面)。
  //
  // 不缓存结果:注入发生在 document-start,此时文档还在 about:blank(不透明源),
  // localStorage 是另一个临时存储 —— getItem 返回 null 且**不抛异常**,任何
  // "读一次就定下来" 的写法都会把 zh 永久缓存,用户选的语言再也不生效。
  // getItem 是同步内存读取,每次调用的代价可以忽略。
  function currentLang() {
    try {
      const saved = localStorage.getItem(LANG_KEY);
      if (saved === "zh" || saved === "tw" || saved === "ru") return saved;
    } catch (e) {}
    const raw = String(
      (document.documentElement && document.documentElement.lang) ||
        (navigator && navigator.language) ||
        ""
    ).toLowerCase();
    if (raw.startsWith("zh-tw") || raw.startsWith("zh-hk") || raw.startsWith("zh-hant")) return "tw";
    if (raw.startsWith("ru")) return "ru";
    return "zh";
  }
  function t(s) {
    const dict = I18N[currentLang()];
    return (dict && dict[s]) || s;
  }
  function setLang(value) {
    try { localStorage.setItem(LANG_KEY, value); } catch (e) {}
  }

  // ── 样式 ───────────────────────────────────────────────────
  const style = document.createElement("style");
  style.textContent = `
    #recodex-fab{position:fixed;right:18px;bottom:18px;z-index:2147483000;width:44px;height:44px;border-radius:50%;
      border:none;cursor:pointer;box-shadow:0 2px 10px rgba(0,0,0,.35);padding:0;overflow:hidden;background:#10221b}
    #recodex-fab:hover{filter:brightness(1.12)}
    #recodex-fab img{width:100%;height:100%;object-fit:cover;display:block}
    #recodex-fab{position:fixed}
    .rcx-dot{display:inline-block;width:9px;height:9px;border-radius:50%;flex:0 0 9px;
      animation:rcx-breathe 2.4s ease-in-out infinite}
    #recodex-fab-dot{position:absolute;top:1px;right:1px;width:11px;height:11px;
      border:2px solid #10221b;box-shadow:0 0 4px rgba(0,0,0,.5)}
    #recodex-fab{overflow:visible}
    #recodex-fab img{border-radius:50%}
    @keyframes rcx-breathe{0%,100%{opacity:1;transform:scale(1)}50%{opacity:.45;transform:scale(.86)}}
    .rcx-dot.ok{background:#3ee98a}
    .rcx-dot.off{background:#ff6b5e}
    .rcx-dot.anon{background:#f5b944}
    .rcx-dot.quota{background:#a970ff}
    .rcx-dot.hidden{display:none}
    #recodex-panel{position:fixed;right:18px;bottom:72px;z-index:2147483000;width:320px;max-height:70vh;overflow:auto;
      background:#1b1e24;color:#e6e9ef;border:1px solid #2c313a;border-radius:12px;box-shadow:0 8px 30px rgba(0,0,0,.45);
      font:13px/1.5 system-ui,sans-serif;padding:16px;display:none;
      scrollbar-width:none;-ms-overflow-style:none}
    #recodex-panel::-webkit-scrollbar{width:0;height:0;display:none}
    #recodex-panel.open{display:block}
    #recodex-panel h3{margin:0 0 10px;font-size:15px;display:flex;align-items:center;gap:8px}
    #recodex-panel .rcx-row{display:flex;justify-content:space-between;gap:8px;padding:4px 0;border-bottom:1px solid #23272f}
    #recodex-panel .rcx-k{color:#9aa3b2}
    #recodex-panel .rcx-bar{height:6px;border-radius:3px;background:#2c313a;margin-top:4px;overflow:hidden}
    #recodex-panel .rcx-bar>i{display:block;height:100%;background:#10a37f}
    #recodex-panel button.rcx-act{margin-top:12px;width:100%;padding:8px;border:none;border-radius:8px;background:#10a37f;color:#fff;cursor:pointer;font:600 13px system-ui}
    #recodex-panel button.rcx-act.sec{background:#2c313a;color:#e6e9ef}
    #recodex-panel .rcx-muted{color:#7c8598;font-size:12px}
    #recodex-panel .rcx-err{color:#ff6b5e}
    #recodex-panel .rcx-toggle{display:flex;justify-content:space-between;align-items:center;padding:5px 0}
    #recodex-panel .rcx-toggle input{width:34px;height:18px;cursor:pointer}
    #recodex-panel .rcx-qr{background:#fff;border-radius:8px;padding:8px;margin-top:8px}
    #recodex-panel .rcx-qr svg{width:100%;height:auto;display:block}
    #recodex-panel .rcx-field{margin-top:8px}
    #recodex-panel .rcx-field label{display:block;color:#9aa3b2;font-size:12px;margin-bottom:3px}
    #recodex-panel .rcx-field input,#recodex-panel .rcx-field select{width:100%;box-sizing:border-box;padding:6px 8px;
      background:#12151a;color:#e6e9ef;border:1px solid #2c313a;border-radius:6px;font:12px system-ui}
    #recodex-panel .rcx-badge{display:inline-block;padding:1px 7px;border-radius:10px;font-size:11px;background:#2c313a;color:#9aa3b2}
    #recodex-panel .rcx-badge.on{background:#10a37f;color:#fff}
    #recodex-panel .rcx-badge.warn{background:#7a4a12;color:#ffce8a}
    #recodex-panel .rcx-tabs{display:flex;gap:2px;margin:2px 0 10px;border-bottom:1px solid #23272f}
    #recodex-panel .rcx-tab{flex:1;padding:6px 2px;background:none;border:none;border-bottom:2px solid transparent;
      color:#7c8598;cursor:pointer;font:12px system-ui;white-space:nowrap}
    #recodex-panel .rcx-tab:hover{color:#c3cad6}
    #recodex-panel .rcx-tab.on{color:#e6e9ef;border-bottom-color:#10a37f}
    #recodex-panel .rcx-pane{display:none}
    #recodex-panel .rcx-pane.on{display:block}
  `;
  document.documentElement.appendChild(style);

  // ── Tab ────────────────────────────────────────────────────
  const TABS = ["account", "enh", "wx", "adv"];
  const TAB_LABEL = { account: "账号", enh: "增强", wx: "微信", adv: "高级" };
  let activeTab = "account";

  function renderTabLabels() {
    panel.querySelectorAll(".rcx-tab").forEach((btn) => {
      btn.textContent = t(TAB_LABEL[btn.dataset.tab] || "");
    });
  }

  function showTab(id) {
    activeTab = id;
    panel.querySelectorAll(".rcx-tab").forEach((b) => b.classList.toggle("on", b.dataset.tab === id));
    panel.querySelectorAll(".rcx-pane").forEach((p) => p.classList.toggle("on", p.dataset.pane === id));
    renderActiveTab();
  }

  // 按需渲染:只画当前页签,切走的页签不做无谓的网络请求
  function renderActiveTab() {
    if (activeTab === "account") render();
    else if (activeTab === "enh") renderEnhancements();
    else if (activeTab === "wx") renderWeixin();
    else if (activeTab === "adv") renderAdvanced();
    if (activeTab !== "wx") wxStopPolling();
  }

  // ── DOM ────────────────────────────────────────────────────
  const fab = document.createElement("button");
  fab.id = "recodex-fab";
  // ReCodex 站点原图标(方形 Rx,base64 内联,圆形裁切)
  fab.innerHTML = '<img alt="ReCodex" src="data:image/png;base64,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">';
  fab.title = "ReCodex";
  const fabDot = document.createElement("span");
  fabDot.id = "recodex-fab-dot";
  fabDot.className = "rcx-dot ok";
  fab.appendChild(fabDot);
  const panel = document.createElement("div");
  panel.id = "recodex-panel";
  // Tab 结构:面板内容多了以后单列会拉得很长,分页签放。
  // 标签文字在 document-start 读不到语言,统一在打开面板时按当前语言重写。
  panel.innerHTML =
    `<h3><span class="rcx-dot ok" id="rcx-title-dot"></span> ReCodex</h3>` +
    `<div class="rcx-tabs">` +
      TABS.map((id, i) => `<button class="rcx-tab${i === 0 ? " on" : ""}" data-tab="${id}"></button>`).join("") +
    `</div>` +
    `<div class="rcx-pane on" data-pane="account"><div id="recodex-body"></div></div>` +
    `<div class="rcx-pane" data-pane="enh"><div id="recodex-enh"></div></div>` +
    `<div class="rcx-pane" data-pane="wx"><div id="recodex-wx"></div></div>` +
    `<div class="rcx-pane" data-pane="adv"><div id="recodex-adv"></div></div>`;
  document.documentElement.appendChild(fab);
  document.documentElement.appendChild(panel);
  panel.querySelectorAll(".rcx-tab").forEach((btn) => {
    btn.onclick = () => showTab(btn.dataset.tab);
  });

  const body = () => panel.querySelector("#recodex-body");
  const esc = (s) => String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

  function pct(w) {
    if (!w || !w.limit) return 0;
    return Math.min(100, Math.max(0, Math.round((w.used / w.limit) * 100)));
  }

  // ── 渲染 ───────────────────────────────────────────────────
  async function render() {
    body().innerHTML = `<div class="rcx-muted">${t("加载中…")}</div>`;
    const res = await bridge("/recodex/status", {});
    // 账号页本来就要这份数据,顺带把状态灯刷新了 —— 用户打开面板时看到的
    // 就是当下的状态,不必等 60 秒轮询
    rcxStatus = computeStatus(res);
    applyStatus();
    if (res.status === "signed_out") {
      // 后端可能带回"为什么没登录上"(凭据读不出来 / token 用不了)。
      // 少了这一句,用户看到的就是「明明登录过,重启后变成未登录」且毫无解释 ——
      // 而如果原因是凭据存储坏了,他重新登录多少次都会再掉出来。
      body().innerHTML = `<div class="rcx-muted">${t("未登录 ReCodex。")}</div>` +
        (res.notice ? `<div class="rcx-err" style="font-size:12px;margin-top:6px">${esc(res.notice)}</div>` : "") +
        `<button class="rcx-act" id="rcx-login">${t("登录 ReCodex")}</button>`;
      body().querySelector("#rcx-login").onclick = doLogin;
      return;
    }
    if (res.status === "error" || !res.data) {
      const msg = res.error ? res.error.message : t("无法读取状态");
      body().innerHTML = `<div class="rcx-err">${esc(msg)}</div>
        <button class="rcx-act sec" id="rcx-retry">${t("重试")}</button>`;
      body().querySelector("#rcx-retry").onclick = render;
      return;
    }
    const d = res.data;
    let html = "";
    // 后端用 status:"stale" 明确说过"这份数据不一定是当前的",
    // 而且 account_error / gateway_error 里已经写了为什么。
    // 原先这三样全被丢掉,过期的额度和「—」的账号被当成当前值展示 ——
    // 用户看到 4% 会以为真的还剩 96%。
    if (res.status === "stale") {
      // 三个原因来源要凑齐:上一版漏了 usage.refresh_error —— 那恰恰是"刷新没成功"
      // 这一条最直接的解释。
      const u = d.usage || {};
      const why = [
        u.refresh_error && u.refresh_error.message,
        d.account_error,
        d.gateway_error,
      ].filter(Boolean).join(" / ");
      // 光说"可能不是最新"帮不上忙。把数据的时间点显示出来,用户点完刷新一看时间
      // 有没有动,就能自己判断是"没更新"还是"本来就没变化"。
      const at = u.refreshed_at ? fmtTime(u.refreshed_at) : "";
      // 红字留给真故障。服务端把"距上游观测超过 5 分钟"就算 stale,
      // 而那个观测时间取自上游探测的 FetchedAt —— 探测不前进,点多少次刷新
      // 时间戳都不动。这是常态,不是错误;永远红着只会让人对红色脱敏。
      // 有 why(读取/刷新真的失败了)才红,否则只是平静地告诉你数据截止到什么时候。
      const cls = why ? "rcx-err" : "rcx-muted";
      html += `<div class="${cls}" style="font-size:12px;margin-bottom:6px">${
        t("数据可能不是最新的")}${at ? `(${t("数据截至")} ${esc(at)})` : ""}${
        why ? `:${esc(why)}` : ""}</div>`;
    }
    const acc = d.account || {};
    const u = d.usage || {};
    const w5 = (u.windows || []).find((w) => w.window === "5h");
    const w7 = (u.windows || []).find((w) => w.window === "7d");
    const gws = d.gateways || [];
    const sel = d.selected_gateway;
    html += `<div class="rcx-row"><span class="rcx-k">${t("邮箱")}</span><span>${esc(acc.email ? maskEmail(acc.email) : "—")}</span></div>`;
    html += `<div class="rcx-row"><span class="rcx-k">${t("套餐")}</span><span>${esc(acc.plan || acc.account_type || "—")}</span></div>`;
    // 5h 窗口只在有实际用量时显示;无用量则只显示 7 天(对齐 mdash「隐藏空 5h」)
    if (w5 && w5.used > 0) html += `<div style="padding:6px 0"><div class="rcx-row" style="border:0"><span class="rcx-k">${t("5 小时")}</span><span>${pct(w5)}%</span></div><div class="rcx-bar"><i style="width:${pct(w5)}%"></i></div></div>`;
    if (w7) html += `<div style="padding:6px 0"><div class="rcx-row" style="border:0"><span class="rcx-k">${t("7 天")}</span><span>${pct(w7)}%</span></div><div class="rcx-bar"><i style="width:${pct(w7)}%"></i></div></div>`;
    html += `<div class="rcx-row"><span class="rcx-k">${t("网关")}</span><span>${esc(sel ? sel.name : t("未选"))}</span></div>`;
    html += `<button class="rcx-act" id="rcx-fastest">${t("用最快网关")}</button>`;
    html += `<button class="rcx-act sec" id="rcx-refresh">${t("刷新额度")}</button>`;
    html += `<button class="rcx-act sec" id="rcx-logout">${t("登出")}</button>`;
    body().innerHTML = html;
    // 桥可能带回 warning(例如官方模式下网关只记进快照、切回后才生效)。
    // 原先直接丢掉,用户点完只看到界面刷新一下,不知道到底生没生效。
    body().querySelector("#rcx-fastest").onclick = async () => {
      const r = await bridge("/recodex/gateway/fastest", {});
      await render();
      const note = r && r.warning && r.warning.message;
      if (note) {
        const line = document.createElement("div");
        line.className = "rcx-err";
        line.style.cssText = "font-size:12px;margin-top:6px";
        line.textContent = note;
        body().appendChild(line);
      }
    };
    body().querySelector("#rcx-refresh").onclick = async () => { await bridge("/recodex/refresh-usage", {}); render(); };
    body().querySelector("#rcx-logout").onclick = async () => { await bridge("/recodex/logout", {}); render(); };
  }

  async function doLogin() {
    body().innerHTML = `<div class="rcx-muted">${t("正在发起登录…")}</div>`;
    const start = await bridge("/recodex/login/start", {});
    if (start.status !== "pending" || !start.data) {
      body().innerHTML = `<div class="rcx-err">${esc(start.error ? start.error.message : t("登录发起失败"))}</div>`;
      return;
    }
    const { user_code, verify_url } = start.data;
    body().innerHTML = `<div>${t("在浏览器打开并输入授权码:")}</div>
      <div class="rcx-row"><span class="rcx-k">${t("授权码")}</span><b>${esc(user_code)}</b></div>
      <div class="rcx-muted" style="word-break:break-all">${esc(verify_url)}</div>
      <button class="rcx-act" id="rcx-open">${t("打开授权页")}</button>
      <div class="rcx-muted" id="rcx-poll" style="margin-top:8px">${t("等待确认…")}</div>`;
    body().querySelector("#rcx-open").onclick = async () => {
      // verify_url 可能已含 user_code(backend 返回完整地址),避免重复拼接
      const url = /[?&]user_code=/.test(verify_url)
        ? verify_url
        : verify_url + (verify_url.includes("?") ? "&" : "?") + "user_code=" + encodeURIComponent(user_code);
      // 注入页里 window.open 常被 Electron 拦掉。原先直接调它并且把异常 catch 掉,
      // 于是用户点「打开授权页」什么都不会发生、也没有提示 —— 新用户在这一步就卡死了。
      // UI 助手那个按钮早因为同样原因改走桥了,登录这里漏了。
      const r = await bridge("/open-external", { url });
      if (r && r.status !== "error") return;
      try {
        if (window.open(url, "_blank")) return;
      } catch (e) {}
      // 两条路都不通:把地址亮出来,至少用户能自己复制
      const el = body().querySelector("#rcx-poll");
      if (el) { el.className = "rcx-err"; el.textContent = `${t("无法自动打开,请手动复制地址")}: ${url}`; }
    };
    // 轮询
    const deadline = Date.now() + 10 * 60 * 1000;
    const tick = async () => {
      // 面板重绘或切走之后这个节点就没了 —— 循环必须跟着停。
      // 原先它会闷头跑满 10 分钟;用户再点一次登录,就是两个循环并行轮询。
      if (!body().querySelector("#rcx-poll")) return;
      if (Date.now() > deadline) { const el = body().querySelector("#rcx-poll"); if (el) el.textContent = t("授权超时,请重试"); return; }
      const poll = await bridge("/recodex/login/poll", {});
      if (poll.status === "approved") { showLoginDone(); return; }
      if (poll.status === "error") { const el = body().querySelector("#rcx-poll"); if (el) { el.className = "rcx-err"; el.textContent = poll.error ? poll.error.message : t("登录失败"); } return; }
      setTimeout(tick, 5000);
    };
    setTimeout(tick, 5000);
  }

  // 登录已批准:config/auth/key 已写入,但官方 Codex 早在登录前就启动了,读不到新配置。
  // 首次登录必须重启整个 ReCodex(setx 已把 key 持久化到用户环境,重开后新起的 Codex 才生效)。
  function showLoginDone() {
    body().innerHTML =
      `<div style="color:#3ee98a;font-weight:600;font-size:14px">${t("✅ 登录成功")}</div>` +
      // 句中有 <b> 标记,拆成三段分别翻译,避免把 HTML 塞进词条
      `<div class="rcx-muted" style="margin-top:8px">${t("首次登录需重启生效:请")}<b style="color:#e6e9ef">${t("完全退出 Codex,再双击桌面 ReCodex 重新打开")}</b>${t(",官方界面才会用上你的账号(无需再登 ChatGPT)。")}</div>` +
      `<button class="rcx-act sec" id="rcx-recheck" style="margin-top:12px">${t("我已重启,刷新状态")}</button>`;
    const btn = body().querySelector("#rcx-recheck");
    if (btn) btn.onclick = render;
  }

  // ── 增强开关(Codex++ 增强,经 /settings 桥,与 recodex 登录无关)──
  // 键名必须与后端设置一致(camelCase);snake_case 会被服务端静默忽略,表现为「点了没反应」。
  const ENH = [
    ["codexAppSessionDelete", "会话删除"],
    ["codexAppMarkdownExport", "Markdown 导出"],
    ["codexAppConversationView", "对话居中宽度"],
    ["codexAppThreadIdBadge", "会话 ID 标识"],
    ["codexAppPasteFix", "粘贴修复(需重启)"],
    ["codexAppFastStartup", "Fast 按钮"],
    ["codexAppModelWhitelistUnlock", "模型白名单解锁"],
    ["codexAppPluginMarketplaceUnlock", "插件市场解锁"],
    ["codexAppPetRealMouseLook", "桌宠跟随真实鼠标"],
    ["codexAppThreadScrollRestore", "切换对话保留位置"],
    ["codexAppForceChineseLocale", "强制中文界面"],
  ];

  // 高级页签的开关:低频 + 偏系统集成,和日常增强分开放
  const ENH_ADV = [
    ["codexAppNativeMenuPlacement", "原生菜单栏位置"],
    ["codexAppNativeMenuLocalization", "原生菜单本地化"],
    ["codexAppZedRemoteOpen", "Zed Remote 打开"],
    ["codexAppUpstreamWorktreeCreate", "上游 worktree 创建"],
  ];
  // 开关列表渲染:增强页与高级页共用同一套读写路径
  // 开关写失败时的提示条。挂在开关列表下方,成功时清空。
  function toggleNote(container, message) {
    if (!container) return;
    let note = container.querySelector(".rcx-toggle-note");
    if (!message) { if (note) note.remove(); return; }
    if (!note) {
      note = document.createElement("div");
      note.className = "rcx-err rcx-toggle-note";
      note.style.cssText = "margin-top:6px;font-size:12px";
      container.appendChild(note);
    }
    note.textContent = message;
  }

  function renderToggleList(container, items, settings) {
    container.innerHTML = items
      .map(([k, label]) =>
        `<label class="rcx-toggle"><span>${esc(t(label))}</span>` +
        `<input type="checkbox" data-k="${esc(k)}" ${settings[k] ? "checked" : ""}></label>`
      )
      .join("");
    // 写完必须**回读校验**。原先是发射后不管:不 await、不看结果、不回滚,
    // 写失败时勾选框仍停在新状态,用户以为生效了 —— 这正是「开关不灵」的体感来源
    // (界面说开着,后端其实是关的,重开面板才发现又变回去了)。
    // 回读比"看写入返回值"可靠:两条写入路径(renderer 快通道 / 桥)返回形状并不一致。
    bindToggles(container);
  }

  async function readSettings() {
    if (typeof window.__codexPlusGetBackendSettings === "function") {
      return window.__codexPlusGetBackendSettings() || {};
    }
    const s = await bridge("/settings/get", {});
    return s && typeof s === "object" && !s.error ? s : {};
  }

  const TIER_MODES = [
    ["inherit", "继承"],
    ["global-standard", "全局 Standard"],
    ["global-fast", "全局 Fast"],
    ["custom", "自定义"],
  ];

  // ── 桥不可用时的统一降级 ────────────────────────────────────
  // 桥断了(launcher 没起来 / CDP binding 掉了)时,各页签原先各自"就地编造":
  //   增强页照常画出 11 个开关 —— 点了没反应,也不说为什么;
  //   微信页说"未绑定微信" —— 其实是不知道,把"没答复"当成了"答复是没绑定";
  //   高级页少画几块也不吭声。
  // 只有账号页做对了。把它那套统一出来:说清楚桥没就绪,给一个重试。
  function bridgeUnavailable(res) {
    if (!res) return true;
    const code = res.error && res.error.code;
    return res.status === "error" &&
      (code === "no_bridge" || code === "bridge_error" || code === "bridge_timeout");
  }

  function renderBridgeDown(box, retry) {
    if (!box) return;
    box.innerHTML = `<div class="rcx-err">${t("ReCodex 桥未就绪")}</div>` +
      `<button class="rcx-act sec" id="rcx-bridge-retry">${t("重试")}</button>`;
    box.querySelector("#rcx-bridge-retry").onclick = retry;
  }

  async function renderEnhancements() {
    const c = panel.querySelector("#recodex-enh");
    if (!c) return;
    const probe = await bridge("/backend/settings", {});
    if (bridgeUnavailable(probe)) { renderBridgeDown(c, renderEnhancements); return; }
    const settings = await readSettings();
    c.innerHTML = `<div id="rcx-enh-toggles"></div>` +
      `<div id="rcx-enh-width" style="margin-top:10px"></div>` +
      `<div id="rcx-enh-tier" style="margin-top:14px;border-top:1px solid #23272f;padding-top:10px"></div>`;
    renderToggleList(c.querySelector("#rcx-enh-toggles"), ENH, settings);
    renderWidthField(c.querySelector("#rcx-enh-width"), settings);
    renderServiceTier(c.querySelector("#rcx-enh-tier"), settings);
  }

  // 对话居中宽度:数值没有后端键(存在 renderer 的 localStorage),走它暴露的 setter。
  // 只在「对话居中宽度」开关打开时才显示,关着时给个数值框没有意义。
  function renderWidthField(box, settings) {
    if (!box) return;
    if (!settings.codexAppConversationView) { box.innerHTML = ""; return; }
    const all = typeof window.__codexPlusGetSettings === "function" ? window.__codexPlusGetSettings() : {};
    const width = Number(all.conversationViewMaxWidth) || 900;
    box.innerHTML = `<div class="rcx-field"><label>${t("居中宽度(px)")}</label>` +
      `<input id="rcx-width" type="number" min="320" max="4000" step="10" value="${width}"></div>`;
    const input = box.querySelector("#rcx-width");
    input.onchange = async () => {
      const v = Math.max(320, Math.min(4000, Math.round(Number(input.value) || 900)));
      input.value = v;
      if (typeof window.__codexPlusSetSetting !== "function") {
        toggleNote(box, t("设置没有生效,请重试"));
        return;
      }
      await window.__codexPlusSetSetting("conversationViewMaxWidth", v);
      // 同样回读:写了不等于生效
      const now = typeof window.__codexPlusGetSettings === "function" ? window.__codexPlusGetSettings() : {};
      const actual = Number(now.conversationViewMaxWidth);
      if (actual === v) { toggleNote(box, ""); return; }
      input.value = actual || width;
      toggleNote(box, t("设置没有生效,请重试"));
    };
  }

  // 服务模式:四选一 + 当前 config.toml 值回显。逻辑全在 renderer 侧(含 Fast 可用性校验),
  // 面板只负责画 UI 和转发,不复刻判断。
  function renderServiceTier(box, settings) {
    if (!box) return;
    if (!settings.codexAppServiceTierControls) {
      box.innerHTML = `<label class="rcx-toggle"><span>${esc(t("服务模式控件"))}</span>` +
        `<input type="checkbox" data-k="codexAppServiceTierControls"></label>`;
      bindToggles(box);
      return;
    }
    const api = window.__codexPlusServiceTier;
    const st = api && typeof api.get === "function" ? api.get() : null;
    let html = `<label class="rcx-toggle"><span>${esc(t("服务模式控件"))}</span>` +
      `<input type="checkbox" data-k="codexAppServiceTierControls" checked></label>`;
    if (st) {
      html += `<div class="rcx-muted" style="margin:6px 0 4px;font-size:12px">` +
        `${t("继承 config.toml")}: ${esc(st.configServiceTier || "—")}</div>`;
      html += `<div style="display:flex;flex-wrap:wrap;gap:4px">` +
        TIER_MODES.map(([m, label]) =>
          `<button class="rcx-act sec" data-tier="${m}" style="flex:1 1 46%;margin-top:0;padding:5px 4px;font-size:12px${
            st.controlMode === m ? ";background:#10a37f;color:#fff" : ""}">${esc(t(label))}</button>`
        ).join("") + `</div>`;
    } else {
      html += `<div class="rcx-muted" style="font-size:12px;margin-top:6px">${t("服务模式引擎未就绪")}</div>`;
    }
    box.innerHTML = html;
    bindToggles(box);
    box.querySelectorAll("button[data-tier]").forEach((b) => {
      b.onclick = () => {
        if (api && typeof api.setMode === "function") api.setMode(b.dataset.tier);
        setTimeout(renderEnhancements, 600);
      };
    });
  }

  // 开关写入的**唯一**实现。这里一度有两份:`renderToggleList` 里一份、`bindToggles`
  // 里一份,修了前者漏了后者 —— 服务模式那个开关走的正是后者,还留着老毛病。
  // 合成一个,新增调用点自然带上校验。
  function bindToggles(box) {
    box.querySelectorAll("input[data-k]").forEach((inp) => {
      inp.onchange = async () => {
        const k = inp.dataset.k, want = inp.checked;
        inp.disabled = true;
        let ok = false;
        try {
          if (typeof window.__codexPlusSetBackendSetting === "function") {
            await window.__codexPlusSetBackendSetting(k, want);
          } else {
            await bridge("/settings/set", { [k]: want });
          }
          // 回读校验:写入返回值不可靠(两条路径形状不一,快通道干脆没有返回值)
          const after = await readSettings();
          ok = !!after[k] === want;
          inp.checked = !!after[k];
        } catch (e) {
          inp.checked = !want;
        } finally {
          inp.disabled = false;
        }
        toggleNote(box, ok ? "" : t("设置没有生效,请重试"));
        // 这两个开关会改变页面结构(宽度框显隐 / 服务模式面板),生效了才重绘
        if (ok && (k === "codexAppConversationView" || k === "codexAppServiceTierControls")) {
          setTimeout(renderEnhancements, 500);
        }
      };
    });
  }

  // ── 微信连接:扫码登录 → 启停 → 配置(经 /weixin/* 桥,配置项走 /settings/set)──
  const WX_STATE_TEXT = {
    running: ["运行中", "on"], starting: ["启动中", "on"], retrying: ["重连中", "warn"],
    stopping: ["停止中", "warn"], error: ["出错", "warn"], stopped: ["已停止", ""], idle: ["未启动", ""],
  };
  let wxQrTimer = null;
  function wxStopPolling() {
    if (wxQrTimer) { clearTimeout(wxQrTimer); wxQrTimer = null; }
  }
  const wx = () => panel.querySelector("#recodex-wx");

  function wxNote(text, isError) {
    const c = wx();
    if (!c) return;
    let n = c.querySelector("#wx-note");
    if (!n) { n = document.createElement("div"); n.id = "wx-note"; n.style.marginTop = "8px"; c.appendChild(n); }
    n.className = isError ? "rcx-err" : "rcx-muted";
    n.style.fontSize = "12px";
    n.textContent = text;
  }

  /// 停止后轮询到真正 stopped 为止。长轮询最长 35s,期间 start 会被拒,
  /// 所以必须等确认停止再启动。progress 回调用于更新按钮文案。
  async function wxStopAndWait(progress) {
    await bridge("/weixin/stop", {});
    for (let i = 0; i < 30; i++) {
      if (progress) progress(`${t("停止中…")} ${i * 2}s`);
      await new Promise((r) => setTimeout(r, 2000));
      const s = await bridge("/weixin/status", {});
      const state = s && s.connect ? s.connect.state : "";
      if (state === "stopped" || state === "idle") return true;
    }
    return false;
  }

  async function renderWeixin() {
    const c = wx();
    if (!c) return;
    wxStopPolling();
    const res = await bridge("/weixin/status", {});
    if (bridgeUnavailable(res)) { renderBridgeDown(c, renderWeixin); return; }
    const cfg = (res && res.config) || {};
    const conn = (res && res.connect) || {};
    const [stateKey, stateCls] = WX_STATE_TEXT[conn.state] || WX_STATE_TEXT.idle;
    const stateText = t(stateKey);

    if (!cfg.hasToken) {
      c.innerHTML = `<div class="rcx-muted">${t("未绑定微信。扫码后可在微信里直接指挥本机 Codex。")}</div>
        <button class="rcx-act" id="wx-login">${t("扫码登录微信")}</button>`;
      c.querySelector("#wx-login").onclick = wxQrLogin;
      return;
    }

    let html = `<div class="rcx-row"><span class="rcx-k">${t("状态")}</span><span class="rcx-badge ${stateCls}">${esc(stateText)}</span></div>`;
    html += `<div class="rcx-row"><span class="rcx-k">${t("账号")}</span><span>${esc(cfg.accountId || "—")}</span></div>`;
    if (conn.processedMessages) html += `<div class="rcx-row"><span class="rcx-k">${t("已处理")}</span><span>${esc(conn.processedMessages)} ${t("条")}</span></div>`;
    if (conn.message) html += `<div class="rcx-muted" style="margin-top:4px">${esc(conn.message)}</div>`;
    const running = conn.state === "running" || conn.state === "starting" || conn.state === "retrying";
    html += running
      ? `<button class="rcx-act sec" id="wx-stop">${t("停止连接")}</button>`
      : `<button class="rcx-act" id="wx-start">${t("启动连接")}</button>`;
    // 空和 * 是两回事,不能合成一句:空 = 谁都不响应(连接根本起不来),
    // * = 任何人都能驱动本机 Codex。说反了会把用户往危险方向引。
    const allowFrom = String(cfg.allowFrom || "").trim();
    if (!allowFrom) {
      html += `<div class="rcx-err" style="margin-top:8px;font-size:12px">${t("⚠ 白名单为空:微信连接不会响应任何人。填入你的微信 ID,或填 * 放开所有人。")}</div>`;
    } else if (allowFrom === "*") {
      html += `<div class="rcx-err" style="margin-top:8px;font-size:12px">${t("⚠ 白名单为 *:任何人给该微信号发消息都能在本机运行 Codex。")}</div>`;
    }
    html += `<div class="rcx-field"><label>${t("工作目录(Codex 在此目录执行)")}</label>
      <input id="wx-workdir" value="${esc(cfg.workDir || "")}" placeholder="${t("留空 = 启动器当前目录")}"></div>`;
    html += `<div class="rcx-field"><label>${t("沙箱级别")}</label><select id="wx-sandbox">
      <option value="read-only">${t("read-only(只读,推荐)")}</option>
      <option value="workspace-write">${t("workspace-write(可改工作目录)")}</option>
      <option value="danger-full-access">${t("danger-full-access(完全放开)")}</option></select></div>`;
    html += `<div class="rcx-field"><label>${t("白名单(微信 user id,逗号分隔)")}</label>
      <input id="wx-allow" value="${esc(cfg.allowFrom || "")}" placeholder="${t("留空=不响应任何人")}"></div>`;
    html += `<div class="rcx-field"><label>${t("模型(留空=Codex 默认)")}</label>
      <input id="wx-model" value="${esc(cfg.model || "")}" placeholder="${t("如 gpt-5.6-sol")}"></div>`;
    html += `<button class="rcx-act sec" id="wx-save">${t("保存配置")}</button>`;
    c.innerHTML = html;
    const sandboxSel = c.querySelector("#wx-sandbox");
    if (sandboxSel) sandboxSel.value = cfg.sandbox || "read-only";
    const startBtn = c.querySelector("#wx-start");
    if (startBtn) startBtn.onclick = async () => {
      startBtn.disabled = true; startBtn.textContent = t("启动中…");
      const r = await bridge("/weixin/start", {});
      if (r && r.status !== "ok") { startBtn.disabled = false; startBtn.textContent = t("启动连接"); wxNote(r.message || t("启动失败"), true); return; }
      setTimeout(renderWeixin, 1500);
    };
    const stopBtn = c.querySelector("#wx-stop");
    if (stopBtn) stopBtn.onclick = async () => {
      stopBtn.disabled = true;
      await wxStopAndWait((t) => { stopBtn.textContent = t; });
      renderWeixin();
    };
    const saveBtn = c.querySelector("#wx-save");
    if (saveBtn) saveBtn.onclick = async () => {
      saveBtn.disabled = true; saveBtn.textContent = t("保存中…");
      const want = {
        workDir: c.querySelector("#wx-workdir").value.trim(),
        sandbox: c.querySelector("#wx-sandbox").value,
        allowFrom: c.querySelector("#wx-allow").value.trim(),
        model: c.querySelector("#wx-model").value.trim(),
      };
      await bridge("/settings/set", {
        weixinConnectWorkDir: want.workDir,
        weixinConnectSandbox: want.sandbox,
        weixinConnectAllowFrom: want.allowFrom,
        weixinConnectModel: want.model,
      });
      // 回读校验。这里**必须**较真:payload 里有白名单,写不进去却报「已保存」,
      // 就是给用户一个虚假的安全感 —— 他以为只有自己能触发,实际谁都能。
      const saved = ((await bridge("/weixin/status", {})) || {}).config || {};
      const same = ["workDir", "sandbox", "allowFrom", "model"]
        .every((key) => String(saved[key] || "") === String(want[key] || ""));
      if (!same) {
        saveBtn.disabled = false; saveBtn.textContent = t("保存配置");
        wxNote(t("配置没有保存成功,请重试"), true);
        return;
      }
      if (!running) { saveBtn.textContent = t("已保存"); setTimeout(renderWeixin, 1200); return; }
      // 配置只在启动连接时读取,所以保存后必须重启;这里代劳,免得用户在
      // 「停止中」阶段点启动被静默拒掉(停止要等长轮询结束,最长约 35 秒)。
      const ok = await wxStopAndWait((t) => { saveBtn.textContent = t; });
      if (!ok) { saveBtn.disabled = false; saveBtn.textContent = t("保存配置"); wxNote(t("停止超时,请稍后手动启动"), true); return; }
      saveBtn.textContent = t("重新启动连接…");
      const r = await bridge("/weixin/start", {});
      if (r && r.status !== "ok") wxNote(r.message || t("重启失败"), true);
      setTimeout(renderWeixin, 1500);
    };
  }

  async function wxQrLogin() {
    const c = wx();
    c.innerHTML = `<div class="rcx-muted">${t("正在生成二维码…")}</div>`;
    const r = await bridge("/weixin/qr-start", { baseUrl: "", routeTag: "" });
    if (!r || r.status !== "ok" || !r.qrSvg) {
      c.innerHTML = `<div class="rcx-err">${esc((r && r.message) || t("生成二维码失败"))}</div>
        <button class="rcx-act sec" id="wx-retry">${t("重试")}</button>`;
      c.querySelector("#wx-retry").onclick = wxQrLogin;
      return;
    }
    c.innerHTML = `<div class="rcx-muted">${t("用微信扫码并确认授权:")}</div>
      <div class="rcx-qr">${r.qrSvg}</div>
      <div class="rcx-muted" id="wx-qr-tip" style="margin-top:8px">${t("等待扫码…")}</div>
      <button class="rcx-act sec" id="wx-cancel" style="margin-top:8px">${t("取消")}</button>`;
    c.querySelector("#wx-cancel").onclick = () => { wxStopPolling(); renderWeixin(); };
    const deadline = Date.now() + 5 * 60 * 1000;
    const tick = async () => {
      const tip = panel.querySelector("#wx-qr-tip");
      if (!tip) { wxStopPolling(); return; }          // 面板已重绘,停止轮询
      if (Date.now() > deadline) { tip.textContent = t("二维码已过期,请重试"); wxStopPolling(); return; }
      const s = await bridge("/weixin/qr-status", {});
      if (s && s.qrStatus === "confirmed") { wxStopPolling(); renderWeixin(); return; }
      if (s && s.qrStatus === "scanned") tip.textContent = t("已扫码,请在手机上确认…");
      else if (s && s.status === "failed") { tip.className = "rcx-err"; tip.textContent = s.message || t("扫码失败"); wxStopPolling(); return; }
      wxQrTimer = setTimeout(tick, 2000);
    };
    wxQrTimer = setTimeout(tick, 2000);
  }

  // ── 官方侧边栏账号区接管 ────────────────────────────────────
  // 左下角原本显示 provider 名("ReCodex"),改成上游账号邮箱;并在个人资料菜单里
  // 插入额度进度。全部复用官方的 Tailwind token 类名,不自造样式,跟随明暗主题。
  let rcxAccount = null; // {email, plan, w5, w7}

  // 邮箱打码:只保留本地部分前两位,与前端展示口径一致(weiyukong550@gmail.com → we***@gmail.com)
  function maskEmail(email) {
    const at = String(email || "").indexOf("@");
    if (at <= 0) return email || "";
    return email.slice(0, Math.min(2, at)) + "***" + email.slice(at);
  }

  // ── 状态呼吸灯 ─────────────────────────────────────────────
  // 一个灯位只能表达一种状态,所以按「用户该先处理什么」排优先级:
  // 连不上(红) > 未登录(黄) > 登录了但额度打满(紫) > 正常(绿)。
  // 官方模式下整个灯隐藏(P4 会写入这个 flag,这里先读)。
  let rcxStatus = { cls: "ok", tip: "" };

  function officialMode() {
    try { return localStorage.getItem("recodex.officialMode") === "1"; } catch (e) { return false; }
  }

  // 时间戳只给到分钟:秒级噪声对"有没有更新"这个判断没帮助。
  function fmtTime(value) {
    const d = new Date(value);
    if (isNaN(d.getTime())) return String(value);
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  function computeStatus(res) {
    if (officialMode()) return { cls: "hidden", tip: t("官方模式") };
    if (!res || res.status === "error") {
      const msg = res && res.error ? res.error.message : t("无法连接");
      return { cls: "off", tip: `${t("连接中断")}: ${msg}` };
    }
    if (res.status === "signed_out") return { cls: "anon", tip: t("未登录 ReCodex。") };
    const windows = ((res.data || {}).usage || {}).windows || [];
    const full = windows.filter((w) => w && w.limit && w.used >= w.limit);
    if (full.length) {
      const names = full.map((w) => (w.window === "5h" ? t("5 小时") : t("7 天"))).join(" / ");
      return { cls: "quota", tip: `${t("额度已用尽")}: ${names}` };
    }
    // 颜色不加第五种 —— 红/黄/紫/绿是产品定死的语义。
    // 但 tip 必须说实话:绿灯配一份过期数据,等于用绿色替后端背书。
    if (res.status === "stale") {
      return { cls: "ok", tip: `${t("正常")}(${t("数据可能不是最新的")})` };
    }
    return { cls: "ok", tip: t("正常") };
  }

  function applyStatus() {
    [document.getElementById("recodex-fab-dot"), panel.querySelector("#rcx-title-dot")].forEach((d) => {
      if (!d) return;
      d.className = "rcx-dot " + rcxStatus.cls;
      d.title = rcxStatus.tip;
    });
    if (fab) fab.title = rcxStatus.tip ? `ReCodex — ${rcxStatus.tip}` : "ReCodex";
  }

  async function refreshAccountCache() {
    // 官方模式标记必须在这里同步:页面重载后 localStorage 里的值可能是上一次会话留下的,
    // 而侧边栏是否改写、状态灯是否隐藏都依赖它。这是一次本地文件存在性检查,很便宜。
    const mode = await bridge("/recodex/official-mode", {});
    if (mode && mode.data) {
      try { localStorage.setItem("recodex.officialMode", mode.data.official ? "1" : "0"); } catch (e) {}
    }
    const res = await bridge("/recodex/status", {});
    rcxStatus = computeStatus(res); // 复用这一次请求算状态灯,不额外发请求
    applyStatus();
    if (!res || !res.data) { rcxAccount = null; return; }
    const acc = res.data.account || {};
    const windows = (res.data.usage || {}).windows || [];
    rcxAccount = {
      email: acc.email || "",
      plan: acc.plan || acc.account_type || "",
      w5: windows.find((w) => w.window === "5h"),
      w7: windows.find((w) => w.window === "7d"),
    };
  }

  const sidebarAccountButton = () =>
    document.querySelector('div.absolute.inset-x-0.bottom-0.z-20 button[aria-haspopup="menu"]');

  // React 会重渲染覆盖掉我们写的文字,所以每次都比对后再写(也避免触发无限 MutationObserver)
  function patchSidebarLabel() {
    if (!rcxAccount || !rcxAccount.email) return;
    const btn = sidebarAccountButton();
    if (!btn) return;
    const masked = maskEmail(rcxAccount.email);
    const span = btn.querySelector("span.truncate") || btn.querySelector("span");
    if (span && span.textContent !== masked) {
      span.textContent = masked;
      span.title = rcxAccount.plan ? `${masked} · ${rcxAccount.plan}` : masked;
    }
  }

  // 官方菜单项的类名,照抄自实时 DOM,保证视觉与原生一致
  const RCX_MENU_ROW =
    "no-drag outline-hidden rounded-lg px-[var(--padding-row-x)] py-[var(--padding-row-y)] text-sm text-default cursor-default flex flex-col";

  function quotaRowHtml(label, w) {
    const p = pct(w);
    return (
      `<div class="${RCX_MENU_ROW}" data-recodex-quota="1" role="menuitem" aria-disabled="true" data-disabled="" tabindex="-1">` +
      `<div class="flex w-full items-center gap-1.5">` +
      `<span class="flex-1 min-w-0 truncate opacity-70">${esc(t(label))}</span>` +
      `<span class="opacity-70 tabular-nums">${p}%</span></div>` +
      `<div class="mt-1 h-[3px] w-full overflow-hidden rounded-full bg-text/10">` +
      `<div class="h-full rounded-full bg-text/60" style="width:${p}%"></div></div></div>`
    );
  }

  function patchProfileMenu(menu) {
    if (!rcxAccount || menu.dataset.recodexPatched === "1") return;
    const list = menu.querySelector("div.flex.w-full.min-w-0.flex-col");
    if (!list || !list.firstElementChild) return;
    const header = list.firstElementChild;
    const headSpan = header.querySelector("span");
    if (headSpan && rcxAccount.email) {
      headSpan.textContent = maskEmail(rcxAccount.email);
      if (rcxAccount.plan && !header.querySelector("[data-recodex-plan]")) {
        // 套餐单独一行,不跟邮箱抢宽度
        header.insertAdjacentHTML(
          "beforeend",
          `<div data-recodex-plan="1" class="mt-0.5 text-xs opacity-60">${esc(rcxAccount.plan)}</div>`
        );
      }
    }
    // 5h 窗口只在有实际用量时显示,7 天窗口常驻
    let html = "";
    if (rcxAccount.w5 && rcxAccount.w5.used > 0) html += quotaRowHtml(t("5 小时用量"), rcxAccount.w5);
    if (rcxAccount.w7) html += quotaRowHtml(t("7 天用量"), rcxAccount.w7);
    if (html) header.insertAdjacentHTML("afterend", html);
    menu.dataset.recodexPatched = "1";
  }

  // 官方模式下必须彻底退出官方 UI —— 用户切过去就是想用 ChatGPT 自己的账号,
  // 侧边栏和账号菜单都应还原成官方原样,不能再显示 ReCodex 的邮箱和额度。
  function removeOfficialUiPatches() {
    document.querySelectorAll("[data-recodex-quota]").forEach((node) => node.remove());
    document.querySelectorAll("[data-recodex-plan]").forEach((node) => node.remove());
    document
      .querySelectorAll('[role="menu"][data-recodex-patched]')
      .forEach((menu) => menu.removeAttribute("data-recodex-patched"));
  }

  function scanOfficialAccountUi() {
    if (officialMode()) {
      // React 会自己把侧边栏文字渲染回官方账号,我们只需停手并清掉注入的行
      removeOfficialUiPatches();
      return;
    }
    patchSidebarLabel();
    document
      .querySelectorAll('[role="menu"][data-radix-menu-content][data-state="open"]')
      .forEach(patchProfileMenu);
  }

  // 注入发生在 document-start:此时 CDP 桥未就绪、侧边栏也还没渲染,首次取号必然失败。
  // 所以要快速重试到拿着数据为止,不能把首屏依赖在 60 秒的保底轮询上(那会让用户干等一分钟)。
  async function ensureAccountSoon(attempt) {
    await refreshAccountCache();
    scanOfficialAccountUi();
    // 只在**还没拿到答复**时重试(桥没就绪 → 状态灯是 off)。
    // 原条件是"没拿到 email",于是未登录的用户每次启动都会把 40 次重试跑满 ——
    // 30 秒里往服务端打 40 发,而每发在服务端要拉账号+额度+网关三份数据。
    // 「未登录」是个确定答复,不是"还没准备好",没有重试的意义。
    const noAnswerYet = rcxStatus.cls === "off";
    const stillWaiting = noAnswerYet || (rcxAccount && !rcxAccount.email);
    if (stillWaiting && (attempt || 0) < 40) {
      setTimeout(() => ensureAccountSoon((attempt || 0) + 1), 750);
    }
  }
  ensureAccountSoon(0);
  // 菜单是打开时才创建的,侧边栏也会被 React 重渲染 → 观察 DOM 变化补写。
  // 注入脚本会在无 DOM 的环境里被求值(测试 harness),没有 MutationObserver 时直接跳过,
  // 否则整段脚本抛 ReferenceError,后面的逻辑全都不会执行。
  if (typeof MutationObserver === "function" && document.documentElement) {
    new MutationObserver(() => scanOfficialAccountUi()).observe(document.documentElement, {
      childList: true,
      subtree: true,
    });
  }
  // 保底轮询。原先是 setInterval 60 秒**无条件**打一次,而每次 /recodex/status
  // 在服务端要拉账号+额度+网关三份数据 —— 一台机器开一天就是 1440 轮。
  // 未登录、或者桥/服务端不通时,这些数据不会自己变好,继续每分钟敲没有意义,
  // 所以逐步退避到 5 分钟;一旦恢复正常立刻回到 60 秒。
  const POLL_BASE_MS = 60000;
  const POLL_MAX_MS = 5 * 60 * 1000;
  let pollDelay = POLL_BASE_MS;
  async function pollTick() {
    await refreshAccountCache();
    scanOfficialAccountUi();
    const idle = !rcxAccount || rcxStatus.cls === "anon" || rcxStatus.cls === "off";
    pollDelay = idle ? Math.min(pollDelay * 2, POLL_MAX_MS) : POLL_BASE_MS;
    setTimeout(pollTick, pollDelay);
  }
  setTimeout(pollTick, POLL_BASE_MS);

  // ── 版本与更新 ─────────────────────────────────────────────
  // 服务端用两个信号控制:
  //   update-channel.available → 有新版本可装(非强制)
  //   compatibility.supported=false → 当前版本已不受支持(强制更新)
  // 两者组合出「可更新 / 必须更新 / 已是最新」三种展示。
  async function renderUpdate(box) {
    if (!box) return;
    const res = await bridge("/recodex/check-client", {});
    if (!res || res.status === "error") {
      box.innerHTML = `<div class="rcx-err" style="font-size:12px">${
        esc((res && res.error && res.error.message) || t("无法检查更新"))}</div>` +
        `<button class="rcx-act sec" id="rcx-upd-retry">${t("重试")}</button>`;
      box.querySelector("#rcx-upd-retry").onclick = () => renderUpdate(box);
      return;
    }
    const data = res.data || {};
    const compat = data.compatibility || {};
    const channel = data.update_channel || {};
    const current = compat.client_version || "—";
    const forced = compat.supported === false;
    const hasUpdate = !!channel.available;
    // 服务端给的是机器码(already_latest / not_in_rollout / not_configured),
    // 直接打印给用户等于没说。认识的翻译掉,不认识的原样透出(便于排查)。
    const REASONS = {
      already_latest: t("已是最新版本"),
      not_in_rollout: t("你不在本次更新的推送名单内。"),
      not_configured: t("服务端尚未配置更新包。"),
    };
    const reasonText = channel.reason ? (REASONS[channel.reason] || channel.reason) : "";

    let html = `<div class="rcx-row"><span class="rcx-k">${t("当前版本")}</span><span>${esc(current)}</span></div>`;
    if (hasUpdate) {
      html += `<div class="rcx-row"><span class="rcx-k">${t("最新版本")}</span><span>${esc(channel.latest_version || "—")}</span></div>`;
    }
    if (forced) {
      // 「必须更新」+「没有更新可点」是个死角:后端抬了最低版本却没配更新包,
      // 或者这个用户不在灰度名单里。只说"必须更新"等于把人堵死,得说清楚下一步。
      const headline = hasUpdate
        ? t("当前版本已停止支持,必须更新后才能继续使用。")
        : t("当前版本已停止支持,但暂时没有可用的更新包。");
      html += `<div class="rcx-err" style="margin-top:6px;font-size:12px">${headline}${
        compat.minimum_version ? ` ${t("最低版本")}: ${esc(compat.minimum_version)}` : ""}${
        hasUpdate ? "" : `<br>${reasonText ? esc(reasonText) + " " : ""}${t("请联系管理员,或稍后重新检查。")}`}</div>`;
    }
    if (hasUpdate) {
      html += `<button class="rcx-act" id="rcx-upd-go">${forced ? t("立即更新(必需)") : t("更新到最新版")}</button>`;
    } else {
      // forced 分支已经把原因说过一遍了,这里再打一次就是重复
      if (!forced) {
        html += `<div class="rcx-muted" style="margin-top:6px;font-size:12px">${
          reasonText ? esc(reasonText) : t("已是最新版本")}</div>`;
      }
      html += `<button class="rcx-act sec" id="rcx-upd-check">${t("重新检查")}</button>`;
    }
    box.innerHTML = html;

    const check = box.querySelector("#rcx-upd-check");
    if (check) check.onclick = () => { box.innerHTML = `<div class="rcx-muted">${t("检查中…")}</div>`; renderUpdate(box); };
    const go = box.querySelector("#rcx-upd-go");
    if (go) go.onclick = () => startUpdate(box, channel);
  }

  async function startUpdate(box, channel) {
    box.innerHTML = `<div class="rcx-muted">${t("正在下载…")}</div>`;
    // 不传 manifest 地址:更新源由服务端说了算,桥会自己去问一次带认证的接口。
    // 页面能指定安装包地址 = 把「安装任意 exe」的权限交给渲染进程。
    const r = await bridge("/self-update", {});
    if (!r || r.status !== "ok") {
      box.innerHTML = `<div class="rcx-err" style="font-size:12px">${
        esc((r && r.message) || t("更新失败"))}</div>` +
        `<button class="rcx-act sec" id="rcx-upd-back">${t("返回")}</button>`;
      box.querySelector("#rcx-upd-back").onclick = () => renderUpdate(box);
      return;
    }
    box.innerHTML = `<div class="rcx-muted">${esc(r.message || t("更新完成,正在重启…"))}</div>`;
    setTimeout(() => bridge("/restart-codex", {}), 1200);
  }

  // ── 卸载(不可逆,两步确认)─────────────────────────────────
  // 第一次点只是展开确认,第二次才真的执行 —— 误触代价太大(会删程序本体)。
  function renderUninstall(box) {
    if (!box) return;
    box.innerHTML = `<button class="rcx-act sec" id="rcx-uninst" style="color:#ff8b80">${t("卸载 ReCodex")}</button>`;
    box.querySelector("#rcx-uninst").onclick = () => confirmUninstall(box);
  }

  function confirmUninstall(box) {
    box.innerHTML =
      `<div class="rcx-err" style="font-size:12px;line-height:1.5">${t("确定卸载?此操作不可撤销:")}<br>` +
      `· ${t("还原 Codex 配置并清除登录凭据")}<br>` +
      `· ${t("服务端吊销本设备")}<br>` +
      `· ${t("删除快捷方式与程序本体")}<br>` +
      // 用户脚本是用户自己写的东西,删之前必须明说 —— 不可恢复
      `· ${t("删除设备标识与用户脚本(不可恢复)")}</div>` +
      `<button class="rcx-act" id="rcx-uninst-no">${t("取消")}</button>` +
      `<button class="rcx-act sec" id="rcx-uninst-yes" style="color:#ff8b80">${t("确认卸载")}</button>`;
    box.querySelector("#rcx-uninst-no").onclick = () => renderUninstall(box);
    box.querySelector("#rcx-uninst-yes").onclick = async () => {
      const btn = box.querySelector("#rcx-uninst-yes");
      btn.disabled = true;
      btn.textContent = t("卸载中…");
      const r = await bridge("/uninstall", {});
      // 失败要红字显示并**留在原地** —— 原先无论成败都用灰字报一句然后照样退出,
      // 用户会以为卸干净了。而失败恰恰意味着配置没还原、程序也没删,什么都没变。
      if (!r || r.status !== "ok") {
        box.innerHTML = `<div class="rcx-err" style="font-size:12px">${
          esc((r && r.message) || t("卸载失败,未做任何改动"))}</div>` +
          `<button class="rcx-act sec" id="rcx-uninst-back">${t("返回")}</button>`;
        box.querySelector("#rcx-uninst-back").onclick = () => renderUninstall(box);
        return;
      }
      const warns = r.warnings || [];
      box.innerHTML = `<div class="rcx-muted" style="font-size:12px">${
        esc(r.message || t("卸载完成"))}</div>` +
        (warns.length ? `<div class="rcx-err" style="font-size:12px;margin-top:4px">${esc(warns.join(" / "))}</div>` : "");
      // 必须用 /quit 而不是 /restart-codex:后者会拉一个接班进程,
      // 把刚安排自删的 exe 重新锁住,清理脚本重试到超时放弃 ——
      // 结果是配置还了、设备吊销了,程序却还在跑、exe 还在磁盘上。
      setTimeout(() => bridge("/quit", {}), 1500);
    };
  }

  // ── 运行模式(ReCodex / 官方 ChatGPT)────────────────────────
  // 切换只改 ~/.codex 配置,Codex 是启动时读一次,所以必须重启才生效 ——
  // 直接给「切换并重启」一步到位,避免用户改完以为生效了。
  async function renderRunMode(box) {
    if (!box) return;
    const res = await bridge("/recodex/official-mode", {});
    const official = !!(res && res.data && res.data.official);
    try { localStorage.setItem("recodex.officialMode", official ? "1" : "0"); } catch (e) {}
    applyStatus(); // 官方模式下状态灯要隐藏

    box.innerHTML =
      `<div class="rcx-row"><span class="rcx-k">${t("当前")}</span>` +
      `<span>${official ? t("官方 ChatGPT") : "ReCodex"}</span></div>` +
      `<button class="rcx-act${official ? "" : " sec"}" id="rcx-mode-btn">` +
      `${official ? t("切回 ReCodex 并重启") : t("切到官方 ChatGPT 并重启")}</button>` +
      `<div class="rcx-muted" style="margin-top:4px;font-size:12px">${
        official ? t("切回不需要重新登录(凭据与 Codex 配置无关)。")
                 : t("临时改用官方账号;登录状态会保留,可随时切回。")}</div>`;

    box.querySelector("#rcx-mode-btn").onclick = async () => {
      const btn = box.querySelector("#rcx-mode-btn");
      btn.disabled = true;
      btn.textContent = t("切换中…");
      const r = await bridge(official ? "/recodex/official-mode/disable" : "/recodex/official-mode/enable", {});
      if (!r || r.status === "error") {
        btn.disabled = false;
        btn.textContent = t("切换失败");
        return;
      }
      btn.textContent = t("正在重启 Codex…");
      await bridge("/restart-codex", {});
    };
  }

  // ── 高级 ───────────────────────────────────────────────────
  async function renderAdvanced() {
    const c = panel.querySelector("#recodex-adv");
    if (!c) return;
    const cur = currentLang();
    // 界面语言纯本地,桥断了也该能切;其余模块要如实说明不可用
    const probe = await bridge("/recodex/official-mode", {});
    const down = bridgeUnavailable(probe);
    let html = `<div class="rcx-field"><label>${t("界面语言")}</label>` +
      `<select id="rcx-lang">` +
      `<option value="zh"${cur === "zh" ? " selected" : ""}>简体中文</option>` +
      `<option value="tw"${cur === "tw" ? " selected" : ""}>繁體中文</option>` +
      `<option value="ru"${cur === "ru" ? " selected" : ""}>Русский</option>` +
      `</select></div>`;
    html += `<div class="rcx-muted" style="margin-top:6px;font-size:12px">${t("跟随 Codex 语言;英语等未支持语种显示简体中文。")}</div>`;
    if (down) {
      html += `<div class="rcx-err" style="margin-top:10px;font-size:12px">${
        t("ReCodex 桥未就绪")}</div>` +
        `<button class="rcx-act sec" id="rcx-adv-retry">${t("重试")}</button>`;
      c.innerHTML = html;
      c.querySelector("#rcx-adv-retry").onclick = renderAdvanced;
      const langSel = c.querySelector("#rcx-lang");
      if (langSel) langSel.onchange = () => { setLang(langSel.value); renderTabLabels(); renderAdvanced(); };
      return;
    }
    html += `<div style="margin-top:14px;border-top:1px solid #23272f;padding-top:10px">`;
    html += `<div class="rcx-k" style="margin-bottom:4px">${t("系统集成")}</div>`;
    html += `<div id="rcx-adv-toggles"></div></div>`;
    html += `<div style="margin-top:14px;border-top:1px solid #23272f;padding-top:10px">`;
    html += `<div class="rcx-k" style="margin-bottom:4px">${t("版本与更新")}</div>`;
    html += `<div id="rcx-update"><div class="rcx-muted">${t("加载中…")}</div></div></div>`;
    html += `<div style="margin-top:14px;border-top:1px solid #23272f;padding-top:10px">`;
    html += `<div class="rcx-k" style="margin-bottom:4px">${t("运行模式")}</div>`;
    html += `<div id="rcx-mode"><div class="rcx-muted">${t("加载中…")}</div></div></div>`;
    html += `<div style="margin-top:14px;border-top:1px solid #23272f;padding-top:10px">`;
    html += `<button class="rcx-act sec" id="rcx-uiassist">${t("UI 助手")}</button>`;
    html += `<div class="rcx-muted" style="margin-top:4px;font-size:12px">${t("打开 MotionSites,快速生成前端界面。")}</div></div>`;
    html += `<div style="margin-top:16px;border-top:1px solid #3a2320;padding-top:10px">`;
    html += `<div id="rcx-uninstall"></div></div>`;
    c.innerHTML = html;

    renderUpdate(c.querySelector("#rcx-update"));
    renderRunMode(c.querySelector("#rcx-mode"));
    renderUninstall(c.querySelector("#rcx-uninstall"));

    const advToggles = c.querySelector("#rcx-adv-toggles");
    if (advToggles) renderToggleList(advToggles, ENH_ADV, await readSettings());

    const sel = c.querySelector("#rcx-lang");
    if (sel) sel.onchange = () => { setLang(sel.value); renderTabLabels(); renderAdvanced(); };
    const ui = c.querySelector("#rcx-uiassist");
    // 注入页里 window.open 常被 Electron 拦掉,走桥用系统浏览器打开
    if (ui) ui.onclick = async () => {
      const r = await bridge("/open-external", { url: "https://motionsites.dev/" });
      if (!r || r.status === "error") { try { window.open("https://motionsites.dev/", "_blank"); } catch (e) {} }
    };
  }

  fab.onclick = () => {
    const open = panel.classList.toggle("open");
    if (open) {
      renderTabLabels(); // 语言在 document-start 读不到,每次打开按当前语言重写
      applyStatus();
      renderActiveTab();
    } else {
      wxStopPolling();
    }
  };
})();
