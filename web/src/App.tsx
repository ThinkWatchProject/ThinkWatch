import { RouterProvider } from '@tanstack/react-router';
import { router } from './router';
import { Toaster } from '@/components/ui/sonner';
import { ErrorBoundary } from '@/components/ErrorBoundary';
import { CommandPalette } from '@/components/command-palette';

export default function App() {
  return (
    <ErrorBoundary>
      <RouterProvider router={router} />
      <CommandPalette />
      <Toaster position="top-right" richColors />
    </ErrorBoundary>
  );
}
