# Security policy

Please report vulnerabilities privately through GitHub's "Report a vulnerability" (private
vulnerability reporting) on this repository. Do not open public issues for security problems.

## Support

Zeabur Builder has no release yet. Until it does, fixes land on `main` only.

Builds run your teams' code inside a shared buildkitd pool, which sees every build's secrets and
registry credentials. The design assumes teams that trust each other; see §1.3 of the
[plan](docs/plan.md) and the risks in the [feasibility study](docs/feasibility.md#risks).

Maintainers must enable GitHub private vulnerability reporting and confirm that the reporting
link is available before making a release public.
