import { diff_match_patch, DIFF_DELETE, DIFF_INSERT } from 'diff-match-patch';

export type DiffPart = { type: 'equal' | 'insert' | 'delete'; text: string };

export function lineDiff(a: string, b: string): DiffPart[] {
  const dmp = new diff_match_patch();
  const { chars1, chars2, lineArray } = (dmp as unknown as {
    diff_linesToChars_(a: string, b: string): { chars1: string; chars2: string; lineArray: string[] };
  }).diff_linesToChars_(a, b);
  const diffs = dmp.diff_main(chars1, chars2, false);
  dmp.diff_cleanupSemantic(diffs);
  (dmp as unknown as { diff_charsToLines_(diffs: Array<[number, string]>, lineArray: string[]): void })
    .diff_charsToLines_(diffs as unknown as Array<[number, string]>, lineArray);
  return (diffs as Array<[number, string]>).map(([op, text]) => ({
    type: op === DIFF_INSERT ? 'insert' : op === DIFF_DELETE ? 'delete' : 'equal',
    text,
  }));
}
