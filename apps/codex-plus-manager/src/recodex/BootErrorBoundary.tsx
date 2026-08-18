import { Component, type ErrorInfo, type ReactNode } from "react";

type Props = { children: ReactNode };
type State = { error: Error | null };

function safeMessage(error: unknown): string {
  const text = error instanceof Error ? error.message : String(error ?? "Unknown startup error");
  return text
    .replace(/Bearer\s+[^\s]+/gi, "Bearer [redacted]")
    .replace(/\brct_[A-Za-z0-9._~-]+/g, "[redacted]")
    .replace(/\b(sk-[A-Za-z0-9._~-]+)/g, "[redacted]")
    .replace(/["']?\b(access_token|refresh_token|id_token|token|api[_-]?key|client_secret|password)\b["']?\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,}&]+)/gi, "$1=[redacted]")
    .slice(0, 500) || "Unknown startup error";
}

/** Keeps a renderer exception actionable instead of leaving a blank WebView. */
export class BootErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    try {
      console.error("[ReCodex] renderer startup failed", safeMessage(error), info.componentStack?.slice(0, 1200));
    } catch {
      // A broken console must not hide the recovery screen.
    }
  }

  render(): ReactNode {
    if (!this.state.error) return this.props.children;
    const message = safeMessage(this.state.error);
    return (
      <main
        role="alert"
        style={{
          boxSizing: "border-box",
          display: "grid",
          gap: "12px",
          minHeight: "100vh",
          padding: "32px",
          placeContent: "center",
          background: "#101318",
          color: "#f5f7fa",
          fontFamily: "system-ui, sans-serif",
        }}
      >
        <h1 style={{ margin: 0, fontSize: "20px" }}>ReCodex could not load</h1>
        <p style={{ margin: 0, maxWidth: "680px", color: "#c8ced8" }}>
          The renderer hit an unexpected error. Reload the window or open the local diagnostic log.
        </p>
        <button
          type="button"
          onClick={() => window.location.reload()}
          style={{ justifySelf: "start", padding: "8px 14px", cursor: "pointer" }}
        >
          Reload ReCodex
        </button>
        <details>
          <summary>Technical details</summary>
          <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{message}</pre>
        </details>
      </main>
    );
  }
}
