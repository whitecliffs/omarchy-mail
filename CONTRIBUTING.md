# Contributing to Omarchy Mail

Thanks for helping improve Omarchy Mail. The project is an independent
community application for Omarchy Linux.

## Before opening an issue

Search existing issues first. For a bug, include:

- Omarchy version and hardware details when relevant
- the application version or commit
- steps to reproduce and what happened
- relevant logs with email addresses, message bodies, tokens, passwords, and
  private calendar URLs removed

Use a GitHub Discussion for questions, feature ideas, and broader design
conversation.

## Development

Install the Arch dependencies described in the README, then run:

```bash
cargo fmt --all -- --check
cargo test --locked
```

Use `OMARCHY_MAIL_DEMO=1 cargo run` to inspect the interface without an
account. Changes that affect mail transport, caching, credentials, or calendar
sync should include focused offline tests where practical.

## Pull requests

Keep pull requests focused and explain the user-visible behavior. Do not commit
credentials, OAuth client secrets, keyring exports, mail databases, calendar
feeds, generated build artifacts, or real message fixtures. A maintainer will
run the release checks and review packaging before publication.

## License

By contributing, you agree that your contribution may be distributed under the
MIT license in [LICENSE](LICENSE).
