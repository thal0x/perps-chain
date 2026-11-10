import { createContext, useCallback, useContext, useState, type ReactNode } from "react";

export interface Toast {
  id: number;
  kind: "ok" | "err" | "info";
  title: string;
  body?: string;
  at: number;
}

interface Ctx {
  toasts: Toast[];
  push: (t: Omit<Toast, "id" | "at">) => void;
  dismiss: (id: number) => void;
}

const ToastCtx = createContext<Ctx | null>(null);
let nextId = 1;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const dismiss = useCallback((id: number) => setToasts((ts) => ts.filter((t) => t.id !== id)), []);
  const push = useCallback(
    (t: Omit<Toast, "id" | "at">) => {
      const id = nextId++;
      setToasts((ts) => [...ts.slice(-5), { ...t, id, at: Date.now() }]);
      // Errors stay until dismissed; others fade after 6 s.
      if (t.kind !== "err") setTimeout(() => dismiss(id), 6000);
    },
    [dismiss],
  );
  return <ToastCtx.Provider value={{ toasts, push, dismiss }}>{children}</ToastCtx.Provider>;
}

export function useToasts(): Ctx {
  const c = useContext(ToastCtx);
  if (!c) throw new Error("no ToastProvider");
  return c;
}

export function Toasts() {
  const { toasts, dismiss } = useToasts();
  return (
    <div className="toasts" data-testid="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.kind}`} data-testid={`toast-${t.kind}`} role={t.kind === "err" ? "alert" : "status"}>
          <div className="toast-head">
            <strong>{t.title}</strong>
            <button className="icon-btn" onClick={() => dismiss(t.id)} aria-label="Dismiss">
              ×
            </button>
          </div>
          {t.body && <div className="toast-body">{t.body}</div>}
        </div>
      ))}
    </div>
  );
}
