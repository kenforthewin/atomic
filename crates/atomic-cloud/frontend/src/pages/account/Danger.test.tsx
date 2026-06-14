import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Routes, Route, Outlet } from 'react-router-dom';
import { Danger } from './Danger';
import { deletionErrorMessage } from './deletionError';
import type { AccountContext } from '../../lib/accountContext';
import { ApiError } from '../../lib/api';
import * as apiModule from '../../lib/api';
import { overview } from '../../test/fixtures';

/**
 * Render the Danger page inside an outlet that supplies the account context,
 * exactly as the dashboard shell does. The subdomain drives the confirm gate.
 */
function renderDanger(subdomain = 'alpha') {
  const context: AccountContext = {
    overview: overview({ subdomain }),
    reload: vi.fn(),
  };
  return render(
    <MemoryRouter>
      <Routes>
        <Route element={<Outlet context={context} />}>
          <Route path="*" element={<Danger />} />
        </Route>
      </Routes>
    </MemoryRouter>,
  );
}

function deleteButton() {
  return screen.getByRole('button', { name: /delete account permanently/i });
}

describe('Danger — destructive-confirm gating', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('disables the delete button until the typed text exactly matches the subdomain', async () => {
    const user = userEvent.setup();
    renderDanger('alpha');

    // Empty → disabled.
    expect(deleteButton()).toBeDisabled();

    const field = screen.getByLabelText(/type .* to confirm/i);

    // Partial / wrong → still disabled.
    await user.type(field, 'alph');
    expect(deleteButton()).toBeDisabled();

    // Wrong-case is a mismatch → still disabled (the server compares exactly).
    await user.clear(field);
    await user.type(field, 'ALPHA');
    expect(deleteButton()).toBeDisabled();

    // Exact match → enabled.
    await user.clear(field);
    await user.type(field, 'alpha');
    expect(deleteButton()).toBeEnabled();
  });

  it('does not call the delete API while the confirmation is unmatched', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(apiModule, 'deleteAccount');
    renderDanger('alpha');

    const field = screen.getByLabelText(/type .* to confirm/i);
    await user.type(field, 'wrong');
    // The disabled button can't be clicked; assert the gate, not just the click.
    expect(deleteButton()).toBeDisabled();
    expect(spy).not.toHaveBeenCalled();
  });

  it('re-disables when a matched confirmation is edited back to a mismatch', async () => {
    const user = userEvent.setup();
    renderDanger('alpha');
    const field = screen.getByLabelText(/type .* to confirm/i);

    await user.type(field, 'alpha');
    expect(deleteButton()).toBeEnabled();

    await user.type(field, 'x'); // now "alphax"
    expect(deleteButton()).toBeDisabled();
  });
});

describe('deletionErrorMessage', () => {
  it('gives retry guidance for a busy backup (503 deletion_busy)', () => {
    const msg = deletionErrorMessage(
      new ApiError(503, 'deletion_busy', 'busy'),
    );
    expect(msg).toMatch(/try again/i);
    expect(msg).toMatch(/nothing was changed/i);
  });

  it('explains a scope-limited token (403 account_scope_required)', () => {
    const msg = deletionErrorMessage(
      new ApiError(403, 'account_scope_required', 'nope'),
    );
    expect(msg).toMatch(/can’t delete the whole account/i);
  });

  it("passes through the server's message for other ApiErrors", () => {
    const msg = deletionErrorMessage(
      new ApiError(400, 'confirmation_mismatch', 'The confirmation did not match.'),
    );
    expect(msg).toBe('The confirmation did not match.');
  });

  it('falls back to a generic message for non-ApiError throwables', () => {
    expect(deletionErrorMessage(new Error('weird'))).toMatch(/something went wrong/i);
  });
});
