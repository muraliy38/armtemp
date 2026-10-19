import { LogStatus, SensorSnapshot } from "../app/types";
import { formatTemp } from "../app/theme";

interface Props {
  snap: SensorSnapshot | null;
  unit: "C" | "F";
  log: LogStatus | null;
}

// The sunken status bar footer: CPU Temp / Low / High. Snapdragon X exposes
// no per-core temperature sensor, so this is the app's single honest CPU
// temperature (the hottest valid thermal zone); Low/High are its session
// min/max since the app started.
export function StatusBar({ snap, unit, log }: Props) {
  const cpu = snap?.package_c ?? null;
  const low = snap?.package_min_c ?? null;
  const high = snap?.package_max_c ?? null;
  return (
    <div className="status-bar">
      <span className="sb-item">
        <span className="sb-label">CPU Temp:</span> {formatTemp(cpu, unit)}
      </span>
      <span className="sb-sep" />
      <span className="sb-item">
        <span className="sb-label">Low:</span> {formatTemp(low, unit)}
      </span>
      <span className="sb-sep" />
      <span className="sb-item">
        <span className="sb-label">High:</span> {formatTemp(high, unit)}
      </span>
      <span className="sb-grip" />
      {log?.active && (
        <span className="sb-item sb-log" title={log.file ?? log.dir}>
          ● Logging
        </span>
      )}
      {!log?.active && log?.error && (
        <span className="sb-item sb-log-error" title={log.error}>
          Log error
        </span>
      )}
    </div>
  );
}
