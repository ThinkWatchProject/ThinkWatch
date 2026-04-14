import { cn } from '@/lib/utils';

interface SparklineProps {
  data: number[];
  width?: number;
  height?: number;
  /** CSS color string — defaults to emerald for "healthy". */
  stroke?: string;
  /** Optional fill gradient underneath the line. */
  fill?: boolean;
  className?: string;
}

/**
 * Tiny inline SVG trend line. No axes, no labels — just the shape of
 * recent values. Built on raw SVG (no Recharts import) so it's cheap
 * to render in a dense list.
 */
export function Sparkline({
  data,
  width = 60,
  height = 20,
  stroke = 'currentColor',
  fill = true,
  className,
}: SparklineProps) {
  if (data.length < 2) {
    return <div className={cn('inline-block', className)} style={{ width, height }} />;
  }
  const min = Math.min(...data);
  const max = Math.max(...data);
  const range = max - min || 1;

  const points = data.map((v, i) => {
    const x = (i / (data.length - 1)) * width;
    const y = height - ((v - min) / range) * height;
    return [x, y] as const;
  });

  const line = points.map(([x, y], i) => `${i === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`).join(' ');
  const area = fill
    ? `${line} L${width},${height} L0,${height} Z`
    : '';

  // Stable-enough gradient id per render — ok for our use (few sparklines on page).
  const gid = `sl-${Math.random().toString(36).slice(2, 8)}`;

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      width={width}
      height={height}
      className={cn('overflow-visible', className)}
      aria-hidden
    >
      {fill && (
        <defs>
          <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={stroke} stopOpacity={0.35} />
            <stop offset="100%" stopColor={stroke} stopOpacity={0} />
          </linearGradient>
        </defs>
      )}
      {fill && <path d={area} fill={`url(#${gid})`} />}
      <path d={line} fill="none" stroke={stroke} strokeWidth={1.25} strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}
