import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import { LogStatus } from "./types";

/// Temperature-logging state owned by the Rust backend (it keeps logging
/// while the window is hidden in the tray). Synced via the `log-status`
/// event so the Tools menu, tray menu, and status bar always agree.
export function useLogging() {
  const [status, setStatus] = useState<LogStatus | null>(null);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let cancelled = false;
    invoke<LogStatus>("get_log_status")
      .then((s) => !cancelled && setStatus(s))
      .catch((e) => console.warn("get_log_status failed", e));
    listen<LogStatus>("log-status", (e) => {
      if (!cancelled) setStatus(e.payload);
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const setActive = useCallback((active: boolean) => {
    invoke<LogStatus>("set_logging", { active })
      .then(setStatus)
      .catch((e) => console.warn("set_logging failed", e));
  }, []);

  const openFolder = useCallback(() => {
    invoke("open_log_folder").catch((e) => console.warn("open_log_folder failed", e));
  }, []);

  return { status, setActive, openFolder };
}
