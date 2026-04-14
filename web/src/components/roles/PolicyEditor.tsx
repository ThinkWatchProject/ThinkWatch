import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import CodeMirror, { EditorView } from '@uiw/react-codemirror';
import { json } from '@codemirror/lang-json';
import { Copy } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { useTheme } from '@/hooks/use-theme';
import { POLICY_TEMPLATES, type PolicyDocument } from '@/routes/admin/roles/types';

interface PolicyEditorProps {
  value: string;
  onChange: (v: string) => void;
  error: string;
  onApplyTemplate: (tpl: PolicyDocument) => void;
}

/**
 * Policy JSON editor with template picker. Codemirror + @codemirror/lang-json
 * add ~418 KB to the bundle, so this module is split out into its own file
 * and imported lazily from roles.tsx via React.lazy — users who never open
 * the policy editor don't pay the download cost.
 */
export default function PolicyEditor({
  value,
  onChange,
  error,
  onApplyTemplate,
}: PolicyEditorProps) {
  const { t } = useTranslation();
  // Resolve the `system` theme value at render time so the editor matches
  // whatever class is currently on <html>. MutationObserver picks up the
  // header theme toggle live without us having to thread it through props.
  const { theme } = useTheme();
  const [isDark, setIsDark] = useState(() =>
    typeof document !== 'undefined' && document.documentElement.classList.contains('dark'),
  );
  useEffect(() => {
    const update = () => setIsDark(document.documentElement.classList.contains('dark'));
    update();
    const obs = new MutationObserver(update);
    obs.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
    return () => obs.disconnect();
  }, [theme]);

  return (
    <div className="space-y-3">
      <div>
        <Label className="text-sm font-medium">{t('roles.policyTemplates')}</Label>
        <div className="flex flex-wrap gap-1.5 mt-1.5">
          {Object.entries(POLICY_TEMPLATES).map(([key, tpl]) => (
            <Button
              key={key}
              variant="outline"
              size="sm"
              type="button"
              className="text-xs h-7"
              onClick={() => onApplyTemplate(tpl)}
            >
              <Copy className="mr-1 h-3 w-3" />
              {t(`roles.template_${key}` as const, { defaultValue: key })}
            </Button>
          ))}
        </div>
      </div>
      <div>
        <Label className="text-sm font-medium">{t('roles.policyDocument')}</Label>
        <p className="text-xs text-muted-foreground mb-1.5">{t('roles.policyDocumentDesc')}</p>
        <div className="overflow-hidden rounded-md border">
          <CodeMirror
            value={value}
            onChange={onChange}
            theme={isDark ? 'dark' : 'light'}
            extensions={[json(), EditorView.lineWrapping]}
            placeholder={JSON.stringify(POLICY_TEMPLATES.developer, null, 2)}
            basicSetup={{
              lineNumbers: true,
              foldGutter: true,
              highlightActiveLine: true,
              bracketMatching: true,
              closeBrackets: true,
              autocompletion: false,
              indentOnInput: true,
            }}
            height="320px"
            className="text-xs"
          />
        </div>
        {error && <p className="text-xs text-destructive mt-1">{error}</p>}
      </div>
    </div>
  );
}
