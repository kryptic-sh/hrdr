# Session retention: compression and purge

Status: **implemented.**

hrdr accumulates one JSON transcript per session under
`$XDG_DATA_HOME/hrdr/sessions/<cwd-slug>/<id>.json`. Over time this grows
without bound — thousands of files, some several megabytes. This feature
reclaims that space automatically: old sessions are **compressed**, and old
**auto-named** sessions are eventually **purged**, by a background worker that
never touches a session another hrdr instance is using.

## Behaviour

- **Compress** a session whose file has not changed in more than **1 week**:
  rewrite `<id>.json` as `<id>.json.zst` (zstd, level 3) and delete the
  plaintext. The session stays fully usable — the loader decompresses
  `.json.zst` transparently.
- **Purge** a session whose file has not changed in more than **1 month**, but
  **only if it was auto-named** (never given a name by the user). A purge
  deletes the file outright. User-named sessions are kept forever.
- Both ages are configurable; either phase can be disabled.

### Why zstd

zstd at its default level (3) sits on the good part of the ratio/speed frontier:
it compresses JSON transcripts roughly 8–10x for very little CPU, so the sweep
stays cheap. gzip was considered; zstd wins on ratio at equal or lower CPU.

### What counts as "user-named"

A new persisted boolean, `named_by_user`, distinguishes an explicit name from
the auto-derived one:

- It is set **`true`** whenever the user names a session — both `/rename <name>`
  and `/new <name>` funnel through `set_session_label`, so the flag is set
  there, in one place.
- The auto-name path (`session_name_from`, derived from the first user message)
  does **not** set it, so it stays `false`.
- Sessions written before this feature have no flag, so they deserialize to
  `false` and are treated as auto-named and eligible for purge. This is a
  deliberate, accepted consequence (no pre-1.0 migration; see
  `no-migration-pre-1.0`).

Only `named_by_user == false` sessions are ever purged.

## The background worker

- Spawned once at TUI startup. The first sweep runs **30 s after start**, then
  every **1 hour**.
- It is **peer-aware**: multiple hrdr instances may run at once, and none may
  act on a session a live instance is using, nor collide with another sweeper.

### Peer-safety via the existing open-lock

Every open session already holds a per-session **open-lock** — an `O_EXCL` lock
file (`acquire_open_lock`, `session.rs`), whose stale entries self-reap. The
sweep reuses it as the one coordination primitive.

For each candidate file, try to acquire its open-lock:

- **`Err(SessionBusy)`** — a live instance holds it (the session is in use), or
  another sweeper is on it. **Skip.**
- **`Ok(lock)`** — we hold it exclusively. No live user, no other sweeper. Do
  the work, then drop the lock (which removes the lock file).

No global sweep lock is needed: `O_EXCL` guarantees exactly one holder, so two
sweepers can never touch the same session.

### mtime is the clock

The sweep decides by file **mtime** (a cheap `stat`), not by parsing every file.
Compression **preserves the original mtime** (via `filetime`) so a compressed
session's "last used" time is unchanged — otherwise every compression would
reset the purge clock and a session would never age out. Only a purge candidate
(mtime already past the purge age) is loaded, and only to read `named_by_user`.

## Configuration

Two settings, following the existing `Option<u64>`-seconds pattern (cf.
`request_timeout`). Each is overridable by config, environment, and flag; `0`
disables that phase.

| Setting                  | Config key               | Env                           | Flag                       | Default             |
| ------------------------ | ------------------------ | ----------------------------- | -------------------------- | ------------------- |
| Compress after (seconds) | `session_compress_after` | `HRDR_SESSION_COMPRESS_AFTER` | `--session-compress-after` | `604800` (1 week)   |
| Purge after (seconds)    | `session_purge_after`    | `HRDR_SESSION_PURGE_AFTER`    | `--session-purge-after`    | `2592000` (1 month) |

## Implementation map

Five slices, each independently testable.

1. **Storage** (`hrdr-app/src/session.rs`)
   - Add `named_by_user: bool` to `SessionState` (`#[serde(default)]`).
   - `load_path`: when the path ends `.json.zst`, read bytes, zstd-decode, then
     parse; fix id derivation for the double extension.
   - `save`: still writes plaintext `<id>.json` for an active session; if a
     `<id>.json.zst` exists (session was compressed, then resumed), remove it
     after writing the fresh `.json`.
   - `collect_sessions`: also enumerate `.json.zst`.

2. **Sweep** (`hrdr-app/src/session.rs`) —
   `sweep_sessions(compress_after, purge_after)`:
   - Walk every session file; decide by mtime age.
   - Purge (mtime past purge_after): lock, load, delete iff `!named_by_user`.
   - Compress (`.json`, mtime past compress_after): lock, zstd to `.json.zst`,
     **preserve mtime**, remove `.json`.
   - Peer-safe via the open-lock. Purge handles both `.json` and `.json.zst`.

3. **Config** (`hrdr-agent/src/config.rs`) — the two settings plus env and
   flags.

4. **Worker** (TUI startup) — spawn a tokio task: `sleep 30s` then
   `loop { sweep_sessions(...); sleep 1h }`, with ages from config.

5. **Naming** (`hrdr-tui/src/app/commands.rs`) — set `named_by_user = true` in
   `set_session_label`, covering both `/new <name>` and `/rename`.

## Dependencies

- `zstd` — compression.
- `filetime` — preserve a compressed file's mtime.
