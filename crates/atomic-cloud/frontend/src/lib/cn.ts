/** Join conditional class names — tiny `clsx` stand-in, no dependency. */
export function cn(
  ...parts: Array<string | false | null | undefined>
): string {
  return parts.filter(Boolean).join(' ');
}
