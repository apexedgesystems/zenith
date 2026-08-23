import { Component, type ReactNode } from "react";

/** Page-level error boundary: a render-time throw (typically schema
 *  drift reaching a formatter) degrades to an error card with the
 *  message and a retry, instead of white-screening the whole console.
 *  Keyed remounting by page path resets it on navigation. */
export default class ErrorBoundary extends Component<
  { children: ReactNode },
  { error: Error | null }
> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (this.state.error) {
      return (
        <div
          className="m-6 rounded-lg p-4"
          style={{
            backgroundColor: "var(--color-surface)",
            border: "1px solid var(--color-crit)",
          }}
        >
          <div
            className="text-xs uppercase tracking-widest font-bold mb-2"
            style={{ color: "var(--color-crit)" }}
          >
            Page error
          </div>
          <div
            className="mono text-xs mb-3"
            style={{ color: "var(--color-text-primary)" }}
          >
            {this.state.error.message}
          </div>
          <button
            className="text-xs px-3 py-1.5 rounded"
            style={{
              backgroundColor: "var(--color-elevated)",
              color: "var(--color-text-primary)",
            }}
            onClick={() => this.setState({ error: null })}
          >
            Retry
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
