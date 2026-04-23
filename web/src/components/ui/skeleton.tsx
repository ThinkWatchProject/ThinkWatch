import { cn } from "@/lib/utils"

/**
 * Loading-state placeholder.
 *
 * Use this — never a textual "Loading..." — for any list / table /
 * card whose data isn't ready yet. Match the shape of the resolved
 * UI as closely as possible (row count, column widths, badge
 * outlines) so the user can locate the eventual content with their
 * eyes already on the right region instead of scanning a blank
 * card. Examples to copy:
 *
 *   - `routes/admin/users.tsx`        — table-row skeleton matching
 *                                       the real columns
 *   - `routes/admin/outbox-backlog-dialog.tsx` — single block skeleton
 *                                       when the entire panel is loading
 *   - `routes/api-keys.tsx`           — repeated row skeletons for
 *                                       the list view
 *
 * `className` accepts shape + spacing utilities; the bg + shimmer
 * are baked in.
 */
function Skeleton({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="skeleton"
      className={cn("skeleton-shimmer rounded-md bg-muted", className)}
      {...props}
    />
  )
}

export { Skeleton }
