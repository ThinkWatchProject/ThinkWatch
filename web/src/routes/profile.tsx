import { useState, useEffect, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import { Lock, LogOut, Trash2, ShieldCheck, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { useNavigate } from '@tanstack/react-router';
import { TotpEnrollment } from '@/components/auth/totp-enrollment';
import { useAuth } from '@/hooks/use-auth';

export function ProfilePage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { logout } = useAuth();

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

  const handleTotpDisable = async () => {
    setTotpDisableError('');
    try {
      await apiPost('/api/auth/totp/disable', { old_password: totpDisablePassword });
      setTotpEnabled(false);
      setDisableDialogOpen(false);
      setTotpDisablePassword('');
    } catch (err) {
      setTotpDisableError(err instanceof Error ? err.message : t('common.error'));
    }
  };

  // Real logout — POSTs /api/auth/logout (clears HttpOnly cookies +
  // invalidates refresh tokens server-side), wipes local signing key,
  // clears the cached permission set, and broadcasts to sibling tabs.
  // The previous helper here only deleted long-empty `localStorage`
  // tokens (the project moved to HttpOnly cookies long ago) — leaving
  // the session valid after password change / session revoke / account
  // delete.
  const logoutAndRedirect = async () => {
    await logout();
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
      // Server invalidates the OLD session (pw_epoch bump) AND
      // immediately re-issues fresh cookies for THIS caller so we
      // stay logged in. No forced logout — the user can continue
      // using the app. Other tabs / devices still get evicted via
      // the pw_epoch check on their next request.
      setPwSuccess(t('auth.passwordChanged'));
      setOldPassword('');
      setNewPassword('');
      setConfirmPassword('');
    } catch (err) {
      setPwError(err instanceof Error ? err.message : t('common.error'));
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
      logoutAndRedirect();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : t('common.error'));
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
      logoutAndRedirect();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : t('common.error'));
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
              <p className="text-sm text-green-600">
                {totpRequired ? t('auth.totpRequiredEnabledStatus') : t('auth.totpEnabledStatus')}
              </p>
              {!totpRequired && (
                <Button variant="outline" onClick={() => setDisableDialogOpen(true)}>
                  {t('auth.totpDisable')}
                </Button>
              )}
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
          ) : (
            <TotpEnrollment onEnrolled={() => setTotpEnabled(true)} />
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
