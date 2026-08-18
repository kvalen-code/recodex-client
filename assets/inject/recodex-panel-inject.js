// recodex-overlay: ReCodex in-page 面板。注入官方 ChatGPT/Codex 页面,提供一个悬浮
// 「ReCodex」按钮 → 点开面板(账号/额度/网关/登录)。所有数据经 CDP 桥 /recodex/* 取,
// 逻辑在 recodex-integration crate 的 desktop 模块;本文件只画 UI + 调桥。
// 幂等:重注入(SPA 跳转)只装一次。
(() => {
  "use strict";
  if (window.__recodexPanelInstalled === "1") return;
  window.__recodexPanelInstalled = "1";

  // ── 调桥封装 ───────────────────────────────────────────────
  function bridge(path, payload) {
    const fn = window.__codexSessionDeleteBridge;
    if (typeof fn !== "function") {
      return Promise.resolve({ status: "error", error: { code: "no_bridge", message: "ReCodex 桥未就绪" } });
    }
    return Promise.resolve(fn(path, payload || {})).catch((e) => ({
      status: "error",
      error: { code: "bridge_error", message: String(e && e.message ? e.message : e) },
    }));
  }

  // ── 样式 ───────────────────────────────────────────────────
  const style = document.createElement("style");
  style.textContent = `
    #recodex-fab{position:fixed;right:18px;bottom:18px;z-index:2147483000;width:44px;height:44px;border-radius:50%;
      background:#10a37f;color:#fff;border:none;cursor:pointer;font:600 13px system-ui,sans-serif;box-shadow:0 2px 10px rgba(0,0,0,.25)}
    #recodex-fab:hover{background:#0e8e6e}
    #recodex-panel{position:fixed;right:18px;bottom:72px;z-index:2147483000;width:320px;max-height:70vh;overflow:auto;
      background:#1b1e24;color:#e6e9ef;border:1px solid #2c313a;border-radius:12px;box-shadow:0 8px 30px rgba(0,0,0,.45);
      font:13px/1.5 system-ui,sans-serif;padding:16px;display:none}
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
  `;
  document.documentElement.appendChild(style);

  // ── DOM ────────────────────────────────────────────────────
  const fab = document.createElement("button");
  fab.id = "recodex-fab";
  fab.textContent = "Rx";
  fab.title = "ReCodex";
  const panel = document.createElement("div");
  panel.id = "recodex-panel";
  panel.innerHTML = `<h3>🟢 ReCodex</h3><div id="recodex-body"><div class="rcx-muted">加载中…</div></div>`
    + `<div style="margin-top:14px;border-top:1px solid #23272f;padding-top:10px">`
    + `<div class="rcx-k" style="margin-bottom:4px">增强功能</div>`
    + `<div id="recodex-enh"><div class="rcx-muted">加载中…</div></div></div>`;
  document.documentElement.appendChild(fab);
  document.documentElement.appendChild(panel);

  const body = () => panel.querySelector("#recodex-body");
  const esc = (s) => String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

  function pct(w) {
    if (!w || !w.limit) return 0;
    return Math.min(100, Math.max(0, Math.round((w.used / w.limit) * 100)));
  }

  // ── 渲染 ───────────────────────────────────────────────────
  async function render() {
    body().innerHTML = `<div class="rcx-muted">加载中…</div>`;
    const res = await bridge("/recodex/status", {});
    if (res.status === "signed_out") {
      body().innerHTML = `<div class="rcx-muted">未登录 ReCodex。</div>
        <button class="rcx-act" id="rcx-login">登录 ReCodex</button>`;
      body().querySelector("#rcx-login").onclick = doLogin;
      return;
    }
    if (res.status === "error" || !res.data) {
      const msg = res.error ? res.error.message : "无法读取状态";
      body().innerHTML = `<div class="rcx-err">${esc(msg)}</div>
        <button class="rcx-act sec" id="rcx-retry">重试</button>`;
      body().querySelector("#rcx-retry").onclick = render;
      return;
    }
    const d = res.data;
    const acc = d.account || {};
    const u = d.usage || {};
    const w5 = (u.windows || []).find((w) => w.window === "5h");
    const w7 = (u.windows || []).find((w) => w.window === "7d");
    const gws = d.gateways || [];
    const sel = d.selected_gateway;
    let html = "";
    html += `<div class="rcx-row"><span class="rcx-k">邮箱</span><span>${esc(acc.email || "—")}</span></div>`;
    html += `<div class="rcx-row"><span class="rcx-k">套餐</span><span>${esc(acc.plan || acc.account_type || "—")}</span></div>`;
    if (w5) html += `<div style="padding:6px 0"><div class="rcx-row" style="border:0"><span class="rcx-k">5 小时</span><span>${pct(w5)}%</span></div><div class="rcx-bar"><i style="width:${pct(w5)}%"></i></div></div>`;
    if (w7) html += `<div style="padding:6px 0"><div class="rcx-row" style="border:0"><span class="rcx-k">7 天</span><span>${pct(w7)}%</span></div><div class="rcx-bar"><i style="width:${pct(w7)}%"></i></div></div>`;
    html += `<div class="rcx-row"><span class="rcx-k">网关</span><span>${esc(sel ? sel.name : "未选")}</span></div>`;
    html += `<button class="rcx-act" id="rcx-fastest">用最快网关</button>`;
    html += `<button class="rcx-act sec" id="rcx-refresh">刷新额度</button>`;
    html += `<button class="rcx-act sec" id="rcx-logout">登出</button>`;
    body().innerHTML = html;
    body().querySelector("#rcx-fastest").onclick = async () => { await bridge("/recodex/gateway/fastest", {}); render(); };
    body().querySelector("#rcx-refresh").onclick = async () => { await bridge("/recodex/refresh-usage", {}); render(); };
    body().querySelector("#rcx-logout").onclick = async () => { await bridge("/recodex/logout", {}); render(); };
  }

  async function doLogin() {
    body().innerHTML = `<div class="rcx-muted">正在发起登录…</div>`;
    const start = await bridge("/recodex/login/start", {});
    if (start.status !== "pending" || !start.data) {
      body().innerHTML = `<div class="rcx-err">${esc(start.error ? start.error.message : "登录发起失败")}</div>`;
      return;
    }
    const { user_code, verify_url } = start.data;
    body().innerHTML = `<div>在浏览器打开并输入授权码:</div>
      <div class="rcx-row"><span class="rcx-k">授权码</span><b>${esc(user_code)}</b></div>
      <div class="rcx-muted" style="word-break:break-all">${esc(verify_url)}</div>
      <button class="rcx-act" id="rcx-open">打开授权页</button>
      <div class="rcx-muted" id="rcx-poll" style="margin-top:8px">等待确认…</div>`;
    body().querySelector("#rcx-open").onclick = () => { try { window.open(verify_url + "?user_code=" + encodeURIComponent(user_code), "_blank"); } catch (e) {} };
    // 轮询
    const deadline = Date.now() + 10 * 60 * 1000;
    const tick = async () => {
      if (Date.now() > deadline) { const el = body().querySelector("#rcx-poll"); if (el) el.textContent = "授权超时,请重试"; return; }
      const poll = await bridge("/recodex/login/poll", {});
      if (poll.status === "approved") { render(); return; }
      if (poll.status === "error") { const el = body().querySelector("#rcx-poll"); if (el) { el.className = "rcx-err"; el.textContent = poll.error ? poll.error.message : "登录失败"; } return; }
      setTimeout(tick, 5000);
    };
    setTimeout(tick, 5000);
  }

  // ── 增强开关(Codex++ 增强,经 /settings 桥,与 recodex 登录无关)──
  const ENH = [
    ["codex_app_session_delete", "会话删除"],
    ["codex_app_markdown_export", "Markdown 导出"],
    ["codex_app_conversation_view", "会话项目移动"],
    ["codex_app_thread_id_badge", "会话 ID 标识"],
    ["codex_app_paste_fix", "粘贴修复"],
    ["codex_app_fast_startup", "Fast 按钮"],
    ["codex_app_model_whitelist_unlock", "模型白名单解锁"],
    ["codex_app_plugin_marketplace_unlock", "插件市场解锁"],
    ["codex_app_pet_real_mouse_look", "桌宠跟随真实鼠标"],
    ["codex_app_stepwise_enabled", "Stepwise"],
    ["codex_app_dream_skin_enabled", "皮肤"],
  ];
  async function renderEnhancements() {
    const c = panel.querySelector("#recodex-enh");
    if (!c) return;
    const s = await bridge("/settings/get", {});
    const settings = s && typeof s === "object" && !s.error ? s : {};
    c.innerHTML = ENH.map(
      ([k, label]) =>
        `<label class="rcx-toggle"><span>${esc(label)}</span>` +
        `<input type="checkbox" data-k="${esc(k)}" ${settings[k] ? "checked" : ""}></label>`
    ).join("");
    c.querySelectorAll("input[data-k]").forEach((inp) => {
      inp.onchange = () => {
        bridge("/settings/set", { [inp.dataset.k]: inp.checked });
      };
    });
  }

  fab.onclick = () => {
    const open = panel.classList.toggle("open");
    if (open) {
      render();
      renderEnhancements();
    }
  };
})();
