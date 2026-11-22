# Krowk

Krowk publishes a file produced during this session — screenshot, screen recording, log,
report — to a permanent URL that renders as a card in GitHub, Slack, Basecamp and Linear.

Use it when a person needs to see the result and the environment that produced it will
not be around later, or when the reader works in Slack, Basecamp or Linear and will not
open the pull request.

Tools: `krowk_push`, `krowk_list_artifacts`, `krowk_get_artifact`, `krowk_claim_artifact`,
`krowk_get_run`, `krowk_verify_key`.

Pushing requires no account. `GRUNTAS_API_KEY` is optional and adds run grouping, listings
and, on a paid plan, permanence past the 24-hour expiry — a free workspace's
uploads expire in 24 hours too.

Deleting an upload is immediate and unrecoverable — confirm with the user first.

Docs: https://krowk.com/docs/gemini-cli
