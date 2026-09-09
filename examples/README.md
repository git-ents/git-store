# Equivalent database examples

These scripts run the same small data-manipulation walkthrough against the two
CLIs:

```sh
./examples/git-db.sh
./examples/dolt-db.sh
```

Each script creates a temporary repository and removes it when it exits. Pass a
directory to keep the repository for inspection:

```sh
./examples/git-db.sh /tmp/git-db-example
./examples/dolt-db.sh /tmp/dolt-db-example
```

The Git script invokes the repository's external subcommand as `git store db`.
It uses `GIT_PAGER=cat`, `PAGER=cat`, and `git --no-pager` so Git never opens a
pager. It expects `git-store` to be available as `git-store` on `PATH` (Git's
external-subcommand lookup supplies that command in a normal installation).
The Dolt script expects `dolt` on `PATH` and sets `PAGER=cat`.

## Operation mapping

| Task                      | Git database CLI                  | Dolt CLI                                      |
| ------------------------- | --------------------------------- | --------------------------------------------- |
| Initialize                | `git store db init`               | `dolt init`                                   |
| Create a table            | `git store db table create users` | `CREATE TABLE users (...)` via `dolt sql`     |
| Insert/update/delete rows | `git store db put` / `rm`         | `INSERT` / `UPDATE` / `DELETE` via `dolt sql` |
| Read rows                 | `git store db get`                | `SELECT` via `dolt sql`                       |
| Inspect changes           | `git store db status`, `diff`     | `dolt status`, `diff`                         |
| Stage                     | `git store db add users`          | `dolt add users`                              |
| Commit                    | `git store db commit -m ...`      | `dolt commit -m ...`                          |
| History                   | `git store db log`                | `dolt log`                                    |
| Export/import             | `db table export/import`          | `table export/import`                         |

The database CLI intentionally has a Dolt-shaped porcelain, but it is a
key/value database today: a row is a string key and a JSON value. Dolt uses
relational tables with declared columns and primary keys, so SQL is the closest
semantic equivalent for row manipulation. Dolt's JSON export has a `rows`
wrapper and needs a matching SQL schema when imported; the scripts account for
that difference explicitly.

To compare persisted objects in the Git example, inspect the printed directory
with ordinary Git plumbing, for example:

```sh
git -C /tmp/git-db-example ls-tree refs/db/heads/main^{tree}
git -C /tmp/git-db-example log --oneline refs/db/heads/main
```
