import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { QRCodeSVG } from 'qrcode.react';
import { AlertCircle, Check, Copy, Download } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiPost, describeApiError } from '@/lib/api';

interface TotpSetup {
  secret: string;
  otpauth_uri: string;
  recovery_codes: string[];
}

/**
 * The TOTP enrollment steps: start, scan the QR code and keep the
 * recovery codes, then confirm with a code from the authenticator app.
 * Used on the profile page and on the screen a session is held at while
 * the platform requires TOTP.
 */
export function TotpEnrollment({ onEnrolled }: { onEnrolled: () => void | Promise<void> }) {
  const { t } = useTranslation();
  const [setup, setSetup] = useState<TotpSetup | null>(null);
  const [code, setCode] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [codesCopied, setCodesCopied] = useState(false);

  const start = async () => {
    setError('');
    setLoading(true);
    try {
      setSetup(await apiPost<TotpSetup>('/api/auth/totp/setup', {}));
    } catch (err) {
      setError(describeApiError(err, t));
    } finally {
      setLoading(false);
    }
  };

  const verify = async (e: FormEvent) => {
    e.preventDefault();
    setLoading(true);
    setError('');
    try {
      await apiPost('/api/auth/totp/verify-setup', { code });
      setSetup(null);
      setCode('');
      await onEnrolled();
    } catch (err) {
      setError(describeApiError(err, t));
    } finally {
      setLoading(false);
    }
  };

  const cancel = () => {
    setSetup(null);
    setCode('');
    setError('');
  };

  const copyRecoveryCodes = async () => {
    if (!setup) return;
    await navigator.clipboard.writeText(setup.recovery_codes.join('\n'));
    setCodesCopied(true);
    setTimeout(() => setCodesCopied(false), 2000);
  };

  const downloadRecoveryCodes = () => {
    if (!setup) return;
    const blob = new Blob([setup.recovery_codes.join('\n') + '\n'], { type: 'text/plain' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'thinkwatch-recovery-codes.txt';
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  };

  const errorAlert = error && (
    <Alert variant="destructive">
      <AlertCircle className="h-4 w-4" />
      <AlertDescription>{error}</AlertDescription>
    </Alert>
  );

  if (!setup) {
    return (
      <div className="space-y-3">
        {errorAlert}
        <Button onClick={start} disabled={loading}>
          {loading ? t('common.loading') : t('auth.totpEnable')}
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="space-y-3">
        <p className="text-sm font-medium">{t('auth.totpScanQr')}</p>
        <div className="flex justify-center rounded-lg bg-white p-4 w-fit mx-auto">
          <QRCodeSVG value={setup.otpauth_uri} size={200} level="M" />
        </div>
        <Collapsible className="text-xs">
          <CollapsibleTrigger className="cursor-pointer text-muted-foreground hover:text-foreground">
            {t('auth.totpManualEntry')}
          </CollapsibleTrigger>
          <CollapsibleContent>
            <code className="mt-1 block rounded bg-muted p-2 break-all font-mono tracking-wider">
              {setup.secret}
            </code>
          </CollapsibleContent>
        </Collapsible>
      </div>
      <div className="space-y-2">
        <p className="text-sm font-medium">{t('auth.totpRecoveryCodes')}</p>
        <div className="grid grid-cols-2 gap-1 rounded bg-muted p-3">
          {setup.recovery_codes.map((c) => (
            <code key={c} className="text-xs font-mono">{c}</code>
          ))}
        </div>
        <div className="flex flex-wrap gap-2">
          <Button type="button" variant="outline" size="sm" onClick={copyRecoveryCodes}>
            {codesCopied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
            {codesCopied ? t('common.copied') : t('auth.totpCopyCodes')}
          </Button>
          <Button type="button" variant="outline" size="sm" onClick={downloadRecoveryCodes}>
            <Download className="h-3.5 w-3.5" />
            {t('auth.totpDownloadCodes')}
          </Button>
        </div>
        <p className="text-xs text-muted-foreground">{t('auth.totpRecoveryWarning')}</p>
      </div>
      <form onSubmit={verify} className="space-y-3">
        {errorAlert}
        <div className="space-y-1">
          <Label htmlFor="totp-enroll-code">{t('auth.totpCode')}</Label>
          <Input
            id="totp-enroll-code"
            type="text"
            inputMode="numeric"
            autoComplete="one-time-code"
            pattern="[0-9]{6}"
            maxLength={6}
            placeholder="000000"
            value={code}
            onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, ''))}
            required
          />
        </div>
        <div className="flex gap-2">
          <Button type="submit" disabled={loading}>
            {loading ? t('common.loading') : t('auth.totpVerify')}
          </Button>
          <Button type="button" variant="outline" onClick={cancel} disabled={loading}>
            {t('common.cancel')}
          </Button>
        </div>
      </form>
    </div>
  );
}
