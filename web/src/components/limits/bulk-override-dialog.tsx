// ============================================================================
// Bulk override dialog
//
// Triggered from the users list toolbar. Operator picks N users +
// spec one override → we fire a single POST
// /api/admin/limits/bulk/{rules|budgets} and render per-user
// outcomes so partial failures are explicit.
//
// Design note: the override meta (expires_at + reason) is validated
// once on the server for the whole batch, so this dialog shares its
// form shape with the single-user inline form — same field set,
// different endpoint.
// ============================================================================

import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Search, X } from 'lucide-react';
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
import { Checkbox } from '@/components/ui/checkbox';
import { Textarea } from '@/components/ui/textarea';
import { ScrollArea } from '@/components/ui/scroll-area';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { api, apiPost } from '@/lib/api';
import { toast } from 'sonner';

// ----------------------------------------------------------------------------
// Static option lists — mirror the per-user form and the backend.
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

interface UserLite {
  id: string;
  email: string;
  display_name?: string | null;
}

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

interface BulkOverrideDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function BulkOverrideDialog({ open, onOpenChange }: BulkOverrideDialogProps) {
  const { t } = useTranslation();
  const [users, setUsers] = useState<UserLite[]>([]);
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<Set<string>>(new Set());
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

  const reset = useCallback(() => {
    setSelected(new Set());
    setSearch('');
    setValue('');
    setExpiryPreset('7d');
    setCustomExpiry('');
    setReason('');
    setOutcomes(null);
  }, []);

  // Lazy load users only when the dialog opens so the users list
  // page stays snappy. Uses the existing admin users endpoint.
  useEffect(() => {
    if (!open) return;
    api<{ items: UserLite[] }>('/api/admin/users?limit=500&offset=0')
      .then((r) => setUsers(r.items))
      .catch((e) =>
        toast.error(e instanceof Error ? e.message : t('common.operationFailed')),
      );
  }, [open, t]);

  useEffect(() => {
    if (!open) reset();
  }, [open, reset]);

  const filteredUsers = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return users;
    return users.filter(
      (u) =>
        u.email.toLowerCase().includes(q) ||
        (u.display_name ?? '').toLowerCase().includes(q),
    );
  }, [users, search]);

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
    if (selected.size === 0) {
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
    const targets = Array.from(selected).map((id) => ({ kind: 'user', id }));

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
        kind === 'rule' ? '/api/admin/limits/bulk/rules' : '/api/admin/limits/bulk/budgets';
      const res = await apiPost<BulkApplyResponse>(path, body);
      setOutcomes(res.outcomes);
      if (res.error_count === 0) {
        toast.success(
          t('bulkOverride.allOk', { count: res.success_count }),
        );
      } else {
        toast.warning(
          t('bulkOverride.partial', {
            ok: res.success_count,
            fail: res.error_count,
          }),
        );
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setBusy(false);
    }
  };

  const toggleUser = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t('bulkOverride.title')}</DialogTitle>
          <DialogDescription>{t('bulkOverride.description')}</DialogDescription>
        </DialogHeader>

        {/* Step 1: pick users */}
        <div className="space-y-2">
          <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
            {t('bulkOverride.step1', { selected: selected.size })}
          </Label>
          <div className="relative">
            <Search className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder={t('bulkOverride.searchPlaceholder')}
              className="pl-7 text-xs"
              style={{ height: 28 }}
            />
          </div>
          <ScrollArea className="h-40 rounded-md border">
            <ul className="divide-y text-xs">
              {filteredUsers.map((u) => (
                <li
                  key={u.id}
                  className="flex items-center gap-2 px-2 py-1.5 hover:bg-muted/30"
                >
                  <Checkbox
                    checked={selected.has(u.id)}
                    onCheckedChange={() => toggleUser(u.id)}
                  />
                  <span className="flex-1 font-mono text-[11px]">{u.email}</span>
                  {u.display_name && (
                    <span className="text-muted-foreground">{u.display_name}</span>
                  )}
                </li>
              ))}
              {filteredUsers.length === 0 && (
                <li className="px-2 py-4 text-center italic text-muted-foreground">
                  {t('common.noResults')}
                </li>
              )}
            </ul>
          </ScrollArea>
          {selected.size > 0 && (
            <div className="flex flex-wrap gap-1">
              {Array.from(selected).slice(0, 10).map((id) => {
                const u = users.find((x) => x.id === id);
                return (
                  <Badge key={id} variant="secondary" className="gap-1 pr-1">
                    {u?.email ?? id.slice(0, 8)}
                    <button
                      type="button"
                      className="ml-1 rounded-sm hover:bg-muted"
                      onClick={() => toggleUser(id)}
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </Badge>
                );
              })}
              {selected.size > 10 && (
                <Badge variant="outline">+ {selected.size - 10}</Badge>
              )}
            </div>
          )}
        </div>

        {/* Step 2: spec override */}
        <div className="space-y-2">
          <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
            {t('bulkOverride.step2')}
          </Label>
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

        {/* Outcomes after submission */}
        {outcomes && (
          <div className="space-y-1">
            <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
              {t('bulkOverride.outcomes')}
            </Label>
            <div className="max-h-40 overflow-y-auto rounded-md border text-xs">
              <table className="w-full">
                <tbody className="divide-y">
                  {outcomes.map((o) => {
                    const u = users.find((x) => x.id === o.subject_id);
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
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t('common.close')}
          </Button>
          <Button onClick={submit} disabled={busy || selected.size === 0}>
            {busy
              ? t('common.loading')
              : t('bulkOverride.submit', { count: selected.size })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
