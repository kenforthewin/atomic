import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, act } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { Signup } from './Signup';
import * as apiModule from '../lib/api';

function renderSignup() {
  return render(
    <MemoryRouter>
      <Signup />
    </MemoryRouter>,
  );
}

describe('Signup', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('renders the live <slug>.<base> subdomain preview as the user types', async () => {
    const user = userEvent.setup();
    renderSignup();

    // Before any input, the help text shows a placeholder slug.
    expect(screen.getByText(/your-name\.atomicapp\.ai/)).toBeInTheDocument();

    await user.type(screen.getByLabelText(/subdomain/i), 'my-team');

    expect(screen.getByText(/my-team\.atomicapp\.ai/)).toBeInTheDocument();
  });

  it('normalizes disallowed characters out of the subdomain field', async () => {
    const user = userEvent.setup();
    renderSignup();

    const field = screen.getByLabelText(/subdomain/i) as HTMLInputElement;
    await user.type(field, 'My Team!');

    // Uppercase lowercased, space + bang stripped.
    expect(field.value).toBe('myteam');
  });

  it('shows an inline email error and does not call the API for an invalid email', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(apiModule, 'requestSignupLink');
    renderSignup();

    await user.type(screen.getByLabelText(/email/i), 'not-an-email');
    await user.type(screen.getByLabelText(/subdomain/i), 'valid-team');
    // Submit button is disabled while the email is invalid; submit the form
    // directly to exercise the client-side guard.
    const form = screen
      .getByRole('button', { name: /send my sign-up link/i })
      .closest('form')!;
    await act(async () => {
      form.requestSubmit();
    });

    expect(
      await screen.findByText(/that email address doesn't look valid/i),
    ).toBeInTheDocument();
    expect(spy).not.toHaveBeenCalled();
  });

  it('maps a subdomain_taken response to an inline field error', async () => {
    const user = userEvent.setup();
    vi.spyOn(apiModule, 'requestSignupLink').mockRejectedValue(
      new apiModule.ApiError(
        400,
        'subdomain_taken',
        'That subdomain is already taken.',
      ),
    );
    renderSignup();

    await user.type(screen.getByLabelText(/email/i), 'you@example.com');
    await user.type(screen.getByLabelText(/subdomain/i), 'taken-name');
    await user.click(
      screen.getByRole('button', { name: /send my sign-up link/i }),
    );

    expect(
      await screen.findByText(/that subdomain is already taken/i),
    ).toBeInTheDocument();
  });

  it('shows the check-your-email confirmation on success', async () => {
    const user = userEvent.setup();
    vi.spyOn(apiModule, 'requestSignupLink').mockResolvedValue({
      status: 'ok',
      message: 'ok',
    });
    renderSignup();

    await user.type(screen.getByLabelText(/email/i), 'you@example.com');
    await user.type(screen.getByLabelText(/subdomain/i), 'fresh-team');
    await user.click(
      screen.getByRole('button', { name: /send my sign-up link/i }),
    );

    expect(
      await screen.findByRole('heading', { name: /check your email/i }),
    ).toBeInTheDocument();
    expect(screen.getByText(/you@example\.com/)).toBeInTheDocument();
  });
});
