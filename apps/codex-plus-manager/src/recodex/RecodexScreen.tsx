import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Download, KeyRound, LogIn, LogOut, RefreshCw, Send, Zap } from "lucide-react";
import { getLanguage } from "@/i18n";
import "./RecodexScreen.css";

type Gateway = { id: string; name: string; client_latency_ms?: number; healthy: boolean; enabled?: boolean; maintenance?: boolean; selected: boolean };
type Organization = { id: number; kind?: string; name: string; member_count?: number; plan_name?: string; is_current?: boolean };
type UsageWindow = { window: string; limit: number; used: number; remaining: number; reset_at?: string };
type Usage = { account_type: string; available: number; total: number; used: number; windows?: UsageWindow[]; refreshed_at: string; source: string; stale: boolean; refresh_error?: { code: string; message: string } };
type Account = { display_name?: string; email?: string; plan?: string; account_type: string };
type ClientCompatibility = { client_version: string; supported: boolean; minimum_version: string };
type UpdateChannel = { channel: string; available: boolean; latest_version?: string; manifest_url?: string; reason?: string };
type AdapterEnvelope = {
  status: string;
  data?: { account?: Account; usage?: Usage; gateways?: Gateway[]; selected_gateway?: Gateway; account_error?: string; gateway_error?: string; user_code?: string; verify_url?: string; compatibility?: ClientCompatibility; update_channel?: UpdateChannel; diagnostics?: { status: string }; organizations?: Organization[]; org_id?: number; org_name?: string; plan_name?: string };
  error?: { code: string; message: string };
};

// Self-contained bilingual copy for the ReCodex panel. The manager reloads the
// webview on language switch (see i18n.ts), so reading the language once here is
// safe and keeps this overlay component independent of the source-keyed t().
const RECODEX_EN = getLanguage() === "en";
const tr = (zh: string, en: string): string => (RECODEX_EN ? en : zh);

function accountTypeLabel(value: string): string {
  switch (value) {
    case "exclusive":
    case "dedicated":
      return tr("独享", "Dedicated");
    case "shared":
      return tr("共享", "Shared");
    default:
      return tr("未知", "Unknown");
  }
}

function statusLabel(status: string): string {
  switch (status) {
    case "loading": return tr("加载中", "loading");
    case "ready": return tr("就绪", "ready");
    case "stale": return tr("缓存", "stale");
    case "signed_out": return tr("未登录", "signed out");
    case "error": return tr("错误", "error");
    default: return status.replaceAll("_", " ");
  }
}

function redactError(value: unknown): string {
  const text = typeof value === "string"
    ? value
    : value instanceof Error
      ? value.message
      : String(value ?? "Request failed");
  return text
    .replace(/Bearer\s+[^\s]+/gi, "Bearer [redacted]")
    .replace(/\brct_[A-Za-z0-9._~-]+/g, "[redacted]")
    .replace(/\bsk-[A-Za-z0-9._~-]+/g, "[redacted]")
    .replace(/["']?\b(access_token|refresh_token|id_token|token|api[_-]?key|client_secret|password)\b["']?\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,}&]+)/gi, "$1=[redacted]")
    .slice(0, 240) || "Request failed";
}

export function RecodexScreen() {
  const [state, setState] = useState<AdapterEnvelope>({ status: "loading" });
  const [login, setLogin] = useState<AdapterEnvelope | null>(null);
  const [clientInfo, setClientInfo] = useState<AdapterEnvelope | null>(null);
  const [diagnostic, setDiagnostic] = useState<AdapterEnvelope | null>(null);
  const [busy, setBusy] = useState(false);
  // 组织列表单独放:拉失败不该把整个面板打成错误态 ——
  // 切换是附加能力,而账号和用量才是这个面板的主功能。
  const [orgs, setOrgs] = useState<Organization[]>([]);

  const setStateIPCError = (error: unknown) => {
    const message = redactError(error);
    setState((previous) => ({ status: "error", data: previous.data, error: { code: "ipc_error", message } }));
  };
  const setSnapshotResult = (result: AdapterEnvelope) => {
    if (result.status === "error" && !result.data) {
      setState((previous) => ({ ...result, data: previous.data }));
      return;
    }
    setState(result);
  };
  const shouldRefreshAfterAction = (result: AdapterEnvelope) => {
    if (result.status === "ready") {
      const selected = result.data?.selected_gateway;
      if (selected) {
        setState((previous) => {
          if (!previous.data) return previous;
          const gateways = previous.data.gateways ?? [];
          const matched = gateways.some((gateway) => gateway.id === selected.id);
          const updatedGateways = gateways.map((gateway) =>
            gateway.id === selected.id ? selected : { ...gateway, selected: false },
          );
          if (!matched) updatedGateways.push(selected);
          return {
            ...previous,
            data: { ...previous.data, gateways: updatedGateways, selected_gateway: selected },
          };
        });
      }
      return true;
    }
    if (result.status === "signed_out") {
      setState(result);
      return false;
    }
    setState((previous) => ({
      status: result.status,
      data: previous.data ?? result.data,
      error: result.error,
    }));
    return false;
  };

  const load = async () => {
    setBusy(true);
    try { setSnapshotResult(await invoke<AdapterEnvelope>("recodex_status")); }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const refresh = async () => {
    setBusy(true);
    try { setSnapshotResult(await invoke<AdapterEnvelope>("recodex_refresh_usage")); }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const startLogin = async () => {
    setBusy(true);
    try { setLogin(await invoke<AdapterEnvelope>("recodex_login_start")); }
    catch (error) { setLogin({ status: "error", error: { code: "ipc_error", message: redactError(error) } }); }
    finally { setBusy(false); }
  };
  const pollLogin = async () => {
    setBusy(true);
    try {
      const result = await invoke<AdapterEnvelope>("recodex_login_poll");
      if (result.status === "approved") { setLogin(null); await refresh(); }
      else {
        setLogin((previous) => ({
          status: result.status,
          data: previous?.data ?? result.data,
          error: result.error,
        }));
      }
    } catch (error) {
      const message = redactError(error);
      setLogin((previous) => ({
        status: "error",
        data: previous?.data,
        error: { code: "ipc_error", message },
      }));
    }
    finally { setBusy(false); }
  };
  const logout = async () => {
    setBusy(true);
    try { setState(await invoke<AdapterEnvelope>("recodex_logout")); setLogin(null); }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const selectGateway = async (id: string) => {
    setBusy(true);
    try {
      const result = await invoke<AdapterEnvelope>("recodex_select_gateway", { id });
      if (shouldRefreshAfterAction(result)) await refresh();
    }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const loadOrgs = async () => {
    try {
      const result = await invoke<AdapterEnvelope>("recodex_organizations");
      setOrgs(result.status === "ready" ? (result.data?.organizations ?? []) : []);
    } catch {
      // 静默:服务端版本旧(501)或网络抖动时不显示切换器,而不是弹错。
      setOrgs([]);
    }
  };
  const switchOrg = async (id: number) => {
    setBusy(true);
    try {
      const result = await invoke<AdapterEnvelope>("recodex_switch_org", { orgId: id });
      // 切完必须刷新:组织决定用哪个 AI 账号、走哪份额度,
      // 不刷的话面板上的用量还是上一个组织的 —— 用户会以为切换没生效。
      if (shouldRefreshAfterAction(result)) await refresh();
      if (result.status === "ready") await loadOrgs();
    }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const useFastestGateway = async () => {
    setBusy(true);
    try {
      const result = await invoke<AdapterEnvelope>("recodex_use_fastest_gateway");
      if (shouldRefreshAfterAction(result)) await refresh();
    }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const checkClient = async () => {
    setBusy(true);
    try { setClientInfo(await invoke<AdapterEnvelope>("recodex_check_client")); }
    catch (error) { setClientInfo({ status: "error", error: { code: "ipc_error", message: redactError(error) } }); }
    finally { setBusy(false); }
  };
  const refreshSession = async () => {
    setBusy(true);
    try {
      const result = await invoke<AdapterEnvelope>("recodex_refresh_token");
      if (shouldRefreshAfterAction(result)) await load();
    }
    catch (error) { setStateIPCError(error); }
    finally { setBusy(false); }
  };
  const reportDiagnostics = async () => {
    setBusy(true);
    try { setDiagnostic(await invoke<AdapterEnvelope>("recodex_report_diagnostics")); }
    catch (error) { setDiagnostic({ status: "error", error: { code: "ipc_error", message: redactError(error) } }); }
    finally { setBusy(false); }
  };
  useEffect(() => {
    const action = window.location.hash;
    if (action.startsWith("#recodex")) {
      window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}#recodex`);
    }
    switch (action) {
      case "#recodex-login": void startLogin(); break;
      case "#recodex-refresh": void refresh(); break;
      case "#recodex-fastest": void useFastestGateway(); break;
      case "#recodex-update": void checkClient(); break;
      case "#recodex-diagnostics": void reportDiagnostics(); break;
      default: void load();
    }
    void loadOrgs();
  }, []);

  const data = state.data;
  const usage = data?.usage;
  const gateways = useMemo(() => data?.gateways ?? [], [data?.gateways]);
  const signedOut = !data && (state.status === "loading" || state.status === "signed_out" || state.status === "error");

  return (
    <div className="recodex-screen" data-testid="recodex-screen">
      <section className="recodex-panel recodex-header">
        <div className="recodex-heading">
          <div><h2>ReCodex</h2><p>{tr("账户、额度与网关", "Account, quota and gateway")}</p></div>
          <span className={`recodex-status recodex-status-${state.status}`} data-testid="recodex-status" aria-live="polite">{statusLabel(state.status)}</span>
        </div>
        <div className="recodex-actions">
          <button type="button" aria-label={tr("刷新额度", "Refresh quota")} onClick={() => void refresh()} disabled={busy}><RefreshCw size={16} /> {tr("刷新", "Refresh")}</button>
          <button type="button" aria-label={tr("刷新会话", "Refresh session")} onClick={() => void refreshSession()} disabled={busy}><KeyRound size={16} /> {tr("刷新会话", "Refresh session")}</button>
          <button type="button" aria-label={tr("检查更新", "Check updates")} onClick={() => void checkClient()} disabled={busy}><Download size={16} /> {tr("检查更新", "Check updates")}</button>
          <button type="button" aria-label={tr("发送诊断", "Send diagnostics")} onClick={() => void reportDiagnostics()} disabled={busy}><Send size={16} /> {tr("发送诊断", "Send diagnostics")}</button>
          {signedOut ? <button className="recodex-primary" type="button" aria-label={tr("登录", "Sign in")} onClick={() => void startLogin()} disabled={busy}><LogIn size={16} /> {tr("登录", "Sign in")}</button> : <button type="button" aria-label={tr("退出登录", "Sign out")} onClick={() => void logout()} disabled={busy}><LogOut size={16} /> {tr("退出登录", "Sign out")}</button>}
        </div>
        <div className="recodex-feedback">
          {state.error ? <p role="alert">{redactError(state.error.message)}</p> : null}
          {data?.account_error ? <p role="alert">{tr("账户暂时不可用：", "Account temporarily unavailable: ")}{redactError(data.account_error)}</p> : null}
          {data?.gateway_error ? <p role="alert">{tr("网关暂时不可用：", "Gateways temporarily unavailable: ")}{redactError(data.gateway_error)}</p> : null}
          {login?.error ? <p role="alert">{redactError(login.error.message)}</p> : null}
          {clientInfo?.error ? <p role="alert">{redactError(clientInfo.error.message)}</p> : null}
          {diagnostic?.error ? <p role="alert">{redactError(diagnostic.error.message)}</p> : null}
          {clientInfo?.data?.compatibility ? <p className="recodex-client-meta">{clientInfo.data.compatibility.supported ? tr("已兼容", "Compatible") : tr("需要更新", "Update required")}（{tr("最低版本", "minimum")} {clientInfo.data.compatibility.minimum_version}）{clientInfo.data.update_channel ? ` | ${clientInfo.data.update_channel.available ? `${tr("最新", "Latest")} ${clientInfo.data.update_channel.latest_version ?? tr("可用", "available")}` : `${tr("稳定通道不可用", "Stable channel unavailable")}（${clientInfo.data.update_channel.reason ?? tr("未配置", "not configured")}）`}` : ""}</p> : null}
          {diagnostic?.data?.diagnostics ? <p className="recodex-client-meta">{tr("诊断信息已被 ReCodex 接受。", "Diagnostics accepted by ReCodex.")}</p> : null}
          {login?.data ? <div className="recodex-login" data-testid="recodex-login"><strong>{statusLabel(login.status)}</strong><code>{login.data.user_code ?? ""}</code><button type="button" className="recodex-link" aria-label={tr("打开授权页", "Open verification")} onClick={() => { const url = login.data?.verify_url; if (url) void invoke("open_external_url", { url }); }} disabled={busy || !login.data.verify_url}>{tr("打开授权页", "Open verification")}</button><button type="button" aria-label={tr("检查授权", "Check login approval")} onClick={() => void pollLogin()} disabled={busy}>{tr("检查授权", "Check approval")}</button></div> : null}
        </div>
      </section>
      {usage ? <section className="recodex-panel recodex-usage" data-testid="recodex-usage"><div className="recodex-panel-title"><h3>{tr("额度", "Quota")}</h3><span className={usage.stale ? "recodex-warning" : "recodex-live"}>{usage.stale ? tr("缓存", "stale") : tr("实时", "live")}</span></div><p className="recodex-quota"><strong>{usage.available}</strong><span>/ {usage.total} {tr("剩余", "remaining")}</span></p><p className="recodex-muted">{tr("已用", "Used")} {usage.used} | {accountTypeLabel(usage.account_type)}</p>{usage.refresh_error ? <p className="recodex-warning" role="alert">{redactError(usage.refresh_error.message)} ({usage.refresh_error.code})</p> : null}<div className="recodex-window-list">{(usage.windows ?? []).map((window) => <div className="recodex-row" key={window.window}><span>{window.window}</span><span>{window.remaining} / {window.limit}{window.reset_at ? <small>{tr("重置", "Resets")} {window.reset_at}</small> : null}</span></div>)}</div><small className="recodex-meta">{tr("更新于", "Updated")} {usage.refreshed_at} | {usage.source}</small></section> : null}
      {data?.account ? <section className="recodex-panel recodex-account" data-testid="recodex-account"><h3>{tr("账户", "Account")}</h3><p className="recodex-account-name">{data.account.display_name || data.account.email || tr("ReCodex 账户", "ReCodex account")}</p><p className="recodex-muted">{data.account.plan || tr("无套餐", "No plan")} | {accountTypeLabel(data.account.account_type)}</p></section> : null}
      {orgs.length > 1 ? <section className="recodex-panel recodex-orgs" data-testid="recodex-orgs"><div className="recodex-panel-title"><h3>{tr("组织", "Organization")}</h3></div><div className="recodex-gateway-list">{orgs.map((org) => <div className="recodex-row recodex-gateway" key={org.id}><span><span>{org.name}</span></span><span><span className={org.plan_name ? "recodex-live" : "recodex-warning"}>{org.plan_name || tr("无生效订阅", "no subscription")}</span>{org.is_current ? <strong>{tr("使用中", "In use")}</strong> : <button type="button" aria-label={tr("切换到", "Switch to") + ` ${org.name}`} onClick={() => void switchOrg(org.id)} disabled={busy || !org.plan_name}>{tr("切换", "Switch")}</button>}</span></div>)}</div></section> : null}
      {gateways.length > 0 ? <section className="recodex-panel recodex-gateways" data-testid="recodex-gateways"><div className="recodex-panel-title"><h3>{tr("网关", "Gateways")}</h3><button type="button" aria-label={tr("使用最快网关", "Use fastest gateway")} onClick={() => void useFastestGateway()} disabled={busy}><Zap size={16} /> {tr("使用最快", "Use fastest")}</button></div><div className="recodex-gateway-list">{gateways.map((gateway) => <div className="recodex-row recodex-gateway" key={gateway.id}><span><Zap size={14} /> <span>{gateway.name || gateway.id}</span></span><span><span className={gateway.healthy ? "recodex-live" : "recodex-warning"}>{gateway.healthy ? `${gateway.client_latency_ms ?? "-"} ms` : tr("不可用", "unavailable")}</span>{gateway.selected ? <strong>{tr("已选用", "Selected")}</strong> : <button type="button" aria-label={tr("使用网关", "Use gateway") + ` ${gateway.name || gateway.id}`} onClick={() => void selectGateway(gateway.id)} disabled={busy || !gateway.healthy}>{tr("使用", "Use")}</button>}</span></div>)}</div></section> : null}
    </div>
  );
}
