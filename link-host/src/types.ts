import type { Context } from 'hono';

/** A file on a link, without the file: a store keeps the file itself apart. */
export interface FileEntry {
  id: string;
  contentType: string;
  bytes: number;
}

/** What a link is. Times are seconds since 1970. */
export interface Link {
  id: string;
  /** SHA-256, in hex, of the token that lets the link's owner change it. */
  tokenHash: string;
  active: boolean;
  createdAt: number;
  createdBy: string;
  expiresAt: number;
  endedAt: number | null;
  attemptsLeft: number;
  passcode: { salt: string; hash: string } | null;
  maxFiles: number;
  maxFileBytes: number;
  files: FileEntry[];
}

export interface AccessEntry {
  recipient: string;
  at: number;
  outcome: 'opened' | 'wrong-passcode';
  ip?: string;
  userAgent?: string;
}

export interface SweepCounts {
  /** Links that can be served, and the bytes of their files. */
  links: number;
  bytes: number;
  /** Links that had ended and still had files, which were deleted. */
  emptied: number;
  /** Links that ended long ago, which were forgotten. */
  forgotten: number;
}

/** What `Store.update` lets a change do to the link it holds, besides changing the link's own fields. */
export interface LinkTx {
  /** Keeps a file on the link, and adds it to `link.files`. */
  putFile(file: FileEntry, text: string): Promise<void>;
  /** Deletes every file of the link, and empties `link.files`. */
  removeFiles(): Promise<void>;
  /** Notes who asked, and keeps only the latest `keep` entries. */
  recordAccess(entry: AccessEntry, keep: number): Promise<void>;
}

/**
 * Where links are kept. The host asks nothing else of one, so another database can stand in.
 *
 * `update` is the one that matters: it is a transaction on one link. `change` is called with the link
 * (or `null`, when there is none), and what `change` did to the link's `active`, `endedAt` and
 * `attemptsLeft`, and through `tx`, is kept if it returns and thrown away if it throws. What it
 * returns is what `update` returns. Nothing else changes the link between the `change` that saw it
 * and the keeping of what that `change` did, in this process or another. A store that can hold the
 * link while `change` runs does (a row lock does); one that cannot (SQLite, which would hold every
 * link at once) checks at the end that the link is as `change` saw it, and if it is not, calls
 * `change` again with the link as it is. So `change` may be called more than once, and must do
 * nothing but change the link and use `tx`: what is counted or told is done after `update` returns.
 */
export interface Store {
  create(link: Link): Promise<void>;
  get(id: string): Promise<Link | null>;
  update<T>(id: string, change: (link: Link | null, tx: LinkTx) => Promise<T> | T): Promise<T>;
  getFile(id: string, name: string): Promise<string | null>;
  /** The latest entries, oldest first. */
  listAccess(id: string): Promise<AccessEntry[]>;
  /** A secret of the host's, made the first time anyone asks and the same for everyone after. */
  secret(name: string): Promise<string>;
  /** Deletes the files of links that have ended, and the links that ended `retainSeconds` ago.
   * `null` if another process is already doing it. */
  sweep(args: { now: number; retainSeconds: number }): Promise<SweepCounts | null>;
  /** Throws if the store cannot take a write. */
  ping(): Promise<void>;
  close(): Promise<void>;
}

/** The store could not take the work in time. The host answers 503, and the caller may try again. */
export class StoreBusy extends Error {}

/** Someone who may make links, and what each of their links may be. */
export interface Client {
  name: string;
  token?: string;
  /** `sha256:` and the SHA-256 of the token in hex, in place of the token. */
  tokenHash?: string;
  /** Addresses and ranges the client's token is accepted from. */
  allow?: string[];
  defaultAttempts?: number;
  maxAttempts?: number;
  defaultLifetimeSeconds?: number;
  maxLifetimeSeconds?: number;
  maxFileBytes?: number;
  maxFilesPerLink?: number;
  requirePasscode?: boolean;
}

export interface RateLimits {
  /** Requests a minute from one address to what a receiver calls. 0 allows any number. */
  public: number;
  /** Requests a minute, from anywhere, to one link. */
  perLink: number;
  /** Requests a minute from one address to what an owner calls. */
  management: number;
  /** Links a minute for one client. */
  create: number;
  /** Wrong tokens in five minutes from one address before it is shut out. */
  authFailures: number;
}

/** What a hook is told of a link: nothing that opens it. */
export interface LinkSummary {
  id: string;
  createdBy: string;
  createdAt: number;
  expiresAt: number;
  passcodeRequired: boolean;
  files: number;
}

export type Verdict = boolean | void | { allow: false; status?: number; error?: string } | { allow?: true };

export interface HostEvent {
  type: string;
  at: number;
  [field: string]: unknown;
}

export interface LogEntry {
  ts: string;
  level: 'info' | 'warn' | 'error';
  msg: string;
  [field: string]: unknown;
}

export interface HostOptions {
  store: Store;
  /** One token that may make links, as the client named `admin`. */
  adminToken?: string;
  clients?: Client[];
  /** Says who is making a link, in place of the tokens. */
  authenticate?: (request: Request, info: { ip: string }) => Client | null | Promise<Client | null>;
  authorizeCreate?: (details: { client: string; options: Record<string, unknown>; ip: string; request: Request }) => Verdict | Promise<Verdict>;
  authorizeManifest?: (details: { link: LinkSummary; recipient: string; ip: string; request: Request }) => Verdict | Promise<Verdict>;
  onEvent?: (event: HostEvent) => void | Promise<void>;
  logger?: (entry: LogEntry) => void;

  /** The address the host is reached at. Without it the `Host` header of each request is used. */
  publicUrl?: string;
  /** Signs file tickets. Without it the store keeps one, which every process shares. */
  ticketSecret?: string;
  now?: () => number;
  /** The address a request came from, when it is not a Node request. */
  remoteAddress?: (c: Context) => string | undefined;

  defaultAttempts?: number;
  maxAttempts?: number;
  maxFileBytes?: number;
  maxFilesPerLink?: number;
  defaultLifetimeSeconds?: number;
  maxLifetimeSeconds?: number;
  retainSeconds?: number;
  sweepIntervalSeconds?: number;
  accessKept?: number;
  maxConcurrent?: number;
  trustProxy?: number;
  requirePasscode?: boolean;
  allowedContentTypes?: string[];
  corsOrigins?: string[];
  managementCorsOrigins?: string[];
  managementAllow?: string[];
  recordClientInfo?: boolean;
  metricsToken?: string;
  rateLimits?: Partial<RateLimits>;
}
