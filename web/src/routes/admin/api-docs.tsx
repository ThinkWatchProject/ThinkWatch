import { API_BASE } from '@/lib/api';

export function ApiDocsPage() {
  return (
    <iframe
      src={`${API_BASE}/api/docs`}
      className="h-full w-full border-0"
      title="API Documentation"
      sandbox="allow-same-origin allow-scripts allow-forms allow-popups"
    />
  );
}
