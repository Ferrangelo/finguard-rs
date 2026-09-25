# Sync

The desktop is the sync hub. A phone pairs with it once, using a 6-digit code, and from
then on the phone starts each round by pressing Sync now on its Sync page. Both sides
exchange change log entries over an encrypted connection on the local network. There is
no cloud service and no third-party server involved.

## How to sync

1. On the desktop, open the Sync page. Its listener binds `0.0.0.0:3112` only while that
   page stays open, and closes about 60 seconds after the page stops sending
   heartbeats.
2. On the desktop, issue a pairing code. It is 6 digits, valid for 5 minutes, and
   accepts 3 attempts before it is withdrawn.
3. On the phone's Sync page, find the desktop automatically (the phone sends one UDP
   broadcast; the desktop answers within a 3 second window, only while its listener is
   open) or type its address by hand. Enter the pairing code.
4. Press Sync now. The first round after pairing always resets the phone from the
   desktop's full log: the phone shows what it would lose and asks you to confirm,
   unless every loss count is zero, in which case it confirms on its own. The app backs
   up the phone's data folder before the reset.
5. Every later round, Sync now pushes what the desktop lacks and pulls what the phone
   lacks.
6. To stop syncing a device, unpair it from either side's Sync page.

### Troubleshooting

- **Nothing found during discovery, or pairing fails to connect**: check that the
  desktop's Sync page is open (the listener only runs then) and that both devices are
  on the same local network. If a firewall blocks the desktop, allow TCP and UDP on
  port 3112; with ufw, `sudo ufw allow 3112/tcp` and `sudo ufw allow 3112/udp`.
- **"another sync is running on this device"**: only one sync round runs at a time per
  device. Wait and try again.
- **A settings file could not be read**: the hub shows this as a banner. Fix or restore
  the file (`category_mappings.json`, `known_categories.json`, or `currency.json` in
  the config folder) and try again.
- **A log needs repair**: the app repairs its own change log automatically when it
  detects damage (see "Repairs", below); no manual step is normally needed.

## What syncs and what does not

Synced: the six row-id tables (monthly expenses, recurring templates, investments,
investment prices, liquidity, and credits and debts), the income cells of the cashflow
table, and settings (`category_mappings`, `known_categories`, `currency_settings`).

Not synced: `fx_rates.json` (each device fetches and caches its own rates), the legacy
`primaries.parquet` and `secondaries.parquet` files, the other (computed) rows of the
cashflow table, and backups.

## Design choices

**An append-only change log, not copied files.** Every change is one JSON line
appended to `sync/changelog.jsonl`. A write failure can damage at most that one line,
and every change can be replayed from the log.

**A hybrid logical clock plus device id, not wall-clock time.** A device stamp combines
a logical clock with the device id, so a device with a wrong wall clock cannot win or
lose every conflict just because its clock is fast or slow. Two devices can issue the
same clock value, but never the same (clock, device id) pair, so there is always exactly
one order across every device.

**A stable `row_id` per row, not row position.** Row order in a file can shift when a
row is inserted; a stable id lets both devices agree on which row a change refers to.

**Merge rules differ by table shape.** Expenses and recurring templates merge as whole
rows: the newest version of a row wins outright, because merging them field by field
could invent a row nobody actually typed. Net worth tables and the cashflow income row
merge cell by cell instead, because two devices filling in different months of the same
row is normal, not a conflict. A generated recurring row never comes back from a delete
just because a newer entry exists for it: `generated_row_stays_deleted` keeps a user's
deletion of a generated row in place regardless of stamps. Every losing version stays in
the log; nothing is destroyed, and only the destination file loses the losing value.

**No stored sync position.** What each side lacks is computed by comparing the two logs
at the start of every round, not read from a saved cursor. If applying a batch fails,
nothing was marked sent, so the next round computes the same missing entries and tries
again.

**Device id, keys, and paired peers live in the config folder, not the data folder.** A
copied data folder must not clone a device's identity or its pairings: pairing is part
of what makes a device this device, not part of its data.

**Repairs go through the desktop.** A phone whose log is damaged or incomplete resets
from the desktop rather than trying to repair itself. A damaged desktop log is rewritten
from its still-readable lines, the desktop's data is recorded into the log again, and
every paired phone is then required to reset on its next round.

**A newly paired phone always resets before it may push.** This keeps data the phone
held before pairing from ever reaching the desktop unasked. The desktop tracks this with
a reset id it issues at pairing time and clears only once the phone proves, in a later
round, that it completed that specific reset.

**Security.** Pairing uses SPAKE2 over the 6-digit code, so an attacker gets one guess
per pairing attempt and cannot test codes against a recording offline. The pairing
handshake continues with `Noise_XXpsk3` using the key SPAKE2 produced. Every sync round
after pairing uses `Noise_KK` with the static keys exchanged during pairing, so the
phone's device id never has to cross the wire in the clear before it is proven. The
private key file is written with mode 0600. A device allows at most 8 concurrent sync
connections and limits a session to 15 minutes. Only peer-safe error text (counts,
device ids, error kinds) ever crosses the wire; row values, names, amounts, and
categories never do. The sync listener runs only while the Sync page is open, and the
ordinary API ports stay on loopback with a `Host` header check that blocks DNS
rebinding.

**Android backup stays off.** The phone gets its data back through sync, not through
Android's cloud backup, which is disabled in its manifest.

## Not built yet

- A view to browse or restore a conflict's losing version; it stays in the log but has
  no UI yet.
- Automatic sync; today a round only starts when a Sync page presses Sync now.
- Sync directly between two phones, without a desktop hub.
