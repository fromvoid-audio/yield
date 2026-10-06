# Original recovery admission audit backing (OPT-2391)

Generic retained-worker admission audits recovery-reserve history on the same
Store connection and held read transaction as the ordinary rebuild. The funded
reader borrows original SQLite values, validates UUID/device/closed-enum text
before projection, and shares the complete transition state machine with the
unfunded compatibility reader. It streams every row; no history count limit,
schema change, retry, reservation refund or physical authority is introduced.

The original ordinary InputPermit pays compiled statement, row, state-machine,
error and permit headers before SQL. Simultaneous charge/transition/prior-state
text and canonical validation renderings have closed bounds. Formatting growth
and bounded SQL column names have explicit backing; invalid-UTF8 paths charge
seven times encoded path length: one retained PathBuf plus twice the maximum
three-byte replacement rendering. The original native statement helper selects
SQLite code/message before any subsequent native call and prepays lossy copy and
formatting overlap. Wrong-type errors retain original SQLite projection names.

Owned failure fields precede fixed/native credits. A real queued reply inherits
those credits before the diagnostic escapes; its existing consumer custody lasts
through formatting, delivery/lost delivery and receiver teardown. An isolated
component with no reply backing may inspect the private funded failure wrapper;
it cannot detach an owned diagnostic into an unfunded return. Quota refusal uses
an allocation-free error. Stable input/header custody is unchanged.

File-backed fixtures compare empty, long reconstituted histories, every active
state, all legal transition kinds and mandatory corruption rejection with the
compatibility audit. Original queued admission tests cover retained errors/lost
reply, teardown and exact pressure at this audit boundary without IDs or claims.
The System allocator probe compares the same original stored histories and large
corrupt text, separately naming isolated pool setup. Rust backing measurements
exclude SQLite native allocation/RSS and supply no physical qualification.

Only recovery-reserve admission auditing is covered. Recording, database claim
and attempt, chunk and startup audit backing, provider graphs (OPT-2390), full
Legacy adoption/migration, atomic joint-WAL admission, private permits, native
physical qualification and genuine settlement remain open. OPT-2373, OPT-2117,
OPT-1553 and the execution-plan tracking issue remain open.
