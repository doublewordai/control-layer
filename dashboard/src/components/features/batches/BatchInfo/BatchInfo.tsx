import React from "react";
import { useParams, useNavigate, useSearchParams } from "react-router-dom";
import {
  ArrowLeft,
  FileInput,
  FileCheck,
  AlertCircle,
  Activity,
  Clock,
  CheckCircle,
  XCircle,
  Ban,
  Loader2,
} from "lucide-react";
import {
  useBatch,
  useBatchAnalytics,
} from "../../../../api/control-layer/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "../../../ui/card";
import { Badge } from "../../../ui/badge";
import { Button } from "../../../ui/button";
import { Skeleton } from "../../../ui/skeleton";
import type { BatchStatus } from "../../../../api/control-layer/types";

const BatchInfo: React.FC = () => {
  const { batchId } = useParams<{ batchId: string }>();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();

  const fromUrl = searchParams.get("from");

  const { data: batch, isLoading, error } = useBatch(batchId!);
  const { data: analytics, isLoading: analyticsLoading } = useBatchAnalytics(
    batchId!,
  );

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div
            className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
            aria-label="Loading"
          ></div>
          <p className="text-doubleword-neutral-600">
            Loading batch details...
          </p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="text-red-500 mb-4">
            <ArrowLeft className="h-12 w-12 mx-auto" />
          </div>
          <p className="text-red-600 font-semibold">
            Error: {error instanceof Error ? error.message : "Unknown error"}
          </p>
          <Button
            variant="outline"
            onClick={() => navigate(fromUrl || "/batches")}
            className="mt-4"
          >
            <ArrowLeft className="mr-2 h-4 w-4" />
            {fromUrl ? "Go Back" : "Back to Batches"}
          </Button>
        </div>
      </div>
    );
  }

  if (!batch) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <p className="text-gray-600 font-semibold">Batch not found</p>
          <Button
            variant="outline"
            onClick={() => navigate(fromUrl || "/batches")}
            className="mt-4"
          >
            <ArrowLeft className="mr-2 h-4 w-4" />
            {fromUrl ? "Go Back" : "Back to Batches"}
          </Button>
        </div>
      </div>
    );
  }

  const getStatusBadge = (status: BatchStatus) => {
    const statusConfig: Record<
      BatchStatus,
      {
        label: string;
        variant: "default" | "destructive" | "outline" | "secondary";
        icon: React.ReactNode;
      }
    > = {
      validating: {
        label: "Validating",
        variant: "secondary",
        icon: <Loader2 className="w-3 h-3 animate-spin" />,
      },
      in_progress: {
        label: "In Progress",
        variant: "default",
        icon: <Activity className="w-3 h-3" />,
      },
      finalizing: {
        label: "Finalizing",
        variant: "secondary",
        icon: <Loader2 className="w-3 h-3 animate-spin" />,
      },
      completed: {
        label: "Completed",
        variant: "outline",
        icon: <CheckCircle className="w-3 h-3 text-green-600" />,
      },
      failed: {
        label: "Failed",
        variant: "destructive",
        icon: <XCircle className="w-3 h-3" />,
      },
      expired: {
        label: "Expired",
        variant: "outline",
        icon: <Clock className="w-3 h-3 text-gray-500" />,
      },
      cancelling: {
        label: "Cancelling",
        variant: "secondary",
        icon: <Loader2 className="w-3 h-3 animate-spin" />,
      },
      cancelled: {
        label: "Cancelled",
        variant: "outline",
        icon: <Ban className="w-3 h-3 text-gray-500" />,
      },
    };

    const config = statusConfig[status];
    return (
      <Badge variant={config.variant} className="flex items-center gap-1 w-fit">
        {config.icon}
        {config.label}
      </Badge>
    );
  };

  const formatTimestamp = (timestamp: number | null | undefined) => {
    if (!timestamp) return "N/A";
    return new Date(timestamp * 1000).toLocaleString();
  };

  const formatDuration = (
    startTimestamp: number | null | undefined,
    endTimestamp: number | null | undefined,
  ) => {
    if (!startTimestamp || !endTimestamp) return "N/A";
    const durationMs = (endTimestamp - startTimestamp) * 1000;
    const seconds = Math.floor(durationMs / 1000);
    const minutes = Math.floor(seconds / 60);
    const hours = Math.floor(minutes / 60);

    if (hours > 0) {
      return `${hours}h ${minutes % 60}m`;
    } else if (minutes > 0) {
      return `${minutes}m ${seconds % 60}s`;
    } else {
      return `${seconds}s`;
    }
  };

  const progress =
    batch.request_counts.total > 0
      ? Math.round(
          (batch.request_counts.completed / batch.request_counts.total) * 100,
        )
      : 0;

  const description = batch.metadata?.batch_description;

  return (
    <div className="p-6">
      {/* Header */}
      <div className="mb-6">
        <div className="flex items-center gap-4 mb-4">
          <button
            onClick={() => navigate(fromUrl || "/batches")}
            className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors"
            aria-label={fromUrl ? "Go back" : "Back to Batches"}
            title={fromUrl ? "Go back" : "Back to Batches"}
          >
            <ArrowLeft className="w-5 h-5" />
          </button>
          <div className="flex-1">
            <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
              <div>
                <h1 className="text-3xl font-bold text-doubleword-neutral-900">
                  Batch Details
                </h1>
                <p className="text-doubleword-neutral-600 mt-1 font-mono text-sm">
                  {batch.id}
                </p>
              </div>
              <div className="flex items-center gap-3">
                {getStatusBadge(batch.status)}
              </div>
            </div>
          </div>
        </div>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
        {/* Main Content */}
        <div className="lg:col-span-2 space-y-6">
          {/* Progress Card */}
          {batch.status !== "failed" &&
            batch.status !== "cancelled" &&
            batch.status !== "expired" && (
              <Card className="p-0 gap-0 rounded-lg">
                <CardHeader className="px-6 pt-5 pb-4">
                  <CardTitle>Progress</CardTitle>
                </CardHeader>
                <CardContent className="px-6 pb-6 pt-0">
                  <div className="space-y-4">
                    {/* Progress Bar */}
                    <div className="space-y-2">
                      <div className="flex justify-between text-sm">
                        <span className="text-gray-600">Overall Progress</span>
                        <span className="font-medium">{progress}%</span>
                      </div>
                      <div className="w-full rounded-full h-2.5">
                        <div
                          className="bg-blue-600 h-2.5 rounded-full transition-all duration-300"
                          style={{ width: `${progress}%` }}
                        ></div>
                      </div>
                    </div>

                    {/* Request Counts */}
                    <div className="grid grid-cols-3 gap-4 pt-4">
                      <div className="text-center p-3 rounded-lg">
                        <p className="text-2xl font-bold text-gray-900">
                          {batch.request_counts.total}
                        </p>
                        <p className="text-xs text-gray-600 mt-1">
                          Total Requests
                        </p>
                      </div>
                      <div className="text-center p-3 rounded-lg">
                        <p className="text-2xl font-bold text-green-700">
                          {batch.request_counts.completed}
                        </p>
                        <p className="text-xs text-gray-600 mt-1">Completed</p>
                      </div>
                      <div className="text-center p-3 rounded-lg">
                        <p className="text-2xl font-bold text-red-700">
                          {batch.request_counts.failed}
                        </p>
                        <p className="text-xs text-gray-600 mt-1">Failed</p>
                      </div>
                    </div>
                  </div>
                </CardContent>
              </Card>
            )}

          {/* Analytics Card */}
          {analytics && (
            <Card className="p-0 gap-0 rounded-lg">
              <CardHeader className="px-6 pt-5 pb-4">
                <CardTitle>Metrics</CardTitle>
              </CardHeader>
              <CardContent className="px-6 pb-6 pt-0">
                {analyticsLoading ? (
                  <div className="space-y-4">
                    <Skeleton className="h-20 w-full" />
                    <Skeleton className="h-20 w-full" />
                  </div>
                ) : analytics.total_requests > 0 ? (
                  <div className="space-y-6">
                    {/* Token Usage */}
                    <div>
                      <h4 className="text-sm font-medium text-gray-900 mb-3">
                        Token Usage
                      </h4>
                      <div className="grid grid-cols-3 gap-4">
                        <div className="text-center p-3 rounded-lg">
                          <p className="text-2xl font-bold">
                            {analytics.total_prompt_tokens.toLocaleString()}
                          </p>
                          <p className="text-xs text-gray-600 mt-1">
                            Prompt Tokens
                          </p>
                        </div>
                        <div className="text-center p-3 rounded-lg">
                          <p className="text-2xl font-bold">
                            {analytics.total_completion_tokens.toLocaleString()}
                          </p>
                          <p className="text-xs text-gray-600 mt-1">
                            Completion Tokens
                          </p>
                        </div>
                        <div className="text-center p-3 rounded-lg">
                          <p className="text-2xl font-bold text-gray-900">
                            {analytics.total_tokens.toLocaleString()}
                          </p>
                          <p className="text-xs text-gray-600 mt-1">
                            Total Tokens
                          </p>
                        </div>
                      </div>
                    </div>

                    {/* Cost */}
                    {analytics.total_cost &&
                      parseFloat(analytics.total_cost) > 0 && (
                        <div className="border-t pt-6">
                          <h4 className="text-sm font-medium text-gray-900 mb-3">
                            Cost
                          </h4>
                          <div className="p-4 rounded-lg text-center">
                            <p className="text-3xl font-bold text-green-700">
                              ${parseFloat(analytics.total_cost).toFixed(4)}
                            </p>
                            <p className="text-xs text-gray-600 mt-1">
                              Total Cost
                            </p>
                          </div>
                        </div>
                      )}
                  </div>
                ) : (
                  <div className="text-center py-8 text-gray-500">
                    <p className="text-sm">No analytics data available yet.</p>
                    <p className="text-xs mt-1">
                      Analytics will appear as requests complete.
                    </p>
                  </div>
                )}
              </CardContent>
            </Card>
          )}

          {/* Batch Details */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="px-6 pt-5 pb-4">
              <CardTitle>Batch Information</CardTitle>
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              <div className="space-y-6">
                <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Endpoint</p>
                    <p className="font-medium font-mono text-sm">
                      {batch.endpoint}
                    </p>
                  </div>
                  <div>
                    <p className="text-sm text-gray-600 mb-1">
                      Completion Window
                    </p>
                    <p className="font-medium">{batch.completion_window}</p>
                  </div>
                </div>

                {description && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Description</p>
                    <p className="text-gray-700">{description}</p>
                  </div>
                )}

                {/* Files */}
                <div className="border-t pt-6">
                  <h4 className="text-sm font-medium text-gray-900 mb-3">
                    Associated Files
                  </h4>
                  <div className="space-y-2">
                    <div className="flex items-center gap-2 p-2 bg-gray-50 rounded">
                      <FileInput className="w-4 h-4 text-gray-600" />
                      <div className="flex-1 min-w-0">
                        <p className="text-sm font-medium text-gray-700">
                          Input File
                        </p>
                        <p className="text-xs text-gray-500 font-mono truncate">
                          {batch.input_file_id}
                        </p>
                      </div>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() =>
                          navigate(
                            `/batches/files/${batch.input_file_id}/content`,
                          )
                        }
                        className="shrink-0"
                      >
                        View
                      </Button>
                    </div>

                    {batch.output_file_id && (
                      <div className="flex items-center gap-2 p-2 bg-green-50 rounded">
                        <FileCheck className="w-4 h-4 text-green-600" />
                        <div className="flex-1 min-w-0">
                          <p className="text-sm font-medium text-gray-700">
                            Output File
                          </p>
                          <p className="text-xs text-gray-500 font-mono truncate">
                            {batch.output_file_id}
                          </p>
                        </div>
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={() =>
                            navigate(
                              `/batches/files/${batch.output_file_id}/content`,
                            )
                          }
                          className="shrink-0"
                        >
                          View
                        </Button>
                      </div>
                    )}

                    {batch.error_file_id && (
                      <div className="flex items-center gap-2 p-2 bg-red-50 rounded">
                        <AlertCircle className="w-4 h-4 text-red-600" />
                        <div className="flex-1 min-w-0">
                          <p className="text-sm font-medium text-gray-700">
                            Error File
                          </p>
                          <p className="text-xs text-gray-500 font-mono truncate">
                            {batch.error_file_id}
                          </p>
                        </div>
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={() =>
                            navigate(
                              `/batches/files/${batch.error_file_id}/content`,
                            )
                          }
                          className="shrink-0"
                        >
                          View
                        </Button>
                      </div>
                    )}
                  </div>
                </div>

                {/* Errors */}
                {batch.errors && batch.errors.data.length > 0 && (
                  <div className="border-t pt-6">
                    <h4 className="text-sm font-medium text-gray-900 mb-3">
                      Errors
                    </h4>
                    <div className="space-y-2">
                      {batch.errors.data.map((error, index) => (
                        <div
                          key={index}
                          className="p-3 bg-red-50 border border-red-200 rounded-lg"
                        >
                          <div className="flex items-start gap-2">
                            <AlertCircle className="w-4 h-4 text-red-600 mt-0.5 shrink-0" />
                            <div className="flex-1 min-w-0">
                              <p className="text-sm font-medium text-red-900">
                                {error.code}
                              </p>
                              <p className="text-sm text-red-700 mt-1">
                                {error.message}
                              </p>
                              {error.line && (
                                <p className="text-xs text-red-600 mt-1">
                                  Line {error.line}
                                </p>
                              )}
                            </div>
                          </div>
                        </div>
                      ))}
                    </div>
                  </div>
                )}
              </div>
            </CardContent>
          </Card>
        </div>

        {/* Sidebar */}
        <div className="space-y-6">
          {/* Timeline Card */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="px-6 pt-5 pb-4">
              <CardTitle>Timeline</CardTitle>
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              <div className="space-y-4">
                <div>
                  <p className="text-sm text-gray-600 mb-1">Created</p>
                  <p className="text-sm font-medium">
                    {formatTimestamp(batch.created_at)}
                  </p>
                </div>

                {batch.in_progress_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Started</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(batch.in_progress_at)}
                    </p>
                  </div>
                )}

                {batch.finalizing_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Finalizing</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(batch.finalizing_at)}
                    </p>
                  </div>
                )}

                {batch.completed_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Completed</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(batch.completed_at)}
                    </p>
                  </div>
                )}

                {batch.failed_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Failed</p>
                    <p className="text-sm font-medium text-red-600">
                      {formatTimestamp(batch.failed_at)}
                    </p>
                  </div>
                )}

                {batch.cancelled_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Cancelled</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(batch.cancelled_at)}
                    </p>
                  </div>
                )}

                {batch.expired_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Expired</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(batch.expired_at)}
                    </p>
                  </div>
                )}

                {batch.expires_at && !batch.expired_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Expires</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(batch.expires_at)}
                    </p>
                  </div>
                )}

                {/* Duration */}
                {batch.in_progress_at && batch.completed_at && (
                  <div className="border-t pt-4">
                    <p className="text-sm text-gray-600 mb-1">Duration</p>
                    <p className="text-sm font-medium">
                      {formatDuration(batch.in_progress_at, batch.completed_at)}
                    </p>
                  </div>
                )}

                {analytics && analytics.avg_ttfb_ms && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Avg TTFB</p>
                    <p className="text-sm font-medium">
                      {analytics.avg_ttfb_ms.toFixed(0)}ms
                    </p>
                  </div>
                )}

                {analytics && analytics.avg_duration_ms && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Avg Duration</p>
                    <p className="text-sm font-medium">
                      {analytics.avg_duration_ms.toFixed(0)}ms
                    </p>
                  </div>
                )}
              </div>
            </CardContent>
          </Card>

          {/* Metadata Card */}
          {batch.metadata && Object.keys(batch.metadata).length > 0 && (
            <Card className="p-0 gap-0 rounded-lg">
              <CardHeader className="px-6 pt-5 pb-4">
                <CardTitle>Metadata</CardTitle>
              </CardHeader>
              <CardContent className="px-6 pb-6 pt-0">
                <div className="space-y-2">
                  {Object.entries(batch.metadata).map(([key, value]) => (
                    <div key={key}>
                      <p className="text-sm text-gray-600 mb-1">{key}</p>
                      <p className="text-sm font-medium wrap-break-words">
                        {value}
                      </p>
                    </div>
                  ))}
                </div>
              </CardContent>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
};

export default BatchInfo;
