import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';

// Locale bundles are 60-70 KB gzipped each. Eagerly importing both
// adds ~140 KB to every initial page load even though a given browser
// only ever renders one of them. The `import()` form below makes Vite
// emit each locale as its own chunk, fetched only when that language
// is selected. The "switch language" path waits for the new bundle to
// arrive before swapping, so a click never lands on missing strings.

const SUPPORTED = ['en', 'zh'] as const;
type SupportedLang = (typeof SUPPORTED)[number];

function detectLanguage(): SupportedLang {
  try {
    const saved = globalThis.localStorage?.getItem('lang');
    if (saved && (SUPPORTED as readonly string[]).includes(saved)) {
      return saved as SupportedLang;
    }
  } catch {
    // localStorage may not be available (SSR, tests)
  }
  try {
    if (globalThis.navigator?.language?.startsWith('zh')) return 'zh';
  } catch {
    // navigator may not be available
  }
  return 'en';
}

async function loadLocale(lng: SupportedLang): Promise<Record<string, unknown>> {
  // Vite turns these dynamic imports into per-language chunks
  // (en.json → assets/i18n-en-<hash>.js, etc.).
  switch (lng) {
    case 'zh':
      return (await import('./zh.json')).default;
    case 'en':
    default:
      return (await import('./en.json')).default;
  }
}

const initial = detectLanguage();

// Initialise synchronously with an empty resource map for the chosen
// language so React renders without crashing during the first paint;
// the real bundle resolves on the next tick and re-renders translated
// strings via i18next's namespace-add notification.
i18n.use(initReactI18next).init({
  resources: { [initial]: { translation: {} } },
  lng: initial,
  fallbackLng: 'en',
  interpolation: { escapeValue: false },
});

void loadLocale(initial).then((translation) => {
  i18n.addResourceBundle(initial, 'translation', translation, true, true);
});

// Pull in the new bundle (if not already loaded) BEFORE letting the
// `languageChanged` event fan out, so consumers always render against
// a populated namespace.
const originalChangeLanguage = i18n.changeLanguage.bind(i18n);
i18n.changeLanguage = (async (lng?: string) => {
  if (lng && (SUPPORTED as readonly string[]).includes(lng)) {
    if (!i18n.hasResourceBundle(lng, 'translation')) {
      const translation = await loadLocale(lng as SupportedLang);
      i18n.addResourceBundle(lng, 'translation', translation, true, true);
    }
  }
  return originalChangeLanguage(lng);
}) as typeof i18n.changeLanguage;

i18n.on('languageChanged', (lng) => {
  try {
    globalThis.localStorage?.setItem('lang', lng);
  } catch {
    // ignore
  }
});

export default i18n;
