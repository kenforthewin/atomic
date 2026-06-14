import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, act } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { Login } from './Login';
import * as apiModule from '../lib/api';

function renderLogin() {
  return render(
    <MemoryRouter>
      <Login />
    </MemoryRouter>,
  );
}

describe('Login', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('shows account-existence-neutral confirmation copy on submit', async () => {
    const user = userEvent.setup();
    vi.spyOn(apiModule, 'requestLoginLink').mockResolvedValue({
      status: 'ok',
      message: 'ok',
    });
    renderLogin();

    await user.type(screen.getByLabelText(/email/i), 'someone@example.com');
    await user.click(
      screen.getByRole('button', { name: /send my sign-in link/i }),
    );

    // The copy must NOT assert an account exists — it hedges with "if".
    const confirmation = await screen.findByText(
      /if an atomic account uses that address/i,
    );
    expect(confirmation).toBeInTheDocument();
    expect(confirmation.textContent).toMatch(/^If /);
  });

  it('rejects an invalid email inline without calling the API', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(apiModule, 'requestLoginLink');
    renderLogin();

    await user.type(screen.getByLabelText(/email/i), 'nope');
    const form = screen
      .getByRole('button', { name: /send my sign-in link/i })
      .closest('form')!;
    await act(async () => {
      form.requestSubmit();
    });

    expect(
      await screen.findByText(/that email address doesn't look valid/i),
    ).toBeInTheDocument();
    expect(spy).not.toHaveBeenCalled();
  });
});
