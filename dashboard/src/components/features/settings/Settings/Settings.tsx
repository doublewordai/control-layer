import { Database, Server, AlertCircle, Check, DollarSign } from "lucide-react";
import { useSettings } from "../../../../contexts";
import { useAuthorization } from "../../../../utils";
import { useState } from "react";
import { Switch } from "../../../ui/switch";
import { Button } from "../../../ui/button";

export function Settings() {
  const { toggleFeature, isFeatureEnabled, updateDemoConfig, settings } =
    useSettings();
  const { hasPermission } = useAuthorization();
  const canAccessSettings = hasPermission("settings");
  const [demoResponse, setDemoResponse] = useState(
    settings.demoConfig?.customResponse || "",
  );
  const [useCustomResponse, setUseCustomResponse] = useState(
    !!settings.demoConfig?.customResponse,
  );
  const [savedResponse, setSavedResponse] = useState(
    settings.demoConfig?.customResponse || "",
  );

  const hasUnsavedChanges = demoResponse !== savedResponse;

  const handleSave = () => {
    updateDemoConfig({ customResponse: useCustomResponse ? demoResponse : "" });
    setSavedResponse(demoResponse);
  };

  const handleResponseChange = (value: string) => {
    setDemoResponse(value);
  };

  return (
    <div className="p-8">
      <div className="max-w-5xl mx-auto">
        <h1 className="text-3xl font-bold text-doubleword-neutral-900 mb-2">
          Settings
        </h1>
        <p className="text-doubleword-neutral-600 mb-8">
          Configure your application preferences
        </p>

        {canAccessSettings ? (
          <div className="bg-white rounded-lg border border-doubleword-neutral-200">
            {/* Demo Mode Section */}
            <div className="p-6">
              <div className="flex items-center gap-2 mb-4">
                {isFeatureEnabled("demo") ? (
                  <Database className="w-5 h-5 text-blue-600" />
                ) : (
                  <Server className="w-5 h-5 text-gray-600" />
                )}
                <h2 className="text-lg font-semibold text-doubleword-neutral-900">
                  Demo Mode
                </h2>
              </div>

              <div className="space-y-6">
                <div className="flex items-center justify-between">
                  <div className="flex-1">
                    <h3 className="text-sm font-medium text-doubleword-neutral-900">
                      Enable Demo Mode
                    </h3>
                    <p className="text-sm text-doubleword-neutral-600 mt-1">
                      Use mock data for demonstration purposes. Toggle off to
                      connect to the live API.
                    </p>
                  </div>
                  <Switch
                    checked={isFeatureEnabled("demo")}
                    onCheckedChange={(checked) =>
                      toggleFeature("demo", checked)
                    }
                    aria-label="Toggle demo mode"
                  />
                </div>

                <div className="flex items-start gap-3 p-4 bg-amber-50 border border-amber-200 rounded-lg">
                  <AlertCircle className="w-5 h-5 text-amber-600 mt-0.5 flex-shrink-0" />
                  <div className="flex-1">
                    <p className="text-sm text-amber-800">
                      <strong>Note:</strong> Changing this setting will reload
                      the page to apply the new configuration.
                    </p>
                  </div>
                </div>

                {/* Custom Response Configuration - only show when demo mode is enabled */}
                {isFeatureEnabled("demo") && (
                  <div className="pt-6 mt-6 border-t border-doubleword-neutral-200">
                    <div className="flex items-center justify-between mb-4">
                      <div className="flex-1">
                        <h3 className="text-sm font-medium text-doubleword-neutral-900">
                          Custom Response Template
                        </h3>
                        <p className="text-sm text-doubleword-neutral-600 mt-1">
                          Override the default playground response with a custom
                          template.
                        </p>
                      </div>
                      <Switch
                        checked={useCustomResponse}
                        onCheckedChange={setUseCustomResponse}
                        aria-label="Toggle custom response template"
                      />
                    </div>

                    {useCustomResponse && (
                      <div className="space-y-3 mt-4">
                        <textarea
                          id="demo-response"
                          value={demoResponse}
                          onChange={(e) => handleResponseChange(e.target.value)}
                          placeholder="Enter your custom demo response... Use {userMessage} to include the user input."
                          className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-blue-500 font-mono"
                          rows={4}
                        />
                        <p className="text-xs text-doubleword-neutral-600">
                          Use{" "}
                          <code className="px-1 py-0.5 bg-gray-100 rounded text-xs">
                            {"{userMessage}"}
                          </code>{" "}
                          as a placeholder to include the user's message in the
                          response.
                        </p>

                        <div className="flex justify-end">
                          <Button
                            onClick={handleSave}
                            variant="default"
                            size="sm"
                            disabled={!hasUnsavedChanges}
                          >
                            {hasUnsavedChanges ? (
                              "Save Template"
                            ) : (
                              <>
                                <Check className="w-4 h-4" />
                                Saved
                              </>
                            )}
                          </Button>
                        </div>
                      </div>
                    )}
                  </div>
                )}
              </div>
            </div>

            {/* Billing Features Section */}
            <div className="p-6 border-t border-doubleword-neutral-200">
              <div className="flex items-center gap-2 mb-4">
                <DollarSign className="w-5 h-5 text-green-600" />
                <h2 className="text-lg font-semibold text-doubleword-neutral-900">
                  Billing Features
                </h2>
              </div>

              <div className="space-y-6">
                <div className="flex items-center justify-between">
                  <div className="flex-1">
                    <h3 className="text-sm font-medium text-doubleword-neutral-900">
                      Enable Cost Management
                    </h3>
                    <p className="text-sm text-doubleword-neutral-600 mt-1">
                      Show billing and cost management features including credit balance tracking and transaction history.
                    </p>
                    {!isFeatureEnabled("demo") && (
                      <p className="text-sm text-amber-600 mt-2">
                        Requires demo mode to be enabled
                      </p>
                    )}
                  </div>
                  <Switch
                    checked={isFeatureEnabled("use_billing")}
                    onCheckedChange={(checked) =>
                      toggleFeature("use_billing", checked)
                    }
                    disabled={!isFeatureEnabled("demo")}
                    aria-label="Toggle billing features"
                  />
                </div>
              </div>
            </div>
          </div>
        ) : (
          <div className="bg-gray-50 border border-gray-200 rounded-lg p-6">
            <h2 className="text-lg font-semibold text-gray-900 mb-2">
              Settings Access Restricted
            </h2>
            <p className="text-sm text-gray-600">
              Settings are only available to users with the appropriate
              permissions. Contact your admin if you need access to
              configuration options.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}
