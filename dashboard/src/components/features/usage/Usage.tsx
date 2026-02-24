import { useState, useMemo, useEffect } from "react";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components";
import { useUsage } from "@/api/control-layer/hooks";

// Keyframe for inline-style spinner (Tailwind's animate-spin only works with class names)
const spinKeyframes = `@keyframes spin { to { transform: rotate(360deg) } }`;


const RANGE_MINUTES: Record<string, number> = {
  "5m": 5, "15m": 15, "30m": 30,
  "1h": 60, "3h": 180, "8h": 480,
  "1d": 1440, "3d": 4320, "7d": 10080,
  "30d": 43200, "60d": 86400, "90d": 129600, "180d": 259200,
};

function formatCompact(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}
function formatDollars(n: number, d = 2) {
  return `$${n.toFixed(d)}`;
}
function formatNumber(n: number): string {
  return new Intl.NumberFormat("en-US").format(n);
}

const RANGE_OPTIONS = [
  { value: "5m", label: "5m" },
  { value: "15m", label: "15m" },
  { value: "30m", label: "30m" },
  { value: "1h", label: "1h" },
  { value: "3h", label: "3h" },
  { value: "8h", label: "8h" },
  { value: "1d", label: "1d" },
  { value: "3d", label: "3d" },
  { value: "7d", label: "7d" },
  { value: "30d", label: "30d" },
  { value: "60d", label: "60d" },
  { value: "90d", label: "90d" },
  { value: "180d", label: "180d" },
  { value: "all", label: "All" },
];

const PALETTE = [
  "#7c3aed", "#0ea5e9", "#10b981", "#f59e0b",
  "#ef4444", "#ec4899", "#06b6d4", "#84cc16",
];

function AnimatedNumber({ value, format = (v: number) => String(v) }: { value: number; format?: (v: number) => string }) {
  const [display, setDisplay] = useState(0);
  useEffect(() => {
    const start = performance.now();
    const dur = 700;
    function tick(now: number) {
      const p = Math.min((now - start) / dur, 1);
      const e = 1 - Math.pow(1 - p, 3);
      setDisplay(value * e);
      if (p < 1) requestAnimationFrame(tick);
    }
    requestAnimationFrame(tick);
  }, [value]);
  return <>{format(display)}</>;
}

function MiniBar({ value, max, color, delay = 0 }: { value: number; max: number; color: string; delay?: number }) {
  const [w, setW] = useState(0);
  useEffect(() => {
    const t = setTimeout(() => setW(max > 0 ? (value / max) * 100 : 0), 60 + delay);
    return () => clearTimeout(t);
  }, [value, max, delay]);
  return (
      <div style={{ height: 5, borderRadius: 3, background: "#f1f5f9", overflow: "hidden", flex: 1 }}>
        <div style={{
          height: "100%", borderRadius: 3, background: color,
          width: `${w}%`, transition: "width 0.9s cubic-bezier(0.16,1,0.3,1)",
        }} />
      </div>
  );
}

interface DonutDatum {
  label: string;
  value: number;
  format: (v: number) => string;
  totalFormat: (v: number) => string;
}

function DonutChart({ data, size = 190, thickness = 24, centerLabel }: { data: DonutDatum[]; size?: number; thickness?: number; centerLabel: string }) {
  const total = data.reduce((s, d) => s + d.value, 0);
  const r = (size - thickness) / 2;
  const cx = size / 2, cy = size / 2;
  let cum = -90;

  const arcs = data.map((d, i) => {
    const angle = total > 0 ? (d.value / total) * 360 : 0;
    const s = cum;
    cum += angle;
    const sRad = (s * Math.PI) / 180;
    const eRad = (cum * Math.PI) / 180;
    const large = angle > 180 ? 1 : 0;
    const gap = data.length > 1 ? 0.015 : 0;
    const sG = sRad + gap, eG = eRad - gap;
    return {
      path: `M ${cx + r * Math.cos(sG)} ${cy + r * Math.sin(sG)} A ${r} ${r} 0 ${large} 1 ${cx + r * Math.cos(eG)} ${cy + r * Math.sin(eG)}`,
      color: PALETTE[i % PALETTE.length],
      ...d,
    };
  });

  const [hovered, setHovered] = useState<number | null>(null);

  return (
      <div style={{ position: "relative", width: size, height: size }}>
        <svg width={size} height={size} style={{ overflow: "visible" }}>
          {arcs.map((arc, i) => (
              <path
                  key={i} d={arc.path} fill="none"
                  stroke={arc.color}
                  strokeWidth={hovered === i ? thickness + 5 : thickness}
                  strokeLinecap="round"
                  style={{
                    transition: "stroke-width 0.2s, opacity 0.2s",
                    opacity: hovered !== null && hovered !== i ? 0.3 : 1,
                    cursor: "pointer",
                  }}
                  onMouseEnter={() => setHovered(i)}
                  onMouseLeave={() => setHovered(null)}
              />
          ))}
        </svg>
        <div style={{
          position: "absolute", inset: 0, display: "flex", flexDirection: "column",
          alignItems: "center", justifyContent: "center", pointerEvents: "none",
        }}>
          {hovered !== null && (
              <div style={{
                position: "absolute", top: -8, left: "50%", transform: "translateX(-50%)",
                background: "white", borderRadius: 8, padding: "6px 12px",
                boxShadow: "0 2px 8px rgba(0,0,0,0.12)", border: "1px solid #e2e8f0",
                display: "flex", flexDirection: "column", alignItems: "center", gap: 2,
                whiteSpace: "nowrap", pointerEvents: "none",
              }}>
                <span style={{ fontSize: 11, color: "#475569", fontWeight: 600 }}>
                  {arcs[hovered].label}
                </span>
                <span style={{ fontSize: 16, fontWeight: 700, color: "#1e293b", fontVariantNumeric: "tabular-nums" }}>
                  {arcs[hovered].format(arcs[hovered].value)}
                </span>
              </div>
          )}
          <span style={{ fontSize: 10, color: "#94a3b8", textTransform: "uppercase", letterSpacing: "0.06em", fontWeight: 600 }}>
            {centerLabel}
          </span>
          <span style={{ fontSize: 18, fontWeight: 700, color: "#1e293b", fontVariantNumeric: "tabular-nums" }}>
            {data[0]?.totalFormat(total)}
          </span>
        </div>
      </div>
  );
}

export function Usage() {
  const [range, setRange] = useState("1h");
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);

  const { startDate, endDate } = useMemo(() => {
    const minutes = RANGE_MINUTES[range];
    if (!minutes) return { startDate: undefined, endDate: undefined };
    const now = new Date();
    const from = new Date(now.getTime() - minutes * 60 * 1000);
    return { startDate: from.toISOString(), endDate: now.toISOString() };
  }, [range]);

  const { data: usage, isLoading, isFetching, refresh } = useUsage(startDate, endDate);
  const busy = isLoading || isFetching;

  const chartData = useMemo(() => {
    if (!usage?.by_model.length) return [];
    return usage.by_model.map((entry) => ({
      model: entry.model.split("/").pop() || entry.model,
      provider: entry.model.split("/")[0] || "",
      requests: entry.request_count,
      input_tokens: entry.input_tokens,
      output_tokens: entry.output_tokens,
      cost: parseFloat(entry.cost),
    }));
  }, [usage]);

  const card = {
    background: "#fff",
    borderRadius: 12,
    border: "1px solid #e2e8f0",
    boxShadow: "0 1px 3px rgba(0,0,0,0.04), 0 1px 2px rgba(0,0,0,0.02)",
  };

  const header = (
          <>
          <style>{spinKeyframes}</style>
          <div style={{
            display: "flex", alignItems: "center", justifyContent: "space-between",
            marginBottom: 28, flexWrap: "wrap", gap: 12,
          }}>
            <div>
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <h1 style={{ fontSize: 22, fontWeight: 700, color: "#0f172a", margin: 0 }}>Usage</h1>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={refresh}
                  disabled={busy}
                  className="h-8 w-8 p-0"
                  title="Refresh usage data"
                >
                  <RefreshCw
                    className={`h-4 w-4 ${busy ? "animate-spin" : ""}`}
                  />
                </Button>
              </div>
              <p style={{ fontSize: 13, color: "#94a3b8", margin: "2px 0 0" }}>
                API consumption &amp; cost breakdown
              </p>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <div style={{
                display: "flex", gap: 1, padding: 3, borderRadius: 10,
                background: "#f1f5f9", border: "1px solid #e2e8f0",
              }}>
                {["1h","1d","7d","30d","90d","all"].map((v) => {
                  const o = RANGE_OPTIONS.find(r => r.value === v);
                  return (
                      <button key={v} onClick={() => setRange(v)} style={{
                        padding: "5px 12px", fontSize: 12, fontWeight: 500, borderRadius: 7,
                        border: "none", cursor: "pointer", transition: "all 0.15s",
                        background: range === v ? "#fff" : "transparent",
                        color: range === v ? "#0f172a" : "#64748b",
                        boxShadow: range === v ? "0 1px 3px rgba(0,0,0,0.08)" : "none",
                      }}>
                        {o?.label}
                      </button>
                  );
                })}
              </div>
              {(() => {
                const moreValues = ["5m","15m","30m","3h","8h","3d","60d","180d"];
                const isMoreSelected = moreValues.includes(range);
                return (
                    <div style={{ position: "relative", display: "inline-block" }}>
                      <select
                          value={isMoreSelected ? range : ""}
                          onChange={(e) => { if (e.target.value) setRange(e.target.value); }}
                          style={{
                            appearance: "none", WebkitAppearance: "none",
                            padding: "5px 28px 5px 10px", fontSize: 12, fontWeight: 500,
                            borderRadius: 8, border: "1px solid #e2e8f0",
                            background: isMoreSelected ? "#fff" : "#f1f5f9",
                            color: isMoreSelected ? "#0f172a" : "#64748b",
                            cursor: "pointer", outline: "none",
                            boxShadow: isMoreSelected ? "0 1px 3px rgba(0,0,0,0.08)" : "none",
                          }}
                      >
                        <option value="" disabled hidden>More</option>
                        {moreValues.map(v => {
                          const o = RANGE_OPTIONS.find(r => r.value === v);
                          return <option key={v} value={v}>{o?.label}</option>;
                        })}
                      </select>
                      <svg width="10" height="10" viewBox="0 0 10 10" fill="none"
                           style={{ position: "absolute", right: 8, top: "50%", transform: "translateY(-50%)", pointerEvents: "none" }}>
                        <path d="M2.5 4L5 6.5L7.5 4" stroke="#64748b" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                    </div>
                );
              })()}
            </div>
          </div>
          </>
  );

  if (isLoading) {
    return (
      <div style={{ minHeight: "100vh", background: "#f8fafc" }}>
        <div style={{ maxWidth: 1140, margin: "0 auto", padding: "32px 24px 64px" }}>
          {header}
          <div style={{ display: "flex", alignItems: "center", justifyContent: "center", gap: 10, padding: "80px 0", color: "#94a3b8", fontSize: 14 }}>
            <svg width="16" height="16" viewBox="0 0 16 16" style={{ animation: "spin 1s linear infinite" }}>
              <circle cx="8" cy="8" r="6" fill="none" stroke="#e2e8f0" strokeWidth="2.5" />
              <path d="M8 2A6 6 0 0 1 14 8" fill="none" stroke="#94a3b8" strokeWidth="2.5" strokeLinecap="round" />
            </svg>
            Collecting usage data — this can take up to 30 seconds for larger date ranges
          </div>
        </div>
      </div>
    );
  }

  if (!usage) {
    return (
      <div style={{ minHeight: "100vh", background: "#f8fafc" }}>
        <div style={{ maxWidth: 1140, margin: "0 auto", padding: "32px 24px 64px" }}>
          {header}
          <p style={{ color: "#94a3b8", fontSize: 14, textAlign: "center", padding: "80px 0" }}>
            No usage data available.
          </p>
        </div>
      </div>
    );
  }

  const totalCost = parseFloat(usage.total_cost);
  const realtimeCost = parseFloat(usage.estimated_realtime_cost);
  const savings = realtimeCost > 0 ? (1 - totalCost / realtimeCost) * 100 : 0;
  const maxRequests = Math.max(...chartData.map((d) => d.requests));
  const maxTokens = Math.max(...chartData.map((d) => d.input_tokens + d.output_tokens));

  const costPieData = chartData.map((d) => ({
    label: d.model, value: d.cost,
    format: (v: number) => formatDollars(v), totalFormat: (v: number) => formatDollars(v),
  }));
  const requestPieData = chartData.map((d) => ({
    label: d.model, value: d.requests,
    format: (v: number) => formatNumber(Math.round(v)), totalFormat: (v: number) => formatCompact(v),
  }));

  return (
      <div style={{
        minHeight: "100vh",
        opacity: mounted ? 1 : 0, transition: "opacity 0.4s ease",
      }}>
        <div style={{ maxWidth: 1140, margin: "0 auto", padding: "32px 24px 64px" }}>
          {header}

          {/* KPI row */}
          <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 14, marginBottom: 20 }}>

            {/* Tokens */}
            <div style={{ ...card, padding: "20px 24px" }}>
              <div style={{ fontSize: 12, fontWeight: 600, color: "#94a3b8", textTransform: "uppercase", letterSpacing: "0.05em", marginBottom: 8 }}>
                Tokens
              </div>
              <div style={{ fontSize: 30, fontWeight: 700, color: "#0f172a", fontVariantNumeric: "tabular-nums", lineHeight: 1 }}>
                <AnimatedNumber value={usage.total_input_tokens + usage.total_output_tokens} format={(v) => formatCompact(Math.round(v))} />
              </div>
              <div style={{ display: "flex", gap: 20, marginTop: 12 }}>
                <div>
                  <div style={{ fontSize: 11, color: "#94a3b8", marginBottom: 2 }}>Input</div>
                  <div style={{ fontSize: 15, fontWeight: 600, color: "#0ea5e9", fontVariantNumeric: "tabular-nums" }}>
                    {formatCompact(usage.total_input_tokens)}
                  </div>
                </div>
                <div style={{ width: 1, background: "#e2e8f0" }} />
                <div>
                  <div style={{ fontSize: 11, color: "#94a3b8", marginBottom: 2 }}>Output</div>
                  <div style={{ fontSize: 15, fontWeight: 600, color: "#10b981", fontVariantNumeric: "tabular-nums" }}>
                    {formatCompact(usage.total_output_tokens)}
                  </div>
                </div>
              </div>
            </div>

            {/* Batches */}
            <div style={{ ...card, padding: "20px 24px" }}>
              <div style={{ fontSize: 12, fontWeight: 600, color: "#94a3b8", textTransform: "uppercase", letterSpacing: "0.05em", marginBottom: 8 }}>
                Batches
              </div>
              <div style={{ fontSize: 30, fontWeight: 700, color: "#0f172a", fontVariantNumeric: "tabular-nums", lineHeight: 1 }}>
                <AnimatedNumber value={usage.total_batch_count} format={(v) => formatNumber(Math.round(v))} />
              </div>
              <div style={{ marginTop: 12 }}>
                <div style={{ fontSize: 11, color: "#94a3b8", marginBottom: 2 }}>Avg batch size</div>
                <div style={{ fontSize: 15, fontWeight: 600, color: "#f59e0b", fontVariantNumeric: "tabular-nums" }}>
                  {usage.avg_requests_per_batch.toFixed(1)} reqs
                </div>
              </div>
            </div>

            {/* Cost */}
            <div style={{ ...card, padding: "20px 24px" }}>
              <div style={{ fontSize: 12, fontWeight: 600, color: "#94a3b8", textTransform: "uppercase", letterSpacing: "0.05em", marginBottom: 8 }}>
                Cost
              </div>
              <div style={{ fontSize: 30, fontWeight: 700, color: "#0f172a", fontVariantNumeric: "tabular-nums", lineHeight: 1 }}>
                <AnimatedNumber value={totalCost} format={(v) => formatDollars(v)} />
              </div>
              {realtimeCost > 0 && (
                  <div style={{ display: "flex", alignItems: "center", gap: 10, marginTop: 14 }}>
                    <div style={{
                      display: "inline-flex", alignItems: "center", gap: 5,
                      background: "#ecfdf5", borderRadius: 8, padding: "5px 10px",
                    }}>
                      <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
                        <path d="M12 9L7.5 4.5L5.5 6.5L2 3" stroke="#059669" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                        <path d="M8.5 9H12V5.5" stroke="#059669" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                      <span style={{ fontSize: 13, fontWeight: 600, color: "#059669" }}>
                    Saved {savings.toFixed(0)}% ({formatDollars(realtimeCost - totalCost)})
                  </span>
                    </div>
                    <span style={{ fontSize: 12, color: "#64748b", fontWeight: 500 }}>
                  vs sync
                </span>
                  </div>
              )}
            </div>
          </div>

          {/* Donuts + Table */}
          <div style={{ display: "grid", gridTemplateColumns: "340px 1fr", gap: 14 }}>

            {/* Left: donuts stacked */}
            <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
              <div style={{ ...card, padding: "20px 24px", display: "flex", flexDirection: "column", alignItems: "center" }}>
                <div style={{ fontSize: 13, fontWeight: 600, color: "#475569", marginBottom: 12, alignSelf: "flex-start" }}>
                  Cost by Model
                </div>
                <DonutChart data={costPieData} size={180} thickness={22} centerLabel="Total" />
              </div>
              <div style={{ ...card, padding: "20px 24px", display: "flex", flexDirection: "column", alignItems: "center" }}>
                <div style={{ fontSize: 13, fontWeight: 600, color: "#475569", marginBottom: 12, alignSelf: "flex-start" }}>
                  Requests by Model
                </div>
                <DonutChart data={requestPieData} size={180} thickness={22} centerLabel="Total" />
              </div>
              {/* Legend */}
              <div style={{ display: "flex", flexWrap: "wrap", gap: "6px 16px", padding: "0 4px" }}>
                {chartData.map((d, i) => (
                    <div key={d.model} style={{ display: "flex", alignItems: "center", gap: 6 }}>
                      <div style={{ width: 8, height: 8, borderRadius: 2, background: PALETTE[i % PALETTE.length], flexShrink: 0 }} />
                      <span style={{ fontSize: 12, color: "#64748b" }}>{d.model}</span>
                    </div>
                ))}
              </div>
            </div>

            {/* Right: model table */}
            <div style={{ ...card, overflow: "hidden" }}>
              <div style={{ padding: "18px 24px 12px" }}>
                <div style={{ fontSize: 13, fontWeight: 600, color: "#475569" }}>Model Breakdown</div>
              </div>

              {/* Header */}
              <div style={{
                display: "grid", gridTemplateColumns: "2fr 1.1fr 1.6fr 0.7fr",
                padding: "0 24px 10px", gap: 12,
                fontSize: 11, color: "#94a3b8", textTransform: "uppercase", letterSpacing: "0.06em", fontWeight: 600,
                borderBottom: "1px solid #f1f5f9",
              }}>
                <span>Model</span>
                <span style={{ textAlign: "right" }}>Requests</span>
                <span>Tokens</span>
                <span style={{ textAlign: "right" }}>Cost</span>
              </div>

              {chartData.map((d, i) => (
                  <div
                      key={d.model}
                      style={{
                        display: "grid", gridTemplateColumns: "2fr 1.1fr 1.6fr 0.7fr",
                        padding: "14px 24px", gap: 12, alignItems: "center",
                        borderBottom: i < chartData.length - 1 ? "1px solid #f8fafc" : "none",
                        transition: "background 0.12s",
                        opacity: mounted ? 1 : 0,
                        transform: mounted ? "none" : "translateY(6px)",
                        transitionProperty: "background, opacity, transform",
                        transitionDuration: "0.12s, 0.5s, 0.5s",
                        transitionDelay: `0s, ${i * 50}ms, ${i * 50}ms`,
                      }}
                      onMouseEnter={(e) => (e.currentTarget.style.background = "#fafbfd")}
                      onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
                  >
                    {/* Model */}
                    <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                      <div style={{ width: 8, height: 8, borderRadius: 2, background: PALETTE[i % PALETTE.length], flexShrink: 0 }} />
                      <div>
                        <div style={{ fontSize: 13, fontWeight: 600, color: "#1e293b" }}>{d.model}</div>
                        <div style={{ fontSize: 11, color: "#94a3b8" }}>{d.provider}</div>
                      </div>
                    </div>

                    {/* Requests */}
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <MiniBar value={d.requests} max={maxRequests} color={PALETTE[i % PALETTE.length]} delay={i * 60} />
                      <span style={{ fontSize: 13, fontWeight: 600, color: "#334155", fontVariantNumeric: "tabular-nums", minWidth: 42, textAlign: "right" }}>
                    {formatCompact(d.requests)}
                  </span>
                    </div>

                    {/* Tokens */}
                    <div>
                      <div style={{ display: "flex", gap: 8, fontSize: 12, fontVariantNumeric: "tabular-nums", alignItems: "baseline" }}>
                        <span style={{ color: "#0ea5e9", fontWeight: 500 }}>{formatCompact(d.input_tokens)}</span>
                        <span style={{ color: "#cbd5e1" }}>/</span>
                        <span style={{ color: "#10b981", fontWeight: 500 }}>{formatCompact(d.output_tokens)}</span>
                        <span style={{ color: "#b0b8c4", fontSize: 11 }}>{formatCompact(d.input_tokens + d.output_tokens)}</span>
                      </div>
                      <div style={{ display: "flex", gap: 2, marginTop: 5, maxWidth: 160 }}>
                        <div style={{
                          height: 3, borderRadius: 2, background: "#0ea5e9",
                          width: `${maxTokens > 0 ? (d.input_tokens / maxTokens) * 100 : 0}%`,
                          transition: "width 0.9s cubic-bezier(0.16,1,0.3,1)",
                          transitionDelay: `${i * 60 + 150}ms`,
                          minWidth: d.input_tokens > 0 ? 2 : 0,
                        }} />
                        <div style={{
                          height: 3, borderRadius: 2, background: "#10b981",
                          width: `${maxTokens > 0 ? (d.output_tokens / maxTokens) * 100 : 0}%`,
                          transition: "width 0.9s cubic-bezier(0.16,1,0.3,1)",
                          transitionDelay: `${i * 60 + 250}ms`,
                          minWidth: d.output_tokens > 0 ? 2 : 0,
                        }} />
                      </div>
                    </div>

                    {/* Cost */}
                    <div style={{ textAlign: "right", fontSize: 13, fontWeight: 600, color: "#1e293b", fontVariantNumeric: "tabular-nums" }}>
                      {formatDollars(d.cost)}
                    </div>
                  </div>
              ))}
            </div>
          </div>
        </div>
      </div>
  );
}