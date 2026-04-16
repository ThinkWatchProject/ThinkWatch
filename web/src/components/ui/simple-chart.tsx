interface BarChartProps {
  data: { label: string; value: number }[];
  height?: number;
  color?: string;
  formatValue?: (v: number) => string;
}

export function SimpleBarChart({ data, height = 192, color = 'hsl(var(--primary))', formatValue }: BarChartProps) {
  if (data.length === 0) return null;

  const max = Math.max(...data.map((d) => d.value), 1);
  const barWidth = Math.max(4, Math.min(32, Math.floor(600 / data.length) - 4));
  const chartWidth = data.length * (barWidth + 4);
  const chartH = height - 32; // leave room for labels

  return (
    <div className="w-full overflow-x-auto">
      <svg width={Math.max(chartWidth, 200)} height={height} className="mx-auto" role="img">
        {data.map((d, i) => {
          const barH = (d.value / max) * chartH;
          const x = i * (barWidth + 4) + 2;
          const y = chartH - barH;
          return (
            <g key={i}>
              <title>{`${d.label}: ${formatValue ? formatValue(d.value) : d.value.toLocaleString()}`}</title>
              <rect
                x={x}
                y={y}
                width={barWidth}
                height={Math.max(barH, 1)}
                rx={2}
                fill={color}
                opacity={0.85}
              />
              {/* Show label for every Nth bar to avoid overlap */}
              {(data.length <= 15 || i % Math.ceil(data.length / 15) === 0) && (
                <text
                  x={x + barWidth / 2}
                  y={chartH + 14}
                  textAnchor="middle"
                  className="fill-muted-foreground"
                  fontSize={10}
                >
                  {d.label}
                </text>
              )}
            </g>
          );
        })}
        {/* Y-axis baseline */}
        <line x1={0} y1={chartH} x2={chartWidth} y2={chartH} stroke="currentColor" strokeOpacity={0.1} />
      </svg>
    </div>
  );
}

