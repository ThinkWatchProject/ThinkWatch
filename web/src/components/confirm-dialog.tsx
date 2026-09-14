import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { useState } from 'react';

interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description: string;
  /** If provided, user must type this value to confirm (like "delete account" flow). */
  requireInput?: string;
  inputPlaceholder?: string;
  variant?: 'default' | 'destructive';
  confirmLabel?: string;
  onConfirm: (inputValue?: string) => void;
  loading?: boolean;
}

export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  requireInput,
  inputPlaceholder,
  variant = 'default',
  confirmLabel,
  onConfirm,
  loading,
}: ConfirmDialogProps) {
  const { t } = useTranslation();
  const [inputValue, setInputValue] = useState('');

  // Clear the typed confirmation when the dialog closes, adjusted during
  // render rather than in an effect: an effect would paint the stale text
  // for one frame on the way out, and React re-runs this render before
  // anything reaches the screen.
  const [wasOpen, setWasOpen] = useState(open);
  if (wasOpen !== open) {
    setWasOpen(open);
    if (!open) setInputValue('');
  }

  const canConfirm = requireInput ? inputValue === requireInput : true;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        {requireInput !== undefined && (
          <Input
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            placeholder={inputPlaceholder}
          />
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t('common.cancel')}
          </Button>
          <Button
            variant={variant === 'destructive' ? 'destructive' : 'default'}
            disabled={!canConfirm || loading}
            onClick={() => onConfirm(inputValue || undefined)}
          >
            {loading ? t('common.loading') : confirmLabel ?? t('common.confirm')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
