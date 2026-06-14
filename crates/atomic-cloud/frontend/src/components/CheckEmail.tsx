import { MailCheck } from 'lucide-react';
import { Card, CardIcon } from './ui/Card';
import { Button } from './ui/Button';

interface CheckEmailProps {
  /** The address the link was sent to, echoed back for reassurance. */
  email: string;
  /** Body copy — differs between signup and the account-neutral login flow. */
  children: React.ReactNode;
  /** Resets the parent form so the user can try a different address. */
  onUseDifferentEmail: () => void;
}

/**
 * The post-submit confirmation card shared by /signup and /login. The login
 * copy is deliberately account-existence-neutral (passed in by the caller);
 * this component only renders it.
 */
export function CheckEmail({ email, children, onUseDifferentEmail }: CheckEmailProps) {
  return (
    <Card className="text-center fade-in-up">
      <CardIcon className="mx-auto mb-5 w-12 h-12 rounded-xl">
        <MailCheck className="w-6 h-6" strokeWidth={1.5} aria-hidden="true" />
      </CardIcon>
      <h2 className="font-display text-2xl tracking-tight mb-2">Check your email</h2>
      <p className="text-text-secondary leading-relaxed">{children}</p>
      <p className="mt-4 text-sm text-text-muted">
        Sent to <span className="font-medium text-text-secondary">{email}</span>.
        The link expires shortly — request a new one if it lapses.
      </p>
      <div className="mt-6">
        <Button variant="secondary" size="sm" onClick={onUseDifferentEmail}>
          Use a different email
        </Button>
      </div>
    </Card>
  );
}
