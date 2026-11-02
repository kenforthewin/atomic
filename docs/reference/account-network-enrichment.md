# Account network research records

The single-account canvas reads research outcomes from the account's atoms. Its **Research checks** panel distinguishes a recorded run that found data, a recorded run with no findings, and a node with no recorded run. The last state does not prove that no external activity occurred. A verified connection is counted separately from the run result.

## Recording an enrichment run

Put one line in the atom for the person, company, or research door that was checked:

```markdown
- Enrichment run: Provider name | 2026-11-02 | data | 2 exact profiles | https://example.com/run/123
- Enrichment run: Provider name | 2026-11-02 | empty | No exact profile | https://example.com/run/124
```

The fields are provider, check date, result (`data` or `empty`), a short result description, and an optional source URL. Each line is one recorded check. The canvas also reads the existing Happenstance check and sourced research-door findings in the seeded account notes. An empty generated door is a placeholder until an actual run is recorded. If a new explicit line names the same provider, it replaces that provider's legacy summary in the panel. The Categories tab shows a green check for a category with a recorded run or sourced findings, including explicit runs that found nothing.

## Marking a verified entry path

In the source atom, record a resolved wiki link to the next atom in the path and cite evidence on the same line:

```markdown
- Verified entry point: [[target-atom-id|Named connection]] ([Evidence](https://example.com/source))
```

The canvas animates this connection only when the target belongs to the selected account, Atomic has resolved the wiki link, and the evidence URL is present. A possible overlap, shared school, profile match, or unresolved link does not activate the signal. People who prefer reduced motion see a static highlighted line; the panel also provides a pause control when an animated path exists.

These records live in the atoms, so they follow the selected database and account. Freeform and chart layouts display the same verified path. Chart positions remain device-local, as described by the canvas controls.
