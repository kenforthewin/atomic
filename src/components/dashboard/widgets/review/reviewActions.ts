import { getTransport } from '../../../../lib/transport';
import { toast } from '../../../../stores/toasts';

export interface ReviewActionInput {
  /** Human label for the action, e.g. "Remove link" */
  label: string;
  /** The command to invoke (usually 'apply_health_item_fix'). */
  command: string;
  /** Payload for the invoke. */
  args: Record<string, unknown>;
}

/**
 * Fire a review-queue action. On failure, surface a toast with a Retry
 * button. On success, returns the response; on failure, returns undefined.
 *
 * Callers update local optimistic state from the return value.
 */
export async function runReviewAction(input: ReviewActionInput): Promise<unknown | undefined> {
  try {
    return await getTransport().invoke(input.command, input.args);
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    toast.error(`${input.label} failed`, {
      detail,
      retry: () => runReviewAction(input).then(() => undefined),
    });
    console.error(`[review-action] ${input.label} failed`, err);
    return undefined;
  }
}
