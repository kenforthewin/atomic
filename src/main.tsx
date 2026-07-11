import ReactDOM from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import App from './App'
import { ErrorBoundary } from './components/ui/ErrorBoundary'
import './index.css'
import { initTransport } from './lib/transport'
import { setupPwaUpdates } from './lib/pwa'

setupPwaUpdates()

initTransport()
  .catch((err) => console.error('Transport init failed:', err))
  .then(() => {
    const app = (
      <ErrorBoundary>
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </ErrorBoundary>
    )

    ReactDOM.createRoot(document.getElementById('root')!).render(app)
  })
