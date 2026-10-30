# Loop log (shared JSONL schema, v1)

Tallimo writes its loop events (self, human, agent) to an append-only JSONL file:
one JSON object per line, UTF-8, `\n`-terminated. Any reader can load it
as plain JSONL with no product code.

| field | type | meaning |
|---|---|---|
| `v` | int | schema version, `1` |
| `ts` | string | UTC time, `YYYY-MM-DDTHH:MM:SSZ` |
| `product` | string | product id, e.g. `tallimo` |
| `loop` | string | `self`, `human` or `agent` |
| `kind` | string | `action`, `decision`, `outcome` or `feedback` |
| `actor` | string | who or what produced it (`ci`, `operator`, `agent:<name>`) |
| `ref` | string | what it is about (a case id, a card id, a PR number); no paths |
| `detail` | string | short plain text, at most 200 characters |

Unknown fields are ignored by readers; new fields only ever get added.

Data rule: the file stays on the machine that wrote it. Nothing here sends it anywhere, and no
user install reports anything. Never put secrets, prompt text, commands or file contents in it;
writers drop secret-shaped strings and truncate `detail`.

Where it goes (Tallimo): `feedback.jsonl` in the same folder as the database, as an absolute path
(`rollup-core::feedback::path_for`; a relative DB_PATH is resolved first).
Tests write only under the system temp dir.

## Tallimo loops
- **human (live):** an operator flags a Today or Inbox card as `wrong`, `confirm` or `dismiss`. Each flag is one
  `human`/`feedback` line, `ref` = `<page>:<card id>`. Leadership cards are out of scope for now. The daily file
  scan reads the count; each `wrong` flag becomes a fixture or regression case.
- self, agent: later steps.
