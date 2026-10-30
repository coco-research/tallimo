# Fixtures

`sample.json` is the only data the app should see in this repo.

It is fictional. Boards, repos, people, projects, and companies are made up (for example Nora Example, June Sample, Theo Demo, Example Corp). Any figures are synthetic round numbers. Use it to run ingest, the matcher, and the UI without a network.

## Forbidden

- Real client exports. Real Jira issues. Real GitHub pulls. Real names, emails, or ids.
- A "temporary" copy of production JSON to debug a matcher miss. Build a smaller fictional case instead.
- Pasting fixture-shaped client text into a model prompt, a wiki page, or a commit message.

`sample*.json` stays tracked. Databases do not. If a fixture starts looking like a dump, delete it before it is committed.

New fixtures stay fictional.
