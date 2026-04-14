import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Plus, Trash2, AlertCircle, CheckCircle, FlaskConical, Sparkles, ShieldCheck, Eye } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Textarea } from '@/components/ui/textarea';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { api, apiPatch, apiPost } from '@/lib/api';
import { toast } from 'sonner';
import {
  type ContentFilterPreset,
  type ContentFilterRule,
  type ContentFilterTestMatch,
  type PiiPattern,
  type PiiTestResponse,
  type SettingEntry,
  getSettingValue,
  normalizeContentRule,
} from '../admin/settings/types';

export function GatewaySecurityPage() {
  const { t } = useTranslation();

  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [statusMsg, setStatusMsg] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  const [contentFilters, setContentFilters] = useState<ContentFilterRule[]>([]);
  const [piiPatterns, setPiiPatterns] = useState<PiiPattern[]>([]);

  // Unified sandbox state
  const [sandboxOpen, setSandboxOpen] = useState(false);
  const [sandboxText, setSandboxText] = useState('');
  const [sandboxTab, setSandboxTab] = useState('filter');
  const [cfSandboxResult, setCfSandboxResult] = useState<ContentFilterTestMatch[] | null>(null);
  const [cfSandboxLoading, setCfSandboxLoading] = useState(false);
  const [piiSandboxResult, setPiiSandboxResult] = useState<PiiTestResponse | null>(null);
  const [piiSandboxLoading, setPiiSandboxLoading] = useState(false);

  // Content filter presets
  const [cfPresetsOpen, setCfPresetsOpen] = useState(false);
  const [cfPresets, setCfPresets] = useState<ContentFilterPreset[]>([]);

  useEffect(() => {
    api<Record<string, SettingEntry[]>>('/api/admin/settings')
      .then((data) => {
        const cf = getSettingValue(data, 'security', 'content_filter_patterns');
        setContentFilters(Array.isArray(cf) ? cf.map(normalizeContentRule) : []);
        const pp = getSettingValue(data, 'security', 'pii_redactor_patterns');
        setPiiPatterns(Array.isArray(pp) ? pp : []);
      })
      .catch((err) => {
        // Previously silent — left the form blank with no feedback.
        toast.error(err instanceof Error ? err.message : 'Failed to load security settings');
      })
      .finally(() => setLoading(false));
  }, []);

  const handleSave = async () => {
    setSaving(true);
    setStatusMsg(null);
    try {
      // Deduplicate rules before saving
      const dedupCf = contentFilters.filter((r, i, arr) =>
        arr.findIndex(o => o.pattern === r.pattern && o.match_type === r.match_type && o.action === r.action) === i,
      );
      const dedupPii = piiPatterns.filter((p, i, arr) =>
        arr.findIndex(o => o.name === p.name || o.regex === p.regex) === i,
      );
      setContentFilters(dedupCf);
      setPiiPatterns(dedupPii);
      await apiPatch('/api/admin/settings', {
        settings: {
          'security.content_filter_patterns': dedupCf,
          'security.pii_redactor_patterns': dedupPii,
        },
      });
      setStatusMsg({ type: 'success', text: t('settings.saved') });
    } catch (err) {
      setStatusMsg({
        type: 'error',
        text: `${t('settings.saveError')}: ${err instanceof Error ? err.message : 'Unknown error'}`,
      });
    } finally {
      setSaving(false);
    }
  };

  // ---------------------------------------------------------------------------
  // Content filter helpers
  // ---------------------------------------------------------------------------

  const addContentFilter = () =>
    setContentFilters([
      ...contentFilters,
      { name: '', pattern: '', match_type: 'contains', action: 'block' },
    ]);

  const removeContentFilter = (i: number) =>
    setContentFilters(contentFilters.filter((_, idx) => idx !== i));

  const updateContentFilter = (i: number, field: keyof ContentFilterRule, value: string) =>
    setContentFilters(
      contentFilters.map((p, idx) => (idx === i ? { ...p, [field]: value } : p)),
    );

  const openCfPresets = async () => {
    setCfPresetsOpen(true);
    if (cfPresets.length === 0) {
      try {
        const presets = await api<ContentFilterPreset[]>(
          '/api/admin/settings/content-filter/presets',
        );
        setCfPresets(presets);
      } catch {}
    }
  };

  const applyPreset = (preset: ContentFilterPreset) => {
    const existing = new Set(
      contentFilters.map((r) => `${r.pattern}|${r.match_type}|${r.action}`),
    );
    const additions = preset.rules.filter(
      (r) => !existing.has(`${r.pattern}|${r.match_type}|${r.action}`),
    );
    setContentFilters([...contentFilters, ...additions.map(normalizeContentRule)]);
    setCfPresetsOpen(false);
  };

  // ---------------------------------------------------------------------------
  // PII helpers
  // ---------------------------------------------------------------------------

  const addPiiPattern = () =>
    setPiiPatterns([...piiPatterns, { name: '', regex: '', placeholder_prefix: '' }]);

  const removePiiPattern = (i: number) =>
    setPiiPatterns(piiPatterns.filter((_, idx) => idx !== i));

  const updatePiiPattern = (i: number, field: keyof PiiPattern, value: string) =>
    setPiiPatterns(piiPatterns.map((p, idx) => (idx === i ? { ...p, [field]: value } : p)));

  // ---------------------------------------------------------------------------
  // Unified sandbox
  // ---------------------------------------------------------------------------

  const openSandbox = () => {
    setSandboxOpen(true);
    setCfSandboxResult(null);
    setPiiSandboxResult(null);
  };

  const sandboxRunning = cfSandboxLoading || piiSandboxLoading;

  const runSandbox = async () => {
    if (!sandboxText.trim()) return;
    setCfSandboxLoading(true);
    setPiiSandboxLoading(true);
    setCfSandboxResult(null);
    setPiiSandboxResult(null);

    const cfPromise = apiPost<{ matches: ContentFilterTestMatch[] }>(
      '/api/admin/settings/content-filter/test',
      { text: sandboxText, rules: contentFilters },
    ).then(res => setCfSandboxResult(res.matches))
      .catch(() => setCfSandboxResult([]))
      .finally(() => setCfSandboxLoading(false));

    const piiPromise = apiPost<PiiTestResponse>(
      '/api/admin/settings/pii-redactor/test',
      { text: sandboxText, patterns: piiPatterns },
    ).then(res => setPiiSandboxResult(res))
      .catch(() => setPiiSandboxResult({ redacted_text: '', matches: [] }))
      .finally(() => setPiiSandboxLoading(false));

    await Promise.all([cfPromise, piiPromise]);
  };

  // ---------------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------------

  if (loading) {
    return (
      <div className="flex items-center justify-center py-24">
        <p className="text-muted-foreground">{t('common.loading')}</p>
      </div>
    );
  }

  const hasResults = cfSandboxResult !== null || piiSandboxResult !== null;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('nav.contentSecurity')}</h1>
          <p className="text-muted-foreground">{t('contentSecurity.subtitle')}</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={openSandbox}>
            <FlaskConical className="h-4 w-4" />
            {t('settings.sandbox.title')}
          </Button>
          <Button onClick={handleSave} disabled={saving}>
            {saving ? t('common.loading') : t('common.save')}
          </Button>
        </div>
      </div>

      {statusMsg && (
        <Alert variant={statusMsg.type === 'success' ? 'default' : 'destructive'}>
          {statusMsg.type === 'success'
            ? <CheckCircle className="h-4 w-4" />
            : <AlertCircle className="h-4 w-4" />}
          <AlertDescription>{statusMsg.text}</AlertDescription>
        </Alert>
      )}

      {/* Content filter rules */}
      <Card>
        <CardHeader>
          <div className="flex items-start justify-between gap-4">
            <div className="space-y-1">
              <CardTitle className="text-base">{t('settings.contentFilter.title')}</CardTitle>
              <p className="text-xs text-muted-foreground max-w-2xl">
                {t('settings.contentFilter.intro')}
              </p>
            </div>
            <div className="flex gap-2 shrink-0">
              <Button variant="outline" size="sm" onClick={openCfPresets}>
                <Sparkles className="h-4 w-4" />
                {t('settings.contentFilter.loadPresets')}
              </Button>
              <Button variant="outline" size="sm" onClick={addContentFilter}>
                <Plus className="h-4 w-4" />
                {t('settings.addRule')}
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          {contentFilters.length === 0 ? (
            <p className="text-sm text-muted-foreground py-4 text-center">
              {t('settings.contentFilter.empty')}
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-[180px]">{t('settings.contentFilter.ruleName')}</TableHead>
                  <TableHead className="w-[130px]">{t('settings.contentFilter.matchType')}</TableHead>
                  <TableHead>{t('settings.contentFilter.pattern')}</TableHead>
                  <TableHead className="w-[110px]">{t('settings.contentFilter.action')}</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {contentFilters.map((cf, i) => (
                  <TableRow key={i}>
                    <TableCell>
                      <Input
                        value={cf.name}
                        onChange={(e) => updateContentFilter(i, 'name', e.target.value)}
                        placeholder={t('settings.contentFilter.namePlaceholder')}
                        className="h-8"
                      />
                    </TableCell>
                    <TableCell>
                      <Select
                        value={cf.match_type}
                        onValueChange={(v) => v && updateContentFilter(i, 'match_type', v)}
                      >
                        <SelectTrigger className="h-8">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="contains">{t('settings.contentFilter.contains')}</SelectItem>
                          <SelectItem value="regex">{t('settings.contentFilter.regex')}</SelectItem>
                        </SelectContent>
                      </Select>
                    </TableCell>
                    <TableCell>
                      <Input
                        value={cf.pattern}
                        onChange={(e) => updateContentFilter(i, 'pattern', e.target.value)}
                        placeholder={cf.match_type === 'regex' ? '\\d{4}-\\d{4}' : 'jailbreak'}
                        className="h-8 font-mono text-xs"
                      />
                    </TableCell>
                    <TableCell>
                      <Select
                        value={cf.action}
                        onValueChange={(v) => v && updateContentFilter(i, 'action', v)}
                      >
                        <SelectTrigger className="h-8">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="block">
                            <span className="text-destructive">{t('settings.contentFilter.actionBlock')}</span>
                          </SelectItem>
                          <SelectItem value="warn">
                            <span className="text-amber-600 dark:text-amber-400">{t('settings.contentFilter.actionWarn')}</span>
                          </SelectItem>
                          <SelectItem value="log">
                            <span className="text-muted-foreground">{t('settings.contentFilter.actionLog')}</span>
                          </SelectItem>
                        </SelectContent>
                      </Select>
                    </TableCell>
                    <TableCell>
                      <Button variant="ghost" size="icon-sm" onClick={() => removeContentFilter(i)}>
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          <div className="mt-4 grid grid-cols-1 gap-1 text-xs text-muted-foreground sm:grid-cols-3">
            <p><strong className="text-destructive">{t('settings.contentFilter.actionBlock')}:</strong> {t('settings.contentFilter.actionBlockHint')}</p>
            <p><strong className="text-amber-600 dark:text-amber-400">{t('settings.contentFilter.actionWarn')}:</strong> {t('settings.contentFilter.actionWarnHint')}</p>
            <p><strong>{t('settings.contentFilter.actionLog')}:</strong> {t('settings.contentFilter.actionLogHint')}</p>
          </div>
        </CardContent>
      </Card>

      {/* PII redactor patterns */}
      <Card>
        <CardHeader>
          <div className="flex items-start justify-between gap-4">
            <div className="space-y-1">
              <CardTitle className="text-base">{t('settings.pii.title')}</CardTitle>
              <p className="text-xs text-muted-foreground max-w-2xl">
                {t('settings.pii.intro')}
              </p>
            </div>
            <div className="flex gap-2 shrink-0">
              <Popover>
                <PopoverTrigger asChild>
                  <Button variant="outline" size="sm">
                    <Sparkles className="h-4 w-4" />
                    {t('settings.pii.loadPresets')}
                  </Button>
                </PopoverTrigger>
                <PopoverContent className="w-72 p-2" align="start">
                  <div className="space-y-1">
                    <p className="text-xs font-medium text-muted-foreground px-2 py-1">{t('settings.pii.presetHint')}</p>
                    {[
                      { name: 'email', regex: '[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}', placeholder_prefix: 'EMAIL', label: 'Email' },
                      { name: 'phone', regex: '(?:\\+?\\d{1,3}[-.\\s]?)?\\(?\\d{2,4}\\)?[-.\\s]?\\d{3,4}[-.\\s]?\\d{3,4}', placeholder_prefix: 'PHONE', label: 'Phone' },
                      { name: 'uuid', regex: '[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}', placeholder_prefix: 'UUID', label: 'UUID' },
                      { name: 'credit_card', regex: '\\b\\d{4}[- ]?\\d{4}[- ]?\\d{4}[- ]?\\d{4}\\b', placeholder_prefix: 'CARD', label: 'Credit Card' },
                      { name: 'ip_address', regex: '\\b\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\b', placeholder_prefix: 'IP', label: 'IP Address' },
                      { name: 'ssn', regex: '\\b\\d{3}-\\d{2}-\\d{4}\\b', placeholder_prefix: 'SSN', label: 'SSN (US)' },
                      { name: 'id_card', regex: '\\b\\d{17}[\\dXx]\\b', placeholder_prefix: 'IDCARD', label: 'ID Card (CN)' },
                    ].filter(p => !piiPatterns.some(pp => pp.name === p.name)).map(p => (
                      <button
                        key={p.name}
                        className="w-full text-left rounded px-2 py-1.5 text-sm hover:bg-muted flex justify-between items-center"
                        onClick={() => setPiiPatterns([...piiPatterns, { name: p.name, regex: p.regex, placeholder_prefix: p.placeholder_prefix }])}
                      >
                        <span>{p.label}</span>
                        <span className="text-xs text-muted-foreground font-mono">[{p.placeholder_prefix}]</span>
                      </button>
                    ))}
                  </div>
                </PopoverContent>
              </Popover>
              <Button variant="outline" size="sm" onClick={addPiiPattern}>
                <Plus className="h-4 w-4" />
                {t('settings.pii.addPattern')}
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          {piiPatterns.length === 0 ? (
            <p className="text-sm text-muted-foreground py-4 text-center">
              {t('settings.pii.empty')}
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-[180px]">{t('settings.pii.name')}</TableHead>
                  <TableHead>{t('settings.pii.regex')}</TableHead>
                  <TableHead className="w-[160px]">{t('settings.pii.placeholderLabel')}</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {piiPatterns.map((pp, i) => (
                  <TableRow key={i}>
                    <TableCell>
                      <Input
                        value={pp.name}
                        onChange={(e) => updatePiiPattern(i, 'name', e.target.value)}
                        placeholder={t('settings.pii.namePlaceholder')}
                        className="h-8"
                      />
                    </TableCell>
                    <TableCell>
                      <Input
                        value={pp.regex}
                        onChange={(e) => updatePiiPattern(i, 'regex', e.target.value)}
                        placeholder="\\d{3}-\\d{2}-\\d{4}"
                        className="h-8 font-mono text-xs"
                      />
                    </TableCell>
                    <TableCell>
                      <Input
                        value={pp.placeholder_prefix}
                        onChange={(e) => updatePiiPattern(i, 'placeholder_prefix', e.target.value)}
                        placeholder="EMAIL"
                        className="h-8 font-mono text-xs"
                      />
                    </TableCell>
                    <TableCell>
                      <Button variant="ghost" size="icon-sm" onClick={() => removePiiPattern(i)}>
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          <p className="mt-4 text-xs text-muted-foreground">
            {t('settings.pii.behavior')}
          </p>
        </CardContent>
      </Card>

      {/* Unified test sandbox dialog */}
      <Dialog open={sandboxOpen} onOpenChange={setSandboxOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('settings.sandbox.title')}</DialogTitle>
            <DialogDescription>{t('settings.sandbox.desc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            <Textarea
              rows={5}
              value={sandboxText}
              onChange={(e) => setSandboxText(e.target.value)}
              placeholder={t('settings.sandbox.placeholder')}
              className="font-mono text-sm"
            />

            {hasResults && (
              <Tabs value={sandboxTab} onValueChange={setSandboxTab}>
                <TabsList className="w-full">
                  <TabsTrigger value="filter" className="flex-1 gap-1.5">
                    <ShieldCheck className="h-3.5 w-3.5" />
                    {t('settings.contentFilter.title')}
                    {cfSandboxResult && cfSandboxResult.length > 0 && (
                      <Badge variant="destructive" className="text-[10px] ml-1 px-1.5 py-0">
                        {cfSandboxResult.length}
                      </Badge>
                    )}
                  </TabsTrigger>
                  <TabsTrigger value="pii" className="flex-1 gap-1.5">
                    <Eye className="h-3.5 w-3.5" />
                    {t('settings.pii.title')}
                    {piiSandboxResult && piiSandboxResult.matches.length > 0 && (
                      <Badge variant="secondary" className="text-[10px] ml-1 px-1.5 py-0">
                        {piiSandboxResult.matches.length}
                      </Badge>
                    )}
                  </TabsTrigger>
                </TabsList>

                {/* Content filter results */}
                <TabsContent value="filter">
                  {cfSandboxLoading ? (
                    <p className="text-xs text-muted-foreground text-center py-3">{t('common.loading')}</p>
                  ) : cfSandboxResult !== null && (
                    <div className="border rounded-md p-3 max-h-64 overflow-y-auto">
                      {cfSandboxResult.length === 0 ? (
                        <p className="text-sm text-muted-foreground text-center py-2">
                          {t('settings.contentFilter.sandboxNoMatches')}
                        </p>
                      ) : (
                        <div className="space-y-2">
                          <p className="text-xs font-medium text-muted-foreground">
                            {t('settings.contentFilter.sandboxMatchCount', { count: cfSandboxResult.length })}
                          </p>
                          {cfSandboxResult.map((m, i) => (
                            <div key={i} className="text-xs border-l-2 pl-3 py-1" style={{
                              borderColor: m.action === 'block' ? 'hsl(var(--destructive))' : m.action === 'warn' ? 'rgb(217 119 6)' : 'hsl(var(--muted-foreground))',
                            }}>
                              <div className="flex items-center gap-2 flex-wrap">
                                <span className="font-semibold">{m.name || m.pattern}</span>
                                <Badge variant="outline" className="text-[10px]">{m.match_type}</Badge>
                                <Badge
                                  variant={m.action === 'block' ? 'destructive' : m.action === 'warn' ? 'default' : 'secondary'}
                                  className="text-[10px]"
                                >
                                  {m.action}
                                </Badge>
                              </div>
                              <p className="font-mono text-muted-foreground mt-1 break-all">{m.matched_snippet}</p>
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  )}
                </TabsContent>

                {/* PII results */}
                <TabsContent value="pii">
                  {piiSandboxLoading ? (
                    <p className="text-xs text-muted-foreground text-center py-3">{t('common.loading')}</p>
                  ) : piiSandboxResult !== null && (
                    <div className="border rounded-md p-3 max-h-64 overflow-y-auto space-y-3">
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settings.pii.redactedOutput')}</Label>
                        <pre className="font-mono text-xs bg-muted p-2 rounded mt-1 whitespace-pre-wrap break-all">
                          {piiSandboxResult.redacted_text || t('settings.pii.sandboxNoMatches')}
                        </pre>
                      </div>
                      {piiSandboxResult.matches.length > 0 && (
                        <div>
                          <Label className="text-xs text-muted-foreground">
                            {t('settings.pii.sandboxMatchCount', { count: piiSandboxResult.matches.length })}
                          </Label>
                          <div className="space-y-1 mt-1">
                            {piiSandboxResult.matches.map((m, i) => (
                              <div key={i} className="text-xs flex items-center gap-2 flex-wrap">
                                <Badge variant="outline" className="text-[10px]">{m.name}</Badge>
                                <span className="font-mono text-destructive break-all">{m.original}</span>
                                <span className="text-muted-foreground">→</span>
                                <span className="font-mono text-muted-foreground break-all">{m.placeholder}</span>
                              </div>
                            ))}
                          </div>
                        </div>
                      )}
                    </div>
                  )}
                </TabsContent>
              </Tabs>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setSandboxOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button onClick={runSandbox} disabled={sandboxRunning || !sandboxText.trim()}>
              <FlaskConical className="h-4 w-4" />
              {sandboxRunning ? t('common.loading') : t('settings.sandbox.runAll')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Content filter presets dialog */}
      <Dialog open={cfPresetsOpen} onOpenChange={setCfPresetsOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('settings.contentFilter.presetsTitle')}</DialogTitle>
            <DialogDescription>{t('settings.contentFilter.presetsDesc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            {cfPresets.length === 0 ? (
              <p className="text-sm text-muted-foreground text-center py-4">{t('common.loading')}</p>
            ) : (
              cfPresets.map((preset) => (
                <div
                  key={preset.id}
                  className="border rounded-md p-3 hover:bg-muted/50 cursor-pointer"
                  onClick={() => applyPreset(preset)}
                >
                  <div className="flex items-center justify-between mb-1">
                    <h4 className="text-sm font-semibold">{t(`settings.contentFilter.preset.${preset.id}.name`)}</h4>
                    <Badge variant="secondary">{preset.rules.length} rules</Badge>
                  </div>
                  <p className="text-xs text-muted-foreground mb-2">
                    {t(`settings.contentFilter.preset.${preset.id}.description`)}
                  </p>
                  <div className="flex flex-wrap gap-1">
                    {preset.rules.slice(0, 5).map((r, i) => (
                      <Badge key={i} variant="outline" className="text-[10px] font-mono">
                        {r.pattern}
                      </Badge>
                    ))}
                    {preset.rules.length > 5 && (
                      <Badge variant="outline" className="text-[10px]">
                        +{preset.rules.length - 5}
                      </Badge>
                    )}
                  </div>
                </div>
              ))
            )}
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
