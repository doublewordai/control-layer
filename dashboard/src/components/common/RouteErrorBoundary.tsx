import { Component, type ReactNode, type ErrorInfo } from "react";
import { useLocation } from "react-router-dom";

/**
 * Error boundary that wraps a single route. When a route throws during render,
 * the sidebar and header remain visible and the user sees a contained error
 * message with a reload option. The boundary automatically resets when the
 * user navigates to a different path, so recovering from a crash does not
 * require a full page reload.
 */
export function RouteErrorBoundary({ children }: { children: ReactNode }) {
  const location = useLocation();
  return (
    <InnerRouteErrorBoundary pathname={location.pathname}>
      {children}
    </InnerRouteErrorBoundary>
  );
}

interface InnerProps {
  children: ReactNode;
  pathname: string;
}

interface InnerState {
  error: Error | null;
  errorPathname: string | null;
}

class InnerRouteErrorBoundary extends Component<InnerProps, InnerState> {
  state: InnerState = { error: null, errorPathname: null };

  static getDerivedStateFromError(error: Error): Partial<InnerState> {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(
      "Route ErrorBoundary caught:",
      error,
      info.componentStack,
    );
    this.setState({ errorPathname: this.props.pathname });
  }

  componentDidUpdate(prevProps: InnerProps) {
    // Reset error state when the user navigates away from the crashing route.
    if (
      this.state.error &&
      this.state.errorPathname !== null &&
      prevProps.pathname !== this.props.pathname &&
      this.props.pathname !== this.state.errorPathname
    ) {
      this.setState({ error: null, errorPathname: null });
    }
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex flex-col items-center justify-center min-h-[60vh] p-8 text-center">
          <h2 className="text-xl font-semibold text-doubleword-neutral-900 mb-2">
            Something went wrong
          </h2>
          <p className="text-sm text-doubleword-neutral-600 mb-6 max-w-md">
            An unexpected error occurred while loading this page. Try
            reloading, or navigate to another page from the sidebar.
          </p>
          <button
            type="button"
            onClick={() => window.location.reload()}
            className="px-4 py-2 text-sm font-medium rounded-md border border-doubleword-neutral-300 hover:bg-doubleword-neutral-100 transition-colors"
          >
            Reload page
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
