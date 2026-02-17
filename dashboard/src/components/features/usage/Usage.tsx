import { useMemo } from "react";
import { useUsage } from "@/api/control-layer/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Legend } from "recharts";
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

const tokenChartConfig = {
  input_tokens: { label: "Input Tokens", color: "#3b82f6" },
  output_tokens: { label: "Output Tokens", color: "#10b981" },
} satisfies ChartConfig;

const costChartConfig = {
  cost: { label: "Cost", color: "#8b5cf6" },
  requests: { label: "Requests", color: "#f59e0b" },
} satisfies ChartConfig;

export function Usage() {
  const { data: usage, isLoading } = useUsage();

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
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium">
                Tokens by Model
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer
                config={tokenChartConfig}
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
                    tickFormatter={formatCompact}
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
                        formatter={(value) => formatNumber(value as number)}
                      />
                    }
                  />
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

          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium">
                Cost & Requests by Model
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer
                config={costChartConfig}
                className="h-[300px] w-full"
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
                    orientation="bottom"
                    tickFormatter={(v: number) => formatDollars(v, 2)}
                    fontSize={12}
                  />
                  <XAxis
                    xAxisId="requests"
                    type="number"
                    orientation="top"
                    tickFormatter={formatCompact}
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
                        formatter={(value, name) => {
                          if (name === "cost") return formatDollars(value as number, 4);
                          return formatNumber(value as number);
                        }}
                      />
                    }
                  />
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
            </CardContent>
          </Card>
        </div>
      )}
    </div>
  );
}
