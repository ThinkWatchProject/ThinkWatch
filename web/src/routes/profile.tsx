import { useState, useEffect, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { QRCodeSVG } from 'qrcode.react';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import { Lock, LogOut, Trash2, ShieldCheck, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { useNavigate } from '@tanstack/react-router';

export function ProfilePage() {
  const { t } = useTranslation();
  const navigate = useNavigate();

  // --- Password change ---
  const [oldPassword, setOldPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [pwError, setPwError] = useState('');
  const [pwSuccess, setPwSuccess] = useState('');
  const [pwLoading, setPwLoading] = useState(false);

  // --- Dialog states ---
  const [revokeDialogOpen, setRevokeDialogOpen] = useState(false);
  const [revokeLoading, setRevokeLoading] = useState(false);
  const [deleteDialogOpen, setDeleteDialogOpen] = useState(false);
  const [deleteLoading, setDeleteLoading] = useState(false);
  const [actionError, setActionError] = useState('');

  // --- TOTP states ---
  const [totpEnabled, setTotpEnabled] = useState(false);
  const [totpRequired, setTotpRequired] = useState(false);
  const [totpLoading, setTotpLoading] = useState(true);
  const [totpSetup, setTotpSetup] = useState<{ secret: string; otpauth_uri: string; recovery_codes: string[] } | null>(null);
  const [totpVerifyCode, setTotpVerifyCode] = useState('');
  const [totpVerifyError, setTotpVerifyError] = useState('');
  const [totpVerifyLoading, setTotpVerifyLoading] = useState(false);
  const [totpDisablePassword, setTotpDisablePassword] = useState('');
  const [totpDisableError, setTotpDisableError] = useState('');
  const [disableDialogOpen, setDisableDialogOpen] = useState(false);

  useEffect(() => {
    api<{ enabled: boolean; required: boolean }>('/api/auth/totp/status')
      .then((s) => {
        setTotpEnabled(s.enabled);
        setTotpRequired(s.required);
      })
      .catch((err) => {
        console.error('TOTP status fetch failed:', err);
      })
      .finally(() => setTotpLoading(false));
  }, []);

  const handleTotpSetup = async () => {
    setTotpVerifyError('');
    try {
      const res = await apiPost<{ secret: string; otpauth_uri: string; recovery_codes: string[] }>('/api/auth/totp/setup', {});
      setTotpSetup(res);
    } catch (err) {
      setTotpVerifyError(err instanceof Error ? err.message : 'Failed');
    }
  };

  const handleTotpVerifySetup = async (e: FormEvent) => {
    e.preventDefault();
    setTotpVerifyLoading(true);
    setTotpVerifyError('');
    try {
      await apiPost('/api/auth/totp/verify-setup', { code: totpVerifyCode });
      setTotpEnabled(true);
      setTotpSetup(null);
      setTotpVerifyCode('');
    } catch (err) {
      setTotpVerifyError(err instanceof Error ? err.message : 'Failed');
    } finally {
      setTotpVerifyLoading(false);
    }
  };

  const handleTotpDisable = async () => {
    setTotpDisableError('');
    try {
      await apiPost('/api/auth/totp/disable', { old_password: totpDisablePassword, new_password: '' });
      setTotpEnabled(false);
      setDisableDialogOpen(false);
      setTotpDisablePassword('');
    } catch (err) {
      setTotpDisableError(err instanceof Error ? err.message : 'Failed');
    }
  };

  const clearTokensAndRedirect = async () => {
    localStorage.removeItem('access_token');
    localStorage.removeItem('refresh_token');
    const { clearSigningKey } = await import('@/lib/crypto-store');
    await clearSigningKey();
    navigate({ to: '/' });
  };

  const handleChangePassword = async (e: FormEvent) => {
    e.preventDefault();
    setPwError('');
    setPwSuccess('');

    if (newPassword !== confirmPassword) {
      setPwError(t('auth.passwordMismatch'));
      return;
    }
    if (newPassword.length < 8) {
      setPwError(t('auth.passwordTooShort'));
      return;
    }
    if (!/[A-Z]/.test(newPassword) || !/[a-z]/.test(newPassword) || !/\d/.test(newPassword)) {
      setPwError(t('auth.passwordComplexity'));
      return;
    }

    setPwLoading(true);
    try {
      await apiPost('/api/auth/password', {
        old_password: oldPassword,
        new_password: newPassword,
      });
      setPwSuccess(t('auth.passwordChanged'));
      setOldPassword('');
      setNewPassword('');
      setConfirmPassword('');
      // Force logout after 2 seconds
      setTimeout(clearTokensAndRedirect, 2000);
    } catch (err) {
      setPwError(err instanceof Error ? err.message : 'Failed to change password');
    } finally {
      setPwLoading(false);
    }
  };

  // --- Revoke all sessions ---
  const handleRevokeSessions = async () => {
    setRevokeLoading(true);
    setActionError('');
    try {
      await apiPost('/api/auth/revoke-sessions', {});
      setRevokeDialogOpen(false);
      clearTokensAndRedirect();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Failed');
    } finally {
      setRevokeLoading(false);
    }
  };

  // --- Delete account ---
  const handleDeleteAccount = async () => {
    setDeleteLoading(true);
    setActionError('');
    try {
      await apiDelete('/api/auth/account');
      setDeleteDialogOpen(false);
      clearTokensAndRedirect();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Failed');
    } finally {
      setDeleteLoading(false);
    }
  };

  return (
    <div className="space-y-6 max-w-2xl">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('auth.profile')}</h1>
      </div>

      {/* Password Change */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Lock className="h-4 w-4" />
            {t('auth.changePassword')}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleChangePassword} className="space-y-4">
            {pwError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{pwError}</AlertDescription>
              </Alert>
            )}
            {pwSuccess && (
              <div className="rounded-md bg-green-500/10 p-3 text-sm text-green-700">{pwSuccess}</div>
            )}
            <div className="space-y-2">
              <Label htmlFor="old-pw">{t('auth.oldPassword')}</Label>
              <Input id="old-pw" type="password" value={oldPassword} onChange={(e) => setOldPassword(e.target.value)} required />
            </div>
            <div className="space-y-2">
              <Label htmlFor="new-pw">{t('auth.newPassword')}</Label>
              <Input id="new-pw" type="password" value={newPassword} onChange={(e) => setNewPassword(e.target.value)} required />
            </div>
            <div className="space-y-2">
              <Label htmlFor="confirm-pw">{t('auth.confirmPassword')}</Label>
              <Input id="confirm-pw" type="password" value={confirmPassword} onChange={(e) => setConfirmPassword(e.target.value)} required />
            </div>
            <Button type="submit" disabled={pwLoading}>
              {pwLoading ? t('common.loading') : t('auth.changePassword')}
            </Button>
          </form>
        </CardContent>
      </Card>

      {/* TOTP Two-Factor Authentication */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <ShieldCheck className="h-4 w-4" />
            {t('auth.totp')}
          </CardTitle>
          <CardDescription>
            {totpRequired && !totpEnabled ? t('auth.totpRequiredNotice') : t('auth.totpDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {totpLoading ? (
            <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
          ) : totpEnabled ? (
            <div className="space-y-3">
              <p className="text-sm text-green-600">{t('auth.totpEnabledStatus')}</p>
              <Button variant="outline" onClick={() => setDisableDialogOpen(true)}>
                {t('auth.totpDisable')}
              </Button>
              {/* Disable dialog */}
              {disableDialogOpen && (
                <div className="space-y-3 rounded-md border p-4">
                  <p className="text-sm">{t('auth.totpDisableConfirm')}</p>
                  {totpDisableError && (
                    <Alert variant="destructive">
                      <AlertCircle className="h-4 w-4" />
                      <AlertDescription>{totpDisableError}</AlertDescription>
                    </Alert>
                  )}
                  <Input
                    type="password"
                    placeholder={t('auth.password')}
                    value={totpDisablePassword}
                    onChange={(e) => setTotpDisablePassword(e.target.value)}
                  />
                  <div className="flex gap-2">
                    <Button variant="destructive" onClick={handleTotpDisable} disabled={!totpDisablePassword}>
                      {t('auth.totpDisable')}
                    </Button>
                    <Button variant="outline" onClick={() => { setDisableDialogOpen(false); setTotpDisablePassword(''); setTotpDisableError(''); }}>
                      {t('common.cancel')}
                    </Button>
                  </div>
                </div>
              )}
            </div>
          ) : totpSetup ? (
            <div className="space-y-4">
              <div className="space-y-3">
                <p className="text-sm font-medium">{t('auth.totpScanQr')}</p>
                <div className="flex justify-center rounded-lg bg-white p-4 w-fit mx-auto">
                  <QRCodeSVG value={totpSetup.otpauth_uri} size={200} level="M" />
                </div>
                <Collapsible className="text-xs">
                  <CollapsibleTrigger className="cursor-pointer text-muted-foreground hover:text-foreground">
                    {t('auth.totpManualEntry', 'Manual entry')}
                  </CollapsibleTrigger>
                  <CollapsibleContent>
                    <code className="mt-1 block rounded bg-muted p-2 break-all">
                      {totpSetup.otpauth_uri}
                    </code>
                  </CollapsibleContent>
                </Collapsible>
              </div>
              <div className="space-y-2">
                <p className="text-sm font-medium">{t('auth.totpRecoveryCodes')}</p>
                <div className="grid grid-cols-2 gap-1 rounded bg-muted p-3">
                  {totpSetup.recovery_codes.map((code) => (
                    <code key={code} className="text-xs font-mono">{code}</code>
                  ))}
                </div>
                <p className="text-xs text-muted-foreground">{t('auth.totpRecoveryWarning')}</p>
              </div>
              <form onSubmit={handleTotpVerifySetup} className="space-y-3">
                {totpVerifyError && (
                  <Alert variant="destructive">
                    <AlertCircle className="h-4 w-4" />
                    <AlertDescription>{totpVerifyError}</AlertDescription>
                  </Alert>
                )}
                <div className="space-y-1">
                  <Label>{t('auth.totpCode')}</Label>
                  <Input
                    type="text"
                    inputMode="numeric"
                    pattern="[0-9]{6}"
                    maxLength={6}
                    placeholder="000000"
                    value={totpVerifyCode}
                    onChange={(e) => setTotpVerifyCode(e.target.value.replace(/[^0-9]/g, ''))}
                    required
                  />
                </div>
                <Button type="submit" disabled={totpVerifyLoading}>
                  {totpVerifyLoading ? t('common.loading') : t('auth.totpVerify')}
                </Button>
              </form>
            </div>
          ) : (
            <div className="space-y-3">
              {totpVerifyError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{totpVerifyError}</AlertDescription>
                </Alert>
              )}
              <Button onClick={handleTotpSetup}>{t('auth.totpEnable')}</Button>
            </div>
          )}
        </CardContent>
      </Card>

      {/* Session Management */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <LogOut className="h-4 w-4" />
            {t('auth.revokeSessions')}
          </CardTitle>
          <CardDescription>
            {t('auth.revokeSessionsConfirm')}
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Button variant="outline" onClick={() => setRevokeDialogOpen(true)}>
            <LogOut className="h-4 w-4 mr-2" />
            {t('auth.revokeSessions')}
          </Button>
        </CardContent>
      </Card>

      <Separator />

      {/* Danger Zone: Delete Account */}
      <Card className="border-destructive/50">
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base text-destructive">
            <Trash2 className="h-4 w-4" />
            {t('auth.deleteAccount')}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {actionError && (
            <Alert variant="destructive" className="mb-4">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{actionError}</AlertDescription>
            </Alert>
          )}
          <Button variant="destructive" onClick={() => setDeleteDialogOpen(true)}>
            <Trash2 className="h-4 w-4 mr-2" />
            {t('auth.deleteAccount')}
          </Button>
        </CardContent>
      </Card>

      <ConfirmDialog
        open={revokeDialogOpen}
        onOpenChange={setRevokeDialogOpen}
        title={t('auth.revokeSessions')}
        description={t('auth.revokeSessionsConfirm')}
        onConfirm={handleRevokeSessions}
        loading={revokeLoading}
      />

      <ConfirmDialog
        open={deleteDialogOpen}
        onOpenChange={setDeleteDialogOpen}
        title={t('auth.deleteAccount')}
        description={t('auth.deleteAccountConfirm')}
        variant="destructive"
        confirmLabel={t('auth.deleteAccount')}
        onConfirm={handleDeleteAccount}
        loading={deleteLoading}
      />
    </div>
  );
}
