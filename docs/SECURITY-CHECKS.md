# Security checks

Pull requests and pushes to `main` run two security checks from
`.github/workflows/security.yml`:

- **CodeQL** analyses the Rust source with the `security-extended` query suite
  and publishes its SARIF results to GitHub Code Scanning.
- **RustSec** runs `cargo-audit` against the committed root `Cargo.lock`.

The checks also run every Monday so newly published advisories are detected
without waiting for a source change. Dependabot separately monitors Cargo and
GitHub Actions dependencies through `.github/dependabot.yml`.

## Blocking policy

CodeQL security alerts rated High or Critical block updates to `main`.
`cargo-audit` uses `.cargo/audit.toml` and fails for RustSec vulnerabilities
with CVSS severity High or Critical. Lower-severity and informational findings
do not fail the check by default; informational warnings remain visible in the
job log.

An ignored RustSec advisory must be added to `advisories.ignore` with a nearby
comment explaining why VMLord is not affected. Upgrade the dependency instead
whenever a fixed version is available.

## Applying the GitHub ruleset

GitHub stores merge protection outside the repository. The reproducible REST
API request body is committed as `.github/rulesets/main-security.json`.
After the Security workflow has completed once on `main`, a repository
administrator applies it with:

```shell
gh api --method POST repos/MrUndead1996/vm-lord/rulesets \
  --input .github/rulesets/main-security.json
```

The ruleset requires the `RustSec` status check and CodeQL results, and applies
the `high_or_higher` Code Scanning security threshold. Do not apply it before
both checks exist on `main`, because GitHub will otherwise block every update.
