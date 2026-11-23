# Gruntas

Gruntas publishes a file produced during this session — screenshot, screen recording, log,
report — to a permanent URL that renders as a card in GitHub, Slack, Basecamp and Linear.

Use it when a person needs to see the result and the environment that produced it will
not be around later, or when the reader works in Slack, Basecamp or Linear and will not
open the pull request.

Tools: `gruntas_push`, `gruntas_list_artifacts`, `gruntas_get_artifact`, `gruntas_claim_artifact`,
`gruntas_get_run`, `gruntas_verify_key`.

Pushing requires no account. `GRUNTAS_TOKEN` is optional and adds run grouping, listings
and, on a paid plan, permanence past the 24-hour expiry — a free workspace's
uploads expire in 24 hours too.

Deleting an upload is immediate and unrecoverable — confirm with the user first.

Docs: https://gruntas.com/docs/gemini-cli
