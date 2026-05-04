# Release migrations — escape hatch for non-idempotent schema changes

The day-to-day workflow uses `db/schema.sql` (declarative, idempotent
DDL) + `db/seeds.sql` (idempotent INSERTs), applied on every boot via
`crates/common/src/db.rs::run_migrations`. That covers most schema
changes — adding columns/indexes/tables, modifying functions/triggers,
adding seed rows.

Some changes can't be expressed declaratively:

- **Column rename** — `ALTER TABLE … RENAME COLUMN` is a one-shot.
  Putting `IF NOT EXISTS` on the new column doesn't undo the old one.
- **Type narrowing** — `ALTER TABLE … ALTER COLUMN … TYPE` may need a
  USING clause and isn't safe to re-apply.
- **DROP COLUMN** — `DROP COLUMN IF EXISTS` works but is destructive;
  you only want it to run once, after data has been backfilled or
  confirmed unused.
- **Data backfill / migration** — `UPDATE foo SET …` is never
  idempotent in a useful way.

For each of these, write a one-off SQL file here, run it manually
against each environment, then commit the file as audit history. The
boot path doesn't apply files in this directory — they're documentation
+ paper trail.

## Conventions

- Filename: `YYYY-MM-DD_short_description.sql`
- One concern per file. If a release contains 3 unrelated changes, that's 3 files.
- Top of file: comment explaining what + why, and which environments it's been applied to.
- After applying, edit `db/schema.sql` so the declarative source-of-truth reflects the post-migration state.

## Example

```sql
-- 2026-05-04_rename_users_avatar_to_picture.sql
--
-- Why: marketing wanted "profile picture" terminology in the UI
-- copy; renaming the underlying column for clarity.
--
-- Applied to: dev (2026-05-04), staging (2026-05-05), prod (—).
--
-- After running this, db/schema.sql was updated to declare
-- `picture_url` instead of `avatar_url`.

ALTER TABLE users RENAME COLUMN avatar_url TO picture_url;
```

## Why this directory isn't auto-applied

If we auto-applied these on every boot we'd be back in
"versioned migration" land — a stack of files that accumulates
forever. The whole point of `db/schema.sql` is to be the single
source of truth, with this directory as an explicit, manual escape
hatch — used rarely, reviewed carefully, retained only as audit.
