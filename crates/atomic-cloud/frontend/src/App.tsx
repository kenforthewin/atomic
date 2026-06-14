import { BrowserRouter, Routes, Route, Navigate } from 'react-router-dom';
import { isAppHost } from './lib/host';
import { Landing } from './pages/Landing';
import { Signup } from './pages/Signup';
import { Login } from './pages/Login';
import { AccountShell } from './pages/AccountShell';
import { NotFound } from './pages/NotFound';

/**
 * One build, two route contexts, switched by `Host`:
 *
 * - **App host** (bare base domain + `app.<base>`) — the public pre-auth pages:
 *   landing, /signup, /login.
 * - **Tenant subdomain** (`<slug>.<base>`) — the authenticated `/account/*`
 *   dashboard. The placeholder shell lands next phase; for now any path under a
 *   tenant host renders the branded loading frame, and `/` redirects to
 *   `/account`.
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
      <Route path="*" element={<NotFound />} />
    </Routes>
  );
}

function TenantRoutes() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/account" replace />} />
      <Route path="/account/*" element={<AccountShell />} />
      <Route path="*" element={<AccountShell />} />
    </Routes>
  );
}
