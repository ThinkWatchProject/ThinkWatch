import { useEffect, useRef, useState } from 'react';
import { api } from '@/lib/api';
import type { RouteHistoryResponse, RouteHistoryBucket } from '../models';

interface Props {
  modelId: string;
  routeId: string;
  /// Window length in seconds. Default 1h.
  windowSecs?: number;
  /// Width × height in CSS pixels. Defaults to a small inline format.
  width?: number;
  height?: number;
  /// Fetch interval. Default 30s — the sparkline is informational, not
  /// load-bearing, so we don't poll aggressively.
  refreshSecs?: number;
}

/// 60-bucket latency sparkline rendered as an SVG polyline. Used inline
/// next to the EWMA latency number in the routes table so admins see
/// trend (rising / flat / falling) at a glance, not just a snapshot.
///
/// The endpoint returns 0..N buckets where each bucket is one minute;
/// gaps (no traffic) are NOT padded — we just connect adjacent buckets
/// with the polyline, which is honest about "no data" periods.
export function LatencySparkline({
  modelId,
  routeId,
  windowSecs = 3600,
  width = 60,
  height = 16,
  refreshSecs = 30,
}: Props) {
  const [buckets, setBuckets] = useState<RouteHistoryBucket[] | null>(null);
  const cancelled = useRef(false);

  useEffect(() => {
    cancelled.current = false;
    const fetchOnce = async () => {
      try {
        const res = await api<RouteHistoryResponse>(
          `/api/admin/models/${encodeURIComponent(modelId)}/route-history?route_id=${routeId}&window=${windowSecs}`,
        );
        if (!cancelled.current) setBuckets(res.buckets);
      } catch {
        // Silently degrade — empty sparkline is fine for an
        // informational widget.
      }
    };
    void fetchOnce();
    const id = window.setInterval(() => void fetchOnce(), refreshSecs * 1000);
    return () => {
      cancelled.current = true;
      window.clearInterval(id);
    };
  }, [modelId, routeId, windowSecs, refreshSecs]);

  if (!buckets || buckets.length === 0) {
    return (
      <svg
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        className="opacity-30"
        aria-hidden="true"
      >
        <line
          x1={0}
          y1={height / 2}
          x2={width}
          y2={height / 2}
          stroke="currentColor"
          strokeWidth={1}
          strokeDasharray="2 2"
        />
      </svg>
    );
  }

  const values = buckets.map((b) => b.p50_ms ?? 0);
  const max = Math.max(...values, 1);
  const min = Math.min(...values);
  const span = Math.max(1, max - min);

  const points = buckets.map((b, i) => {
    const x = (i / Math.max(1, buckets.length - 1)) * width;
    // Higher latency = lower y (top of svg is small y). Pad 1px so the
    // line never touches the edge.
    const v = b.p50_ms ?? min;
    const norm = (v - min) / span;
    const y = height - 1 - norm * (height - 2);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className="text-emerald-500"
      aria-label={`Latency trend (last ${Math.round(windowSecs / 60)}m, ${buckets.length} samples)`}
    >
      <polyline
        points={points.join(' ')}
        fill="none"
        stroke="currentColor"
        strokeWidth={1.2}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}
