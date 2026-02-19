import { useMemo, useState } from "react";
import { useUsage } from "@/api/control-layer/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  ChartContainer,
  type ChartConfig,
} from "@/components/ui/chart";
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Legend, Tooltip, PieChart, Pie, Cell } from "recharts";
import type { TooltipProps } from "recharts";
import { BarChart3, CircleHelp, DollarSign, Layers, PieChartIcon, TrendingDown, Zap } from "lucide-react";
import { formatDollars } from "@/utils/money";
import { DateTimeRangeSelector } from "@/components/ui/date-time-range-selector";

function formatNumber(n: number): string {
  return new Intl.NumberFormat("en-US").format(n);
}

function formatCompact(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}


const tokenChartConfig = {
  input_tokens: { label: "Input Tokens", color: "#3b82f6" },
  output_tokens: { label: "Output Tokens", color: "#10b981" },
} satisfies ChartConfig;

const costChartConfig = {
  cost: { label: "Cost", color: "#8b5cf6" },
  requests: { label: "Requests", color: "#f59e0b" },
} satisfies ChartConfig;

const METRIC_FORMAT: Record<string, { label: string; format: (v: number) => string }> = {
  input_tokens: { label: "Input Tokens", format: (v) => formatNumber(v) },
  output_tokens: { label: "Output Tokens", format: (v) => formatNumber(v) },
  cost: { label: "Cost", format: (v) => formatDollars(v, 4) },
  requests: { label: "Requests", format: (v) => formatNumber(v) },
};

const PIE_COLORS = [
  "#8b5cf6", "#f59e0b", "#3b82f6", "#10b981",
  "#ef4444", "#ec4899", "#06b6d4", "#84cc16",
];

function PieTooltip({ active, payload }: TooltipProps<number, string>) {
  if (!active || !payload?.length) return null;
  const entry = payload[0];
  const dataKey = entry.dataKey as string;
  const meta = METRIC_FORMAT[dataKey];

  return (
    <div className="rounded-lg border border-border/50 bg-background px-3 py-2 text-sm shadow-xl">
      <div className="font-medium mb-1">{entry.name}</div>
      <div className="flex items-center gap-2">
        <span
          className="inline-block h-2.5 w-2.5 shrink-0 rounded-[2px]"
          style={{ backgroundColor: entry.payload?.fill }}
        />
        <span className="text-muted-foreground">{meta?.label ?? dataKey}</span>
        <span className="ml-auto font-mono font-medium tabular-nums">
          {meta ? meta.format(entry.value as number) : entry.value}
        </span>
      </div>
    </div>
  );
}

function BarTooltip({ active, payload, label }: TooltipProps<number, string>) {
  if (!active || !payload?.length) return null;

  return (
    <div className="rounded-lg border border-border/50 bg-background px-3 py-2 text-sm shadow-xl">
      <div className="font-medium mb-1">{label}</div>
      {payload.map((entry) => {
        const key = entry.dataKey as string;
        const meta = METRIC_FORMAT[key];
        return (
          <div key={key} className="flex items-center gap-2">
            <span
              className="inline-block h-2.5 w-2.5 shrink-0 rounded-[2px]"
              style={{ backgroundColor: entry.color }}
            />
            <span className="text-muted-foreground">{meta?.label ?? key}</span>
            <span className="ml-auto font-mono font-medium tabular-nums">
              {meta ? meta.format(entry.value as number) : entry.value}
            </span>
          </div>
        );
      })}
    </div>
  );
}

export function Usage() {
  const [dateRange, setDateRange] = useState<{ from: Date; to: Date }>(() => {
    const now = new Date();
    const from = new Date(now.getTime() - 30 * 24 * 60 * 60 * 1000);
    return { from, to: now };
  });

  const { startDate, endDate } = useMemo(
    () => ({
      startDate: dateRange.from.toISOString(),
      endDate: dateRange.to.toISOString(),
    }),
    [dateRange],
  );

  const { data: usage, isLoading } = useUsage(startDate, endDate);
  const [costView, setCostView] = useState<"bar" | "pie">("pie");

  const chartData = useMemo(() => {
    if (!usage?.by_model.length) return [];

    return usage.by_model.slice(0, 8).map((entry) => ({
      model: entry.model.split("/").pop() || entry.model,
      requests: entry.request_count,
      input_tokens: entry.input_tokens,
      output_tokens: entry.output_tokens,
      cost: parseFloat(entry.cost),
    }));
  }, [usage?.by_model]);

  const datePicker = (
    <DateTimeRangeSelector
      value={dateRange}
      onChange={(range) => {
        if (range) setDateRange(range);
      }}
    />
  );

  if (isLoading) {
    return (
      <div className="p-6 md:p-8 space-y-6">
        <div className="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
          <h1 className="text-2xl font-bold tracking-tight">Usage</h1>
          {datePicker}
        </div>
        <div className="grid gap-4 grid-cols-2 lg:grid-cols-4">
          {Array.from({ length: 6 }).map((_, i) => (
            <Card key={i} className={i === 0 ? "col-span-2" : ""}>
              <CardHeader className="pb-2">
                <div className="h-4 w-24 bg-muted rounded animate-pulse" />
              </CardHeader>
              <CardContent>
                <div className="h-8 w-32 bg-muted rounded animate-pulse" />
              </CardContent>
            </Card>
          ))}
        </div>
      </div>
    );
  }

  if (!usage) {
    return (
      <div className="p-6 md:p-8 space-y-6">
        <div className="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
          <h1 className="text-2xl font-bold tracking-tight">Usage</h1>
          {datePicker}
        </div>
        <p className="text-muted-foreground">No usage data available.</p>
      </div>
    );
  }

  const totalCost = parseFloat(usage.total_cost);
  const realtimeCost = parseFloat(usage.estimated_realtime_cost);
  const savings = realtimeCost > 0 ? ((1 - totalCost / realtimeCost) * 100) : 0;

  return (
    <div className="p-6 md:p-8 space-y-6">
      <div className="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
        <h1 className="text-2xl font-bold tracking-tight">Usage</h1>
        {datePicker}
      </div>

      <div className="grid gap-3 grid-cols-1 md:grid-cols-3">
        {/* Tokens */}
        <Card>
          <CardContent className="p-6">
            <div className="flex items-center gap-2 text-sm text-muted-foreground mb-3">
              <Zap className="h-4 w-4" />
              Total Tokens
            </div>
            <div className="text-4xl font-bold tabular-nums mb-4">
              {formatCompact(usage.total_input_tokens + usage.total_output_tokens)}
            </div>
            <div className="flex items-center gap-6 text-muted-foreground">
              <div>
                <span className="text-xs">In</span>
                <p className="text-lg font-semibold tabular-nums text-foreground">
                  {formatCompact(usage.total_input_tokens)}
                </p>
              </div>
              <div className="w-px h-8 bg-border" />
              <div>
                <span className="text-xs">Out</span>
                <p className="text-lg font-semibold tabular-nums text-foreground">
                  {formatCompact(usage.total_output_tokens)}
                </p>
              </div>
            </div>
          </CardContent>
        </Card>

        {/* Batches */}
        <Card>
          <CardContent className="p-6">
            <div className="flex items-center gap-2 text-sm text-muted-foreground mb-3">
              <Layers className="h-4 w-4" />
              Total Batches
            </div>
            <div className="text-4xl font-bold tabular-nums mb-4">
              {formatNumber(usage.total_batch_count)}
            </div>
            <div className="flex items-center gap-6 text-muted-foreground">
              <div>
                <span className="text-xs">Avg Batch Size</span>
                <p className="text-lg font-semibold tabular-nums text-foreground">
                  {usage.avg_requests_per_batch.toFixed(1)} reqs
                </p>
              </div>
            </div>
          </CardContent>
        </Card>

        {/* Cost & Savings */}
        <Card>
          <CardContent className="p-6">
            <div className="flex items-center gap-2 text-sm text-muted-foreground mb-3">
              <DollarSign className="h-4 w-4" />
              Cost
            </div>
            <div className="flex items-center gap-6">
              <div>
                <span className="text-xs text-muted-foreground">Total Spent</span>
                <p className="text-4xl font-bold tabular-nums">
                  {formatDollars(totalCost, 2)}
                </p>
              </div>
              {realtimeCost > 0 && (
                <>
                  <div className="w-px h-14 bg-border" />
                  <div>
                    <span className="text-xs text-muted-foreground flex items-center gap-1">
                      <TrendingDown className="h-3 w-3 text-emerald-500" />
                      Saved vs. Sync
                      <span className="relative group">
                        <CircleHelp className="h-3.5 w-3.5 text-muted-foreground/60" />
                        <span className="pointer-events-none absolute bottom-full left-1/2 -translate-x-1/2 mb-1.5 w-48 rounded-md border bg-popover px-2.5 py-1.5 text-xs leading-snug text-popover-foreground shadow-md opacity-0 group-hover:opacity-100 transition-opacity z-50">
                          Estimated cost if all tokens were charged at current realtime tariffs.
                        </span>
                      </span>
                    </span>
                    <p className="text-4xl font-bold tabular-nums text-emerald-600 dark:text-emerald-400">
                      {formatDollars(realtimeCost - totalCost, 2)}
                    </p>
                    {savings > 0 && (
                      <span className="text-xs font-medium text-emerald-600 dark:text-emerald-400">
                        {savings.toFixed(0)}% saved
                      </span>
                    )}
                  </div>
                </>
              )}
            </div>
          </CardContent>
        </Card>
      </div>

      {usage.by_model.length > 0 && (
        <div className="grid gap-4 lg:grid-cols-2">
          <Card>
            <CardHeader className="pb-2">
              <div className="flex items-center justify-between">
                <CardTitle className="text-base font-medium">
                  Cost & Requests by Model
                </CardTitle>
                <div className="flex items-center gap-0.5 rounded-md border p-0.5">
                  <button
                    type="button"
                    onClick={() => setCostView("bar")}
                    className={`rounded p-1 transition-colors ${costView === "bar" ? "bg-muted text-foreground" : "text-muted-foreground hover:text-foreground"}`}
                    aria-label="Bar chart view"
                  >
                    <BarChart3 className="h-3.5 w-3.5" />
                  </button>
                  <button
                    type="button"
                    onClick={() => setCostView("pie")}
                    className={`rounded p-1 transition-colors ${costView === "pie" ? "bg-muted text-foreground" : "text-muted-foreground hover:text-foreground"}`}
                    aria-label="Pie chart view"
                  >
                    <PieChartIcon className="h-3.5 w-3.5" />
                  </button>
                </div>
              </div>
            </CardHeader>
            <CardContent>
              {costView === "bar" ? (
                <ChartContainer
                  config={costChartConfig}
                  style={{ height: Math.max(300, chartData.length * 50) }}
                  className="w-full"
                >
                  <BarChart
                    data={chartData}
                    layout="vertical"
                    margin={{ top: 0, right: 16, bottom: 0, left: 0 }}
                  >
                    <CartesianGrid horizontal={false} strokeDasharray="3 3" />
                    <XAxis
                      xAxisId="cost"
                      type="number"
                      orientation="top"
                      tickFormatter={(v: number) => formatDollars(v, 2)}
                      fontSize={13}
                      tick={{ fill: "currentColor" }}
                    />
                    <XAxis
                      xAxisId="requests"
                      type="number"
                      orientation="bottom"
                      tickFormatter={formatCompact}
                      fontSize={13}
                      tick={{ fill: "currentColor" }}
                    />
                    <YAxis
                      dataKey="model"
                      type="category"
                      width={180}
                      tick={({ x, y, payload }) => {
                        const text = String(payload.value);
                        const lines: string[] = [];
                        if (text.length <= 22) {
                          lines.push(text);
                        } else {
                          let remaining = text;
                          while (remaining.length > 22) {
                            const chunk = remaining.slice(0, 22);
                            const dashIdx = chunk.lastIndexOf("-");
                            const breakAt = dashIdx > 8 ? dashIdx + 1 : 22;
                            lines.push(remaining.slice(0, breakAt));
                            remaining = remaining.slice(breakAt);
                          }
                          if (remaining) lines.push(remaining);
                        }
                        return (
                          <text x={x} y={y} textAnchor="end" fontSize={13} fill="currentColor">
                            {lines.map((line, i) => (
                              <tspan key={i} x={x} dy={i === 0 ? "0.35em" : "1.2em"}>
                                {line}
                              </tspan>
                            ))}
                          </text>
                        );
                      }}
                      fontSize={13}
                      tickLine={false}
                      axisLine={false}
                    />
                    <Tooltip content={<BarTooltip />} cursor={false} />
                    <Legend />
                    <Bar
                      xAxisId="cost"
                      dataKey="cost"
                      fill="#8b5cf6"
                      radius={[0, 4, 4, 0]}
                      name="Cost"
                    />
                    <Bar
                      xAxisId="requests"
                      dataKey="requests"
                      fill="#f59e0b"
                      radius={[0, 4, 4, 0]}
                      opacity={0.6}
                      name="Requests"
                    />
                  </BarChart>
                </ChartContainer>
              ) : (
                <div className="grid grid-cols-2 gap-2">
                  <div className="relative">
                    <ChartContainer config={costChartConfig} className="h-[260px] w-full">
                      <PieChart>
                        <Pie
                          data={chartData}
                          dataKey="cost"
                          nameKey="model"
                          cx="50%"
                          cy="50%"
                          innerRadius="45%"
                          outerRadius="75%"
                          strokeWidth={1}
                          label={({ percent }) =>
                            percent > 0.03 ? `${(percent * 100).toFixed(1)}%` : ""
                          }
                          labelLine={false}
                          fontSize={14}
                        >
                          {chartData.map((_, i) => (
                            <Cell key={i} fill={PIE_COLORS[i % PIE_COLORS.length]} />
                          ))}
                        </Pie>
                        <Tooltip content={<PieTooltip />} />
                      </PieChart>
                    </ChartContainer>
                    <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
                      <span className="text-sm font-medium text-muted-foreground">Cost</span>
                    </div>
                  </div>
                  <div className="relative">
                    <ChartContainer config={costChartConfig} className="h-[260px] w-full">
                      <PieChart>
                        <Pie
                          data={chartData}
                          dataKey="requests"
                          nameKey="model"
                          cx="50%"
                          cy="50%"
                          innerRadius="45%"
                          outerRadius="75%"
                          strokeWidth={1}
                          label={({ percent }) =>
                            percent > 0.03 ? `${(percent * 100).toFixed(1)}%` : ""
                          }
                          labelLine={false}
                          fontSize={14}
                        >
                          {chartData.map((_, i) => (
                            <Cell key={i} fill={PIE_COLORS[i % PIE_COLORS.length]} />
                          ))}
                        </Pie>
                        <Tooltip content={<PieTooltip />} />
                      </PieChart>
                    </ChartContainer>
                    <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
                      <span className="text-sm font-medium text-muted-foreground">Requests</span>
                    </div>
                  </div>
                  <div className="col-span-2 flex flex-wrap justify-center gap-x-3 gap-y-1 text-sm pb-1">
                    {chartData.map((entry, i) => (
                      <div key={entry.model} className="flex items-center gap-1.5">
                        <span
                          className="inline-block h-2.5 w-2.5 shrink-0 rounded-[2px]"
                          style={{ backgroundColor: PIE_COLORS[i % PIE_COLORS.length] }}
                        />
                        <span className="text-muted-foreground">{entry.model}</span>
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base font-medium">
                Tokens by Model
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer
                config={tokenChartConfig}
                style={{ height: Math.max(300, chartData.length * 50) }}
                className="w-full"
              >
                <BarChart
                  data={chartData}
                  layout="vertical"
                  margin={{ top: 0, right: 16, bottom: 0, left: 0 }}
                >
                  <CartesianGrid horizontal={false} strokeDasharray="3 3" />
                  <XAxis
                    type="number"
                    tickFormatter={formatCompact}
                    fontSize={13}
                    tick={{ fill: "currentColor" }}
                  />
                  <YAxis
                    dataKey="model"
                    type="category"
                    width={180}
                    tick={({ x, y, payload }) => {
                      const text = String(payload.value);
                      const lines: string[] = [];
                      if (text.length <= 22) {
                        lines.push(text);
                      } else {
                        let remaining = text;
                        while (remaining.length > 22) {
                          const chunk = remaining.slice(0, 22);
                          const dashIdx = chunk.lastIndexOf("-");
                          const breakAt = dashIdx > 8 ? dashIdx + 1 : 22;
                          lines.push(remaining.slice(0, breakAt));
                          remaining = remaining.slice(breakAt);
                        }
                        if (remaining) lines.push(remaining);
                      }
                      return (
                        <text x={x} y={y} textAnchor="end" fontSize={13} fill="currentColor">
                          {lines.map((line, i) => (
                            <tspan key={i} x={x} dy={i === 0 ? "0.35em" : "1.2em"}>
                              {line}
                            </tspan>
                          ))}
                        </text>
                      );
                    }}
                    fontSize={13}
                    tickLine={false}
                    axisLine={false}
                  />
                  <Tooltip content={<BarTooltip />} cursor={false} />
                  <Legend />
                  <Bar
                    dataKey="input_tokens"
                    stackId="tokens"
                    fill="#3b82f6"
                    radius={[0, 0, 0, 0]}
                    name="Input Tokens"
                  />
                  <Bar
                    dataKey="output_tokens"
                    stackId="tokens"
                    fill="#10b981"
                    radius={[0, 4, 4, 0]}
                    name="Output Tokens"
                  />
                </BarChart>
              </ChartContainer>
            </CardContent>
          </Card>
        </div>
      )}
    </div>
  );
}
