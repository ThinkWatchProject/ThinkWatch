import { useEffect, useId, useRef, useState, type KeyboardEvent } from 'react';
import { ChevronDown, Loader2 } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { Popover, PopoverAnchor, PopoverContent } from '@/components/ui/popover';
import { cn } from '@/lib/utils';

/**
 * A text field that suggests values from a list but takes whatever is
 * typed. The list is a shortcut, not a limit: a value it doesn't name is
 * typed in as it is.
 *
 * The list opens on a click, on ↓ or on typing — not on focus, so a dialog
 * that focuses the field on open doesn't open it too. Typing filters it;
 * ↑/↓ and Enter pick from it without submitting the form.
 */
export function ComboboxInput({
  id,
  value,
  onChange,
  options,
  loading = false,
  loadingText,
  placeholder,
  optionClassName,
}: {
  id?: string;
  value: string;
  onChange: (value: string) => void;
  options: string[];
  /** The options are still coming: the list says so rather than stay shut. */
  loading?: boolean;
  loadingText?: string;
  placeholder?: string;
  optionClassName?: string;
}) {
  const listId = useId();
  const anchorRef = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  // What was typed since the list opened. Opened to browse, it is empty
  // and the whole list shows.
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(-1);

  const q = query.trim().toLowerCase();
  const shown = q ? options.filter((o) => o.toLowerCase().includes(q)) : options;
  const listed = open && (loading || shown.length > 0);
  const optionId = (i: number) => `${listId}-${i}`;

  // Keep the option ↑/↓ reached in view
  useEffect(() => {
    if (listed && active >= 0) {
      document.getElementById(`${listId}-${active}`)?.scrollIntoView?.({ block: 'nearest' });
    }
  }, [listed, active, listId]);

  const openList = (typed: string, highlight = -1) => {
    setQuery(typed);
    setActive(highlight);
    setOpen(true);
  };

  const pick = (option: string) => {
    onChange(option);
    setOpen(false);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        if (!open) openList('', 0);
        else setActive((i) => Math.min(i + 1, shown.length - 1));
        break;
      case 'ArrowUp':
        e.preventDefault();
        setActive((i) => Math.max(i - 1, 0));
        break;
      case 'Enter':
        // Picking from the list, not submitting the form
        if (listed && active >= 0 && active < shown.length) {
          e.preventDefault();
          pick(shown[active]);
        }
        break;
      case 'Tab':
        setOpen(false);
        break;
    }
  };

  return (
    <Popover open={listed} onOpenChange={setOpen}>
      <PopoverAnchor asChild>
        <div ref={anchorRef} className="relative">
          <Input
            id={id}
            role="combobox"
            aria-expanded={listed}
            aria-controls={listId}
            aria-autocomplete="list"
            aria-activedescendant={listed && active >= 0 ? optionId(active) : undefined}
            autoComplete="off"
            value={value}
            placeholder={placeholder}
            className={options.length > 0 || loading ? 'pr-8' : undefined}
            onChange={(e) => {
              onChange(e.target.value);
              openList(e.target.value);
            }}
            onClick={() => {
              if (!open) openList('');
            }}
            onKeyDown={onKeyDown}
          />
          {(options.length > 0 || loading) && (
            // A mouse shortcut for ↓, so it stays out of the tab order
            <button
              type="button"
              tabIndex={-1}
              aria-hidden
              className="absolute inset-y-0 right-0 flex w-8 items-center justify-center text-muted-foreground"
              // Keep the focus, and with it the typing, in the field
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => (open ? setOpen(false) : openList(''))}
            >
              <ChevronDown className="h-4 w-4 opacity-50" />
            </button>
          )}
        </div>
      </PopoverAnchor>
      <PopoverContent
        align="start"
        className="w-[var(--radix-popover-trigger-width)] p-0"
        onOpenAutoFocus={(e) => e.preventDefault()}
        onCloseAutoFocus={(e) => e.preventDefault()}
        onInteractOutside={(e) => {
          // A click on the field is not a click away from it
          if (anchorRef.current?.contains(e.target as Node)) e.preventDefault();
        }}
      >
        {loading ? (
          <div className="flex items-center gap-2 px-3 py-2 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {loadingText}
          </div>
        ) : (
          <ul id={listId} role="listbox" className="max-h-64 overflow-y-auto py-1">
            {shown.map((option, i) => (
              <li
                key={option}
                id={optionId(i)}
                role="option"
                aria-selected={i === active}
                className={cn(
                  'cursor-pointer px-3 py-1.5 text-sm',
                  i === active && 'bg-muted',
                  optionClassName,
                )}
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={() => setActive(i)}
                onClick={() => pick(option)}
              >
                {option}
              </li>
            ))}
          </ul>
        )}
      </PopoverContent>
    </Popover>
  );
}
