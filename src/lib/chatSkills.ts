import YAML from 'yaml';

export interface ChatSkill {
  name: string;
  description: string;
  instructions: string;
}

const MAX_SKILL_BYTES = 24_000;
const MAX_SAVED_SKILLS = 20;
const MAX_SKILLS_PER_TURN = 3;
const SKILL_NAME = /^[a-z][a-z0-9-]{0,63}$/;

export function parseChatSkill(markdown: string): ChatSkill {
  const match = markdown.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/);
  if (!match) throw new Error('The file needs YAML frontmatter with a name and description.');

  let metadata: unknown;
  try {
    metadata = YAML.parse(match[1]);
  } catch {
    throw new Error('The skill frontmatter is not valid YAML.');
  }
  if (!metadata || typeof metadata !== 'object') {
    throw new Error('The skill needs a name and description.');
  }
  const fields = metadata as Record<string, unknown>;
  const name = fields.name;
  const description = fields.description;
  const instructions = match[2].trim();
  if (typeof name !== 'string' || !SKILL_NAME.test(name)) {
    throw new Error('Skill names must start with a letter and use lowercase letters, numbers, or hyphens.');
  }
  if (typeof description !== 'string' || !description.trim() || !instructions) {
    throw new Error('The skill needs a description and instructions.');
  }
  if (new TextEncoder().encode(instructions).length > MAX_SKILL_BYTES) {
    throw new Error('This skill is too long to use in chat (24,000 bytes maximum).');
  }
  return { name, description: description.trim(), instructions };
}

export function referencedChatSkills(message: string, skills: ChatSkill[]): ChatSkill[] {
  const names = referencedSkillNames(message);
  return skills.filter((skill) => names.has(skill.name));
}

function referencedSkillNames(message: string): Set<string> {
  const names = new Set<string>();
  for (const match of message.matchAll(/(^|[^\w])\$([a-z][a-z0-9-]*)(?![\w-])/g)) {
    names.add(match[2]);
  }
  return names;
}

export function skillsForTurn(message: string, skills: ChatSkill[]): ChatSkill[] {
  const loaded = new Set(skills.map((skill) => skill.name));
  const missing = [...referencedSkillNames(message)].filter((name) => !loaded.has(name));
  if (missing.length > 0) {
    throw new Error(`Skill $${missing[0]} is not loaded. Import its SKILL.md or remove the reference.`);
  }
  const selected = referencedChatSkills(message, skills);
  if (selected.length > MAX_SKILLS_PER_TURN) {
    throw new Error(`Use up to ${MAX_SKILLS_PER_TURN} skills in one question.`);
  }
  return selected;
}

function storageKey(databaseId: string): string {
  return `atomic-chat-skills:${databaseId}`;
}

export function loadChatSkills(databaseId: string | null): ChatSkill[] {
  if (!databaseId) return [];
  try {
    const value: unknown = JSON.parse(localStorage.getItem(storageKey(databaseId)) ?? '[]');
    if (!Array.isArray(value)) return [];
    return value.filter((item): item is ChatSkill =>
      item && typeof item === 'object' &&
      typeof item.name === 'string' && SKILL_NAME.test(item.name) &&
      typeof item.description === 'string' &&
      typeof item.instructions === 'string' &&
      new TextEncoder().encode(item.instructions).length <= MAX_SKILL_BYTES,
    ).slice(0, MAX_SAVED_SKILLS);
  } catch {
    return [];
  }
}

export function saveChatSkills(databaseId: string | null, skills: ChatSkill[]): void {
  if (!databaseId) throw new Error('Wait for the database to finish loading.');
  if (skills.length > MAX_SAVED_SKILLS) {
    throw new Error(`You can save up to ${MAX_SAVED_SKILLS} skills in this database.`);
  }
  localStorage.setItem(storageKey(databaseId), JSON.stringify(skills));
}
