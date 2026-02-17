import { useMemo, useState } from "react";
import { useUsage } from "@/api/control-layer/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import { BarChart, Bar, XAxis, YAxis, CartesianGrid } from "recharts";
import { formatDollars } from "@/utils/money";

function formatNumber(n: number): string {
  return new Intl.NumberFormat("en-US").format(n);
}

function formatCompact(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

function StatCard({
  title,
  value,
  description,
}: {
  title: string;
  value: string;
  description?: string;
}) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">
          {title}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className="text-2xl font-bold">{value}</div>
        {description && (
          <p className="text-xs text-muted-foreground mt-1">{description}</p>
        )}
      </CardContent>
    </Card>
  );
}

function TokenBreakdownCard({
  input,
  output,
}: {
  input: number;
  output: number;
}) {
  const total = input + output;
  const inputPct = total > 0 ? (input / total) * 100 : 50;
  const outputPct = total > 0 ? (output / total) * 100 : 50;

  return (
    <Card className="col-span-2 lg:col-span-2">
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">
          Total Tokens
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="text-2xl font-bold">{formatNumber(total)}</div>
        <div className="h-2.5 rounded-full bg-muted overflow-hidden flex">
          <div
            className="h-full bg-blue-500 transition-all"
            style={{ width: `${inputPct}%` }}
          />
          <div
            className="h-full bg-emerald-500 transition-all"
            style={{ width: `${outputPct}%` }}
          />
        </div>
        <div className="flex justify-between text-sm">
          <div className="flex items-center gap-2">
            <span className="inline-block w-2.5 h-2.5 rounded-full bg-blue-500" />
            <span className="text-muted-foreground">Input</span>
            <span className="font-medium">{formatCompact(input)}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="inline-block w-2.5 h-2.5 rounded-full bg-emerald-500" />
            <span className="text-muted-foreground">Output</span>
            <span className="font-medium">{formatCompact(output)}</span>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

type ChartMetric = "requests" | "batches" | "tokens" | "cost";

const CHART_METRICS: { value: ChartMetric; label: string }[] = [
  { value: "requests", label: "Requests" },
  { value: "batches", label: "Batches" },
  { value: "tokens", label: "Tokens" },
  { value: "cost", label: "Cost" },
];

export function Usage() {
  const { data: usage, isLoading } = useUsage();
  const [chartMetric, setChartMetric] = useState<ChartMetric>("requests");

  const { chartData, chartConfig } = useMemo(() => {
    if (!usage?.by_model.length) return { chartData: [], chartConfig: {} };

    const models = usage.by_model.slice(0, 8);
    const data = models.map((entry, index) => ({
      model: entry.model.split("/").pop() || entry.model,
      requests: entry.request_count,
      batches: entry.batch_count,
      tokens: entry.input_tokens + entry.output_tokens,
      cost: parseFloat(entry.cost),
      fill: `var(--chart-${(index % 5) + 1})`,
    }));

    const config = Object.fromEntries(
      models.map((entry, index) => [
        entry.model.split("/").pop() || entry.model,
        {
          label: entry.model.split("/").pop() || entry.model,
          color: `var(--chart-${(index % 5) + 1})`,
        },
      ]),
    ) satisfies ChartConfig;

    return { chartData: data, chartConfig: config };
  }, [usage?.by_model]);

  const chartFormatter = (value: number) => {
    if (chartMetric === "cost") return formatDollars(value, 2);
    return formatNumber(value);
  };

  if (isLoading) {
    return (
      <div className="p-6 md:p-8 space-y-6">
        <h1 className="text-2xl font-bold tracking-tight">Usage</h1>
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
        <h1 className="text-2xl font-bold tracking-tight">Usage</h1>
        <p className="text-muted-foreground">No usage data available.</p>
      </div>
    );
  }

  const totalCost = parseFloat(usage.total_cost);

  return (
    <div className="p-6 md:p-8 space-y-6">
      <h1 className="text-2xl font-bold tracking-tight">Usage</h1>

      <div className="grid gap-4 grid-cols-2 lg:grid-cols-4">
        <TokenBreakdownCard
          input={usage.total_input_tokens}
          output={usage.total_output_tokens}
        />
        <StatCard
          title="Total Requests"
          value={formatNumber(usage.total_request_count)}
        />
        <StatCard
          title="Total Cost"
          value={formatDollars(totalCost, 4)}
        />
        <StatCard
          title="Total Batches"
          value={formatNumber(usage.total_batch_count)}
        />
        <StatCard
          title="Avg Requests / Batch"
          value={usage.avg_requests_per_batch.toFixed(1)}
        />
      </div>

      {usage.by_model.length > 0 && (
        <div className="grid gap-4 lg:grid-cols-2">
          <Card>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <CardTitle className="text-sm font-medium">
                By Model
              </CardTitle>
              <div className="flex gap-1 rounded-lg bg-muted p-0.5">
                {CHART_METRICS.map((m) => (
                  <button
                    key={m.value}
                    onClick={() => setChartMetric(m.value)}
                    className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors ${
                      chartMetric === m.value
                        ? "bg-background text-foreground shadow-sm"
                        : "text-muted-foreground hover:text-foreground"
                    }`}
                  >
                    {m.label}
                  </button>
                ))}
              </div>
            </CardHeader>
            <CardContent>
              <ChartContainer
                config={chartConfig}
                className="h-[300px] w-full"
              >
                <BarChart
                  data={chartData}
                  layout="vertical"
                  margin={{ top: 0, right: 16, bottom: 0, left: 0 }}
                >
                  <CartesianGrid horizontal={false} strokeDasharray="3 3" />
                  <XAxis
                    type="number"
                    tickFormatter={chartMetric === "cost" ? (v: number) => formatDollars(v, 0) : formatCompact}
                    fontSize={12}
                  />
                  <YAxis
                    dataKey="model"
                    type="category"
                    width={140}
                    fontSize={12}
                    tickLine={false}
                    axisLine={false}
                  />
                  <ChartTooltip
                    content={
                      <ChartTooltipContent
                        formatter={(value) => chartFormatter(value as number)}
                      />
                    }
                  />
                  <Bar
                    dataKey={chartMetric}
                    radius={[0, 4, 4, 0]}
                  />
                </BarChart>
              </ChartContainer>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm font-medium">
                Per-Model Breakdown
              </CardTitle>
            </CardHeader>
            <CardContent>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Model</TableHead>
                    <TableHead className="text-right">Input</TableHead>
                    <TableHead className="text-right">Output</TableHead>
                    <TableHead className="text-right">Cost</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {usage.by_model.map((entry) => (
                    <TableRow key={entry.model}>
                      <TableCell className="font-medium">
                        {entry.model}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {formatCompact(entry.input_tokens)}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {formatCompact(entry.output_tokens)}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {formatDollars(parseFloat(entry.cost), 4)}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </CardContent>
          </Card>
        </div>
      )}
    </div>
  );
}
