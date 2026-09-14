import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClientProvider } from '@tanstack/react-query'
import { TooltipProvider } from '@/components/ui/tooltip'
import { onSuccessfulWrite } from '@/lib/api'
import { createQueryClient } from '@/lib/query-client'
import './i18n'
import './index.css'
import App from './App'

const queryClient = createQueryClient()

// A write can change what any cached screen holds. The screen making it
// refreshes what it shows; this drops the cached queries no screen is using,
// so none of them reopens onto data from before the write.
onSuccessfulWrite(() => queryClient.removeQueries({ type: 'inactive' }))

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <TooltipProvider>
        <App />
      </TooltipProvider>
    </QueryClientProvider>
  </StrictMode>,
)
