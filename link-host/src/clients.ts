// What a client may be given, read and checked the same way from a clients file and from the admin
// API, which both say lifetimes in days. A host keeps them in seconds.
import type { ClientSettings } from './types.js';
import { addressList } from './util.js';

const DAY = 86_400;

/** The settings of a client, and what each is. */
export const SETTING_FIELDS = {
  defaultAttempts: 'count',
  maxAttempts: 'count',
  maxFileBytes: 'count',
  maxFilesPerLink: 'count',
  defaultLifetimeDays: 'days',
  maxLifetimeDays: 'days',
  requirePasscode: 'boolean',
  allow: 'addresses',
  allowedContentTypes: 'types',
} as const;

export type FieldType = (typeof SETTING_FIELDS)[keyof typeof SETTING_FIELDS] | 'string';

const MEDIA_TYPE = /^[\w.+-]+\/[\w.+-]+$/;

/** The id of a client made over the admin API, and nothing else. A client given to the host by name
 * may not have a name of this shape, so that the two cannot be taken for each other. */
export const STORED_ID = /^cl_[A-Za-z0-9_-]{22}$/;

/**
 * Checks each field of `record` against `fields`, and says what is wrong with every one that is.
 * With `clearable`, `null` is allowed for a setting, and means it is taken away.
 */
export function checkFields(
  record: Record<string, unknown>,
  label: string,
  fields: Record<string, FieldType>,
  { clearable = false } = {},
): string[] {
  const problems: string[] = [];
  for (const [field, value] of Object.entries(record)) {
    const type = fields[field];
    if (type === undefined) {
      problems.push(`${label}: ${field} is not a setting of a client (${Object.keys(fields).join(', ')})`);
      continue;
    }
    if (value === null && clearable && type !== 'string') continue;
    const fine =
      type === 'string' ? typeof value === 'string' && value !== '' :
      type === 'boolean' ? typeof value === 'boolean' :
      type === 'count' ? Number.isInteger(value) && (value as number) > 0 :
      type === 'days' ? Number.isInteger(value) && (value as number) > 0 && (value as number) <= 36500 :
      type === 'addresses' || type === 'types' ? Array.isArray(value) && value.length > 0 && value.every((item) => typeof item === 'string') :
      false;
    if (!fine) {
      const what = {
        days: 'whole number of days',
        count: 'whole number above 0',
        addresses: 'list of addresses and ranges',
        types: 'list of media types, such as application/fhir+json',
      }[type as string] ?? type;
      problems.push(`${label}: ${field} is a ${what}`);
    } else if (type === 'addresses') {
      try {
        addressList(value as string[], `${label}: allow`);
      } catch (error) {
        problems.push((error as Error).message);
      }
    } else if (type === 'types' && !(value as string[]).every((item) => MEDIA_TYPE.test(item))) {
      problems.push(`${label}: ${field} is a list of media types, such as application/fhir+json`);
    }
  }
  return problems;
}

/** Checked settings, as a host keeps them: lifetimes in seconds, media types in lower case. A `null`
 * is kept as `null`, which a change reads as "take it away". */
export function toSettings(record: Record<string, unknown>): { [K in keyof ClientSettings]: ClientSettings[K] | null } {
  const settings: Record<string, unknown> = {};
  for (const [field, value] of Object.entries(record)) {
    if (!(field in SETTING_FIELDS)) continue;
    if (field === 'defaultLifetimeDays') settings.defaultLifetimeSeconds = value === null ? null : (value as number) * DAY;
    else if (field === 'maxLifetimeDays') settings.maxLifetimeSeconds = value === null ? null : (value as number) * DAY;
    else if (field === 'allowedContentTypes') settings.allowedContentTypes = value === null ? null : (value as string[]).map((type) => type.toLowerCase());
    else settings[field] = value;
  }
  return settings;
}

/** Settings as the admin API says them: lifetimes in days. */
export function fromSettings(settings: ClientSettings): Record<string, unknown> {
  const { defaultLifetimeSeconds, maxLifetimeSeconds, ...rest } = settings;
  return {
    ...rest,
    ...(defaultLifetimeSeconds === undefined ? {} : { defaultLifetimeDays: defaultLifetimeSeconds / DAY }),
    ...(maxLifetimeSeconds === undefined ? {} : { maxLifetimeDays: maxLifetimeSeconds / DAY }),
  };
}
