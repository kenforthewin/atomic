import { BrowserRouter, Routes, Route, Navigate } from 'react-router-dom';
import { isAppHost } from './lib/host';
import { Landing } from './pages/Landing';
import { Signup } from './pages/Signup';
import { Login } from './pages/Login';
import { Legal } from './pages/Legal';
import { NotFound } from './pages/NotFound';
import { AccountShell } from './pages/account/AccountShell';
import { Overview } from './pages/account/Overview';
import { Provider } from './pages/account/Provider';
import { Billing } from './pages/account/Billing';
import { Mcp } from './pages/account/Mcp';
import { Danger } from './pages/account/Danger';

/**
 * One build, two route contexts, switched by `Host`:
 *
 * - **App host** (bare base domain + `app.<base>`) — the public pre-auth
 *   pages: landing, /signup, /login.
 * - **Tenant subdomain** (`<slug>.<base>`) — the authenticated dashboard. The
 *   {@link AccountShell} owns the chrome and the overview load; the nested
 *   routes render the active section. Unauthenticated calls under it bounce to
 *   the app-host login (the API client redirects a 401).
 *
 * The split happens once at the router root rather than per-route so the two
 * surfaces never bleed into each other (a tenant host can't render the public
 * signup form, and vice versa).
 */
export function App() {
  return (
    <BrowserRouter>
      {isAppHost() ? <AppHostRoutes /> : <TenantRoutes />}
    </BrowserRouter>
  );
}

function AppHostRoutes() {
  return (
    <Routes>
      <Route path="/" element={<Landing />} />
      <Route path="/signup" element={<Signup />} />
      <Route path="/login" element={<Login />} />
      <Route path="/terms" element={<Legal kind="terms" />} />
      <Route path="/privacy" element={<Legal kind="privacy" />} />
      <Route path="*" element={<NotFound />} />
    </Routes>
  );
}

function TenantRoutes() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/account" replace />} />
      <Route path="/account" element={<AccountShell />}>
        <Route index element={<Overview />} />
        <Route path="provider" element={<Provider />} />
        <Route path="billing" element={<Billing />} />
        <Route path="mcp" element={<Mcp />} />
        <Route path="danger" element={<Danger />} />
        {/* An unknown /account/* deep link returns to the overview. */}
        <Route path="*" element={<Navigate to="/account" replace />} />
      </Route>
      {/* Any non-account path on a tenant host → the dashboard root. */}
      <Route path="*" element={<Navigate to="/account" replace />} />
    </Routes>
  );
}
