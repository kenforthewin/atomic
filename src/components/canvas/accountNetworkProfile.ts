export interface CompanyProfile {
  checkedAt?: string;
  website: string;
  linkedIn?: string;
  annualOriginations?: { value: string; source: string };
  headcount?: { value: string; source: string };
  hq?: { value: string; source: string };
  logo?: { url: string; source: string };
  researchContent: string;
}

export interface ExecutiveProfile {
  title: string;
  officialBio: string;
  linkedIn?: string;
  networkCheck?: { checkedAt: string; source: string };
  college: { undergraduate: string[]; graduate: string[]; other: string[] };
  priorWork: string[];
  affiliations: string[];
  linkedInAbout: string;
  email: string;
  phone: string;
  mutuals: string;
  linkedInMutualsEvidence?: string;
  researchContent: string;
}

function externalUrl(value: string | undefined): string | undefined {
  if (!value) return undefined;
  try {
    const url = new URL(value);
    return url.protocol === 'https:' ? url.href : undefined;
  } catch {
    return undefined;
  }
}

function imageUrl(value: string | undefined): string | undefined {
  return value?.startsWith('/account-network/') && !value.includes('..')
    ? value
    : externalUrl(value);
}

function linkedUrl(section: string, label: string): string | undefined {
  const match = section.match(new RegExp(`^- ${label}: \\[[^\\]]+\\]\\(([^)]+)\\)`, 'm'));
  return externalUrl(match?.[1]);
}

function sourcedValue(section: string, label: string): { value: string; source: string } | undefined {
  const match = section.match(new RegExp(`^- ${label}: (.+?) \\(\\[Source\\]\\(([^)]+)\\)\\)$`, 'm'));
  const source = externalUrl(match?.[2]);
  return match?.[1] && source ? { value: match[1], source } : undefined;
}

export function parseCompanyProfile(content: string): CompanyProfile | null {
  const match = content.match(/^## Company profile\s*\n([\s\S]*?)(?=^## Research notes\s*$)/m);
  if (!match) return null;
  const section = match[1];
  const website = linkedUrl(section, 'Website');
  if (!website) return null;
  const logoMatch = section.match(/^- Logo: \[[^\]]+\]\(([^)]+)\) \(\[Source\]\(([^)]+)\)\)$/m);
  const logoUrl = imageUrl(logoMatch?.[1]);
  const logoSource = externalUrl(logoMatch?.[2]);
  return {
    checkedAt: section.match(/^- Checked: (\d{4}-\d{2}-\d{2})$/m)?.[1],
    website,
    linkedIn: linkedUrl(section, 'LinkedIn'),
    annualOriginations: sourcedValue(section, 'Annual originations'),
    headcount: sourcedValue(section, 'Headcount'),
    hq: sourcedValue(section, 'HQ'),
    logo: logoUrl && logoSource ? { url: logoUrl, source: logoSource } : undefined,
    researchContent: content.slice(match.index! + match[0].length)
      .replace(/^\s*## Research notes\s*\n/, '').trim(),
  };
}

export function parseCanvasImage(content: string): string | undefined {
  const match = content.match(/^- Canvas image: \[[^\]]+\]\(([^)]+)\)/m);
  return imageUrl(match?.[1]);
}

function linesBetween(content: string, start: string, end: string): string[] {
  const from = content.indexOf(start);
  if (from < 0) return [];
  const to = content.indexOf(end, from + start.length);
  return content.slice(from + start.length, to < 0 ? undefined : to)
    .split('\n').filter((line) => line.startsWith('- '));
}

export function parseExecutiveProfile(content: string): ExecutiveProfile | null {
  const title = content.match(/^Official title: (.*?)\. Research role:/m)?.[1];
  const officialBio = externalUrl(content.match(/\[Official(?: [^\]]+)? bio\]\(([^)]+)\)/)?.[1]);
  if (!title || !officialBio) return null;

  const sectionMatch = content.match(/^## Person profile\s*\n([\s\S]*?)(?=^## Research notes\s*$)/m);
  const section = sectionMatch?.[1] ?? '';
  const field = (label: string) => section.match(new RegExp(`^- ${label}: (.+)$`, 'm'))?.[1];
  const names = (value: string | undefined) => value && value !== 'Not verified' && value !== 'Not documented'
    ? value.split('; ').filter(Boolean) : [];
  const networkCheck = content.match(/^Happenstance network check: (\d{4}-\d{2}-\d{2}); source: (https:\/\/\S+)\.$/m);
  const networkSource = externalUrl(networkCheck?.[2]);
  const legacyEducation = linesBetween(content, 'Education (school match with Mark pending his resume):', 'Prior work (Mark overlap unknown):')
    .map((line) => line.match(/^- (.*?): .*?\. Source: https:\/\//)?.[1]).filter((name): name is string => !!name);
  const priorWork = linesBetween(content, 'Prior work (Mark overlap unknown):', 'External affiliations (do not confuse with Peachtree board membership):')
    .map((line) => line.match(/^- (.*?)\. Source: https:\/\//)?.[1]?.split(' — ')[0]).filter((name): name is string => !!name);
  const legacyAffiliations = linesBetween(content, 'External affiliations (do not confuse with Peachtree board membership):', '## Network connections')
    .map((line) => line.match(/^- (.*?): .*?\. Source: https:\/\//)?.[1]).filter((name): name is string => !!name);
  const mutuals = field('Mutuals')?.replace(/ \(\[Source\]\([^)]+\)\)$/, '')
    ?? (content.includes('no direct connection or named mutual found in the current Happenstance scope')
      ? 'No direct connection or named mutual found in checked sources' : 'Not verified');
  const mutualEvidence = field('LinkedIn mutuals evidence');

  return {
    title,
    officialBio,
    linkedIn: externalUrl(field('LinkedIn URL')?.match(/\[Profile\]\(([^)]+)\)/)?.[1]
      ?? content.match(/\[LinkedIn\]\(([^)]+)\)/)?.[1]),
    networkCheck: networkCheck && networkSource ? {
      checkedAt: networkCheck[1],
      source: networkSource,
    } : undefined,
    college: {
      undergraduate: names(field('Undergraduate college')),
      graduate: names(field('Graduate college')),
      other: sectionMatch ? names(field('Other college')) : legacyEducation,
    },
    priorWork,
    affiliations: sectionMatch ? names(field('Associations')) : legacyAffiliations,
    linkedInAbout: field('LinkedIn About') ?? 'Not verified',
    email: field('Email') ?? 'Not verified',
    phone: field('Phone') ?? 'Not verified',
    mutuals,
    ...(mutualEvidence && mutualEvidence !== 'Not captured'
      ? { linkedInMutualsEvidence: mutualEvidence.replace(/; /g, ' · ') }
      : {}),
    researchContent: sectionMatch
      ? content.slice(sectionMatch.index! + sectionMatch[0].length).replace(/^\s*## Research notes\s*\n/, '').trim()
      : content.replace(/^# [^\n]+\n+/, '').trim(),
  };
}
