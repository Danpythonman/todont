# todont

A todo list for the terminal (CLI and TUI) that syncs between devices
through a small self-hosted server. The server also sends due-date
reminders and overdue nags to your phone using [ntfy](https://ntfy.sh).

Everything is one binary, `td`:

| Command | What it does |
|---|---|
| `td` | TUI |
| `td add call mom -d "tomorrow 5pm"` | add a task (`a`) |
| `td ls [-a] [--json]` | list open (or all) tasks |
| `td done 3 4` / `td undo 3` | complete / reopen |
| `td edit 3 -t "new title" -d fri` / `--no-due` | change a task |
| `td add x -d fri --remind 15m --nag off` | per-task alerts (also on `edit`) |
| `td rm 3` | delete |
| `td sync` | sync now |
| `td init [--server]` | first-time setup (writes the config) |
| `td serve` | run the sync server and notifier |

**Due dates** use your local time zone. These all work, and are
case-insensitive:

| Kind | Examples |
|---|---|
| relative | `+30m`, `+2h`, `+3d`, `+1w`, `in 2h` |
| named days | `today`, `tomorrow` / `tmr`, `fri`, `next friday` |
| numeric | `2026-09-30`, `2026/09/30` |
| month names | `sep 30`, `September 30th`, `30 sept`, `the 30th of september`, `Sep 30, 2027` |
| day of month | `the 30th`, `1st` |
| times | `17:00`, `5pm`, `5:30 pm`, `noon` |

- **Adding a time:** you can put a time before or after any date, e.g.
  `fri 5pm`, `sep 30 at 17:30` or `5pm tomorrow`.
- **Filler words:** commas and the words *on*, *the*, *of*, *at* and *by*
  are ignored.
- **Weekdays:** `fri` means the next Friday, never today.
- **No year:** a date like `sep 1` means the next time it comes around. So
  does `the 31st`, which skips months that don't have one.
- **A time on its own:** means the next time the clock reads that.
- **A date on its own:** means 09:00.
- **Not supported:** slash dates like `9/30` are rejected on purpose,
  because they're ambiguous (month/day or day/month?).

**TUI keys:** `j`/`k` or arrows move, `a` add, `e` or `Enter` edit,
`space` done/reopen, `d` delete, `h` show/hide done, `q` quit. Completing
or deleting a task asks for confirmation first (`y`/`Enter` or `n`/`Esc`).
Reopening a done task doesn't ask. The bottom row always lists every key
that works on the current screen.

With sync configured, each task has a status column:
- **✓** means the server has this version.
- **A spinner** means it's being pushed right now.
- **○** means it's changed here but not pushed yet, e.g. while offline. It
  goes out with the next sync.

`a` and `e` open a form with Title, Due, Remind and Nag fields:
- `Tab` / `Shift-Tab` (or `↑` / `↓`) move between fields.
- In text fields, use `←` / `→`, `Home` / `End` (or `^A` / `^E`), `Del`
  and `^U` to edit. The Due field shows the date it understood as you type.
- In Remind and Nag, `←` / `→` or `space` change the value, and
  `Backspace` / `Del` reset it to the default.
- `Enter` saves from any field. If something is wrong, the form stays open
  and jumps to the field at fault. `Esc` cancels.

## Setup

```sh
cargo build --release   # target/release/td
```

The local database lives at `~/.local/share/todont/todont.db` (override
with `--db` or `$TODONT_DB`). With no config, `td` is a local-only todo list.

### Server (home machine)

```sh
td init --server
```

This asks a few questions. Every one has a default ready; press Enter to
accept it, or pass `--yes` to accept them all. It:
- generates the sync token and a secret ntfy topic;
- detects your time zone;
- writes the config (mode `0600`), optionally setting this machine up as a
  client too;
- offers to send a test notification;
- prints the exact command to run on your other devices.

Then run `td serve`. To run it as a service instead, write the config
system-wide and install the unit, as described in the header of
[`deploy/todont.service`](deploy/todont.service):

```sh
sudo td init --server --config /etc/todont/config.toml
```

That also moves the database default to `/var/lib/todont/`.

The server speaks plain HTTP and listens on `127.0.0.1:8787` by default.
To reach it from other devices, put TLS in front:
- **Tailscale:** `tailscale serve --bg 8787` gives you
  `https://<host>.<tailnet>.ts.net`, reachable only by your own devices.
- **Reverse proxy:** e.g. Caddy's `todo.example.com { reverse_proxy
  127.0.0.1:8787 }`.

`GET /health` returns `ok` without auth, for uptime checks.

### Each device

```sh
td init --url https://todo.example.net --token <token from the server>
```

Or run plain `td init` to be asked for them. It writes
`~/.config/todont/config.toml` (or `--config` / `$TODONT_CONFIG`) and does a
first sync to check that the URL and token work.

Each `init` only rewrites its own sections of the config:
- `td init` rewrites `[sync]`.
- `td init --server` rewrites `[server]` and `[ntfy]`, plus `[sync]` if this
  machine is also a client.

Everything else in the file, comments included, is left alone, so one
machine can be both server and client in either order. Rerunning `init`
offers the current values (token, ntfy topic, …) as defaults. It only
replaces a section after asking, or with `--force`. There are also hand-written
examples in [`deploy/`](deploy/).

### Phone

Install the ntfy app (F-Droid or Play Store) and subscribe to your topic on
`ntfy.sh`. Reminders arrive at high priority; repeat nags arrive at default
priority. On public ntfy.sh the topic name is the only secret, so keep it
long and random.

## How sync works

- **Offline first.** Every device has a full local copy. Changes are
  marked dirty and pushed by `td sync`. They're also pushed automatically
  after each CLI change and in the background from the TUI (on start, after
  each edit, every minute, and briefly on quit). If the server can't be
  reached, you get a one-line note and the change goes out next time.
- **One endpoint.** `POST /sync` carries the client's dirty tasks and the
  last server cursor it saw. The server keeps whichever version of each task
  has the newer `updated` timestamp (last write wins, whole task), then
  returns everything that changed after the cursor.
- **Identity.** Tasks are identified by UUID across devices. The short
  numbers you type (`td done 3`) belong to one device only and differ
  between machines.
- **Deletes** are kept as tombstones so they propagate.
- **Server replaced or restored?** It gets a new `server_id`, and clients
  notice and re-push everything.

**Caveats.** Last write wins uses device clocks, so a machine with a badly
wrong clock can win or lose edits it shouldn't. If two devices edit
different fields of the same task while offline, one edit replaces the
other; fields aren't merged.

## Notifications

The notifier runs inside `td serve` and checks every 30 seconds.

- **Reminder.** One notification when a task comes due (or
  `lead_minutes` before). If you move the due date, you get a new reminder
  for the new time.
- **Nag.** Every `nag_minutes` while an overdue task stays open. Set it
  to `0` to turn nags off.

Each task can override both, from the TUI form or with `--remind` /
`--nag`:
- `--remind` takes `default`, `off`, `due` (at the due time), or a lead
  time like `15m`, `1h` or `1d`.
- `--nag` takes `default`, `off`, or an interval like `30m` or `2h`.

These settings sync with the task. With reminders off but nags on, the
first nag comes one interval after the due time.

A notification counts as sent only once ntfy accepts it, so a network
blip means a retry, not a lost reminder. If the server was down when a task
came due, the first message says "Overdue by …" instead of "Due now".

## Layout

| File | Role |
|---|---|
| `src/core.rs` | domain logic (`App`); never prints or formats |
| `src/db.rs`, `src/sql/*.sql` | local SQLite queries and migrations |
| `src/due.rs` | due-date parsing and display |
| `src/cli.rs`, `src/tui.rs` | the two frontends |
| `src/form.rs` | the TUI's add/edit form |
| `src/alerts.rs` | per-task remind/nag settings |
| `src/proto.rs` | `/sync` wire format |
| `src/sync.rs` | sync client |
| `src/server.rs`, `src/sql/server/` | sync server |
| `src/notify.rs` | ntfy notifier |
| `src/config.rs` | `config.toml` |

`cargo test` covers core, due parsing, the TUI's update logic, notifier
scheduling, and real HTTP sync between in-memory clients and a server.
