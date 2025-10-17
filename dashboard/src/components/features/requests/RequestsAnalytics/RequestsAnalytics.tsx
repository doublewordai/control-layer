import * as React from "react";
import { X, Info, Activity } from "lucide-react";
import {
  Area,
  AreaChart,
  Line,
  LineChart,
  XAxis,
  YAxis,
  Label,
  Pie,
  PieChart,
  Sector,
} from "recharts";
import { useRequestsAggregate, useModels } from "../../../../api/dwctl";
import { useMockAggregateData } from "../../../../api/demo/mockRequests";
import { useSettings } from "../../../../contexts";
import { Card, CardContent, CardHeader, CardTitle } from "../../../ui/card";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
} from "../../../ui/chart";
import type { ChartConfig } from "../../../ui/chart";

interface RequestsAnalyticsProps {
  selectedModel?: string;
  dateRange?: { from: Date; to: Date };
}

export function RequestsAnalytics({
  selectedModel,
  dateRange,
}: RequestsAnalyticsProps) {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const realDataQuery = useRequestsAggregate(selectedModel, dateRange);
  const mockDataQuery = useMockAggregateData(selectedModel, dateRange);
  const { data, isLoading, error } = isDemoMode ? mockDataQuery : realDataQuery;

  const realAllModelsQuery = useRequestsAggregate(undefined, dateRange);
  const mockAllModelsQuery = useMockAggregateData(undefined, dateRange);
  const { data: allModelsData } = isDemoMode
    ? mockAllModelsQuery
    : realAllModelsQuery;
  const { data: modelsData } = useModels();

  // Initialize all React hooks at the top
  const [activeModel, setActiveModel] = React.useState("");
  const [activeStatusIndex, setActiveStatusIndex] = React.useState<
    number | undefined
  >();

  // For models, create a pie chart showing distribution - use all models data for the chart
  const modelChartData = React.useMemo(
    () =>
      allModelsData?.models
        ? allModelsData.models.slice(0, 5).map((model, index) => ({
            model: model.model,
            name: model.model.split("/").pop() || model.model,
            count: model.count,
            fill: `var(--chart-${index + 1})`,
          }))
        : [],
    [allModelsData?.models],
  );

  const modelChartConfig = allModelsData?.models
    ? ({
        ...Object.fromEntries(
          allModelsData.models.slice(0, 5).map((model, index) => [
            model.model,
            {
              label: model.model.split("/").pop() || model.model,
              color: `var(--chart-${index + 1})`,
            },
          ]),
        ),
      } satisfies ChartConfig)
    : {};

  // Update activeModel state and calculate activeIndex after data is available
  const activeModelIndex = React.useMemo(
    () => modelChartData.findIndex((item) => item.model === activeModel),
    [activeModel, modelChartData],
  );

  React.useEffect(() => {
    if (selectedModel) {
      setActiveModel(selectedModel);
    } else {
      setActiveModel(""); // Clear active model when no model is selected
    }
  }, [selectedModel]);

  // Find the selected model details if a model is selected
  const selectedModelDetails =
    selectedModel && modelsData
      ? modelsData.find((model) => model.model_name === selectedModel)
      : null;

  // Show loading state
  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-center">
          <div
            className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
            role="progressbar"
            aria-label="Loading"
          ></div>
          <p className="text-doubleword-neutral-600">Loading analytics...</p>
        </div>
      </div>
    );
  }

  // Show error state
  if (error) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-center">
          <div className="text-red-500 mb-4">
            <X className="h-12 w-12 mx-auto" />
          </div>
          <h3 className="text-lg font-medium text-red-600 mb-2">
            Error Loading Analytics
          </h3>
          <p className="text-red-600">
            {error instanceof Error
              ? error.message
              : "Failed to load analytics data"}
          </p>
        </div>
      </div>
    );
  }

  // Show empty state when no data or no requests
  if (!data || data.total_requests === 0) {
    return (
      <div className="text-center py-12">
        <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
          <Activity className="w-8 h-8 text-doubleword-neutral-600" />
        </div>
        <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
          No analytics data available
        </h3>
        <p className="text-doubleword-neutral-600">
          No requests found for the selected time period. Try adjusting the date
          range or check back later once requests start flowing through the
          gateway.
        </p>
      </div>
    );
  }

  // Calculate total tokens
  const totalInputTokens = data.time_series.reduce(
    (sum, point) => sum + point.input_tokens,
    0,
  );
  const totalOutputTokens = data.time_series.reduce(
    (sum, point) => sum + point.output_tokens,
    0,
  );

  // Transform data for status code chart - one slice per status code
  const statusChartData = data.status_codes.map((status, index) => ({
    name: status.status,
    count: status.count,
    percentage: status.percentage,
    fill: `var(--chart-${(index % 5) + 1})`,
  }));

  const statusChartConfig = Object.fromEntries(
    data.status_codes.map((status, index) => [
      status.status,
      {
        label: status.status,
        color: `var(--chart-${(index % 5) + 1})`,
      },
    ]),
  ) satisfies ChartConfig;

  const tokenChartConfig = {
    input_tokens: {
      label: "Input Tokens",
      color: "var(--chart-1)",
    },
    output_tokens: {
      label: "Output Tokens",
      color: "var(--chart-2)",
    },
  } satisfies ChartConfig;

  const latencyChartConfig = {
    avg_latency_ms: {
      label: "Avg",
      color: "var(--chart-1)",
    },
    p95_latency_ms: {
      label: "P95",
      color: "var(--chart-2)",
    },
    p99_latency_ms: {
      label: "P99",
      color: "var(--chart-3)",
    },
  } satisfies ChartConfig;

  return (
    <div className="grid grid-cols-1 lg:grid-cols-3 gap-4">
      {/* Gateway Traffic Card */}
      <Card>
        <CardHeader>
          <CardTitle className="text-sm flex items-center gap-1">
            {selectedModel
              ? `${selectedModelDetails?.alias || selectedModel} Traffic`
              : "Gateway Traffic"}
            <HoverCard openDelay={100} closeDelay={100}>
              <HoverCardTrigger asChild>
                <Info className="h-3 w-3 text-muted-foreground " />
              </HoverCardTrigger>
              <HoverCardContent className="w-80">
                <p className="text-sm">
                  Shows the total number of requests and token usage
                  (input/output) for{" "}
                  {selectedModel ? "the selected model" : "all models"} over the
                  selected time period.
                </p>
              </HoverCardContent>
            </HoverCard>
          </CardTitle>
        </CardHeader>
        <CardContent className="flex justify-center items-center min-h-32">
          <div className="text-center">
            <p className="text-sm text-doubleword-neutral-600">Requests</p>
            <div className="text-3xl font-bold">
              {data.total_requests.toLocaleString()}
            </div>
          </div>
          <div className="w-px bg-border mx-4"></div>
          <div className="text-center">
            <p className="text-sm text-doubleword-neutral-600">Tokens</p>
            <div className="text-sm">
              <span className="text-xl font-bold">
                {totalInputTokens.toLocaleString()}
              </span>{" "}
              in
            </div>
            <div className="text-sm">
              <span className="text-xl font-bold">
                {totalOutputTokens.toLocaleString()}
              </span>{" "}
              out
            </div>
          </div>
        </CardContent>
      </Card>

      {/* Status Codes Card */}
      <Card>
        <CardHeader>
          <CardTitle className="text-sm flex items-center gap-1">
            Status Code Breakdown
            <HoverCard openDelay={100} closeDelay={100}>
              <HoverCardTrigger asChild>
                <Info className="h-3 w-3 text-muted-foreground " />
              </HoverCardTrigger>
              <HoverCardContent className="w-80">
                <p className="text-sm">
                  Shows the percentage of successful requests (HTTP 2xx status
                  codes) versus failed requests. Status codes can be returned by
                  downstream AI model providers or applications.
                </p>
              </HoverCardContent>
            </HoverCard>
          </CardTitle>
        </CardHeader>
        <CardContent>
          <ChartContainer config={statusChartConfig}>
            <PieChart>
              <ChartTooltip
                cursor={false}
                content={({ active, payload }) => {
                  if (active && payload && payload.length) {
                    const pieData = payload[0];
                    const statusCode = pieData.name;
                    const count = pieData.value;
                    const percentage = pieData.payload?.percentage || 0;

                    return (
                      <div className="rounded-lg border bg-background p-2 shadow-sm">
                        <div className="grid gap-2">
                          <div className="flex flex-col">
                            <span className="text-[0.70rem] uppercase text-muted-foreground">
                              Status {statusCode}
                            </span>
                            <span className="font-bold">
                              {count} requests ({percentage.toFixed(1)}%)
                            </span>
                          </div>
                        </div>
                      </div>
                    );
                  }
                  return null;
                }}
              />
              <Pie
                data={statusChartData}
                dataKey="count"
                nameKey="name"
                innerRadius="50%"
                outerRadius="100%"
                strokeWidth={5}
                activeIndex={activeStatusIndex}
                animationDuration={400}
                onMouseEnter={(_, index) => setActiveStatusIndex(index)}
                onMouseLeave={() => setActiveStatusIndex(undefined)}
                activeShape={({ outerRadius = 0, ...props }: any) => (
                  <Sector {...props} outerRadius={outerRadius + 4} />
                )}
              >
                <Label
                  content={({ viewBox }) => {
                    if (viewBox && "cx" in viewBox && "cy" in viewBox) {
                      const successRate = data.status_codes
                        .filter((status) => status.status.startsWith("2"))
                        .reduce((sum, status) => sum + status.percentage, 0);
                      const displayRate =
                        successRate >= 99 && successRate < 99.95
                          ? successRate.toFixed(1)
                          : Math.round(successRate);
                      return (
                        <text
                          x={viewBox.cx}
                          y={viewBox.cy}
                          textAnchor="middle"
                          dominantBaseline="middle"
                        >
                          <tspan
                            x={viewBox.cx}
                            y={viewBox.cy}
                            className="fill-foreground text-xl font-bold"
                          >
                            {displayRate}%
                          </tspan>
                          <tspan
                            x={viewBox.cx}
                            y={(viewBox.cy || 0) + 18}
                            className="fill-muted-foreground text-xs"
                          >
                            2xx Codes
                          </tspan>
                        </text>
                      );
                    }
                  }}
                />
              </Pie>
            </PieChart>
          </ChartContainer>
        </CardContent>
      </Card>

      {/* Models Card */}
      <Card>
        <CardHeader>
          <CardTitle className="text-sm flex items-center gap-1">
            Model Breakdown
            <HoverCard openDelay={100} closeDelay={100}>
              <HoverCardTrigger asChild>
                <Info className="h-3 w-3 text-muted-foreground " />
              </HoverCardTrigger>
              <HoverCardContent className="w-80">
                <p className="text-sm">
                  {selectedModel
                    ? "Shows the percentage of total traffic handled by the selected model."
                    : "Shows the distribution of requests across different AI models. Click on a model in the navigation to see detailed analytics for that specific model."}
                </p>
              </HoverCardContent>
            </HoverCard>
          </CardTitle>
        </CardHeader>
        <CardContent>
          {modelChartData.length > 0 ? (
            <ChartContainer config={modelChartConfig}>
              <PieChart>
                <ChartTooltip
                  cursor={false}
                  content={<ChartTooltipContent hideLabel />}
                />
                <Pie
                  data={modelChartData}
                  dataKey="count"
                  nameKey="name"
                  innerRadius="50%"
                  outerRadius="100%"
                  strokeWidth={5}
                  activeIndex={
                    activeModelIndex >= 0 ? activeModelIndex : undefined
                  }
                  animationDuration={400}
                  activeShape={({ outerRadius = 0, ...props }: any) => (
                    <Sector {...props} outerRadius={outerRadius + 4} />
                  )}
                >
                  <Label
                    content={({ viewBox }) => {
                      if (viewBox && "cx" in viewBox && "cy" in viewBox) {
                        const activeData = modelChartData[activeModelIndex];
                        const totalAllModels = modelChartData.reduce(
                          (sum, model) => sum + model.count,
                          0,
                        );
                        const percentage =
                          activeModelIndex >= 0 &&
                          activeData &&
                          totalAllModels > 0
                            ? Math.round(
                                (activeData.count / totalAllModels) * 100,
                              )
                            : 100; // Show 100% when no specific model is selected
                        return (
                          <text
                            x={viewBox.cx}
                            y={viewBox.cy}
                            textAnchor="middle"
                            dominantBaseline="middle"
                          >
                            <tspan
                              x={viewBox.cx}
                              y={viewBox.cy}
                              className="fill-foreground text-xl font-bold"
                            >
                              {percentage}%
                            </tspan>
                          </text>
                        );
                      }
                    }}
                  />
                </Pie>
              </PieChart>
            </ChartContainer>
          ) : (
            <div className="text-xs">No model data available</div>
          )}
        </CardContent>
      </Card>

      {/* Time Series Charts */}
      {selectedModel &&
      data.time_series.some((point) => point.avg_latency_ms != null) ? (
        <>
          <Card>
            <CardHeader>
              <CardTitle className="text-sm flex items-center gap-1">
                Requests Over Time
                <HoverCard openDelay={100} closeDelay={100}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-muted-foreground " />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-80">
                    <p className="text-sm">
                      Shows the trend of incoming requests over time for the
                      selected model. Use this to identify traffic patterns and
                      peak usage periods.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer
                config={{ requests: { color: "var(--chart-1)" } }}
              >
                <AreaChart data={data.time_series}>
                  <XAxis
                    dataKey="timestamp"
                    tickFormatter={(value) =>
                      new Date(value).toLocaleTimeString("en-US", {
                        hour: "2-digit",
                        minute: "2-digit",
                        hour12: false,
                      })
                    }
                  />
                  <YAxis />
                  <ChartTooltip content={<ChartTooltipContent />} />
                  <Area
                    dataKey="requests"
                    type="monotone"
                    fill="var(--chart-1)"
                    fillOpacity={0.3}
                    stroke="var(--chart-1)"
                  />
                </AreaChart>
              </ChartContainer>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm flex items-center gap-1">
                Tokens Over Time
                <HoverCard openDelay={100} closeDelay={100}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-muted-foreground" />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-80">
                    <p className="text-sm">
                      Shows the input and output token consumption over time.
                      Input tokens represent the prompt/request size, while
                      output tokens show the response size generated by the
                      model.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer config={tokenChartConfig}>
                <AreaChart data={data.time_series}>
                  <XAxis
                    dataKey="timestamp"
                    tickFormatter={(value) =>
                      new Date(value).toLocaleTimeString("en-US", {
                        hour: "2-digit",
                        minute: "2-digit",
                        hour12: false,
                      })
                    }
                  />
                  <YAxis tickFormatter={(value) => value.toLocaleString()} />
                  <ChartTooltip content={<ChartTooltipContent />} />
                  <Area
                    dataKey="input_tokens"
                    type="monotone"
                    fill="var(--chart-1)"
                    fillOpacity={0.4}
                    stroke="var(--chart-1)"
                    stackId="a"
                  />
                  <Area
                    dataKey="output_tokens"
                    type="monotone"
                    fill="var(--chart-2)"
                    fillOpacity={0.4}
                    stroke="var(--chart-2)"
                    stackId="a"
                  />
                </AreaChart>
              </ChartContainer>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm flex items-center gap-1">
                Latency Over Time
                <HoverCard openDelay={100} closeDelay={100}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-muted-foreground" />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-80">
                    <p className="text-sm">
                      Shows response latency metrics over time for the selected
                      model. Displays average (typical), P95 (95% of requests
                      faster), and P99 (99% of requests faster) response times.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer config={latencyChartConfig}>
                <LineChart data={data.time_series}>
                  <XAxis
                    dataKey="timestamp"
                    tickFormatter={(value) =>
                      new Date(value).toLocaleTimeString("en-US", {
                        hour: "2-digit",
                        minute: "2-digit",
                        hour12: false,
                      })
                    }
                  />
                  <YAxis tickFormatter={(value) => `${value}ms`} />
                  <ChartTooltip content={<ChartTooltipContent />} />
                  <Line
                    dataKey="avg_latency_ms"
                    type="monotone"
                    stroke="var(--chart-1)"
                    strokeWidth={2}
                    connectNulls={true}
                  />
                  <Line
                    dataKey="p95_latency_ms"
                    type="monotone"
                    stroke="var(--chart-2)"
                    strokeWidth={2}
                    connectNulls={true}
                  />
                  <Line
                    dataKey="p99_latency_ms"
                    type="monotone"
                    stroke="var(--chart-3)"
                    strokeWidth={2}
                    connectNulls={true}
                  />
                </LineChart>
              </ChartContainer>
            </CardContent>
          </Card>
        </>
      ) : (
        <>
          <Card>
            <CardHeader>
              <CardTitle className="text-sm flex items-center gap-1">
                Requests Over Time
                <HoverCard openDelay={100} closeDelay={100}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-muted-foreground" />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-80">
                    <p className="text-sm">
                      Shows the trend of incoming requests over time across all
                      models. Use this to identify traffic patterns, peak usage
                      periods, and overall system load.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer
                config={{ requests: { color: "var(--chart-1)" } }}
              >
                <AreaChart data={data.time_series}>
                  <XAxis
                    dataKey="timestamp"
                    tickFormatter={(value) =>
                      new Date(value).toLocaleTimeString("en-US", {
                        hour: "2-digit",
                        minute: "2-digit",
                        hour12: false,
                      })
                    }
                  />
                  <YAxis />
                  <ChartTooltip content={<ChartTooltipContent />} />
                  <Area
                    dataKey="requests"
                    type="monotone"
                    fill="var(--chart-1)"
                    fillOpacity={0.3}
                    stroke="var(--chart-1)"
                  />
                </AreaChart>
              </ChartContainer>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm flex items-center gap-1">
                Tokens Over Time
                <HoverCard openDelay={100} closeDelay={100}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-muted-foreground" />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-80">
                    <p className="text-sm">
                      Shows the input and output token consumption over time.
                      Input tokens represent the prompt/request size, while
                      output tokens show the response size generated by the
                      model.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </CardTitle>
            </CardHeader>
            <CardContent>
              <ChartContainer config={tokenChartConfig}>
                <AreaChart data={data.time_series}>
                  <XAxis
                    dataKey="timestamp"
                    tickFormatter={(value) =>
                      new Date(value).toLocaleTimeString("en-US", {
                        hour: "2-digit",
                        minute: "2-digit",
                        hour12: false,
                      })
                    }
                  />
                  <YAxis tickFormatter={(value) => value.toLocaleString()} />
                  <ChartTooltip content={<ChartTooltipContent />} />
                  <Area
                    dataKey="input_tokens"
                    type="monotone"
                    fill="var(--chart-1)"
                    fillOpacity={0.4}
                    stroke="var(--chart-1)"
                    stackId="a"
                  />
                  <Area
                    dataKey="output_tokens"
                    type="monotone"
                    fill="var(--chart-2)"
                    fillOpacity={0.4}
                    stroke="var(--chart-2)"
                    stackId="a"
                  />
                </AreaChart>
              </ChartContainer>
            </CardContent>
          </Card>
        </>
      )}
    </div>
  );
}
