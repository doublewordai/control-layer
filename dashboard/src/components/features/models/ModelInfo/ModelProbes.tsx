import React, { useState, useMemo } from "react";
import {
  Activity,
  Plus,
  Pause,
  Play,
  StopCircle,
  RefreshCw,
  CheckCircle,
  XCircle,
  Loader2,
  Edit2,
  Check,
  X,
} from "lucide-react";
import {
  useProbes,
  useProbeResults,
  useProbeStatistics,
  useCreateProbe,
  useDeleteProbe,
  useActivateProbe,
  useDeactivateProbe,
  useExecuteProbe,
  useTestProbe,
  useUpdateProbe,
  type Probe,
  type ProbeResult,
  type Model,
} from "../../../../api/control-layer";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CardDescription,
} from "../../../ui/card";
import { Button } from "../../../ui/button";
import { Input } from "../../../ui/input";
import { Textarea } from "../../../ui/textarea";
import { Badge } from "../../../ui/badge";
import { Popover, PopoverContent, PopoverTrigger } from "../../../ui/popover";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "../../../ui/select";
import { toast } from "sonner";
import { ProbeTimeline } from "./ProbeTimeline";

interface ModelProbesProps {
  model: Model;
}

const ModelProbes: React.FC<ModelProbesProps> = ({ model }) => {
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [intervalSeconds, setIntervalSeconds] = useState(60);
  const [pingType, setPingType] = useState<"default" | "custom">("default");
  const [httpMethod, setHttpMethod] = useState("POST");
  const [requestPath, setRequestPath] = useState("");
  const [requestBody, setRequestBody] = useState("");
  const [testResult, setTestResult] = useState<ProbeResult | null>(null);
  const [testPassed, setTestPassed] = useState(false);
  const [editingInterval, setEditingInterval] = useState(false);
  const [newInterval, setNewInterval] = useState(60);
  const [isExecuting, setIsExecuting] = useState(false);

  // API hooks
  const { data: probes, isLoading: probesLoading } = useProbes();
  const createProbeMutation = useCreateProbe();
  const deleteProbeMutation = useDeleteProbe();
  const activateProbeMutation = useActivateProbe();
  const deactivateProbeMutation = useDeactivateProbe();
  const executeProbeMutation = useExecuteProbe();
  const testProbeMutation = useTestProbe();
  const updateProbeMutation = useUpdateProbe();

  // Find probe for this model (should only be one due to unique constraint)
  const modelProbe = useMemo(() => {
    return probes?.find((p) => p.deployment_id === model.id);
  }, [probes, model.id]);

  // Only fetch results/stats if probe exists
  const { data: probeResults } = useProbeResults(
    modelProbe?.id || "",
    { limit: 100 },
    { enabled: !!modelProbe },
  );
  const { data: _probeStats } = useProbeStatistics(
    modelProbe?.id || "",
    undefined,
    { enabled: !!modelProbe },
  );

  const handleTestProbe = async () => {
    // Build test parameters
    let params: { http_method?: string; request_path?: string; request_body?: Record<string, unknown> } | undefined;

    if (pingType === "custom") {
      // Validate and parse custom body if provided
      let parsedBody: Record<string, unknown> | undefined;
      if (requestBody.trim()) {
        try {
          parsedBody = JSON.parse(requestBody);
        } catch {
          toast.error("Invalid JSON in request body");
          return;
        }
      }

      params = {
        http_method: httpMethod,
        request_path: requestPath.trim() || undefined,
        request_body: parsedBody,
      };
    }

    try {
      const result = await testProbeMutation.mutateAsync({ deploymentId: model.id, params });
      setTestResult(result);
      setTestPassed(result.success);
      if (result.success) {
        toast.success(
          `Test successful! Response time: ${result.response_time_ms}ms`,
        );
      } else {
        toast.error(`Test failed: ${result.error_message || "Unknown error"}`);
      }
    } catch {
      toast.error("Failed to test probe");
      setTestPassed(false);
    }
  };

  const handleCreateProbe = async () => {
    if (!testPassed) {
      toast.error("Please test the probe successfully before creating");
      return;
    }

    // Only use custom parameters if pingType is custom
    let parsedBody = null;
    let finalHttpMethod = "POST";
    let finalRequestPath = null;

    if (pingType === "custom") {
      finalHttpMethod = httpMethod;
      finalRequestPath = requestPath.trim() || null;

      // Parse request body if provided
      if (requestBody.trim()) {
        try {
          parsedBody = JSON.parse(requestBody);
        } catch {
          toast.error("Invalid JSON in request body");
          return;
        }
      }
    }

    try {
      // Generate a random name since it's not displayed anywhere
      const randomName = `probe-${model.id.substring(0, 8)}-${Date.now()}`;
      await createProbeMutation.mutateAsync({
        name: randomName,
        deployment_id: model.id,
        interval_seconds: intervalSeconds,
        http_method: finalHttpMethod,
        request_path: finalRequestPath,
        request_body: parsedBody,
      });
      toast.success("Probe created successfully");
      setShowCreateForm(false);
      setTestResult(null);
      setTestPassed(false);
      setIntervalSeconds(60);
      setPingType("default");
      setHttpMethod("POST");
      setRequestPath("");
      setRequestBody("");
    } catch {
      toast.error("Failed to create probe");
    }
  };

  const handleDeleteProbe = async (id: string) => {
    try {
      await deleteProbeMutation.mutateAsync(id);
      toast.success("Monitoring stopped");
    } catch {
      toast.error("Failed to stop monitoring");
    }
  };

  const handleToggleActive = async (probe: Probe) => {
    try {
      if (probe.active) {
        await deactivateProbeMutation.mutateAsync(probe.id);
        toast.success("Probe paused");
      } else {
        await activateProbeMutation.mutateAsync(probe.id);
        toast.success("Probe resumed");
      }
    } catch {
      toast.error("Failed to toggle probe");
    }
  };

  const handleExecuteNow = async (id: string) => {
    setIsExecuting(true);
    try {
      // Ensure animation runs for at least 200ms
      await Promise.all([
        executeProbeMutation.mutateAsync(id),
        new Promise((resolve) => setTimeout(resolve, 200)),
      ]);
      toast.success("Probe executed");
    } catch {
      toast.error("Failed to execute probe");
    } finally {
      setIsExecuting(false);
    }
  };

  if (probesLoading) {
    return (
      <div className="flex items-center justify-center p-12">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-doubleword-accent-blue"></div>
      </div>
    );
  }

  // No probe exists yet - show create form
  if (!modelProbe && !showCreateForm) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Uptime Monitoring</CardTitle>
          <CardDescription>
            Monitor this model's availability and response times
          </CardDescription>
        </CardHeader>
        <CardContent>
          <div className="text-center py-8">
            <Activity className="h-12 w-12 text-gray-400 mx-auto mb-4" />
            <p className="text-gray-600 mb-4">
              Monitoring is not enabled for this model
            </p>
            <Button onClick={() => setShowCreateForm(true)}>
              <Plus className="mr-2 h-4 w-4" />
              Start Monitoring
            </Button>
          </div>
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-6">
      {/* Existing Probe Display */}
      {modelProbe && !showCreateForm && (
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <div>
                <CardTitle>Uptime Monitoring</CardTitle>
                <CardDescription>
                  Monitor endpoint availability and response times
                </CardDescription>
              </div>
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => handleToggleActive(modelProbe)}
                  disabled={
                    activateProbeMutation.isPending ||
                    deactivateProbeMutation.isPending
                  }
                >
                  {modelProbe.active ? (
                    <>
                      <Pause className="mr-2 h-4 w-4" />
                      Pause
                    </>
                  ) : (
                    <>
                      <Play className="mr-2 h-4 w-4" />
                      Resume
                    </>
                  )}
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => handleExecuteNow(modelProbe.id)}
                  disabled={isExecuting}
                >
                  <RefreshCw
                    className={`mr-2 h-4 w-4 ${isExecuting ? "animate-spin" : ""}`}
                  />
                  Run Now
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => handleDeleteProbe(modelProbe.id)}
                  disabled={deleteProbeMutation.isPending}
                >
                  <StopCircle className="mr-2 h-4 w-4" />
                  Stop Monitoring
                </Button>
              </div>
            </div>
          </CardHeader>
          <CardContent>
            <div className="grid grid-cols-2 gap-4">
              <div>
                <p className="text-sm text-gray-600 mb-1">Status</p>
                <Badge variant={modelProbe.active ? "default" : "secondary"}>
                  {modelProbe.active ? "Active" : "Paused"}
                </Badge>
              </div>
              <div>
                <p className="text-sm text-gray-600 mb-1">Interval</p>
                <div className="group/edit-cell flex items-center gap-1">
                  <p className="text-sm font-medium">
                    {modelProbe.interval_seconds}s
                  </p>
                  <Popover
                    open={editingInterval}
                    onOpenChange={setEditingInterval}
                  >
                    <PopoverTrigger asChild>
                      <Edit2 className="h-3.5 w-3.5 opacity-0 group-hover/edit-cell:opacity-100 transition-opacity cursor-pointer text-gray-600 hover:text-gray-900" />
                    </PopoverTrigger>
                    <PopoverContent className="w-80" align="start">
                      <div className="space-y-2">
                        <h4 className="font-medium text-sm">Edit Interval</h4>
                        <div className="flex gap-2">
                          <div className="flex items-center gap-2 flex-1">
                            <Input
                              type="number"
                              min="10"
                              value={newInterval}
                              onChange={(e) =>
                                setNewInterval(parseInt(e.target.value))
                              }
                              placeholder="Seconds"
                              autoFocus
                              onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                  updateProbeMutation
                                    .mutateAsync({
                                      id: modelProbe.id,
                                      data: { interval_seconds: newInterval },
                                    })
                                    .then(() => {
                                      setEditingInterval(false);
                                      toast.success("Interval updated");
                                    })
                                    .catch(() => {
                                      toast.error("Failed to update interval");
                                    });
                                } else if (e.key === "Escape") {
                                  setEditingInterval(false);
                                  setNewInterval(modelProbe.interval_seconds);
                                }
                              }}
                            />
                            <span className="text-sm text-gray-600">s</span>
                          </div>
                          <Button
                            size="icon"
                            variant="ghost"
                            className="h-8 w-8"
                            onClick={async () => {
                              try {
                                await updateProbeMutation.mutateAsync({
                                  id: modelProbe.id,
                                  data: { interval_seconds: newInterval },
                                });
                                setEditingInterval(false);
                                toast.success("Interval updated");
                              } catch {
                                toast.error("Failed to update interval");
                              }
                            }}
                          >
                            <Check className="h-4 w-4" />
                          </Button>
                          <Button
                            size="icon"
                            variant="ghost"
                            className="h-8 w-8"
                            onClick={() => {
                              setEditingInterval(false);
                              setNewInterval(modelProbe.interval_seconds);
                            }}
                          >
                            <X className="h-4 w-4" />
                          </Button>
                        </div>
                      </div>
                    </PopoverContent>
                  </Popover>
                </div>
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      {/* Create Probe Form */}
      {showCreateForm && (
        <Card>
          <CardHeader>
            <CardTitle>Start Monitoring</CardTitle>
            <CardDescription>
              Configure uptime monitoring for {model.alias || model.model_name}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <div className="space-y-6">
              {/* Ping Type Selection */}
              <div>
                <h3 className="text-sm font-medium mb-3">Step 1: Configure Probe Type</h3>
                <div className="space-y-4">
                  <div>
                    <label className="text-sm font-medium mb-2 block">
                      Ping Type
                    </label>
                    <Select value={pingType} onValueChange={(value: "default" | "custom") => {
                      setPingType(value);
                      // Reset test when changing ping type
                      setTestResult(null);
                      setTestPassed(false);
                    }}>
                      <SelectTrigger>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="default">Default (based on model type)</SelectItem>
                        <SelectItem value="custom">Custom (specify path and body)</SelectItem>
                      </SelectContent>
                    </Select>
                    <p className="text-xs text-gray-500 mt-1">
                      Use default to automatically probe based on model type, or custom to specify your own request
                    </p>
                  </div>

                  {/* Custom ping fields - only show when custom is selected */}
                  {pingType === "custom" && (
                    <>
                      <div>
                        <label className="text-sm font-medium mb-2 block">
                          HTTP Method
                        </label>
                        <Select value={httpMethod} onValueChange={(value) => {
                          setHttpMethod(value);
                          setTestResult(null);
                          setTestPassed(false);
                        }}>
                          <SelectTrigger>
                            <SelectValue />
                          </SelectTrigger>
                          <SelectContent>
                            <SelectItem value="GET">GET</SelectItem>
                            <SelectItem value="POST">POST</SelectItem>
                            <SelectItem value="PUT">PUT</SelectItem>
                            <SelectItem value="PATCH">PATCH</SelectItem>
                            <SelectItem value="DELETE">DELETE</SelectItem>
                          </SelectContent>
                        </Select>
                      </div>

                      <div>
                        <label className="text-sm font-medium mb-2 block">
                          Request Path
                        </label>
                        <Input
                          type="text"
                          placeholder="/v1/chat/completions"
                          value={requestPath}
                          onChange={(e) => {
                            setRequestPath(e.target.value);
                            setTestResult(null);
                            setTestPassed(false);
                          }}
                        />
                        <p className="text-xs text-gray-500 mt-1">
                          Path to append to endpoint URL (e.g., /v1/chat/completions)
                        </p>
                      </div>

                      <div>
                        <label className="text-sm font-medium mb-2 block">
                          Request Body (JSON)
                        </label>
                        <Textarea
                          placeholder='{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello"}]}'
                          value={requestBody}
                          onChange={(e) => {
                            setRequestBody(e.target.value);
                            setTestResult(null);
                            setTestPassed(false);
                          }}
                          rows={4}
                          className="font-mono text-sm"
                        />
                        <p className="text-xs text-gray-500 mt-1">
                          JSON body for the request
                        </p>
                      </div>
                    </>
                  )}
                </div>
              </div>

              {/* Test Section */}
              <div>
                <div className="flex items-center justify-between mb-4">
                  <div>
                    <h3 className="text-sm font-medium mb-1">Step 2: Test Connection</h3>
                    <p className="text-sm text-gray-600">
                      Verify that the endpoint is accessible with these settings
                    </p>
                  </div>
                  <Button
                    onClick={handleTestProbe}
                    disabled={testProbeMutation.isPending}
                  >
                    {testProbeMutation.isPending ? (
                      <>
                        <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                        Testing...
                      </>
                    ) : (
                      <>
                        <RefreshCw className="mr-2 h-4 w-4" />
                        Test Probe
                      </>
                    )}
                  </Button>
                </div>

                {testResult && (
                  <div
                    className={`p-4 rounded-lg border ${
                      testResult.success
                        ? "bg-green-50 border-green-200"
                        : "bg-red-50 border-red-200"
                    }`}
                  >
                    <div className="flex items-start gap-3">
                      {testResult.success ? (
                        <CheckCircle className="h-5 w-5 text-green-600 mt-0.5" />
                      ) : (
                        <XCircle className="h-5 w-5 text-red-600 mt-0.5" />
                      )}
                      <div className="flex-1">
                        <p
                          className={`font-medium ${
                            testResult.success
                              ? "text-green-900"
                              : "text-red-900"
                          }`}
                        >
                          {testResult.success
                            ? "Test Successful"
                            : "Test Failed"}
                        </p>
                        {testResult.success ? (
                          <p className="text-sm text-green-700 mt-1">
                            Response time: {testResult.response_time_ms}ms
                            {testResult.status_code &&
                              ` • Status: ${testResult.status_code}`}
                          </p>
                        ) : (
                          <p className="text-sm text-red-700 mt-1">
                            {testResult.error_message ||
                              "Unknown error occurred"}
                          </p>
                        )}
                      </div>
                    </div>
                  </div>
                )}
              </div>

              {/* Configuration Section */}
              <div>
                <h3 className="text-sm font-medium mb-3">Step 3: Set Interval</h3>
                <div className="space-y-4">
                  <div>
                    <label className="text-sm font-medium mb-2 block">
                      Check Interval (seconds)
                    </label>
                    <Input
                      type="number"
                      min="10"
                      value={intervalSeconds}
                      onChange={(e) => setIntervalSeconds(parseInt(e.target.value))}
                      disabled={!testPassed}
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      How often the probe should check the endpoint
                    </p>
                  </div>
                </div>
              </div>

              {/* Action Buttons */}
              <div className="flex gap-2 justify-end pt-4 border-t">
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => {
                    setShowCreateForm(false);
                    setTestResult(null);
                    setTestPassed(false);
                    setIntervalSeconds(60);
                    setPingType("default");
                    setHttpMethod("POST");
                    setRequestPath("");
                    setRequestBody("");
                  }}
                >
                  Cancel
                </Button>
                <Button
                  onClick={handleCreateProbe}
                  disabled={!testPassed || createProbeMutation.isPending}
                >
                  {createProbeMutation.isPending ? (
                    <>
                      <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                      Starting...
                    </>
                  ) : (
                    "Start Monitoring"
                  )}
                </Button>
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      {/* Uptime Timeline */}
      {modelProbe &&
        probeResults &&
        probeResults.length > 0 &&
        !showCreateForm && (
          <Card>
            <CardHeader>
              <CardTitle>Uptime History</CardTitle>
              <CardDescription>Recent availability checks</CardDescription>
            </CardHeader>
            <CardContent>
              <ProbeTimeline results={probeResults} />
            </CardContent>
          </Card>
        )}
    </div>
  );
};

export default ModelProbes;
