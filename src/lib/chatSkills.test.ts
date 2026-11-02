import { beforeEach, describe, expect, it, vi } from 'vitest';
import { loadChatSkills, parseChatSkill, referencedChatSkills, saveChatSkills, skillsForTurn } from './chatSkills';

const method = `---
name: source-check
description: Check source quality before answering.
---
# Source check

Ask for a primary source and distinguish evidence from inference.`;

beforeEach(() => {
  const values = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value); },
  });
});

describe('chat skills', () => {
  it('imports a SKILL.md with YAML frontmatter', () => {
    expect(parseChatSkill(method)).toEqual({
      name: 'source-check',
      description: 'Check source quality before answering.',
      instructions: '# Source check\n\nAsk for a primary source and distinguish evidence from inference.',
    });
    expect(() => parseChatSkill('# Missing frontmatter')).toThrow('YAML frontmatter');
    expect(() => parseChatSkill(method.replace('source-check', 'Invalid Name'))).toThrow('Skill names');
  });

  it('only applies loaded skills explicitly referenced in this question', () => {
    const skill = parseChatSkill(method);
    expect(referencedChatSkills('Use $source-check for this.', [skill])).toEqual([skill]);
    expect(referencedChatSkills('What is $100M worth? source-check', [skill])).toEqual([]);
    expect(referencedChatSkills('Use $source-check-extra', [skill])).toEqual([]);
    expect(skillsForTurn('Use $source-check', [skill])).toEqual([skill]);
    expect(() => skillsForTurn('Use $source-chek', [skill])).toThrow('not loaded');
  });

  it('saves skills per database and enforces the per-turn limit', () => {
    const skill = parseChatSkill(method);
    saveChatSkills('db-a', [skill]);
    expect(loadChatSkills('db-a')).toEqual([skill]);
    expect(loadChatSkills('db-b')).toEqual([]);
    const four = ['one', 'two', 'three', 'four'].map(name => ({ ...skill, name }));
    expect(() => skillsForTurn('$one $two $three $four', four)).toThrow('up to 3');
  });
});
