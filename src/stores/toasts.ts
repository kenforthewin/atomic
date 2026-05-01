/**
 * Thin toast helpers wrapping sonner. Provides a `toast` object with
 * `.error / .info / .success` methods that accept an optional `retry`
 * callback — rendered as an action button in the toast.
 *
 * Sonner's <Toaster> is already mounted in App.tsx; no additional
 * host component is needed.
 */
import { toast as sonnerToast } from 'sonner';

export type ToastKind = 'error' | 'info' | 'success';

export interface ToastOptions {
  /** Secondary description shown below the title. */
  detail?: string;
  /** When provided, toast renders a "Retry" action button. */
  retry?: () => void | Promise<void>;
  /** Auto-dismiss ms; undefined = sticky until user closes.
   *  Errors default to sticky; others default to sonner's duration (5 s). */
  ttlMs?: number;
}

function makeOptions(opts: ToastOptions | undefined, kind: ToastKind) {
  const action = opts?.retry
    ? { label: 'Retry', onClick: () => void opts.retry!() }
    : undefined;
  const duration = opts?.ttlMs ?? (kind === 'error' ? Infinity : undefined);
  return {
    description: opts?.detail,
    action,
    duration,
  };
}

export const toast = {
  error: (title: string, opts?: ToastOptions) =>
    sonnerToast.error(title, makeOptions(opts, 'error')),
  info: (title: string, opts?: ToastOptions) =>
    sonnerToast(title, makeOptions(opts, 'info')),
  success: (title: string, opts?: ToastOptions) =>
    sonnerToast.success(title, makeOptions(opts, 'success')),
};
