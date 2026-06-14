import type { AnchorHTMLAttributes } from 'react';
import { Link as RouterLink } from 'react-router-dom';
import type { LinkProps as RouterLinkProps } from 'react-router-dom';
import { cn } from '../../lib/cn';

const linkClass =
  'text-sm font-medium text-accent hover:text-accent-dark transition-colors ' +
  'rounded-sm focus-visible:outline-2';

/** An in-app router link styled as the website's accent text link. */
export function TextLink({ className, ...props }: RouterLinkProps) {
  return <RouterLink className={cn(linkClass, className)} {...props} />;
}

/** An external/plain anchor styled identically. */
export function ExternalTextLink({
  className,
  ...props
}: AnchorHTMLAttributes<HTMLAnchorElement>) {
  return <a className={cn(linkClass, className)} {...props} />;
}
