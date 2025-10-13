import { Database, Server, AlertCircle, UserCheck } from "lucide-react";
import { useSettings } from "../../../../contexts";
import { useAuthorization } from "../../../../utils";
import { useState } from "react";
import { Switch } from "../../../ui/switch";

export function Settings() {
  const { toggleFeature, isFeatureEnabled, setImpersonateUser, settings } =
    useSettings();
  const { hasPermission } = useAuthorization();
  const canAccessSettings = hasPermission("settings");
  const [impersonateEmail, setImpersonateEmail] = useState(
    settings.impersonateUserHeader || "",
  );

  // Check if user with settings access is currently impersonating a user
  const isSettingsUserImpersonating =
    !canAccessSettings && isFeatureEnabled("devUser");

  return (
    <div className="p-8">
      <div className="max-w-7xl mx-auto">
        <h1 className="text-3xl font-bold text-doubleword-neutral-900 mb-2">
          Settings
        </h1>
        <p className="text-doubleword-neutral-600 mb-8">
          Configure your application preferences
        </p>

        {/* Show impersonation controls if user is impersonating */}
        {isSettingsUserImpersonating && (
          <div className="bg-amber-50 border border-amber-200 rounded-lg p-6 mb-6">
            <h2 className="text-lg font-semibold text-amber-900 mb-4">
              Admin Impersonation Active
            </h2>
            <p className="text-sm text-amber-800 mb-4">
              You are currently impersonating a user. Use the controls below to
              change or clear the impersonation.
            </p>
            <div className="space-y-4">
              <div>
                <label
                  htmlFor="impersonate-email"
                  className="block text-sm font-medium text-amber-900 mb-2"
                >
                  User Email to Impersonate
                </label>
                <div className="flex gap-2">
                  <input
                    id="impersonate-email"
                    type="email"
                    value={impersonateEmail}
                    onChange={(e) => setImpersonateEmail(e.target.value)}
                    placeholder="user@example.com"
                    className="flex-1 px-3 py-2 border border-amber-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-amber-500 focus:border-amber-500"
                  />
                  <button
                    onClick={() => setImpersonateUser(impersonateEmail)}
                    className="px-4 py-2 bg-amber-600 text-white text-sm font-medium rounded-md hover:bg-amber-700 focus:outline-none focus:ring-2 focus:ring-amber-500"
                  >
                    Set
                  </button>
                </div>
                <p className="text-xs text-amber-700 mt-2">
                  Current: {settings.impersonateUserHeader || "None set"}
                </p>
              </div>
            </div>
          </div>
        )}

        {/* Show all settings if user has settings permission */}
        {canAccessSettings && (
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
            <div className="bg-white rounded-lg border border-doubleword-neutral-200 p-6">
              <h2 className="text-lg font-semibold text-doubleword-neutral-900 mb-4">
                Data Source
              </h2>

              <div className="space-y-4">
                <div className="flex items-center justify-between">
                  <div className="flex-1">
                    <h3 className="text-sm font-medium text-doubleword-neutral-900">
                      Demo Mode
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

                <div
                  className={`flex items-start gap-3 p-4 rounded-lg ${
                    isFeatureEnabled("demo")
                      ? "bg-blue-50 border border-blue-200"
                      : "bg-gray-50 border border-gray-200"
                  }`}
                >
                  {isFeatureEnabled("demo") ? (
                    <>
                      <Database className="w-5 h-5 text-blue-600 mt-0.5" />
                      <div className="flex-1">
                        <p className="text-sm font-medium text-blue-900">
                          Using Mock Data
                        </p>
                        <p className="text-sm text-blue-700 mt-1">
                          The application is currently displaying sample data.
                          This is perfect for demonstrations and testing without
                          connecting to a backend service.
                        </p>
                      </div>
                    </>
                  ) : (
                    <>
                      <Server className="w-5 h-5 text-gray-600 mt-0.5" />
                      <div className="flex-1">
                        <p className="text-sm font-medium text-gray-900">
                          Connected to Live API
                        </p>
                        <p className="text-sm text-gray-700 mt-1">
                          The application is fetching real data from the backend
                          API. Ensure your backend service is running and
                          accessible.
                        </p>
                      </div>
                    </>
                  )}
                </div>

                <div className="flex items-start gap-3 p-4 bg-amber-50 border border-amber-200 rounded-lg">
                  <AlertCircle className="w-5 h-5 text-amber-600 mt-0.5" />
                  <div className="flex-1">
                    <p className="text-sm text-amber-800">
                      <strong>Note:</strong> Changing this setting will reload
                      the page to apply the new configuration.
                    </p>
                  </div>
                </div>
              </div>
            </div>

            {/* Feature Flags Section */}
            <div className="bg-white rounded-lg border border-doubleword-neutral-200 p-6">
              <h2 className="text-lg font-semibold text-doubleword-neutral-900 mb-4">
                Feature Flags
              </h2>
              <p className="text-sm text-doubleword-neutral-600 mb-6">
                Control which features are enabled. These settings are
                independent of demo mode.
              </p>

              <div className="space-y-4">
                {/* Development User Feature */}
                <div className="flex items-center justify-between">
                  <div className="flex items-center gap-3">
                    <UserCheck className="w-5 h-5 text-doubleword-neutral-500" />
                    <div className="flex-1">
                      <h3 className="text-sm font-medium text-doubleword-neutral-900">
                        Development User Switcher
                      </h3>
                      <p className="text-sm text-doubleword-neutral-600 mt-1">
                        Enable user impersonation for testing different user
                        roles. Adds X-Doubleword-User header to API requests.
                      </p>
                    </div>
                  </div>
                  <Switch
                    checked={isFeatureEnabled("devUser")}
                    onCheckedChange={(checked) =>
                      toggleFeature("devUser", checked)
                    }
                    aria-label="Toggle dev user feature"
                    className="ml-4 flex-shrink-0"
                  />
                </div>

                {/* Impersonate User Input - only show when devUser is enabled */}
                {isFeatureEnabled("devUser") && (
                  <div className="ml-8 p-4 bg-blue-50 border border-blue-200 rounded-lg">
                    <label
                      htmlFor="impersonate-email"
                      className="block text-sm font-medium text-blue-900 mb-2"
                    >
                      User Email to Impersonate
                    </label>
                    <div className="flex gap-2">
                      <input
                        id="impersonate-email"
                        type="email"
                        value={impersonateEmail}
                        onChange={(e) => setImpersonateEmail(e.target.value)}
                        placeholder="user@example.com"
                        className="flex-1 px-3 py-2 border border-blue-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
                      />
                      <button
                        onClick={() => setImpersonateUser(impersonateEmail)}
                        className="px-4 py-2 bg-blue-600 text-white text-sm font-medium rounded-md hover:bg-blue-700 focus:outline-none focus:ring-2 focus:ring-blue-500"
                      >
                        Set
                      </button>
                    </div>
                    <p className="text-xs text-blue-700 mt-2">
                      Current: {settings.impersonateUserHeader || "None set"}
                    </p>
                  </div>
                )}
              </div>
            </div>
          </div>
        )}

        {/* Show message for users without settings access who aren't being impersonated */}
        {!canAccessSettings && !isSettingsUserImpersonating && (
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
