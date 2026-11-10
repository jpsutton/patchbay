import { Component, type ReactNode } from "react";

/**
 * Keeps a render error in one part of the UI from unmounting the whole app (React drops the tree
 * when nothing catches). `fallback` gets the error; `resetKey` clears it when it changes, so
 * reopening the part tries again.
 */
export class ErrorBoundary extends Component<
  { fallback: (error: Error) => ReactNode; resetKey?: unknown; children: ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(error: unknown) {
    console.error("render failed", error);
  }

  componentDidUpdate(prev: { resetKey?: unknown }) {
    if (this.state.error && prev.resetKey !== this.props.resetKey) this.setState({ error: null });
  }

  render() {
    return this.state.error ? this.props.fallback(this.state.error) : this.props.children;
  }
}
