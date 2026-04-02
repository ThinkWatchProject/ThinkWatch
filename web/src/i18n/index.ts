import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import en from './en.json';
import zh from './zh.json';

function detectLanguage(): string {
  try {
    const saved = globalThis.localStorage?.getItem('lang');
    if (saved) return saved;
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

i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
  },
  lng: detectLanguage(),
  fallbackLng: 'en',
  interpolation: {
    escapeValue: false,
  },
});

i18n.on('languageChanged', (lng) => {
  try {
    globalThis.localStorage?.setItem('lang', lng);
  } catch {
    // ignore
  }
});

export default i18n;
