import { Activity, Settings } from "lucide-react";
import type { ReactNode } from "react";
import { useSettings } from "../../contexts";
import { Button } from "../ui/button";

interface DemoOnlyRouteProps {
  children: ReactNode;
  featureName?: string;
}

export function DemoOnlyRoute({ children, featureName = "This feature" }: DemoOnlyRouteProps) {
  const { isFeatureEnabled } = useSettings();
  
  if (!isFeatureEnabled("demo")) {
    return (
      <div className="flex items-center justify-center min-h-[400px]">
        <div className="text-center max-w-md">
          <Activity className="w-16 h-16 mx-auto mb-4 text-gray-400" />
          <h2 className="text-2xl font-bold text-gray-900 mb-2">
            Demo Mode Required
          </h2>
          <p className="text-gray-600 mb-6">
            {featureName} is currently only available in demo mode. 
            Enable demo mode in Settings to explore this feature.
          </p>
          <Button onClick={() => window.location.href = '/settings'}>
            <Settings className="w-4 h-4 mr-2" />
            Go to Settings
          </Button>
        </div>
      </div>
    );
  }
  
  return <>{children}</>;
}