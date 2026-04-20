// ============================================================================
// Bulk override dialog
//
// Receives a pre-selected set of user ids from the users list's
// multi-select and lets the operator spec one override to apply
// across all of them in a single round-trip. No user picker inside
// the dialog — that's the users list's job, and duplicating it here
// meant operators had to re-find users they'd already checked.
//
// On submit we fire one POST to
//   /api/admin/limits/bulk/{rules|budgets}
// with the same override meta (expires_at + reason) as the per-user
// form. The backend validates the spec once, then upserts per target
// independently; partial failures surface as per-user outcome rows.
// ============================================================================

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Textarea } from '@/components/ui/textarea';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { apiPost } from '@/lib/api';
import { toast } from 'sonner';

// ----------------------------------------------------------------------------
// Option lists — mirror the per-user form and the backend.
// ----------------------------------------------------------------------------

type Surface = 'ai_gateway' | 'mcp_gateway';
type Metric = 'requests' | 'tokens';
type Period = 'daily' | 'weekly' | 'monthly';

const WINDOW_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 60, labelKey: 'limits.window_60' },
  { secs: 300, labelKey: 'limits.window_300' },
  { secs: 3600, labelKey: 'limits.window_3600' },
  { secs: 18000, labelKey: 'limits.window_18000' },
  { secs: 86400, labelKey: 'limits.window_86400' },
  { secs: 604800, labelKey: 'limits.window_604800' },
];

const PERIOD_OPTIONS: Period[] = ['daily', 'weekly', 'monthly'];

const EXPIRY_PRESETS: { key: string; hours: number | null }[] = [
  { key: '4h', hours: 4 },
  { key: '24h', hours: 24 },
  { key: '7d', hours: 24 * 7 },
  { key: '30d', hours: 24 * 30 },
  { key: 'custom', hours: null },
  { key: 'permanent', hours: null },
];

interface BulkOutcome {
  subject_kind: string;
  subject_id: string;
  row_id?: string;
  error?: string;
}

interface BulkApplyResponse {
  outcomes: BulkOutcome[];
  success_count: number;
  error_count: number;
}

/// `targetUserIds` comes from the users list selection. `userLookup`
/// is an optional id → email map so outcome rows can show something
/// more readable than a UUID fragment; if omitted we fall back to
/// the short id.
interface BulkOverrideDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  targetUserIds: string[];
  userLookup?: Map<string, { email: string; display_name?: string | null }>;
  /// Fired after a successful batch so the caller can refetch lists
  /// etc. Not required.
  onApplied?: () => void;
}

export function BulkOverrideDialog({
  open,
  onOpenChange,
  targetUserIds,
  userLookup,
  onApplied,
}: BulkOverrideDialogProps) {
  const { t } = useTranslation();
  const [kind, setKind] = useState<'rule' | 'cap'>('rule');
  const [surface, setSurface] = useState<Surface>('ai_gateway');
  const [metric, setMetric] = useState<Metric>('requests');
  const [windowSecs, setWindowSecs] = useState<number>(3600);
  const [period, setPeriod] = useState<Period>('monthly');
  const [value, setValue] = useState('');
  const [expiryPreset, setExpiryPreset] = useState<string>('7d');
  const [customExpiry, setCustomExpiry] = useState('');
  const [reason, setReason] = useState('');
  const [busy, setBusy] = useState(false);
  const [outcomes, setOutcomes] = useState<BulkOutcome[] | null>(null);

  // Reset form state each time the dialog opens — stale values from a
  // prior session would be confusing, and the spec is usually
  // different for each cohort anyway.
  useEffect(() => {
    if (!open) return;
    setKind('rule');
    setSurface('ai_gateway');
    setMetric('requests');
    setWindowSecs(3600);
    setPeriod('monthly');
    setValue('');
    setExpiryPreset('7d');
    setCustomExpiry('');
    setReason('');
    setOutcomes(null);
  }, [open]);

  const resolveExpiry = (): string | null | 'invalid' => {
    if (expiryPreset === 'permanent') return null;
    if (expiryPreset === 'custom') {
      if (!customExpiry) return 'invalid';
      const d = new Date(customExpiry);
      if (Number.isNaN(d.getTime())) return 'invalid';
      return d.toISOString();
    }
    const preset = EXPIRY_PRESETS.find((p) => p.key === expiryPreset);
    if (!preset?.hours) return 'invalid';
    return new Date(Date.now() + preset.hours * 3_600_000).toISOString();
  };

  const submit = async () => {
    if (targetUserIds.length === 0) {
      toast.error(t('bulkOverride.pickAtLeastOne'));
      return;
    }
    const n = parseInt(value, 10);
    if (!Number.isFinite(n) || n <= 0) {
      toast.error(t('userLimitOverrides.valueInvalid'));
      return;
    }
    const expiry = resolveExpiry();
    if (expiry === 'invalid') {
      toast.error(t('userLimitOverrides.expiryInvalid'));
      return;
    }
    const targets = targetUserIds.map((id) => ({ kind: 'user', id }));

    setBusy(true);
    setOutcomes(null);
    try {
      const body =
        kind === 'rule'
          ? {
              targets,
              surface,
              metric,
              window_secs: windowSecs,
              max_count: n,
              enabled: true,
              expires_at: expiry,
              reason: reason.trim() || null,
            }
          : {
              targets,
              period,
              limit_tokens: n,
              enabled: true,
              expires_at: expiry,
              reason: reason.trim() || null,
            };
      const path =
        kind === 'rule'
          ? '/api/admin/limits/bulk/rules'
          : '/api/admin/limits/bulk/budgets';
      const res = await apiPost<BulkApplyResponse>(path, body);
      setOutcomes(res.outcomes);
      if (res.error_count === 0) {
        toast.success(t('bulkOverride.allOk', { count: res.success_count }));
        // Auto-close on clean success — operator wants to go back to
        // their workflow, not stare at a wall of ✓s.
        onOpenChange(false);
        onApplied?.();
      } else {
        toast.warning(
          t('bulkOverride.partial', {
            ok: res.success_count,
            fail: res.error_count,
          }),
        );
        onApplied?.();
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setBusy(false);
    }
  };

  // Show up to 8 targets inline as chips so the operator can double-
  // check before hitting apply; past that, collapse to "+ N more"
  // to keep the dialog compact.
  const inlinePreview = targetUserIds.slice(0, 8);
  const overflow = Math.max(0, targetUserIds.length - inlinePreview.length);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>
            {t('bulkOverride.titleForCount', { count: targetUserIds.length })}
          </DialogTitle>
          <DialogDescription>{t('bulkOverride.description')}</DialogDescription>
        </DialogHeader>

        {/* Target summary — confirmation chip row instead of a full
            picker, since the selection already happened on the users
            list. */}
        <div className="rounded-md border bg-muted/30 px-3 py-2">
          <Label className="text-[10px] uppercase tracking-wider text-muted-foreground">
            {t('bulkOverride.targets')}
          </Label>
          <div className="mt-1 flex flex-wrap gap-1">
            {inlinePreview.map((id) => {
              const u = userLookup?.get(id);
              return (
                <Badge key={id} variant="secondary" className="font-mono text-[11px]">
                  {u?.email ?? id.slice(0, 8)}
                </Badge>
              );
            })}
            {overflow > 0 && (
              <Badge variant="outline" className="text-[11px]">
                +{overflow}
              </Badge>
            )}
          </div>
        </div>

        {/* Override spec */}
        <div className="space-y-2">
          <div className="flex items-center gap-3">
            <Label className="text-xs">{t('userLimitOverrides.col.type')}</Label>
            <Select value={kind} onValueChange={(v) => setKind(v as 'rule' | 'cap')}>
              <SelectTrigger className="w-32 text-xs" style={{ height: 28 }}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="rule">{t('userLimitOverrides.type_rule')}</SelectItem>
                <SelectItem value="cap">{t('userLimitOverrides.type_cap')}</SelectItem>
              </SelectContent>
            </Select>
          </div>
          {kind === 'rule' ? (
            <div className="grid gap-2 sm:grid-cols-3">
              <div className="space-y-0.5">
                <Label className="text-[10px] text-muted-foreground">
                  {t('limits.surface')}
                </Label>
                <Select value={surface} onValueChange={(v) => setSurface(v as Surface)}>
                  <SelectTrigger className="text-xs" style={{ height: 28 }}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="ai_gateway">
                      {t('limits.surface_ai_gateway')}
                    </SelectItem>
                    <SelectItem value="mcp_gateway">
                      {t('limits.surface_mcp_gateway')}
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-0.5">
                <Label className="text-[10px] text-muted-foreground">
                  {t('limits.metric')}
                </Label>
                <Select
                  value={metric}
                  onValueChange={(v) => setMetric(v as Metric)}
                  disabled={surface === 'mcp_gateway'}
                >
                  <SelectTrigger className="text-xs" style={{ height: 28 }}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="requests">{t('limits.metric_requests')}</SelectItem>
                    <SelectItem value="tokens">{t('limits.metric_tokens')}</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-0.5">
                <Label className="text-[10px] text-muted-foreground">
                  {t('limits.window')}
                </Label>
                <Select
                  value={String(windowSecs)}
                  onValueChange={(v) => setWindowSecs(Number(v))}
                >
                  <SelectTrigger className="text-xs" style={{ height: 28 }}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {WINDOW_OPTIONS.map((w) => (
                      <SelectItem key={w.secs} value={String(w.secs)}>
                        {t(w.labelKey)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </div>
          ) : (
            <div className="space-y-0.5 max-w-xs">
              <Label className="text-[10px] text-muted-foreground">
                {t('limits.period')}
              </Label>
              <Select value={period} onValueChange={(v) => setPeriod(v as Period)}>
                <SelectTrigger className="text-xs" style={{ height: 28 }}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {PERIOD_OPTIONS.map((p) => (
                    <SelectItem key={p} value={p}>
                      {t(`limits.period_${p}` as const)}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}
          <div className="grid gap-2 sm:grid-cols-2">
            <div className="space-y-0.5">
              <Label className="text-[10px] text-muted-foreground">
                {kind === 'rule' ? t('limits.maxCount') : t('limits.limitTokens')}
              </Label>
              <Input
                type="number"
                min={1}
                value={value}
                onChange={(e) => setValue(e.target.value)}
                placeholder={kind === 'rule' ? '100' : '1000000'}
                className="text-xs"
                style={{ height: 28 }}
              />
            </div>
            <div className="space-y-0.5">
              <Label className="text-[10px] text-muted-foreground">
                {t('userLimitOverrides.col.expires')}
              </Label>
              <Select value={expiryPreset} onValueChange={setExpiryPreset}>
                <SelectTrigger className="text-xs" style={{ height: 28 }}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {EXPIRY_PRESETS.map((p) => (
                    <SelectItem key={p.key} value={p.key}>
                      {t(`userLimitOverrides.expiry_${p.key}` as const)}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
          {expiryPreset === 'custom' && (
            <div className="space-y-0.5 max-w-xs">
              <Label className="text-[10px] text-muted-foreground">
                {t('userLimitOverrides.customExpiryLabel')}
              </Label>
              <Input
                type="datetime-local"
                value={customExpiry}
                onChange={(e) => setCustomExpiry(e.target.value)}
                className="text-xs"
                style={{ height: 28 }}
              />
            </div>
          )}
          <div className="space-y-0.5">
            <Label className="text-[10px] text-muted-foreground">
              {t('userLimitOverrides.col.reason')}
            </Label>
            <Textarea
              value={reason}
              onChange={(e) => setReason(e.target.value)}
              placeholder={t('userLimitOverrides.reasonPlaceholder')}
              className="text-xs"
              rows={2}
            />
          </div>
        </div>

        {/* Partial-failure outcomes stay rendered so the operator can
            see which users missed the override and why. */}
        {outcomes && (
          <div className="space-y-1">
            <Label className="text-[10px] uppercase tracking-wider text-muted-foreground">
              {t('bulkOverride.outcomes')}
            </Label>
            <div className="max-h-40 overflow-y-auto rounded-md border text-xs">
              <table className="w-full">
                <tbody className="divide-y">
                  {outcomes.map((o) => {
                    const u = userLookup?.get(o.subject_id);
                    return (
                      <tr key={o.subject_id}>
                        <td className="px-2 py-1 font-mono text-[11px]">
                          {u?.email ?? o.subject_id.slice(0, 8)}
                        </td>
                        <td className="px-2 py-1">
                          {o.error ? (
                            <span className="text-destructive">{o.error}</span>
                          ) : (
                            <Badge variant="default">{t('common.ok')}</Badge>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            {t('common.close')}
          </Button>
          <Button
            onClick={submit}
            disabled={busy || targetUserIds.length === 0}
          >
            {busy
              ? t('common.loading')
              : t('bulkOverride.submit', { count: targetUserIds.length })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
