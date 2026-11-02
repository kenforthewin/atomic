import { describe, expect, it } from 'vitest';
import { parseCanvasImage, parseCompanyProfile, parseExecutiveProfile } from './accountNetworkProfile';

describe('account network profile metadata', () => {
  it('extracts sourced company fields and keeps research notes separate', () => {
    const content = `# Peachtree Group

## Company profile

- Checked: 2026-11-01
- Website: [www.peachtreegroup.com](https://www.peachtreegroup.com/)
- LinkedIn: [Company page](https://www.linkedin.com/company/peachtree-group/)
- Annual originations: $1.6B in hotel and commercial real estate loans (2024) ([Source](https://www.peachtreegroup.com/originations))
- Headcount: 357 corporate employees ([Source](https://www.peachtreegroup.com/leadership))
- HQ: Atlanta, GA ([Source](https://www.peachtreegroup.com/contact))
- Logo: [Official logo](/account-network/peachtree-group/logo.svg) ([Source](https://www.peachtreegroup.com/))

## Research notes

Fit review: Research candidate.`;
    expect(parseCompanyProfile(content)).toEqual({
      checkedAt: '2026-11-01',
      website: 'https://www.peachtreegroup.com/',
      linkedIn: 'https://www.linkedin.com/company/peachtree-group/',
      annualOriginations: { value: '$1.6B in hotel and commercial real estate loans (2024)', source: 'https://www.peachtreegroup.com/originations' },
      headcount: { value: '357 corporate employees', source: 'https://www.peachtreegroup.com/leadership' },
      hq: { value: 'Atlanta, GA', source: 'https://www.peachtreegroup.com/contact' },
      logo: { url: '/account-network/peachtree-group/logo.svg', source: 'https://www.peachtreegroup.com/' },
      researchContent: 'Fit review: Research candidate.',
    });
  });

  it('accepts only safe image URLs', () => {
    expect(parseCanvasImage('- Canvas image: [Portrait](/account-network/peachtree-group/people/greg.webp)')).toBe('/account-network/peachtree-group/people/greg.webp');
    expect(parseCanvasImage('- Canvas image: [Portrait](javascript:alert(1))')).toBeUndefined();
    expect(parseCanvasImage('- Canvas image: [Portrait](/account-network/../private.png)')).toBeUndefined();
  });

  it('extracts executive scan facts without inventing a warm path', () => {
    const content = `# Greg Friedman

## Person profile

- LinkedIn URL: [Profile](https://www.linkedin.com/in/greg-friedman-0312192)
- Undergraduate college: Not verified
- Graduate college: Not verified
- Other college: University of Texas at Austin
- Associations: American Hotel & Lodging Association; Texas McCombs real estate fund advisory board
- LinkedIn About: Not verified
- Email: Not verified
- Phone: Not verified
- Mutuals: No direct connection or named mutual found in checked Happenstance sources
- Mutuals check: 2026-11-01 ([Source](https://happenstance.ai/chat/check-id))

## Research notes

Record type: current executive candidate; not a contact record or outreach approval.
Official title: Managing Principal & CEO. Research role: Executive entry candidate.
Profile URLs: [LinkedIn](https://www.linkedin.com/in/greg-friedman-0312192) · [Official bio](https://www.peachtreegroup.com/team-members/greg-friedman).
Common ground with Mark Dusseau: no direct connection or named mutual found in the current Happenstance scope. This is not proof that no path exists; school, prior-work, and community overlap remain unverified.
Happenstance network check: 2026-11-01; source: https://happenstance.ai/chat/check-id.
Education (school match with Mark pending his resume):
- University of Texas at Austin: Graduate; degree and dates not stated. Source: https://www.peachtreegroup.com/team-members/greg-friedman
Prior work (Mark overlap unknown):
- Specialty Finance Group — former senior vice president. Source: https://www.peachtreegroup.com/team-members/greg-friedman
External affiliations (do not confuse with Peachtree board membership):
- American Hotel & Lodging Association: Board member. Source: https://www.peachtreegroup.com/team-members/greg-friedman

## Network connections
- Some edge: [[id|Title]]`;
    expect(parseExecutiveProfile(content)).toEqual({
      title: 'Managing Principal & CEO',
      officialBio: 'https://www.peachtreegroup.com/team-members/greg-friedman',
      linkedIn: 'https://www.linkedin.com/in/greg-friedman-0312192',
      networkCheck: { checkedAt: '2026-11-01', source: 'https://happenstance.ai/chat/check-id' },
      college: { undergraduate: [], graduate: [], other: ['University of Texas at Austin'] },
      priorWork: ['Specialty Finance Group'],
      affiliations: ['American Hotel & Lodging Association', 'Texas McCombs real estate fund advisory board'],
      linkedInAbout: 'Not verified',
      email: 'Not verified',
      phone: 'Not verified',
      mutuals: 'No direct connection or named mutual found in checked Happenstance sources',
      researchContent: content.split('## Research notes\n\n')[1].trim(),
    });
    expect(parseExecutiveProfile(content.replace('https://www.peachtreegroup.com/team-members/greg-friedman', 'javascript:alert(1)'))).toBeNull();
    expect(parseExecutiveProfile(content.replace('[Official bio]', '[Official Peachtree bio]'))?.officialBio)
      .toBe('https://www.peachtreegroup.com/team-members/greg-friedman');
    const linkedinEvidence = content.replace(
      '- Mutuals: No direct connection or named mutual found in checked Happenstance sources',
      '- Mutuals: LinkedIn shows Bo, Tony and 78 other mutual connections in Mark\'s view\n- LinkedIn mutuals evidence: Mark-provided screenshot; 2026-11-01; first-name labels only; introduction path unverified',
    );
    expect(parseExecutiveProfile(linkedinEvidence)).toMatchObject({
      mutuals: "LinkedIn shows Bo, Tony and 78 other mutual connections in Mark's view",
      linkedInMutualsEvidence: 'Mark-provided screenshot · 2026-11-01 · first-name labels only · introduction path unverified',
    });
    const apiEvidence = content.replace(
      '- Mutuals: No direct connection or named mutual found in checked Happenstance sources',
      '- Mutuals: Unipile shows 1 shared LinkedIn connection with Mark; name unresolved\n- LinkedIn mutuals evidence: Unipile API; 2026-11-01; exact profile; count only; name unresolved',
    );
    expect(parseExecutiveProfile(apiEvidence)).toMatchObject({
      mutuals: 'Unipile shows 1 shared LinkedIn connection with Mark; name unresolved',
      linkedInMutualsEvidence: 'Unipile API · 2026-11-01 · exact profile · count only · name unresolved',
    });
    const namedMutual = content.replace(
      '- Mutuals: No direct connection or named mutual found in checked Happenstance sources',
      "- Mutuals: Andy Chopra — first-degree with Mark and listed among Mitul Patel's LinkedIn relations; introduction unconfirmed\n- LinkedIn mutuals evidence: Unipile v2 API; 2026-11-01; Mitul relation plus Mark first-degree; introduction unconfirmed",
    );
    expect(parseExecutiveProfile(namedMutual)).toMatchObject({
      mutuals: "Andy Chopra — first-degree with Mark and listed among Mitul Patel's LinkedIn relations; introduction unconfirmed",
      linkedInMutualsEvidence: 'Unipile v2 API · 2026-11-01 · Mitul relation plus Mark first-degree · introduction unconfirmed',
    });
  });
});
