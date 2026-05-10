import { Component, type ErrorInfo, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';

interface Props {
  children: ReactNode;
  fallback?: ReactNode;
}

interface State {
  error: Error | null;
}

// Throttle the POST so a render-loop crash doesn't DoS the ingest
// endpoint with thousands of identical reports per second.
let lastReportAt = 0;
const REPORT_THROTTLE_MS = 5_000;

async function reportToServer(error: Error, info: ErrorInfo): Promise<void> {
  const now = Date.now();
  if (now - lastReportAt < REPORT_THROTTLE_MS) return;
  lastReportAt = now;
  try {
    // Fire-and-forget; never let the report itself bubble back into the
    // boundary's render path. `keepalive` lets the request survive the
    // tab being closed when the crash also navigates away.
    await fetch('/api/client-errors', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      keepalive: true,
      body: JSON.stringify({
        message: error.message,
        stack: error.stack ?? null,
        component_stack: info.componentStack ?? null,
        url: window.location.href,
        user_agent: navigator.userAgent,
        ts: new Date().toISOString(),
      }),
    });
  } catch {
    // The endpoint may not exist (older server) or the network may
    // be down. Either way the user-facing fallback already rendered;
    // a silent swallow is correct.
  }
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('ErrorBoundary caught:', error, info);
    void reportToServer(error, info);
  }

  render() {
    if (this.state.error) {
      if (this.props.fallback) return this.props.fallback;
      return (
        <DefaultFallback
          error={this.state.error}
          onRetry={() => this.setState({ error: null })}
        />
      );
    }
    return this.props.children;
  }
}

/** The default fallback. Lifted to its own functional component so we
 *  can use `useTranslation` — the boundary itself has to stay a class
 *  component because hook-based error boundaries don't exist yet. */
function DefaultFallback({ error, onRetry }: { error: Error; onRetry: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="p-6 max-w-xl mx-auto mt-12">
      <Alert variant="destructive">
        <AlertCircle className="h-4 w-4" />
        <AlertDescription className="flex items-center justify-between">
          <span>{error.message}</span>
          <Button variant="outline" size="sm" onClick={onRetry}>
            {t('common.retry')}
          </Button>
        </AlertDescription>
      </Alert>
    </div>
  );
}
